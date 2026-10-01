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

use crate::{
    git_ops::{GitWorktree, extract_patch_metadata, get_commit_hash, resolve_git_range},
    references::ReviewReferences,
    settings::{AiSettings, Settings},
    toolbox::ToolBox,
    worker::{
        PatchInput, ReviewInput, Worker, WorkerConfig, calculate_series_range,
        prompts::PromptRegistry,
    },
};
use anyhow::{Context, Result, anyhow};
use futures::stream::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

/// What a worker is asked to do: only what the settings have no notion of, such
/// as which patch, which baseline, and where the prompts are.
///
/// Anything a settings file can also say — the provider, the stage list, whether
/// to skip the AI, the repository, the worktree directory — is not here. `main`
/// merges those onto the settings it read, so there is one answer to read.
#[derive(Clone, Debug)]
pub struct WorkerOptions {
    pub project: crate::project::ProjectId,
    pub baseline: Option<String>,
    pub prompts: PathBuf,
    pub review_patch_index: Option<i64>,
    pub review_commit: Option<String>,
    pub reuse_worktree: Option<PathBuf>,
    pub custom_prompt: Option<String>,
    pub scratch_clone: bool,
    pub current_tree: bool,
    pub agent: bool,
    pub report_preexisting: bool,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            project: crate::project::ProjectId::Linux,
            baseline: None,
            prompts: PathBuf::from("third_party/prompts/kernel"),
            review_patch_index: None,
            review_commit: None,
            reuse_worktree: None,
            custom_prompt: None,
            scratch_clone: false,
            current_tree: false,
            agent: false,
            report_preexisting: false,
        }
    }
}

#[derive(Clone, Debug)]
/// What a local review is asked to do. The settings it works from arrive
/// separately, already read by the process that starts it.
pub struct ReviewOptions {
    pub project: crate::project::ProjectId,
    pub baseline: Option<String>,
    pub prompts: PathBuf,
    pub custom_prompt: Option<String>,
    pub agent: bool,
    pub report_preexisting: bool,
}

