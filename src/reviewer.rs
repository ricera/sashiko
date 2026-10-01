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
use crate::ai::backoff_provider::{ReviewDeadline, ReviewTimedOut};
use crate::ai::quota::QuotaManager;
use crate::ai::{
    AiErrorClass, AiProvider, AiRequest, AiResponse, RemoteAiErrorPayload, classify_ai_error,
    create_provider_cached,
};
use crate::baseline::{BaselineRegistry, BaselineResolution, CommitId, extract_files_from_diff};
use crate::db::{AiInteractionParams, Database, Finding, PatchsetRow, Severity};
use crate::email_policy::EmailPolicyConfig;
use crate::email_router::{Action as EmailAction, EmailRouter};
use crate::git_ops::{GitWorktree, PatchApplyFailure, ensure_remote, get_commit_hash};
use crate::prerequisites::{PrerequisitePatch, resolve_prerequisites_from_lore};
use crate::settings::Settings;
use crate::utils::redact_secret;
use crate::worker::prompts::ReviewError;
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
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
    /// The program run as the review worker. `None` runs the daemon's own
    /// binary; tests set a script that plays the worker's side of the
    /// protocol.
    worker_program: Option<PathBuf>,
}

enum PatchResult {
    Success,
    ReviewFailed,
    Cancelled,
}

#[derive(Serialize)]
struct BaselineAttempt {
    baseline: String,
    status: String,
    log: String,
}

static INTERACTION_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Budget for one fetch of a missing base-commit, and for the search of
/// the trees that may hold it.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

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

