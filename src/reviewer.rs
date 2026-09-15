// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::ReviewStatus;
use crate::ai::quota::QuotaManager;
use crate::ai::{
    AiErrorClass, AiProvider, AiRequest, RemoteAiErrorPayload, classify_ai_error,
    create_provider_cached,
};
use crate::baseline::{BaselineRegistry, BaselineResolution, extract_files_from_diff};
use crate::db::{AiInteractionParams, Database, Finding, PatchsetRow, Severity};
use crate::email_policy::EmailPolicyConfig;
use crate::email_router::{Action as EmailAction, EmailRouter};
use crate::git_ops::{GitWorktree, ensure_remote, get_commit_hash};
use crate::settings::Settings;
use crate::utils::redact_secret;
use crate::worker::prompts::ReviewError;
use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

#[derive(Clone)]
struct ReviewContext {
    semaphore: Arc<Semaphore>,
    llm_semaphore: Arc<Semaphore>,
    db: Arc<Database>,
    settings: Settings,
    baseline_registry: Arc<BaselineRegistry>,
    quota_manager: Arc<QuotaManager>,
    target_review_count: usize,
    provider: Arc<dyn AiProvider>,
    activity: Arc<crate::activity::ActivityRegistry>,
    cancels: Arc<crate::cancel::CancelRegistry>,
}

enum PatchResult {
    Success,
    ReviewFailed,
}

#[derive(Serialize)]
struct BaselineAttempt {
    baseline: String,
    status: String,
    log: String,
}

static INTERACTION_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn generate_interaction_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let epoch_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    generate_interaction_id_at(epoch_millis)
}

fn generate_interaction_id_at(epoch_millis: u128) -> String {
    let sequence = INTERACTION_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("rev_{}_{}", epoch_millis, sequence)
}

/// The `Reviewer` service orchestrates the review process for patchsets.
///
/// It manages:
/// - Baseline resolution and worktree preparation.
/// - AI-based code review execution.
/// - Patch application verification.
/// - Interaction with the database and external tools.
pub struct Reviewer {
    db: Arc<Database>,
    settings: Settings,
    semaphore: Arc<Semaphore>,
    llm_semaphore: Arc<Semaphore>,
    baseline_registry: Arc<BaselineRegistry>,
    quota_manager: Arc<QuotaManager>,
    provider: Arc<dyn AiProvider>,
    activity: Arc<crate::activity::ActivityRegistry>,
    cancels: Arc<crate::cancel::CancelRegistry>,
}

/// Writes the review's running total to the database on an interval.
///
/// A review task has many exit paths and a killed daemon takes none of them,
/// so the total cannot be left to a write at the end. The interval matches
/// the activity registry's: at most half a minute of a run's time is lost if
/// the process dies, against one small UPDATE per in-flight patchset.
struct ReviewClockGuard {
    handle: tokio::task::JoinHandle<()>,
}

impl ReviewClockGuard {
    fn new(
        db: Arc<Database>,
        patchset_id: i64,
        carried_seconds: u64,
        started: std::time::Instant,
    ) -> Self {
        const CHECKPOINT: std::time::Duration = std::time::Duration::from_secs(30);

        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(CHECKPOINT);
            // The first tick fires immediately; skip it, since zero elapsed
            // would overwrite the carried total with itself for no reason.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let total = carried_seconds + started.elapsed().as_secs();
                // Best effort: timing must never be able to fail a review.
                if let Err(e) = db.set_patchset_review_duration(patchset_id, total).await {
                    tracing::debug!(
                        "Failed to checkpoint review duration for {}: {}",
                        patchset_id,
                        e
                    );
                }
            }
        });

        Self { handle }
    }
}

impl Drop for ReviewClockGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Reviewer {
    /// Creates a new `Reviewer` instance.
    ///
    /// # Arguments
    ///
    /// * `db` - The database connection.
    /// * `settings` - Application settings.
    pub async fn new(
        db: Arc<Database>,
        settings: Settings,
        activity: Arc<crate::activity::ActivityRegistry>,
        cancels: Arc<crate::cancel::CancelRegistry>,
    ) -> Self {
        let concurrency = settings.review.concurrency;
        let repo_path = PathBuf::from(&settings.git.repository_path);

        let baseline_registry = match BaselineRegistry::with_options(
            &repo_path,
            settings.git.custom_remotes.clone(),
            settings.git.mainline_baseline_fallback,
        ) {
            Ok(r) => Arc::new(r),
            Err(e) => {
                error!(
                    "Failed to initialize BaselineRegistry: {}. Using empty registry.",
                    e
                );
                Arc::new(
                    BaselineRegistry::with_options(
                        &repo_path,
                        settings.git.custom_remotes.clone(),
                        settings.git.mainline_baseline_fallback,
                    )
                    .unwrap_or_else(|_| {
                        panic!("Critical error initializing BaselineRegistry: {}", e)
                    }),
                )
            }
        };

        let provider = create_provider_cached(
            &settings,
            settings.ai.response_cache,
            settings.ai.response_cache_ttl_days,
        )
        .await
        .expect("Failed to create AI provider");

        let llm_concurrency = crate::ai::concurrency_limited_provider::llm_permits(concurrency);

        Self {
            db,
            settings,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            llm_semaphore: Arc::new(Semaphore::new(llm_concurrency)),
            baseline_registry,
            quota_manager: Arc::new(QuotaManager::new()),
            provider,
            activity,
            cancels,
        }
    }

    /// Starts the reviewer service loop.
    ///
    /// This method runs indefinitely, polling the database for pending patchsets
    /// and processing them. It handles concurrency limits and worktree cleanup.
    pub async fn start(&self) {
        info!(
            "Starting Reviewer service with concurrency limit: {}",
            self.settings.review.concurrency
        );

        if self.settings.ai.no_ai {
            info!(
                "AI interactions disabled via settings. Reviewer service will skip AI analysis but verify patch application."
            );
        }

        // Ensure Context Cache
        let worktree_dir = PathBuf::from(&self.settings.review.worktree_dir);
        if worktree_dir.exists() {
            info!(
                "Cleaning up previous worktree directory: {:?}",
                worktree_dir
            );
            if let Err(e) = std::fs::remove_dir_all(&worktree_dir) {
                error!("Failed to cleanup worktree directory: {}", e);
            }
        }
        if let Err(e) = std::fs::create_dir_all(&worktree_dir) {
            error!("Failed to create worktree directory: {}", e);
        }

        match self.db.reset_reviewing_status().await {
            Ok(count) => {
                if count > 0 {
                    info!("Recovered {} interrupted reviews (reset to Pending)", count);
                }
            }
            Err(e) => error!("Failed to reset reviewing status: {}", e),
        }

        loop {
            match self.process_pending_patchsets().await {
                Ok(_) => {}
                Err(e) => error!("Error in reviewer loop: {}", e),
            }

            if let Err(e) = self.release_embargoed_results().await {
                error!("Error releasing embargoed results: {}", e);
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        }
    }

    async fn process_pending_patchsets(&self) -> Result<()> {
        let patchsets = self.db.get_pending_patchsets(10).await?;

        if patchsets.is_empty() {
            return Ok(());
        }

        info!("Found {} pending patchsets for review", patchsets.len());

        for mut patchset in patchsets {
            let permit = self.semaphore.clone().acquire_owned().await?;
            let target_review_count = patchset.target_review_count.unwrap_or(1) as usize;

            // Mark status as 'In Review' in the DB immediately to prevent double-fetching
            if let Err(e) = self
                .db
                .update_patchset_status(patchset.id, ReviewStatus::InReview.as_str())
                .await
            {
                error!(
                    "Failed to update status to In Review for {}: {}",
                    patchset.id, e
                );
                continue;
            }
            patchset.status = Some(ReviewStatus::InReview.as_str().to_string());

            let context = ReviewContext {
                semaphore: self.semaphore.clone(),
                llm_semaphore: self.llm_semaphore.clone(),
                db: self.db.clone(),
                settings: self.settings.clone(),
                baseline_registry: self.baseline_registry.clone(),
                quota_manager: self.quota_manager.clone(),
                target_review_count,
                provider: self.provider.clone(),
                activity: self.activity.clone(),
                cancels: self.cancels.clone(),
            };

            tokio::spawn(async move {
                let _permit = permit;
                Self::review_patchset_task(context, patchset).await;
            });
        }

        Ok(())
    }

    async fn release_embargoed_results(&self) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let patchsets = self.db.get_releasable_embargoed_patchsets(now, 10).await?;

        if patchsets.is_empty() {
            return Ok(());
        }

        info!(
            "Found {} embargoed patchsets eligible for release",
            patchsets.len()
        );

        for patchset in patchsets {
            let patchset_id = patchset.id;
            info!("Releasing embargo for patchset {}", patchset_id);

            let context = ReviewContext {
                semaphore: self.semaphore.clone(),
                llm_semaphore: self.llm_semaphore.clone(),
                db: self.db.clone(),
                settings: self.settings.clone(),
                baseline_registry: self.baseline_registry.clone(),
                quota_manager: self.quota_manager.clone(),
                target_review_count: 1,
                provider: self.provider.clone(),
                activity: self.activity.clone(),
                cancels: self.cancels.clone(),
            };

            if let Err(e) = Self::release_patchset_results(&context, &patchset).await {
                error!("Failed to release patchset {}: {}", patchset_id, e);
            }
        }

        Ok(())
    }

    async fn release_patchset_results(ctx: &ReviewContext, patchset: &PatchsetRow) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if !ctx
            .db
            .claim_patchset_embargo_release(patchset.id, now)
            .await?
        {
            info!(
                "Patchset {} is no longer eligible for embargo release or is already claimed",
                patchset.id
            );
            return Ok(());
        }