impl Default for ReviewOptions {
    fn default() -> Self {
        Self {
            project: crate::project::ProjectId::Linux,
            baseline: None,
            prompts: PathBuf::from("third_party/prompts/kernel"),
            custom_prompt: None,
            agent: false,
            report_preexisting: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ProgressEvent {
    ResolvingInput {
        input: String,
    },
    ResolvedCommits {
        commits: Vec<CommitSummary>,
    },
    BaselineResolved {
        rev: String,
        sha: String,
    },
    CurrentTreeReady {
        path: PathBuf,
    },
    WorktreeCreated {
        path: PathBuf,
    },
    ApplyingPatch {
        index: i64,
        total: usize,
        subject: String,
    },
    PatchApplied {
        index: i64,
    },
    PatchFailed {
        index: i64,
        error: String,
    },
    AiReviewStarted {
        patches: usize,
    },
    AiReviewPreScreenStarted {
        patch_index: i64,
    },
    AiReviewPlanningStarted {
        patch_index: i64,
    },
    AiReviewPlanReady {
        patch_index: i64,
        planned_stages: Vec<String>,
    },
    AiReviewStageStarted {
        patch_index: i64,
        stage: String,
    },
    AiReviewStageTurn {
        patch_index: i64,
        stage: String,
        turn: usize,
        max_turns: usize,
    },
    /// A stage started running tools (non-empty `tools`) or finished them.
    AiReviewStageTools {
        patch_index: i64,
        stage: String,
        tools: Vec<String>,
        turn: usize,
        max_turns: usize,
    },
    /// A stage is backing off; `retry_in_seconds` is `None` once it is over.
    AiReviewStageBackoff {
        patch_index: i64,
        stage: String,
        retry_in_seconds: Option<u64>,
        turn: usize,
        max_turns: usize,
    },
    AiReviewStageFinished {
        patch_index: i64,
        stage: String,
    },
    /// A stage stopped without finishing. Distinct from
    /// [`ProgressEvent::AiReviewStageFinished`] because the two mean opposite
    /// things to anyone watching: one stage is done, the other never will be.
    AiReviewStageFailed {
        patch_index: i64,
        stage: String,
        reason: String,
        cause: crate::workflow::StageStopCause,
    },
    /// One message joining a stage's conversation log, so a daemon can show
    /// a running review's conversation before it finishes. `stage` is `None`
    /// for the system message, which belongs to no stage.
    AiReviewLogEntry {
        patch_index: i64,
        stage: Option<String>,
        message: crate::ai::AiMessage,
    },
    AiReviewAttempt {
        patch_index: i64,
        attempt: usize,
        max_attempts: usize,
    },
    AiReviewFinished {
        patch_index: i64,
    },
    AiReviewFailed {
        patch_index: i64,
    },
    ReviewComplete {
        partial: bool,
    },
}

#[derive(Debug, Clone)]
pub struct CommitSummary {
    pub index: i64,
    pub sha: String,
    pub subject: String,
    pub author: String,
}

pub type ProgressCallback<'a> = dyn Fn(ProgressEvent) + Send + Sync + 'a;

pub async fn build_review_input_from_git(
    repo_path: &Path,
    input: &str,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<(ReviewInput, Vec<String>)> {
    emit(
        progress,
        ProgressEvent::ResolvingInput {
            input: input.to_string(),
        },
    );

    let shas = if input.contains("..") {
        resolve_git_range(repo_path, input).await?
    } else {
        vec![get_commit_hash(repo_path, input).await?]
    };

    let mut patches = Vec::new();
    let mut summaries = Vec::new();

    for (i, sha) in shas.iter().enumerate() {
        let meta = extract_patch_metadata(repo_path, sha)
            .await
            .with_context(|| format!("Failed to extract metadata for commit {}", sha))?;
        let index = (i + 1) as i64;
        summaries.push(CommitSummary {
            index,
            sha: short_sha(sha),
            subject: meta.subject.clone(),
            author: meta.author.clone(),
        });
        patches.push(PatchInput {
            index,
            diff: meta.diff,
            subject: Some(meta.subject),
            author: Some(meta.author),
            date: Some(meta.timestamp),
            message_id: None,
            commit_id: Some(sha.clone()),
        });
    }

    emit(
        progress,
        ProgressEvent::ResolvedCommits { commits: summaries },
    );

    let subject = patches
        .first()
        .and_then(|p| p.subject.clone())
        .unwrap_or_else(|| input.to_string());

    Ok((
        ReviewInput {
            id: 0,
            subject,
            patches,
        },
        shas,
    ))
}

pub async fn run_git_review(
    repo_path: PathBuf,
    input: String,
    options: ReviewOptions,
    settings: &Settings,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<Value> {
    let (review_input, shas) = build_review_input_from_git(&repo_path, &input, progress).await?;
    let baseline = options
        .baseline
        .clone()
        .or_else(|| shas.first().map(|sha| format!("{}^", sha)));

    run_worker(
        review_input,
        WorkerOptions {
            project: options.project,
            baseline,
            prompts: options.prompts,
            custom_prompt: options.custom_prompt,
            current_tree: true,
            agent: options.agent,
            report_preexisting: options.report_preexisting,
            ..WorkerOptions::default()
        },
        settings,
        progress,
    )
    .await
}

pub async fn run_worker(
    input: ReviewInput,
    options: WorkerOptions,
    settings: &Settings,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<Value> {
    // Handed in, because the process read them once already: nothing here opens
    // the settings file, and nothing here merges an override onto it.
    let concurrency = settings.review.concurrency;
    let timeout_seconds = settings.review.timeout_seconds;
    let worktree_dir = settings.review.worktree_dir.as_deref().map(Path::new);

    // The references are a [git] setting like the repository itself; the
    // worker's --reference and --reference-revision flags have already been
    // merged onto them.
    let references =
        crate::references::references_for_project(options.project, settings.git.references.clone());

    // Validated here as well as at daemon startup, because this is the only
    // check that runs on every path into a review, including builds without the
    // server feature. Each path is made absolute so it means the same thing
    // whatever directory each tool's git command runs in, and each revision is
    // pinned to the commit it resolved to now, so a fetch in a clone during the
    // review cannot move what the rest of it reads.
    let references = crate::references::resolve_references(&references).await?;

    // One line per reference, with the commit it was pinned to. A branch pin
    // is expected -- rdma-core, for one, is pinned to a release branch that
    // moves as the operator fetches -- so it earns no warning: this line is
    // the record of which commit this run actually read.
    for r in &references {
        info!(
            "Reference repository {}: kind {}, path {}, revision {} -> {}, applies to {}",
            r.name,
            r.kind,
            r.path.display(),
            r.revision,
            r.commit,
            r.applies_to
                .as_ref()
                .map_or("every patch".to_string(), |a| a.patterns().join(", "))
        );
    }

    // Decided here, on the resolved provider, so it is the same predicate the
    // limiters and the reference hand-off use. Every event still reaches the
    // caller's own callback (the `progress: ` lines on stderr, or a terminal
    // display); a daemon-spawned worker also forwards it to the daemon, which
    // is what lets a running review say what it is doing.
    let daemon_progress = DaemonProgress::for_worker(&settings.ai);
    let forward = |event: ProgressEvent| {
        if let Some(daemon) = &daemon_progress {
            daemon.send(&event);
        }
        if let Some(cb) = progress {
            cb(event);
        }
    };
    let progress: Option<&ProgressCallback<'_>> =
        (progress.is_some() || daemon_progress.is_some()).then_some(&forward);

    let patchset_id = input.id;
    let subject = input.subject;
    let patches = input.patches;
    let baseline_arg = if options.current_tree {
        options.baseline.clone().unwrap_or_default()
    } else {
        options
            .baseline
            .clone()
            .unwrap_or_else(|| "HEAD".to_string())
    };
    // The tree to review, which `main` resolved: a local review writes the
    // checkout it was run in, a worker given --repo writes that, and a daemon's
    // worker takes what the settings file says.
    let repo_path = Some(settings.git.repository_path.as_str())
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("Missing repository path"))?;

    let (worktree, baseline_sha) = if options.current_tree {
        emit(
            progress,
            ProgressEvent::CurrentTreeReady {
                path: repo_path.clone(),
            },
        );
        (
            GitWorktree::from_path(repo_path.clone(), repo_path.clone()),
            baseline_arg.clone(),
        )
    } else if let Some(path) = &options.reuse_worktree {
        let baseline_sha = get_commit_hash(&repo_path, &baseline_arg).await?;
        emit(
            progress,
            ProgressEvent::BaselineResolved {
                rev: baseline_arg.clone(),
                sha: short_sha(&baseline_sha),
            },
        );
        info!("Reusing existing worktree at {:?}", path);
        (
            GitWorktree::from_path(path.clone(), repo_path.clone()),
            baseline_sha,
        )
    } else if options.scratch_clone {
        let baseline_sha = get_commit_hash(&repo_path, &baseline_arg).await?;
        emit(
            progress,
            ProgressEvent::BaselineResolved {
                rev: baseline_arg.clone(),
                sha: short_sha(&baseline_sha),
            },
        );
        let worktree =
            GitWorktree::new_scratch_clone(&repo_path, &baseline_sha, worktree_dir).await?;
        emit(
            progress,
            ProgressEvent::WorktreeCreated {
                path: worktree.path.clone(),
            },
        );
        (worktree, baseline_sha)
    } else {
        let baseline_sha = get_commit_hash(&repo_path, &baseline_arg).await?;
        emit(
            progress,
            ProgressEvent::BaselineResolved {
                rev: baseline_arg.clone(),
                sha: short_sha(&baseline_sha),
            },
        );
        let worktree = GitWorktree::new(&repo_path, &baseline_sha, worktree_dir).await?;
        emit(
            progress,
            ProgressEvent::WorktreeCreated {
                path: worktree.path.clone(),
            },
        );
        (worktree, baseline_sha)
    };

    let result = run_worker_in_worktree(
        &worktree,
        settings,
        concurrency,
        timeout_seconds,
        patchset_id,
        subject,
        patches,
        &baseline_arg,
        &baseline_sha,
        &options,
        references,
        progress,
    )
    .await;

    if !options.current_tree
        && let Err(e) = worktree.remove().await
    {
        error!("Failed to remove worktree: {}", e);
    }
    emit(
        progress,
        ProgressEvent::ReviewComplete {
            partial: result.as_ref().map_or(true, result_has_error),
        },
    );

    // Every progress line has to reach the daemon before the result does: the
    // daemon stops reading at the result, so a stage's "finished" written after
    // it would never be seen, and the stage would read as still running.
    if let Some(daemon) = daemon_progress {
        daemon.finish().await;
    }

    result
}

#[allow(clippy::too_many_arguments)]
/// Wrap the raw provider with the decorators a worker-run review needs.
///
/// Per-turn logging is implemented once, as a provider decorator, rather than
/// in each front end: both local-CLI and daemon-spawned worker reviews run this
/// same path, so wrapping here covers both.
///
/// The limiters are skipped for a daemon-spawned worker, which reaches the
/// model through a stdio provider and is throttled by the daemon instead.
fn decorate_provider(
    inner: std::sync::Arc<dyn crate::ai::AiProvider>,
    ai: &AiSettings,
    llm_semaphore: &Arc<Semaphore>,
    quota: &Arc<crate::ai::quota::QuotaManager>,
    retry_budget: &Option<Arc<dyn crate::ai::backoff_provider::RetryBudget>>,
) -> std::sync::Arc<dyn crate::ai::AiProvider> {
    let provider: std::sync::Arc<dyn crate::ai::AiProvider> = if ai.log_turns {
        std::sync::Arc::new(crate::ai::logging_provider::LoggingProvider::new(inner))
    } else {
        inner
    };

    if reaches_model_through_daemon(ai) {
        return provider;
    }

    let provider: std::sync::Arc<dyn crate::ai::AiProvider> = std::sync::Arc::new(
        crate::ai::concurrency_limited_provider::ConcurrencyLimitedProvider::new(
            provider,
            llm_semaphore.clone(),
        ),
    );

    // Backoff goes outermost, so a call that is waiting out a rate limit holds
    // no concurrency permit while it sleeps.
    // With a retry budget it stops at the review's deadline, as in the daemon;
    // without one it falls back to an attempt ceiling.
    std::sync::Arc::new(crate::ai::backoff_provider::BackoffProvider::new(
        provider,
        quota.clone(),
        retry_budget.clone(),
    ))
}

/// Whether this worker was spawned by the daemon and reaches the model through
/// it, over a `stdio-*` provider.
///
/// One predicate for the limiters, the retry count and the reference hand-off, so
/// the three cannot disagree about who is on the other end of stdout.
fn reaches_model_through_daemon(ai: &AiSettings) -> bool {
    ai.provider.starts_with("stdio-")
}

/// Forwards review progress to the daemon as JSON lines on stdout.
///
/// Only for a worker that [`reaches_model_through_daemon`]: that is the one
/// case with a reader on the other end of stdout that understands these lines.
/// `sashiko-cli local` parses the worker's whole stdout as one JSON document,
/// so a progress line there would corrupt its result.
///
/// Lines go through an unbounded channel to one writer task, so reporting
/// never blocks or reorders the review, and through the shared IPC writer, so
/// they cannot interleave with a concurrent model request.
struct DaemonProgress {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    writer: tokio::task::JoinHandle<()>,
}

impl DaemonProgress {
    /// A forwarder for a worker the daemon spawned, and `None` for any other:
    /// the one place the gate is decided.
    fn for_worker(ai: &AiSettings) -> Option<Self> {
        reaches_model_through_daemon(ai).then(Self::start)
    }

    fn start() -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let writer = tokio::spawn(async move {
            let ipc = crate::ai::ipc_writer();
            while let Some(line) = rx.recv().await {
                if let Err(e) = ipc.write_line(&line).await {
                    // Advisory: a lost progress line must not fail the review.
                    tracing::debug!("Could not report progress to the daemon: {e}");
                }
            }
        });
        Self { tx, writer }
    }

    fn send(&self, event: &ProgressEvent) {
        if let Some((patch_index, wire)) = to_wire(event)
            && let Some(line) = crate::activity::progress_line(patch_index, &wire)
        {
            let _ = self.tx.send(line);
        }
    }

    /// Waits until every line sent so far is written, so all progress precedes
    /// the final result line the caller prints next.
    async fn finish(self) {
        drop(self.tx);
        let _ = self.writer.await;
    }
}

/// The daemon's view of a progress event, or `None` for events it has no use
/// for. The patch index travels beside it, as in the reference hand-off.
fn to_wire(event: &ProgressEvent) -> Option<(i64, crate::activity::ProgressWire)> {
    use crate::activity::ProgressWire;
    let (patch_index, wire) = match event.clone() {
        ProgressEvent::AiReviewPreScreenStarted { patch_index } => {
            (patch_index, ProgressWire::PreScreen)
        }
        ProgressEvent::AiReviewPlanningStarted { patch_index } => {
            (patch_index, ProgressWire::Planning)
        }
        ProgressEvent::AiReviewPlanReady {
            patch_index,
            planned_stages,
        } => (patch_index, ProgressWire::PlanReady { planned_stages }),
        ProgressEvent::AiReviewStageStarted { patch_index, stage } => {
            (patch_index, ProgressWire::StageStarted { stage })
        }
        ProgressEvent::AiReviewStageTurn {
            patch_index,
            stage,
            turn,
            max_turns,
        } => (
            patch_index,
            ProgressWire::StageTurn {
                stage,
                turn,
                max_turns,
            },
        ),
        ProgressEvent::AiReviewStageTools {
            patch_index,
            stage,
            tools,
            turn,
            max_turns,
        } => (
            patch_index,
            ProgressWire::StageTools {
                stage,
                tools,
                turn,
                max_turns,
            },
        ),
        ProgressEvent::AiReviewStageBackoff {
            patch_index,
            stage,
            retry_in_seconds,
            turn,
            max_turns,
        } => (
            patch_index,
            ProgressWire::StageBackoff {
                stage,
                retry_in_seconds,
                turn,
                max_turns,
            },
        ),
        ProgressEvent::AiReviewStageFinished { patch_index, stage } => {
            (patch_index, ProgressWire::StageFinished { stage })
        }
        ProgressEvent::AiReviewStageFailed {
            patch_index,
            stage,
            reason,
            cause,
        } => (
            patch_index,
            ProgressWire::StageFailed {
                stage,
                reason,
                cause,
            },
        ),
        // Forwarded only on this path, which runs only for a daemon-spawned
        // worker: the daemon stores it for the review's live log.
        ProgressEvent::AiReviewLogEntry {
            patch_index,
            stage,
            message,
        } => (patch_index, ProgressWire::LogEntry { stage, message }),
        _ => return None,
    };
    Some((patch_index, wire))
}

/// The local view of one worker progress event, for the patch it belongs to.
///
/// Every field is carried across unchanged; only the patch index is added.
fn from_worker_progress(
    patch_index: i64,
    event: crate::worker::WorkerProgressEvent,
) -> ProgressEvent {
    use crate::worker::WorkerProgressEvent;
    match event {
        WorkerProgressEvent::PreScreenStarted => {
            ProgressEvent::AiReviewPreScreenStarted { patch_index }
        }
        WorkerProgressEvent::PlanningStarted => {
            ProgressEvent::AiReviewPlanningStarted { patch_index }
        }
        WorkerProgressEvent::ReviewStarted { planned_stages } => ProgressEvent::AiReviewPlanReady {
            patch_index,
            planned_stages,
        },
        WorkerProgressEvent::StageStarted { stage } => {
            ProgressEvent::AiReviewStageStarted { patch_index, stage }
        }
        WorkerProgressEvent::StageTurn {
            stage,
            turn,
            max_turns,
        } => ProgressEvent::AiReviewStageTurn {
            patch_index,
            stage,
            turn,
            max_turns,
        },
        WorkerProgressEvent::StageTools {
            stage,
            tools,
            turn,
            max_turns,
        } => ProgressEvent::AiReviewStageTools {
            patch_index,
            stage,
            tools,
            turn,
            max_turns,
        },
        WorkerProgressEvent::StageBackoff {
            stage,
            retry_in_seconds,
            turn,
            max_turns,
        } => ProgressEvent::AiReviewStageBackoff {
            patch_index,
            stage,
            retry_in_seconds,
            turn,
            max_turns,
        },
        WorkerProgressEvent::StageFinished { stage } => {
            ProgressEvent::AiReviewStageFinished { patch_index, stage }
        }
        WorkerProgressEvent::StageFailed {
            stage,
            reason,
            cause,
        } => ProgressEvent::AiReviewStageFailed {
            patch_index,
            stage,
            reason,
            cause,
        },
        WorkerProgressEvent::LogEntry { stage, message } => ProgressEvent::AiReviewLogEntry {
            patch_index,
            stage,
            message,
        },
    }
}

/// Daemon-spawned workers (`stdio-*`) already have their retry lifecycle managed
/// by the parent `Reviewer` via `review.max_retries`, so retrying the full
/// workflow inside the child process as well would multiply attempts.
fn worker_max_attempts(ai: &AiSettings) -> usize {
    if reaches_model_through_daemon(ai) {
        1
    } else {
        3
    }
}

/// Validates and extracts the inline review body from a worker result.
///
/// When findings are present without a non-empty `review_inline` string, this
/// returns an error so `review_single_patch` records `last_error` and fails the
/// attempt (allowing either the next in-process attempt or the parent daemon's
/// retry loop to run) instead of falling through and returning a successful
/// payload with a missing inline review.
fn extract_inline_review(patch_index: i64, output: Option<&Value>) -> Result<Option<String>> {
    let inline_content = output
        .and_then(|out| out.get("review_inline"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);

    let has_findings = output
        .and_then(|out| out.get("findings"))
        .and_then(|f| f.as_array())
        .is_some_and(|findings| !findings.is_empty());

    if has_findings && inline_content.is_none() {
        return Err(anyhow!(
            "Review failure on patch {}: Findings detected but review_inline field was missing or empty.",
            patch_index
        ));
    }

    Ok(inline_content)
}

/// The files a patch touches: the list handed to the tools as the active
/// patch files, and the wider list that decides which references attach.
///
/// The first is exactly what `git diff-tree` reports for the patch's commit,
/// so tools that favour the patch's own files behave as they always have. The
/// second adds the paths named in the patch text, because a patch reviewed
/// from the current tree without a commit has no entry in `patch_shas`, and a
/// scoped reference would otherwise go silently dormant for it.
async fn touched_files(
    worktree: &Path,
    sha: Option<&str>,
    diff: &str,
) -> (Vec<String>, Vec<String>) {
    let mut patch_files = Vec::new();
    if let Some(sha) = sha {
        // --no-renames, so a rename lists its old path as well as its new
        // one: a reference scoped to the tree a file left must still attach.
        // Plumbing does not detect renames by default today; the flag keeps
        // it that way whatever diff.renames says.
        let output = crate::git_cmd::in_dir_async(worktree)
            .args([
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "--no-renames",
                "-r",
                sha,
            ])
            .output()
            .await;
        if let Ok(out) = output
            && out.status.success()
        {
            let file_list = String::from_utf8_lossy(&out.stdout);
            for file in file_list.lines() {
                let trimmed = file.trim().to_string();
                if !trimmed.is_empty() {
                    patch_files.push(trimmed);
                }
            }
        }
    }
    let mut touched = patch_files.clone();
    for path in diff_paths(diff) {
        if !touched.contains(&path) {
            touched.push(path);
        }
    }
    (patch_files, touched)
}

/// The paths a unified diff names: both sides of every `diff --git` line,
/// file header, rename and copy, without the `a/` and `b/` prefixes and
/// without `/dev/null`.
///
/// The `diff --git` line matters because some changes have nothing else: a
/// binary change, a mode change, or an empty file added or deleted carries no
/// `---`/`+++` pair. Paths git has C-quoted (`"a/foo\tbar"`) are unquoted
/// before the prefix comes off, so they can match a glob like any other.
///
/// A `--- ` line counts only when a `+++ ` line follows it, because a removed
/// hunk line whose text starts with `-- ` looks the same on its own. The other
/// header lines need no such care: every hunk line starts with a space, `+`,
/// `-` or `\`, so they cannot be mistaken for one.
fn diff_paths(diff: &str) -> Vec<String> {
    fn strip_side(path: &str) -> &str {
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path)
    }

    fn header_path(rest: &str) -> Option<String> {
        let path = if rest.starts_with('"') {
            unquote_c_path(rest)?.0
        } else {
            // An unquoted header may carry a tab and a timestamp after the
            // path; a quoted one has any tab escaped inside the quotes.
            rest.split('\t')
                .next()
                .unwrap_or(rest)
                .trim_end()
                .to_string()
        };
        if path == "/dev/null" {
            return None;
        }
        Some(strip_side(&path).to_string())
    }

    let mut paths: Vec<String> = Vec::new();
    let mut push = |path: &str| {
        if !path.is_empty() && !paths.iter().any(|p| p == path) {
            paths.push(path.to_string());
        }
    };
    let lines: Vec<&str> = diff.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            for path in diff_git_paths(rest) {
                push(strip_side(&path));
            }
        } else if let Some(old) = line.strip_prefix("--- ")
            && let Some(new) = lines.get(i + 1).and_then(|l| l.strip_prefix("+++ "))
        {
            for side in [old, new] {
                if let Some(path) = header_path(side) {
                    push(&path);
                }
            }
        } else if let Some(rest) = ["rename from ", "rename to ", "copy from ", "copy to "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
        {
            let rest = rest.trim_end();
            match unquote_c_path(rest) {
                Some((path, _)) => push(&path),
                None => push(rest),
            }
        }
    }
    paths
}

/// Both paths of a `diff --git <old> <new>` line, still carrying their `a/`
/// and `b/` prefixes.
///
/// A quoted side is unambiguous. Two unquoted sides are split on a ` b/` that
/// leaves the same path on both sides, which is how git writes every change
/// that is not a rename or copy; failing that, a line with exactly one ` b/`
/// splits there. Anything else is left alone: a rename or copy has its own
/// `rename`/`copy` lines, and a guess could invent a path the patch never
/// touched.
fn diff_git_paths(rest: &str) -> Vec<String> {
    let rest = rest.trim_end();
    if let Some((old, after)) = unquote_c_path(rest) {
        let after = after.trim_start();
        let new = match unquote_c_path(after) {
            Some((new, _)) => new,
            None => after.to_string(),
        };
        return vec![old, new];
    }
    if let Some(quote) = rest.find(" \"b/") {
        let new = unquote_c_path(&rest[quote + 1..]).map(|(new, _)| new);
        return std::iter::once(rest[..quote].to_string())
            .chain(new)
            .collect();
    }
    let splits: Vec<usize> = rest.match_indices(" b/").map(|(i, _)| i).collect();
    let same = splits.iter().copied().find(|&i| {
        let (old, new) = (&rest[..i], &rest[i + 1..]);
        old.strip_prefix("a/")
            .is_some_and(|o| Some(o) == new.strip_prefix("b/"))
    });
    let split = same.or(match splits.as_slice() {
        [only] => Some(*only),
        _ => None,
    });
    match split {
        Some(i) => vec![rest[..i].to_string(), rest[i + 1..].to_string()],
        None => Vec::new(),
    }
}

/// Undoes git's C-style quoting of a path that starts with `"`, returning the
/// path and whatever follows the closing quote. `None` when the text is not a
/// well-formed quoted path.
///
/// Git escapes bytes, not characters, so octal escapes are gathered as bytes
/// and decoded as UTF-8 at the end; a path that is not UTF-8 is decoded
/// lossily, which cannot match a glob it would not have matched anyway.
fn unquote_c_path(text: &str) -> Option<(String, &str)> {
    let body = text.strip_prefix('"')?;
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((String::from_utf8_lossy(&out).into_owned(), &body[i + 1..])),
            b'\\' => {
                let esc = *bytes.get(i + 1)?;
                i += 2;
                out.push(match esc {
                    b'a' => 0x07,
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'v' => 0x0b,
                    b'\\' | b'"' => esc,
                    b'0'..=b'3' => {
                        let digits = bytes.get(i..i + 2)?;
                        if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                            return None;
                        }
                        i += 2;
                        ((esc - b'0') << 6) | ((digits[0] - b'0') << 3) | (digits[1] - b'0')
                    }
                    _ => return None,
                });
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
async fn review_single_patch(
    worktree: &GitWorktree,
    settings: &Settings,
    patchset_id: i64,
    subject: &str,
    p: &PatchInput,
    all_patches: &[PatchInput],
    rich_patches: &[Value],
    patch_shas: &HashMap<i64, String>,
    options: &WorkerOptions,
    baseline_sha: &str,
    llm_semaphore: &Arc<Semaphore>,
    quota: &Arc<crate::ai::quota::QuotaManager>,
    timeout_seconds: u64,
    references: &[crate::toolbox::ReferenceRepo],
    progress: Option<&ProgressCallback<'_>>,
) -> (Result<Value>, Option<ReviewReferences>) {
    // The AI section and the stage list as this process resolved them, command
    // line included.
    let ai = &settings.ai;
    let retry_budget: Option<Arc<dyn crate::ai::backoff_provider::RetryBudget>> =
        (timeout_seconds > 0).then(|| {
            // Salvage is zero because no review is given a salvage window
            // yet. This budget also governs the worker the daemon spawns, so
            // once the daemon grants salvage_seconds this deadline must be
            // given the same value. Otherwise the worker's own retries stop
            // at the soft limit while the daemon still waits for it.
            let deadline = Arc::new(crate::ai::backoff_provider::ReviewDeadline::new(
                std::time::Duration::from_secs(timeout_seconds),
                std::time::Duration::ZERO,
            ));
            Arc::new(crate::ai::backoff_provider::DeadlineBudget::new(deadline))
                as Arc<dyn crate::ai::backoff_provider::RetryBudget>
        });
    let max_attempts = worker_max_attempts(ai);

    // Gathered once per patch rather than once per attempt: neither the files
    // nor which references they attach depend on the attempt, and logging the
    // decision once keeps it readable.
    let (patch_files, touched) = touched_files(
        &worktree.path,
        patch_shas.get(&p.index).map(String::as_str),
        &p.diff,
    )
    .await;
    info!(
        "Active patch files gathered for patch {}: {:?}",
        p.index, patch_files
    );
    let attach = crate::references::attach_for_patch(references, &touched);
    if let Some(line) = attach.summary(p.index) {
        info!("{line}");
    }

    // Tell the daemon now which references this patch has, rather than only
    // in the final output: the deployment's logs last about an hour, and a
    // review that times out or is cancelled is killed before it writes a final
    // output, as is a worker that crashes, yet each still has to record which
    // trees it was given. Only
    // the daemon reads these lines -- `sashiko-cli local` parses the worker's
    // whole stdout as one JSON document -- and they go through the shared IPC
    // writer so they cannot interleave with a concurrent model request.
    if let Some(line) = early_reference_handoff(ai, p.index, &attach) {
        let sent = match line {
            Ok(line) => crate::ai::ipc_writer().write_line(&line).await,
            Err(e) => Err(e),
        };
        if let Err(e) = sent {
            warn!(
                "Could not report patch {}'s references to the daemon: {e}",
                p.index
            );
        }
    }

    // Summed over every attempt, in configured order of the attached
    // references, so the count describes everything the review read.
    let mut reads = vec![0u64; attach.attached.len()];
    let result = review_patch_attempts(
        &attach,
        &mut reads,
        AttemptInputs {
            worktree,
            settings,
            patchset_id,
            subject,
            p,
            all_patches,
            rich_patches,
            patch_shas,
            options,
            baseline_sha,
            llm_semaphore,
            quota,
            retry_budget: &retry_budget,
            max_attempts,
            patch_files: &patch_files,
            progress,
        },
    )
    .await;
    // Reported for a failed review too: which trees it had is as much a part
    // of its record as of a successful one.
    (result, attach.record(Some(&reads)))
}

/// The line that tells the daemon which references a patch's review has, or
/// `None` when nothing should be sent.
///
/// Apart from [`review_single_patch`] so the decision can be tested without a
/// model: only a daemon-spawned worker has a reader on the other end of stdout,
/// and a deployment with no references has nothing to record.
fn early_reference_handoff(
    ai: &AiSettings,
    patch_index: i64,
    attach: &crate::references::PatchReferences,
) -> Option<Result<String>> {
    if !reaches_model_through_daemon(ai) {
        return None;
    }
    let rec = attach.record(None)?;
    Some(crate::references::handoff_line(patch_index, &rec))
}

/// Runs `attempt` until one succeeds or `max_attempts` have failed, adding
/// each attempt's reference reads into `reads` whatever its outcome.
///
/// Each attempt returns its outcome and its reads; an outer `Err` is fatal and
/// ends the loop at once without a further attempt, and is returned as the
/// outer `Err` here too, so the caller can tell it from a review whose every
/// attempt failed. Apart from the review itself so the accounting can be
/// tested without a model: an attempt that failed or was retried still read
/// the trees, and counting only the successful one would understate what the
/// review saw.
async fn attempts_counting_reads<F, Fut>(
    max_attempts: usize,
    reads: &mut [u64],
    mut attempt: F,
) -> Result<Result<Value>>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Result<(Result<Value>, Vec<u64>)>>,
{
    let mut last_error = None;
    for n in 1..=max_attempts {
        let (outcome, attempt_reads) = attempt(n).await?;
        for (total, read) in reads.iter_mut().zip(attempt_reads) {
            *total += read;
        }
        match outcome {
            Ok(value) => return Ok(Ok(value)),
            Err(e) => last_error = Some(e),
        }
    }
    Ok(Err(
        last_error.unwrap_or_else(|| anyhow!("Patch review failed"))
    ))
}

/// What [`review_patch_attempts`] needs from [`review_single_patch`].
struct AttemptInputs<'a, 'p> {
    worktree: &'a GitWorktree,
    settings: &'a Settings,
    patchset_id: i64,
    subject: &'a str,
    p: &'a PatchInput,
    all_patches: &'a [PatchInput],
    rich_patches: &'a [Value],
    patch_shas: &'a HashMap<i64, String>,
    options: &'a WorkerOptions,
    baseline_sha: &'a str,
    llm_semaphore: &'a Arc<Semaphore>,
    quota: &'a Arc<crate::ai::quota::QuotaManager>,
    retry_budget: &'a Option<Arc<dyn crate::ai::backoff_provider::RetryBudget>>,
    max_attempts: usize,
    patch_files: &'a [String],
    progress: Option<&'a ProgressCallback<'p>>,
}

/// Runs the review attempts for one patch, adding each attempt's reference
/// reads into `reads` whatever its outcome.
async fn review_patch_attempts(
    attach: &crate::references::PatchReferences,
    reads: &mut [u64],
    inputs: AttemptInputs<'_, '_>,
) -> Result<Value> {
    let AttemptInputs {
        worktree,
        settings,
        patchset_id,
        subject,
        p,
        all_patches,
        rich_patches,
        patch_shas,
        options,
        baseline_sha,
        llm_semaphore,
        quota,
        retry_budget,
        max_attempts,
        patch_files,
        progress,
    } = inputs;
    let ai = &settings.ai;
    let stages = &settings.review.stages;

    let result = attempts_counting_reads(max_attempts, reads, |attempt| async move {
        emit(
            progress,
            ProgressEvent::AiReviewAttempt {
                patch_index: p.index,
                attempt,
                max_attempts,
            },
        );

        if attempt > 1 {
            info!(
                "Restarting AI review for patch {} (attempt {}/{})...",
                p.index, attempt, max_attempts
            );
        }

        // No database, so the cache goes where the prompt bundle already lives.
        let provider = crate::ai::create_provider_cached(ai, None)
            .await
            .context("Failed to create AI provider")?;
        let provider = decorate_provider(provider, ai, llm_semaphore, quota, retry_budget);
        // The directory itself: read_prompt resolves a name against it.
        let prompts_tool_path = Some(options.prompts.clone());

        let mut tools = ToolBox::new(worktree.path.clone(), prompts_tool_path)
            .with_references(attach.attached.iter().cloned())
            .with_dormant_references(attach.dormant.iter().cloned());
        tools.set_active_patch_files(patch_files.to_vec());

        if let Some(sha) = patch_shas.get(&p.index) {
            info!("Setting virtual HEAD to {} for patch {}", sha, p.index);
            tools.set_virtual_head(sha.clone());
        }

        let prompts = PromptRegistry::new(options.prompts.clone());
        let series_range = calculate_series_range(
            all_patches,
            std::slice::from_ref(p),
            patch_shas,
            baseline_sha,
        );

        let tools = Arc::new(tools);
        let mut worker = Worker::new(
            provider,
            tools.clone(),
            prompts,
            WorkerConfig {
                project: options.project,
                max_input_tokens: ai.max_input_tokens,
                max_interactions: ai.max_interactions,
                temperature: ai.temperature,
                custom_prompt: options.custom_prompt.clone(),
                series_range,
                baseline_sha: Some(baseline_sha.to_string()),
                stages: stages.clone(),
                skip_report: options.agent,
                report_preexisting: options.report_preexisting,
                effort: settings.effective_effort(),
            },
        );

        let p_index = p.index;
        let progress_cb = progress.map(|cb| move |event| cb(from_worker_progress(p_index, event)));

        let patchset_val = json!({
            "id": patchset_id,
            "subject": subject,
            "patches": rich_patches,
            "patch_index": Some(p.index),
            "baseline": baseline_sha
        });

        let outcome = worker
            .run(
                patchset_val,
                progress_cb
                    .as_ref()
                    .map(|f| f as &(dyn Fn(_) + Send + Sync)),
            )
            .await;
        // Taken before the outcome is looked at, so an attempt that fails or
        // is retried still counts what it read.
        let attempt_reads = tools.reference_reads();
        let outcome = match outcome {
            Ok(result) => {
                let inline_content = if options.agent {
                    None
                } else {
                    match extract_inline_review(p.index, result.output.as_ref()) {
                        Ok(content) => content,
                        Err(e) => {
                            error!("{}", e);
                            return Ok((Err(e), attempt_reads));
                        }
                    }
                };

                info!("AI review completed for patch {}.", p.index);
                emit(
                    progress,
                    ProgressEvent::AiReviewFinished {
                        patch_index: p.index,
                    },
                );

                Ok(json!({
                    "patch_index": p.index,
                    "review": result.output,
                    "error": result.error,
                    "inline_review": inline_content,
                    "input_context": result.input_context,
                    "history": result.history,
                    "tokens_in": result.tokens_in,
                    "tokens_out": result.tokens_out,
                    "tokens_cached": result.tokens_cached
                }))
            }
            Err(e) => {
                error!(
                    "AI review for patch {} failed with exception: {}",
                    p.index, e
                );
                Err(e)
            }
        };
        Ok((outcome, attempt_reads))
    })
    .await?;

    if result.is_err() {
        emit(
            progress,
            ProgressEvent::AiReviewFailed {
                patch_index: p.index,
            },
        );
    }
    result
}

/// Lists the failed patches in series order.
fn combined_review_error(mut errors: Vec<(i64, String)>) -> String {
    errors.sort_by_key(|(patch_index, _)| *patch_index);
    errors
        .iter()
        .map(|(patch_index, err)| format!("patch {patch_index}: {err}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Records a patch whose review failed, so the report does not present it as
/// reviewed.
fn mark_patch_review_incomplete(patches: &mut [Value], patch_index: i64, error: &str) {
    if let Some(patch) = patches
        .iter_mut()
        .find(|patch| patch["index"].as_i64() == Some(patch_index))
        && let Some(fields) = patch.as_object_mut()
    {
        fields.insert("review_status".into(), json!("incomplete"));
        fields.insert("review_error".into(), json!(error));
    }
}

/// Records which references a patch's review had, with their read counts, on
/// the patch's entry, where the daemon picks it up for the review row.
fn record_patch_references(patches: &mut [Value], patch_index: i64, refs: &ReviewReferences) {
    if let Some(patch) = patches
        .iter_mut()
        .find(|patch| patch["index"].as_i64() == Some(patch_index))
        && let Some(fields) = patch.as_object_mut()
    {
        fields.insert("reference_repos".into(), json!(refs));
    }
}

/// Offsets `log_start` and `log_end` on each stage in `review_map` by
/// `history_offset` so stage transcript bounds align with `combined_history`.
fn offset_review_map_log_bounds(map: &Value, history_offset: usize) -> Value {
    let offset = u64::try_from(history_offset).unwrap_or(u64::MAX);
    if offset == 0 {
        return map.clone();
    }
    let mut adjusted = map.clone();
    if let Some(stages) = adjusted.get_mut("stages").and_then(Value::as_array_mut) {
        for stage in stages {
            if let Some(start) = stage.get("log_start").and_then(Value::as_u64) {
                stage["log_start"] = json!(start.saturating_add(offset));
            }
            if let Some(end) = stage.get("log_end").and_then(Value::as_u64) {
                stage["log_end"] = json!(end.saturating_add(offset));
            }
        }
    }
    adjusted
}

fn attach_patch_review_map(patches: &mut [Value], patch_index: i64, review_map: Value) {
    if let Some(patch) = patches
        .iter_mut()
        .find(|patch| patch["index"].as_i64() == Some(patch_index))
        && let Some(fields) = patch.as_object_mut()
    {
        fields.insert("review_map".into(), review_map);
    }
}

/// Appends a single patch's inline review to `combined_inline`, prefixing a
/// patch header banner when multiple patches are reviewed and skipping empty
/// or clean (`"No issues found."`) inline blocks.
fn append_patch_inline_review(
    combined_inline: &mut String,
    patch_index: i64,
    patch_subject: &str,
    total_patches: usize,
    inline_review: Option<&str>,
) {
    if let Some(inline) = inline_review
        && !inline.trim().is_empty()
        && inline.trim() != "No issues found."
    {
        if !combined_inline.is_empty() {
            combined_inline.push_str("\n\n");
        }
        if total_patches > 1 {
            combined_inline.push_str(&format!(
                "--- Patch [{}]: {} ---\n",
                patch_index, patch_subject
            ));
        }
        combined_inline.push_str(inline.trim());
    }
}

/// Assembles the combined review payload for a review.
///
/// Pre-existing concerns are preserved in the payload so that daemon-spawned
/// worker reviews can hand them to the standalone Linux bug pipeline for
/// verification, deduplication, and database tracking.
fn build_review_output(
    summary: String,
    findings: Vec<Value>,
    concerns: Vec<Value>,
    dismissed_concerns: Vec<Value>,
    concerns_count: u64,
    dismissed_concerns_count: u64,
    review_map: Option<Value>,
) -> Value {
    let mut out = json!({
        "summary": summary,
        "findings": findings,
        "concerns": concerns,
        "dismissed_concerns": dismissed_concerns,
        "concerns_count": concerns_count,
        "dismissed_concerns_count": dismissed_concerns_count
    });
    if let Some(map) = review_map
        && let Some(obj) = out.as_object_mut()
    {
        obj.insert("review_map".to_string(), map);
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn run_worker_in_worktree(
    worktree: &GitWorktree,
    settings: &Settings,
    concurrency: usize,
    timeout_seconds: u64,
    patchset_id: i64,
    subject: String,
    patches: Vec<PatchInput>,
    baseline_arg: &str,
    baseline_sha: &str,
    options: &WorkerOptions,
    references: Vec<crate::toolbox::ReferenceRepo>,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<Value> {
    info!("Worktree at {:?}", worktree.path);
    info!("Found {} patches total", patches.len());

    let mut patch_results = Vec::new();
    let mut patch_shas = HashMap::new();
    let mut patch_shows = HashMap::new();
    let mut patch_messages = HashMap::new();

    let all_applied = if options.current_tree {
        for p in &patches {
            if let Some(sha) = &p.commit_id {
                patch_shas.insert(p.index, sha.clone());
                if let Ok(show) = worktree.get_commit_show(sha).await {
                    patch_shows.insert(p.index, show);
                }
                if let Ok(msg) = worktree.get_commit_message(sha).await {
                    patch_messages.insert(p.index, msg);
                }
            }
            patch_results.push(json!({
                "index": p.index,
                "status": "present",
                "method": "current-tree",
                "subject": p.subject.clone()
            }));
        }
        true
    } else if let Some(commit_hash) = &options.review_commit {
        info!("Directly reviewing commit {}", commit_hash);
        for p in &patches {
            if let Some(sha) = &p.commit_id {
                patch_shas.insert(p.index, sha.clone());
            }
        }
        if let Some(idx) = options.review_patch_index {
            patch_shas.insert(idx, commit_hash.clone());
            if let Ok(show) = worktree.get_commit_show(commit_hash).await {
                patch_shows.insert(idx, show);
            }
            let subject = patches
                .iter()
                .find(|p| p.index == idx)
                .and_then(|p| p.subject.clone());
            patch_results.push(json!({
                "index": idx,
                "status": "applied",
                "method": "pre-applied",
                "subject": subject
            }));
        }
        true
    } else {
        info!(
            "Applying all {} patches to validate series...",
            patches.len()
        );
        let mut applied = true;

        for p in &patches {
            emit(
                progress,
                ProgressEvent::ApplyingPatch {
                    index: p.index,
                    total: patches.len(),
                    subject: p.subject.clone().unwrap_or_else(|| "patch".to_string()),
                },
            );

            let success = apply_single_patch(
                worktree,
                p,
                &mut patch_shas,
                &mut patch_shows,
                &mut patch_messages,
                &mut patch_results,
            )
            .await;

            if success {
                emit(progress, ProgressEvent::PatchApplied { index: p.index });
            } else {
                applied = false;
                let error = patch_results
                    .last()
                    .and_then(|p| p.get("error"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("patch application failed")
                    .to_string();
                emit(
                    progress,
                    ProgressEvent::PatchFailed {
                        index: p.index,
                        error,
                    },
                );
            }
        }
        applied
    };

    let mut patches_to_review: Vec<PatchInput> =
        if let Some(target_idx) = options.review_patch_index {
            patches
                .iter()
                .filter(|p| p.index == target_idx)
                .cloned()
                .collect()
        } else {
            patches.clone()
        };

    if settings.ai.no_ai {
        info!("Skipping AI review due to --no-ai flag.");
        patches_to_review.clear();
    }

    if !all_applied {
        info!("Not all patches applied successfully. Skipping AI review.");
        return Ok(json!({
            "patchset_id": patchset_id,
            "baseline": baseline_arg,
            "patches": patch_results,
            "error": "Patch application failed"
        }));
    }

    if patches_to_review.is_empty() {
        info!("No patches matched review index or list empty. Skipping AI review.");
        return Ok(json!({
            "patchset_id": patchset_id,
            "baseline": baseline_arg,
            "patches": patch_results,
            "review": null,
            "input_context": "",
            "tokens_in": 0,
            "tokens_out": 0,
            "tokens_cached": 0
        }));
    }

    info!(
        "Patches applied. Starting AI reviews for {} patches...",
        patches_to_review.len()
    );
    emit(
        progress,
        ProgressEvent::AiReviewStarted {
            patches: patches_to_review.len(),
        },
    );

    let rich_patches: Vec<Value> = patches
        .iter()
        .map(|p| {
            let date_str = if let Some(ts) = p.date {
                use chrono::{TimeZone, Utc};
                Utc.timestamp_opt(ts, 0)
                    .single()
                    .map(|dt| dt.to_rfc2822())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            json!({
                "index": p.index,
                "subject": p.subject,
                "author": p.author,
                "date_string": date_str,
                "diff": p.diff,
                "commit_id": patch_shas.get(&p.index).cloned().or_else(|| p.commit_id.clone()),
                "git_show": patch_shows.get(&p.index).cloned(),
                "commit_message_full": patch_messages.get(&p.index).cloned()
            })
        })
        .collect();

    // Cap in-flight model calls across the whole run. The patch fan-out below
    // is bounded by `concurrency`; each patch then fans its stages out
    // concurrently on top of that, so without a shared ceiling the number of
    // simultaneous requests is unbounded.
    let llm_semaphore = Arc::new(Semaphore::new(
        crate::ai::concurrency_limited_provider::llm_permits(concurrency),
    ));
    // Shared so a rate-limit response from one request backs the whole run off.
    let quota = Arc::new(crate::ai::quota::QuotaManager::new());
    // Execute patch reviews concurrently with a limit
    let futures_stream = futures::stream::iter(patches_to_review.iter().map(|p| {
        let rich_patches = rich_patches.clone();
        let patch_shas = &patch_shas;
        let options = &options;
        let references = references.as_slice();
        let subject_clone = subject.clone();
        let all_patches = &patches;
        let llm_semaphore = &llm_semaphore;
        let quota = &quota;
        async move {
            let result = review_single_patch(
                worktree,
                settings,
                patchset_id,
                &subject_clone,
                p,
                all_patches,
                &rich_patches,
                patch_shas,
                options,
                baseline_sha,
                llm_semaphore,
                quota,
                timeout_seconds,
                references,
                progress,
            )
            .await;
            let (result, refs) = result;
            (p.index, result, refs)
        }
    }));

    // A failed patch must not discard the reviews of the other patches, so
    // collect every result and report the failures alongside them.
    let mut buffered = futures_stream.buffer_unordered(concurrency);
    let mut results = Vec::new();
    let mut review_errors = Vec::new();
    let mut failed_outcomes = Vec::new();
    while let Some((patch_index, result, refs)) = buffered.next().await {
        if let Some(refs) = refs {
            record_patch_references(&mut patch_results, patch_index, &refs);
        }
        match result {
            Ok(res) => results.push(res),
            Err(err) => {
                if let Some(wf) = err.downcast_ref::<crate::workflow::WorkflowFailure>() {
                    failed_outcomes.push((patch_index, wf.outcome.clone()));
                }
                let err = format!("{:?}", err);
                mark_patch_review_incomplete(&mut patch_results, patch_index, &err);
                review_errors.push((patch_index, err));
            }
        }
    }
    results.sort_by_key(|res| res["patch_index"].as_i64().unwrap_or(0));
    failed_outcomes.sort_by_key(|(idx, _)| *idx);

    // Aggregate findings, inline reviews, history, input context, and concern counts
    let mut combined_summary = String::new();
    let mut combined_findings = Vec::new();
    let mut combined_concerns = Vec::new();
    let mut combined_dismissed_concerns = Vec::new();
    let mut combined_inline = String::new();
    let mut combined_history = Vec::new();
    let mut combined_input_context = String::new();
    let mut single_review_map = None;
    let mut total_tokens_in: u64 = 0;
    let mut total_tokens_out: u64 = 0;
    let mut total_tokens_cached: u64 = 0;
    let mut total_concerns_count: u64 = 0;
    let mut total_dismissed_concerns_count: u64 = 0;

    for (_idx, failed) in failed_outcomes {
        total_tokens_in = total_tokens_in.saturating_add(u64::from(failed.tokens_in));
        total_tokens_out = total_tokens_out.saturating_add(u64::from(failed.tokens_out));
        total_tokens_cached = total_tokens_cached.saturating_add(u64::from(failed.tokens_cached));
        for msg in failed.history {
            if let Ok(val) = serde_json::to_value(msg) {
                combined_history.push(val);
            }
        }
    }
    let mut combined_stage_failures = Vec::new();

    for res in results {
        let p_idx = res["patch_index"].as_i64().unwrap_or(0);
        let history_offset = combined_history.len();
        let patch_subject = patches_to_review
            .iter()
            .find(|p| p.index == p_idx)
            .and_then(|p| p.subject.as_ref())
            .cloned()
            .unwrap_or_default();

        if let Some(review) = res.get("review") {
            if let Some(summary) = review.get("summary").and_then(|v| v.as_str())
                && !summary.trim().is_empty()
            {
                if !combined_summary.is_empty() {
                    combined_summary.push_str("\n\n");
                }
                if patches_to_review.len() > 1 {
                    combined_summary.push_str(&format!("Patch [{}]: {}", p_idx, summary.trim()));
                } else {
                    combined_summary.push_str(summary.trim());
                }
            }
            if let Some(findings) = review.get("findings").and_then(|v| v.as_array()) {
                for f in findings {
                    let mut finding_val = f.clone();
                    finding_val["patch_index"] = json!(p_idx);
                    finding_val["patch_subject"] = json!(patch_subject);
                    combined_findings.push(finding_val);
                }
            }
            if let Some(concerns) = review.get("concerns").and_then(|v| v.as_array()) {
                for c in concerns {
                    let mut concern_val = c.clone();
                    concern_val["patch_index"] = json!(p_idx);
                    concern_val["patch_subject"] = json!(patch_subject);
                    combined_concerns.push(concern_val);
                }
            }
            if let Some(dismissed) = review.get("dismissed_concerns").and_then(|v| v.as_array()) {
                combined_dismissed_concerns.extend(dismissed.clone());
            }
            // Carried through rather than dropped with the rest of the
            // per-patch output: a review that lost a stage must not reach its
            // reader as one with full coverage.
            if let Some(failures) = review.get("stage_failures").and_then(|v| v.as_array()) {
                for f in failures {
                    let mut failure = f.clone();
                    failure["patch_index"] = json!(p_idx);
                    combined_stage_failures.push(failure);
                }
            }

            if let Some(cc) = review.get("concerns_count").and_then(|v| v.as_u64()) {
                total_concerns_count = total_concerns_count.saturating_add(cc);
            }
            if let Some(dcc) = review
                .get("dismissed_concerns_count")
                .and_then(|v| v.as_u64())
            {
                total_dismissed_concerns_count = total_dismissed_concerns_count.saturating_add(dcc);
            }
            if let Some(map) = review.get("review_map").filter(|v| !v.is_null()) {
                let adjusted_map = offset_review_map_log_bounds(map, history_offset);
                attach_patch_review_map(&mut patch_results, p_idx, adjusted_map.clone());
                if patches_to_review.len() == 1 {
                    single_review_map = Some(adjusted_map);
                }
            }
        }

        append_patch_inline_review(
            &mut combined_inline,
            p_idx,
            &patch_subject,
            patches_to_review.len(),
            res["inline_review"].as_str(),
        );

        if let Some(hist) = res.get("history").and_then(|h| h.as_array()) {
            combined_history.extend(hist.clone());
        }

        if let Some(inp) = res["input_context"].as_str()
            && !inp.is_empty()
        {
            if !combined_input_context.is_empty() {
                combined_input_context.push_str("\n\n");
            }
            combined_input_context.push_str(inp);
        }

        total_tokens_in = total_tokens_in.saturating_add(res["tokens_in"].as_u64().unwrap_or(0));
        total_tokens_out = total_tokens_out.saturating_add(res["tokens_out"].as_u64().unwrap_or(0));
        total_tokens_cached =
            total_tokens_cached.saturating_add(res["tokens_cached"].as_u64().unwrap_or(0));
    }

    let mut review_output = build_review_output(
        combined_summary,
        combined_findings,
        combined_concerns,
        combined_dismissed_concerns,
        total_concerns_count,
        total_dismissed_concerns_count,
        single_review_map,
    );
    if !combined_stage_failures.is_empty() {
        review_output["stage_failures"] = json!(combined_stage_failures);
    }

    let mut combined_result = json!({
        "patchset_id": patchset_id,
        "baseline": baseline_arg,
        "patches": patch_results,
        "review": review_output,
        "inline_review": if options.agent {
            String::new()
        } else if combined_inline.is_empty() && review_errors.is_empty() {
            "No issues found.".to_string()
        } else {
            combined_inline
        },
        "history": combined_history,
        "input_context": combined_input_context,
        "tokens_in": total_tokens_in,
        "tokens_out": total_tokens_out,
        "tokens_cached": total_tokens_cached
    });

    if !review_errors.is_empty()
        && let Some(fields) = combined_result.as_object_mut()
    {
        fields.insert("partial".into(), json!(true));
        fields.insert("error".into(), json!(combined_review_error(review_errors)));
    }

    Ok(combined_result)
}

pub async fn run_worker_from_stdin(options: WorkerOptions, settings: &Settings) -> Result<Value> {
    let mut buffer = String::new();
    if std::io::stdin().read_line(&mut buffer)? == 0 {
        return Err(anyhow!("No input provided on stdin"));
    }
    let input: ReviewInput = serde_json::from_str(&buffer)?;
    run_worker(input, options, settings, Some(&progress_to_stderr)).await
}

/// The line prefix `progress_to_stderr` writes and `sashiko-cli local` keys
/// on when it streams the worker's stderr.
pub const PROGRESS_LINE_PREFIX: &str = "progress: ";

/// Renders the AI-phase events as plain lines on stderr, one per event, for
/// the worker subprocess. Its parent owns the terminal and decides what to
/// show; without these lines the whole AI phase is silent from outside, and a
/// slow review is indistinguishable from a hung one.
pub fn progress_to_stderr(event: ProgressEvent) {
    if let Some(line) = progress_line(event) {
        eprintln!("{PROGRESS_LINE_PREFIX}{line}");
    }
}

/// The text after the prefix for an AI-phase event; `None` for the events
/// the worker's log lines already cover.
pub fn progress_line(event: ProgressEvent) -> Option<String> {
    let line = match event {
        ProgressEvent::AiReviewStarted { patches } => format!(
            "reviewing {} patch{}",
            patches,
            if patches == 1 { "" } else { "es" }
        ),
        ProgressEvent::AiReviewPreScreenStarted { patch_index } => {
            format!("patch {patch_index}: pre-screen")
        }
        ProgressEvent::AiReviewPlanningStarted { patch_index } => {
            format!("patch {patch_index}: planning")
        }
        ProgressEvent::AiReviewPlanReady {
            patch_index,
            planned_stages,
        } => format!("patch {patch_index}: stages {}", planned_stages.join(", ")),
        ProgressEvent::AiReviewStageStarted { patch_index, stage } => {
            format!("patch {patch_index}: {stage} started")
        }
        ProgressEvent::AiReviewStageTurn {
            patch_index,
            stage,
            turn,
            max_turns,
        } => format!("patch {patch_index}: {stage} turn {turn}/{max_turns}"),
        ProgressEvent::AiReviewStageFinished { patch_index, stage } => {
            format!("patch {patch_index}: {stage} finished")
        }
        // Without the reason, deliberately. `sashiko-cli local` treats any
        // line containing " turn " as a turn tick, and a provider's error text
        // can contain those words; the reason reaches the daemon and the
        // review's record by other routes. The cause's label is fixed text
        // with no such words, and says whether to look for a fault.
        ProgressEvent::AiReviewStageFailed {
            patch_index,
            stage,
            cause,
            ..
        } => {
            format!("patch {patch_index}: {stage} {}", cause.label())
        }
        ProgressEvent::AiReviewAttempt {
            patch_index,
            attempt,
            max_attempts,
        } if attempt > 1 => format!("patch {patch_index}: retry {attempt}/{max_attempts}"),
        ProgressEvent::AiReviewFinished { patch_index } => format!("patch {patch_index}: done"),
        ProgressEvent::AiReviewFailed { patch_index } => format!("patch {patch_index}: incomplete"),
        // Never a line. These feed `sashiko-cli local`, which treats any line
        // containing " turn " as a turn tick, and a conversation is full of
        // such words; it is also transcript text, which has no business in a
        // progress stream.
        ProgressEvent::AiReviewLogEntry { .. } => return None,
        _ => return None,
    };
    Some(line)
}

pub fn result_has_error(result: &Value) -> bool {
    result
        .get("error")
        .and_then(|e| e.as_str())
        .map(|e| !e.is_empty())
        .unwrap_or(false)
}

pub fn result_has_high_or_critical_findings(result: &Value) -> bool {
    let Some(findings) = result
        .get("review")
        .and_then(|review| review.get("findings"))
        .and_then(|f| f.as_array())
    else {
        return false;
    };

    findings.iter().any(|finding| {
        let is_new = !finding
            .get("preexisting")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let severity = finding
            .get("severity")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        is_new && matches!(severity.as_str(), "critical" | "high")
    })
}

/// Formats a local review result as a concise, machine-friendly JSON payload for
/// `--agent` mode, omitting internal LLM conversation history, raw prompt context,
/// dismissed concerns, and human-formatted inline reports.
pub fn format_agent_review_output(result: &Value) -> Value {
    let findings = result
        .get("review")
        .and_then(|r| r.get("findings"))
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    let concerns = result
        .get("review")
        .and_then(|r| r.get("concerns"))
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let status = if result_has_error(result) {
        "error"
    } else if !findings.is_empty() {
        "issues_found"
    } else {
        "clean"
    };

    let patches: Vec<Value> = result
        .get("patches")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|p| {
                    let mut p_val = p.clone();
                    if let Some(map) = p_val.as_object_mut() {
                        map.remove("review_map");
                    }
                    p_val
                })
                .collect()
        })
        .unwrap_or_default();

    let mut out = json!({
        "status": status,
        "baseline": result.get("baseline").cloned().unwrap_or(Value::Null),
        "patches": patches,
        "findings": findings,
        "concerns": concerns,
        "tokens_in": result.get("tokens_in").and_then(|v| v.as_u64()).unwrap_or(0),
        "tokens_out": result.get("tokens_out").and_then(|v| v.as_u64()).unwrap_or(0),
        "tokens_cached": result.get("tokens_cached").and_then(|v| v.as_u64()).unwrap_or(0),
    });

    if let Some(obj) = out.as_object_mut() {
        if let Some(partial) = result.get("partial") {
            obj.insert("partial".into(), partial.clone());
        }
        if let Some(err) = result.get("error")
            && !err.is_null()
        {
            obj.insert("error".into(), err.clone());
        }
    }

    out
}

async fn apply_single_patch(
    worktree: &GitWorktree,
    p: &PatchInput,
    patch_shas: &mut HashMap<i64, String>,
    patch_shows: &mut HashMap<i64, String>,
    patch_messages: &mut HashMap<i64, String>,
    patch_results: &mut Vec<Value>,
) -> bool {
    if let Some(sha) = &p.commit_id {
        info!(
            "Patch {} is identified by commit ID {}, attempting direct checkout...",
            p.index, sha
        );
        return checkout_patch(
            worktree,
            p,
            sha,
            "checkout",
            patch_shas,
            patch_shows,
            patch_messages,
            patch_results,
        )
        .await;
    }

    if let Some(sha) = &p.message_id
        && sha.len() == 40
        && sha.chars().all(|c| c.is_ascii_hexdigit())
    {
        info!(
            "Patch {} message_id looks like a SHA {}, checking out...",
            p.index, sha
        );
        return checkout_patch(
            worktree,
            p,
            sha,
            "checkout",
            patch_shas,
            patch_shows,
            patch_messages,
            patch_results,
        )
        .await;
    }

    if let (Some(author), Some(subject)) = (&p.author, &p.subject) {
        let date_str = if let Some(ts) = p.date {
            use chrono::{TimeZone, Utc};
            Utc.timestamp_opt(ts, 0)
                .single()
                .map(|dt| dt.to_rfc2822())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let mbox = format!(
            "From: {}\nDate: {}\nSubject: {}\n\n{}\n",
            author, date_str, subject, p.diff
        );

        match worktree.apply_patch(&mbox).await {
            Ok(_) => {
                if let Ok(sha) = get_commit_hash(&worktree.path, "HEAD").await {
                    patch_shas.insert(p.index, sha.clone());
                    if let Ok(show) = worktree.get_commit_show(&sha).await {
                        patch_shows.insert(p.index, show);
                    }
                    if let Ok(msg) = worktree.get_commit_message(&sha).await {
                        patch_messages.insert(p.index, msg);
                    }
                }
                patch_results.push(json!({
                    "index": p.index,
                    "status": "applied",
                    "method": "git-am",
                    "subject": p.subject.clone()
                }));
                return true;
            }
            Err(e) => {
                error!("git am failed: {}", e);
                patch_results.push(json!({
                    "index": p.index,
                    "status": "error",
                    "method": "git-am",
                    "subject": p.subject.clone(),
                    "error": e.to_string()
                }));
                return false;
            }
        }
    }

    patch_results.push(json!({
        "index": p.index,
        "status": "error",
        "method": "unknown",
        "error": "Missing author or subject for am apply"
    }));
    false
}

#[allow(clippy::too_many_arguments)]
async fn checkout_patch(
    worktree: &GitWorktree,
    p: &PatchInput,
    sha: &str,
    method: &str,
    patch_shas: &mut HashMap<i64, String>,
    patch_shows: &mut HashMap<i64, String>,
    patch_messages: &mut HashMap<i64, String>,
    patch_results: &mut Vec<Value>,
) -> bool {
    match worktree.reset_hard(sha).await {
        Ok(_) => {
            if let Ok(show) = worktree.get_commit_show(sha).await {
                patch_shows.insert(p.index, show);
            }
            if let Ok(msg) = worktree.get_commit_message(sha).await {
                patch_messages.insert(p.index, msg);
            }
            patch_shas.insert(p.index, sha.to_string());
            patch_results.push(json!({
                "index": p.index,
                "status": "applied",
                "method": method,
                "subject": p.subject.clone()
            }));
            true
        }
        Err(e) => {
            error!("Failed to checkout commit {}: {}", sha, e);
            patch_results.push(json!({
                "index": p.index,
                "status": "error",
                "method": method,
                "subject": p.subject.clone(),
                "error": e.to_string()
            }));
            false
        }
    }
}

fn emit(progress: Option<&ProgressCallback<'_>>, event: ProgressEvent) {
    if let Some(progress) = progress {
        progress(event);
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}

pub fn print_worker_json(result: &Value) -> Result<()> {
    println!("{}", serde_json::to_string(result)?);
    std::io::stdout().flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::sync::Arc;

    /// A provider that does nothing; the decoration tests only care about
    /// whether it was wrapped, which `Arc::ptr_eq` answers directly.
    struct StubProvider;

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for StubProvider {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> Result<crate::ai::AiResponse> {
            unreachable!("decoration tests never issue a request")
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "stub".into(),
                context_window_size: 1000,
            }
        }
    }

    fn stub() -> Arc<dyn crate::ai::AiProvider> {
        Arc::new(StubProvider)
    }

    /// Fails the first call with a rate limit, then succeeds, so a test can
    /// tell whether the retry limiter was actually installed.
    struct RateLimitOnce {
        calls: std::sync::atomic::AtomicU32,
    }

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for RateLimitOnce {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> Result<crate::ai::AiResponse> {
            use std::sync::atomic::Ordering;
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(crate::ai::gemini::GeminiError::QuotaExceeded(
                    std::time::Duration::from_millis(5),
                )
                .into());
            }
            Ok(crate::ai::AiResponse {
                content: Some("ok".into()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "rate-limit-once".into(),
                context_window_size: 1000,
            }
        }
    }

    fn dummy_request() -> crate::ai::AiRequest {
        crate::ai::AiRequest {
            system: None,
            messages: vec![crate::ai::AiMessage {
                role: crate::ai::AiRole::User,
                content: Some("hi".into()),
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
        }
    }

    #[tokio::test]
    async fn test_decorate_provider_retries_rate_limits_for_in_process_reviews() -> Result<()> {
        use std::sync::atomic::{AtomicU32, Ordering};
        let mut settings = Settings::new()?;
        settings.ai.log_turns = false;
        let sem = Arc::new(Semaphore::new(4));
        let quota = Arc::new(crate::ai::quota::QuotaManager::new());

        // In-process: the limiter waits out the window and retries, so the
        // caller sees a success rather than the rate-limit error.
        settings.ai.provider = "claude-cli".to_string();
        let inner = Arc::new(RateLimitOnce {
            calls: AtomicU32::new(0),
        });
        let decorated = decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &None);
        let response = decorated.generate_content(dummy_request()).await?;
        assert_eq!(response.content.as_deref(), Some("ok"));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);

        // Daemon-spawned worker: the daemon owns the retry, so the error is
        // passed straight back instead of being retried here as well.
        settings.ai.provider = "stdio-claude".to_string();
        let inner = Arc::new(RateLimitOnce {
            calls: AtomicU32::new(0),
        });
        let decorated = decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &None);
        assert!(decorated.generate_content(dummy_request()).await.is_err());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_decorate_provider_honours_the_retry_budget() -> Result<()> {
        use std::sync::atomic::{AtomicU32, Ordering};

        /// Stands in for a review whose deadline has already passed.
        struct Expired;
        impl crate::ai::backoff_provider::RetryBudget for Expired {
            fn credit_wait(&self, _slept: std::time::Duration) {}
            fn check(&self) -> Result<()> {
                Err(anyhow!("deadline exceeded"))
            }
        }

        let mut settings = Settings::new()?;
        settings.ai.log_turns = false;
        settings.ai.provider = "claude-cli".to_string();
        let sem = Arc::new(Semaphore::new(4));
        let quota = Arc::new(crate::ai::quota::QuotaManager::new());
        let budget: Option<Arc<dyn crate::ai::backoff_provider::RetryBudget>> =
            Some(Arc::new(Expired));

        let inner = Arc::new(RateLimitOnce {
            calls: AtomicU32::new(0),
        });
        let decorated = decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &budget);
        assert!(decorated.generate_content(dummy_request()).await.is_err());
        // The budget is consulted before the request goes out, so a review that
        // is already past its deadline stops rather than retrying through it.
        assert_eq!(inner.calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[test]
    fn test_decorate_provider_log_turns_gate() -> Result<()> {
        let mut settings = Settings::new()?;
        // A stdio provider skips the limiters, isolating the logging decision.
        settings.ai.provider = "stdio-gemini".to_string();
        let sem = Arc::new(Semaphore::new(1));
        let quota = Arc::new(crate::ai::quota::QuotaManager::new());

        // Off: the provider is handed back untouched, so a review that does not
        // ask for turn logging pays nothing for it.
        settings.ai.log_turns = false;
        let inner = stub();
        assert!(Arc::ptr_eq(
            &inner,
            &decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &None)
        ));

        // On: wrapped, so the turns are logged.
        settings.ai.log_turns = true;
        let inner = stub();
        assert!(!Arc::ptr_eq(
            &inner,
            &decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &None)
        ));
        Ok(())
    }

    #[test]
    fn test_decorate_provider_skips_limiters_for_stdio_workers() -> Result<()> {
        let mut settings = Settings::new()?;
        settings.ai.log_turns = false;
        let sem = Arc::new(Semaphore::new(1));
        let quota = Arc::new(crate::ai::quota::QuotaManager::new());

        // A daemon-spawned worker is throttled by the daemon, so it must be
        // left unwrapped rather than limited twice.
        settings.ai.provider = "stdio-claude".to_string();
        let inner = stub();
        assert!(Arc::ptr_eq(
            &inner,
            &decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &None)
        ));

        // A review running in-process has nothing in front of it, so it gets
        // the limiter.
        settings.ai.provider = "claude-cli".to_string();
        let inner = stub();
        assert!(!Arc::ptr_eq(
            &inner,
            &decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &None)
        ));
        Ok(())
    }

    #[test]
    fn daemon_worker_gate() -> Result<()> {
        let mut settings = Settings::new()?;
        for (provider, through_daemon) in [
            ("stdio-claude", true),
            ("stdio-gemini", true),
            ("claude", false),
            ("gemini", false),
        ] {
            settings.ai.provider = provider.to_string();
            assert_eq!(
                reaches_model_through_daemon(&settings.ai),
                through_daemon,
                "{provider}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_worker_max_attempts_skips_inner_retries_for_stdio_workers() -> Result<()> {
        let mut settings = Settings::new()?;

        settings.ai.provider = "stdio-gemini".to_string();
        assert_eq!(worker_max_attempts(&settings.ai), 1);

        settings.ai.provider = "gemini".to_string();
        assert_eq!(worker_max_attempts(&settings.ai), 3);
        Ok(())
    }

    #[test]
    fn test_extract_inline_review_errors_when_findings_lack_inline_text() {
        let missing_inline = json!({
            "findings": [{"problem": "bug"}]
        });
        assert!(extract_inline_review(1, Some(&missing_inline)).is_err());

        let empty_inline = json!({
            "findings": [{"problem": "bug"}],
            "review_inline": "   "
        });
        assert!(extract_inline_review(1, Some(&empty_inline)).is_err());

        let valid_inline = json!({
            "findings": [{"problem": "bug"}],
            "review_inline": "> +foo();\n\nBug here."
        });
        assert_eq!(
            extract_inline_review(1, Some(&valid_inline))
                .unwrap()
                .as_deref(),
            Some("> +foo();\n\nBug here.")
        );

        let no_findings = json!({
            "findings": []
        });
        assert_eq!(extract_inline_review(1, Some(&no_findings)).unwrap(), None);
    }

    /// Counts the requests it was asked to forget.
    struct ForgetCounter {
        forgotten: Arc<std::sync::atomic::AtomicU32>,
    }

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for ForgetCounter {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> Result<crate::ai::AiResponse> {
            unreachable!("this test only forgets")
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "forget-counter".into(),
                context_window_size: 1000,
            }
        }
        async fn forget(&self, _request: &crate::ai::AiRequest) {
            self.forgotten
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn test_decorated_provider_passes_forget_to_the_cache() -> Result<()> {
        // The response cache sits innermost, under every wrapper added here.
        // forget defaults to doing nothing, so a wrapper that does not pass
        // it on would silently let a retry replay a rejected answer again.
        let mut settings = Settings::new()?;
        settings.ai.provider = "gemini".to_string();
        settings.ai.log_turns = true;
        let sem = Arc::new(Semaphore::new(1));
        let quota = Arc::new(crate::ai::quota::QuotaManager::new());
        let forgotten = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let inner: Arc<dyn crate::ai::AiProvider> = Arc::new(ForgetCounter {
            forgotten: forgotten.clone(),
        });

        let decorated = decorate_provider(inner, &settings.ai, &sem, &quota, &None);
        decorated.forget(&dummy_request()).await;

        assert_eq!(
            forgotten.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "forget did not reach the innermost provider"
        );
        Ok(())
    }

    fn git(repo_path: &Path, args: &[&str]) -> Result<()> {
        let output = crate::git_cmd::in_dir(repo_path).args(args).output()?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    async fn test_repo() -> Result<(tempfile::TempDir, PathBuf, String, String)> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        git(&repo_path, &["init"])?;
        git(&repo_path, &["config", "user.email", "test@example.com"])?;
        git(&repo_path, &["config", "user.name", "Test User"])?;

        let file_path = repo_path.join("file.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "Initial")?;
        git(&repo_path, &["add", "."])?;
        git(&repo_path, &["commit", "-m", "Initial"])?;
        let initial_sha = get_commit_hash(&repo_path, "HEAD").await?;

        writeln!(file, "Change")?;
        git(&repo_path, &["add", "."])?;
        git(&repo_path, &["commit", "-m", "Feature"])?;
        let feature_sha = get_commit_hash(&repo_path, "HEAD").await?;

        Ok((temp_dir, repo_path, initial_sha, feature_sha))
    }

    #[tokio::test]
    async fn test_build_review_input_from_git_single_commit() -> Result<()> {
        let (_temp, repo_path, _initial_sha, feature_sha) = test_repo().await?;
        let (input, shas) = build_review_input_from_git(&repo_path, &feature_sha, None).await?;

        assert_eq!(shas, vec![feature_sha.clone()]);
        assert_eq!(input.id, 0);
        assert_eq!(input.patches.len(), 1);
        assert_eq!(input.patches[0].index, 1);
        assert_eq!(
            input.patches[0].commit_id.as_deref(),
            Some(feature_sha.as_str())
        );
        assert_eq!(input.patches[0].subject.as_deref(), Some("Feature"));

        Ok(())
    }

    #[tokio::test]
    async fn test_apply_single_patch_remote_checkout() -> Result<()> {
        let (_temp, repo_path, initial_sha, feature_sha) = test_repo().await?;
        let worktree = GitWorktree::new(&repo_path, &initial_sha, None).await?;

        let patch = PatchInput {
            index: 1,
            diff: "INVALID DIFF content that would fail git apply".to_string(),
            subject: Some("Feature".to_string()),
            author: Some("Test User <test@example.com>".to_string()),
            date: None,
            message_id: Some("some-msg-id".to_string()),
            commit_id: Some(feature_sha),
        };

        let mut patch_shas = HashMap::new();
        let mut patch_shows = HashMap::new();
        let mut patch_messages = HashMap::new();
        let mut patch_results = Vec::new();

        let success = apply_single_patch(
            &worktree,
            &patch,
            &mut patch_shas,
            &mut patch_shows,
            &mut patch_messages,
            &mut patch_results,
        )
        .await;

        assert!(success);
        assert_eq!(patch_results[0]["status"], "applied");
        assert_eq!(patch_results[0]["method"], "checkout");
        assert!(std::fs::read_to_string(worktree.path.join("file.txt"))?.contains("Change"));

        Ok(())
    }

    #[tokio::test]
    async fn test_apply_single_patch_checkout_failure() -> Result<()> {
        let (_temp, repo_path, initial_sha, _feature_sha) = test_repo().await?;
        let worktree = GitWorktree::new(&repo_path, &initial_sha, None).await?;

        let patch = PatchInput {
            index: 1,
            diff: "Valid Diff content that would apply if we fell back".to_string(),
            subject: Some("Feature".to_string()),
            author: Some("Test User <test@example.com>".to_string()),
            date: None,
            message_id: Some("some-msg-id".to_string()),
            commit_id: Some("0000000000000000000000000000000000000000".to_string()),
        };

        let mut patch_shas = HashMap::new();
        let mut patch_shows = HashMap::new();
        let mut patch_messages = HashMap::new();
        let mut patch_results = Vec::new();

        let success = apply_single_patch(
            &worktree,
            &patch,
            &mut patch_shas,
            &mut patch_shows,
            &mut patch_messages,
            &mut patch_results,
        )
        .await;

        assert!(!success);
        assert_eq!(patch_results[0]["status"], "error");
        assert_eq!(patch_results[0]["method"], "checkout");

        Ok(())
    }

    #[tokio::test]
    async fn test_apply_single_patch_legacy_message_id_sha() -> Result<()> {
        let (_temp, repo_path, initial_sha, feature_sha) = test_repo().await?;
        let worktree = GitWorktree::new(&repo_path, &initial_sha, None).await?;

        let patch = PatchInput {
            index: 1,
            diff: "INVALID".to_string(),
            subject: Some("Feature".to_string()),
            author: Some("Test User <test@example.com>".to_string()),
            date: None,
            message_id: Some(feature_sha),
            commit_id: None,
        };

        let mut patch_shas = HashMap::new();
        let mut patch_shows = HashMap::new();
        let mut patch_messages = HashMap::new();
        let mut patch_results = Vec::new();

        let success = apply_single_patch(
            &worktree,
            &patch,
            &mut patch_shas,
            &mut patch_shows,
            &mut patch_messages,
            &mut patch_results,
        )
        .await;

        assert!(success);
        assert_eq!(patch_results[0]["status"], "applied");
        assert_eq!(patch_results[0]["method"], "checkout");
        assert!(std::fs::read_to_string(worktree.path.join("file.txt"))?.contains("Change"));

        Ok(())
    }

    #[test]
    fn test_series_range_in_local_review() {
        let p1 = PatchInput {
            index: 1,
            diff: "diff1".to_string(),
            subject: Some("Patch 1".to_string()),
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "diff2".to_string(),
            subject: Some("Patch 2".to_string()),
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let all_patches = vec![p1.clone(), p2.clone()];
        let mut patch_shas = HashMap::new();
        patch_shas.insert(1, "sha1".to_string());
        patch_shas.insert(2, "sha2".to_string());

        let range_p1 = calculate_series_range(
            &all_patches,
            std::slice::from_ref(&p1),
            &patch_shas,
            "baseline_sha",
        );
        assert_eq!(range_p1, Some("baseline_sha..sha2".to_string()));

        let range_p2 = calculate_series_range(
            &all_patches,
            std::slice::from_ref(&p2),
            &patch_shas,
            "baseline_sha",
        );
        assert_eq!(range_p2, None);
    }

    #[test]
    fn test_rich_patches_and_follow_up_context_with_patch_index_filter() {
        use crate::worker::build_follow_up_series_context;

        let p1 = PatchInput {
            index: 1,
            diff: "diff1".to_string(),
            subject: Some("Patch 1".to_string()),
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "diff2".to_string(),
            subject: Some("Patch 2".to_string()),
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let all_patches = vec![p1.clone(), p2.clone()];
        let mut patch_shas = HashMap::new();
        patch_shas.insert(1, "sha1".to_string());
        patch_shas.insert(2, "sha2".to_string());

        // When reviewing only patch 1 with index filter:
        let rich_patches: Vec<Value> = all_patches
            .iter()
            .map(|p| {
                json!({
                    "index": p.index,
                    "subject": p.subject,
                    "author": p.author,
                    "commit_id": patch_shas.get(&p.index).cloned(),
                })
            })
            .collect();

        let patchset_val = json!({
            "id": 100,
            "subject": "Series Subject",
            "patches": rich_patches,
            "patch_index": Some(1),
            "baseline": "baseline_sha"
        });

        let range = calculate_series_range(
            &all_patches,
            std::slice::from_ref(&p1),
            &patch_shas,
            "baseline_sha",
        );
        assert_eq!(range, Some("baseline_sha..sha2".to_string()));

        let context = build_follow_up_series_context(range.as_deref(), &patchset_val, "sha1");
        assert!(context.is_some());
        let ctx_str = context.unwrap();
        assert!(ctx_str.contains("Current Patch Under Review: [Patch 1 of 2] - Patch 1"));
        assert!(ctx_str.contains("Series End Commit (Final State): sha2"));
        assert!(ctx_str.contains("- [Patch 2 of 2] (commit sha2): Patch 2"));
    }

    #[test]
    fn test_inline_review_aggregation_single_and_multi_patch() {
        let mut combined_single = String::new();
        append_patch_inline_review(
            &mut combined_single,
            1,
            "[PATCH 1/1] test single",
            1,
            Some("> +int x;\n+Use unsigned int instead."),
        );
        assert_eq!(combined_single, "> +int x;\n+Use unsigned int instead.");
        assert!(!combined_single.contains("--- Patch [1]:"));

        // Clean ("No issues found.") and whitespace-only reviews are omitted.
        let mut combined_clean = String::new();
        append_patch_inline_review(&mut combined_clean, 1, "subj", 2, Some("No issues found."));
        append_patch_inline_review(&mut combined_clean, 2, "subj", 2, Some("   \n  "));
        assert!(combined_clean.is_empty());

        let mut combined_multi = String::new();
        append_patch_inline_review(
            &mut combined_multi,
            1,
            "[PATCH 1/2] test first",
            2,
            Some("  Comment on patch 1\n"),
        );
        append_patch_inline_review(
            &mut combined_multi,
            2,
            "[PATCH 2/2] test second",
            2,
            Some("Comment on patch 2"),
        );
        assert_eq!(
            combined_multi,
            "--- Patch [1]: [PATCH 1/2] test first ---\nComment on patch 1\n\n\
             --- Patch [2]: [PATCH 2/2] test second ---\nComment on patch 2"
        );
    }

    #[test]
    fn test_local_review_output_preserves_preexisting_concerns() {
        let single_map = json!({
            "version": 1,
            "project": "sashiko",
            "prompts": {"base": ["review-core.md"], "pre_screen": [], "stage_guides": {}, "tool_read": []},
            "stages": [{"name": "pre-screen", "log_start": 0, "log_end": 2}],
            "raw_concerns": [{"id": "C1"}],
            "raw_dismissed_concerns": [],
            "verification": {"findings": [], "hard_cases": [], "dismissed_concerns": []},
            "post_verification": {"findings": [], "dismissed_concerns": []},
            "threads": [{"thread_id": "T1"}],
        });
        let output = build_review_output(
            "Adds dev-queue routing heuristic.".to_string(),
            vec![json!({"problem": "new regression"})],
            vec![json!({"problem": "preexisting bug"})],
            vec![],
            3,
            0,
            Some(offset_review_map_log_bounds(&single_map, 0)),
        );

        // Pre-existing candidates, summary, and review_map must be preserved so daemon-spawned
        // worker reviews store them in ai_interactions.output.
        assert_eq!(output["summary"], "Adds dev-queue routing heuristic.");
        assert_eq!(output["concerns"].as_array().unwrap().len(), 1);
        assert_eq!(output["concerns_count"], 3);
        assert_eq!(output["findings"].as_array().unwrap().len(), 1);
        assert_eq!(output["review_map"], single_map);

        let mut patches = vec![json!({"index": 1}), json!({"index": 2})];
        let adjusted = offset_review_map_log_bounds(&single_map, 4);
        attach_patch_review_map(&mut patches, 2, adjusted);
        assert_eq!(patches[1]["review_map"]["project"], "sashiko");
        assert_eq!(patches[1]["review_map"]["raw_concerns"][0]["id"], "C1");
        assert_eq!(patches[1]["review_map"]["stages"][0]["log_start"], 4);
        assert_eq!(patches[1]["review_map"]["stages"][0]["log_end"], 6);
    }

    #[test]
    fn combined_review_error_orders_patches_numerically() {
        let errors = vec![
            (10, "timed out".to_string()),
            (2, "output truncated".to_string()),
        ];
        assert_eq!(
            combined_review_error(errors),
            "patch 2: output truncated; patch 10: timed out"
        );
    }

    #[test]
    fn patch_status_marks_only_incomplete_reviews() {
        let mut patches = vec![json!({"index": 1}), json!({"index": 2})];
        mark_patch_review_incomplete(&mut patches, 2, "output truncated");
        assert!(patches[0].get("review_status").is_none());
        assert_eq!(patches[1]["review_status"], "incomplete");
        assert_eq!(patches[1]["review_error"], "output truncated");
    }

    /// The kernel for every patch and rdma-core for none of this test's, so a
    /// record has both an attached and a dormant entry.
    fn kernel_and_dormant_rdma() -> crate::references::PatchReferences {
        let reference = |name: &str, kind, applies_to| crate::toolbox::ReferenceRepo {
            name: name.to_string(),
            kind,
            path: std::path::PathBuf::from(format!("/srv/{name}")),
            revision: "v6.18".to_string(),
            commit: "0".repeat(40),
            description: None,
            applies_to,
        };
        let refs = [
            reference("kernel", crate::settings::ReferenceKind::Kernel, None),
            reference(
                "rdma-core",
                crate::settings::ReferenceKind::Source,
                Some(
                    crate::references::AppliesTo::new("rdma-core", &["rdma/drv/**".to_string()])
                        .unwrap(),
                ),
            ),
        ];
        crate::references::attach_for_patch(&refs, &["eth/ionic/ionic_lif.c".to_string()])
    }

    #[test]
    fn early_handoff_is_sent_only_by_a_daemon_worker_with_references() -> Result<()> {
        let mut settings = Settings::new()?;
        let attach = kernel_and_dormant_rdma();

        settings.ai.provider = "stdio-claude".to_string();
        let line = early_reference_handoff(&settings.ai, 3, &attach)
            .expect("a daemon worker reports its references")?;
        let msg: Value = serde_json::from_str(&line)?;
        let (index, rec) =
            crate::references::parse_handoff(&msg).expect("the daemon accepts the hand-off");
        assert_eq!(index, 3);
        assert_eq!(rec.attached.len(), 1);
        assert_eq!(rec.attached[0].name, "kernel");
        assert_eq!(rec.attached[0].reads, None, "no reads yet at hand-off");
        assert_eq!(rec.dormant.len(), 1);
        assert_eq!(rec.dormant[0].name, "rdma-core");

        // Nobody reads a CLI worker's stdout line by line, and its whole
        // stdout must stay one JSON document.
        settings.ai.provider = "claude-cli".to_string();
        assert!(early_reference_handoff(&settings.ai, 3, &attach).is_none());

        // Without references there is nothing to record.
        settings.ai.provider = "stdio-claude".to_string();
        let none = crate::references::PatchReferences::default();
        assert!(early_reference_handoff(&settings.ai, 3, &none).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn reads_of_failed_attempts_are_counted() {
        // Fails once, then succeeds: both attempts' reads are in the total.
        let mut reads = vec![0u64; 2];
        let result = attempts_counting_reads(3, &mut reads, |attempt| async move {
            Ok(if attempt == 1 {
                (Err(anyhow!("model gave up")), vec![2, 1])
            } else {
                (Ok(json!({"attempt": attempt})), vec![3, 0])
            })
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap()["attempt"], 2);
        assert_eq!(reads, [5, 1]);

        // Every attempt fails: still counted, and the last error is returned.
        let mut reads = vec![0u64; 1];
        let result = attempts_counting_reads(2, &mut reads, |attempt| async move {
            Ok((Err(anyhow!("attempt {attempt} failed")), vec![4]))
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap_err().to_string(), "attempt 2 failed");
        assert_eq!(reads, [8]);

        // A fatal error stops at once, keeping what earlier attempts read.
        let mut reads = vec![0u64; 1];
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let calls = &calls;
        let result = attempts_counting_reads(3, &mut reads, |attempt| async move {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt == 2 {
                return Err(anyhow!("no provider"));
            }
            Ok((Err(anyhow!("retry")), vec![1]))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(reads, [1]);
    }

    #[test]
    fn patch_results_carry_reference_repos() {
        let refs: ReviewReferences = serde_json::from_value(json!({
            "attached": [{
                "name": "kernel",
                "kind": "kernel",
                "revision": "v6.18",
                "commit": "0".repeat(40),
                "why": "always",
                "reads": 4,
            }],
        }))
        .unwrap();
        let mut patches = vec![json!({"index": 1}), json!({"index": 2})];
        record_patch_references(&mut patches, 2, &refs);
        assert!(patches[0].get("reference_repos").is_none());
        assert_eq!(patches[1]["reference_repos"]["attached"][0]["reads"], 4);
        assert_eq!(
            crate::references::parse_record(&patches[1]["reference_repos"]),
            Some(refs),
            "what the worker writes is what the daemon reads back"
        );
    }

    #[test]
    fn test_worker_progress_lines_are_what_the_cli_keys_on() {
        // sashiko-cli local strips PROGRESS_LINE_PREFIX and treats a line
        // containing " turn " as a tick to overwrite rather than print.
        let turn = progress_line(ProgressEvent::AiReviewStageTurn {
            patch_index: 2,
            stage: "security".to_string(),
            turn: 3,
            max_turns: 20,
        })
        .unwrap();
        assert_eq!(turn, "patch 2: security turn 3/20");
        assert!(turn.contains(" turn "));

        for (event, expected) in [
            (
                ProgressEvent::AiReviewStageStarted {
                    patch_index: 1,
                    stage: "locking".to_string(),
                },
                "patch 1: locking started",
            ),
            (
                ProgressEvent::AiReviewStageFinished {
                    patch_index: 1,
                    stage: "locking".to_string(),
                },
                "patch 1: locking finished",
            ),
            (
                ProgressEvent::AiReviewAttempt {
                    patch_index: 1,
                    attempt: 2,
                    max_attempts: 3,
                },
                "patch 1: retry 2/3",
            ),
            (
                ProgressEvent::AiReviewFailed { patch_index: 1 },
                "patch 1: incomplete",
            ),
            // The reason stays off this line: provider error text can say
            // anything, " turn " included, and would then read as a tick.
            (
                ProgressEvent::AiReviewStageFailed {
                    patch_index: 1,
                    stage: "locking".to_string(),
                    reason: "Session exceeded max turns limit; last turn 20".to_string(),
                    cause: crate::workflow::StageStopCause::Error,
                },
                "patch 1: locking failed",
            ),
        ] {
            let line = progress_line(event).unwrap();
            assert_eq!(line, expected);
            assert!(!line.contains(" turn "), "a non-tick line reads as a tick");
        }

        // A stage that stopped for any other reason says why, and no label can
        // read as a tick either.
        use crate::workflow::StageStopCause;
        for (cause, expected) in [
            (StageStopCause::Cancelled, "patch 1: locking cancelled"),
            (
                StageStopCause::OutOfTime,
                "patch 1: locking stopped: out of time",
            ),
            (StageStopCause::CutShort, "patch 1: locking cut short"),
            (
                StageStopCause::Interrupted,
                "patch 1: locking still running when the review stopped",
            ),
        ] {
            let line = progress_line(ProgressEvent::AiReviewStageFailed {
                patch_index: 1,
                stage: "locking".to_string(),
                reason: "a turn of phrase".to_string(),
                cause,
            })
            .unwrap();
            assert_eq!(line, expected);
            assert!(!line.contains(" turn "), "a non-tick line reads as a tick");
        }

        // A first attempt and the pre-AI events are covered by the worker's
        // log lines and print nothing here.
        assert!(
            progress_line(ProgressEvent::AiReviewAttempt {
                patch_index: 1,
                attempt: 1,
                max_attempts: 3,
            })
            .is_none()
        );
        assert!(progress_line(ProgressEvent::PatchApplied { index: 1 }).is_none());
        assert!(PROGRESS_LINE_PREFIX.ends_with(' '));

        // What a turn is waiting on is for the daemon; on this channel it
        // would only be noise between the ticks.
        assert!(
            progress_line(ProgressEvent::AiReviewStageTools {
                patch_index: 1,
                stage: "locking".to_string(),
                tools: vec!["git_grep".to_string()],
                turn: 2,
                max_turns: 20,
            })
            .is_none()
        );
        assert!(
            progress_line(ProgressEvent::AiReviewStageBackoff {
                patch_index: 1,
                stage: "locking".to_string(),
                retry_in_seconds: Some(5),
                turn: 2,
                max_turns: 20,
            })
            .is_none()
        );

        // The conversation never reaches this channel: it is transcript text,
        // and "the next turn " in a model's answer would read as a tick.
        assert!(
            progress_line(ProgressEvent::AiReviewLogEntry {
                patch_index: 1,
                stage: Some("locking".to_string()),
                message: crate::ai::AiMessage {
                    role: crate::ai::AiRole::Assistant,
                    content: Some("on the next turn I will grep".to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            })
            .is_none()
        );
    }

    /// Every review progress event reaches the daemon, through the same encode
    /// and decode the two processes use, with the patch index it belongs to.
    /// Everything else stays on this side.
    #[test]
    fn test_review_progress_reaches_the_daemon_and_nothing_else_does() {
        use crate::activity::{ProgressWire, parse_progress, progress_line as wire_line};

        let stage = || "locking".to_string();
        let log_message = || crate::ai::AiMessage {
            role: crate::ai::AiRole::Tool,
            content: Some("git_grep output".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: Some("c0".to_string()),
        };
        let forwarded = [
            (
                ProgressEvent::AiReviewPreScreenStarted { patch_index: 3 },
                ProgressWire::PreScreen,
            ),
            (
                ProgressEvent::AiReviewPlanningStarted { patch_index: 3 },
                ProgressWire::Planning,
            ),
            (
                ProgressEvent::AiReviewPlanReady {
                    patch_index: 3,
                    planned_stages: vec![stage()],
                },
                ProgressWire::PlanReady {
                    planned_stages: vec![stage()],
                },
            ),
            (
                ProgressEvent::AiReviewStageStarted {
                    patch_index: 3,
                    stage: stage(),
                },
                ProgressWire::StageStarted { stage: stage() },
            ),
            (
                ProgressEvent::AiReviewStageTurn {
                    patch_index: 3,
                    stage: stage(),
                    turn: 2,
                    max_turns: 20,
                },
                ProgressWire::StageTurn {
                    stage: stage(),
                    turn: 2,
                    max_turns: 20,
                },
            ),
            (
                ProgressEvent::AiReviewStageTools {
                    patch_index: 3,
                    stage: stage(),
                    tools: vec!["git_grep".to_string()],
                    turn: 2,
                    max_turns: 20,
                },
                ProgressWire::StageTools {
                    stage: stage(),
                    tools: vec!["git_grep".to_string()],
                    turn: 2,
                    max_turns: 20,
                },
            ),
            (
                ProgressEvent::AiReviewStageBackoff {
                    patch_index: 3,
                    stage: stage(),
                    retry_in_seconds: None,
                    turn: 2,
                    max_turns: 20,
                },
                ProgressWire::StageBackoff {
                    stage: stage(),
                    retry_in_seconds: None,
                    turn: 2,
                    max_turns: 20,
                },
            ),
            (
                ProgressEvent::AiReviewStageFinished {
                    patch_index: 3,
                    stage: stage(),
                },
                ProgressWire::StageFinished { stage: stage() },
            ),
            (
                // A cause other than the default, so the hop cannot pass by
                // writing `Error` whatever it was given.
                ProgressEvent::AiReviewStageFailed {
                    patch_index: 3,
                    stage: stage(),
                    reason: "boom".to_string(),
                    cause: crate::workflow::StageStopCause::OutOfTime,
                },
                ProgressWire::StageFailed {
                    stage: stage(),
                    reason: "boom".to_string(),
                    cause: crate::workflow::StageStopCause::OutOfTime,
                },
            ),
            (
                ProgressEvent::AiReviewLogEntry {
                    patch_index: 3,
                    stage: Some(stage()),
                    message: log_message(),
                },
                ProgressWire::LogEntry {
                    stage: Some(stage()),
                    message: log_message(),
                },
            ),
        ];
        for (event, expected) in forwarded {
            let (idx, wire) = to_wire(&event).expect("review progress is forwarded");
            let line = wire_line(idx, &wire).unwrap();
            let decoded = parse_progress(&serde_json::from_str(&line).unwrap());
            assert_eq!(decoded, Some((3, expected)), "{line}");
        }

        for event in [
            ProgressEvent::AiReviewStarted { patches: 1 },
            ProgressEvent::AiReviewAttempt {
                patch_index: 3,
                attempt: 2,
                max_attempts: 3,
            },
            ProgressEvent::AiReviewFinished { patch_index: 3 },
            ProgressEvent::PatchApplied { index: 3 },
            ProgressEvent::ReviewComplete { partial: false },
        ] {
            assert!(to_wire(&event).is_none(), "{event:?}");
        }
    }

    /// A stopped stage's cause crosses from the worker's event to the local
    /// one as it was given, together with the patch it belongs to.
    #[test]
    fn test_a_stage_stop_cause_survives_the_worker_hop() {
        use crate::workflow::StageStopCause;
        for cause in [
            StageStopCause::Error,
            StageStopCause::Cancelled,
            StageStopCause::OutOfTime,
            StageStopCause::CutShort,
            StageStopCause::Interrupted,
        ] {
            let event = from_worker_progress(
                4,
                crate::worker::WorkerProgressEvent::StageFailed {
                    stage: "locking".to_string(),
                    reason: "boom".to_string(),
                    cause,
                },
            );
            match event {
                ProgressEvent::AiReviewStageFailed {
                    patch_index,
                    stage,
                    reason,
                    cause: carried,
                } => {
                    assert_eq!(patch_index, 4);
                    assert_eq!(stage, "locking");
                    assert_eq!(reason, "boom");
                    assert_eq!(carried, cause);
                }
                other => panic!("a stage failure must stay one, got {other:?}"),
            }
        }
    }

    /// `sashiko-cli local` parses the worker's whole stdout as one JSON
    /// document, so only a worker the daemon spawned may write progress there.
    #[tokio::test]
    async fn test_only_a_daemon_spawned_worker_forwards_progress() -> Result<()> {
        let mut settings = Settings::new()?;
        for (provider, forwards) in [
            ("stdio-claude", true),
            ("stdio-gemini", true),
            ("claude", false),
            ("gemini", false),
            ("claude-cli", false),
        ] {
            settings.ai.provider = provider.to_string();
            let daemon = DaemonProgress::for_worker(&settings.ai);
            assert_eq!(daemon.is_some(), forwards, "{provider}");
            if let Some(daemon) = daemon {
                // Nothing was sent, so nothing is written.
                daemon.finish().await;
            }
        }
        Ok(())
    }

    #[test]
    fn test_format_agent_review_output_clean_and_issues_and_error() {
        let clean = json!({
            "patchset_id": 0,
            "baseline": "abc1234",
            "patches": [{"index": 1, "status": "applied", "sha": "def5678", "subject": "feat: x"}],
            "review": {
                "summary": "",
                "findings": [],
                "concerns": [{"type": "Note", "description": "pre-existing", "preexisting": true}],
                "dismissed_concerns": [{"type": "Noise"}],
            },
            "inline_review": "",
            "history": [{"role": "user", "content": "huge prompt"}],
            "input_context": "Multi-stage execution completed",
            "tokens_in": 1200,
            "tokens_out": 300,
            "tokens_cached": 800
        });
        let formatted_clean = format_agent_review_output(&clean);
        assert_eq!(formatted_clean["status"], "clean");
        assert_eq!(formatted_clean["baseline"], "abc1234");
        assert_eq!(formatted_clean["findings"], json!([]));
        assert_eq!(formatted_clean["concerns"].as_array().unwrap().len(), 1);
        assert_eq!(formatted_clean["tokens_in"], 1200);
        assert_eq!(formatted_clean["tokens_out"], 300);
        assert_eq!(formatted_clean["tokens_cached"], 800);
        assert!(formatted_clean.get("history").is_none());
        assert!(formatted_clean.get("input_context").is_none());
        assert!(formatted_clean.get("inline_review").is_none());
        assert!(formatted_clean.get("dismissed_concerns").is_none());
        assert!(formatted_clean.get("error").is_none());

        let issues = json!({
            "baseline": "abc1234",
            "patches": [{"index": 1, "status": "applied", "sha": "def5678"}],
            "review": {
                "findings": [{
                    "severity": "High",
                    "problem": "missing bounds check",
                    "severity_explanation": "panics on empty slice",
                    "locations": [{"file": "src/lib.rs", "line": 42}],
                    "patch_index": 1,
                    "patch_subject": "feat: x"
                }],
                "concerns": []
            },
            "tokens_in": 100,
            "tokens_out": 50,
            "tokens_cached": 0
        });
        let formatted_issues = format_agent_review_output(&issues);
        assert_eq!(formatted_issues["status"], "issues_found");
        assert_eq!(formatted_issues["findings"].as_array().unwrap().len(), 1);
        assert_eq!(
            formatted_issues["findings"][0]["problem"],
            "missing bounds check"
        );

        let errored = json!({
            "baseline": "abc1234",
            "patches": [{"index": 1, "status": "applied", "review_status": "incomplete"}],
            "review": {"findings": [], "concerns": []},
            "partial": true,
            "error": "patch 1: timed out",
            "tokens_in": 50,
            "tokens_out": 10,
            "tokens_cached": 0
        });
        let formatted_err = format_agent_review_output(&errored);
        assert_eq!(formatted_err["status"], "error");
        assert_eq!(formatted_err["partial"], true);
        assert_eq!(formatted_err["error"], "patch 1: timed out");
    }

    fn rev_parse_head(repo: &Path) -> String {
        let out = crate::git_cmd::in_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// A rename lists both its paths, so a reference scoped to the tree the
    /// file left still attaches, even with rename detection configured on.
    #[tokio::test]
    async fn touched_files_include_both_sides_of_a_rename() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path();
        git(repo, &["init"])?;
        git(repo, &["config", "user.email", "t@e"])?;
        git(repo, &["config", "user.name", "T"])?;
        git(repo, &["config", "diff.renames", "true"])?;
        std::fs::create_dir_all(repo.join("rdma/drv"))?;
        std::fs::write(repo.join("rdma/drv/a.c"), "int a;\n")?;
        git(repo, &["add", "."])?;
        git(repo, &["commit", "-m", "add"])?;
        std::fs::create_dir_all(repo.join("eth"))?;
        git(repo, &["mv", "rdma/drv/a.c", "eth/a.c"])?;
        git(repo, &["commit", "-m", "move"])?;
        let sha = rev_parse_head(repo);

        let (active, touched) = touched_files(repo, Some(&sha), "").await;
        let mut sorted = touched.clone();
        sorted.sort();
        assert_eq!(sorted, ["eth/a.c", "rdma/drv/a.c"]);
        assert_eq!(active, touched, "the active files are the diff-tree list");
        Ok(())
    }

    /// Without a commit the patch text names the files, renames and new files
    /// included, and /dev/null is not a file.
    #[tokio::test]
    async fn touched_files_fall_back_to_the_patch_diff() {
        let diff = "\
diff --git a/rdma/drv/a.c b/eth/a.c
similarity index 100%
rename from rdma/drv/a.c
rename to eth/a.c
diff --git a/include/uapi/rdma/x-abi.h b/include/uapi/rdma/x-abi.h
new file mode 100644
--- /dev/null
+++ b/include/uapi/rdma/x-abi.h
@@ -0,0 +1,2 @@
+-- not a header
+struct x;
diff --git a/eth/b.c b/eth/b.c
--- a/eth/b.c
+++ b/eth/b.c
@@ -1,2 +1,2 @@
--- removed text, not a header
 context
";
        let temp = tempfile::tempdir().unwrap();
        let (active, touched) = touched_files(temp.path(), None, diff).await;
        assert!(active.is_empty());
        assert_eq!(
            touched,
            [
                "rdma/drv/a.c",
                "eth/a.c",
                "include/uapi/rdma/x-abi.h",
                "eth/b.c"
            ]
        );
    }

    /// Changes with no `---`/`+++` pair still name their files through the
    /// `diff --git` line, and quoted paths lose their quotes and prefixes.
    #[test]
    fn diff_paths_read_changes_without_file_headers() {
        let diff = "\
diff --git a/rdma/drv/fw.bin b/rdma/drv/fw.bin
index 1111111..2222222 100644
Binary files a/rdma/drv/fw.bin and b/rdma/drv/fw.bin differ
diff --git a/rdma/drv/tool.sh b/rdma/drv/tool.sh
old mode 100644
new mode 100755
diff --git a/include/uapi/rdma/empty.h b/include/uapi/rdma/empty.h
new file mode 100644
index 0000000..e69de29
diff --git \"a/rdma/drv/odd\\tname.c\" \"b/rdma/drv/odd\\tname.c\"
--- \"a/rdma/drv/odd\\tname.c\"
+++ \"b/rdma/drv/odd\\tname.c\"
@@ -1 +1 @@
-x
+y
diff --git a/a b/c.c b/a b/c.c
--- a/a b/c.c\t
+++ b/a b/c.c\t
@@ -1 +1 @@
-x
+y
diff --git \"a/rdma/drv/caf\\303\\251.c\" \"b/rdma/drv/caf\\303\\251.c\"
deleted file mode 100644
index e69de29..0000000
";
        assert_eq!(
            diff_paths(diff),
            [
                "rdma/drv/fw.bin",
                "rdma/drv/tool.sh",
                "include/uapi/rdma/empty.h",
                "rdma/drv/odd\tname.c",
                "a b/c.c",
                "rdma/drv/café.c",
            ]
        );
    }

    /// A rename whose names cannot be told apart on the `diff --git` line
    /// takes its paths from the rename lines alone, rather than a guess.
    #[test]
    fn diff_paths_do_not_guess_an_ambiguous_diff_git_line() {
        let diff = "\
diff --git a/x b/y b/z b/w
similarity index 100%
rename from x b/y
rename to z b/w
";
        assert_eq!(diff_paths(diff), ["x b/y", "z b/w"]);
    }
}