fn git_am_message(author: &str, date: i64, subject: &str, diff: &str) -> Result<String> {
    let date = chrono::DateTime::from_timestamp(date, 0)
        .ok_or_else(|| anyhow!("timestamp {date} is outside chrono's supported range"))?
        .with_timezone(&chrono::Local)
        .to_rfc2822();
    Ok(format!(
        "From: {author}\nDate: {date}\nSubject: {subject}\n\n{diff}\n"
    ))
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
    initialized: std::sync::atomic::AtomicBool,
    activity: Arc<crate::activity::ActivityRegistry>,
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
    /// Checkpoints only while `run` is the patchset's newest review. A rerun
    /// requested mid-review resets the clock and starts a second task; the
    /// superseded one would otherwise keep overwriting the reset with its own
    /// total.
    fn new(
        db: Arc<Database>,
        run: crate::activity::PatchsetRun,
        carried_seconds: u64,
        started: Instant,
    ) -> Self {
        const CHECKPOINT: Duration = Duration::from_secs(30);

        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(CHECKPOINT);
            // The first tick fires immediately; skip it, since zero elapsed
            // would overwrite the carried total with itself for no reason.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if !run.is_current() {
                    return;
                }
                let total = carried_seconds + started.elapsed().as_secs();
                // Best effort: timing must never be able to fail a review.
                let patchset_id = run.patchset_id();
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
    /// * `activity` - Where running reviews report what they are doing; shared
    ///   with the API that serves it.
    pub async fn new(
        db: Arc<Database>,
        settings: Settings,
        activity: Arc<crate::activity::ActivityRegistry>,
    ) -> Self {
        let concurrency = settings.review.concurrency;
        let repo_path = PathBuf::from(&settings.git.repository_path);

        let baseline_registry =
            match BaselineRegistry::new(&repo_path, settings.git.custom_remotes.clone()) {
                Ok(r) => Arc::new(r),
                Err(e) => {
                    error!(
                        "Failed to initialize BaselineRegistry: {}. Using empty registry.",
                        e
                    );
                    Arc::new(
                        BaselineRegistry::new(&repo_path, settings.git.custom_remotes.clone())
                            .unwrap_or_else(|_| {
                                panic!("Critical error initializing BaselineRegistry: {}", e)
                            }),
                    )
                }
            };

        let provider = create_provider_cached(&settings.ai, Some(settings.database.url.as_str()))
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
            initialized: std::sync::atomic::AtomicBool::new(false),
            activity,
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

        // Only wipe previous worktrees and reset in-flight review statuses on
        // the initial daemon startup. If `Reviewer::start` is restarted by the
        // worker supervisor after a panic, detached review tasks spawned before
        // the panic may still be using their worktrees under `worktree_dir`.
        if !self
            .initialized
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            // The daemon is required to name a worktree directory, so this
            // only unwraps what validate_for_daemon checked.
            if let Some(worktree_dir) = self.settings.review.worktree_dir.clone().map(PathBuf::from)
                && let Err(e) = tokio::task::spawn_blocking(move || {
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
                })
                .await
            {
                error!("Worktree directory initialization task failed: {}", e);
            }

            match self.db.reset_reviewing_status().await {
                Ok(count) => {
                    if count > 0 {
                        info!("Recovered {} interrupted reviews (reset to Pending)", count);
                    }
                }
                Err(e) => error!("Failed to reset reviewing status: {}", e),
            }
        }

        // After the reset, so the streamed conversation of a review that was
        // interrupted is judged by the status it now has. Those reviews are
        // Pending again and keep their rows until they restart, which clears
        // them; everything else is left over from a review that ended without
        // reaching complete_review.
        match self.db.sweep_orphan_review_log_entries().await {
            Ok(count) => {
                if count > 0 {
                    info!("Swept {} streamed log entries of finished reviews", count);
                }
            }
            Err(e) => error!("Failed to sweep streamed review log entries: {}", e),
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

            // Atomically claim only if still Pending (avoids reopening a patchset
            // that was cancelled while waiting for a semaphore permit)
            match self.db.claim_patchset_for_review(patchset.id).await {
                Ok(true) => {}
                Ok(false) => {
                    info!(
                        "Skipping patchset {} as it is no longer Pending (likely cancelled)",
                        patchset.id
                    );
                    continue;
                }
                Err(e) => {
                    error!(
                        "Failed to update status to In Review for {}: {}",
                        patchset.id, e
                    );
                    continue;
                }
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
                worker_program: None,
            };

            let patchset_id = patchset.id;
            Self::spawn_supervised_patchset_review(
                self.db.clone(),
                permit,
                patchset_id,
                Self::review_patchset_task(context, patchset),
            );
        }

        Ok(())
    }

    async fn mark_patchset_failed_unless_cancelled(db: &Database, patchset_id: i64) {
        let current_status = db.get_patchset_status(patchset_id).await.ok().flatten();
        if current_status.as_deref() != Some(ReviewStatus::Cancelled.as_str())
            && let Err(db_err) = db
                .update_patchset_status(patchset_id, ReviewStatus::Failed.as_str())
                .await
        {
            error!(
                "Failed to mark patchset {} as Failed: {}",
                patchset_id, db_err
            );
        }
    }

    fn spawn_supervised_patchset_review<F>(
        db: Arc<Database>,
        permit: tokio::sync::OwnedSemaphorePermit,
        patchset_id: i64,
        review_fut: F,
    ) -> tokio::task::JoinHandle<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(async move {
            let _permit = permit;
            let inner = tokio::spawn(review_fut);
            if let Err(join_err) = inner.await {
                error!(
                    "Review task for patchset {} terminated abnormally ({}); marking as Failed",
                    patchset_id, join_err
                );
                Self::mark_patchset_failed_unless_cancelled(&db, patchset_id).await;
            }
        })
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
                worker_program: None,
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
                Some(review.id),
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

        ctx.db.release_embargoed_forge_outbox(patchset.id).await?;
        ctx.db.clear_patchset_embargo(patchset.id).await?;
        info!("Embargo released successfully for patchset {}", patchset.id);
        Ok(())
    }

    async fn queue_forge_pr_comment(
        ctx: &ReviewContext,
        patchset: &PatchsetRow,
        diffs: &[(i64, i64, String, String, String, i64, String)],
    ) -> Result<()> {
        let Some(mr_url) = patchset.mr_url.as_deref() else {
            return Ok(());
        };
        let Some(repo) = crate::forge::extract_owner_repo_from_mr_url(mr_url) else {
            return Ok(());
        };
        let Some(pr_number) = patchset.mr_number else {
            return Ok(());
        };

        let provider = if mr_url.contains("gitlab") {
            "gitlab"
        } else {
            "github"
        };

        let reviews = ctx
            .db
            .get_completed_reviews_for_release(patchset.id)
            .await?;

        let total_parts = patchset.total_parts.unwrap_or(diffs.len() as u32) as usize;
        let mut series_summary: Option<String> = None;
        let mut items = Vec::new();

        for (patch_id, idx, _diff, subj, _auth, _date, msg_id) in diffs {
            let matching_review = reviews
                .iter()
                .find(|r| r.patch_id == *patch_id || r.index == *idx);
            if *idx == 0 {
                if let Some(r) = matching_review
                    && !r.summary.trim().is_empty()
                {
                    series_summary = Some(r.summary.clone());
                }
                continue;
            }
            let is_sha = msg_id.len() == 40 && msg_id.chars().all(|c| c.is_ascii_hexdigit());
            let inline_review = matching_review
                .map(|r| r.inline_review.clone())
                .filter(|s| {
                    let t = s.trim();
                    !t.is_empty() && t != "No issues found."
                });
            items.push(crate::forge::PatchReviewSummaryItem {
                part_index: *idx as usize,
                total_parts,
                commit_id: if is_sha { Some(msg_id.clone()) } else { None },
                subject: subj.clone(),
                inline_review,
            });
        }

        let domain = if ctx.settings.project.domain.is_empty() {
            "sashiko.sashiko.dev"
        } else {
            ctx.settings.project.domain.as_str()
        };
        let slug = patchset
            .slug
            .as_deref()
            .or(patchset.message_id.as_deref())
            .unwrap_or("");
        let target_url = format!("https://{}/#/patchset/{}", domain, slug);

        let version =
            crate::forge::extract_mr_version_from_subject(patchset.subject.as_deref(), pr_number);
        let body = crate::forge::compose_pr_review_comment(
            version,
            items.len(),
            series_summary.as_deref(),
            &items,
            &target_url,
        );

        let head_sha = items.last().and_then(|p| p.commit_id.as_deref());
        let now = chrono::Utc::now().timestamp();
        let is_embargoed = patchset.embargo_until.is_some_and(|t| t > now);
        let status = ctx.settings.forge.post_mode.outbox_status(is_embargoed);

        ctx.db
            .insert_forge_outbox(
                patchset.id,
                provider,
                &repo,
                pr_number,
                head_sha,
                &body,
                &target_url,
                status,
            )
            .await
    }

    async fn review_patchset_task(ctx: ReviewContext, patchset: PatchsetRow) {
        let patchset_id = patchset.id;
        if ctx
            .db
            .get_patchset_status(patchset_id)
            .await
            .ok()
            .flatten()
            .as_deref()
            == Some(ReviewStatus::Cancelled.as_str())
        {
            info!(
                "Patchset {} was cancelled before review started, skipping",
                patchset_id
            );
            return;
        }
        info!("Starting review for patchset {}", patchset_id);

        // Held for the lifetime of the task: whichever way this returns, every
        // entry for this patchset is cleared rather than left claiming work is
        // still running. That includes the cancellation paths, which end this
        // task (and kill its workers) rather than signalling them.
        let activity = crate::activity::PatchsetActivityGuard::new(
            ctx.activity.clone(),
            patchset_id,
            crate::activity::Phase::Queued,
        );

        // Clock for the whole patchset, covering every patch and every retry.
        //
        // Seeded from what is already banked, because a review interrupted by a
        // restart is not redone from scratch: patches that completed keep their
        // reviews and are skipped on the next run, so the earlier run bought
        // work the final review still rests on.
        let patchset_review_started = Instant::now();
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

        // Checkpointed while the review runs, not only at the end: a daemon
        // killed mid-review never reaches the end, and that is exactly the run
        // whose time would otherwise go unrecorded.
        let _clock_guard = ReviewClockGuard::new(
            ctx.db.clone(),
            activity.run(),
            carried_seconds,
            patchset_review_started,
        );

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
                Self::mark_patchset_failed_unless_cancelled(&ctx.db, patchset_id).await;
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

        let body_msg_id = patchset
            .message_id
            .as_deref()
            .or_else(|| patches_json.first().and_then(|p| p["message_id"].as_str()));
        let body = match body_msg_id {
            Some(mid) => match ctx.db.get_message_body(mid).await {
                Ok(b) => b,
                Err(e) => {
                    error!(
                        "Failed to fetch message body {} for patchset {}: {}",
                        mid, patchset_id, e
                    );
                    Self::mark_patchset_failed_unless_cancelled(&ctx.db, patchset_id).await;
                    return;
                }
            },
            None => None,
        };
        let subject = patchset.subject.clone().unwrap_or("Unknown".to_string());
        // A baseline recorded before ingestion required an object ID may be
        // anything, and anything else resolves nothing, so the series is
        // reviewed on the candidates it would have had without it.
        let forced_baseline = match patchset.baseline_id {
            Some(bid) => match ctx.db.get_baseline_commit(bid).await {
                Ok(Some(commit)) => CommitId::parse(&commit).or_else(|| {
                    warn!(
                        "Ignoring forced baseline of patchset {}: not a commit ID",
                        patchset_id
                    );
                    None
                }),
                Ok(None) => None,
                Err(e) => {
                    error!(
                        "Failed to fetch baseline {} for patchset {}: {}",
                        bid, patchset_id, e
                    );
                    Self::mark_patchset_failed_unless_cancelled(&ctx.db, patchset_id).await;
                    return;
                }
            },
            None => None,
        };
        let candidates = if let Some(commit) = forced_baseline {
            info!(
                "Using forced baseline commit {} from ingestion for patchset {}",
                commit, patchset_id
            );
            vec![BaselineResolution::Commit(commit)]
        } else {
            ctx.baseline_registry
                .resolve_candidates(&all_files, &subject, body.as_deref())
                .await
        };

        // 1. Find a working baseline (apply series)
        activity.update(crate::activity::Phase::GitOp {
            op: "baseline resolution (applying the series)".to_string(),
        });
        let (found_baseline, patch_commits, logs) = Self::prepare_baseline_worktree(
            &ctx,
            patchset_id,
            &candidates,
            &diffs,
            patchset.message_id.as_deref(),
            body.as_deref(),
        )
        .await;

        let prompts_hash = Some(env!("GIT_HASH"));

        // Save findings to patchset
        if let Some((baseline_id, worktree, review_baseline)) = found_baseline {
            if ctx
                .db
                .get_patchset_status(patchset_id)
                .await
                .ok()
                .flatten()
                .as_deref()
                == Some(ReviewStatus::Cancelled.as_str())
            {
                info!(
                    "Patchset {} was cancelled during baseline preparation, aborting",
                    patchset_id
                );
                if let Err(e) = worktree.remove().await {
                    warn!(
                        "Failed to remove worktree after patchset {} cancellation: {}",
                        patchset_id, e
                    );
                }
                return;
            }

            if let Err(e) = ctx
                .db
                .update_patchset_baseline_info(
                    patchset_id,
                    Some(baseline_id),
                    Some(ctx.settings.ai.model.as_str()),
                    prompts_hash,
                    Some(logs.as_str()),
                    Some(ctx.settings.ai.provider.as_str()),
                )
                .await
            {
                warn!(
                    "Failed to update baseline info for patchset {}: {}",
                    patchset_id, e
                );
            }

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

            let input_payload = json!({
                "id": patchset_id,
                "message_id": patchset_msg_id,
                "subject": patchset.subject.clone().unwrap_or("Unknown".to_string()),
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
                    if let Err(e) = ctx.db.update_patch_status(*patch_id, "Skipped").await {
                        warn!(
                            "Failed to mark patch {} as Skipped for patchset {}: {}",
                            patch_id, patchset_id, e
                        );
                    }
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

            // The one place the patchset's own phase moves once review work
            // begins. Set here rather than from the patches, so the clock behind
            // it spans the whole review instead of restarting whenever a patch
            // moves.
            activity.update(crate::activity::Phase::ReviewingPatches {
                patches: total_valid,
            });
            let valid_jobs_queue = Arc::new(tokio::sync::Mutex::new(valid_jobs));
            let mut handles = Vec::new();
            let baseline_ref_str = review_baseline;

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
                            if ctx_clone
                                .db
                                .get_patchset_status(patchset_id)
                                .await
                                .ok()
                                .flatten()
                                .as_deref()
                                == Some(ReviewStatus::Cancelled.as_str())
                            {
                                queue.lock().await.clear();
                                break;
                            }
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
                                    Ok(PatchResult::Cancelled) => {
                                        queue.lock().await.clear();
                                        break;
                                    }
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
                if ctx
                    .db
                    .get_patchset_status(patchset_id)
                    .await
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some(ReviewStatus::Cancelled.as_str())
                {
                    valid_jobs_queue.lock().await.clear();
                    break;
                }
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
                        Ok(PatchResult::Cancelled) => {
                            valid_jobs_queue.lock().await.clear();
                            break;
                        }
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
            if let Err(e) = worktree.remove().await {
                warn!(
                    "Failed to remove worktree for patchset {}: {}",
                    patchset_id, e
                );
            }

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

                if let Err(e) = ctx
                    .db
                    .update_patchset_status(patchset_id, &final_status)
                    .await
                {
                    warn!(
                        "Failed to update patchset {} status to {}: {}",
                        patchset_id, final_status, e
                    );
                }

                let elapsed = carried_seconds + patchset_review_started.elapsed().as_secs();
                // A superseded run's total belongs to a review that has been
                // restarted; the newer run records its own.
                if activity.run().is_current() {
                    let _ = ctx
                        .db
                        .set_patchset_review_duration(patchset_id, elapsed)
                        .await;
                }
                info!(
                    "Patchset {} finished review in {} ({})",
                    patchset_id,
                    crate::activity::format_duration(elapsed),
                    final_status
                );

                if review_success {
                    if let Err(e) = Self::queue_forge_pr_comment(&ctx, &patchset, &diffs).await {
                        error!(
                            "Failed to queue forge PR comment for patchset {}: {}",
                            patchset_id, e
                        );
                    }

                    if patchset.embargo_until.is_some()
                        && let Err(e) = Self::release_patchset_results(&ctx, &patchset).await
                    {
                        error!(
                            "Failed to release clean patchset {} immediately: {}",
                            patchset_id, e
                        );
                    }
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
            let total_hits = stats.hits_this_session + stats.hits_prev_session;
            let total_tokens = stats.tokens_saved_this_session + stats.tokens_saved_prev_session;
            if total_hits > 0 {
                info!(
                    "Patchset {} cache summary — {} hits ({} this session, {} previous), {} tokens saved ({} this session, {} previous)",
                    patchset_id,
                    total_hits,
                    stats.hits_this_session,
                    stats.hits_prev_session,
                    total_tokens,
                    stats.tokens_saved_this_session,
                    stats.tokens_saved_prev_session,
                );
            }
        }
    }

    async fn apply_prerequisites(
        worktree: &GitWorktree,
        prerequisites: &[PrerequisitePatch],
        baseline_sha: &str,
        logs: &mut String,
    ) -> Result<String> {
        let mut review_baseline_sha = baseline_sha.to_string();

        for prerequisite in prerequisites {
            let mbox = git_am_message(
                &prerequisite.author,
                prerequisite.date,
                &prerequisite.subject,
                &prerequisite.diff,
            )
            .with_context(|| {
                format!(
                    "Failed to format prerequisite {} ({})",
                    prerequisite.git_patch_id, prerequisite.message_id
                )
            })?;
            worktree.apply_patch(&mbox).await.map_err(|e| {
                anyhow!(
                    "Prerequisite {} ({}) failed to apply: {}",
                    prerequisite.git_patch_id,
                    prerequisite.message_id,
                    e
                )
            })?;

            let sha = get_commit_hash(&worktree.path, "HEAD").await.map_err(|e| {
                anyhow!(
                    "Failed to resolve HEAD after prerequisite {} ({}): {}",
                    prerequisite.git_patch_id,
                    prerequisite.message_id,
                    e
                )
            })?;
            logs.push_str(&format!(
                "Applied prerequisite {} ({}) as {}.\n",
                prerequisite.git_patch_id, prerequisite.message_id, sha
            ));
            review_baseline_sha = sha;
        }

        Ok(review_baseline_sha)
    }

    /// Fetch an explicit base-commit that the object store is missing.
    ///
    /// Returns the resolved SHA, or the error of the last resolution attempt.
    async fn fetch_missing_commit(
        repo_path: &Path,
        mainline_remote: &str,
        sha: &CommitId,
        candidates: &[BaselineResolution],
    ) -> Result<String> {
        // A base-commit that is already merged upstream comes straight from
        // the mainline remote.
        Self::try_fetch(repo_path, &["fetch", mainline_remote, sha.as_str()]).await;
        if let Ok(resolved) = get_commit_hash(repo_path, sha.as_str()).await {
            return Ok(resolved);
        }

        // b4 can emit annotated tag object SHAs as base-commit (e.g. the tag
        // object for v7.2-rc2). Those aren't fetchable by SHA, so pull tags
        // from the mainline remote and retry.
        Self::try_fetch(repo_path, &["fetch", mainline_remote, "--tags"]).await;
        if let Ok(resolved) = get_commit_hash(repo_path, sha.as_str()).await {
            return Ok(resolved);
        }

        // What is left is a base-commit that only exists in the tree the
        // series was built on.  Those trees are candidates for this series
        // already, so ask each of them for that one commit.  Asking by SHA
        // keeps the cost proportional to what is missing, needs no remote to
        // be configured, takes no per-remote lock, and leaves the fetch
        // schedule and the failure backoff of ensure_remote untouched for the
        // candidate loop that follows.
        //
        // One budget covers the whole search, so a base-commit no tree has
        // costs a bounded wait however many trees the series touches.
        let deadline = Instant::now() + FETCH_TIMEOUT;
        // The mainline remote answered above, and a candidate tree can carry
        // its local name, so recognise it by URL.
        let mainline_url = crate::git_cmd::in_dir_async(repo_path)
            .args(["remote", "get-url", "--end-of-options", mainline_remote])
            .kill_on_drop(true)
            .output()
            .await
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());

        let mut asked: Vec<(&str, &str)> = Vec::new();
        for candidate in candidates {
            let BaselineResolution::RemoteTarget { url, name, .. } = candidate else {
                continue;
            };
            // One tree reaches the candidate list once per branch it offers.
            if Some(url.as_str()) == mainline_url.as_deref()
                || asked.iter().any(|(_, seen)| *seen == url.as_str())
            {
                continue;
            }
            let budget = deadline.saturating_duration_since(Instant::now());
            if budget.is_zero() {
                break;
            }
            asked.push((name, url));

            // upload-pack serves an object nothing advertises only over
            // protocol v2, and the fetch writes no FETCH_HEAD so that
            // concurrent reviews of the same repository do not serialise on
            // its lock.
            Self::try_fetch_within(
                repo_path,
                &[
                    "-c",
                    "protocol.version=2",
                    "fetch",
                    "--no-tags",
                    "--no-write-fetch-head",
                    "--end-of-options",
                    url,
                    sha.as_str(),
                ],
                budget,
            )
            .await;
            if let Ok(resolved) = get_commit_hash(repo_path, sha.as_str()).await {
                info!("Fetched base-commit {} from {}", sha, name);
                return Ok(resolved);
            }
        }

        // Name what was asked, and claim no more than that: a fetch that
        // times out or is refused says nothing about the tree holding the
        // commit.  The baseline log is what a maintainer reads to find out
        // why their base-commit was ignored.
        get_commit_hash(repo_path, sha.as_str()).await.map_err(|e| {
            let mut sources = vec![mainline_remote];
            sources.extend(asked.iter().map(|(name, _)| *name));
            anyhow!("{} (not obtained from {})", e, sources.join(", "))
        })
    }

    /// Run a fetch bounded by the default budget.
    async fn try_fetch(repo_path: &Path, args: &[&str]) {
        Self::try_fetch_within(repo_path, args, FETCH_TIMEOUT).await
    }

    /// Run a bounded fetch, ignoring its outcome: callers judge the result by
    /// resolving the revision they are after.
    async fn try_fetch_within(repo_path: &Path, args: &[&str], timeout: Duration) {
        let _ = tokio::time::timeout(
            timeout,
            crate::git_cmd::in_dir_async(repo_path)
                .args(crate::git_ops::GIT_PROTOCOL_RESTRICTIONS)
                .args(args)
                .kill_on_drop(true)
                .output(),
        )
        .await;
    }

    async fn prepare_baseline_worktree(
        ctx: &ReviewContext,
        patchset_id: i64,
        candidates: &[BaselineResolution],
        diffs: &[(i64, i64, String, String, String, i64, String)],
        target_message_id: Option<&str>,
        prerequisite_metadata: Option<&str>,
    ) -> (
        Option<(i64, GitWorktree, String)>,
        HashMap<i64, String>,
        String,
    ) {
        let mut attempts: Vec<BaselineAttempt> = Vec::new();
        let prerequisites = match resolve_prerequisites_from_lore(
            &ctx.db,
            patchset_id,
            target_message_id,
            prerequisite_metadata.unwrap_or_default(),
        )
        .await
        {
            Ok(patches) => patches,
            Err(e) => {
                let message = format!(
                    "Failed to resolve prerequisites: {}\n",
                    redact_secret(&format!("{e:#}")),
                );
                error!("{}", message.trim());
                attempts.push(BaselineAttempt {
                    baseline: "prerequisites".to_string(),
                    status: "Failed".to_string(),
                    log: message,
                });
                return (
                    None,
                    HashMap::new(),
                    serde_json::to_string(&attempts).unwrap_or_default(),
                );
            }
        };
        let repo_path = PathBuf::from(&ctx.settings.git.repository_path);
        let mainline_remote = ctx.baseline_registry.mainline_remote_name();
        let mut tested_shas = std::collections::HashSet::new();

        // A candidate already merged into mainline goes to the back of the
        // queue, flagged so that it is not deferred a second time.
        let mut queue: std::collections::VecDeque<(&BaselineResolution, bool)> =
            candidates.iter().map(|c| (c, false)).collect();

        while let Some((candidate, deferred)) = queue.pop_front() {
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
            let resolved = match get_commit_hash(&repo_path, &baseline_ref).await {
                Ok(sha) => Ok(sha),
                Err(e) => match candidate {
                    BaselineResolution::Commit(sha_str) => {
                        Self::fetch_missing_commit(&repo_path, mainline_remote, sha_str, candidates)
                            .await
                    }
                    _ => Err(e),
                },
            };
            let baseline_sha = match resolved {
                Ok(sha) => sha,
                Err(e) => {
                    let msg = format!("Failed to resolve baseline ref {}: {}\n", baseline_ref, e);
                    current_log.push_str(&msg);
                    attempts.push(BaselineAttempt {
                        baseline: baseline_ref.clone(),
                        status: current_status,
                        log: current_log,
                    });
                    continue;
                }
            };

            if !deferred
                && ctx
                    .baseline_registry
                    .is_merged_default_head(candidate, &baseline_sha)
                    .await
            {
                current_log.push_str(&format!(
                    "{} is already merged into {}/master; deferring it until \
                     the other candidates have been tried.\n",
                    baseline_sha, mainline_remote
                ));
                attempts.push(BaselineAttempt {
                    baseline: format!("{} ({})", baseline_ref, baseline_sha),
                    status: "Skipped".to_string(),
                    log: current_log,
                });
                queue.push_back((candidate, true));
                continue;
            }

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
                ctx.settings.review.worktree_dir.as_deref().map(Path::new),
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
            let mut review_baseline_sha = baseline_sha.clone();

            match Self::apply_prerequisites(
                &worktree,
                &prerequisites,
                &baseline_sha,
                &mut apply_logs,
            )
            .await
            {
                Ok(sha) => review_baseline_sha = sha,
                Err(e) => {
                    apply_logs.push_str(&format!("{e}\n"));
                    application_failed = true;
                }
            }

            for (i, (patch_id, index, diff, subject, author, date_ts, msg_id)) in
                diffs.iter().enumerate()
            {
                if application_failed {
                    break;
                }
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
                    let mbox = match git_am_message(author, *date_ts, subject, diff) {
                        Ok(mbox) => mbox,
                        Err(e) => {
                            apply_logs.push_str(&format!(
                                "Patch {}/{} (ID: {}) has an invalid date: {}\n",
                                patchset_id, index, patch_id, e
                            ));
                            application_failed = true;
                            break;
                        }
                    };

                    // Try git am
                    match worktree.apply_patch(&mbox).await {
                        Ok(()) => applied = true,
                        Err(e) => {
                            apply_logs.push_str(&format!(
                                "Patch application error: {}\n",
                                PatchApplyFailure::from_error(&e).public_message()
                            ));
                        }
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
                current_log.push_str(&apply_logs);
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
                        Some((bid, worktree, review_baseline_sha)),
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

        let patch = PatchUnderReview {
            patchset_id,
            patch_id,
            index,
            baseline_ref,
            commit_sha: commit_sha.as_deref(),
            worktree_path,
            input_payload,
            embargo_until,
        };
        let mut retries = 0;
        let max_retries = ctx.settings.review.max_retries;

        // Started before the retry loop, so the recorded duration covers every
        // attempt. Timing each attempt separately would undercount a review that
        // needed three goes, which is exactly the case worth measuring.
        let review_started = Instant::now();

        let mut existing_pending_review_id = ctx
            .db
            .get_pending_review_id(patchset_id, Some(patch_id))
            .await?;

        loop {
            if Self::patchset_cancelled(ctx, patchset_id).await {
                info!(
                    "Patchset {} was cancelled before reviewing patch {}/{}",
                    patchset_id, patchset_id, index
                );
                Self::mark_attempt_cancelled(
                    ctx,
                    existing_pending_review_id.take(),
                    patch_id,
                    None,
                    None,
                )
                .await;
                return Ok(PatchResult::Cancelled);
            }

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

            if let Err(e) = ctx
                .db
                .update_review_status(review_id, ReviewStatus::InReview.as_str(), None)
                .await
            {
                warn!(
                    "Failed to update review {} status to In Review: {}",
                    review_id, e
                );
            }

            // Stamped before the work runs, not after, so a row that never
            // reaches a terminal state still records which attempt it was.
            let _ = ctx.db.set_review_attempt(review_id, retries + 1).await;

            // Owned here rather than by the run, so what it saw of the stages
            // outlives a run that ended in an error.
            let mut stages = crate::activity::StageTracker::new(ctx.activity.clone());
            let result = run_review_tool(
                ctx.worker_program.as_deref(),
                patchset_id,
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
                PatchActivity {
                    registry: ctx.activity.clone(),
                    patch_id,
                    attempt: retries + 1,
                    max_attempts: max_retries + 1,
                },
                &mut stages,
            )
            .await;

            // Stamped once per attempt with the cumulative elapsed time, so
            // whichever row ends up final carries the full total. Recorded here
            // rather than at each of the loop's exit points, which would be easy
            // to add one more of and forget.
            let _ = ctx
                .db
                .set_review_duration(review_id, review_started.elapsed().as_secs())
                .await;

            let end = AttemptEnd::decide(
                result,
                index,
                ctx.settings.ai.no_ai,
                Self::patchset_cancelled(ctx, patchset_id).await,
            );
            let conversation = Self::settle_attempt(ctx, review_id, &mut stages, &end).await;

            let verdict = match end {
                AttemptEnd::Finished { output } => {
                    Self::store_finished_attempt(ctx, &patch, review_id, &output).await?
                }
                AttemptEnd::Cancelled { output } => {
                    info!(
                        "Review {} of patch {}/{} was cancelled",
                        review_id, patchset_id, index
                    );
                    Self::mark_attempt_cancelled(
                        ctx,
                        Some(review_id),
                        patch_id,
                        output.as_ref(),
                        conversation,
                    )
                    .await;
                    AttemptVerdict::Final(PatchResult::Cancelled)
                }
                AttemptEnd::TimedOut { timeout } => {
                    error!(
                        "Review {} of patch {}/{} timed out after {} active seconds; \
                         not retrying",
                        review_id, patchset_id, index, timeout.active_seconds
                    );
                    Self::fail_review_row(
                        ctx,
                        review_id,
                        &format!("Tool error: {}", timeout),
                        None,
                        conversation,
                    )
                    .await;
                    // A review that used up its whole deadline has shown it does
                    // not fit in one, and a retry gets an identical deadline.
                    // Retrying is how one patch came to spend four deadlines
                    // and produce nothing; a rerun is one command away.
                    AttemptVerdict::GiveUp
                }
                AttemptEnd::Failed { reason, output } => {
                    error!(
                        "Review {} of patch {}/{} failed: {}",
                        review_id, patchset_id, index, reason
                    );
                    Self::fail_review_row(ctx, review_id, &reason, output.as_ref(), conversation)
                        .await;
                    // A worker that hits its own deadline also ends here and is
                    // retried, because its timeout reaches the daemon as an
                    // untyped error rather than as a ReviewTimedOut.
                    AttemptVerdict::Retry
                }
            };

            match verdict {
                AttemptVerdict::Final(result) => return Ok(result),
                AttemptVerdict::Retry if retries < max_retries => retries += 1,
                AttemptVerdict::Retry | AttemptVerdict::GiveUp => {
                    let _ = ctx.db.update_patch_status(patch_id, "Failed").await;
                    return Ok(PatchResult::ReviewFailed);
                }
            }
        }
    }

    /// Whether the patchset has been cancelled. A status that cannot be read
    /// counts as not cancelled, so a passing database error does not end a
    /// review that is still wanted.
    async fn patchset_cancelled(ctx: &ReviewContext, patchset_id: i64) -> bool {
        ctx.db
            .get_patchset_status(patchset_id)
            .await
            .ok()
            .flatten()
            .as_deref()
            == Some(ReviewStatus::Cancelled.as_str())
    }

    /// Marks the attempt's review row, when it has one, and the patch as
    /// cancelled. `output` is the worker's output when it returned a review
    /// before the cancel was seen, and `conversation` is what the attempt
    /// streamed when it returned none; see [`Self::attempt_record`].
    async fn mark_attempt_cancelled(
        ctx: &ReviewContext,
        review_id: Option<i64>,
        patch_id: i64,
        output: Option<&Value>,
        conversation: Option<String>,
    ) {
        if let Some(rid) = review_id {
            let (interaction_id, logs) = Self::attempt_record(ctx, output, conversation).await;
            if let Err(e) = ctx
                .db
                .complete_review(
                    rid,
                    ReviewStatus::Cancelled.as_str(),
                    "Cancelled because patchset was superseded or cancelled",
                    None,
                    interaction_id.as_deref(),
                    None,
                    logs.as_deref(),
                )
                .await
            {
                warn!("Failed to mark review {} as Cancelled: {}", rid, e);
            }
        }
        if let Err(e) = ctx
            .db
            .update_patch_status(patch_id, ReviewStatus::Cancelled.as_str())
            .await
        {
            warn!("Failed to mark patch {} as Cancelled: {}", patch_id, e);
        }
    }

    /// Settles the stages still running when the worker stopped, and writes
    /// the daemon's record of an attempt that ended without a review from the
    /// worker. Returns the conversation that attempt streamed, for its review
    /// row's log, or `None` when the worker returned a review or nothing was
    /// streamed.
    ///
    /// Settling always runs, because it is also what stops a killed worker's
    /// stage rows claiming to be mid-turn. When the worker returned a review,
    /// its own list of stage failures and its own history are the record, and
    /// nothing is still running in practice.
    async fn settle_attempt(
        ctx: &ReviewContext,
        review_id: i64,
        stages: &mut crate::activity::StageTracker,
        end: &AttemptEnd,
    ) -> Option<String> {
        let settled = stages.settle(end.settle_cause());
        if !settled.is_empty() {
            info!(
                "Review {} stopped with {} stage(s) still running: {:?}",
                review_id,
                settled.len(),
                settled.iter().map(|s| s.stage.as_str()).collect::<Vec<_>>()
            );
        }
        if end.has_worker_review() {
            return None;
        }
        Self::record_unfinished_attempt(ctx, review_id, stages).await
    }

    /// Writes the daemon's record of an attempt with no review from the
    /// worker: which stages did not run to completion and why. Returns the
    /// conversation the attempt streamed, promoted to a final log.
    ///
    /// The worker's output carries neither for a patch whose review failed,
    /// and a killed worker returns no output at all, so without this a failed
    /// or timed-out row could not say which stage was stuck or what the model
    /// was doing when it stopped. The conversation is read here, before the
    /// row is completed, because completing it deletes the streamed copy.
    async fn record_unfinished_attempt(
        ctx: &ReviewContext,
        review_id: i64,
        stages: &crate::activity::StageTracker,
    ) -> Option<String> {
        let project = ctx.settings.project.kind.unwrap_or_default();
        Self::store_stopped_stages(ctx, review_id, project, stages).await;
        match promoted_live_log(&ctx.db, review_id, project).await {
            Ok(log) => log,
            Err(e) => {
                warn!(
                    "Failed to keep the streamed conversation of review {}: {}",
                    review_id, e
                );
                None
            }
        }
    }

    /// Writes the stages of an unfinished attempt that did not run to
    /// completion, in pipeline order.
    ///
    /// Taken from the stage tracker, which saw every stage failure the worker
    /// reported as well as the stages settled when it stopped.
    async fn store_stopped_stages(
        ctx: &ReviewContext,
        review_id: i64,
        project: crate::project::ProjectId,
        stages: &crate::activity::StageTracker,
    ) {
        let mut failures = stages.stopped();
        if failures.is_empty() {
            return;
        }
        failures.sort_by_cached_key(|f| pipeline_position(project, &f.stage));
        let stored = match serde_json::to_string(&failures) {
            Ok(json) => ctx.db.set_review_stage_failures(review_id, &json).await,
            Err(e) => Err(e.into()),
        };
        if let Err(e) = stored {
            warn!(
                "Failed to record the unfinished stages of review {}: {}",
                review_id, e
            );
        }
    }

    /// Marks an attempt's review row Failed with `reason`. `output` is the
    /// worker's output when it returned one, and `conversation` is what the
    /// attempt streamed; see [`Self::attempt_record`].
    async fn fail_review_row(
        ctx: &ReviewContext,
        review_id: i64,
        reason: &str,
        output: Option<&Value>,
        conversation: Option<String>,
    ) {
        let (interaction_id, logs) = Self::attempt_record(ctx, output, conversation).await;
        if let Err(e) = ctx
            .db
            .complete_review(
                review_id,
                ReviewStatus::Failed.as_str(),
                reason,
                None,
                interaction_id.as_deref(),
                None,
                logs.as_deref(),
            )
            .await
        {
            warn!("Failed to mark review {} as Failed: {}", review_id, e);
        }
    }

    /// The AI interaction and the log a review row keeps of an attempt that
    /// did not store a review: `(interaction_id, logs)`.
    ///
    /// `output` is the worker's output when it returned one; its token counts
    /// are stored as the interaction. `conversation` is what the attempt
    /// streamed, which [`Self::settle_attempt`] gives only for an attempt with
    /// no review from the worker, and is the log when there is one: a worker
    /// output with no review carries an empty history, so the worker's
    /// history is the log only in its absence.
    async fn attempt_record(
        ctx: &ReviewContext,
        output: Option<&Value>,
        conversation: Option<String>,
    ) -> (Option<String>, Option<String>) {
        let (interaction_id, history) = match output {
            Some(output) => Self::record_worker_output(ctx, output).await,
            None => (None, None),
        };
        (interaction_id, conversation.or(history))
    }

    /// Stores the worker output's token counts as an AI interaction, and
    /// renders its history as the review's log. Returns the interaction's id,
    /// when the output had token counts, and the log.
    async fn record_worker_output(
        ctx: &ReviewContext,
        output: &Value,
    ) -> (Option<String>, Option<String>) {
        let logs = output.get("history").and_then(|h| {
            let mut scrubbed = h.clone();
            crate::ai::scrub_thought_signatures(&mut scrubbed);
            serde_json::to_string_pretty(&scrubbed).ok()
        });

        let Some(tokens_in) = output["tokens_in"].as_u64() else {
            return (None, logs);
        };
        let id = generate_interaction_id();
        let input_ctx = output["input_context"].as_str().unwrap_or("");
        let output_raw = if let Some(r) = output.get("review") {
            r.to_string()
        } else if let Some(e) = output.get("error") {
            e.to_string()
        } else {
            String::new()
        };
        let _ = ctx
            .db
            .create_ai_interaction(AiInteractionParams {
                id: &id,
                parent_id: None,
                workflow_id: None,
                provider: &ctx.settings.ai.provider,
                model: &ctx.settings.ai.model,
                input: input_ctx,
                output: &output_raw,
                tokens_in: tokens_in as u32,
                tokens_out: output["tokens_out"].as_u64().unwrap_or(0) as u32,
                tokens_cached: output["tokens_cached"].as_u64().unwrap_or(0) as u32,
            })
            .await;
        (Some(id), logs)
    }

    /// Records incomplete coverage the worker reported for a review it
    /// returned. Written before anything else touches the review row, so a
    /// partial review can never be presented as a complete one.
    async fn store_worker_stage_failures(ctx: &ReviewContext, review_id: i64, output: &Value) {
        let Some(failures) = output
            .get("review")
            .and_then(|r| r.get("stage_failures"))
            .or_else(|| output.get("stage_failures"))
            .and_then(|f| f.as_array())
            .filter(|f| !f.is_empty())
        else {
            return;
        };
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

    /// Stores an attempt in which the worker returned a review for the patch,
    /// or skipped the review as it was asked to.
    async fn store_finished_attempt(
        ctx: &ReviewContext,
        patch: &PatchUnderReview<'_>,
        review_id: i64,
        output: &Value,
    ) -> Result<AttemptVerdict> {
        Self::store_worker_stage_failures(ctx, review_id, output).await;
        let (interaction_id, logs) = Self::record_worker_output(ctx, output).await;

        // `AttemptEnd::classify` lets a null review through only when the AI
        // review was turned off.
        let Some(review_content) = output.get("review").filter(|r| !r.is_null()) else {
            info!(
                "Review skipped as requested for ps={} idx={}",
                patch.patchset_id, patch.index
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
                    logs.as_deref(),
                )
                .await;
            let _ = ctx.db.update_patch_status(patch.patch_id, "Skipped").await;
            return Ok(AttemptVerdict::Final(PatchResult::Success));
        };

        Self::store_review_findings(ctx, review_id, review_content).await?;
        if ctx.settings.linux_bug.enabled
            && let Some(concerns) = review_content.get("concerns").and_then(|f| f.as_array())
        {
            for concern in concerns {
                Self::hand_off_concern(ctx, patch, review_id, concern).await;
            }
        }

        let summary = review_content["summary"].as_str().unwrap_or("");
        let inline_review = output["inline_review"].as_str();
        if let Err(e) = ctx
            .db
            .complete_review(
                review_id,
                ReviewStatus::Reviewed.as_str(),
                "Review completed successfully.",
                Some(summary),
                interaction_id.as_deref(),
                inline_review,
                logs.as_deref(),
            )
            .await
        {
            error!("Failed to save review completion: {}", e);
            let _ = ctx.db.update_patch_status(patch.patch_id, "Failed").await;
            return Ok(AttemptVerdict::Final(PatchResult::ReviewFailed));
        }

        if let Some(inline) = inline_review {
            Self::notify_reviewed_patch(ctx, patch, review_id, inline, review_content, summary)
                .await;
        }
        // Failing the patchset over a notification would strand it:
        // get_pending_patchsets() selects only Pending rows.
        let _ = ctx.db.update_patch_status(patch.patch_id, "Reviewed").await;
        Ok(AttemptVerdict::Final(PatchResult::Success))
    }

    /// Stores the findings of a review the worker returned.
    async fn store_review_findings(
        ctx: &ReviewContext,
        review_id: i64,
        review_content: &Value,
    ) -> Result<()> {
        let Some(findings) = review_content.get("findings").and_then(|f| f.as_array()) else {
            return Ok(());
        };
        for f in findings {
            let preexisting = f["preexisting"].as_bool();
            if preexisting == Some(true) && !ctx.settings.linux_bug.enabled {
                continue;
            }
            ctx.db
                .create_finding(Finding {
                    review_id,
                    severity: Severity::from_str(f["severity"].as_str().unwrap_or("Low")),
                    severity_explanation: f["severity_explanation"].as_str().map(|s| s.to_string()),
                    problem: f["problem"].as_str().unwrap_or("").to_string(),
                    preexisting,
                    locations: f.get("locations").cloned(),
                })
                .await?;
        }
        Ok(())
    }

    /// Queues the patch's notifications for a review just stored, unless the
    /// patchset is still embargoed.
    async fn notify_reviewed_patch(
        ctx: &ReviewContext,
        patch: &PatchUnderReview<'_>,
        review_id: i64,
        inline: &str,
        review_content: &Value,
        summary: &str,
    ) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if let Some(until) = patch.embargo_until.filter(|&u| u > now) {
            info!(
                "Review completed but embargoed until {} for patch {}/{}",
                until, patch.patchset_id, patch.index
            );
            return;
        }

        let patch_msg_id = patch.input_payload["patches"]
            .as_array()
            .and_then(|arr| arr.iter().find(|p| p["index"] == patch.index))
            .and_then(|p| p["message_id"].as_str())
            .unwrap_or("");
        let patchset_msg_id = patch.input_payload["message_id"]
            .as_str()
            .unwrap_or(patch_msg_id);

        if let Err(e) = Self::queue_notifications(
            ctx,
            patch.patchset_id,
            Some(review_id),
            patch.patch_id,
            patch_msg_id,
            patchset_msg_id,
            patch.index,
            inline,
            review_content["findings"].as_array(),
            summary,
        )
        .await
        {
            error!(
                "Failed to queue email for patch {}/{} (ID: {}): {}",
                patch.patchset_id, patch.index, patch.patch_id, e
            );
        }
    }

    /// Hands one concern of a stored review to the bug discovery pipeline.
    /// A concern with no description is skipped, and a failure is logged:
    /// neither affects the review.
    async fn hand_off_concern(
        ctx: &ReviewContext,
        patch: &PatchUnderReview<'_>,
        review_id: i64,
        concern: &Value,
    ) {
        let problem = concern
            .get("description")
            .or_else(|| concern.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if problem.is_empty() {
            return;
        }
        let reasoning = concern
            .get("reasoning")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let locations = concern.get("locations").cloned();
        let source_files: Vec<String> = locations
            .as_ref()
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|loc| loc.get("file").and_then(|v| v.as_str()))
            .map(str::to_string)
            .collect();
        let project = ctx.settings.project.kind.unwrap_or_default();
        let input = crate::api::BugInput {
            problem,
            reasoning,
            locations,
            subsystems: Self::concern_subsystems(ctx, project, &source_files),
            source_files,
            commit_sha: patch.commit_sha.map(str::to_string),
            patchset_id: Some(patch.patchset_id),
            patch_id: Some(patch.patch_id),
            baseline_sha: Some(patch.baseline_ref.to_string()),
            review_id: Some(review_id),
        };
        let toolbox = Self::mainline_toolbox(ctx, patch.worktree_path).await;
        let review_tool = match project {
            crate::project::ProjectId::Linux => "sashiko:linux_patch_review",
            crate::project::ProjectId::Sashiko => "sashiko:sashiko_patch_review",
        };
        let discovery_db = ctx.db.with_bug_actor(
            ctx.settings.project.attribution(),
            review_tool,
            Some(ctx.provider.get_capabilities().model_name),
        );
        if let Err(e) = crate::workflows::linux_bug::process_issue_for_project(
            ctx.provider.as_ref(),
            toolbox,
            &discovery_db,
            input,
            Some("bug"),
            project,
        )
        .await
        {
            warn!("Failed to queue candidate pre-existing bug: {}", e);
        }
    }

    /// The subsystems a concern touching `source_files` is attributed to.
    fn concern_subsystems(
        ctx: &ReviewContext,
        project: crate::project::ProjectId,
        source_files: &[String],
    ) -> Vec<crate::db::AttributedSubsystem> {
        if !project.uses_maintainers() {
            return crate::workflows::linux_bug::extract_directory_subsystems_for_project(
                source_files,
                project,
            )
            .into_iter()
            .map(crate::db::AttributedSubsystem::from_path_prefix)
            .collect();
        }
        let names = if let Some(mindex) = crate::maintainers::get_global_maintainers() {
            mindex.match_files(source_files)
        } else if let Ok(mindex) = crate::maintainers::MaintainersIndex::from_top_of_trunk(
            &ctx.settings.git.repository_path,
        ) {
            mindex.match_files(source_files)
        } else {
            Vec::new()
        };
        names
            .into_iter()
            .map(crate::db::AttributedSubsystem::from_maintainers)
            .collect()
    }

    /// A toolbox over the repository, or else the review's worktree, whose
    /// head is the mainline tip when one can be found.
    async fn mainline_toolbox(
        ctx: &ReviewContext,
        worktree_path: Option<&Path>,
    ) -> Option<Arc<crate::toolbox::ToolBox>> {
        let repo_path = std::path::PathBuf::from(&ctx.settings.git.repository_path);
        let mainline_remote = ctx.baseline_registry.mainline_remote_name();
        let candidate_refs = [
            format!("{}/master", mainline_remote),
            format!("{}/main", mainline_remote),
            "origin/master".to_string(),
            "origin/main".to_string(),
            "master".to_string(),
            "main".to_string(),
            "HEAD".to_string(),
        ];
        let mut mainline_sha = None;
        for candidate_ref in &candidate_refs {
            if let Ok(sha) = get_commit_hash(&repo_path, candidate_ref).await {
                mainline_sha = Some(sha);
                break;
            }
        }

        let target_dir = if repo_path.exists() {
            repo_path
        } else {
            worktree_path?.to_path_buf()
        };
        let mut tb = crate::toolbox::ToolBox::new(target_dir, None);
        if let Some(m_sha) = mainline_sha {
            tb.set_virtual_head(m_sha);
        }
        Some(Arc::new(tb))
    }
}

/// The patch one call of `process_patch_review` reviews, and what its review
/// runs against. Grouped because the helpers that store an attempt need most
/// of it.
struct PatchUnderReview<'a> {
    patchset_id: i64,
    patch_id: i64,
    index: i64,
    baseline_ref: &'a str,
    commit_sha: Option<&'a str>,
    worktree_path: Option<&'a Path>,
    input_payload: &'a Value,
    embargo_until: Option<i64>,
}

/// How one worker run for a patch ended.
///
/// Decided once, right after the run, so that what is recorded and whether
/// the patch is tried again follow from one value. Whether the I/O loop
/// returned `Err` does not decide it: a worker whose review failed reports
/// that in an `Ok` output, while a killed one comes back as an `Err`.
#[derive(Debug)]
enum AttemptEnd {
    /// The worker returned a review for the patch, or skipped the review as
    /// it was asked to.
    Finished { output: Value },
    /// The review's deadline passed and the worker was killed.
    TimedOut { timeout: ReviewTimedOut },
    /// The patchset was cancelled while the patch was reviewed. `output` is
    /// the worker's output when it returned a review before the cancel was
    /// seen: the review is no longer wanted, but the worker's history is
    /// still the full record of what the attempt did.
    Cancelled { output: Option<Value> },
    /// Any other end with no review for the patch. `output` is the worker's
    /// output when it returned one, kept for its token counts and history.
    Failed {
        reason: String,
        output: Option<Value>,
    },
}

impl AttemptEnd {
    /// Decides how a run of the worker for patch `index` ended, given
    /// whether its patchset was `cancelled` by then.
    ///
    /// A cancel decides the end whatever the run returned, because the
    /// patchset is no longer wanted and neither is its result. A review the
    /// worker did return is kept with the cancel, so that the row records the
    /// worker's own history rather than calling a finished review unfinished.
    fn decide(result: Result<Value>, index: i64, no_ai: bool, cancelled: bool) -> Self {
        match Self::classify(result, index, no_ai) {
            end if !cancelled => end,
            Self::Finished { output } => Self::Cancelled {
                output: Some(output),
            },
            Self::Cancelled { output } => Self::Cancelled { output },
            Self::TimedOut { .. } | Self::Failed { .. } => Self::Cancelled { output: None },
        }
    }

    /// Classifies what a run of the worker for patch `index` returned.
    /// `no_ai` is whether the AI review was turned off, which makes a null
    /// review the expected answer rather than a failure.
    fn classify(result: Result<Value>, index: i64, no_ai: bool) -> Self {
        let output = match result {
            Ok(output) => output,
            Err(e) => return Self::from_error(e),
        };
        match missing_review(&output, index, no_ai) {
            None => Self::Finished { output },
            Some(reason) => Self::Failed {
                reason,
                output: Some(output),
            },
        }
    }

    fn from_error(e: anyhow::Error) -> Self {
        if let Some(timeout) = e.downcast_ref::<ReviewTimedOut>() {
            return Self::TimedOut { timeout: *timeout };
        }
        if matches!(
            e.downcast_ref::<ReviewError>(),
            Some(ReviewError::Cancelled)
        ) {
            return Self::Cancelled { output: None };
        }
        Self::Failed {
            reason: format!("Tool error: {}", e),
            output: None,
        }
    }

    /// Why the stages still running when the worker stopped are stopped.
    fn settle_cause(&self) -> crate::workflow::StageStopCause {
        use crate::workflow::StageStopCause;
        match self {
            Self::TimedOut { .. } => StageStopCause::OutOfTime,
            Self::Cancelled { .. } => StageStopCause::Cancelled,
            Self::Finished { .. } | Self::Failed { .. } => StageStopCause::Interrupted,
        }
    }

    /// Whether the worker returned a review for the patch, including one
    /// returned just before a cancel was seen. Every other end gets the
    /// daemon's record of what the attempt did.
    fn has_worker_review(&self) -> bool {
        matches!(
            self,
            Self::Finished { .. } | Self::Cancelled { output: Some(_) }
        )
    }
}

/// Why the worker's `output` holds no review for patch `index`, or `None`
/// when it holds one. `no_ai` makes a null review count as one: the review
/// was skipped as asked.
fn missing_review(output: &Value, index: i64, no_ai: bool) -> Option<String> {
    let applied = output["patches"]
        .as_array()
        .and_then(|arr| arr.iter().find(|p| p["index"] == index))
        .is_some_and(|p| p["status"] == "applied");
    let error = output["error"].as_str();
    if !applied {
        return Some(
            error
                .unwrap_or("Tool failed to return patch status")
                .to_string(),
        );
    }
    if let Some(error) = error {
        return Some(error.to_string());
    }
    match output.get("review") {
        None => Some("Missing review content".to_string()),
        Some(review) if review.is_null() && !no_ai => Some("AI returned null response".to_string()),
        Some(_) => None,
    }
}

/// What follows an attempt: a final result for the patch, another attempt
/// while retries remain, or the patch marked failed with no further attempt.
enum AttemptVerdict {
    Final(PatchResult),
    /// The attempt failed in a way another attempt may get past.
    Retry,
    /// The attempt failed in a way an identical attempt would repeat, so the
    /// patch fails now whatever retries remain.
    GiveUp,
}

/// The conversation review `review_id` streamed, as a final log for a review
/// that did not finish, or `None` when it streamed nothing.
///
/// Reads the review's live log, which must happen before the row is
/// completed, since completing it deletes the streamed entries.
async fn promoted_live_log(
    db: &Database,
    review_id: i64,
    project: crate::project::ProjectId,
) -> Result<Option<String>> {
    let mut entries = db.get_review_log_entries(review_id, None).await?;
    if entries.is_empty() {
        return Ok(None);
    }
    for entry in &mut entries {
        entry.label_stage(project);
    }
    let capped = db.review_live_log_span(review_id).await?.capped;
    let log = unfinished_log(entries, capped)?;
    Ok(Some(serde_json::to_string_pretty(&log)?))
}

/// The final log of a review that did not finish, built from what it
/// streamed.
///
/// A system message heads it, so a reader does not take the conversation for
/// the whole review. The entries keep their stage: stages run concurrently, so
/// the stream interleaves their conversations, and the page tells them apart
/// by it as it does in the live view. Each entry is a finished-log message
/// plus where it sat, so the page draws it with the same renderer.
fn unfinished_log(entries: Vec<crate::api::LiveLogEntry>, capped: bool) -> Result<Value> {
    let cap_note = if capped {
        format!(", capped at {} entries", crate::db::MAX_LIVE_LOG_ENTRIES)
    } else {
        String::new()
    };
    let header = crate::ai::AiMessage {
        role: crate::ai::AiRole::System,
        content: Some(format!(
            "[The review did not finish. This is the conversation streamed while it ran{cap_note}.]"
        )),
        thought: None,
        thought_signature: None,
        tool_calls: None,
        tool_call_id: None,
    };
    let mut log = vec![serde_json::to_value(header)?];
    for entry in entries {
        log.push(serde_json::to_value(entry)?);
    }
    Ok(Value::Array(log))
}

/// Where `stage` sits in the project's pipeline, as a sort key: stages the
/// project does not know go last, by name. Stored stage lists are written in
/// this order because their readers cannot recover it: they do not know which
/// project's pipeline ran.
fn pipeline_position(project: crate::project::ProjectId, stage: &str) -> (u32, String) {
    (
        crate::workflows::stage_order(project, stage).unwrap_or(u32::MAX),
        stage.to_string(),
    )
}

/// Where one patch's review reports what it is doing, and which attempt it is.
///
/// Grouped because the review tool already takes a long list of arguments, and
/// these four only ever travel together.
struct PatchActivity {
    registry: Arc<crate::activity::ActivityRegistry>,
    /// The patch's database id, which activity keys use; the worker's own
    /// messages carry the patch index instead.
    patch_id: i64,
    /// 1-based.
    attempt: u32,
    max_attempts: u32,
}

#[cfg(test)]
impl PatchActivity {
    /// A first attempt reporting to a registry nobody reads.
    fn untracked(patch_id: i64) -> Self {
        Self {
            registry: crate::activity::ActivityRegistry::new(),
            patch_id,
            attempt: 1,
            max_attempts: 1,
        }
    }
}

/// Reports a stage's waits for a model slot or a rate limit to the activity
/// registry.
///
/// Both are invisible without this. A stage queued behind its siblings, or
/// sleeping out a rate limit in the daemon, reports "awaiting model" exactly
/// as a stage with a slow model does, and they call for different responses --
/// more concurrency, more quota, or patience.
struct StageSlotObserver {
    activity: Arc<crate::activity::ActivityRegistry>,
    project: crate::project::ProjectId,
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
            .and_then(crate::activity::stage_from_context_tag)
        else {
            return;
        };
        // Only stages the worker also reports on. The pre-screen and the
        // planner tag their model calls like any other stage, but they are the
        // patch's phase rather than stage rows, so nothing would ever advance or
        // remove a row opened for them here: it would sit reading "starting
        // (queued for a model slot)" for the rest of the patchset's run.
        if !crate::workflows::reports_stage_row(self.project, &stage) {
            return;
        }
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

    fn backing_off(&self, request: &crate::ai::AiRequest, retry_in: Duration) {
        self.mark(
            request,
            crate::activity::StageWait::RateLimited {
                retry_in_seconds: crate::activity::retry_in_seconds(retry_in),
            },
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_review_tool(
    worker_program: Option<&Path>,
    patchset_id: i64,
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
    patch: PatchActivity,
    stages: &mut crate::activity::StageTracker,
) -> Result<serde_json::Value> {
    let cmd = match worker_program {
        Some(program) => Command::new(program),
        None => default_worker_command()?,
    };
    run_review_tool_with_cmd(
        cmd,
        patchset_id,
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
        patch,
        stages,
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

/// Records the tool calls in a provider response that the worker will run.
///
/// The worker never runs the calls in a truncated response: it discards the
/// response unread and either asks the model again or ends the session. A row
/// for one of those calls would stay at zero output length and count as a call
/// in the tool usage statistics. When a retried answer repeats the same call,
/// its result fills in the newest matching row and the phantom row is left
/// behind.
async fn record_tool_calls(
    db: &Database,
    review_id: i64,
    provider: &str,
    model: &str,
    resp: &AiResponse,
) {
    if resp.truncated {
        return;
    }
    for call in resp.tool_calls.iter().flatten() {
        let usage = crate::db::ToolUsage {
            review_id,
            provider: provider.to_string(),
            model: model.to_string(),
            tool_name: call.function_name.clone(),
            arguments: Some(call.arguments.to_string()),
            output_length: 0,
        };
        // Tool usage rows are statistics only; failing to write one must not
        // fail the review.
        if let Err(e) = db.create_tool_usage(usage).await {
            warn!(
                "Failed to record tool usage for review {}: {}",
                review_id, e
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_review_tool_with_cmd(
    mut cmd: Command,
    patchset_id: i64,
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
    patch: PatchActivity,
    stages: &mut crate::activity::StageTracker,
) -> Result<serde_json::Value> {
    let project = settings.project.kind.unwrap_or_default();
    let activity = patch.registry.clone();
    // The limiter and the backoff are the only layers that know a request is
    // queued or rate limited rather than merely slow, so they are the ones
    // that say so. The worker supplies the stage, through the context tag.
    let slot_observer = Arc::new(StageSlotObserver {
        activity: activity.clone(),
        project,
        patchset_id,
        patch_id: patch.patch_id,
    });

    // Cap concurrent model calls with the shared limiter instead of taking the
    // semaphore by hand around each call. This also releases the permit as soon
    // as the call returns, so a request that is backing off no longer occupies
    // a slot while it sleeps.
    let provider: Arc<dyn AiProvider> = Arc::new(
        crate::ai::concurrency_limited_provider::ConcurrencyLimitedProvider::new(
            provider,
            llm_semaphore.clone(),
        )
        .with_observer(slot_observer.clone()),
    );
    // Always, in practice: the daemon is required to name one. Without the flag
    // a worker puts its worktree under the system temporary directory.
    if let Some(dir) = settings.review.worktree_dir.as_deref() {
        cmd.arg("--worktree-dir").arg(dir);
    }

    cmd.args([
        "--json",
        "--baseline",
        baseline,
        // The worker is a fresh process with a cleared environment, so it
        // would otherwise fall back to the default project and load the
        // kernel prompts no matter what this daemon was started for.
        "--project",
        settings.project.kind.unwrap_or_default().as_str(),
        "--ai-provider",
        match settings.ai.provider.as_str() {
            "claude" | "stdio-claude" | "claude-cli" | "codex-cli" | "copilot-cli" | "kiro-cli"
            | "goose" | "goose-cli" => "stdio-claude",
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

    if let Some(effort) = settings.review.effort {
        cmd.arg("--effort").arg(effort.as_str());
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
    use tokio::time::{timeout, timeout_at};

    // Perform interaction with timeout. There is no salvage window yet, so
    // the worker is stopped at the soft limit, which is also the hard one.
    let deadline = Arc::new(ReviewDeadline::new(
        Duration::from_secs(settings.review.timeout_seconds),
        Duration::ZERO,
    ));

    // Retry rate-limited and transient failures with the shared limiter rather
    // than an open-coded loop. A review is bounded by its activity deadline
    // rather than an attempt count, and time spent waiting out a rate limit is
    // credited back so it does not consume that budget.
    let provider: Arc<dyn AiProvider> = Arc::new(
        crate::ai::backoff_provider::BackoffProvider::new(
            provider,
            quota_manager.clone(),
            Some(Arc::new(crate::ai::backoff_provider::DeadlineBudget::new(
                deadline.clone(),
            ))),
        )
        .with_observer(slot_observer),
    );

    // The conversation starts over with this worker, and so does its live log.
    // A review reset to Pending by a restart reuses its row, so the old run's
    // entries are cleared and this run numbers its own from past the last seq
    // the old run used rather than from 0. A log page left open across the
    // restart holds the old run's entries and asks only for what follows the
    // last of them; restarting at 0 would hide the new run's first entries
    // behind that cursor and splice the rest onto the old conversation. A seq
    // that only grows lets the page see the run change, through the first
    // stored seq it is served, and draw the new conversation from the start.
    let mut log_seq: i64 = match db.review_live_log_span(review_id).await {
        Ok(span) => span.next_seq,
        Err(e) => {
            warn!(
                "Failed to read the streamed log of review {}: {}",
                review_id, e
            );
            0
        }
    };
    if let Err(e) = db.delete_review_log_entries(review_id).await {
        warn!(
            "Failed to clear the streamed log of review {}: {}",
            review_id, e
        );
    }

    let mut spawned_tasks = Vec::new();
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
            let mut cancel_interval = tokio::time::interval(Duration::from_secs(2));
            cancel_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                // Read on every pass, because a credited quota-gate wait
                // moves it later while the loop runs.
                let current_deadline = deadline.soft();

                tokio::select! {
                    Some(err) = abort_rx.recv() => {
                        return Err(err);
                    }
                    _ = cancel_interval.tick() => {
                        if let Ok(Some(status)) = db.get_patchset_status(patchset_id).await
                            && status == ReviewStatus::Cancelled.as_str()
                        {
                            info!(
                                "Patchset {} was cancelled during active review; aborting worker",
                                patchset_id
                            );
                            return Err(ReviewError::Cancelled.into());
                        }
                    }
                    line_res = timeout_at(current_deadline, lines.next_line()) => {
                        let line_result = match line_res {
                            Ok(res) => res,
                            Err(_) => {
                                return Err(deadline.timed_out().into());
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
                                        if let Ok(Some(status)) = db.get_patchset_status(patchset_id).await
                                            && status == ReviewStatus::Cancelled.as_str()
                                        {
                                            info!(
                                                "Patchset {} was cancelled before AI request; aborting worker",
                                                patchset_id
                                            );
                                            return Err(ReviewError::Cancelled.into());
                                        }

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

                                                    record_tool_calls(
                                                        &db_clone,
                                                        review_id,
                                                        &settings_clone.ai.provider,
                                                        &settings_clone.ai.model,
                                                        &p,
                                                    )
                                                    .await;

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
                                    "ai_forget" => {
                                        // The worker rejected, or was cut off in, an answer this
                                        // side served. Drop it from the cache so a retry asks the
                                        // model again. Handled inline so that it is done before
                                        // anything the worker sends after it.
                                        match serde_json::from_value::<AiRequest>(json_msg["payload"].clone()) {
                                            Ok(req) => {
                                                // Under the request's log context, as for ai_request,
                                                // so the cache's log line names the review.
                                                let ctx_tag = req.context_tag.clone().unwrap_or_default();
                                                crate::ai::LOG_CONTEXT.scope(ctx_tag, provider.forget(&req)).await
                                            }
                                            Err(e) => tracing::warn!("Ignoring malformed ai_forget: {}", e),
                                        }
                                    }
                                    // Stored as soon as it arrives, not with the
                                    // final result: a review that times out, fails
                                    // or is cancelled is killed before it writes
                                    // one, and its row must still say which
                                    // reference trees it had. Awaited inline so the
                                    // write lands before the final line is handled.
                                    // A bad hand-off never stops the review.
                                    crate::references::REFERENCE_REPOS_MESSAGE => {
                                        match crate::references::parse_handoff(&json_msg) {
                                            Some((idx, rec)) if Some(idx) == review_index => {
                                                let stored = match serde_json::to_string(&rec) {
                                                    Ok(text) => db.set_review_reference_repos(review_id, &text).await,
                                                    Err(e) => Err(e.into()),
                                                };
                                                if let Err(e) = stored {
                                                    warn!("Failed to store reference repositories for review {}: {}", review_id, e);
                                                }
                                            }
                                            Some((idx, _)) => {
                                                warn!(
                                                    "Ignoring reference repositories for patch {} from the worker of review {} (patch {:?})",
                                                    idx, review_id, review_index
                                                );
                                            }
                                            None => {
                                                warn!("Ignoring a malformed reference repositories message from the worker of review {}", review_id);
                                            }
                                        }
                                    }
                                    // What the review is doing, stage by stage.
                                    // Advisory: a line that does not decode, or
                                    // belongs to another patch, is dropped and
                                    // never disturbs the review.
                                    crate::activity::PROGRESS_MESSAGE => {
                                        match crate::activity::parse_progress(&json_msg) {
                                            // One message of the conversation,
                                            // for the review's live log. Never
                                            // activity: it does not reach the
                                            // stage tracker or the registry.
                                            // Awaited inline so entries are
                                            // stored in the order they came.
                                            Some((idx, crate::activity::ProgressWire::LogEntry { stage, message }))
                                                if Some(idx) == review_index =>
                                            {
                                                let stored = match (
                                                    serde_json::to_string(&message),
                                                    serde_json::to_value(&message.role),
                                                ) {
                                                    (Ok(text), Ok(serde_json::Value::String(role))) => {
                                                        db.append_review_log_entry(
                                                            review_id,
                                                            log_seq,
                                                            stage.as_deref(),
                                                            &role,
                                                            &text,
                                                        )
                                                        .await
                                                    }
                                                    _ => Err(anyhow::anyhow!("message does not serialise")),
                                                };
                                                if let Err(e) = stored {
                                                    tracing::debug!(
                                                        "Failed to store live log entry {} of review {}: {}",
                                                        log_seq, review_id, e
                                                    );
                                                }
                                                log_seq += 1;
                                            }
                                            Some((idx, wire)) if Some(idx) == review_index => {
                                                if let Some(update) = crate::activity::progress_update(
                                                    project,
                                                    patchset_id,
                                                    patch.patch_id,
                                                    &wire,
                                                    patch.attempt,
                                                    patch.max_attempts,
                                                ) {
                                                    stages.apply(update);
                                                }
                                            }
                                            Some((idx, _)) => {
                                                warn!(
                                                    "Ignoring progress for patch {} from the worker of review {} (patch {:?})",
                                                    idx, review_id, review_index
                                                );
                                            }
                                            None => {
                                                tracing::debug!("Ignoring a malformed progress message from the worker of review {}", review_id);
                                            }
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

    // Recorded for every attempt however it ended. Each attempt has its own
    // review row, so the timings of attempts never mix, and on a row that
    // failed or timed out they show which stage used up the time.
    let mut timings = stages.timings();
    if !timings.is_empty() {
        timings.sort_by_cached_key(|t| pipeline_position(project, &t.stage));
        match serde_json::to_string(&timings) {
            Ok(json) => {
                let _ = db.set_review_stage_durations(review_id, &json).await;
            }
            Err(e) => warn!(
                "Failed to encode stage timings for review {}: {}",
                review_id, e
            ),
        }
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

    let timed_out = match &interaction_result {
        Err(e) => e.downcast_ref::<ReviewTimedOut>().copied(),
        Ok(_) => None,
    };
    let cancelled = match &interaction_result {
        Err(e) => e
            .downcast_ref::<ReviewError>()
            .is_some_and(|re| matches!(re, ReviewError::Cancelled)),
        Ok(_) => false,
    };

    if let Some(timed_out) = timed_out {
        error!(
            "Review tool timed out after {} active seconds. Killing process.",
            timed_out.active_seconds
        );
        let _ = child.start_kill();
        let _ = timeout(Duration::from_secs(5), child.wait()).await;
    } else if cancelled {
        info!(
            "Review tool aborted because patchset {} was cancelled. Killing process.",
            patchset_id
        );
        let _ = child.start_kill();
        let _ = timeout(Duration::from_secs(5), child.wait()).await;
    } else {
        let _ = child.wait().await; // Reap zombie
    }

    // The patch's own entry describes work in flight, and this patch has none
    // left however it ended. Leaving it meant a reviewed patch still read
    // "running review stages" until the last of its siblings finished. Its
    // stage rows stay: they are the record of what ran, and the patchset's
    // guard clears them when the patchset is done.
    activity.clear(&crate::activity::ActivityKey::PatchsetPatch {
        patchset_id,
        patch_id: patch.patch_id,
    });

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
            store_final_reference_repos(&db, review_id, review_index, &json).await;
            Ok(json)
        }
        Err(e) => Err(e),
    }
}

/// Replaces the review's early reference record with the worker's final one,
/// which adds how many reads each reference served.
///
/// Without a final record the early one is kept, read counts and all missing,
/// which the UI shows as not reported rather than as zero.
async fn store_final_reference_repos(
    db: &Database,
    review_id: i64,
    review_index: Option<i64>,
    json: &Value,
) {
    let Some(raw) = json["patches"].as_array().and_then(|patches| {
        patches
            .iter()
            .find(|p| review_index.is_some() && p["index"].as_i64() == review_index)
            .and_then(|p| p.get("reference_repos"))
    }) else {
        return;
    };
    let Some(rec) = crate::references::parse_record(raw) else {
        warn!(
            "Ignoring malformed reference repositories in the result of review {}",
            review_id
        );
        return;
    };
    let stored = match serde_json::to_string(&rec) {
        Ok(text) => db.set_review_reference_repos(review_id, &text).await,
        Err(e) => Err(e.into()),
    };
    if let Err(e) = stored {
        warn!(
            "Failed to store reference repositories for review {}: {}",
            review_id, e
        );
    }
}

impl Reviewer {
    #[allow(clippy::too_many_arguments)]
    async fn queue_notifications(
        ctx: &ReviewContext,
        patchset_id: i64,
        _review_id: Option<i64>,
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

        let mut new_findings = Vec::new();
        if let Some(findings_arr) = findings {
            for f in findings_arr {
                let preexisting = f
                    .get("preexisting")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if !preexisting {
                    new_findings.push(f.clone());
                }
            }
        }

        let total_issues_count = new_findings.len();

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

        let policy = EmailPolicyConfig::load_async(&ctx.settings.review.email_policy_path)
            .await
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

        let sashiko_bare = crate::email_policy::extract_bare_email(&sender_address);
        let author_bare = crate::email_policy::extract_bare_email(&patch_author);
        if EmailRouter::is_ignored_author(&policy, &patch_author)
            || (!sashiko_bare.is_empty()
                && !author_bare.is_empty()
                && (author_bare == sashiko_bare
                    || patch_author.to_ascii_lowercase().contains(&sashiko_bare)))
        {
            info!(
                "Bot loop guard muted notifications for patch {}/{} (ID: {}) from {}",
                patchset_id, index, patch_id, patch_author
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
                    "Muted by bot loop guard",
                )
                .await?;
            return Ok(());
        }

        let domain = if ctx.settings.project.domain.is_empty() {
            "sashiko.dev"
        } else {
            ctx.settings.project.domain.as_str()
        };
        let target_url = format!(
            "https://{}/#/patchset/{}?part={}",
            domain, patchset_msg_id_clean, index
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

        if total_issues_count == 0 {
            let mut sent_positive_review = false;
            if let Some((pos_to, pos_cc)) = EmailRouter::resolve_positive_recipients(
                &policy,
                &to_list,
                &cc_list,
                &patch_author,
                &sender_address,
            ) {
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
                    let to_json =
                        serde_json::to_string(&pos_to).unwrap_or_else(|_| "[]".to_string());
                    let cc_json =
                        serde_json::to_string(&pos_cc).unwrap_or_else(|_| "[]".to_string());
                    let subject_prefix = if patch_subject.to_lowercase().starts_with("re:") {
                        ""
                    } else {
                        "Re: "
                    };
                    let final_subject = format!("{}{}", subject_prefix, patch_subject);
                    let final_body = format!(
                        "{}\nSashiko has reviewed this patch and found no issues.\n\n-- \nSashiko AI review · {}\n",
                        body_head, target_url
                    );

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

                if total_issues_count > 0 {
                    header.push_str(&format!(
                        "Thank you for your contribution! Sashiko AI review found {} potential issue(s) to consider:\n",
                        total_issues_count
                    ));

                    let mut sorted_findings = new_findings.clone();
                    let sort_by_severity = |a: &Value, b: &Value| {
                        let sev_a = Severity::from_str(
                            a.get("severity").and_then(|v| v.as_str()).unwrap_or("Low"),
                        );
                        let sev_b = Severity::from_str(
                            b.get("severity").and_then(|v| v.as_str()).unwrap_or("Low"),
                        );
                        sev_b.cmp(&sev_a)
                    };

                    sorted_findings.sort_by(sort_by_severity);

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

                    for f in &sorted_findings {
                        header.push_str(&format_finding(f));
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

    #[test]
    fn interaction_ids_are_unique_with_the_same_timestamp() {
        let ids: HashSet<_> = (0..1_000)
            .map(|_| generate_interaction_id_at(1_234))
            .collect();

        assert_eq!(ids.len(), 1_000);
        assert!(ids.iter().all(|id| id.starts_with("rev_1234_")));
    }

    fn run_git(repo: &Path, args: &[&str]) -> Result<String> {
        let output = crate::git_cmd::in_dir(repo).args(args).output()?;
        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    #[test]
    fn git_am_message_formats_epoch_timestamp() -> Result<()> {
        let message = git_am_message("Author", 0, "Subject", "diff")?;

        assert!(message.contains("Date: "));
        assert!(!message.contains("Date: \n"));
        Ok(())
    }

    #[test]
    fn git_am_message_rejects_out_of_range_timestamp() {
        let error = git_am_message("Author", i64::MAX, "Subject", "diff")
            .expect_err("out-of-range timestamp should fail");

        assert!(
            error
                .to_string()
                .contains("outside chrono's supported range")
        );
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

    #[tokio::test]
    async fn test_prepare_baseline_applies_patch_id_and_series_prerequisites() -> Result<()> {
        let temp_dir = tempdir()?;
        let repo = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo)?;
        run_git(&repo, &["init", "-q"])?;
        run_git(&repo, &["config", "user.name", "Test Author"])?;
        run_git(&repo, &["config", "user.email", "author@example.com"])?;

        let file = repo.join("value.txt");
        std::fs::write(&file, "one\n")?;
        run_git(&repo, &["add", "value.txt"])?;
        run_git(&repo, &["commit", "-q", "-m", "base"])?;
        let base_sha = run_git(&repo, &["rev-parse", "HEAD"])?;

        std::fs::write(&file, "one\ntwo\n")?;
        run_git(&repo, &["commit", "-q", "-am", "prerequisite"])?;
        let prerequisite_diff = run_git(&repo, &["show", "--format=", "--patch", "HEAD"])?;
        let prerequisite_id = crate::prerequisites::calculate_git_patch_id(&prerequisite_diff)
            .await?
            .expect("prerequisite diff should have a stable patch ID");

        std::fs::write(&file, "one\ntwo changed\n")?;
        run_git(&repo, &["commit", "-q", "-am", "target"])?;
        let target_diff = run_git(&repo, &["show", "--format=", "--patch", "HEAD"])?;
        run_git(&repo, &["reset", "--hard", &base_sha])?;

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.git.repository_path = repo.to_string_lossy().into_owned();
        settings.review.worktree_dir = Some(
            temp_dir
                .path()
                .join("worktrees")
                .to_string_lossy()
                .into_owned(),
        );

        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;
        let thread_id = db.create_thread("root", "prerequisite", 1).await?;
        db.create_message(
            "prerequisite@example.com",
            thread_id,
            None,
            "Test Author <author@example.com>",
            "[PATCH] prerequisite",
            1_700_000_000,
            &prerequisite_diff,
            "",
            "",
            None,
            None,
        )
        .await?;
        let prerequisite_patchset_id = db
            .create_patchset(
                thread_id,
                Some("prerequisite@example.com"),
                "prerequisite@example.com",
                "[PATCH] prerequisite",
                "Test Author <author@example.com>",
                1_700_000_000,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await?
            .unwrap();
        db.create_patch_with_git_patch_id(
            prerequisite_patchset_id,
            "prerequisite@example.com",
            1,
            &prerequisite_diff,
            Some(&prerequisite_id),
        )
        .await?;

        let ctx = ReviewContext {
            semaphore: Arc::new(Semaphore::new(1)),
            llm_semaphore: Arc::new(Semaphore::new(1)),
            db: db.clone(),
            settings: settings.clone(),
            baseline_registry: Arc::new(BaselineRegistry::new(&repo, None)?),
            quota_manager: Arc::new(QuotaManager::new()),
            target_review_count: 1,
            provider: Arc::new(MockProvider),
            activity: crate::activity::ActivityRegistry::new(),
            worker_program: None,
        };
        let candidate = BaselineResolution::Commit(CommitId::parse(&base_sha).unwrap());
        let diffs = vec![(
            99,
            1,
            target_diff,
            "[PATCH] target".to_string(),
            "Test Author <author@example.com>".to_string(),
            1_700_000_001,
            "target@example.com".to_string(),
        )];

        let prerequisite_metadata = format!("prerequisite-patch-id: {prerequisite_id}");
        for metadata in [
            prerequisite_metadata.clone(),
            "Based-on: prerequisite@example.com".to_string(),
            "prerequisite-message-id: prerequisite@example.com".to_string(),
            "prerequisite-message-id: prerequisite@example.com\nBased-on: target@example.com"
                .to_string(),
            format!("{prerequisite_metadata}\nBased-on: prerequisite@example.com"),
            format!("{prerequisite_metadata}\nBased-on: target@example.com"),
            format!("{prerequisite_metadata}\nBased-on: malformed"),
            format!(
                "{prerequisite_metadata}\nprerequisite-message-id: target@example.com\nBased-on: target@example.com"
            ),
        ] {
            let (found, patch_commits, logs) = Reviewer::prepare_baseline_worktree(
                &ctx,
                99,
                std::slice::from_ref(&candidate),
                &diffs,
                Some("target@example.com"),
                Some(&metadata),
            )
            .await;
            let (_, worktree, review_baseline) = found.ok_or_else(|| {
                anyhow::anyhow!("baseline preparation failed for {metadata:?}: {logs}")
            })?;

            assert_eq!(
                run_git(
                    &worktree.path,
                    &["rev-parse", &format!("{review_baseline}^")]
                )?,
                base_sha
            );
            assert_eq!(patch_commits.len(), 1);
            let target_sha = patch_commits
                .get(&1)
                .expect("target patch commit should be recorded");
            assert_eq!(
                run_git(&worktree.path, &["rev-parse", &format!("{target_sha}^")])?,
                review_baseline
            );
            assert_eq!(
                std::fs::read_to_string(worktree.path.join("value.txt"))?,
                "one\ntwo changed\n"
            );
            assert!(logs.contains("Applied prerequisite"));
            worktree.remove().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_prepare_baseline_fetches_base_commit_from_maintainer_tree() -> Result<()> {
        let temp_dir = tempdir()?;

        // A maintainer tree holding the base-commit the series was built on.
        let maintainer = temp_dir.path().join("maintainer");
        std::fs::create_dir_all(&maintainer)?;
        run_git(&maintainer, &["init", "-q"])?;
        run_git(&maintainer, &["config", "user.name", "Test Author"])?;
        run_git(&maintainer, &["config", "user.email", "author@example.com"])?;
        let file = maintainer.join("value.txt");
        std::fs::write(&file, "one\n")?;
        run_git(&maintainer, &["add", "value.txt"])?;
        run_git(&maintainer, &["commit", "-q", "-m", "maintainer base"])?;
        let base_sha = run_git(&maintainer, &["rev-parse", "HEAD"])?;
        std::fs::write(&file, "one\ntwo\n")?;
        run_git(&maintainer, &["commit", "-q", "-am", "target"])?;
        let target_diff = run_git(&maintainer, &["show", "--format=", "--patch", "HEAD"])?;
        run_git(&maintainer, &["reset", "--hard", "-q", &base_sha])?;

        // The tree has moved on since, so its tip is not the base-commit:
        // reviewing the series there would not be what the maintainer built.
        std::fs::write(maintainer.join("later.txt"), "later work\n")?;
        run_git(&maintainer, &["add", "later.txt"])?;
        run_git(&maintainer, &["commit", "-q", "-m", "later work"])?;

        // The review repository knows nothing of that tree yet.
        let repo = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo)?;
        run_git(&repo, &["init", "-q"])?;
        run_git(&repo, &["config", "user.name", "Test Author"])?;
        run_git(&repo, &["config", "user.email", "author@example.com"])?;
        std::fs::write(repo.join("unrelated.txt"), "mainline\n")?;
        run_git(&repo, &["add", "unrelated.txt"])?;
        run_git(&repo, &["commit", "-q", "-m", "mainline"])?;
        assert!(get_commit_hash(&repo, &base_sha).await.is_err());

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.git.repository_path = repo.to_string_lossy().into_owned();
        settings.review.worktree_dir = Some(
            temp_dir
                .path()
                .join("worktrees")
                .to_string_lossy()
                .into_owned(),
        );

        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;
        let ctx = ReviewContext {
            semaphore: Arc::new(Semaphore::new(1)),
            llm_semaphore: Arc::new(Semaphore::new(1)),
            db: db.clone(),
            settings: settings.clone(),
            baseline_registry: Arc::new(BaselineRegistry::new(&repo, None)?),
            quota_manager: Arc::new(QuotaManager::new()),
            target_review_count: 1,
            provider: Arc::new(MockProvider),
            activity: crate::activity::ActivityRegistry::new(),
            worker_program: None,
        };

        // A decoy tree that is unreachable, and one that shares the local
        // remote name of the real tree: neither may hide the base-commit.
        let decoy = temp_dir.path().join("decoy");
        std::fs::create_dir_all(&decoy)?;
        run_git(&decoy, &["init", "-q"])?;

        // The subsystem heuristic offers the maintainer trees alongside the
        // explicit base-commit taken from the series.
        let candidates = vec![
            BaselineResolution::Commit(CommitId::parse(&base_sha).unwrap()),
            BaselineResolution::RemoteTarget {
                url: temp_dir.path().join("gone").to_string_lossy().into_owned(),
                name: "gone".to_string(),
                branch: None,
            },
            BaselineResolution::RemoteTarget {
                url: decoy.to_string_lossy().into_owned(),
                name: "linux".to_string(),
                branch: None,
            },
            BaselineResolution::RemoteTarget {
                url: maintainer.to_string_lossy().into_owned(),
                name: "linux".to_string(),
                branch: Some("main".to_string()),
            },
        ];
        let diffs = vec![(
            99,
            1,
            target_diff,
            "[PATCH] target".to_string(),
            "Test Author <author@example.com>".to_string(),
            1_700_000_001,
            "target@example.com".to_string(),
        )];

        let (found, _patch_commits, logs) =
            Reviewer::prepare_baseline_worktree(&ctx, 99, &candidates, &diffs, None, None).await;
        let (_, worktree, review_baseline) =
            found.ok_or_else(|| anyhow::anyhow!("baseline preparation failed: {logs}"))?;

        // The series is reviewed on the exact base the maintainer used,
        // not on the tip of the tree it came from.
        assert_eq!(review_baseline, base_sha);
        assert!(!worktree.path.join("later.txt").exists());
        assert_eq!(
            std::fs::read_to_string(worktree.path.join("value.txt"))?,
            "one\ntwo\n"
        );
        worktree.remove().await?;
        Ok(())
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
        run_review_tool_mock(mock_script, provider, 5).await?.0
    }

    /// Runs `run_review_tool_with_cmd` against a mock worker script for patch
    /// 1 of a fresh patchset, returning its result along with the database
    /// and the review row, so a test can look at what was stored even when
    /// the review failed.
    async fn run_review_tool_mock(
        mock_script: &str,
        provider: Arc<dyn AiProvider>,
        timeout_seconds: u64,
    ) -> Result<(Result<Value>, Arc<Database>, i64)> {
        let run = run_review_tool_mock_reporting(
            mock_script,
            provider,
            timeout_seconds,
            crate::activity::ActivityRegistry::new(),
        )
        .await?;
        Ok((run.result, run.db, run.review_id))
    }

    /// One mock review's outcome, with the ids its activity is keyed by.
    struct MockRun {
        result: Result<Value>,
        db: Arc<Database>,
        review_id: i64,
        patchset_id: i64,
        patch_id: i64,
        /// What the daemon saw of the worker's stages.
        stages: crate::activity::StageTracker,
    }

    /// As [`run_review_tool_mock`], reporting into `activity`.
    async fn run_review_tool_mock_reporting(
        mock_script: &str,
        provider: Arc<dyn AiProvider>,
        timeout_seconds: u64,
        activity: Arc<crate::activity::ActivityRegistry>,
    ) -> Result<MockRun> {
        run_review_tool_mock_after(mock_script, provider, timeout_seconds, activity, None).await
    }

    /// As [`run_review_tool_mock_reporting`], with `stale_log` already stored
    /// as the review's streamed entry 0, as an earlier run of the same review
    /// row would have left it.
    async fn run_review_tool_mock_after(
        mock_script: &str,
        provider: Arc<dyn AiProvider>,
        timeout_seconds: u64,
        activity: Arc<crate::activity::ActivityRegistry>,
        stale_log: Option<&str>,
    ) -> Result<MockRun> {
        let temp_dir = tempdir()?;
        let bin_path = temp_dir.path().join("mock_review");

        crate::test_support::write_executable_script(&bin_path, mock_script);

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.review.timeout_seconds = timeout_seconds;

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
        if let Some(content) = stale_log {
            let message = json!({"role": "user", "content": content}).to_string();
            db.append_review_log_entry(review_id, 0, None, "user", &message)
                .await?;
        }

        let mut stages = crate::activity::StageTracker::new(activity.clone());
        let result = run_review_tool_with_cmd(
            Command::new(&bin_path),
            ps_id,
            &json!({}),
            &settings,
            db.clone(),
            "HEAD",
            Some(1),
            None,
            quota_manager,
            review_id,
            None,
            provider,
            Arc::new(Semaphore::new(56)),
            PatchActivity {
                registry: activity,
                patch_id: p_id,
                attempt: 1,
                max_attempts: 1,
            },
            &mut stages,
        )
        .await;
        Ok(MockRun {
            result,
            db,
            review_id,
            patchset_id: ps_id,
            patch_id: p_id,
            stages,
        })
    }

    /// A worker's progress line for patch `index`, as the worker writes it.
    fn progress(index: i64, wire: crate::activity::ProgressWire) -> String {
        crate::activity::progress_line(index, &wire).unwrap()
    }

    /// A mock worker that prints `lines` after reading its input, then a final
    /// result for patch 1.
    fn worker_printing(lines: &[String]) -> String {
        let mut script = String::from("#!/bin/sh\nread -r _payload\n");
        for line in lines {
            script.push_str(&format!("echo '{}'\n", line));
        }
        script.push_str(
            "echo '{\"patchset_id\": 1, \"patches\": [{\"index\": 1, \"status\": \"applied\"}]}'\n",
        );
        script
    }

    /// The whole path from a worker's stdout to the registry and the review
    /// row, through the real daemon loop. A clean build is no evidence this
    /// works: decode the stage wrongly, or miss the message type, and every
    /// update is silently dropped.
    #[tokio::test]
    async fn test_worker_progress_drives_live_activity_and_stage_timings() -> Result<()> {
        use crate::activity::{ActivityKey, ActivityRegistry, Phase, ProgressWire};

        let goal = || "goal".to_string();
        let lines = vec![
            progress(
                1,
                ProgressWire::PlanReady {
                    planned_stages: vec![goal()],
                },
            ),
            progress(1, ProgressWire::StageStarted { stage: goal() }),
            progress(
                1,
                ProgressWire::StageTurn {
                    stage: goal(),
                    turn: 1,
                    max_turns: 20,
                },
            ),
            progress(
                1,
                ProgressWire::StageTurn {
                    stage: goal(),
                    turn: 2,
                    max_turns: 20,
                },
            ),
            progress(1, ProgressWire::StageFinished { stage: goal() }),
            // Another patch's progress is not this review's to report.
            progress(
                2,
                ProgressWire::StageStarted {
                    stage: "locking".to_string(),
                },
            ),
            // A numbered stage is not a stage; the line is dropped.
            r#"{"type":"progress","patch_index":1,"payload":{"kind":"stage_started","stage":5}}"#
                .to_string(),
        ];
        let activity = ActivityRegistry::new();
        let run = run_review_tool_mock_reporting(
            &worker_printing(&lines),
            Arc::new(MockProvider),
            30,
            activity.clone(),
        )
        .await?;
        run.result.expect("the review completes");

        let goal_key = ActivityKey::PatchsetStage {
            patchset_id: run.patchset_id,
            patch_id: run.patch_id,
            stage: goal(),
        };
        match activity.get(&goal_key).map(|e| e.phase) {
            Some(Phase::StageDone { stage, turns, .. }) => {
                assert_eq!(stage, "goal");
                assert_eq!(turns, 2, "the highest turn reached, not the event count");
            }
            other => panic!("the finished stage must settle as done, got {other:?}"),
        }
        assert_eq!(
            activity.patchset_snapshot(run.patchset_id).len(),
            1,
            "only the finished stage remains: no row for the other patch or the \
             numbered stage, and the patch's own entry goes when its review ends"
        );
        assert!(
            activity
                .get(&ActivityKey::PatchsetPatch {
                    patchset_id: run.patchset_id,
                    patch_id: run.patch_id,
                })
                .is_none()
        );

        let review = run.db.get_review_details(run.review_id).await?.unwrap();
        assert_eq!(
            review["stage_durations"],
            json!([{"stage": "goal", "seconds": 0, "turns": 2}])
        );
        Ok(())
    }

    /// The worker's conversation reaches the review's live log, in order,
    /// with each stage stored as its name -- and never the activity registry.
    #[tokio::test]
    async fn test_worker_log_entries_reach_the_live_log_and_nothing_else() -> Result<()> {
        use crate::activity::{ActivityRegistry, ProgressWire};
        use crate::ai::{AiMessage, AiRole, ToolCall};

        let message = |role: AiRole, content: &str| AiMessage {
            role,
            content: Some(content.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        };
        let mut call = message(AiRole::Assistant, "looking");
        call.tool_calls = Some(vec![ToolCall {
            id: "c0".to_string(),
            function_name: "git_grep".to_string(),
            arguments: json!({"pattern": "spin_lock"}),
            thought_signature: None,
        }]);
        let lines = vec![
            progress(
                1,
                ProgressWire::LogEntry {
                    stage: None,
                    message: message(AiRole::System, "system prompt"),
                },
            ),
            progress(
                1,
                ProgressWire::LogEntry {
                    stage: Some("planning".to_string()),
                    message: message(AiRole::User, "plan this"),
                },
            ),
            // Another patch's conversation is not this review's.
            progress(
                2,
                ProgressWire::LogEntry {
                    stage: Some("goal".to_string()),
                    message: message(AiRole::User, "other patch"),
                },
            ),
            progress(
                1,
                ProgressWire::LogEntry {
                    stage: Some("goal".to_string()),
                    message: call.clone(),
                },
            ),
            // A numbered stage is not a stage; the line is dropped.
            r#"{"type":"progress","patch_index":1,"payload":{"kind":"log_entry","stage":3,"message":{"role":"user","content":"numbered"}}}"#
                .to_string(),
        ];
        let activity = ActivityRegistry::new();
        let run = run_review_tool_mock_after(
            &worker_printing(&lines),
            Arc::new(MockProvider),
            30,
            activity.clone(),
            Some("left by an earlier run"),
        )
        .await?;
        run.result.expect("the review completes");

        let entries = run.db.get_review_log_entries(run.review_id, None).await?;
        let seen: Vec<(i64, Option<&str>, Option<&str>)> = entries
            .iter()
            .map(|e| (e.seq, e.stage.as_deref(), e.message.content.as_deref()))
            .collect();
        assert_eq!(
            seen,
            [
                (1, None, Some("system prompt")),
                (2, Some("planning"), Some("plan this")),
                (3, Some("goal"), Some("looking")),
            ],
            "the earlier run's entry is gone and this run starts past its seq"
        );
        assert_eq!(entries[2].message, call, "the message is stored whole");
        assert_eq!(entries[0].message.role, AiRole::System);

        let mut rows = run
            .db
            .conn
            .query(
                "SELECT DISTINCT typeof(stage), role FROM review_log_entries
                 WHERE stage IS NOT NULL ORDER BY role",
                (),
            )
            .await?;
        let mut kinds = Vec::new();
        while let Some(row) = rows.next().await? {
            kinds.push((row.get::<String>(0)?, row.get::<String>(1)?));
        }
        assert_eq!(
            kinds,
            [
                ("text".to_string(), "assistant".to_string()),
                ("text".to_string(), "user".to_string())
            ],
            "stages are names and roles are serde's lowercase strings"
        );

        assert!(
            activity.patchset_snapshot(run.patchset_id).is_empty(),
            "conversation is never activity"
        );
        Ok(())
    }

    /// A stage that fails says so, instead of sitting on its last turn.
    #[tokio::test]
    async fn test_a_failed_stage_from_the_worker_reads_as_failed() -> Result<()> {
        use crate::activity::{ActivityKey, ActivityRegistry, Phase, ProgressWire};

        let stage = || "implementation".to_string();
        let lines = vec![
            progress(1, ProgressWire::StageStarted { stage: stage() }),
            progress(
                1,
                ProgressWire::StageTurn {
                    stage: stage(),
                    turn: 20,
                    max_turns: 20,
                },
            ),
            progress(
                1,
                ProgressWire::StageFailed {
                    stage: stage(),
                    reason: "Failed to generate valid response".to_string(),
                    cause: crate::workflow::StageStopCause::Error,
                },
            ),
        ];
        let activity = ActivityRegistry::new();
        let run = run_review_tool_mock_reporting(
            &worker_printing(&lines),
            Arc::new(MockProvider),
            30,
            activity.clone(),
        )
        .await?;
        run.result.expect("the review completes without the stage");

        let entry = activity
            .get(&ActivityKey::PatchsetStage {
                patchset_id: run.patchset_id,
                patch_id: run.patch_id,
                stage: stage(),
            })
            .expect("a failed stage keeps its row");
        assert!(
            matches!(entry.phase, Phase::StageFailed { .. }),
            "{entry:?}"
        );
        assert_eq!(
            entry.description,
            "stage implementation failed: Failed to generate valid response"
        );

        // Nothing finished, so there is no breakdown to record.
        let review = run.db.get_review_details(run.review_id).await?.unwrap();
        assert!(review["stage_durations"].is_null(), "{review}");

        // The daemon knows the stage stopped, should the worker stop later.
        assert_eq!(
            run.stages.stopped(),
            [crate::activity::StageFailureRecord {
                stage: stage(),
                reason: "Failed to generate valid response".to_string(),
                cause: crate::workflow::StageStopCause::Error,
            }]
        );
        Ok(())
    }

    /// The observer may only open rows for stages the worker will close.
    ///
    /// The pre-screen and the planner tag their model calls like any other
    /// stage, so the queue observer would create a row for each that nothing
    /// ever advanced or removed, because the worker reports them as the patch's
    /// phase instead.
    #[test]
    fn a_queue_wait_opens_no_row_for_a_stage_the_worker_never_reports() {
        use crate::activity::{ActivityRegistry, StageWait};
        use crate::project::ProjectId;

        for (project, real_stage) in [
            (ProjectId::Linux, "locking"),
            (ProjectId::Sashiko, "llm-pipeline"),
        ] {
            let activity = ActivityRegistry::new();
            let observer = StageSlotObserver {
                activity: activity.clone(),
                project,
                patchset_id: 5,
                patch_id: 19,
            };
            let request = |stage: &str| crate::ai::AiRequest {
                system: None,
                messages: vec![],
                tools: None,
                temperature: None,
                response_format: None,
                context_tag: Some(format!("[ps:5 p:19] [s:{}] ", stage)),
                reasoning_effort: None,
            };

            for uncounted in ["planning", "pre-screen"] {
                observer.mark(&request(uncounted), StageWait::Queued);
            }
            // A request with no stage in its tag is not attributed to any.
            observer.mark(
                &crate::ai::AiRequest {
                    context_tag: Some("[ps:5 p:19] ".to_string()),
                    ..request("x")
                },
                StageWait::Queued,
            );
            assert!(
                activity.patchset_snapshot(5).is_empty(),
                "a stage the worker never reports on must get no row at all"
            );

            // A real stage still does: this is the only side that can tell a
            // queue wait from a slow model.
            observer.mark(&request(real_stage), StageWait::Queued);
            let entries = activity.patchset_snapshot(5);
            assert_eq!(entries.len(), 1, "{project:?}");
            assert_eq!(entries[0].stage.as_deref(), Some(real_stage));
            assert_eq!(
                entries[0].description,
                format!("stage {real_stage}, starting (queued for a model slot)")
            );
        }
    }

    /// A backoff in the daemon reads as a rate limit, rounded up so a short
    /// transient wait does not claim "retrying in 0s", and its end reads as
    /// the model's time again.
    #[test]
    fn a_daemon_backoff_reads_as_rate_limited_until_it_resumes() {
        use crate::activity::{ActivityKey, ActivityRegistry};
        use crate::ai::concurrency_limited_provider::SlotObserver;

        let activity = ActivityRegistry::new();
        let observer = StageSlotObserver {
            activity: activity.clone(),
            project: crate::project::ProjectId::Linux,
            patchset_id: 5,
            patch_id: 19,
        };
        let request = crate::ai::AiRequest {
            system: None,
            messages: vec![],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: Some("[ps:5 p:19] [s:security] ".to_string()),
            reasoning_effort: None,
        };
        let key = ActivityKey::PatchsetStage {
            patchset_id: 5,
            patch_id: 19,
            stage: "security".to_string(),
        };
        activity.update_stage_turn(key.clone(), "security".to_string(), 4, 20);

        observer.backing_off(&request, Duration::from_millis(1300));
        assert_eq!(
            activity.get(&key).unwrap().description,
            "stage security, turn 4/20 (rate limited, retrying in 2s)"
        );
        observer.running(&request);
        assert_eq!(
            activity.get(&key).unwrap().description,
            "stage security, turn 4/20 (awaiting model)"
        );
    }

    /// The early reference hand-off as a worker would print it for patch
    /// `index`, with no read counts yet.
    fn kernel_handoff(index: i64) -> String {
        crate::references::handoff_line(
            index,
            &serde_json::from_value(json!({
                "attached": [{
                    "name": "kernel",
                    "kind": "kernel",
                    "revision": "v6.18",
                    "commit": "1a2b3c4d5e6f".repeat(3) + "1a2b",
                    "why": "always",
                }],
                "dormant": [{"name": "rdma-core", "applies_to": ["rdma/drv/**"]}],
            }))
            .unwrap(),
        )
        .unwrap()
    }

    /// A worker that is killed for running out of time never writes a final
    /// result, so the references it reported early are all its row will say.
    #[tokio::test]
    async fn test_reference_repos_handoff_survives_a_timeout() -> Result<()> {
        let mock_script = format!(
            "#!/bin/bash\nread -r input\necho '{}'\nsleep 30\n",
            kernel_handoff(1)
        );
        let (result, db, review_id) =
            run_review_tool_mock(&mock_script, Arc::new(MockProvider), 1).await?;
        let err = result.expect_err("the review must time out");
        assert!(err.downcast_ref::<ReviewTimedOut>().is_some(), "{err}");

        let review = db.get_review_details(review_id).await?.unwrap();
        let refs = &review["reference_repos"];
        assert_eq!(refs["attached"][0]["name"], "kernel");
        assert!(
            refs["attached"][0].get("reads").is_none(),
            "an unfinished review has no read count: {refs}"
        );
        assert_eq!(refs["dormant"][0]["name"], "rdma-core");
        Ok(())
    }

    #[tokio::test]
    async fn test_final_output_fills_in_reference_reads() -> Result<()> {
        let mut final_refs: Value =
            serde_json::from_str::<Value>(&kernel_handoff(1))?["payload"].clone();
        final_refs["attached"][0]["reads"] = json!(5);
        let final_line = json!({
            "patchset_id": 1,
            "patches": [{"index": 1, "status": "applied", "reference_repos": final_refs}],
        });
        let mock_script = format!(
            "#!/bin/bash\nread -r input\necho '{}'\necho '{}'\n",
            kernel_handoff(1),
            final_line
        );
        let (result, db, review_id) =
            run_review_tool_mock(&mock_script, Arc::new(MockProvider), 5).await?;
        result?;
        let review = db.get_review_details(review_id).await?.unwrap();
        assert_eq!(review["reference_repos"]["attached"][0]["reads"], 5);
        assert_eq!(review["reference_repos"]["dormant"][0]["name"], "rdma-core");
        Ok(())
    }

    /// A hand-off the daemon cannot trust, or one about another patch, is
    /// dropped without failing the review.
    #[tokio::test]
    async fn test_malformed_reference_handoff_is_ignored() -> Result<()> {
        let mut unknown: Value = serde_json::from_str(&kernel_handoff(1))?;
        unknown["payload"]["attached"][0]["extra"] = json!(true);
        let mock_script = format!(
            "#!/bin/bash\nread -r input\necho '{}'\necho '{}'\n\
             echo '{{\"patchset_id\":1,\"patches\":[{{\"index\":1,\"status\":\"applied\"}}]}}'\n",
            unknown,
            kernel_handoff(2)
        );
        let (result, db, review_id) =
            run_review_tool_mock(&mock_script, Arc::new(MockProvider), 5).await?;
        result?;
        let review = db.get_review_details(review_id).await?.unwrap();
        assert!(review["reference_repos"].is_null(), "{review}");
        Ok(())
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

        crate::test_support::write_executable_script(&bin_path, mock_script);

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
                                reasoning_effort: None,
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

    /// Records every request it is asked to forget.
    struct ForgetRecorder {
        forgotten: std::sync::Mutex<Vec<AiRequest>>,
    }

    #[async_trait]
    impl AiProvider for ForgetRecorder {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            unreachable!("the worker in this test only forgets")
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "forget-recorder".to_string(),
                context_window_size: 1000,
            }
        }

        async fn forget(&self, request: &AiRequest) {
            self.forgotten.lock().unwrap().push(request.clone());
        }
    }

    #[tokio::test]
    async fn test_run_review_tool_passes_a_worker_forget_to_the_provider() -> Result<()> {
        // A worker's cache is the daemon's, on the far side of the stdio
        // link, so a forget from the worker's session runner must cross it.
        // The line is built by the same function the worker uses.
        let request = AiRequest {
            system: None,
            messages: vec![crate::ai::AiMessage {
                role: crate::ai::AiRole::User,
                content: Some("hello".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
        };
        let line = serde_json::to_string(&crate::ai::forget_envelope(&request))?;
        let mock_script = format!(
            r#"#!/bin/bash
read -r input
echo '{line}'
echo '{{"patchset_id":1,"patches":[{{"index":1,"status":"done"}}]}}'
"#
        );

        let provider = Arc::new(ForgetRecorder {
            forgotten: std::sync::Mutex::new(Vec::new()),
        });
        let result = run_single_ai_request_mock(&mock_script, provider.clone()).await?;

        assert_eq!(result["patches"][0]["status"], "done");
        let forgotten = provider.forgotten.lock().unwrap();
        assert_eq!(forgotten.len(), 1, "the daemon did not pass the forget on");
        assert_eq!(forgotten[0].messages[0].content.as_deref(), Some("hello"));
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

    #[tokio::test]
    async fn test_run_review_tool_timeout_reaps_stuck_child() -> Result<()> {
        let temp_dir = tempdir()?;
        let bin_path = temp_dir.path().join("mock_review");
        let mock_script = r#"#!/bin/bash
read -r input
sleep 30
"#;

        crate::test_support::write_executable_script(&bin_path, mock_script);

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.review.timeout_seconds = 1;

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
                PatchActivity::untracked(p_id),
                &mut crate::activity::StageTracker::new(crate::activity::ActivityRegistry::new()),
            ),
        )
        .await;

        let result =
            completed.expect("run_review_tool should kill/reap a timed-out child promptly");
        // The daemon tells a timeout from other failures by its type alone.
        let err = result.expect_err("the stuck child must time out");
        assert_eq!(
            err.downcast_ref::<ReviewTimedOut>(),
            Some(&ReviewTimedOut {
                active_seconds: 1,
                salvage_seconds: 0,
            }),
            "{err}"
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

    /// Runs git in a test directory and fails the test if it fails.
    fn git_in(dir: &Path, args: &[&str]) {
        let out = crate::git_cmd::in_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {:?} failed: {:?}", args, out);
    }

    /// A review repository with two remotes: "origin", mainline, whose
    /// history is old -> tip, and "stale", whose HEAD is still at old.
    /// Between old and tip, mainline rewrites mainline.txt and leaves
    /// shared.txt alone.
    fn stale_head_fixture(root: &Path) -> PathBuf {
        let upstream = root.join("linux.git");
        std::fs::create_dir_all(&upstream).unwrap();
        git_in(&upstream, &["init", "--quiet", "--initial-branch=master"]);
        git_in(&upstream, &["config", "user.email", "test@example.com"]);
        git_in(&upstream, &["config", "user.name", "Test User"]);
        std::fs::write(upstream.join("shared.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(upstream.join("mainline.txt"), "old\n").unwrap();
        git_in(&upstream, &["add", "shared.txt", "mainline.txt"]);
        git_in(&upstream, &["commit", "--quiet", "-m", "old"]);
        git_in(&upstream, &["branch", "stale-master"]);
        std::fs::write(upstream.join("mainline.txt"), "new\n").unwrap();
        git_in(&upstream, &["commit", "--quiet", "--all", "-m", "tip"]);

        let stale = root.join("stale.git");
        let stale_str = stale.to_str().unwrap();
        git_in(
            root,
            &[
                "clone",
                "--quiet",
                "--bare",
                "--branch=stale-master",
                upstream.to_str().unwrap(),
                stale_str,
            ],
        );

        let repo = root.join("review");
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "--quiet"]);
        git_in(
            &repo,
            &["remote", "add", "origin", upstream.to_str().unwrap()],
        );
        git_in(&repo, &["remote", "add", "stale", stale_str]);
        for remote in ["origin", "stale"] {
            git_in(&repo, &["fetch", "--quiet", remote]);
            git_in(&repo, &["remote", "set-head", remote, "--auto"]);
        }
        repo
    }

    async fn stale_head_context(root: &Path, repo: &Path) -> Result<ReviewContext> {
        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.git.repository_path = repo.to_str().unwrap().to_string();
        settings.review.worktree_dir = Some(root.join("worktrees").to_str().unwrap().to_string());
        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;
        Ok(ReviewContext {
            semaphore: Arc::new(Semaphore::new(1)),
            llm_semaphore: Arc::new(Semaphore::new(1)),
            db,
            settings,
            baseline_registry: Arc::new(BaselineRegistry::new(repo, None)?),
            quota_manager: Arc::new(QuotaManager::new()),
            target_review_count: 1,
            provider: Arc::new(MockProvider),
            activity: crate::activity::ActivityRegistry::new(),
            worker_program: None,
        })
    }

    fn stale_head_candidates(repo: &Path) -> Vec<BaselineResolution> {
        let url = |name: &str| {
            repo.parent()
                .unwrap()
                .join(name)
                .to_str()
                .unwrap()
                .to_string()
        };
        vec![
            BaselineResolution::RemoteTarget {
                url: url("stale.git"),
                name: "stale".to_string(),
                branch: None,
            },
            BaselineResolution::RemoteTarget {
                url: url("linux.git"),
                name: "origin".to_string(),
                branch: Some("master".to_string()),
            },
        ]
    }

    fn one_patch(diff: &str) -> Vec<(i64, i64, String, String, String, i64, String)> {
        vec![(
            1,
            1,
            diff.to_string(),
            "[PATCH] test".to_string(),
            "Test User <test@example.com>".to_string(),
            1_000_000_000,
            "msg@example.com".to_string(),
        )]
    }

    fn attempt_statuses(logs_json: &str) -> Vec<(String, String)> {
        let logs: Vec<Value> = serde_json::from_str(logs_json).unwrap();
        logs.iter()
            .map(|a| {
                let name = a["baseline"].as_str().unwrap();
                let name = name.split(' ').next().unwrap().to_string();
                (name, a["status"].as_str().unwrap().to_string())
            })
            .collect()
    }

    async fn assert_baseline_failure_log(
        ctx: &ReviewContext,
        diff: &str,
        expected_message: &str,
    ) -> Result<()> {
        let mut patches = one_patch(diff);
        patches[0].3 =
            "[PATCH] test https://test-user:test-password@example.com/?key=stdout-secret"
                .to_string();

        let (chosen, patch_commits, logs) = Reviewer::prepare_baseline_worktree(
            ctx,
            1,
            &[BaselineResolution::LocalRef("origin/master".to_string())],
            &patches,
            None,
            None,
        )
        .await;

        assert!(chosen.is_none());
        assert!(patch_commits.is_empty());
        let attempts: Vec<Value> = serde_json::from_str(&logs)?;
        let baseline = run_git(
            Path::new(&ctx.settings.git.repository_path),
            &["rev-parse", "origin/master"],
        )?;
        assert_eq!(
            attempts,
            vec![json!({
                "baseline": format!("origin/master ({baseline})"),
                "status": "Failed",
                "log": format!(
                    "Trying baseline: origin/master ({baseline})\n\
                     Patch application error: {expected_message}\n\
                     Patch 1/1 (ID: 1) failed to apply.\n\
                     Application failed.\n"
                ),
            })]
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_prepare_baseline_stale_head_and_error_classification() -> Result<()> {
        let root = tempdir()?;
        let repo = stale_head_fixture(root.path());
        let ctx = stale_head_context(root.path(), &repo).await?;
        let candidates = stale_head_candidates(&repo);

        // 1. Classified git-am errors log only the sanitized category message.
        let missing_diff = "diff --git a/missing-token=stderr-secret.txt b/missing-token=stderr-secret.txt\n\
                            --- a/missing-token=stderr-secret.txt\n\
                            +++ b/missing-token=stderr-secret.txt\n\
                            @@ -1 +1 @@\n-old\n+new\n";
        assert_baseline_failure_log(
            &ctx,
            missing_diff,
            "A required file is missing from the index.",
        )
        .await?;

        // 2. A merged default HEAD (stale/HEAD) is deferred behind mainline (origin/master).
        let shared_diff = "diff --git a/shared.txt b/shared.txt\n\
                           --- a/shared.txt\n\
                           +++ b/shared.txt\n\
                           @@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        let (chosen, _, logs) = Reviewer::prepare_baseline_worktree(
            &ctx,
            1,
            &candidates,
            &one_patch(shared_diff),
            None,
            None,
        )
        .await;
        let (_, worktree, _) = chosen.expect("mainline should apply");
        assert_eq!(
            attempt_statuses(&logs),
            vec![
                ("stale/HEAD".to_string(), "Skipped".to_string()),
                ("origin/master".to_string(), "Applied".to_string()),
            ]
        );
        worktree.remove().await?;

        // 3. A series that applies only to the merged default HEAD is still tried last.
        let stale_only_diff = "diff --git a/mainline.txt b/mainline.txt\n\
                               --- a/mainline.txt\n\
                               +++ b/mainline.txt\n\
                               @@ -1 +1 @@\n-old\n+older\n";
        let (chosen, _, logs) = Reviewer::prepare_baseline_worktree(
            &ctx,
            1,
            &candidates,
            &one_patch(stale_only_diff),
            None,
            None,
        )
        .await;
        let (_, worktree, _) = chosen.expect("the merged HEAD should apply");
        assert_eq!(
            attempt_statuses(&logs),
            vec![
                ("stale/HEAD".to_string(), "Skipped".to_string()),
                ("origin/master".to_string(), "Failed".to_string()),
                ("stale/HEAD".to_string(), "Applied".to_string()),
            ]
        );
        let attempts: Vec<Value> = serde_json::from_str(&logs)?;
        let failed_log = attempts[1]["log"].as_str().unwrap();
        assert!(
            failed_log.contains("Patch application error: Patch context does not match.\n"),
            "{failed_log}"
        );
        assert!(!failed_log.contains("mainline.txt"));
        assert!(!failed_log.contains("git am failed"));
        let applied_log = attempts[2]["log"].as_str().unwrap();
        assert!(applied_log.ends_with("Application successful.\n"));
        assert!(!applied_log.contains("Patch application error:"));
        worktree.remove().await?;

        // 4. Unrecognized git-am hook failures do not leak private stdout/stderr.
        let hook = repo.join(".git/hooks/applypatch-msg");
        crate::test_support::write_executable_script(
            &hook,
            "#!/bin/sh\nprintf '%s\\n' 'private stdout'\n\
             printf '%s\\n' 'private stderr <script>example</script>' >&2\nexit 1\n",
        );
        let tip_diff = "diff --git a/mainline.txt b/mainline.txt\n\
                        --- a/mainline.txt\n\
                        +++ b/mainline.txt\n\
                        @@ -1 +1 @@\n-new\n+newer\n";
        assert_baseline_failure_log(&ctx, tip_diff, "Unrecognized patch application failure.").await
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
            worker_program: None,
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
read -r input || exit 1
echo '{"type": "ai_request", "payload": {"messages": [{"role": "user", "content": "first"}], "temperature": 0.5}}'
read -r ai_response || exit 1
case "$ai_response" in *'"type":"error"'*) while read -r _; do :; done; exit 1 ;; esac
echo '{"type": "ai_request", "payload": {"messages": [{"role": "user", "content": "second"}], "temperature": 0.5}}'
read -r ai_response || exit 1
case "$ai_response" in *'"type":"error"'*) while read -r _; do :; done; exit 1 ;; esac
echo '{"patchset_id": 1, "patches": [{"index": 1, "status": "applied"}]}'
"#;
        crate::test_support::write_executable_script(&bin_path, mock_script);

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
            PatchActivity::untracked(p_id),
            &mut crate::activity::StageTracker::new(crate::activity::ActivityRegistry::new()),
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
            reply_to = ["author", "list", "recipients"]
            "#,
        )?;

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.review.email_policy_path = policy_path.to_str().unwrap().to_string();
        settings.smtp = Some(crate::settings::SmtpSettings {
            transport: crate::settings::MailTransport::Smtp,
            server: Some("localhost".to_string()),
            port: Some(25),
            username: None,
            password: None,
            sendmail_path: None,
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
            worker_program: None,
        };

        // Scenario 1: Mixed findings
        let findings_mixed = vec![
            json!({
                "problem": "New High issue",
                "severity": "High",
                "preexisting": false
            }),
            json!({
                "problem": "Medium issue",
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
            None,
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
Thank you for your contribution! Sashiko AI review found 2 potential issue(s) to consider:
- [High] New High issue
- [Low] New Low issue
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
            None,
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

        let findings_only = vec![json!({
            "problem": "Medium issue",
            "severity": "Medium",
            "preexisting": true
        })];

        Reviewer::queue_notifications(
            &ctx,
            ps_id,
            None,
            p_id_3,
            "msg_id_p3",
            "msg_id_1",
            3, // index
            "inline review content 3",
            Some(&findings_only),
            "summary",
        )
        .await?;

        let mut rows = db
            .conn
            .query(
                "SELECT status, body FROM email_outbox WHERE patch_id = ?",
                libsql::params![p_id_3],
            )
            .await?;
        let row = rows.next().await?.expect("Expected email in outbox");
        let status: String = row.get(0)?;
        let body: String = row.get(1)?;
        assert_eq!(status, "Skipped");
        assert_eq!(body, "Skipped due to no findings");

        // Setup for Scenario 4: Linked newly discovered preexisting bug with slug
        let rev_id = db
            .create_review(ps_id, Some(p_id_3), "provider", "model", None, None)
            .await?;
        let bug_id = db
            .create_bug(&crate::db::NewBug {
                bugid: "linux-deadbeef".to_string(),
                title: " High UAF in cleanup".to_string(),
                lifecycle_status: crate::db::BugLifecycleStatus::New,
                pipeline_state: crate::db::BugPipelineState::Pending,
                assignee: None,
                reporter: "sashiko".to_string(),
                reported_at: 1000,
                discovered_in_patchset_id: Some(ps_id),
                discovered_in_patch_id: Some(p_id_3),
                discovered_in_commit: None,
                source_ref: None,
                vector_json: None,
                duplicate_of_id: None,
                subsystems: vec![crate::db::AttributedSubsystem::from_maintainers("net")],
            })
            .await?;
        db.add_bug_enrichment(
            bug_id,
            &crate::db::NewBugEnrichment {
                kind: "severity_calibration".to_string(),
                tool: "sashiko".to_string(),
                model: None,
                author: None,
                created_at: 1000,
                content: Some("Reasoning".to_string()),
                data_json: Some(serde_json::json!({
                    "severity": "High",
                    "severity_int": 3,
                })),
                ..Default::default()
            },
        )
        .await?;
        db.link_review_to_bug(rev_id, bug_id, true).await?;

        db.create_message(
            "msg_id_p4",
            thread_id,
            None,
            "Author <author@example.com>",
            "Subject 4",
            1000,
            "Body 4",
            "to@example.com",
            "cc@example.com",
            None,
            None,
        )
        .await?;
        let p_id_4 = db.create_patch(ps_id, "msg_id_p4", 4, "diff").await?;

        Reviewer::queue_notifications(
            &ctx,
            ps_id,
            Some(rev_id),
            p_id_4,
            "msg_id_p4",
            "msg_id_1",
            4,
            "inline review content 4",
            Some(&findings_new_only),
            "summary",
        )
        .await?;

        let mut rows = db
            .conn
            .query(
                "SELECT body FROM email_outbox WHERE patch_id = ?",
                libsql::params![p_id_4],
            )
            .await?;
        let row = rows.next().await?.expect("Expected email in outbox");
        let body: String = row.get(0)?;
        let expected_scenario_4 = "\
Thank you for your contribution! Sashiko AI review found 2 potential issue(s) to consider:
- [High] New High issue
- [Low] New Low issue
--

inline review content 4\n\n-- \nSashiko AI review · https://sashiko.dev/#/patchset/msg_id_1?part=4";
        assert_eq!(body, expected_scenario_4);

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
            reply_to = ["author", "list", "recipients"]
            "#,
        )?;

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.review.email_policy_path = policy_path.to_str().unwrap().to_string();
        settings.smtp = Some(crate::settings::SmtpSettings {
            transport: crate::settings::MailTransport::Smtp,
            server: Some("localhost".to_string()),
            port: Some(25),
            username: None,
            password: None,
            sendmail_path: None,
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
            worker_program: None,
        };

        Reviewer::queue_notifications(
            &ctx,
            ps_id,
            None,
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
            None,
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

    #[tokio::test]
    async fn test_queue_notifications_positive_review_author() -> Result<()> {
        let temp_dir = tempdir()?;
        let policy_path = temp_dir.path().join("mailing_lists.toml");
        std::fs::write(
            &policy_path,
            r#"
            [defaults]
            mute_all = false
            reply_to = ["author", "list", "recipients"]
            positive_review = "author"
            cc = ["maintainer@example.com"]
            "#,
        )?;

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.review.email_policy_path = policy_path.to_str().unwrap().to_string();
        settings.smtp = Some(crate::settings::SmtpSettings {
            transport: crate::settings::MailTransport::Smtp,
            server: Some("localhost".to_string()),
            port: Some(25),
            username: None,
            password: None,
            sendmail_path: None,
            sender_address: "bot@sashiko.dev".to_string(),
            reply_to: None,
            dry_run: false,
        });

        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;

        let thread_id = db.create_thread("msg_pos_root", "Subject", 1000).await?;

        // Case 1: Clean patch (0 issues) from external author -> sends positive review
        // only to the author, with empty Cc.
        db.create_message(
            "msg_pos_1",
            thread_id,
            None,
            "Author <author@example.com>",
            "[PATCH] clean patch",
            1000,
            "Commit body\n\nSigned-off-by: Author <author@example.com>\n---\ndiff",
            "netdev@vger.kernel.org",
            "reviewer@example.com",
            None,
            None,
        )
        .await?;

        let ps_id = db
            .create_patchset(
                thread_id,
                None,
                "msg_pos_root",
                "[PATCH] clean patch",
                "Author <author@example.com>",
                1000,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await?
            .unwrap();

        let p_id_1 = db.create_patch(ps_id, "msg_pos_1", 1, "diff").await?;

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
            activity: Default::default(),
            worker_program: None,
        };

        let empty_findings = Vec::new();
        Reviewer::queue_notifications(
            &ctx,
            ps_id,
            None,
            p_id_1,
            "msg_pos_1",
            "msg_pos_root",
            1,
            "",
            Some(&empty_findings),
            "No issues found",
        )
        .await?;

        let mut rows = db
            .conn
            .query(
                "SELECT status, to_addresses, cc_addresses, body FROM email_outbox WHERE patch_id = ?",
                libsql::params![p_id_1],
            )
            .await?;
        let row = rows.next().await?.expect("Expected email in outbox");
        let status: String = row.get(0)?;
        let to_addrs: String = row.get(1)?;
        let cc_addrs: String = row.get(2)?;
        let body: String = row.get(3)?;
        assert_eq!(status, "Pending");
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&to_addrs)?,
            vec!["Author <author@example.com>".to_string()]
        );
        assert!(serde_json::from_str::<Vec<String>>(&cc_addrs)?.is_empty());
        assert!(body.contains("Sashiko has reviewed this patch and found no issues.\n"));
        assert!(!body.contains("It looks great!"));

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

    #[tokio::test]
    async fn test_supervised_patchset_review_marks_failed_on_panic() -> Result<()> {
        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();

        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;

        let thread_id = db.create_thread("msg_id_panic", "Subject", 1000).await?;
        db.create_message(
            "msg_id_panic_p1",
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
                thread_id,
                None,
                "msg_id_panic",
                "Subject",
                "Author",
                1000,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await?
            .unwrap();
        db.create_patch(ps_id, "msg_id_panic_p1", 1, "diff --git a/a b/a\n")
            .await?;

        assert!(db.claim_patchset_for_review(ps_id).await?);
        assert_eq!(
            db.get_patchset_status(ps_id).await?.as_deref(),
            Some(ReviewStatus::InReview.as_str())
        );

        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await?;
        assert_eq!(semaphore.available_permits(), 0);

        let handle =
            Reviewer::spawn_supervised_patchset_review(db.clone(), permit, ps_id, async move {
                panic!("simulated worker panic");
            });
        handle.await?;

        assert_eq!(
            db.get_patchset_status(ps_id).await?.as_deref(),
            Some(ReviewStatus::Failed.as_str())
        );
        assert_eq!(semaphore.available_permits(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_review_patchset_task_marks_failed_on_baseline_db_error() -> Result<()> {
        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();

        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;

        let thread_id = db.create_thread("msg_id_base_err", "Subject", 1000).await?;
        db.create_message(
            "msg_id_base_err_p1",
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
        let baseline_id = db
            .create_baseline(
                None,
                Some("main"),
                Some("0123456789abcdef0123456789abcdef01234567"),
            )
            .await?;
        let ps_id = db
            .create_patchset(
                thread_id,
                None,
                "msg_id_base_err",
                "Subject",
                "Author",
                1000,
                1,
                1,
                "",
                "",
                None,
                1,
                Some(baseline_id),
                false,
                None,
                None,
            )
            .await?
            .unwrap();
        db.create_patch(ps_id, "msg_id_base_err_p1", 1, "diff --git a/a b/a\n")
            .await?;

        let mut patchsets = db.get_pending_patchsets(10).await?;
        assert_eq!(patchsets.len(), 1);
        let patchset = patchsets.pop().unwrap();
        assert_eq!(patchset.baseline_id, Some(baseline_id));

        // Corrupt the baseline row's column type so get_baseline_commit fails with a DB error
        // instead of returning Ok(Some(...)) or Ok(None).
        db.conn
            .execute(
                "UPDATE baselines SET last_known_commit = X'deadbeef' WHERE id = ?",
                libsql::params![baseline_id],
            )
            .await?;
        assert!(db.get_baseline_commit(baseline_id).await.is_err());

        let ctx = ReviewContext {
            semaphore: Arc::new(Semaphore::new(1)),
            llm_semaphore: Arc::new(Semaphore::new(1)),
            db: db.clone(),
            settings,
            baseline_registry: Arc::new(crate::baseline::BaselineRegistry::new(
                Path::new("."),
                None,
            )?),
            quota_manager: Arc::new(QuotaManager::new()),
            target_review_count: 1,
            provider: Arc::new(MockProvider),
            activity: Default::default(),
            worker_program: None,
        };

        Reviewer::review_patchset_task(ctx, patchset).await;

        // Must be marked Failed (not FailedToApply from falling back to heuristic candidates).
        assert_eq!(
            db.get_patchset_status(ps_id).await?.as_deref(),
            Some(ReviewStatus::Failed.as_str())
        );

        Ok(())
    }

    /// Counts the tool usage rows recorded for a review.
    async fn tool_usage_rows(db: &Database, review_id: i64) -> Result<i64> {
        let mut rows = db
            .conn
            .query(
                "SELECT count(*) FROM tool_usages WHERE review_id = ?",
                libsql::params![review_id],
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("count(*) returned no row"))?;
        Ok(row.get(0)?)
    }

    #[tokio::test]
    async fn tool_calls_in_a_truncated_response_are_not_recorded() -> Result<()> {
        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        let db = Database::new(&settings.database).await?;
        db.migrate().await?;
        let thread_id = db.create_thread("root", "Tool usage", 1000).await?;
        db.create_message(
            "msg1", thread_id, None, "Author", "Subject", 1000, "", "", "", None, None,
        )
        .await?;
        let patchset_id = db
            .create_patchset(
                thread_id, None, "msg1", "Subject", "Author", 1000, 1, 1, "", "", None, 1, None,
                true, None, None,
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("patchset was not created"))?;
        let review_id = db
            .create_review(patchset_id, None, "prov", "model", None, None)
            .await?;

        let mut resp = AiResponse {
            content: None,
            thought: None,
            thought_signature: None,
            tool_calls: Some(vec![crate::ai::ToolCall {
                id: "call_1".to_string(),
                function_name: "git_grep".to_string(),
                arguments: json!({"pattern": "foo"}),
                thought_signature: None,
            }]),
            usage: None,
            truncated: true,
        };
        record_tool_calls(&db, review_id, "prov", "model", &resp).await;
        assert_eq!(
            tool_usage_rows(&db, review_id).await?,
            0,
            "the worker never runs a call from a truncated response"
        );

        resp.truncated = false;
        record_tool_calls(&db, review_id, "prov", "model", &resp).await;
        assert_eq!(tool_usage_rows(&db, review_id).await?, 1);
        Ok(())
    }

    /// What a run of the worker returned decides how the attempt ended,
    /// whichever arm of the I/O loop's `Result` it came back in.
    #[test]
    fn an_attempt_end_is_classified_from_what_the_run_returned() {
        use crate::workflow::StageStopCause;

        let applied = |extra: Value| {
            let mut output = json!({
                "patchset_id": 1,
                "patches": [{"index": 1, "status": "applied"}],
            });
            if let (Some(fields), Some(more)) = (output.as_object_mut(), extra.as_object()) {
                fields.extend(more.clone());
            }
            output
        };
        let failed = |end: AttemptEnd| match end {
            AttemptEnd::Failed { reason, output } => (reason, output.is_some()),
            other => panic!("expected Failed, got {other:?}"),
        };

        let timeout = ReviewTimedOut {
            active_seconds: 60,
            salvage_seconds: 0,
        };
        let end = AttemptEnd::classify(
            Err(anyhow::Error::new(timeout).context("while reading the worker")),
            1,
            false,
        );
        assert!(
            matches!(end, AttemptEnd::TimedOut { timeout: t } if t == timeout),
            "a timeout behind further context is still a timeout: {end:?}"
        );
        assert_eq!(end.settle_cause(), StageStopCause::OutOfTime);
        assert!(!end.has_worker_review());

        let end = AttemptEnd::classify(Err(ReviewError::Cancelled.into()), 1, false);
        assert!(
            matches!(end, AttemptEnd::Cancelled { output: None }),
            "{end:?}"
        );
        assert_eq!(end.settle_cause(), StageStopCause::Cancelled);

        let end = AttemptEnd::classify(Err(anyhow!("worker exited")), 1, false);
        assert_eq!(end.settle_cause(), StageStopCause::Interrupted);
        assert_eq!(
            failed(end),
            ("Tool error: worker exited".to_string(), false)
        );

        // An error output is a failure with the worker's own reason, and keeps
        // the output for its token counts.
        let end = AttemptEnd::classify(
            Ok(applied(
                json!({"review": {"findings": []}, "error": "boom"}),
            )),
            1,
            false,
        );
        assert_eq!(end.settle_cause(), StageStopCause::Interrupted);
        assert!(!end.has_worker_review());
        assert_eq!(failed(end), ("boom".to_string(), true));

        let not_applied = json!({"patchset_id": 1, "patches": [{"index": 1, "status": "error"}]});
        assert_eq!(
            failed(AttemptEnd::classify(Ok(not_applied), 1, false)).0,
            "Tool failed to return patch status"
        );
        let other_patch = applied(json!({"review": {"findings": []}}));
        assert_eq!(
            failed(AttemptEnd::classify(Ok(other_patch), 2, false)).0,
            "Tool failed to return patch status",
            "a result for another patch is no result for this one"
        );
        assert_eq!(
            failed(AttemptEnd::classify(Ok(applied(json!({}))), 1, false)).0,
            "Missing review content"
        );
        assert_eq!(
            failed(AttemptEnd::classify(
                Ok(applied(json!({"review": null}))),
                1,
                false
            ))
            .0,
            "AI returned null response"
        );

        // A null review is the answer when the AI review was turned off.
        for (output, no_ai) in [
            (applied(json!({"review": null})), true),
            (applied(json!({"review": {"findings": []}})), false),
        ] {
            let end = AttemptEnd::classify(Ok(output), 1, no_ai);
            assert!(end.has_worker_review(), "{end:?}");
            assert_eq!(end.settle_cause(), StageStopCause::Interrupted);
        }
    }

    /// A cancel decides the end whatever the run returned, and keeps only a
    /// review the worker returned.
    #[test]
    fn a_cancel_decides_the_end_and_keeps_a_returned_review() {
        use crate::workflow::StageStopCause;

        let reviewed = json!({
            "patchset_id": 1,
            "patches": [{"index": 1, "status": "applied"}],
            "review": {"findings": []},
        });
        let end = AttemptEnd::decide(Ok(reviewed.clone()), 1, false, true);
        assert!(
            matches!(&end, AttemptEnd::Cancelled { output: Some(o) } if *o == reviewed),
            "{end:?}"
        );
        assert!(end.has_worker_review(), "the worker's own record is kept");
        assert_eq!(end.settle_cause(), StageStopCause::Cancelled);

        let end = AttemptEnd::decide(Ok(reviewed), 1, false, false);
        assert!(matches!(end, AttemptEnd::Finished { .. }), "{end:?}");

        let no_review = json!({"patchset_id": 1, "patches": [{"index": 1, "status": "error"}]});
        let timeout = ReviewTimedOut {
            active_seconds: 60,
            salvage_seconds: 0,
        };
        for result in [
            Ok(no_review),
            Err(anyhow::Error::new(timeout)),
            Err(ReviewError::Cancelled.into()),
            Err(anyhow!("worker exited")),
        ] {
            let end = AttemptEnd::decide(result, 1, false, true);
            assert!(
                matches!(end, AttemptEnd::Cancelled { output: None }),
                "{end:?}"
            );
            assert!(!end.has_worker_review(), "{end:?}");
        }
    }

    /// A mock worker that reports `lines` and then runs `then`: the shell
    /// that ends its run.
    fn worker_reporting_then(lines: &[String], then: &str) -> String {
        let mut script = String::from("#!/bin/sh\nread -r _payload\n");
        for line in lines {
            script.push_str(&format!("echo '{}'\n", line));
        }
        script.push_str(then);
        script
    }

    /// Progress for a stage that starts and reaches `turn`.
    fn stage_reaching(stage: &str, turn: usize) -> Vec<String> {
        use crate::activity::ProgressWire;
        vec![
            progress(
                1,
                ProgressWire::StageStarted {
                    stage: stage.to_string(),
                },
            ),
            progress(
                1,
                ProgressWire::StageTurn {
                    stage: stage.to_string(),
                    turn,
                    max_turns: 50,
                },
            ),
        ]
    }

    /// One patch reviewed by `process_patch_review` with `script` as its
    /// worker.
    struct WorkerReview {
        result: PatchResult,
        db: Arc<Database>,
        activity: Arc<crate::activity::ActivityRegistry>,
        patchset_id: i64,
        patch_id: i64,
    }

    async fn review_patch_with_worker(
        script: &str,
        configure: impl FnOnce(&mut Settings),
    ) -> Result<WorkerReview> {
        review_patch_with_worker_while(script, configure, |_, _| async { Ok(()) }).await
    }

    /// As [`review_patch_with_worker`], running `during` alongside the review
    /// with the database and the patchset's id, to act on the review while
    /// its worker runs.
    async fn review_patch_with_worker_while<F, Fut>(
        script: &str,
        configure: impl FnOnce(&mut Settings),
        during: F,
    ) -> Result<WorkerReview>
    where
        F: FnOnce(Arc<Database>, i64) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let temp_dir = tempdir()?;
        let bin_path = temp_dir.path().join("mock_worker");
        std::fs::write(&bin_path, script)?;
        std::fs::set_permissions(&bin_path, Permissions::from_mode(0o755))?;

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        configure(&mut settings);
        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;
        let activity = crate::activity::ActivityRegistry::new();
        let ctx = ReviewContext {
            semaphore: Arc::new(Semaphore::new(1)),
            llm_semaphore: Arc::new(Semaphore::new(56)),
            db: db.clone(),
            settings,
            baseline_registry: Arc::new(BaselineRegistry::new(Path::new("."), None)?),
            quota_manager: Arc::new(QuotaManager::new()),
            target_review_count: 1,
            provider: Arc::new(MockProvider),
            activity: activity.clone(),
            worker_program: Some(bin_path),
        };

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
        let patchset_id = db
            .create_patchset(
                thread_id, None, "msg_id_1", "Subject", "Author", 1000, 1, 1, "", "", None, 1,
                None, false, None, None,
            )
            .await?
            .ok_or_else(|| anyhow!("patchset was not created"))?;
        let diff = "diff --git a/foo.c b/foo.c\n+int x;";
        let patch_id = db.create_patch(patchset_id, "msg_id_p1", 1, diff).await?;

        let input_payload = json!({});
        let review = Reviewer::process_patch_review(
            &ctx,
            patchset_id,
            patch_id,
            1,
            "HEAD",
            None,
            &input_payload,
            None,
            None,
            None,
            diff,
            None,
        );
        let (result, during) = tokio::join!(review, during(db.clone(), patchset_id));
        during?;
        let result = result?;
        Ok(WorkerReview {
            result,
            db,
            activity,
            patchset_id,
            patch_id,
        })
    }

    /// Every review row of the patch, oldest first, with its status, result
    /// and recorded stage lists.
    async fn review_rows(db: &Database, patch_id: i64) -> Result<Vec<Value>> {
        let mut rows = db
            .conn
            .query(
                "SELECT id FROM reviews WHERE patch_id = ? ORDER BY id",
                libsql::params![patch_id],
            )
            .await?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next().await? {
            ids.push(row.get::<i64>(0)?);
        }
        let mut reviews = Vec::new();
        for id in ids {
            reviews.push(
                db.get_review_details(id)
                    .await?
                    .ok_or_else(|| anyhow!("review {id} vanished"))?,
            );
        }
        Ok(reviews)
    }

    /// The stage failures a review row records, as (stage, cause, reason).
    fn recorded_failures(review: &Value) -> Vec<(String, String, String)> {
        review["stage_failures"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| {
                (
                    f["stage"].as_str().unwrap_or_default().to_string(),
                    f["cause"].as_str().unwrap_or_default().to_string(),
                    f["reason"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    /// The stored stage timings as (stage, turns) pairs. The seconds are left
    /// out because they are wall-clock time spent around a real subprocess,
    /// which a loaded machine can push past a whole second.
    fn recorded_timings(review: &Value) -> Vec<(String, u64)> {
        review["stage_durations"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| {
                (
                    t["stage"].as_str().unwrap_or_default().to_string(),
                    t["turns"].as_u64().unwrap_or_default(),
                )
            })
            .collect()
    }

    /// A worker's line streaming `content` from `stage` to the live log.
    fn streamed(stage: Option<&str>, content: &str) -> String {
        use crate::ai::{AiMessage, AiRole};
        progress(
            1,
            crate::activity::ProgressWire::LogEntry {
                stage: stage.map(str::to_string),
                message: AiMessage {
                    role: if stage.is_some() {
                        AiRole::Assistant
                    } else {
                        AiRole::System
                    },
                    content: Some(content.to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            },
        )
    }

    /// The header a promoted live log opens with when it was not capped.
    const UNFINISHED_LOG_HEADER: &str =
        "[The review did not finish. This is the conversation streamed while it ran.]";

    /// A review row's stored log as (stage, content) pairs, in order.
    fn recorded_log(review: &Value) -> Vec<(Option<String>, String)> {
        let logs: Vec<Value> = review["logs"]
            .as_str()
            .and_then(|l| serde_json::from_str(l).ok())
            .unwrap_or_default();
        logs.iter()
            .map(|m| {
                (
                    m["stage"].as_str().map(str::to_string),
                    m["content"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    /// The log a row keeps of an attempt that streamed `entries` and then
    /// stopped: the header, then each entry with its stage.
    fn promoted(entries: &[(Option<&str>, &str)]) -> Vec<(Option<String>, String)> {
        std::iter::once((None, UNFINISHED_LOG_HEADER.to_string()))
            .chain(
                entries
                    .iter()
                    .map(|(stage, content)| (stage.map(str::to_string), content.to_string())),
            )
            .collect()
    }

    /// A worker whose review fails reports it in an output, not an error, and
    /// that output carries no stage list. The row must still say which stages
    /// stopped and how far each got, and the attempt is retried as before.
    #[tokio::test]
    async fn test_an_error_output_records_what_the_attempt_did() -> Result<()> {
        use crate::activity::{ActivityKey, Phase, ProgressWire};

        let mut lines = stage_reaching("goal", 2);
        lines.push(progress(
            1,
            ProgressWire::StageFinished {
                stage: "goal".into(),
            },
        ));
        lines.extend(stage_reaching("security", 1));
        lines.push(progress(
            1,
            ProgressWire::StageFailed {
                stage: "security".into(),
                reason: "provider unavailable".into(),
                cause: crate::workflow::StageStopCause::Error,
            },
        ));
        lines.extend(stage_reaching("locking", 3));
        lines.push(streamed(None, "system prompt"));
        lines.push(streamed(Some("locking"), "reading the lock order"));
        let error_output = r#"{"patchset_id": 1, "patches": [{"index": 1, "status": "applied", "review_status": "incomplete", "review_error": "boom"}], "review": {"findings": []}, "error": "boom", "history": []}"#;
        let script = worker_reporting_then(&lines, &format!("echo '{error_output}'\n"));

        let run = review_patch_with_worker(&script, |s| s.review.max_retries = 1).await?;
        assert!(matches!(run.result, PatchResult::ReviewFailed));

        let reviews = review_rows(&run.db, run.patch_id).await?;
        assert_eq!(reviews.len(), 2, "an error output is retried as before");
        for review in &reviews {
            assert_eq!(review["status"], "Failed", "{review}");
            assert_eq!(review["result"], "boom", "{review}");
            let failures = recorded_failures(review);
            let causes: Vec<(&str, &str)> = failures
                .iter()
                .map(|(stage, cause, _)| (stage.as_str(), cause.as_str()))
                .collect();
            assert_eq!(causes.len(), 2, "{failures:?}");
            assert!(causes.contains(&("security", "error")), "{failures:?}");
            assert!(causes.contains(&("locking", "interrupted")), "{failures:?}");
            let locking = failures
                .iter()
                .find(|(stage, ..)| stage == "locking")
                .map(|(.., reason)| reason.as_str());
            // The elapsed time in the middle is wall-clock time around a real
            // subprocess, so only the parts that do not depend on it are checked.
            assert!(
                locking.is_some_and(|reason| reason
                    .starts_with("still running when the review stopped, after ")
                    && reason.ends_with(" and 3 turns")),
                "{failures:?}"
            );
            assert_eq!(
                recorded_timings(review),
                vec![("goal".to_string(), 2)],
                "a failed attempt keeps its timings: {review}"
            );
            assert_eq!(
                recorded_log(review),
                promoted(&[
                    (None, "system prompt"),
                    (Some("locking"), "reading the lock order")
                ]),
                "the streamed conversation replaces the output's empty history: {review}"
            );
        }
        let streamed_left = run
            .db
            .conn
            .query("SELECT COUNT(*) FROM review_log_entries", ())
            .await?
            .next()
            .await?
            .map(|row| row.get::<i64>(0))
            .transpose()?;
        assert_eq!(
            streamed_left,
            Some(0),
            "the streamed copy is not kept twice"
        );

        // The stage left running no longer claims to be mid-turn.
        let locking = run
            .activity
            .get(&ActivityKey::PatchsetStage {
                patchset_id: run.patchset_id,
                patch_id: run.patch_id,
                stage: "locking".into(),
            })
            .expect("the stage keeps its row");
        assert!(
            matches!(
                locking.phase,
                Phase::StageFailed {
                    cause: crate::workflow::StageStopCause::Interrupted,
                    ..
                }
            ),
            "{locking:?}"
        );
        Ok(())
    }

    /// A worker killed at its deadline leaves the stages it was running in,
    /// and they are recorded as stopped by the deadline, next to the timings
    /// of the stages that finished.
    #[tokio::test]
    async fn test_a_timed_out_attempt_records_what_it_was_running() -> Result<()> {
        use crate::activity::ProgressWire;

        let mut lines = stage_reaching("goal", 1);
        lines.push(progress(
            1,
            ProgressWire::StageFinished {
                stage: "goal".into(),
            },
        ));
        lines.extend(stage_reaching("security", 4));
        lines.push(streamed(Some("security"), "tracing the user copy"));
        let script = worker_reporting_then(&lines, "exec sleep 30\n");

        let run = review_patch_with_worker(&script, |s| {
            s.review.timeout_seconds = 1;
            s.review.max_retries = 0;
        })
        .await?;
        assert!(matches!(run.result, PatchResult::ReviewFailed));

        let reviews = review_rows(&run.db, run.patch_id).await?;
        assert_eq!(reviews.len(), 1);
        let review = &reviews[0];
        assert_eq!(review["status"], "Failed");
        assert_eq!(
            review["result"],
            "Tool error: Review tool timed out (active time exceeded)"
        );
        let failures = recorded_failures(review);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(
            (failures[0].0.as_str(), failures[0].1.as_str()),
            ("security", "out_of_time")
        );
        assert!(
            failures[0].2.ends_with("and 4 turns"),
            "the reason says how far the stage got: {failures:?}"
        );
        assert_eq!(
            recorded_timings(review),
            vec![("goal".to_string(), 1)],
            "{review}"
        );
        assert_eq!(
            recorded_log(review),
            promoted(&[(Some("security"), "tracing the user copy")]),
            "a killed worker returns no history, so the stream is all there is: {review}"
        );
        Ok(())
    }

    /// A review that ran out of time is not tried again, however many retries
    /// remain: an identical attempt gets an identical deadline. The patch
    /// fails at once, while any other failure is still retried (see
    /// `test_an_error_output_records_what_the_attempt_did`).
    #[tokio::test]
    async fn test_a_timed_out_review_is_not_retried() -> Result<()> {
        let script = worker_reporting_then(&stage_reaching("security", 1), "exec sleep 30\n");

        let run = review_patch_with_worker(&script, |s| {
            s.review.timeout_seconds = 1;
            s.review.max_retries = 2;
        })
        .await?;
        assert!(matches!(run.result, PatchResult::ReviewFailed));

        let reviews = review_rows(&run.db, run.patch_id).await?;
        assert_eq!(reviews.len(), 1, "one attempt only: {reviews:?}");
        assert_eq!(reviews[0]["status"], "Failed");
        assert_eq!(reviews[0]["attempt"], 1, "{}", reviews[0]);
        let patch_status = run
            .db
            .conn
            .query(
                "SELECT status FROM patches WHERE id = ?",
                libsql::params![run.patch_id],
            )
            .await?
            .next()
            .await?
            .map(|row| row.get::<String>(0))
            .transpose()?;
        assert_eq!(patch_status.as_deref(), Some("Failed"));
        Ok(())
    }

    /// A review cancelled while its worker runs keeps what it streamed, and
    /// the stage it was running is recorded as cancelled.
    #[tokio::test]
    async fn test_a_cancelled_attempt_keeps_its_conversation() -> Result<()> {
        let mut lines = stage_reaching("security", 2);
        lines.push(streamed(Some("security"), "checking the bounds"));
        let script = worker_reporting_then(&lines, "exec sleep 30\n");

        let run = review_patch_with_worker_while(
            &script,
            |s| s.review.max_retries = 0,
            cancel_once_streamed,
        )
        .await?;
        assert!(matches!(run.result, PatchResult::Cancelled));

        let reviews = review_rows(&run.db, run.patch_id).await?;
        assert_eq!(reviews.len(), 1);
        let review = &reviews[0];
        assert_eq!(review["status"], "Cancelled", "{review}");
        assert_eq!(
            recorded_log(review),
            promoted(&[(Some("security"), "checking the bounds")]),
            "{review}"
        );
        let failures = recorded_failures(review);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(
            (failures[0].0.as_str(), failures[0].1.as_str()),
            ("security", "cancelled")
        );
        Ok(())
    }

    /// Cancels the patchset once the review's conversation has reached the
    /// live log, so the cancel cannot land before there is anything to keep.
    async fn cancel_once_streamed(db: Arc<Database>, patchset_id: i64) -> Result<()> {
        let streamed = || async {
            let mut rows = db
                .conn
                .query("SELECT COUNT(*) FROM review_log_entries", ())
                .await?;
            let count = match rows.next().await? {
                Some(row) => row.get::<i64>(0)?,
                None => 0,
            };
            anyhow::Ok(count > 0)
        };
        let wait = async {
            while !streamed().await? {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            anyhow::Ok(())
        };
        tokio::time::timeout(Duration::from_secs(20), wait).await??;
        db.update_patchset_status(patchset_id, ReviewStatus::Cancelled.as_str())
            .await
    }

    /// A worker that returns its review after the patchset was cancelled,
    /// but before the run noticed, did finish. Its row is still cancelled,
    /// and keeps the worker's own history and token counts rather than the
    /// streamed copy under a header saying the review did not finish.
    #[tokio::test]
    async fn test_a_review_finished_as_it_was_cancelled_keeps_the_workers_history() -> Result<()> {
        let signal_dir = tempdir()?;
        let cancelled = signal_dir.path().join("cancelled");
        let lines = [streamed(Some("security"), "checking the bounds")];
        let finished = r#"{"patchset_id": 1, "patches": [{"index": 1, "status": "applied"}], "review": {"findings": []}, "tokens_in": 7, "tokens_out": 3, "history": [{"role": "assistant", "content": "the whole review"}]}"#;
        // The worker waits for the cancel and then returns at once, well
        // inside the run's two-second cancel poll, so only the check after
        // the run sees the cancel.
        let script = worker_reporting_then(
            &lines,
            &format!(
                "while [ ! -f '{}' ]; do sleep 0.02; done\necho '{finished}'\n",
                cancelled.display()
            ),
        );

        let cancel_then_finish = |db: Arc<Database>, patchset_id: i64| async move {
            cancel_once_streamed(db, patchset_id).await?;
            std::fs::write(&cancelled, "")?;
            Ok(())
        };
        let run = review_patch_with_worker_while(
            &script,
            |s| s.review.max_retries = 0,
            cancel_then_finish,
        )
        .await?;
        assert!(matches!(run.result, PatchResult::Cancelled));

        let reviews = review_rows(&run.db, run.patch_id).await?;
        assert_eq!(reviews.len(), 1);
        let review = &reviews[0];
        assert_eq!(review["status"], "Cancelled", "{review}");
        assert_eq!(
            recorded_log(review),
            vec![(None, "the whole review".to_string())],
            "{review}"
        );
        assert_eq!(review["tokens_in"], 7, "{review}");
        assert!(recorded_failures(review).is_empty(), "{review}");
        Ok(())
    }

    /// A promoted log opens with a header saying the review did not finish,
    /// and keeps each entry whole, with its stage, in the order streamed.
    #[test]
    fn an_unfinished_log_is_headed_and_keeps_each_entry_whole() -> Result<()> {
        use crate::ai::{AiMessage, AiRole};

        let entry = |seq: i64, stage: Option<&str>, content: &str| crate::api::LiveLogEntry {
            seq,
            stage: stage.map(str::to_string),
            stage_label: stage.map(|s| format!("label of {s}")),
            created_at: 100 + seq,
            message: AiMessage {
                role: AiRole::User,
                content: Some(content.to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            },
        };
        let entries = vec![entry(3, None, "first"), entry(4, Some("goal"), "second")];

        let log = unfinished_log(entries.clone(), false)?;
        let log = log.as_array().cloned().unwrap_or_default();
        assert_eq!(log.len(), 3);
        assert_eq!(
            log[0],
            json!({"role": "system", "content": UNFINISHED_LOG_HEADER})
        );
        for (stored, entry) in log[1..].iter().zip(&entries) {
            let back: crate::api::LiveLogEntry = serde_json::from_value(stored.clone())?;
            assert_eq!(&back, entry, "the entry is stored whole");
        }
        assert_eq!(log[2]["role"], "user", "the message's fields stay flat");
        assert_eq!(log[2]["stage_label"], "label of goal");

        let capped = unfinished_log(entries, true)?;
        assert_eq!(
            capped[0]["content"],
            format!(
                "[The review did not finish. This is the conversation streamed while it ran, \
                 capped at {} entries.]",
                crate::db::MAX_LIVE_LOG_ENTRIES
            ),
            "a capped stream says it is not the whole conversation"
        );
        Ok(())
    }

    /// A review that streamed nothing promotes nothing, so the row keeps what
    /// it would have kept without promotion; one that did gets its entries
    /// with the project's stage labels.
    #[tokio::test]
    async fn test_promoted_live_log_reads_the_stream_with_its_labels() -> Result<()> {
        use crate::project::ProjectId;

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        let db = Database::new(&settings.database).await?;
        db.migrate().await?;
        let thread_id = db.create_thread("msg_id_1", "Subject", 1000).await?;
        let patchset_id = db
            .create_patchset(
                thread_id, None, "msg_id_1", "Subject", "Author", 1000, 1, 1, "", "", None, 1,
                None, false, None, None,
            )
            .await?
            .ok_or_else(|| anyhow!("patchset was not created"))?;
        let review_id = db
            .create_review(patchset_id, None, "p", "m", None, None)
            .await?;

        assert_eq!(
            promoted_live_log(&db, review_id, ProjectId::Linux).await?,
            None
        );

        let message = |content: &str| json!({"role": "assistant", "content": content}).to_string();
        db.append_review_log_entry(review_id, 0, Some("goal"), "assistant", &message("a"))
            .await?;
        db.append_review_log_entry(
            review_id,
            1,
            Some("no_such_stage"),
            "assistant",
            &message("b"),
        )
        .await?;
        let log = promoted_live_log(&db, review_id, ProjectId::Linux)
            .await?
            .ok_or_else(|| anyhow!("the stream was promoted"))?;
        let log: Vec<Value> = serde_json::from_str(&log)?;
        assert_eq!(log[0]["content"], UNFINISHED_LOG_HEADER);
        assert_eq!(
            log[1]["stage_label"],
            crate::workflows::stage_short_label(ProjectId::Linux, "goal")
                .ok_or_else(|| anyhow!("goal has a label"))?
        );
        assert!(
            log[2]["stage_label"].is_null(),
            "an unknown stage has no label"
        );
        assert_eq!(log[2]["content"], "b");
        Ok(())
    }
}