        let result = Self::queue_patchset_notifications(ctx, patchset).await;
        if result.is_err()
            && let Err(e) = ctx
                .db
                .clear_patchset_embargo_release_claim(patchset.id)
                .await
        {
            error!(
                "Failed to clear embargo release claim for patchset {}: {}",
                patchset.id, e
            );
        }
        result
    }

    async fn queue_patchset_notifications(
        ctx: &ReviewContext,
        patchset: &PatchsetRow,
    ) -> Result<()> {
        let reviews = ctx
            .db
            .get_completed_reviews_for_release(patchset.id)
            .await?;

        for review in reviews {
            let ps_msg_id = patchset
                .message_id
                .as_deref()
                .unwrap_or(&review.patch_message_id);
            Self::queue_notifications(
                ctx,
                patchset.id,
                review.patch_id,
                &review.patch_message_id,
                ps_msg_id,
                review.index,
                &review.inline_review,
                Some(&review.findings),
                &review.summary,
            )
            .await?;
        }

        ctx.db.clear_patchset_embargo(patchset.id).await?;
        info!("Embargo released successfully for patchset {}", patchset.id);
        Ok(())
    }

    async fn review_patchset_task(ctx: ReviewContext, patchset: PatchsetRow) {
        let patchset_id = patchset.id;
        info!("Starting review for patchset {}", patchset_id);

        // Held for the lifetime of the task: whichever way this returns, every
        // entry for this patchset is cleared rather than left claiming work is
        // still running.
        let _activity = crate::activity::PatchsetActivityGuard::new(
            ctx.activity.clone(),
            patchset_id,
            crate::activity::Phase::Queued,
        );

        // Clock for the whole patchset, covering every patch and every retry.
        //
        // Seeded from what is already banked, because a review interrupted by a
        // restart is not redone from scratch: patches that completed keep their
        // reviews and are skipped on the next run, so the earlier run bought
        // work the final review still rests on. Counting only the run that
        // happened to finish reported a fraction of what the review cost.
        let patchset_review_started = std::time::Instant::now();
        let carried_seconds = ctx
            .db
            .get_patchset_review_duration(patchset_id)
            .await
            .unwrap_or(0);
        if carried_seconds > 0 {
            info!(
                "Resuming review of patchset {} with {} already spent",
                patchset_id,
                crate::activity::format_duration(carried_seconds)
            );
        }
        let review_clock = |now: std::time::Instant| carried_seconds + now.elapsed().as_secs();

        // Checkpointed while the review runs, not only at the end: a daemon
        // killed mid-review never reaches the end, and that is exactly the run
        // whose time would otherwise go unrecorded.
        let _clock_guard = ReviewClockGuard::new(
            ctx.db.clone(),
            patchset_id,
            carried_seconds,
            patchset_review_started,
        );

        // Makes this review reachable by a cancel request for as long as it runs.
        // Bound to a named variable (not `_`) so it lives for the whole task;
        // dropping it early would silently make the review uncancellable.
        // `run_review_tool` looks the token up via `ctx.cancels.token_for`.
        let _cancel_guard = crate::cancel::CancelGuard::new(ctx.cancels.clone(), patchset_id);

        if let Err(e) = ctx
            .db
            .update_patchset_status(patchset_id, ReviewStatus::InReview.as_str())
            .await
        {
            error!(
                "Failed to update status to In Review for {}: {}",
                patchset_id, e
            );
            return;
        }

        let diffs = match ctx.db.get_patch_diffs(patchset_id).await {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to fetch diffs for {}: {}", patchset_id, e);
                let _ = ctx.db.update_patchset_status(patchset_id, "Failed").await;
                return;
            }
        };

        // patches_json for input payload (contains all patches)
        let patches_json: Vec<_> = diffs
            .iter()
            .map(|(_id, idx, diff, subj, auth, date, msg_id)| {
                let is_sha = msg_id.len() == 40 && msg_id.chars().all(|c| c.is_ascii_hexdigit());
                json!({
                    "index": idx,
                    "diff": diff,
                    "subject": subj,
                    "author": auth,
                    "date": date,
                    "message_id": msg_id,
                    "commit_id": if is_sha { Some(msg_id) } else { None }
                })
            })
            .collect();

        // Determine Baseline Candidates and check patchset size limits
        let mut all_files = Vec::new();

        for p in patches_json.iter() {
            if let Some(diff_str) = p["diff"].as_str() {
                let files = extract_files_from_diff(diff_str);
                all_files.extend(files);
            }
        }

        all_files.sort();
        all_files.dedup();

        let body = if let Some(mid) = &patchset.message_id {
            ctx.db.get_message_body(mid).await.unwrap_or(None)
        } else if let Some(first_patch_msg_id) =
            patches_json.first().and_then(|p| p["message_id"].as_str())
        {
            ctx.db
                .get_message_body(first_patch_msg_id)
                .await
                .unwrap_or(None)
        } else {
            None
        };

        let subject = patchset.subject.clone().unwrap_or("Unknown".to_string());
        let candidates = if let Some(bid) = patchset.baseline_id {
            if let Ok(Some(commit)) = ctx.db.get_baseline_commit(bid).await {
                info!(
                    "Using forced baseline commit {} from ingestion for patchset {}",
                    commit, patchset_id
                );
                vec![BaselineResolution::Commit(commit)]
            } else {
                ctx.baseline_registry
                    .resolve_candidates(&all_files, &subject, body.as_deref())
                    .await
            }
        } else {
            ctx.baseline_registry
                .resolve_candidates(&all_files, &subject, body.as_deref())
                .await
        };

        // 1. Find a working baseline (apply series)
        let (found_baseline, patch_commits, logs) =
            Self::prepare_baseline_worktree(&ctx, patchset_id, &candidates, &diffs).await;

        let prompts_hash = Some(crate::prompt_bundle::GIT_HASH);

        // Save findings to patchset
        if let Some((resolution, baseline_id, worktree)) = found_baseline {
            let _ = ctx
                .db
                .update_patchset_baseline_info(
                    patchset_id,
                    Some(baseline_id),
                    Some(ctx.settings.ai.model.as_str()),
                    prompts_hash,
                    Some(logs.as_str()),
                    Some(ctx.settings.ai.provider.as_str()),
                )
                .await;

            // patches_json for input payload (contains all patches)
            let patches_json: Vec<_> = diffs
                .iter()
                .map(|(_id, idx, diff, subj, auth, date, msg_id)| {
                    let resolved_sha = patch_commits.get(idx);
                    let is_msg_sha =
                        msg_id.len() == 40 && msg_id.chars().all(|c| c.is_ascii_hexdigit());

                    let commit_id = if let Some(sha) = resolved_sha {
                        Some(sha.as_str())
                    } else if is_msg_sha {
                        Some(msg_id.as_str())
                    } else {
                        None
                    };

                    json!({
                        "index": idx,
                        "diff": diff,
                        "subject": subj,
                        "author": auth,
                        "date": date,
                        "message_id": msg_id,
                        "commit_id": commit_id
                    })
                })
                .collect();

            let patchset_msg_id = patchset
                .message_id
                .clone()
                .or_else(|| {
                    patches_json
                        .first()
                        .and_then(|p| p["message_id"].as_str().map(|s| s.to_string()))
                })
                .unwrap_or_default();

            // `body` is the patchset's cover letter where one exists. It has
            // fed baseline resolution above; passing it on is what gets the
            // author's series intent in front of the model.
            let input_payload = json!({
                "id": patchset_id,
                "message_id": patchset_msg_id,
                "subject": patchset.subject.clone().unwrap_or("Unknown".to_string()),
                "cover_letter": body,
                "patches": patches_json
            });

            let skip_filters: Vec<String> = patchset
                .skip_filters
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            let only_filters: Vec<String> = patchset
                .only_filters
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();

            let compile_glob = |pattern: &str| -> regex::Regex {
                let mut re = String::from("^");
                for c in pattern.chars() {
                    match c {
                        '*' => re.push_str(".*"),
                        '?' => re.push('.'),
                        '.' | '+' | '(' | ')' | '|' | '^' | '$' | '[' | ']' | '{' | '}' | '\\' => {
                            re.push('\\');
                            re.push(c);
                        }
                        _ => re.push(c),
                    }
                }
                re.push('$');
                regex::Regex::new(&re).unwrap_or_else(|_| regex::Regex::new("a^").unwrap())
            };

            let skip_regexes: Vec<_> = skip_filters.iter().map(|f| compile_glob(f)).collect();
            let only_regexes: Vec<_> = only_filters.iter().map(|f| compile_glob(f)).collect();

            struct ValidJob {
                patch_id: i64,
                index: i64,
                commit_sha: Option<String>,
                diff: String,
            }

            // 2. Run Reviews
            let mut review_success = true; // Optimistic
            let mut failed_patches = 0;

            let mut valid_jobs = Vec::new();

            for (patch_id, index, diff, _subj, _auth, _date, _msg_id) in &diffs {
                let mut should_skip = false;

                // Opt-out logic
                if skip_regexes.iter().any(|re| re.is_match(_subj)) {
                    info!(
                        "Skipping patch {}/{} (subject matches skip filter)",
                        patchset_id, index
                    );
                    should_skip = true;
                }

                // Opt-in logic (if only_filters is not empty, subject MUST match at least one)
                if !should_skip
                    && !only_regexes.is_empty()
                    && !only_regexes.iter().any(|re| re.is_match(_subj))
                {
                    info!(
                        "Skipping patch {}/{} (subject does not match any only filter)",
                        patchset_id, index
                    );
                    should_skip = true;
                }

                if !should_skip {
                    let mut unique_patch_files = extract_files_from_diff(diff);
                    unique_patch_files.sort();
                    unique_patch_files.dedup();
                    let patch_files_count = unique_patch_files.len();

                    let patch_lines_changed = diff
                        .lines()
                        .filter(|line| {
                            (line.starts_with('+') && !line.starts_with("+++"))
                                || (line.starts_with('-') && !line.starts_with("---"))
                        })
                        .count();

                    if patch_lines_changed > ctx.settings.review.max_lines_changed
                        || patch_files_count > ctx.settings.review.max_files_touched
                    {
                        info!(
                            "Skipping patch {}/{} (exceeds size limits: {} lines, {} files)",
                            patchset_id, index, patch_lines_changed, patch_files_count
                        );
                        should_skip = true;
                    }
                }

                if should_skip {
                    let _ = ctx.db.update_patch_status(*patch_id, "Skipped").await;
                    continue;
                }
                let commit_sha = patch_commits.get(index).cloned();

                valid_jobs.push(ValidJob {
                    patch_id: *patch_id,
                    index: *index,
                    commit_sha,
                    diff: diff.to_string(),
                });
            }

            // Reverse so that pop() processes in the original order (index 1 first)
            valid_jobs.reverse();
            let total_valid = valid_jobs.len();

            // The one place the patchset's own phase moves once work begins. Set
            // here rather than from the patches, so the clock behind it spans
            // the whole review instead of restarting whenever a patch does.
            ctx.activity.update(
                crate::activity::ActivityKey::Patchset(patchset_id),
                crate::activity::Phase::ReviewingPatches {
                    patches: total_valid,
                },
            );
            let valid_jobs_queue = Arc::new(tokio::sync::Mutex::new(valid_jobs));
            let mut handles = Vec::new();
            let baseline_ref_str = resolution.as_str();

            // Try concurrent processing using extra available permits in the semaphore
            if total_valid > 1 {
                let worktree_path = worktree.path.clone();
                while let Ok(permit) = ctx.semaphore.clone().try_acquire_owned() {
                    let queue = valid_jobs_queue.clone();
                    let ctx_clone = ctx.clone();
                    let input_payload_clone = input_payload.clone();
                    let prompts_hash_clone = prompts_hash.map(|s| s.to_string());
                    let baseline_ref_clone = baseline_ref_str.to_string();
                    let baseline_id_clone = baseline_id;
                    let embargo_until_clone = patchset.embargo_until;
                    let worktree_path_clone = worktree_path.clone();

                    let handle = tokio::spawn(async move {
                        let mut failed = 0;
                        loop {
                            let job = {
                                let mut q = queue.lock().await;
                                q.pop()
                            };
                            if let Some(job) = job {
                                match Self::process_patch_review(
                                    &ctx_clone,
                                    patchset_id,
                                    job.patch_id,
                                    job.index,
                                    &baseline_ref_clone,
                                    Some(baseline_id_clone),
                                    &input_payload_clone,
                                    job.commit_sha,
                                    prompts_hash_clone.as_deref(),
                                    Some(&worktree_path_clone), // Reuse the single worktree!
                                    &job.diff,
                                    embargo_until_clone,
                                )
                                .await
                                {
                                    Ok(PatchResult::Success) => {}
                                    _ => failed += 1,
                                }
                            } else {
                                break;
                            }
                        }
                        drop(permit);
                        failed
                    });
                    handles.push(handle);

                    // Don't spawn more workers than remaining jobs
                    let remaining = {
                        let q = valid_jobs_queue.lock().await;
                        q.len()
                    };
                    if remaining <= handles.len() {
                        break;
                    }
                }
            }

            // Main worker loop uses the existing worktree
            let mut main_failed = 0;
            loop {
                let job = {
                    let mut q = valid_jobs_queue.lock().await;
                    q.pop()
                };
                if let Some(job) = job {
                    match Self::process_patch_review(
                        &ctx,
                        patchset_id,
                        job.patch_id,
                        job.index,
                        &baseline_ref_str,
                        Some(baseline_id),
                        &input_payload,
                        job.commit_sha,
                        prompts_hash,
                        Some(&worktree.path),
                        &job.diff,
                        patchset.embargo_until,
                    )
                    .await
                    {
                        Ok(PatchResult::Success) => {}
                        _ => main_failed += 1,
                    }
                } else {
                    break;
                }
            }
            failed_patches += main_failed;

            for handle in handles {
                match handle.await {
                    Ok(failed) => failed_patches += failed,
                    Err(e) => {
                        error!("Review worker tokio task crashed/panicked: {}", e);
                        failed_patches += 1;
                    }
                }
            }

            if failed_patches > 0 {
                review_success = false;
            }

            // Cleanup worktree here since we kept it alive for reuse
            let _ = worktree.remove().await;

            let current_status = ctx.db.get_patchset_status(patchset_id).await.ok().flatten();
            if current_status.as_deref() == Some(ReviewStatus::Cancelled.as_str()) {
                info!(
                    "Patchset {} was cancelled during review, preserving status",
                    patchset_id
                );
            } else {
                let final_status = if review_success {
                    ReviewStatus::Reviewed.as_str().to_string()
                } else {
                    ReviewStatus::Failed.as_str().to_string()
                };

                let _ = ctx
                    .db
                    .update_patchset_status(patchset_id, &final_status)
                    .await;

                let elapsed = review_clock(patchset_review_started);
                let _ = ctx
                    .db
                    .set_patchset_review_duration(patchset_id, elapsed)
                    .await;
                info!(
                    "Patchset {} finished review in {} ({})",
                    patchset_id,
                    crate::activity::format_duration(elapsed),
                    final_status
                );

                if review_success
                    && patchset.embargo_until.is_some()
                    && let Err(e) = Self::release_patchset_results(&ctx, &patchset).await
                {
                    error!(
                        "Failed to release clean patchset {} immediately: {}",
                        patchset_id, e
                    );
                }
            }
        } else {
            // No baseline found
            warn!("No working baseline found for patchset {}", patchset_id);
            let _ = ctx
                .db
                .update_patchset_baseline_info(
                    patchset_id,
                    None,
                    Some(ctx.settings.ai.model.as_str()),
                    prompts_hash,
                    Some(logs.as_str()),
                    Some(ctx.settings.ai.provider.as_str()),
                )
                .await;

            let _ = ctx
                .db
                .update_patchset_status(patchset_id, ReviewStatus::FailedToApply.as_str())
                .await;
        }

        if let Some(stats) = ctx.provider.cache_stats() {
            use crate::ai::cache::fmt_thousands;
            let total_hits = stats.hits_this_session + stats.hits_prev_session;
            let total_tokens = stats.tokens_saved_this_session + stats.tokens_saved_prev_session;
            if total_hits > 0 {
                info!(
                    "Patchset {} cache summary — {} hits ({} this session, {} previous), {} tokens saved ({} this session, {} previous)",
                    patchset_id,
                    fmt_thousands(total_hits),
                    fmt_thousands(stats.hits_this_session),
                    fmt_thousands(stats.hits_prev_session),
                    fmt_thousands(total_tokens),
                    fmt_thousands(stats.tokens_saved_this_session),
                    fmt_thousands(stats.tokens_saved_prev_session),
                );
            }
        }
    }

    async fn prepare_baseline_worktree(
        ctx: &ReviewContext,
        patchset_id: i64,
        candidates: &[BaselineResolution],
        diffs: &[(i64, i64, String, String, String, i64, String)],
    ) -> (
        Option<(BaselineResolution, i64, GitWorktree)>,
        HashMap<i64, String>,
        String,
    ) {
        let mut attempts: Vec<BaselineAttempt> = Vec::new();
        let repo_path = PathBuf::from(&ctx.settings.git.repository_path);
        let mainline_remote = ctx.baseline_registry.mainline_remote_name();
        let mut tested_shas = std::collections::HashSet::new();

        for candidate in candidates {
            let baseline_ref = candidate.as_str();
            let mut current_log = format!("Trying baseline: {}\n", baseline_ref);
            let mut current_status = "Failed".to_string();

            // Check remote
            if let BaselineResolution::RemoteTarget { url, name, .. } = candidate
                && let Err(e) = ensure_remote(&repo_path, name, url, false).await
            {
                let msg = format!("Failed to fetch remote {}: {}\n", redact_secret(url), e);
                current_log.push_str(&msg);
                error!("{}", msg.trim());
                attempts.push(BaselineAttempt {
                    baseline: baseline_ref.clone(),
                    status: current_status,
                    log: current_log,
                });
                continue;
            }

            // Resolve SHA
            let baseline_sha = match get_commit_hash(&repo_path, &baseline_ref).await {
                Ok(sha) => sha,
                Err(e) => {
                    if let BaselineResolution::Commit(sha_str) = candidate {
                        // Attempt to fetch the missing commit from the
                        // mainline remote.
                        let _ = Command::new("git")
                            .current_dir(&repo_path)
                            .args(["fetch", mainline_remote, sha_str])
                            .output()
                            .await;
                        // Retry resolving
                        match get_commit_hash(&repo_path, &baseline_ref).await {
                            Ok(sha) => sha,
                            Err(_) => {
                                // b4 can emit annotated tag object SHAs as
                                // base-commit (e.g. the tag object for
                                // v7.2-rc2). Those aren't fetchable by SHA,
                                // so pull tags from the mainline remote and
                                // retry.
                                let _ = Command::new("git")
                                    .current_dir(&repo_path)
                                    .args(["fetch", mainline_remote, "--tags"])
                                    .output()
                                    .await;
                                match get_commit_hash(&repo_path, &baseline_ref).await {
                                    Ok(sha) => sha,
                                    Err(e2) => {
                                        let msg = format!(
                                            "Failed to resolve baseline ref {}: {}\n",
                                            baseline_ref, e2
                                        );
                                        current_log.push_str(&msg);
                                        attempts.push(BaselineAttempt {
                                            baseline: baseline_ref.clone(),
                                            status: current_status,
                                            log: current_log,
                                        });
                                        continue;
                                    }
                                }
                            }
                        }
                    } else {
                        let msg =
                            format!("Failed to resolve baseline ref {}: {}\n", baseline_ref, e);
                        current_log.push_str(&msg);
                        attempts.push(BaselineAttempt {
                            baseline: baseline_ref.clone(),
                            status: current_status,
                            log: current_log,
                        });
                        continue;
                    }
                }
            };

            if !tested_shas.insert(baseline_sha.clone()) {
                info!("Skipping duplicate baseline SHA {}", baseline_sha);
                continue;
            }

            let baseline_display = format!("{} ({})", baseline_ref, baseline_sha);
            current_log = format!("Trying baseline: {}\n", baseline_display);

            // Worktree
            let worktree = match GitWorktree::new(
                &repo_path,
                &baseline_sha,
                Some(Path::new(&ctx.settings.review.worktree_dir)),
            )
            .await
            {
                Ok(wt) => wt,
                Err(e) => {
                    let msg = format!("Failed to create worktree: {}\n", e);
                    current_log.push_str(&msg);
                    attempts.push(BaselineAttempt {
                        baseline: baseline_ref.clone(),
                        status: current_status,
                        log: current_log,
                    });
                    continue;
                }
            };

            // Apply patches
            let mut patch_commits = HashMap::new();
            let mut application_failed = false;
            let mut apply_logs = String::new();

            for (i, (patch_id, index, diff, subject, author, date_ts, msg_id)) in
                diffs.iter().enumerate()
            {
                let date_str = std::process::Command::new("date")
                    .arg("-R")
                    .arg("-d")
                    .arg(format!("@{}", date_ts))
                    .output()
                    .ok()
                    .and_then(|o| {
                        if o.status.success() {
                            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                let mut applied = false;
                let mut fast_path_taken = false;

                // Optimization: If message_id is a valid SHA, just checkout it
                if msg_id.len() == 40 && msg_id.chars().all(|c| c.is_ascii_hexdigit()) {
                    let next_is_sha = diffs
                        .get(i + 1)
                        .map(|(_, _, _, _, _, _, next_msg)| {
                            next_msg.len() == 40 && next_msg.chars().all(|c| c.is_ascii_hexdigit())
                        })
                        .unwrap_or(true); // If there is no next item, we treat it as "safe to skip reset" (we are done)

                    if next_is_sha {
                        // Fast path: verify existence only, skip checkout
                        match get_commit_hash(&worktree.path, msg_id).await {
                            Ok(_) => {
                                applied = true;
                                fast_path_taken = true;
                            }
                            Err(e) => {
                                let msg = format!("Commit {} missing: {}\n", msg_id, e);
                                info!("{}", msg);
                                apply_logs.push_str(&msg);
                            }
                        }
                    } else {
                        match worktree.reset_hard(msg_id).await {
                            Ok(_) => applied = true,
                            Err(e) => {
                                let msg = format!("Failed to reset hard to {}: {}\n", msg_id, e);
                                info!("{}", msg);
                                apply_logs.push_str(&msg);
                            }
                        }
                    }
                }

                if !applied {
                    let mbox = format!(
                        "From: {}\nDate: {}\nSubject: {}\n\n{}\n",
                        author, date_str, subject, diff
                    );

                    // Try git am
                    if (worktree.apply_patch(&mbox).await).is_ok() {
                        applied = true;
                    }
                }

                if applied {
                    if fast_path_taken {
                        patch_commits.insert(*index, msg_id.clone());
                    } else if let Ok(sha) = get_commit_hash(&worktree.path, "HEAD").await {
                        patch_commits.insert(*index, sha);
                    }
                } else {
                    let msg = format!(
                        "Patch {}/{} (ID: {}) failed to apply.\n",
                        patchset_id, index, patch_id
                    );
                    apply_logs.push_str(&msg);
                    application_failed = true;
                    break;
                }
            }

            if !application_failed {
                current_log.push_str("Application successful.\n");
                current_status = "Applied".to_string();

                attempts.push(BaselineAttempt {
                    baseline: baseline_display.clone(),
                    status: current_status,
                    log: current_log,
                });

                // Create baseline in DB
                let baseline_id = {
                    let (repo_url, branch) = match candidate {
                        BaselineResolution::RemoteTarget { url, .. } => {
                            (Some(url.as_str()), Some(baseline_ref.as_str()))
                        }
                        _ => (None, Some(baseline_ref.as_str())),
                    };
                    ctx.db
                        .create_baseline(repo_url, branch, Some(&baseline_sha))
                        .await
                        .ok() // If fail, we just proceed. Better to have it.
                };

                // Serialize attempts to JSON
                let logs_json = serde_json::to_string(&attempts).unwrap_or_default();

                if let Some(bid) = baseline_id {
                    info!(
                        "Baseline found for patchset {}: {} ({} attempts)",
                        patchset_id,
                        candidate.as_str(),
                        attempts.len()
                    );
                    return (
                        Some((candidate.clone(), bid, worktree)),
                        patch_commits,
                        logs_json,
                    );
                }
                // Fallback if DB insert fails, though unlikely
                // We still return success but maybe log error.
                // We do not continue loop as application succeeded.
                // Just return success without ID.
                // This path is tricky. Let's assume ID creation works or we fail this attempt.
                // If we fail, we clean up.
                // For now, let's treat it as success but maybe missing ID is fatal for `Some` return.
                // But `create_baseline` returns Result<i64>.
                // If it fails, we can't associate baseline.
                // Let's count it as failure.
                // Re-push attempt with failure.
                // Actually we already pushed "Applied".
                // Let's modify the last attempt status if we can't save to DB.
                if let Some(last) = attempts.last_mut() {
                    last.status = "DB Error".to_string();
                    last.log.push_str("Failed to record baseline in DB.\n");
                }
            } else {
                current_log.push_str(&apply_logs);
                current_log.push_str("Application failed.\n");
                attempts.push(BaselineAttempt {
                    baseline: baseline_display.clone(),
                    status: current_status,
                    log: current_log,
                });
            }

            // Clean up failed worktree
            let _ = worktree.remove().await;
        }

        let logs_json = serde_json::to_string(&attempts).unwrap_or_default();
        (None, HashMap::new(), logs_json)
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_patch_review(
        ctx: &ReviewContext,
        patchset_id: i64,
        patch_id: i64,
        index: i64,
        baseline_ref: &str,
        baseline_id: Option<i64>,
        input_payload: &Value,
        commit_sha: Option<String>,
        prompts_hash: Option<&str>,
        worktree_path: Option<&Path>,
        diff: &str,
        embargo_until: Option<i64>,
    ) -> Result<PatchResult> {
        info!(
            "Reviewing patch {}/{} (ID: {})",
            patchset_id, index, patch_id
        );

        let successful_count = ctx
            .db
            .count_successful_reviews(patchset_id, patch_id, baseline_id)
            .await?;

        if successful_count >= ctx.target_review_count {
            info!(
                "Patch {}/{} (ID: {}) already has {} successful reviews with baseline {:?} (target: {}). Skipping.",
                patchset_id,
                index,
                patch_id,
                successful_count,
                baseline_id,
                ctx.target_review_count
            );
            return Ok(PatchResult::Success);
        }

        if ctx
            .db
            .has_failed_review(patchset_id, patch_id, baseline_id)
            .await?
        {
            info!(
                "Patch {}/{} (ID: {}) already has a failed review. Skipping to keep it visible.",
                patchset_id, index, patch_id
            );
            let _ = ctx.db.update_patch_status(patch_id, "Failed").await;
            return Ok(PatchResult::ReviewFailed);
        }

        let files = extract_files_from_diff(diff);

        // An empty commit can never be applied, so reviewing it fails and the
        // retry loop then fails identically three more times. Kept separate from
        // the ignored-files rule below, which cannot match this case anyway:
        // that rule means "touches only ignored files" and so requires at least
        // one file.
        if files.is_empty() {
            info!(
                "Skipping review for patch {}/{} (ID: {}) as it changes no files.",
                patchset_id, index, patch_id
            );
            let _ = ctx.db.update_patch_status(patch_id, "Skipped").await;
            return Ok(PatchResult::Success);
        }

        if !files.is_empty()
            && files.iter().all(|f| {
                ctx.settings
                    .review
                    .ignore_files
                    .iter()
                    .any(|ignored| f.starts_with(ignored))
            })
        {
            info!(
                "Skipping review for patch {}/{} (ID: {}) as it touches only ignored files.",
                patchset_id, index, patch_id
            );

            let review_id = if let Some(id) = ctx
                .db
                .get_pending_review_id(patchset_id, Some(patch_id))
                .await?
            {
                id
            } else {
                ctx.db
                    .create_review(
                        patchset_id,
                        Some(patch_id),
                        &ctx.settings.ai.provider,
                        &ctx.settings.ai.model,
                        baseline_id,
                        prompts_hash,
                    )
                    .await?
            };

            let _ = ctx
                .db
                .complete_review(
                    review_id,
                    ReviewStatus::Skipped.as_str(),
                    "Skipped: touches only ignored files",
                    None,
                    None,
                    None,
                    None,
                )
                .await;

            return Ok(PatchResult::Success);
        }

        let mut retries = 0;
        let max_retries = ctx.settings.review.max_retries;

        // Started before the retry loop, so the recorded duration covers every
        // attempt. Timing each attempt separately would undercount a review that
        // needed three goes, which is exactly the case worth measuring.
        let review_started = std::time::Instant::now();

        let mut existing_pending_review_id = ctx
            .db
            .get_pending_review_id(patchset_id, Some(patch_id))
            .await?;

        loop {
            let review_id = if let Some(id) = existing_pending_review_id.take() {
                id
            } else {
                ctx.db
                    .create_review(
                        patchset_id,
                        Some(patch_id),
                        &ctx.settings.ai.provider,
                        &ctx.settings.ai.model,
                        baseline_id,
                        prompts_hash,
                    )
                    .await?
            };

            let _ = ctx
                .db
                .update_review_status(review_id, ReviewStatus::InReview.as_str(), None)
                .await;

            // Stamped before the work runs, not after, so a row that never
            // reaches a terminal state still records which attempt it was.
            let _ = ctx.db.set_review_attempt(review_id, retries + 1).await;

            let result = run_review_tool(
                patchset_id,
                patch_id,
                input_payload,
                &ctx.settings,
                ctx.db.clone(),
                baseline_ref,
                Some(index),
                commit_sha.clone(),
                ctx.quota_manager.clone(),
                review_id,
                worktree_path,
                ctx.provider.clone(),
                ctx.llm_semaphore.clone(),
                ctx.activity.clone(),
                ctx.cancels.token_for(patchset_id),
                retries + 1,
                max_retries + 1,
            )
            .await;

            // Stamped once per attempt with the cumulative elapsed time, so
            // whichever row ends up final carries the full total. Recorded here
            // rather than at each of the loop's several exit points, which would
            // be easy to add an eighth to and forget.
            let _ = ctx
                .db
                .set_review_duration(review_id, review_started.elapsed().as_secs())
                .await;

            match result {
                Ok(ReviewToolOutcome {
                    output: json_output,
                    // Set when the worker produced this result only because it
                    // was asked to wind down early. Its coverage is short of
                    // what was planned however complete the output looks.
                    timed_out: salvaged,
                }) => {
                    let patches_status = json_output["patches"].as_array();
                    let target_applied = patches_status
                        .and_then(|arr| arr.iter().find(|p| p["index"] == index))
                        .map(|p| p["status"] == "applied")
                        .unwrap_or(false);

                    // Record incomplete coverage before anything else touches the
                    // review row, so a partial review can never be presented as
                    // a complete one.
                    if let Some(failures) = json_output
                        .get("review")
                        .and_then(|r| r.get("stage_failures"))
                        .or_else(|| json_output.get("stage_failures"))
                        .and_then(|f| f.as_array())
                        .filter(|f| !f.is_empty())
                    {
                        let stages: Vec<&str> = failures
                            .iter()
                            .filter_map(|f| f["stage"].as_str())
                            .collect();
                        warn!(
                            "Review {} completed with {} incomplete stage(s): {:?}",
                            review_id,
                            failures.len(),
                            stages
                        );
                        let _ = ctx
                            .db
                            .set_review_stage_failures(
                                review_id,
                                &serde_json::Value::Array(failures.clone()).to_string(),
                            )
                            .await;
                    }

                    let history = json_output.get("history");
                    let logs_str = if let Some(h) = history {
                        let mut scrubbed = h.clone();
                        crate::ai::scrub_thought_signatures(&mut scrubbed);
                        serde_json::to_string_pretty(&scrubbed).ok()
                    } else {
                        None
                    };

                    let interaction_id = if let Some(tokens_in) = json_output["tokens_in"].as_u64()
                    {
                        let i_id = generate_interaction_id();
                        let input_ctx = json_output["input_context"].as_str().unwrap_or("");
                        let output_raw = if let Some(r) = json_output.get("review") {
                            r.to_string()
                        } else if let Some(e) = json_output.get("error") {
                            e.to_string()
                        } else {
                            String::new()
                        };

                        let _ = ctx
                            .db
                            .create_ai_interaction(AiInteractionParams {
                                id: &i_id,
                                parent_id: None,
                                workflow_id: None,
                                provider: &ctx.settings.ai.provider,
                                model: &ctx.settings.ai.model,
                                input: input_ctx,
                                output: &output_raw,
                                tokens_in: tokens_in as u32,
                                tokens_out: json_output["tokens_out"].as_u64().unwrap_or(0) as u32,
                                tokens_cached: json_output["tokens_cached"].as_u64().unwrap_or(0)
                                    as u32,
                            })
                            .await;
                        Some(i_id)
                    } else {
                        None
                    };

                    if target_applied {
                        if let Some(error_msg) = json_output["error"].as_str() {
                            error!(
                                "Review tool returned error for ps={} idx={}: {}",
                                patchset_id, index, error_msg
                            );
                            let _ = ctx
                                .db
                                .complete_review(
                                    review_id,
                                    ReviewStatus::Failed.as_str(),
                                    error_msg,
                                    None,
                                    interaction_id.as_deref(),
                                    None,
                                    logs_str.as_deref(),
                                )
                                .await;

                            if retries < max_retries && !salvaged {
                                retries += 1;
                                continue;
                            } else {
                                let _ = ctx.db.update_patch_status(patch_id, "Failed").await;
                                return Ok(PatchResult::ReviewFailed);
                            }
                        } else if let Some(review_content) = json_output.get("review") {
                            if !review_content.is_null() {
                                if let Some(findings_arr) =
                                    review_content.get("findings").and_then(|f| f.as_array())
                                {
                                    for f in findings_arr {
                                        let severity_str = f["severity"].as_str().unwrap_or("Low");
                                        let severity = Severity::from_str(severity_str);

                                        let problem =
                                            f["problem"].as_str().unwrap_or("").to_string();
                                        let severity_explanation = f["severity_explanation"]
                                            .as_str()
                                            .map(|s| s.to_string());
                                        let preexisting = f["preexisting"].as_bool();
                                        let locations = f.get("locations").cloned();

                                        ctx.db
                                            .create_finding(Finding {
                                                review_id,
                                                severity,
                                                severity_explanation,
                                                problem,
                                                preexisting,
                                                locations,
                                            })
                                            .await?;
                                    }
                                }

                                let summary =
                                    review_content["summary"].as_str().unwrap_or("").to_string();

                                // Salvaged, not finished. The findings above are
                                // real and worth keeping -- they are why the
                                // wind-down happens at all -- but the review did
                                // not cover what it planned to, so it must not
                                // read as one that did. Recorded as Failed so
                                // every gate that asks "was this reviewed?"
                                // still answers no, with the patch marked
                                // Partial to say there is something here worth
                                // opening.
                                if salvaged {
                                    let inline_review = json_output["inline_review"].as_str();
                                    let _ = ctx
                                        .db
                                        .complete_review(
                                            review_id,
                                            ReviewStatus::Failed.as_str(),
                                            "Tool error: Review tool timed out (active time \
                                             exceeded); findings salvaged from the stages that \
                                             had completed",
                                            Some(&summary),
                                            interaction_id.as_deref(),
                                            inline_review,
                                            logs_str.as_deref(),
                                        )
                                        .await;
                                    warn!(
                                        "Review {} timed out; salvaged {} finding(s) from the \
                                         stages that completed",
                                        review_id,
                                        review_content
                                            .get("findings")
                                            .and_then(|f| f.as_array())
                                            .map(|f| f.len())
                                            .unwrap_or(0)
                                    );
                                    let _ = ctx.db.update_patch_status(patch_id, "Partial").await;
                                    // No retry: the deadline was already spent
                                    // once, and an identical second attempt has
                                    // no more room than the first had.
                                    return Ok(PatchResult::ReviewFailed);
                                }

                                let result_desc = "Review completed successfully.";

                                let inline_review = json_output["inline_review"].as_str();

                                let mut db_success = true;

                                if let Err(e) = ctx
                                    .db
                                    .complete_review(
                                        review_id,
                                        ReviewStatus::Reviewed.as_str(),
                                        result_desc,
                                        Some(&summary),
                                        interaction_id.as_deref(),
                                        inline_review,
                                        logs_str.as_deref(),
                                    )
                                    .await
                                {
                                    error!("Failed to save review completion: {}", e);
                                    db_success = false;
                                }

                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs() as i64;

                                if db_success {
                                    let inline_opt = inline_review;
                                    if let Some(inline) = inline_opt {
                                        let mut skip_notify = false;
                                        if let Some(until) = embargo_until.filter(|&u| u > now) {
                                            info!(
                                                "Review completed but embargoed until {} for patch {}/{}",
                                                until, patchset_id, index
                                            );
                                            skip_notify = true;
                                        }

                                        if !skip_notify {
                                            let patch_msg_id = input_payload["patches"]
                                                .as_array()
                                                .and_then(|arr| {
                                                    arr.iter().find(|p| p["index"] == index)
                                                })
                                                .and_then(|p| p["message_id"].as_str())
                                                .unwrap_or("");

                                            let patchset_msg_id = input_payload["message_id"]
                                                .as_str()
                                                .unwrap_or(patch_msg_id);

                                            if let Err(e) = Self::queue_notifications(
                                                ctx,
                                                patchset_id,
                                                patch_id,
                                                patch_msg_id,
                                                patchset_msg_id,
                                                index,
                                                inline,
                                                review_content["findings"].as_array(),
                                                &summary,
                                            )
                                            .await
                                            {
                                                error!(
                                                    "Failed to queue email for patch {}/{} (ID: {}): {}",
                                                    patchset_id, index, patch_id, e
                                                );
                                            }
                                        }
                                    }
                                }
                                if !db_success {
                                    let _ = ctx.db.update_patch_status(patch_id, "Failed").await;
                                    return Ok(PatchResult::ReviewFailed);
                                }
                                // Failing the patchset over a notification
                                // would strand it: get_pending_patchsets()
                                // selects only Pending rows.
                                let _ = ctx.db.update_patch_status(patch_id, "Reviewed").await;
                                return Ok(PatchResult::Success);
                            } else if ctx.settings.ai.no_ai {
                                info!(
                                    "Review skipped as requested for ps={} idx={}",
                                    patchset_id, index
                                );
                                let _ = ctx
                                    .db
                                    .complete_review(
                                        review_id,
                                        ReviewStatus::Skipped.as_str(),
                                        "Skipped AI review via --no-ai",
                                        None,
                                        interaction_id.as_deref(),
                                        None,
                                        logs_str.as_deref(),
                                    )
                                    .await;
                                let _ = ctx.db.update_patch_status(patch_id, "Skipped").await;
                                return Ok(PatchResult::Success);
                            } else {
                                let _ = ctx
                                    .db
                                    .complete_review(
                                        review_id,
                                        ReviewStatus::Failed.as_str(),
                                        "AI returned null response",
                                        None,
                                        interaction_id.as_deref(),
                                        None,
                                        logs_str.as_deref(),
                                    )
                                    .await;
                                if retries < max_retries && !salvaged {
                                    retries += 1;
                                    continue;
                                } else {
                                    let _ = ctx.db.update_patch_status(patch_id, "Failed").await;
                                    return Ok(PatchResult::ReviewFailed);
                                }
                            }
                        } else {
                            let error_msg = json_output["error"]
                                .as_str()
                                .unwrap_or("Missing review content");
                            let _ = ctx
                                .db
                                .complete_review(
                                    review_id,
                                    ReviewStatus::Failed.as_str(),
                                    error_msg,
                                    None,
                                    interaction_id.as_deref(),
                                    None,
                                    logs_str.as_deref(),
                                )
                                .await;
                            let _ = ctx.db.update_patch_status(patch_id, "Failed").await;
                            return Ok(PatchResult::ReviewFailed);
                        }
                    } else {
                        // Tool failed to process or missing patches array
                        let error_msg = json_output["error"]
                            .as_str()
                            .unwrap_or("Tool failed to return patch status");
                        let _ = ctx
                            .db
                            .complete_review(
                                review_id,
                                ReviewStatus::Failed.as_str(),
                                error_msg,
                                None,
                                interaction_id.as_deref(),
                                None,
                                logs_str.as_deref(),
                            )
                            .await;
                        if retries < max_retries && !salvaged {
                            retries += 1;
                            continue;
                        }
                        let _ = ctx.db.update_patch_status(patch_id, "Failed").await;
                        return Ok(PatchResult::ReviewFailed);
                    }
                }
                Err(e) => {
                    error!("Review execution failed for {}: {}", patchset_id, e);
                    // The stage timings and the streamed conversation were
                    // already written against this review by the tool itself,
                    // so they survive being completed here with nothing else.
                    let timed_out = e.downcast_ref::<ReviewTimedOut>().is_some();
                    let _ = ctx
                        .db
                        .complete_review(
                            review_id,
                            ReviewStatus::Failed.as_str(),
                            &format!("Tool error: {}", e),
                            None,
                            None,
                            None,
                            None,
                        )
                        .await;
                    // A review that exhausted its deadline is not retried: the
                    // next attempt gets the same budget that has already proved
                    // too small, and four of them is how one patch came to
                    // spend twelve hours producing nothing.
                    if retries < max_retries && !timed_out {
                        retries += 1;
                        continue;
                    }
                    let _ = ctx.db.update_patch_status(patch_id, "Failed").await;
                    return Ok(PatchResult::ReviewFailed);
                }
            }
        }
    }
}

/// How long a cancelled worker gets to wind down and report partial results
/// before it is killed outright.
const GRACEFUL_CANCEL_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Error text when a worker ignored the cancel request and had to be killed.
/// Distinguished from a plain timeout so the log says what actually happened.
const CANCEL_IGNORED: &str = "Review tool ignored cancellation and was killed";

/// Why the worker was asked to stop.
///
/// The two are handled differently on purpose. A cancellation means the review
/// is not wanted, so it stops as soon as the worker reports what it already
/// finished. A timeout means the review wanted more time than it was given, so
/// the worker is granted a bounded extension to turn the concerns it gathered
/// into findings -- abandoning that work is how a three-hour review comes to
/// produce nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindDown {
    Cancelled,
    TimedOut,
}

/// A review that exhausted its deadline.
///
/// Typed rather than a bare message so the caller can recognise it without
/// matching on text; the `Display` text is unchanged from what it replaced, so
/// what is stored and shown stays the same.
#[derive(Debug, thiserror::Error)]
#[error("Review tool timed out (active time exceeded)")]
struct ReviewTimedOut;

/// What a run of the review tool produced, and whether it had to be salvaged.
///
/// `timed_out` cannot be read off the output: the worker reports a normal
/// result either way, and only the daemon knows it asked for that result early.
#[derive(Debug)]
struct ReviewToolOutcome {
    output: serde_json::Value,
    timed_out: bool,
}

/// Renders an elapsed duration the way the rest of the review reports time.
fn format_elapsed(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{}s", s),
        (0, m, s) => format!("{}m {}s", m, s),
        (h, m, _) => format!("{}h {}m", h, m),
    }
}

/// Copies a review's streamed conversation into its durable `logs` column.
///
/// Only for a review that ended without the worker reporting its own history.
/// The streamed entries are whole messages, so what lands here has the same
/// shape as a log the worker would have reported -- but it ends wherever the
/// review stopped, so it says where it came from rather than passing for a
/// complete record.
async fn promote_live_log_to_review(db: &Arc<Database>, review_id: i64) -> Result<()> {
    if db.review_has_logs(review_id).await? {
        return Ok(());
    }

    let entries = db.get_review_log_entries(review_id, None).await?;
    if entries.is_empty() {
        return Ok(());
    }

    let mut log = vec![serde_json::json!({
        "role": "system",
        "content": "[The review did not finish, so the worker never reported its own \
                    conversation. What follows was streamed while it ran, and stops at \
                    whatever had arrived by then.]",
    })];
    log.extend(entries);

    db.set_review_logs(review_id, &serde_json::Value::Array(log).to_string())
        .await
}

/// Resolves when the token fires; never resolves when there is no token.
///
/// `select!` needs a future either way, and a review with no registered token
/// simply cannot be cancelled.
async fn wait_for_cancel(token: &Option<tokio_util::sync::CancellationToken>) {
    match token {
        Some(t) => t.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// Recovers the stage name from a request's context tag.
///
/// The tag is built by the stage layer as `[ps:N p:M] [s:NAME] `. The daemon
/// sees the request and nothing else, so this is the only thing tying a model
/// call back to the stage that made it.
fn stage_from_context_tag(tag: &str) -> Option<String> {
    let start = tag.find("[s:")? + 3;
    let rest = &tag[start..];
    let end = rest.find(']')?;
    let name = rest[..end].trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Reports a stage's wait for a model slot to the activity registry.
///
/// Queueing is invisible without this. A stage waiting its turn behind its
/// siblings reports "awaiting model" exactly as a stage with a slow model does,
/// and the two call for opposite responses -- more concurrency, or patience.
struct StageSlotObserver {
    activity: Arc<crate::activity::ActivityRegistry>,
    patchset_id: i64,
    patch_id: i64,
}

impl StageSlotObserver {
    /// Only the wait is written. This side does not know which turn the request
    /// belongs to -- the worker owns those numbers -- so writing a whole phase
    /// here would overwrite them and leave the stage reading "starting" for
    /// most of its life.
    fn mark(&self, request: &crate::ai::AiRequest, wait: crate::activity::StageWait) {
        let Some(stage) = request
            .context_tag
            .as_deref()
            .and_then(stage_from_context_tag)
        else {
            return;
        };
        self.activity.update_stage_wait(
            crate::activity::ActivityKey::PatchsetStage {
                patchset_id: self.patchset_id,
                patch_id: self.patch_id,
                stage: stage.clone(),
            },
            stage,
            wait,
        );
    }
}

impl crate::ai::concurrency_limited_provider::SlotObserver for StageSlotObserver {
    fn queued(&self, request: &crate::ai::AiRequest) {
        self.mark(request, crate::activity::StageWait::Queued);
    }

    fn running(&self, request: &crate::ai::AiRequest) {
        self.mark(request, crate::activity::StageWait::Model);
    }
}

/// The stage name when this payload marks a stage beginning.
fn started_stage(payload: &serde_json::Value) -> Option<String> {
    match payload.get("kind")?.as_str()? {
        "stage_started" | "stage_turn" => Some(payload.get("stage")?.as_str()?.to_string()),
        _ => None,
    }
}

/// The stage name when this payload marks a stage ending.
fn finished_stage(payload: &serde_json::Value) -> Option<String> {
    match payload.get("kind")?.as_str()? {
        "stage_finished" => Some(payload.get("stage")?.as_str()?.to_string()),
        _ => None,
    }
}

/// What a worker progress message should do to the activity registry.
#[derive(Debug, PartialEq, Eq)]
enum ProgressUpdate {
    Set(crate::activity::ActivityKey, crate::activity::Phase),
    /// The unit is done. Recorded as finished rather than deleted, so a
    /// completed stage stays visible beside the ones still running.
    Finished(crate::activity::ActivityKey),
}

/// Translates a worker progress payload into a registry update.
///
/// Stage events are routed to per-stage keys because the component review
/// stages run concurrently; sharing one key would make the reported phase flap
/// between whichever stage reported last and hide a wedged stage behind its
/// siblings.
///
/// Returns `None` for anything unrecognised — progress reporting is advisory and
/// must never be able to fail a review.
fn progress_update(
    patchset_id: i64,
    patch_id: i64,
    payload: &serde_json::Value,
    attempt: u32,
    max_attempts: u32,
) -> Option<ProgressUpdate> {
    use crate::activity::{ActivityKey, Phase, StageWait};

    let stage_key = |payload: &serde_json::Value| -> Option<(ActivityKey, String)> {
        let stage = payload.get("stage")?.as_str()?.to_string();
        Some((
            ActivityKey::PatchsetStage {
                patchset_id,
                patch_id,
                stage: stage.clone(),
            },
            stage,
        ))
    };

    match payload.get("kind")?.as_str()? {
        // Planning is per patch -- each patch's worker decides which stages that
        // patch needs, and `attempt` is that patch's retry count. Keyed by patch
        // so several planning at once do not overwrite each other, and so the
        // patchset's own entry is left to describe the patchset.
        "planning" => Some(ProgressUpdate::Set(
            ActivityKey::PatchsetPatch {
                patchset_id,
                patch_id,
            },
            Phase::Planning {
                attempt,
                max_attempts,
            },
        )),
        "stage_started" => {
            let (key, stage) = stage_key(payload)?;
            Some(ProgressUpdate::Set(
                key,
                Phase::Stage {
                    stage,
                    turn: 0,
                    max_turns: 0,
                    waiting: StageWait::Model,
                },
            ))
        }
        // A turn beginning means the request has gone to the model.
        "stage_turn" => {
            let (key, stage) = stage_key(payload)?;
            Some(ProgressUpdate::Set(
                key,
                Phase::Stage {
                    stage,
                    turn: payload.get("turn")?.as_u64()? as usize,
                    max_turns: payload.get("max_turns")?.as_u64()? as usize,
                    waiting: StageWait::Model,
                },
            ))
        }
        // The model answered and the worker is now running git. An empty tool
        // list means they finished, so the turn is back to awaiting the model.
        "stage_tools" => {
            let (key, stage) = stage_key(payload)?;
            let tools: Vec<String> = payload
                .get("tools")
                .and_then(|t| t.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            Some(ProgressUpdate::Set(
                key,
                Phase::Stage {
                    stage,
                    turn: payload.get("turn").and_then(|t| t.as_u64()).unwrap_or(0) as usize,
                    max_turns: payload
                        .get("max_turns")
                        .and_then(|t| t.as_u64())
                        .unwrap_or(0) as usize,
                    // Empty means the tools finished, so the turn is back with
                    // the model.
                    waiting: if tools.is_empty() {
                        StageWait::Model
                    } else {
                        StageWait::Tools { names: tools }
                    },
                },
            ))
        }
        // A rate limit with no Retry-After header sleeps a flat 60s, which is
        // otherwise indistinguishable from a very slow model.
        "stage_backoff" => {
            let (key, stage) = stage_key(payload)?;
            let retry = payload.get("retry_in_seconds").and_then(|r| r.as_u64());
            Some(ProgressUpdate::Set(
                key,
                Phase::Stage {
                    stage,
                    turn: payload.get("turn").and_then(|t| t.as_u64()).unwrap_or(0) as usize,
                    max_turns: payload
                        .get("max_turns")
                        .and_then(|t| t.as_u64())
                        .unwrap_or(0) as usize,
                    waiting: match retry {
                        Some(secs) => StageWait::RateLimited {
                            retry_in_seconds: secs,
                        },
                        // The wait is over; the request goes back to the model.
                        None => StageWait::Model,
                    },
                },
            ))
        }
        // A finished stage stops existing rather than lingering in a done state,
        // so the live view lists exactly the stages still running.
        "stage_finished" => Some(ProgressUpdate::Finished(stage_key(payload)?.0)),
        // A failed stage is not cleared: the review carries on without it, and
        // a stage that silently disappeared would be indistinguishable from one
        // that never ran. Replacing the phase is what stops the entry claiming
        // to be mid-turn -- the bug this arm exists to fix.
        "stage_failed" => {
            let (key, stage) = stage_key(payload)?;
            Some(ProgressUpdate::Set(
                key,
                Phase::StageFailed {
                    stage,
                    reason: payload
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    cancelled: payload
                        .get("cancelled")
                        .and_then(|c| c.as_bool())
                        .unwrap_or(false),
                },
            ))
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_review_tool(
    patchset_id: i64,
    patch_id: i64,
    input_payload: &serde_json::Value,
    settings: &Settings,
    db: Arc<Database>,
    baseline: &str,
    review_index: Option<i64>,
    review_commit: Option<String>,
    quota_manager: Arc<QuotaManager>,
    review_id: i64,
    worktree_path: Option<&Path>,
    provider: Arc<dyn AiProvider>,
    llm_semaphore: Arc<Semaphore>,
    activity: Arc<crate::activity::ActivityRegistry>,
    cancel_token: Option<tokio_util::sync::CancellationToken>,
    attempt: u32,
    max_attempts: u32,
) -> Result<ReviewToolOutcome> {
    let cmd = default_worker_command()?;
    run_review_tool_with_cmd(
        cmd,
        patchset_id,
        patch_id,
        input_payload,
        settings,
        db,
        baseline,
        review_index,
        review_commit,
        quota_manager,
        review_id,
        worktree_path,
        provider,
        llm_semaphore,
        activity,
        cancel_token,
        attempt,
        max_attempts,
    )
    .await
}

fn default_worker_command() -> Result<Command> {
    let exe_path = std::env::current_exe()?;
    let bin_dir = exe_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let is_test_runner = bin_dir.file_name().and_then(|f| f.to_str()) == Some("deps");

    let mut c = if !is_test_runner {
        Command::new(exe_path)
    } else if let Some(parent) = bin_dir.parent()
        && parent.join("sashiko").exists()
    {
        Command::new(parent.join("sashiko"))
    } else if bin_dir.join("sashiko").exists() {
        Command::new(bin_dir.join("sashiko"))
    } else {
        warn!("Running in test runner without compiled sashiko binary, falling back to cargo run");
        let mut c = Command::new("cargo");
        c.args(["run", "--bin", "sashiko", "--"]);
        c
    };
    c.arg("worker");
    Ok(c)
}

#[allow(clippy::too_many_arguments)]
async fn run_review_tool_with_cmd(
    mut cmd: Command,
    patchset_id: i64,
    patch_id: i64,
    input_payload: &serde_json::Value,
    settings: &Settings,
    db: Arc<Database>,
    baseline: &str,
    review_index: Option<i64>,
    review_commit: Option<String>,
    quota_manager: Arc<QuotaManager>,
    review_id: i64,
    worktree_path: Option<&Path>,
    provider: Arc<dyn AiProvider>,
    llm_semaphore: Arc<Semaphore>,
    activity: Arc<crate::activity::ActivityRegistry>,
    cancel_token: Option<tokio_util::sync::CancellationToken>,
    attempt: u32,
    max_attempts: u32,
) -> Result<ReviewToolOutcome> {
    // Cap concurrent model calls with the shared limiter instead of taking the
    // semaphore by hand around each call. This also releases the permit as soon
    // as the call returns, so a request that is backing off no longer occupies
    // a slot while it sleeps.
    let provider: Arc<dyn AiProvider> = Arc::new(
        crate::ai::concurrency_limited_provider::ConcurrencyLimitedProvider::new(
            provider,
            llm_semaphore.clone(),
        )
        // The limiter is the only layer that knows a request is queued rather
        // than merely slow, so it is the only one that can say so.
        .with_observer(Arc::new(StageSlotObserver {
            activity: activity.clone(),
            patchset_id,
            patch_id,
        })),
    );
    cmd.args([
        "--json",
        "--baseline",
        baseline,
        "--worktree-dir",
        &settings.review.worktree_dir,
        "--ai-provider",
        match settings.ai.provider.as_str() {
            "claude" | "stdio-claude" | "claude-cli" | "codex-cli" | "copilot-cli" | "kiro-cli" => {
                "stdio-claude"
            }
            _ => "stdio-gemini",
        },
    ]);

    cmd.env_clear();

    // Only restore critical, non-sensitive system variables
    for var in &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM"] {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }

    cmd.env("NO_COLOR", "1");
    cmd.env("SASHIKO_LOG_PLAIN", "1");

    // Forward SASHIKO_* env vars to the child so that env-var overrides
    // (e.g. SASHIKO_AI__MODEL, SASHIKO_GIT__REPOSITORY_PATH) are visible
    // to the review binary's config loading.
    for (key, value) in std::env::vars() {
        if key.starts_with("SASHIKO_") {
            cmd.env(&key, &value);
        }
    }

    if let Some(idx) = review_index {
        cmd.arg("--review-patch-index").arg(idx.to_string());
    }

    if let Some(commit) = review_commit {
        cmd.arg("--review-commit").arg(commit);
    }

    if settings.ai.no_ai {
        cmd.arg("--no-ai");
    }

    if let Some(path) = worktree_path {
        cmd.arg("--reuse-worktree").arg(path);
    }

    if let Some(stages) = &settings.review.stages {
        let stages_str = stages
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(",");
        cmd.arg("--stages").arg(stages_str);
    }

    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn()?;

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.contains(" ERROR ")
                    || line.starts_with("Error:")
                    || line.contains("panicked")
                {
                    error!("[review-bin] {}", line);
                } else if line.contains(" WARN ") {
                    warn!("[review-bin] {}", line);
                } else {
                    info!("[review-bin] {}", line);
                }
            }
        });
    }

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("No stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("No stdout"))?;

    let stdin_writer = Arc::new(tokio::sync::Mutex::new(stdin));

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::time::Instant as TokioInstant;
    use tokio::time::{timeout, timeout_at};

    // Perform interaction with timeout
    let deadline = Arc::new(std::sync::Mutex::new(
        TokioInstant::now() + Duration::from_secs(settings.review.timeout_seconds),
    ));

    // Extra time granted once, after the deadline expires, for the worker to
    // consolidate what it gathered. Zero disables the salvage, which makes a
    // timeout fail immediately as it used to.
    let salvage_seconds = settings.review.salvage_seconds;

    // Retry rate-limited and transient failures with the shared limiter rather
    // than an open-coded loop. A review is bounded by its activity deadline
    // rather than an attempt count, and time spent waiting out a rate limit is
    // credited back so it does not consume that budget.
    let provider: Arc<dyn AiProvider> =
        Arc::new(crate::ai::backoff_provider::BackoffProvider::new(
            provider,
            quota_manager.clone(),
            Some(Arc::new(crate::ai::backoff_provider::DeadlineBudget::new(
                deadline.clone(),
            ))),
        ));

    let mut spawned_tasks = Vec::new();
    // Per-stage timing, derived from the progress events the worker already
    // emits. Doing it here rather than in the worker covers every stage
    // uniformly, including the consolidation stages that run outside the
    // concurrent block.
    //
    // Declared outside the interaction block so they survive its failure. A
    // review that ran out of time is exactly when the question "which stage was
    // this stuck in?" is worth answering, and these are the only record of it.
    let mut stage_starts: std::collections::HashMap<String, TokioInstant> =
        std::collections::HashMap::new();
    // Highest turn number seen per stage, which is how many LLM round trips
    // that stage needed before it finished.
    let mut stage_turns: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut stage_stats: Vec<(String, u64, u64)> = Vec::new();
    // Set when the daemon asks the worker to stop, for the same reason.
    let mut wind_down: Option<WindDown> = None;

    let interaction_result =
        async {
            // Send initial payload
            let mut input_str = serde_json::to_string(input_payload)?;
            input_str.push('\n');
            {
                let mut writer = stdin_writer.lock().await;
                writer.write_all(input_str.as_bytes()).await?;
                writer.flush().await?;
            }

            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut final_result: Option<Value> = None;

            let ai_started = Arc::new(AtomicBool::new(false));
            let total_tokens_used = Arc::new(AtomicUsize::new(0));
            let total_output_tokens_used = Arc::new(AtomicUsize::new(0));

            let (abort_tx, mut abort_rx) = tokio::sync::mpsc::channel::<anyhow::Error>(1);

            // Ordering for streamed log entries; the table is append-only and
            // read back by this number.
            let mut log_seq: i64 = 0;

            loop {
                let current_deadline = {
                    let d = deadline.lock().unwrap();
                    *d
                };

                tokio::select! {
                    Some(err) = abort_rx.recv() => {
                        return Err(err);
                    }
                    // Guarded so it fires once: a cancelled token stays ready
                    // forever and would otherwise spin this arm. A cancellation
                    // overrides an in-progress salvage -- someone asking for the
                    // review to stop outranks finishing it.
                    _ = wait_for_cancel(&cancel_token), if wind_down != Some(WindDown::Cancelled) => {
                        wind_down = Some(WindDown::Cancelled);
                        info!(
                            "Cancellation requested for patchset {}; asking worker to wind down",
                            patchset_id
                        );

                        // Ask politely first. A SIGKILL here would lose every
                        // stage the review had already completed; the worker
                        // stops issuing new LLM calls and reports what it has.
                        let msg = "{\"type\":\"cancel\",\"tx_id\":0,\"payload\":null}\n";
                        {
                            let mut writer = stdin_writer.lock().await;
                            let _ = writer.write_all(msg.as_bytes()).await;
                            let _ = writer.flush().await;
                        }

                        // Bound the politeness. If the worker ignores us, the
                        // deadline arm below escalates to a kill.
                        {
                            let mut d = deadline.lock().unwrap();
                            *d = TokioInstant::now() + GRACEFUL_CANCEL_WAIT;
                        }
                    }
                    line_res = timeout_at(current_deadline, lines.next_line()) => {
                        let line_result = match line_res {
                            Ok(res) => res,
                            // Already winding down and still silent: the worker
                            // had its window and did not use it.
                            Err(_) if wind_down == Some(WindDown::Cancelled) => {
                                return Err(anyhow::anyhow!("{}", CANCEL_IGNORED));
                            }
                            Err(_) if wind_down == Some(WindDown::TimedOut) => {
                                return Err(ReviewTimedOut.into());
                            }
                            // Out of time on the first expiry. Rather than
                            // abandoning the worker -- which throws away every
                            // stage it already finished -- ask it to stop
                            // analysing and consolidate what it has, and grant
                            // exactly enough extra time for that tail to run.
                            Err(_) if salvage_seconds == 0 => {
                                return Err(ReviewTimedOut.into());
                            }
                            Err(_) => {
                                wind_down = Some(WindDown::TimedOut);
                                warn!(
                                    "Review for patchset {} ran out of time; asking worker to \
                                     salvage its findings within {}s",
                                    patchset_id, salvage_seconds
                                );

                                let msg = "{\"type\":\"wind_down\",\"tx_id\":0,\"payload\":null}\n";
                                {
                                    let mut writer = stdin_writer.lock().await;
                                    let _ = writer.write_all(msg.as_bytes()).await;
                                    let _ = writer.flush().await;
                                }

                                {
                                    let mut d = deadline.lock().unwrap();
                                    *d = TokioInstant::now()
                                        + std::time::Duration::from_secs(salvage_seconds);
                                }
                                continue;
                            }
                        };

                        let line = match line_result {
                            Ok(Some(l)) => l,
                            Ok(None) => break,
                            Err(e) => {
                                tracing::error!("Error reading line from child: {}", e);
                                break;
                            }
                        };

                        // Try to parse as JSON
                        if let Ok(json_msg) = serde_json::from_str::<Value>(&line) {
                            if let Some(type_str) = json_msg.get("type").and_then(|v| v.as_str()) {
                                match type_str {
                                    "ai_request" | "ai_request_with_cache" => {
                                        if !ai_started.load(Ordering::SeqCst) {
                                            let _ = db
                                                .update_review_status(
                                                    review_id,
                                                    ReviewStatus::InReview.as_str(),
                                                    None,
                                                )
                                                .await;
                                            ai_started.store(true, Ordering::SeqCst);
                                        }

                                        let tx_id = json_msg.get("tx_id").and_then(|v| v.as_u64()).unwrap_or(0);
                                        let payload = json_msg["payload"].clone();

                                        let db_clone = db.clone();
                                        let provider_clone = provider.clone();
                                        let settings_clone = settings.clone();
                                        let stdin_clone = stdin_writer.clone();
                                        let total_tokens_used_clone = total_tokens_used.clone();
                                        let total_output_tokens_used_clone = total_output_tokens_used.clone();
                                        let abort_tx_clone = abort_tx.clone();

                                        let handle = tokio::spawn(async move {
                                            let req: AiRequest = match serde_json::from_value(payload) {
                                                Ok(r) => r,
                                                Err(e) => {
                                                    let _ = abort_tx_clone.send(anyhow::anyhow!("Failed to parse AiRequest payload: {}", e)).await;
                                                    return;
                                                }
                                            };

                                            let mut tool_calls_map = std::collections::HashMap::new();
                                            for msg in &req.messages {
                                                if let Some(ref calls) = msg.tool_calls {
                                                    for call in calls {
                                                        tool_calls_map.insert(
                                                            call.id.clone(),
                                                            (call.function_name.clone(), call.arguments.to_string()),
                                                        );
                                                    }
                                                }
                                            }

                                            for msg in &req.messages {
                                                if msg.role == crate::ai::AiRole::Tool
                                                    && let Some(ref tool_call_id) = msg.tool_call_id
                                                    && let Some((tool_name, arguments)) = tool_calls_map.get(tool_call_id)
                                                    && let Some(ref content) = msg.content
                                                {
                                                    let _ = db_clone
                                                        .update_tool_usage_length(
                                                            review_id,
                                                            tool_name,
                                                            arguments,
                                                            content.len(),
                                                        )
                                                        .await;
                                                }
                                            }

                                            let ctx_tag = req.context_tag.clone().unwrap_or_default();
                                            let resp_payload = crate::ai::LOG_CONTEXT
                                                .scope(ctx_tag, provider_clone.generate_content(req.clone()))
                                                .await;

                                            let reply = match resp_payload {
                                                Ok(p) => {
                                                    if let Some(usage) = &p.usage {
                                                        let cached = usage.cached_tokens.unwrap_or(0);
                                                        let uncached_input = usage.prompt_tokens.saturating_sub(cached);
                                                        let current_total = total_tokens_used_clone.fetch_add(uncached_input + usage.completion_tokens, Ordering::SeqCst) + uncached_input + usage.completion_tokens;
                                                        let current_output = total_output_tokens_used_clone.fetch_add(usage.completion_tokens, Ordering::SeqCst) + usage.completion_tokens;
                                                        let token_budget = settings_clone.review.max_total_tokens;
                                                        if token_budget > 0 && current_total > token_budget {
                                                            let err_msg = format!("Token budget exceeded: {} uncached input + output tokens used > {} limit", current_total, token_budget);

                                                            let payload = RemoteAiErrorPayload::new(err_msg.clone(), AiErrorClass::Fatal);
                                                            let reply = json!({
                                                                "type": "error",
                                                                "tx_id": tx_id,
                                                                "payload": payload
                                                            });
                                                            let mut reply_str = serde_json::to_string(&reply).unwrap();
                                                            reply_str.push('\n');
                                                            {
                                                                let mut writer = stdin_clone.lock().await;
                                                                let _ = writer.write_all(reply_str.as_bytes()).await;
                                                                let _ = writer.flush().await;
                                                            }

                                                            let _ = abort_tx_clone.send(ReviewError::BudgetExceeded(err_msg).into()).await;
                                                            return;
                                                        }

                                                        let output_budget = settings_clone.review.max_total_output_tokens;
                                                        if output_budget > 0 && current_output > output_budget {
                                                            let err_msg = format!("Output token budget exceeded: {} output tokens used > {} limit", current_output, output_budget);

                                                            let payload = RemoteAiErrorPayload::new(err_msg.clone(), AiErrorClass::Fatal);
                                                            let reply = json!({
                                                                "type": "error",
                                                                "tx_id": tx_id,
                                                                "payload": payload
                                                            });
                                                            let mut reply_str = serde_json::to_string(&reply).unwrap();
                                                            reply_str.push('\n');
                                                            {
                                                                let mut writer = stdin_clone.lock().await;
                                                                let _ = writer.write_all(reply_str.as_bytes()).await;
                                                                let _ = writer.flush().await;
                                                            }

                                                            let _ = abort_tx_clone.send(ReviewError::BudgetExceeded(err_msg).into()).await;
                                                            return;
                                                        }
                                                    }

                                                    if let Some(tool_calls) = &p.tool_calls {
                                                        for call in tool_calls {
                                                            let _ = db_clone
                                                                .create_tool_usage(crate::db::ToolUsage {
                                                                    review_id,
                                                                    provider: settings_clone.ai.provider.clone(),
                                                                    model: settings_clone.ai.model.clone(),
                                                                    tool_name: call.function_name.clone(),
                                                                    arguments: Some(
                                                                        call.arguments.to_string(),
                                                                    ),
                                                                    output_length: 0,
                                                                    // Read from the call rather than
                                                                    // assumed, so the default staying
                                                                    // "review" is the tool's decision
                                                                    // and not restated here.
                                                                    repo: Some(
                                                                        call.arguments
                                                                            [crate::toolbox::REPO_ARG]
                                                                            .as_str()
                                                                            .unwrap_or(
                                                                                crate::toolbox::REPO_ARG_REVIEW,
                                                                            )
                                                                            .to_string(),
                                                                    ),
                                                                })
                                                                .await;
                                                        }
                                                    }

                                                    Some(json!({
                                                        "type": "ai_response",
                                                        "tx_id": tx_id,
                                                        "payload": p
                                                    }))
                                                }
                                                Err(e) => {
                                                    let class = classify_ai_error(&e);
                                                    let message = e.to_string();
                                                    let payload = RemoteAiErrorPayload::new(message, class);
                                                    let reply = json!({
                                                        "type": "error",
                                                        "tx_id": tx_id,
                                                        "payload": payload
                                                    });

                                                    let mut reply_str = serde_json::to_string(&reply).unwrap();
                                                    reply_str.push('\n');
                                                    {
                                                        let mut writer = stdin_clone.lock().await;
                                                        if let Err(e) = writer.write_all(reply_str.as_bytes()).await {
                                                            error!("Failed to write AI response to child: {}", e);
                                                        }
                                                        let _ = writer.flush().await;
                                                    }

                                                    None
                                                }
                                            };

                                            if let Some(reply) = reply {
                                                let mut reply_str = serde_json::to_string(&reply).unwrap();
                                                reply_str.push('\n');
                                                {
                                                    let mut writer = stdin_clone.lock().await;
                                                    if let Err(e) = writer.write_all(reply_str.as_bytes()).await {
                                                        error!("Failed to write AI response to child: {}", e);
                                                    }
                                                    let _ = writer.flush().await;
                                                }
                                            }
                                        });
                                        spawned_tasks.push(handle);
                                    }
                                    "progress" => {
                                        let p = &json_msg["payload"];

                                        // Streamed conversation, so a running
                                        // review can be read before it finishes.
                                        // It rides the progress envelope, so it
                                        // is dispatched on the payload `kind`,
                                        // not the message type. Best-effort: a
                                        // failed insert must never disturb the
                                        // review.
                                        if p["kind"] == "log_entry" {
                                            let message = &p["message"];
                                            let role = message["role"]
                                                .as_str()
                                                .unwrap_or("unknown");
                                            let stage = p["stage"].as_str();
                                            if let Err(e) = db
                                                .append_review_log_entry(
                                                    review_id,
                                                    log_seq,
                                                    stage,
                                                    role,
                                                    &message.to_string(),
                                                )
                                                .await
                                            {
                                                tracing::debug!(
                                                    "Failed to store log entry: {}",
                                                    e
                                                );
                                            }
                                            log_seq += 1;
                                        }

                                        // Advisory only: never let a malformed
                                        // progress line disturb the review.
                                        match progress_update(
                                            patchset_id,
                                            patch_id,
                                            &json_msg["payload"],
                                            attempt,
                                            max_attempts,
                                        ) {
                                            Some(ProgressUpdate::Set(key, phase)) => {
                                                // A stage's clock starts when it
                                                // first reports, not on each turn.
                                                if let Some(stage) = started_stage(
                                                    &json_msg["payload"],
                                                ) {
                                                    stage_starts
                                                        .entry(stage.clone())
                                                        .or_insert_with(TokioInstant::now);
                                                    if let Some(turn) = json_msg["payload"]
                                                        .get("turn")
                                                        .and_then(|t| t.as_u64())
                                                    {
                                                        let seen = stage_turns
                                                            .entry(stage)
                                                            .or_insert(0);
                                                        *seen = (*seen).max(turn);
                                                    }
                                                }
                                                // First stage to start flips this
                                                // patch from planning to reviewing.
                                                // Keyed by patch, not patchset:
                                                // sibling patches plan and review
                                                // at overlapping times, and
                                                // alternating their phases on one
                                                // key restarted the clock the
                                                // top-level display reads.
                                                if matches!(
                                                    key,
                                                    crate::activity::ActivityKey::PatchsetStage { .. }
                                                ) {
                                                    activity.update(
                                                        crate::activity::ActivityKey::PatchsetPatch {
                                                            patchset_id,
                                                            patch_id,
                                                        },
                                                        crate::activity::Phase::Reviewing {
                                                            attempt,
                                                            max_attempts,
                                                        },
                                                    );
                                                }
                                                // A turn report carries the
                                                // worker's numbers but only its
                                                // own view of the wait, which is
                                                // always "awaiting model" -- it
                                                // cannot see that the daemon has
                                                // the request queued for a slot.
                                                // Routed so it updates the
                                                // numbers without overwriting
                                                // that.
                                                match phase {
                                                    crate::activity::Phase::Stage {
                                                        stage,
                                                        turn,
                                                        max_turns,
                                                        waiting: crate::activity::StageWait::Model,
                                                    } => activity
                                                        .update_stage_turn(key, stage, turn, max_turns),
                                                    other => activity.update(key, other),
                                                }
                                            }
                                            Some(ProgressUpdate::Finished(key)) => {
                                                // Marked done rather than
                                                // removed. Dropping it made the
                                                // patch's card show less and
                                                // less as the review went on,
                                                // with the finished work only
                                                // reappearing once the whole
                                                // review ended. Carries the same
                                                // numbers the recorded breakdown
                                                // will show, so the row settles
                                                // in place.
                                                let mut done = None;
                                                if let Some(stage) =
                                                    finished_stage(&json_msg["payload"])
                                                    && let Some(started) =
                                                        stage_starts.remove(&stage)
                                                {
                                                    let seconds =
                                                        started.elapsed().as_secs();
                                                    let turns = stage_turns
                                                        .remove(&stage)
                                                        .unwrap_or(0);
                                                    stage_stats
                                                        .push((stage.clone(), seconds, turns));
                                                    done = Some(
                                                        crate::activity::Phase::StageDone {
                                                            stage,
                                                            seconds,
                                                            turns,
                                                        },
                                                    );
                                                }
                                                match done {
                                                    Some(phase) => {
                                                        activity.update(key, phase)
                                                    }
                                                    // No start time means nothing
                                                    // to report about it, and a
                                                    // row saying only "done" is
                                                    // worse than none.
                                                    None => activity.clear(&key),
                                                }
                                            }
                                            None => {}
                                        }
                                    }
                                    _ => {
                                        // Unknown type. Assume it's result if it matches result structure.
                                        if json_msg.get("patchset_id").is_some() {
                                            final_result = Some(json_msg);
                                            break;
                                        }
                                    }
                                }
                            } else {
                                // No type. Result?
                                if json_msg.get("patchset_id").is_some() {
                                    final_result = Some(json_msg);
                                    break;
                                }
                            }
                        } else {
                            // Non-JSON line. Log it.
                            warn!("Review tool stdout: {}", line);
                        }
                    }
                }
            }

            // Return result
            if let Some(res) = final_result {
                Ok(res)
            } else {
                Err(anyhow::anyhow!("Review tool finished without valid result"))
            }
        }
        .await;

    // Recorded however the run ended. On the failure path these are the only
    // account of what the review was doing, which is precisely when someone
    // needs to know.
    if !stage_stats.is_empty() {
        let payload: Vec<serde_json::Value> = stage_stats
            .iter()
            .map(|(stage, secs, turns)| {
                serde_json::json!({"stage": stage, "seconds": secs, "turns": turns})
            })
            .collect();
        let _ = db
            .set_review_stage_durations(review_id, &serde_json::Value::Array(payload).to_string())
            .await;
    }

    // Stages that started and never reported finishing. On a timeout this is
    // the answer to "which stage was it stuck in?", which nothing else records:
    // the activity registry is cleared when the review ends, and the worker
    // cannot report a stage it never got to the end of.
    if interaction_result.is_err() && !stage_starts.is_empty() {
        let mut unfinished: Vec<(&String, &TokioInstant)> = stage_starts.iter().collect();
        unfinished
            .sort_by_key(|(stage, _)| crate::worker::kernel_workflow::stage_order(stage.as_str()));
        let failures: Vec<serde_json::Value> = unfinished
            .iter()
            .map(|(stage, started)| {
                let turns = stage_turns.get(*stage).copied().unwrap_or(0);
                serde_json::json!({
                    "stage": stage,
                    "reason": format!(
                        "still running when the review stopped, after {} and {} turn(s)",
                        format_elapsed(started.elapsed()),
                        turns
                    ),
                    // Not a fault in the stage: it was cut short, not broken.
                    "cancelled": true,
                })
            })
            .collect();
        let _ = db
            .set_review_stage_failures(review_id, &serde_json::Value::Array(failures).to_string())
            .await;
    }

    // A failed review never reaches the worker's own history, so the streamed
    // preview is the only conversation that survives it. Promote it into the
    // durable column before the rows go: retaining them would not work, because
    // the startup sweep drops entries for any review that is no longer running.
    if interaction_result.is_err()
        && let Err(e) = promote_live_log_to_review(&db, review_id).await
    {
        tracing::debug!("Failed to preserve streamed log entries: {}", e);
    }

    // Unconditional: the streamed preview has served its purpose however the
    // review ended. Leaving it behind grows the table without bound, and the
    // failure mode is slow growth rather than an error, so it is easy to miss.
    if let Err(e) = db.delete_review_log_entries(review_id).await {
        tracing::debug!("Failed to clear streamed log entries: {}", e);
    }

    // Abort all spawned AI request tasks to drop their stdin clones
    for task in spawned_tasks {
        task.abort();
    }

    {
        // Shutdown and drop the stdin writer explicitly
        if let Ok(mut writer) = stdin_writer.try_lock() {
            let _ = writer.shutdown().await;
        }
    }
    drop(stdin_writer);

    let (timed_out, cancel_ignored) = match &interaction_result {
        Err(e) => (
            e.downcast_ref::<ReviewTimedOut>().is_some(),
            e.to_string().contains(CANCEL_IGNORED),
        ),
        Ok(_) => (false, false),
    };

    if timed_out || cancel_ignored {
        if cancel_ignored {
            // Partial results are lost here, unlike the graceful path. Say so
            // rather than leaving it to be inferred from a missing report.
            error!(
                "Review tool did not exit within {}s of cancellation. Killing process; \
                 partial results are lost.",
                GRACEFUL_CANCEL_WAIT.as_secs()
            );
        } else {
            error!(
                "Review tool timed out after {} active seconds and did not salvage its \
                 findings within a further {}s. Killing process.",
                settings.review.timeout_seconds, salvage_seconds
            );
        }
        let _ = child.start_kill();
        let _ = timeout(Duration::from_secs(5), child.wait()).await;
    } else {
        let _ = child.wait().await; // Reap zombie
    }

    match interaction_result {
        Ok(json) => {
            // Update DB with patch statuses if final_result available
            if let Some(patches) = json["patches"].as_array() {
                for p in patches {
                    let idx = p["index"].as_i64().unwrap_or(0);
                    let status = p["status"].as_str().unwrap_or("error");

                    let stderr_str = p["stderr"].as_str().unwrap_or("");
                    let stdout_str = p["stdout"].as_str().unwrap_or("");
                    let am_error = p["am_error"].as_str().unwrap_or("");

                    let mut full_log = String::new();
                    if !am_error.is_empty() {
                        full_log.push_str("git am error:\n");
                        full_log.push_str(am_error);
                        full_log.push_str("\n\n");
                    }
                    if !stdout_str.is_empty() {
                        full_log.push_str("stdout:\n");
                        full_log.push_str(stdout_str);
                        full_log.push('\n');
                    }
                    if !stderr_str.is_empty() {
                        full_log.push_str("stderr:\n");
                        full_log.push_str(stderr_str);
                    }

                    let error_msg = if full_log.trim().is_empty() {
                        None
                    } else {
                        Some(full_log.as_str())
                    };

                    if let Err(e) = db
                        .update_patch_application_status(patchset_id, idx, status, error_msg)
                        .await
                    {
                        error!(
                            "Failed to update patch status for ps={} idx={}: {}",
                            patchset_id, idx, e
                        );
                    }
                }
            }
            Ok(ReviewToolOutcome {
                output: json,
                timed_out: wind_down == Some(WindDown::TimedOut),
            })
        }
        Err(e) => Err(e),
    }
}
impl Reviewer {
    #[allow(clippy::too_many_arguments)]
    async fn queue_notifications(
        ctx: &ReviewContext,
        patchset_id: i64,
        patch_id: i64,
        patch_message_id: &str,
        patchset_message_id: &str,
        index: i64,
        inline_review: &str,
        findings: Option<&Vec<Value>>,
        _summary: &str,
    ) -> Result<()> {
        let sender_address = match &ctx.settings.smtp {
            Some(s) => s.sender_address.clone(),
            None => {
                info!("SMTP not configured, recording email as disabled.");
                "sashiko-bot@localhost".to_string()
            }
        };

        let findings_count = findings.map(|f| f.len()).unwrap_or(0);

        let msg_id = patch_message_id;
        let msg_id_clean = msg_id.trim_matches(|c| c == '<' || c == '>');
        let patchset_msg_id = patchset_message_id;
        let patchset_msg_id_clean = patchset_msg_id.trim_matches(|c| c == '<' || c == '>');

        let msg_details = match ctx.db.get_message_details_by_msgid(msg_id).await? {
            Some(d) => d,
            None => return Ok(()),
        };

        let references_hdr = match &msg_details.references_hdr {
            Some(refs) if !refs.trim().is_empty() => {
                format!("{} {}", refs.trim(), msg_id_clean)
            }
            _ => msg_id_clean.to_string(),
        };

        let policy = EmailPolicyConfig::load(&ctx.settings.review.email_policy_path)
            .map_err(|e| anyhow::anyhow!("Failed to parse email policy: {}", e))?;

        let to_list: Vec<String> = msg_details
            .to
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let cc_list: Vec<String> = msg_details
            .cc
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let patch_author = msg_details.author.unwrap_or_default();
        let patch_subject = msg_details.subject.unwrap_or_default();

        let target_url = format!(
            "https://sashiko.dev/#/patchset/{}?part={}",
            patchset_msg_id_clean, index
        );

        let patchwork_policies =
            crate::email_router::EmailRouter::resolve_patchwork(&policy, &to_list, &cc_list);

        let findings_slice = findings.map(|f| f.as_slice()).unwrap_or(&[]);

        for pw_policy in &patchwork_policies {
            let check_result =
                crate::patchwork::PatchworkCheckResult::from_policy(pw_policy, findings_slice);

            // API mode: insert into patchwork_outbox for retry-queued delivery
            if let Some(api_url) = &pw_policy.api_url {
                ctx.db
                    .insert_patchwork_outbox(
                        msg_id,
                        api_url,
                        &check_result.state,
                        &check_result.description,
                        &target_url,
                        "sashiko",
                    )
                    .await?;
            }

            // Email mode: queue structured notification via email outbox
            if let Some(email_addr) = &pw_policy.email {
                let (pw_subject, pw_body) = crate::patchwork::compose_patchwork_email(
                    msg_id,
                    &check_result.state,
                    &check_result.description,
                    &target_url,
                    &patch_subject,
                );
                let pw_email_status = match &ctx.settings.smtp {
                    None => "Disabled",
                    Some(s) if s.dry_run => "Dry-Run",
                    _ => "Pending",
                };
                ctx.db
                    .insert_patchwork_notification(
                        pw_email_status,
                        email_addr,
                        &pw_subject,
                        msg_id.trim_matches(|c| c == '<' || c == '>'),
                        msg_id.trim_matches(|c| c == '<' || c == '>'),
                        &pw_body,
                    )
                    .await?;
            }

            // Neither mode configured but patchwork enabled
            if pw_policy.api_url.is_none() && pw_policy.email.is_none() {
                tracing::warn!("Patchwork enabled but no api_url or email provided");
            }
        }

        let action = EmailRouter::resolve_recipients(
            &policy,
            &to_list,
            &cc_list,
            &patch_author,
            &sender_address,
        );

        if findings_count == 0 {
            let mut sent_positive_review = false;
            if let EmailAction::Send {
                to,
                cc,
                send_positive_review,
            } = &action
                && *send_positive_review
            {
                let mut body_head = String::new();
                if let Some(body) = &msg_details.body {
                    let mut commit_msg_lines = Vec::new();
                    for line in body.lines() {
                        if line == "---" || line.starts_with("diff --git ") {
                            break;
                        }
                        commit_msg_lines.push(line);
                    }

                    let mut sob_index = None;
                    for (i, line) in commit_msg_lines.iter().enumerate().rev() {
                        if line.to_lowercase().starts_with("signed-off-by:") {
                            sob_index = Some(i);
                            break;
                        }
                    }

                    let end_index = sob_index.unwrap_or(commit_msg_lines.len().saturating_sub(1));
                    if !commit_msg_lines.is_empty() && end_index < commit_msg_lines.len() {
                        let head_lines = &commit_msg_lines[0..=end_index];
                        if head_lines.len() > 30 {
                            let top = 15;
                            let bottom = 5;
                            for line in &head_lines[0..top] {
                                body_head.push_str("> ");
                                body_head.push_str(line);
                                body_head.push('\n');
                            }
                            body_head.push_str("> [ ... ]\n");
                            for line in &head_lines
                                [head_lines.len().saturating_sub(bottom)..head_lines.len()]
                            {
                                body_head.push_str("> ");
                                body_head.push_str(line);
                                body_head.push('\n');
                            }
                        } else {
                            for line in head_lines {
                                body_head.push_str("> ");
                                body_head.push_str(line);
                                body_head.push('\n');
                            }
                        }
                    }
                }

                if !body_head.is_empty() {
                    let to_json = serde_json::to_string(&to).unwrap_or_else(|_| "[]".to_string());
                    let cc_json = serde_json::to_string(&cc).unwrap_or_else(|_| "[]".to_string());
                    let subject_prefix = if patch_subject.to_lowercase().starts_with("re:") {
                        ""
                    } else {
                        "Re: "
                    };
                    let final_subject = format!("{}{}", subject_prefix, patch_subject);
                    let final_body = format!(
                        "{}\nSashiko has reviewed this patch and found no issues. It looks great!\n\n-- \nSashiko AI review · {}\n",
                        body_head, target_url
                    );

                    ctx.db
                        .insert_email_outbox(
                            patch_id,
                            "Pending",
                            &to_json,
                            &cc_json,
                            &final_subject,
                            msg_id_clean,
                            &references_hdr,
                            &final_body,
                        )
                        .await?;
                    sent_positive_review = true;
                }
            }

            if !sent_positive_review {
                info!(
                    "No issues found for patch {}/{} (ID: {}), skipping email.",
                    patchset_id, index, patch_id
                );
                ctx.db
                    .insert_email_outbox(
                        patch_id,
                        "Skipped",
                        "[]",
                        "[]",
                        "Skipped",
                        msg_id_clean,
                        &references_hdr,
                        "Skipped due to no findings",
                    )
                    .await?;
            }
            return Ok(());
        }

        match action {
            EmailAction::Mute => {
                info!(
                    "Email policy muted email for patch {}/{} (ID: {})",
                    patchset_id, index, patch_id
                );
                ctx.db
                    .insert_email_outbox(
                        patch_id,
                        "Muted",
                        "[]",
                        "[]",
                        "Muted",
                        msg_id_clean,
                        &references_hdr,
                        "Muted by policy",
                    )
                    .await?;
            }
            EmailAction::Send { to, cc, .. } => {
                let to_json = serde_json::to_string(&to)?;
                let cc_json = serde_json::to_string(&cc)?;

                let subject_prefix = if patch_subject.to_lowercase().starts_with("re:") {
                    ""
                } else {
                    "Re: "
                };
                let final_subject = format!("{}{}", subject_prefix, patch_subject);

                let msg_id_clean = msg_id.trim_matches(|c| c == '<' || c == '>');

                let mut header = String::new();

                if let Some(findings_arr) = findings
                    && !findings_arr.is_empty()
                {
                    header.push_str(&format!(
                        "Thank you for your contribution! Sashiko AI review found {} potential issue(s) to consider:\n",
                        findings_arr.len()
                    ));

                    let mut new_findings = Vec::new();
                    let mut preexisting_findings = Vec::new();

                    for f in findings_arr {
                        let preexisting = f
                            .get("preexisting")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if preexisting {
                            preexisting_findings.push(f.clone());
                        } else {
                            new_findings.push(f.clone());
                        }
                    }

                    let sort_by_severity = |a: &Value, b: &Value| {
                        let sev_a = Severity::from_str(
                            a.get("severity").and_then(|v| v.as_str()).unwrap_or("Low"),
                        );
                        let sev_b = Severity::from_str(
                            b.get("severity").and_then(|v| v.as_str()).unwrap_or("Low"),
                        );
                        sev_b.cmp(&sev_a)
                    };

                    new_findings.sort_by(sort_by_severity);
                    preexisting_findings.sort_by(sort_by_severity);

                    let format_finding = |f: &Value| {
                        let problem = f
                            .get("problem")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Unknown issue")
                            .trim();
                        let severity = f
                            .get("severity")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Unknown");
                        format!("- [{}] {}\n", severity, problem)
                    };

                    if !new_findings.is_empty() && !preexisting_findings.is_empty() {
                        header.push_str("\nNew issues:\n");
                        for f in &new_findings {
                            header.push_str(&format_finding(f));
                        }
                        header.push_str("\nPre-existing issues:\n");
                        for f in &preexisting_findings {
                            header.push_str(&format_finding(f));
                        }
                    } else if !new_findings.is_empty() {
                        for f in &new_findings {
                            header.push_str(&format_finding(f));
                        }
                    } else if !preexisting_findings.is_empty() {
                        header.push_str("\nPre-existing issues:\n");
                        for f in &preexisting_findings {
                            header.push_str(&format_finding(f));
                        }
                    }

                    header.push_str("--\n\n");
                }

                let mut footer = String::new();

                footer.push_str(&format!("\n\n-- \nSashiko AI review · {}", target_url));

                let final_body = format!("{}{}{}", header, inline_review.trim_end(), footer);

                let status = match &ctx.settings.smtp {
                    None => "Disabled",
                    Some(s) if s.dry_run => "Dry-Run",
                    _ => "Pending",
                };

                ctx.db
                    .insert_email_outbox(
                        patch_id,
                        status,
                        &to_json,
                        &cc_json,
                        &final_subject,
                        msg_id_clean,
                        &references_hdr,
                        &final_body,
                    )
                    .await?;

                info!(
                    "Queued email for patch {}/{} (ID: {})",
                    patchset_id, index, patch_id
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::quota::QuotaManager;
    use crate::ai::{AiRequest, AiResponse, ProviderCapabilities};
    use crate::db::Database;
    use crate::settings::Settings;
    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tempfile::tempdir;

    /// The worker encodes progress and the daemon decodes it. Those two ends live
    /// in different files and different processes, so a round-trip test is what
    /// keeps the wire format honest.
    #[test]
    fn progress_events_round_trip_from_worker_to_activity_update() {
        use crate::activity::{ActivityKey, Phase, StageWait};
        use crate::local_review::{ProgressEvent, encode_progress};

        let cases = vec![
            (
                ProgressEvent::AiReviewPlanningStarted { patch_index: 0 },
                // Per patch, not per patchset: each patch's worker plans its own
                // stages, and `attempt` is that patch's retry count. Sharing the
                // patchset's key meant sibling patches overwrote each other and
                // restarted the clock the top-level display reads.
                ProgressUpdate::Set(
                    ActivityKey::PatchsetPatch {
                        patchset_id: 77,
                        patch_id: 3,
                    },
                    Phase::Planning {
                        attempt: 1,
                        max_attempts: 4,
                    },
                ),
            ),
            (
                ProgressEvent::AiReviewStageStarted {
                    patch_index: 0,
                    stage: "resources".to_string(),
                },
                ProgressUpdate::Set(
                    ActivityKey::PatchsetStage {
                        patchset_id: 77,
                        patch_id: 3,
                        stage: "resources".to_string(),
                    },
                    Phase::Stage {
                        stage: "resources".to_string(),
                        turn: 0,
                        max_turns: 0,
                        waiting: StageWait::Model,
                    },
                ),
            ),
            (
                ProgressEvent::AiReviewStageTurn {
                    patch_index: 0,
                    stage: "security".to_string(),
                    turn: 12,
                    max_turns: 100,
                },
                ProgressUpdate::Set(
                    ActivityKey::PatchsetStage {
                        patchset_id: 77,
                        patch_id: 3,
                        stage: "security".to_string(),
                    },
                    Phase::Stage {
                        stage: "security".to_string(),
                        turn: 12,
                        max_turns: 100,
                        waiting: StageWait::Model,
                    },
                ),
            ),
            // A finished stage is removed, not relabelled.
            (
                ProgressEvent::AiReviewStageFinished {
                    patch_index: 0,
                    stage: "execution-flow".to_string(),
                },
                ProgressUpdate::Finished(ActivityKey::PatchsetStage {
                    patchset_id: 77,
                    patch_id: 3,
                    stage: "execution-flow".to_string(),
                }),
            ),
            // A failed stage is relabelled, not removed: the review carries on
            // without it, and a stage that vanished would be indistinguishable
            // from one that was never planned.
            (
                ProgressEvent::AiReviewStageFailed {
                    patch_index: 0,
                    stage: "implementation".to_string(),
                    reason: "Session exceeded max turns limit (50)".to_string(),
                    cancelled: false,
                },
                ProgressUpdate::Set(
                    ActivityKey::PatchsetStage {
                        patchset_id: 77,
                        patch_id: 3,
                        stage: "implementation".to_string(),
                    },
                    Phase::StageFailed {
                        stage: "implementation".to_string(),
                        reason: "Session exceeded max turns limit (50)".to_string(),
                        cancelled: false,
                    },
                ),
            ),
            (
                ProgressEvent::AiReviewStageFailed {
                    patch_index: 0,
                    stage: "locking".to_string(),
                    reason: "Session cancelled by supervisor".to_string(),
                    cancelled: true,
                },
                ProgressUpdate::Set(
                    ActivityKey::PatchsetStage {
                        patchset_id: 77,
                        patch_id: 3,
                        stage: "locking".to_string(),
                    },
                    Phase::StageFailed {
                        stage: "locking".to_string(),
                        reason: "Session cancelled by supervisor".to_string(),
                        cancelled: true,
                    },
                ),
            ),
        ];

        for (event, expected) in cases {
            let line = encode_progress(&event).expect("event should be forwarded");
            let parsed: serde_json::Value =
                serde_json::from_str(&line).expect("encoded progress must be valid JSON");
            assert_eq!(parsed["type"], "progress");

            let update = progress_update(77, 3, &parsed["payload"], 1, 4)
                .expect("daemon must understand what the worker emitted");
            assert_eq!(update, expected);
        }
    }

    /// Concurrent stages must not share a slot, or a wedged stage would be masked
    /// by whichever sibling reported most recently.
    #[test]
    fn concurrent_stages_are_tracked_independently() {
        use crate::activity::{ActivityKey, ActivityRegistry};

        let registry = ActivityRegistry::new();
        for stage in ["goal", "implementation", "execution-flow"] {
            let payload = serde_json::json!({
                "kind": "stage_turn", "stage": stage, "turn": 4, "max_turns": 100
            });
            match progress_update(5, 3, &payload, 1, 4).unwrap() {
                ProgressUpdate::Set(key, phase) => registry.update(key, phase),
                ProgressUpdate::Finished(_) => panic!("stage_turn must not finish a stage"),
            }
        }

        assert_eq!(registry.patchset_snapshot(5).len(), 3);

        // Finishing one leaves the other two visible and still running.
        match progress_update(
            5,
            3,
            &serde_json::json!({"kind": "stage_finished", "stage": "implementation"}),
            1,
            4,
        )
        .unwrap()
        {
            ProgressUpdate::Finished(key) => {
                assert_eq!(
                    key,
                    ActivityKey::PatchsetStage {
                        patchset_id: 5,
                        patch_id: 3,
                        stage: "implementation".to_string()
                    }
                );
                // What the review loop does with it: record the numbers the
                // breakdown will show, rather than dropping the entry and
                // leaving the patch's card emptier than before.
                registry.update(
                    key,
                    crate::activity::Phase::StageDone {
                        stage: "implementation".to_string(),
                        seconds: 41,
                        turns: 5,
                    },
                );
            }
            ProgressUpdate::Set(..) => panic!("stage_finished must report a finish"),
        }

        let remaining = registry.patchset_snapshot(5);
        assert_eq!(remaining.len(), 3, "a finished stage keeps its slot");

        let finished = remaining
            .iter()
            .find(|e| e.key == "patchset:5/patch:3/stage:implementation")
            .expect("the finished stage must still be listed");
        assert_eq!(
            finished.description,
            "stage implementation done in 41s, 5 turns"
        );

        // Its siblings are untouched and still running.
        assert_eq!(
            remaining
                .iter()
                .filter(|e| e.description.contains("turn 4/100"))
                .count(),
            2
        );
    }

    #[test]
    fn progress_decoding_rejects_garbage_without_panicking() {
        // Progress is advisory: a malformed line must never be able to fail a review.
        assert!(progress_update(1, 3, &serde_json::json!({}), 1, 4).is_none());
        assert!(progress_update(1, 3, &serde_json::json!({"kind": "nonsense"}), 1, 4).is_none());
        assert!(progress_update(1, 3, &serde_json::json!({"kind": "stage_turn"}), 1, 4).is_none());
        assert!(
            progress_update(1, 3, &serde_json::json!({"kind": "stage_finished"}), 1, 4).is_none()
        );
        assert!(progress_update(1, 3, &serde_json::Value::Null, 1, 4).is_none());
    }

    #[test]
    fn interaction_ids_are_unique_with_the_same_timestamp() {
        let ids: HashSet<_> = (0..1_000)
            .map(|_| generate_interaction_id_at(1_234))
            .collect();

        assert_eq!(ids.len(), 1_000);
        assert!(ids.iter().all(|id| id.starts_with("rev_1234_")));
    }

    struct MockProvider;
    #[async_trait]
    impl AiProvider for MockProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            Ok(AiResponse {
                content: Some("<final_verdict>Mocked AI response</final_verdict>".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }
        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    struct FailingProvider;

    #[async_trait]
    impl AiProvider for FailingProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            Err(anyhow::anyhow!("fatal provider failure"))
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            0
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    struct RateLimitThenSuccessProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl AiProvider for RateLimitThenSuccessProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(crate::ai::openai::OpenAiCompatError::RateLimitExceeded(
                    std::time::Duration::from_millis(1),
                )
                .into());
            }

            Ok(AiResponse {
                content: Some("Recovered after rate limit".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            0
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    async fn run_single_ai_request_mock(
        mock_script: &str,
        provider: Arc<dyn AiProvider>,
    ) -> Result<Value> {
        Ok(
            run_single_ai_request_mock_with_cancel(mock_script, provider, None)
                .await?
                .output,
        )
    }

    async fn run_single_ai_request_mock_with_cancel(
        mock_script: &str,
        provider: Arc<dyn AiProvider>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<ReviewToolOutcome> {
        run_single_ai_request_mock_configured(mock_script, provider, cancel_token, |_| {}).await
    }

    /// As above, but lets a test set the deadlines that drive the salvage path.
    /// Both are seconds, and the real defaults are far too long for a test to
    /// wait out.
    async fn run_single_ai_request_mock_configured(
        mock_script: &str,
        provider: Arc<dyn AiProvider>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
        configure: impl FnOnce(&mut Settings),
    ) -> Result<ReviewToolOutcome> {
        let temp_dir = tempdir()?;
        let bin_path = temp_dir.path().join("mock_review");

        std::fs::write(&bin_path, mock_script)?;
        std::fs::set_permissions(&bin_path, Permissions::from_mode(0o755))?;

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.review.timeout_seconds = 5;
        configure(&mut settings);

        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;
        let quota_manager = Arc::new(QuotaManager::new());

        let thread_id = db.create_thread("msg_id_1", "Subject", 1000).await?;
        db.create_message(
            "msg_id_p1",
            thread_id,
            None,
            "Author",
            "Subject",
            1000,
            "Body",
            "",
            "",
            None,
            None,
        )
        .await?;
        let ps_id = db
            .create_patchset(
                thread_id, None, "msg_id_1", "Subject", "Author", 1000, 1, 1, "", "", None, 1,
                None, false, None, None,
            )
            .await?
            .expect("Failed to create patchset");
        let p_id = db
            .create_patch(ps_id, "msg_id_p1", 1, "diff --git a/foo.c b/foo.c\n+int x;")
            .await?;
        let review_id = db
            .create_review(ps_id, Some(p_id), "mock", "mock", None, None)
            .await?;

        run_review_tool_with_cmd(
            Command::new(&bin_path),
            ps_id,
            p_id,
            &json!({}),
            &settings,
            db,
            "HEAD",
            Some(1),
            None,
            quota_manager,
            review_id,
            None,
            provider,
            Arc::new(Semaphore::new(56)),
            crate::activity::ActivityRegistry::new(),
            cancel_token,
            1,
            1,
        )
        .await
    }

    /// Cancelling must interrupt the worker, not merely discard its result.
    ///
    /// The worker is asked to stop over stdin and gets a window to report what it
    /// already finished, so a long review is not a total loss.
    #[tokio::test]
    async fn test_cancellation_asks_worker_to_stop_and_keeps_partial_results() -> Result<()> {
        // Reads the initial payload, then waits for the cancel message and
        // reports the stages it managed to finish before stopping.
        let mock = r#"#!/bin/sh
read -r _payload
while read -r msg; do
  case "$msg" in
    *'"type":"cancel"'*)
      echo '{"patchset_id":1,"cancelled":true,"patches":[],"stage_failures":[{"stage":5,"reason":"Session cancelled by supervisor","cancelled":true}],"findings":[]}'
      exit 0
      ;;
  esac
done
"#;

        // Pre-fired so the cancel arm is ready on the first loop iteration,
        // which keeps the test deterministic rather than racing a sleep.
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();

        let outcome =
            run_single_ai_request_mock_with_cancel(mock, Arc::new(MockProvider), Some(token))
                .await?;
        let result = outcome.output;

        assert!(
            !outcome.timed_out,
            "a cancellation is not a timeout, and must not be reported as one"
        );
        assert_eq!(
            result["cancelled"], true,
            "the worker should have been told to stop"
        );

        let failures = result["stage_failures"]
            .as_array()
            .expect("partial results must survive cancellation");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["stage"], 5);
        assert_eq!(
            failures[0]["cancelled"], true,
            "a cancelled stage must not be reported as a genuine failure"
        );

        Ok(())
    }

    /// Running out of time asks the worker to salvage rather than abandoning it.
    ///
    /// The old behaviour returned the moment the deadline passed, throwing away
    /// every stage the review had already finished -- which is how a three-hour
    /// review came to report nothing at all.
    #[tokio::test]
    async fn test_timeout_asks_worker_to_salvage_its_findings() -> Result<()> {
        // Never answers the initial payload, so the deadline is reached, then
        // reports findings once asked to wind down.
        let mock = r#"#!/bin/sh
read -r _payload
while read -r msg; do
  case "$msg" in
    *'"type":"wind_down"'*)
      echo '{"patchset_id":1,"patches":[],"stage_failures":[{"stage":"locking","reason":"Stage stopped: the review ran out of time","cancelled":true}],"review":{"findings":[{"severity":"High","problem":"salvaged"}]}}'
      exit 0
      ;;
  esac
done
"#;

        let outcome =
            run_single_ai_request_mock_configured(mock, Arc::new(MockProvider), None, |settings| {
                settings.review.timeout_seconds = 1;
                settings.review.salvage_seconds = 10;
            })
            .await?;

        assert!(
            outcome.timed_out,
            "the caller has to know this result was produced under a wind-down, \
             because nothing in the output itself says so"
        );

        let findings = outcome.output["review"]["findings"]
            .as_array()
            .expect("findings gathered before the deadline must survive it");
        assert_eq!(findings.len(), 1);

        let failures = outcome.output["stage_failures"]
            .as_array()
            .expect("the stages that did not finish must be reported");
        assert_eq!(failures[0]["stage"], "locking");

        Ok(())
    }

    /// A worker that ignores the wind-down still fails as a timeout.
    ///
    /// Typed rather than matched on text, so the caller can tell a timeout from
    /// any other tool error and decline to retry it.
    #[tokio::test]
    async fn test_timeout_is_typed_when_the_worker_never_salvages() -> Result<()> {
        // Reads the payload and then ignores everything, including the
        // wind-down request.
        let mock = r#"#!/bin/sh
read -r _payload
sleep 30
"#;

        let err =
            run_single_ai_request_mock_configured(mock, Arc::new(MockProvider), None, |settings| {
                settings.review.timeout_seconds = 1;
                settings.review.salvage_seconds = 1;
            })
            .await
            .expect_err("a worker that never reports must not look like a success");

        assert!(
            err.downcast_ref::<ReviewTimedOut>().is_some(),
            "a timeout must be recognisable without matching on its message: {err}"
        );

        Ok(())
    }

    /// Stage timing is derived in the daemon from the worker's progress events,
    /// so the two ends have to agree on the wire format for it to work at all.
    #[tokio::test]
    async fn test_stage_timings_are_derived_from_worker_progress() {
        use std::collections::HashMap;

        // Replays the event sequence a real stage produces.
        let events = vec![
            serde_json::json!({"kind": "stage_started", "stage": "execution-flow"}),
            serde_json::json!({"kind": "stage_turn", "stage": "execution-flow", "turn": 1, "max_turns": 100}),
            serde_json::json!({"kind": "stage_turn", "stage": "execution-flow", "turn": 5, "max_turns": 100}),
            serde_json::json!({"kind": "stage_finished", "stage": "execution-flow"}),
        ];

        let mut turns: HashMap<String, u64> = HashMap::new();
        let mut started: HashMap<String, ()> = HashMap::new();
        let mut finished: Vec<(String, u64)> = Vec::new();

        for e in &events {
            if let Some(stage) = started_stage(e) {
                started.entry(stage.clone()).or_insert(());
                if let Some(turn) = e.get("turn").and_then(|t| t.as_u64()) {
                    let seen = turns.entry(stage).or_insert(0);
                    *seen = (*seen).max(turn);
                }
            }
            if let Some(stage) = finished_stage(e)
                && started.remove(&stage).is_some()
            {
                let turns = turns.remove(&stage).unwrap_or(0);
                finished.push((stage, turns));
            }
        }

        assert_eq!(
            finished,
            vec![("execution-flow".to_string(), 5u64)],
            "a stage should report the highest turn it reached, not the count of events"
        );
        assert!(
            started.is_empty(),
            "a finished stage must not stay open, or its timing would never be recorded"
        );
    }

    /// A stalled stage is either waiting on the model or grinding through git,
    /// and those need different responses. The worker reports the switch; this
    /// checks the report survives the wire.
    #[test]
    fn tool_execution_is_distinguishable_from_waiting_on_the_model() {
        use crate::activity::{ActivityKey, Phase, StageWait};
        use crate::local_review::{ProgressEvent, encode_progress};

        let running = ProgressEvent::AiReviewStageTools {
            patch_index: 0,
            stage: "resources".to_string(),
            tools: vec!["git_grep".to_string()],
            turn: 9,
            max_turns: 100,
        };
        let line = encode_progress(&running).expect("tool events must be forwarded");
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();

        let update = progress_update(7, 2, &parsed["payload"], 1, 4).expect("daemon must decode");
        let ProgressUpdate::Set(key, phase) = update else {
            panic!("tool events set a phase, they do not clear one");
        };
        assert_eq!(
            key,
            ActivityKey::PatchsetStage {
                patchset_id: 7,
                patch_id: 2,
                stage: "resources".to_string()
            }
        );
        assert_eq!(
            phase,
            Phase::Stage {
                stage: "resources".to_string(),
                turn: 9,
                max_turns: 100,
                waiting: StageWait::Tools {
                    names: vec!["git_grep".to_string()]
                },
            }
        );
        assert_eq!(
            phase.describe(),
            "stage resources, turn 9/100 (running git_grep)"
        );

        // An empty tool list means they finished: back to awaiting the model,
        // without losing the turn number.
        let done = ProgressEvent::AiReviewStageTools {
            patch_index: 0,
            stage: "resources".to_string(),
            tools: vec![],
            turn: 9,
            max_turns: 100,
        };
        let line = encode_progress(&done).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        let ProgressUpdate::Set(_, phase) =
            progress_update(7, 2, &parsed["payload"], 1, 4).expect("daemon must decode")
        else {
            panic!("expected a phase update");
        };
        assert_eq!(
            phase.describe(),
            "stage resources, turn 9/100 (awaiting model)"
        );
    }

    /// The two sub-phases are the same stage, so the stage clock must not reset
    /// when a turn switches between them — otherwise `age_seconds` would measure
    /// the sub-phase rather than the stage.
    #[test]
    fn switching_between_model_and_tools_keeps_the_stage_clock() {
        use crate::activity::{ActivityKey, ActivityRegistry, Phase, StageWait};

        let reg = ActivityRegistry::new();
        let key = ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 1,
            stage: "resources".to_string(),
        };

        reg.update(
            key.clone(),
            Phase::Stage {
                stage: "resources".to_string(),
                turn: 1,
                max_turns: 100,
                waiting: StageWait::Model,
            },
        );
        let age_before = reg.get(&key).unwrap().age_seconds;

        reg.update(
            key.clone(),
            Phase::Stage {
                stage: "resources".to_string(),
                turn: 1,
                max_turns: 100,
                waiting: StageWait::Tools {
                    names: vec!["git_grep".to_string()],
                },
            },
        );

        assert!(reg.get(&key).unwrap().age_seconds >= age_before);
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage resources, turn 1/100 (running git_grep)"
        );
    }

    /// Every wait a stage can be blocked on must be distinguishable, since each
    /// calls for a different response.
    #[test]
    fn every_stage_wait_reads_differently() {
        use crate::activity::{Phase, StageWait};

        let describe = |waiting| {
            Phase::Stage {
                stage: "resources".to_string(),
                turn: 9,
                max_turns: 100,
                waiting,
            }
            .describe()
        };

        assert_eq!(
            describe(StageWait::Model),
            "stage resources, turn 9/100 (awaiting model)"
        );
        assert_eq!(
            describe(StageWait::Queued),
            "stage resources, turn 9/100 (queued for a model slot)"
        );
        assert_eq!(
            describe(StageWait::RateLimited {
                retry_in_seconds: 60
            }),
            "stage resources, turn 9/100 (rate limited, retrying in 60s)"
        );
        assert_eq!(
            describe(StageWait::Tools {
                names: vec!["git_grep".to_string()]
            }),
            "stage resources, turn 9/100 (running git_grep)"
        );
    }

    /// A backoff ending must put the turn back with the model rather than
    /// leaving it stuck reporting a rate limit that has expired.
    #[test]
    fn backoff_events_round_trip_and_clear() {
        use crate::activity::{Phase, StageWait};
        use crate::local_review::{ProgressEvent, encode_progress};

        let decode = |retry: Option<u64>| {
            let line = encode_progress(&ProgressEvent::AiReviewStageBackoff {
                patch_index: 0,
                stage: "resources".to_string(),
                retry_in_seconds: retry,
                turn: 9,
                max_turns: 100,
            })
            .expect("backoff events must be forwarded");
            let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
            match progress_update(1, 2, &parsed["payload"], 1, 4).expect("daemon must decode") {
                ProgressUpdate::Set(_, phase) => phase,
                ProgressUpdate::Finished(_) => panic!("backoff sets a phase"),
            }
        };

        assert_eq!(
            decode(Some(60)),
            Phase::Stage {
                stage: "resources".to_string(),
                turn: 9,
                max_turns: 100,
                waiting: StageWait::RateLimited {
                    retry_in_seconds: 60
                },
            }
        );
        assert_eq!(
            decode(None),
            Phase::Stage {
                stage: "resources".to_string(),
                turn: 9,
                max_turns: 100,
                waiting: StageWait::Model,
            }
        );
    }

    /// The worker streams the conversation and the daemon stores it, so the two
    /// ends must agree on the wire shape.
    #[test]
    fn log_entries_round_trip_and_stay_bounded() {
        use crate::local_review::{ProgressEvent, encode_progress};

        let line = encode_progress(&ProgressEvent::AiReviewLogEntry {
            patch_index: 0,
            stage: "locking".to_string(),
            message: crate::ai::AiMessage {
                role: crate::ai::AiRole::Tool,
                content: Some("git_grep output".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            },
        })
        .expect("log entries must be forwarded");

        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["type"], "progress");
        assert_eq!(parsed["payload"]["kind"], "log_entry");
        assert_eq!(parsed["payload"]["stage"], "locking");
        // The message crosses whole, in the shape the finished log stores.
        assert_eq!(parsed["payload"]["message"]["role"], "tool");
        assert_eq!(parsed["payload"]["message"]["content"], "git_grep output");

        // Log entries are not activity updates; decoding one as a phase must
        // yield nothing rather than corrupting the stage display.
        assert!(progress_update(1, 2, &parsed["payload"], 1, 4).is_none());
    }

    /// A single tool result can be a whole kernel file. Streaming that verbatim
    /// every turn would push megabytes through the IPC pipe.
    #[test]
    fn streamed_log_entries_carry_their_content_whole() {
        use crate::ai::{AiMessage, AiRole};
        use crate::worker::prompts::log_entry_event_for_test;

        let huge = "x".repeat(4_000_000);
        let msg = AiMessage {
            role: AiRole::Tool,
            content: Some(huge.clone()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        };

        let event = log_entry_event_for_test("security".to_string(), &msg);
        let crate::worker::WorkerProgressEvent::LogEntry { message, .. } = event else {
            panic!("expected a log entry");
        };
        assert_eq!(message.role, AiRole::Tool);
        // Clipping this bought little: the daemon proxies every request, so the
        // same tool result already crosses the pipe as part of the history.
        // What it cost was a live view that disagreed with the finished one.
        assert_eq!(
            message.content.unwrap_or_default().len(),
            huge.len(),
            "the streamed entry must hold what the finished log would hold"
        );
    }

    /// A tool call with no arguments says a search happened but not what was
    /// searched for, which is the only part worth watching live.
    #[test]
    fn streamed_tool_calls_stay_structured() {
        use crate::worker::prompts::log_entry_event_for_test;

        let msg = crate::ai::AiMessage {
            role: crate::ai::AiRole::Assistant,
            content: None,
            thought: None,
            thought_signature: None,
            tool_calls: Some(vec![
                crate::ai::ToolCall {
                    id: "c0".to_string(),
                    function_name: "git_grep".to_string(),
                    arguments: serde_json::json!({
                        "pattern": "ionic_hwstamp_replay",
                        "revision": "HEAD",
                        "unset": null,
                    }),
                    thought_signature: None,
                },
                crate::ai::ToolCall {
                    id: "c1".to_string(),
                    function_name: "git_show".to_string(),
                    arguments: serde_json::json!({ "path": "x".repeat(400) }),
                    thought_signature: None,
                },
            ]),
            tool_call_id: None,
        };

        let crate::worker::prompts::WorkerProgressEvent::LogEntry { message, .. } =
            log_entry_event_for_test("resources".to_string(), &msg)
        else {
            panic!("expected a log entry");
        };

        // Flattening these into the content string is what made the live view
        // a wall of prose where the finished view has expandable calls. The
        // renderer was never the difference; what it was handed was.
        let calls = message.tool_calls.expect("the calls must survive as calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function_name, "git_grep");
        assert_eq!(calls[0].arguments["pattern"], "ionic_hwstamp_replay");
        assert_eq!(calls[0].arguments["revision"], "HEAD");
        assert!(
            calls[0].arguments.get("unset").is_none(),
            "an absent argument is noise, not information"
        );

        // Bounded per argument, so one long value cannot crowd out the rest.
        let path = calls[1].arguments["path"].as_str().unwrap();
        assert!(
            path.len() < 200,
            "a long argument must be clipped: {}",
            path.len()
        );
        assert!(path.ends_with("..."), "and must say that it was: {path}");
    }

    /// Tool results reach the model as a JSON envelope, so a diff arrives with
    /// every quote backslash-escaped. Right for the model, unreadable for a
    /// person -- and this preview exists only for the person.
    #[test]
    fn streamed_tool_results_are_not_json_escaped() {
        use crate::worker::prompts::log_entry_event_for_test;

        let envelope = serde_json::json!({
            "content": "diff --git a/x.c b/x.c\n+\tstrcpy(buf, \"name\");\n",
            "truncated": true,
            "metadata": { "total_items": 400, "returned_items": 2 },
        });

        let msg = crate::ai::AiMessage {
            role: crate::ai::AiRole::Tool,
            content: Some(envelope.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: Some("c0".to_string()),
        };

        let crate::worker::prompts::WorkerProgressEvent::LogEntry { message, .. } =
            log_entry_event_for_test("resources".to_string(), &msg)
        else {
            panic!("expected a log entry");
        };
        let content = message.content.unwrap_or_default();

        assert!(
            content.contains("strcpy(buf, \"name\");"),
            "quotes must arrive unescaped: {content}"
        );
        assert!(
            !content.contains("\\\""),
            "no backslash-escaped quotes should survive: {content}"
        );
        assert!(
            content.contains("[truncated]"),
            "a clipped result must not read as a complete one: {content}"
        );

        // A plain-text tool result is passed through untouched.
        let plain = crate::ai::AiMessage {
            content: Some("not json at all".to_string()),
            ..msg
        };
        let crate::worker::prompts::WorkerProgressEvent::LogEntry { message, .. } =
            log_entry_event_for_test("resources".to_string(), &plain)
        else {
            panic!("expected a log entry");
        };
        let content = message.content.unwrap_or_default();
        assert_eq!(content, "not json at all");
    }

    /// git_read_files and git_ls do not use the `{"content": ...}` envelope the
    /// other tools do, so unwrapping only that shape left their results
    /// backslash-escaped -- reported as stray `\` at the start of every line.
    #[test]
    fn streamed_multi_file_and_listing_results_are_unwrapped() {
        use crate::worker::prompts::log_entry_event_for_test;

        fn preview(envelope: serde_json::Value) -> String {
            let msg = crate::ai::AiMessage {
                role: crate::ai::AiRole::Tool,
                content: Some(envelope.to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: Some("c0".to_string()),
            };
            let crate::worker::prompts::WorkerProgressEvent::LogEntry { message, .. } =
                log_entry_event_for_test("resources".to_string(), &msg)
            else {
                panic!("expected a log entry");
            };
            message.content.unwrap_or_default()
        }

        // git_read_files: one entry per file, and a per-file error is not the
        // same thing as an empty file.
        let content = preview(serde_json::json!({
            "results": [
                {
                    "path": "drivers/net/x.c",
                    "content": "int main(void)\n{\n\tchar *s = \"hi\";\n}\n",
                    "total_lines": 4,
                },
                { "path": "drivers/net/missing.c", "error": "does not exist at HEAD" },
            ]
        }));
        assert!(
            !content.contains(r#"\""#),
            "quotes must arrive unescaped: {content}"
        );
        assert!(!content.contains("\\n"), "newlines must be real: {content}");
        assert!(content.contains("--- drivers/net/x.c"), "{content}");
        assert!(content.contains("char *s = \"hi\";"), "{content}");
        assert!(
            content.contains("--- drivers/net/missing.c: does not exist at HEAD"),
            "a per-file failure must still be visible: {content}"
        );

        // git_ls: entries, with directories distinguishable from files.
        let content = preview(serde_json::json!({
            "entries": [
                { "name": "ionic", "type": "tree" },
                { "name": "Makefile", "type": "file" },
            ],
            "truncated": true,
        }));
        assert!(content.contains("ionic/"), "{content}");
        assert!(content.contains("Makefile"), "{content}");
        assert!(
            content.contains("[truncated]"),
            "a clipped listing must not read as complete: {content}"
        );

        // An envelope shape we do not know is passed through, not mangled.
        let content = preview(serde_json::json!({ "surprise": 1 }));
        assert!(content.contains("surprise"), "{content}");
    }

    /// The reported bug: a commit with a message but no file changes was
    /// reviewed, failed to apply, and retried three more times to fail
    /// identically. `extract_files_from_diff` returning nothing is the signal.
    #[test]
    fn empty_commits_are_recognised_as_having_no_files() {
        // A b4 prep tracker: real message, no diff.
        assert!(extract_files_from_diff("").is_empty());
        assert!(extract_files_from_diff("\n\n").is_empty());
        assert!(
            extract_files_from_diff("Signed-off-by: A <a@x>\n").is_empty(),
            "a message body alone is not a change"
        );

        // A real patch still reports its files, so the skip cannot swallow one.
        let real = "diff --git a/drivers/net/ionic/x.c b/drivers/net/ionic/x.c\n                    --- a/drivers/net/ionic/x.c\n                    +++ b/drivers/net/ionic/x.c\n                    @@ -1 +1 @@\n-old\n+new\n";
        assert!(
            !extract_files_from_diff(real).is_empty(),
            "a patch with changes must never be skipped as empty"
        );
    }

    /// The tag the stage layer writes is the tag the daemon reads.
    ///
    /// These two ends were written against each other and never met: the parser
    /// looked for `[s:...]`, nothing ever produced it, so every queue wait was
    /// silently attributed to no stage and dropped. Building the tag here the
    /// same way the stage layer does is what keeps that from recurring.
    #[test]
    fn test_the_stage_survives_the_round_trip_through_the_context_tag() {
        let tag = format!("{}[s:{}] ", "[ps:12 p:34] ", "locking");
        assert_eq!(stage_from_context_tag(&tag), Some("locking".to_string()));

        // Names carry hyphens, which a numeric parser would have rejected.
        let tag = format!("{}[s:{}] ", "[ps:1 p:2] ", "conflict-resolution");
        assert_eq!(
            stage_from_context_tag(&tag),
            Some("conflict-resolution".to_string())
        );

        // Anything else must not guess: attributing a queue wait to the wrong
        // stage is worse than not reporting it.
        assert_eq!(stage_from_context_tag("[ps:12 p:34] "), None);
        assert_eq!(stage_from_context_tag(""), None);
        assert_eq!(stage_from_context_tag("[s:]"), None);
        assert_eq!(stage_from_context_tag("[s:locking"), None);
    }

    #[test]
    fn test_stage_event_classification() {
        // Only stage_finished ends a stage; a turn must not close it early.
        assert_eq!(
            started_stage(&serde_json::json!({"kind": "stage_started", "stage": "implementation"})),
            Some("implementation".to_string())
        );
        assert_eq!(
            started_stage(
                &serde_json::json!({"kind": "stage_turn", "stage": "resources", "turn": 1})
            ),
            Some("resources".to_string())
        );
        assert_eq!(
            started_stage(&serde_json::json!({"kind": "stage_finished", "stage": "resources"})),
            None
        );
        assert_eq!(
            finished_stage(&serde_json::json!({"kind": "stage_finished", "stage": "resources"})),
            Some("resources".to_string())
        );
        assert_eq!(
            finished_stage(
                &serde_json::json!({"kind": "stage_turn", "stage": "resources", "turn": 1})
            ),
            None
        );
        assert_eq!(
            finished_stage(&serde_json::json!({"kind": "planning"})),
            None
        );
    }

    #[tokio::test]
    async fn test_run_review_tool_concurrency() -> Result<()> {
        let temp_dir = tempdir()?;
        let bin_path = temp_dir.path().join("mock_review");

        // Create a mock "review" binary that:
        // 1. Reads initial JSON from stdin.
        // 2. Spams 1000 lines of logs to STDOUT.
        // 3. Sends an 'ai_request' JSON to STDOUT.
        // 4. Reads 'ai_response' from stdin.
        // 5. Prints final result JSON to STDOUT.
        let mock_script = r#"#!/bin/bash
# 1. Read input
read -r input

# 2. Spam logs
for i in {1..1000}; do
    echo "LOG LINE $i - This is a long log line to fill up buffers and test if the parent drains stdout correctly while waiting for AI response."
done

# 3. Send AI request
echo '{"type": "ai_request", "payload": {"messages": [{"role": "user", "content": "hello"}], "temperature": 0.5}}'

# 4. Wait for AI response
read -r ai_response

# 5. Send final result
echo '{"patchset_id": 1, "patches": [{"index": 1, "status": "applied"}]}'
"#;

        std::fs::write(&bin_path, mock_script)?;
        std::fs::set_permissions(&bin_path, Permissions::from_mode(0o755))?;

        // Setup Sashiko dependencies
        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();

        let _db = Arc::new(Database::new(&settings.database).await?);
        let _quota_manager = Arc::new(QuotaManager::new());
        let provider = Arc::new(MockProvider);
        // We manually spawn the mock and run the same loop as run_review_tool
        let mut cmd = Command::new(&bin_path);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let interaction = async {
            // 1. Send input
            stdin.write_all(b"{}\n").await?;
            stdin.flush().await?;

            let mut reader = BufReader::new(stdout).lines();
            let mut final_result = None;

            while let Ok(Some(line)) = reader.next_line().await {
                if let Ok(json_msg) = serde_json::from_str::<Value>(&line) {
                    if json_msg["type"] == "ai_request" {
                        // Concurrency check: We are here, the child already sent 1000 log lines.
                        // If the parent didn't drain them, the child would be blocked on write
                        // and we would never receive this ai_request.

                        let resp = provider
                            .generate_content(AiRequest {
                                system: None,
                                messages: vec![],
                                tools: None,
                                temperature: None,
                                response_format: None,
                                context_tag: None,
                            })
                            .await?;

                        let reply = json!({ "type": "ai_response", "payload": resp });
                        let mut reply_str = serde_json::to_string(&reply)?;
                        reply_str.push('\n');
                        stdin.write_all(reply_str.as_bytes()).await?;
                        stdin.flush().await?;
                    } else if json_msg.get("patchset_id").is_some() {
                        final_result = Some(json_msg);
                        break;
                    }
                } else {
                    // This is where logs are drained
                    // println!("Log: {}", line);
                }
            }
            Ok::<Option<Value>, anyhow::Error>(final_result)
        };

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), interaction).await??;

        assert!(result.is_some());
        assert_eq!(result.unwrap()["patchset_id"], 1);

        child.wait().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_run_review_tool_sends_typed_fatal_error_payload() -> Result<()> {
        let mock_script = r#"#!/bin/bash
read -r input
echo '{"type":"ai_request","payload":{"messages":[{"role":"user","content":"hello"}]}}'
read -r ai_response
if [[ "$ai_response" == *'"type":"error"'* && "$ai_response" == *'"message":"fatal provider failure"'* && "$ai_response" == *'"class":"fatal"'* ]]; then
    echo '{"patchset_id":1,"patches":[{"index":1,"status":"typed_fatal"}]}'
else
    echo '{"patchset_id":1,"patches":[{"index":1,"status":"unexpected"}]}'
fi
"#;

        let result = run_single_ai_request_mock(mock_script, Arc::new(FailingProvider)).await?;

        assert_eq!(result["patches"][0]["status"], "typed_fatal");
        Ok(())
    }

    /// A worker that ignores the wind-down request is still killed and reaped.
    ///
    /// The salvage window bounds how long that takes: this mock never reads its
    /// stdin, so it can neither answer the request nor exit on its own.
    #[tokio::test]
    async fn test_run_review_tool_timeout_reaps_stuck_child() -> Result<()> {
        let temp_dir = tempdir()?;
        let bin_path = temp_dir.path().join("mock_review");
        let mock_script = r#"#!/bin/bash
read -r input
sleep 30
"#;

        std::fs::write(&bin_path, mock_script)?;
        std::fs::set_permissions(&bin_path, Permissions::from_mode(0o755))?;

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.review.timeout_seconds = 1;
        settings.review.salvage_seconds = 1;

        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;
        let quota_manager = Arc::new(QuotaManager::new());
        let provider = Arc::new(MockProvider);

        let thread_id = db.create_thread("msg_id", "Subject", 1000).await?;
        db.create_message(
            "msg_id_p1",
            thread_id,
            None,
            "Author",
            "Subject",
            1000,
            "Body",
            "",
            "",
            None,
            None,
        )
        .await?;
        let ps_id = db
            .create_patchset(
                thread_id, None, "msg_id", "Subject", "Author", 1000, 1, 1, "", "", None, 1, None,
                false, None, None,
            )
            .await?
            .expect("Failed to create patchset");
        let p_id = db
            .create_patch(ps_id, "msg_id_p1", 1, "diff --git a/a b/a\n")
            .await?;
        let review_id = db
            .create_review(ps_id, Some(p_id), "mock", "mock", None, None)
            .await?;

        let completed = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            run_review_tool_with_cmd(
                Command::new(&bin_path),
                ps_id,
                p_id,
                &json!({}),
                &settings,
                db,
                "HEAD",
                Some(1),
                None,
                quota_manager,
                review_id,
                None,
                provider,
                Arc::new(Semaphore::new(56)),
                crate::activity::ActivityRegistry::new(),
                None,
                1,
                1,
            ),
        )
        .await;

        assert!(
            completed.is_ok(),
            "run_review_tool should kill/reap a timed-out child promptly"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_run_review_tool_retries_rate_limits_without_child_error() -> Result<()> {
        let mock_script = r#"#!/bin/bash
read -r input
echo '{"type":"ai_request","payload":{"messages":[{"role":"user","content":"hello"}]}}'
read -r ai_response
if [[ "$ai_response" == *'"type":"error"'* ]]; then
    echo '{"patchset_id":1,"patches":[{"index":1,"status":"error_written"}]}'
else
    echo '{"patchset_id":1,"patches":[{"index":1,"status":"applied"}]}'
fi
"#;
        let provider = Arc::new(RateLimitThenSuccessProvider {
            calls: AtomicUsize::new(0),
        });
        let provider_for_tool: Arc<dyn AiProvider> = provider.clone();

        let result = run_single_ai_request_mock(mock_script, provider_for_tool).await?;

        assert_eq!(result["patches"][0]["status"], "applied");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_skip_ignored_files() -> Result<()> {
        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.review.ignore_files = vec!["ignored.txt".to_string(), "ignore_dir/".to_string()];

        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;
        let quota_manager = Arc::new(QuotaManager::new());
        let provider = Arc::new(MockProvider);

        let ctx = ReviewContext {
            semaphore: Arc::new(Semaphore::new(1)),
            llm_semaphore: Arc::new(Semaphore::new(56)),
            db: db.clone(),
            settings: settings.clone(),
            baseline_registry: Arc::new(BaselineRegistry::new(Path::new("."), None).unwrap()),
            quota_manager,
            target_review_count: 1,
            provider,
            activity: crate::activity::ActivityRegistry::new(),
            cancels: crate::cancel::CancelRegistry::new(),
        };

        // Create dummy patchset and patch in DB
        let thread_id = db.create_thread("msg_id_1", "Subject", 1000).await?;

        // Create messages for patches
        db.create_message(
            "msg_id_p1",
            thread_id,
            None,
            "Author",
            "Subject",
            1000,
            "Body",
            "",
            "",
            None,
            None,
        )
        .await?;
        db.create_message(
            "msg_id_p2",
            thread_id,
            None,
            "Author",
            "Subject",
            1000,
            "Body",
            "",
            "",
            None,
            None,
        )
        .await?;
        db.create_message(
            "msg_id_p3",
            thread_id,
            None,
            "Author",
            "Subject",
            1000,
            "Body",
            "",
            "",
            None,
            None,
        )
        .await?;

        let ps_id = db
            .create_patchset(
                thread_id, None, "msg_id_1", "Subject", "Author", 1000, 1, 1, "", "", None, 1,
                None, false, None, None,
            )
            .await?
            .expect("Failed to create patchset");

        // Case 1: Ignored file
        let diff_ignored = "diff --git a/ignored.txt b/ignored.txt\nindex ...";
        let p_id = db.create_patch(ps_id, "msg_id_p1", 1, diff_ignored).await?;

        let result = Reviewer::process_patch_review(
            &ctx,
            ps_id,
            p_id,
            1,
            "HEAD",
            None,
            &json!({}),
            None,
            None,
            None,
            diff_ignored,
            None,
        )
        .await?;

        // Should return Success (because it's skipped gracefully)
        match result {
            PatchResult::Success => {}
            _ => panic!("Expected Success for skipped review"),
        }

        // Verify DB status
        let mut rows = db
            .conn
            .query(
                "SELECT status, result_description FROM reviews WHERE patch_id = ?",
                libsql::params![p_id],
            )
            .await?;
        let row = rows.next().await?.expect("No review found");
        let status: String = row.get(0)?;
        let description: String = row.get(1).unwrap_or_default();

        assert_eq!(status, "Skipped");
        assert!(description.contains("touches only ignored files"));

        // Case 2: Ignored directory prefix
        let diff_dir = "diff --git a/ignore_dir/subfile.c b/ignore_dir/subfile.c\n...";
        let p_id_2 = db.create_patch(ps_id, "msg_id_p2", 2, diff_dir).await?;

        let result = Reviewer::process_patch_review(
            &ctx,
            ps_id,
            p_id_2,
            2,
            "HEAD",
            None,
            &json!({}),
            None,
            None,
            None,
            diff_dir,
            None,
        )
        .await?;

        match result {
            PatchResult::Success => {}
            _ => panic!("Expected Success for skipped review"),
        }

        let mut rows = db
            .conn
            .query(
                "SELECT status FROM reviews WHERE patch_id = ?",
                libsql::params![p_id_2],
            )
            .await?;
        let row = rows.next().await?.expect("No review found");
        let status: String = row.get(0)?;
        assert_eq!(status, "Skipped");

        // Case 3: Mixed (Ignored + Not Ignored) -> Should NOT skip
        let diff_mixed = "diff --git a/ignored.txt b/ignored.txt\n...\ndiff --git a/src/main.rs b/src/main.rs\n...";
        let p_id_3 = db.create_patch(ps_id, "msg_id_p3", 3, diff_mixed).await?;

        let _result = Reviewer::process_patch_review(
            &ctx,
            ps_id,
            p_id_3,
            3,
            "HEAD",
            None,
            &json!({}),
            None,
            None,
            None,
            diff_mixed,
            None,
        )
        .await;

        // Even if it fails to run tool, it shouldn't be "Skipped".
        let mut rows = db
            .conn
            .query(
                "SELECT status FROM reviews WHERE patch_id = ?",
                libsql::params![p_id_3],
            )
            .await?;
        // Review might be created (Pending/InReview) or not if process failed early (but create_review is called early in loop)
        // Wait, loop calls create_review at start of loop.
        // If run_review_tool fails (which it will), we get ReviewFailed.

        if let Ok(Some(row)) = rows.next().await {
            let status: String = row.get(0)?;
            assert_ne!(status, "Skipped");
        } else {
            // It might fail before creating review?
            // create_review is inside the loop.
            // If run_review_tool fails to spawn (binary not found), it returns Err.
            // process_patch_review handles Err by logging and retrying.
            // If retries exhausted, it returns ReviewFailed.
            // But it DOES create a review entry in each iteration.
            // So we should find at least one review.
        }

        Ok(())
    }

    struct MockProviderWithUsage {
        prompt_tokens: usize,
        completion_tokens: usize,
        cached_tokens: usize,
    }

    #[async_trait]
    impl AiProvider for MockProviderWithUsage {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            Ok(AiResponse {
                content: Some("Mocked AI response".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: Some(crate::ai::AiUsage {
                    prompt_tokens: self.prompt_tokens,
                    completion_tokens: self.completion_tokens,
                    total_tokens: self.prompt_tokens + self.completion_tokens,
                    cached_tokens: Some(self.cached_tokens),
                }),
                truncated: false,
            })
        }
        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    async fn run_two_request_mock(settings: Settings, provider: Arc<dyn AiProvider>) -> Result<()> {
        let temp_dir = tempdir()?;
        let bin_path = temp_dir.path().join("mock_review");

        // Mock binary: sends two consecutive AI requests then a final result.
        let mock_script = r#"#!/bin/bash
read -r input
echo '{"type": "ai_request", "payload": {"messages": [{"role": "user", "content": "first"}], "temperature": 0.5}}'
read -r ai_response
echo '{"type": "ai_request", "payload": {"messages": [{"role": "user", "content": "second"}], "temperature": 0.5}}'
read -r ai_response
sleep 1  # make less sensitive to race condition
echo '{"patchset_id": 1, "patches": [{"index": 1, "status": "applied"}]}'
"#;
        std::fs::write(&bin_path, mock_script)?;
        std::fs::set_permissions(&bin_path, Permissions::from_mode(0o755))?;

        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;
        let quota_manager = Arc::new(QuotaManager::new());

        let thread_id = db.create_thread("msg_id_1", "Subject", 1000).await?;
        db.create_message(
            "msg_id_p1",
            thread_id,
            None,
            "Author",
            "Subject",
            1000,
            "Body",
            "",
            "",
            None,
            None,
        )
        .await?;
        let ps_id = db
            .create_patchset(
                thread_id, None, "msg_id_1", "Subject", "Author", 1000, 1, 1, "", "", None, 1,
                None, false, None, None,
            )
            .await?
            .unwrap();
        let p_id = db
            .create_patch(ps_id, "msg_id_p1", 1, "diff --git a/foo.c b/foo.c\n+int x;")
            .await?;
        let review_id = db
            .create_review(ps_id, Some(p_id), "mock", "mock", None, None)
            .await?;

        run_review_tool_with_cmd(
            Command::new(&bin_path),
            ps_id,
            p_id,
            &json!({}),
            &settings,
            db,
            "HEAD",
            Some(1),
            None,
            quota_manager,
            review_id,
            None,
            provider,
            Arc::new(Semaphore::new(56)),
            crate::activity::ActivityRegistry::new(),
            None,
            1,
            1,
        )
        .await
        .map(|_| ())
    }

    #[tokio::test]
    async fn test_token_budget_aborts_review() -> Result<()> {
        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        // Each turn: 800 uncached input + 100 output = 900 uncached total.
        // Budget of 1000 allows turn 1 (cumulative 900) but aborts on turn 2 (cumulative 1800).
        settings.review.max_total_tokens = 1000;
        settings.review.max_total_output_tokens = 0; // disabled

        let provider = Arc::new(MockProviderWithUsage {
            prompt_tokens: 1000,
            completion_tokens: 100,
            cached_tokens: 200, // uncached input = 800
        });

        let err = run_two_request_mock(settings, provider)
            .await
            .expect_err("Expected token budget error");
        assert!(
            err.to_string().contains("Token budget exceeded"),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_output_token_budget_aborts_review() -> Result<()> {
        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.review.max_total_tokens = 0; // disabled
        // Each turn produces 300 output tokens. Budget of 500 allows turn 1 but aborts on turn 2.
        settings.review.max_total_output_tokens = 500;

        let provider = Arc::new(MockProviderWithUsage {
            prompt_tokens: 100,
            completion_tokens: 300,
            cached_tokens: 0,
        });

        let err = run_two_request_mock(settings, provider)
            .await
            .expect_err("Expected output token budget error");
        assert!(
            err.to_string().contains("Output token budget exceeded"),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_queue_notifications_split_summary() -> Result<()> {
        let temp_dir = tempdir()?;
        let policy_path = temp_dir.path().join("email_policy.toml");
        std::fs::write(
            &policy_path,
            r#"
            [defaults]
            mute_all = false
            reply_all = true
            "#,
        )?;

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.review.email_policy_path = policy_path.to_str().unwrap().to_string();
        settings.smtp = Some(crate::settings::SmtpSettings {
            server: "localhost".to_string(),
            port: 25,
            username: None,
            password: None,
            sender_address: "bot@sashiko.dev".to_string(),
            reply_to: None,
            dry_run: false, // We want 'Pending' status to be able to query it easily if needed, or just query it anyway.
        });

        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;

        let thread_id = db.create_thread("msg_id_1", "Subject", 1000).await?;
        db.create_message(
            "msg_id_p1",
            thread_id,
            None,
            "Author <author@example.com>",
            "Subject",
            1000,
            "Body with ---\nSigned-off-by: Author <author@example.com>",
            "to@example.com",
            "cc@example.com",
            None,
            None,
        )
        .await?;

        let ps_id = db
            .create_patchset(
                thread_id, None, "msg_id_1", "Subject", "Author", 1000, 1, 1, "", "", None, 1,
                None, false, None, None,
            )
            .await?
            .unwrap();

        let p_id_1 = db.create_patch(ps_id, "msg_id_p1", 1, "diff").await?;

        let ctx = ReviewContext {
            semaphore: Arc::new(Semaphore::new(1)),
            llm_semaphore: Arc::new(Semaphore::new(56)),
            db: db.clone(),
            settings,
            baseline_registry: Arc::new(
                crate::baseline::BaselineRegistry::new(Path::new("."), None).unwrap(),
            ),
            quota_manager: Arc::new(QuotaManager::new()),
            target_review_count: 1,
            provider: Arc::new(MockProvider),
            activity: crate::activity::ActivityRegistry::new(),
            cancels: crate::cancel::CancelRegistry::new(),
        };

        // Scenario 1: Mixed findings
        let findings_mixed = vec![
            json!({
                "problem": "New High issue",
                "severity": "High",
                "preexisting": false
            }),
            json!({
                "problem": "Preexisting Medium issue",
                "severity": "Medium",
                "preexisting": true
            }),
            json!({
                "problem": "New Low issue",
                "severity": "Low",
                "preexisting": false
            }),
        ];

        Reviewer::queue_notifications(
            &ctx,
            ps_id,
            p_id_1,
            "msg_id_p1",
            "msg_id_1",
            1, // index
            "inline review content",
            Some(&findings_mixed),
            "summary",
        )
        .await?;

        // Verify Scenario 1
        let mut rows = db
            .conn
            .query(
                "SELECT body FROM email_outbox WHERE patch_id = ?",
                libsql::params![p_id_1],
            )
            .await?;
        let row = rows.next().await?.expect("Expected email in outbox");
        let body: String = row.get(0)?;
        let expected_mixed_body = "\
Thank you for your contribution! Sashiko AI review found 3 potential issue(s) to consider:

New issues:
- [High] New High issue
- [Low] New Low issue

Pre-existing issues:
- [Medium] Preexisting Medium issue
--

inline review content\n\n-- \nSashiko AI review · https://sashiko.dev/#/patchset/msg_id_1?part=1";
        assert_eq!(body, expected_mixed_body);

        // Setup for Scenario 2: Only New
        db.create_message(
            "msg_id_p2",
            thread_id,
            None,
            "Author <author@example.com>",
            "Subject 2",
            1000,
            "Body 2",
            "to@example.com",
            "cc@example.com",
            None,
            None,
        )
        .await?;
        let p_id_2 = db.create_patch(ps_id, "msg_id_p2", 2, "diff").await?;

        let findings_new_only = vec![
            json!({
                "problem": "New High issue",
                "severity": "High",
                "preexisting": false
            }),
            json!({
                "problem": "New Low issue",
                "severity": "Low",
                "preexisting": false
            }),
        ];

        Reviewer::queue_notifications(
            &ctx,
            ps_id,
            p_id_2,
            "msg_id_p2",
            "msg_id_1",
            2, // index
            "inline review content 2",
            Some(&findings_new_only),
            "summary",
        )
        .await?;

        let mut rows = db
            .conn
            .query(
                "SELECT body FROM email_outbox WHERE patch_id = ?",
                libsql::params![p_id_2],
            )
            .await?;
        let row = rows.next().await?.expect("Expected email in outbox");
        let body: String = row.get(0)?;
        let expected_new_only_body = "\
Thank you for your contribution! Sashiko AI review found 2 potential issue(s) to consider:
- [High] New High issue
- [Low] New Low issue
--

inline review content 2\n\n-- \nSashiko AI review · https://sashiko.dev/#/patchset/msg_id_1?part=2";
        assert_eq!(body, expected_new_only_body);

        // Setup for Scenario 3: Only Pre-existing
        db.create_message(
            "msg_id_p3",
            thread_id,
            None,
            "Author <author@example.com>",
            "Subject 3",
            1000,
            "Body 3",
            "to@example.com",
            "cc@example.com",
            None,
            None,
        )
        .await?;
        let p_id_3 = db.create_patch(ps_id, "msg_id_p3", 3, "diff").await?;

        let findings_preexisting_only = vec![json!({
            "problem": "Preexisting Medium issue",
            "severity": "Medium",
            "preexisting": true
        })];

        Reviewer::queue_notifications(
            &ctx,
            ps_id,
            p_id_3,
            "msg_id_p3",
            "msg_id_1",
            3, // index
            "inline review content 3",
            Some(&findings_preexisting_only),
            "summary",
        )
        .await?;

        let mut rows = db
            .conn
            .query(
                "SELECT body FROM email_outbox WHERE patch_id = ?",
                libsql::params![p_id_3],
            )
            .await?;
        let row = rows.next().await?.expect("Expected email in outbox");
        let body: String = row.get(0)?;
        let expected_preexisting_only_body = "\
Thank you for your contribution! Sashiko AI review found 1 potential issue(s) to consider:

Pre-existing issues:
- [Medium] Preexisting Medium issue
--

inline review content 3\n\n-- \nSashiko AI review · https://sashiko.dev/#/patchset/msg_id_1?part=3";
        assert_eq!(body, expected_preexisting_only_body);

        Ok(())
    }

    #[tokio::test]
    async fn test_queue_notifications_email_references() -> Result<()> {
        let temp_dir = tempdir()?;
        let policy_path = temp_dir.path().join("email_policy.toml");
        std::fs::write(
            &policy_path,
            r#"
            [defaults]
            mute_all = false
            reply_all = true
            "#,
        )?;

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.review.email_policy_path = policy_path.to_str().unwrap().to_string();
        settings.smtp = Some(crate::settings::SmtpSettings {
            server: "localhost".to_string(),
            port: 25,
            username: None,
            password: None,
            sender_address: "bot@sashiko.dev".to_string(),
            reply_to: None,
            dry_run: false,
        });

        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;

        let thread_id = db.create_thread("msg_id_1", "Subject", 1000).await?;

        // 1. Scenario A: Parent has no references (NULL in DB)
        db.create_message(
            "msg_id_p1",
            thread_id,
            None,
            "Author <author@example.com>",
            "Subject 1",
            1000,
            "Body 1",
            "to@example.com",
            "cc@example.com",
            None,
            None,
        )
        .await?;

        let ps_id = db
            .create_patchset(
                thread_id, None, "msg_id_1", "Subject", "Author", 1000, 1, 1, "", "", None, 1,
                None, false, None, None,
            )
            .await?
            .unwrap();

        let p_id_1 = db.create_patch(ps_id, "msg_id_p1", 1, "diff").await?;

        let ctx = ReviewContext {
            semaphore: Arc::new(Semaphore::new(1)),
            llm_semaphore: Arc::new(Semaphore::new(56)),
            db: db.clone(),
            settings,
            baseline_registry: Arc::new(
                crate::baseline::BaselineRegistry::new(Path::new("."), None).unwrap(),
            ),
            quota_manager: Arc::new(QuotaManager::new()),
            target_review_count: 1,
            provider: Arc::new(MockProvider),
            activity: crate::activity::ActivityRegistry::new(),
            cancels: crate::cancel::CancelRegistry::new(),
        };

        Reviewer::queue_notifications(
            &ctx,
            ps_id,
            p_id_1,
            "msg_id_p1",
            "msg_id_1",
            1,
            "inline review content 1",
            None,
            "summary 1",
        )
        .await?;

        let mut rows = db
            .conn
            .query(
                "SELECT in_reply_to, references_hdr FROM email_outbox WHERE patch_id = ?",
                libsql::params![p_id_1],
            )
            .await?;
        let row = rows.next().await?.expect("Expected email in outbox");
        let in_reply_to: String = row.get(0)?;
        let references_hdr: String = row.get(1)?;
        assert_eq!(in_reply_to, "msg_id_p1");
        assert_eq!(references_hdr, "msg_id_p1");

        // 2. Scenario B: Parent has existing references (standard references chain)
        db.create_message_with_references(
            "msg_id_p2",
            thread_id,
            Some("msg_id_1"),
            "Author <author@example.com>",
            "Subject 2",
            1001,
            "Body 2",
            "to@example.com",
            "cc@example.com",
            None,
            None,
            Some("msg_id_1"),
        )
        .await?;

        let p_id_2 = db.create_patch(ps_id, "msg_id_p2", 2, "diff").await?;

        Reviewer::queue_notifications(
            &ctx,
            ps_id,
            p_id_2,
            "msg_id_p2",
            "msg_id_1",
            2,
            "inline review content 2",
            None,
            "summary 2",
        )
        .await?;

        let mut rows = db
            .conn
            .query(
                "SELECT in_reply_to, references_hdr FROM email_outbox WHERE patch_id = ?",
                libsql::params![p_id_2],
            )
            .await?;
        let row = rows.next().await?.expect("Expected email in outbox");
        let in_reply_to: String = row.get(0)?;
        let references_hdr: String = row.get(1)?;
        assert_eq!(in_reply_to, "msg_id_p2");
        assert_eq!(references_hdr, "msg_id_1 msg_id_p2");

        Ok(())
    }

    #[test]
    fn test_default_worker_command() -> Result<()> {
        let cmd = default_worker_command()?;
        let program = cmd.as_std().get_program().to_string_lossy().to_string();
        assert!(program.contains("sashiko") || program == "cargo");
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"worker".to_string()));
        Ok(())
    }
}
