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

use crate::activity::{ActivityKey, ActivityRegistry, Phase};
use crate::cancel::CancelRegistry;
use crate::events::{Event, MessageSource};
use crate::utils::redact_secret;
use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Output;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{Duration, interval};
use tracing::{debug, error, info, warn};

/// Timeout for network-bound git operations (fetching from a remote).
const NETWORK_OP_TIMEOUT: Duration = Duration::from_secs(300);

/// Timeout for local metadata operations (rev-parse, remote get-url, ...).
/// These never touch the network, so anything slow here is pathological.
const LOCAL_OP_TIMEOUT: Duration = Duration::from_secs(30);

/// Error text for a fetch abandoned because everything waiting on it was cancelled.
const FETCH_CANCELLED: &str = "Fetch cancelled: every patchset waiting on it was cancelled";

/// Whether an error came from cancellation rather than a genuine fetch failure.
fn is_cancellation(e: &anyhow::Error) -> bool {
    e.to_string().contains(FETCH_CANCELLED)
}

/// Runs a git command to completion under a timeout.
///
/// Without this, a `git fetch` against an unreachable remote hangs forever, and
/// because `FetchAgent::run` awaits `process_queue` inline, that wedges the whole
/// agent: no patchset ever fetches again. `kill_on_drop` matters as much as the
/// timeout — when the timeout fires the future is dropped, and without it the
/// stuck subprocess would survive us giving up on it.
///
/// Mirrors the pattern already used by `GitSyncWorker` in `git_ops.rs`.
async fn run_git(mut cmd: Command, limit: Duration, what: &str) -> Result<std::process::Output> {
    cmd.kill_on_drop(true);
    match tokio::time::timeout(limit, cmd.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(anyhow!("Failed to execute {}: {}", what, e)),
        Err(_) => Err(anyhow!("{} timed out after {}s", what, limit.as_secs())),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FetchRequest {
    pub repo_url: Option<String>,
    pub commit_hash: String,
    pub mr_url: Option<String>,
    pub mr_title: Option<String>,
    pub mr_number: Option<i64>,
    /// The placeholder patchset waiting on this fetch, so the work can be
    /// cancelled. `None` for requests with no patchset behind them.
    pub patchset_id: Option<i64>,
    /// Adopt an empty first commit as the series cover letter (b4 prep style)
    /// rather than reviewing it.
    pub b4_cover_letter: bool,
}

pub struct FetchAgent {
    repo_path: PathBuf,
    rx: mpsc::Receiver<FetchRequest>,
    main_tx: mpsc::Sender<Event>,
    #[allow(clippy::type_complexity)]
    mr_metadata: HashMap<String, (Option<String>, Option<String>, Option<i64>)>,
    gitlab_token: Option<String>,
    activity: Arc<ActivityRegistry>,
    cancels: Arc<CancelRegistry>,
    /// Which patchset each queued commit belongs to. Needed because the queue
    /// batches by repository, so a commit is otherwise anonymous by the time it
    /// is fetched.
    commit_owners: HashMap<String, i64>,
    /// Ranges whose submitter asked for an empty first commit to be adopted as
    /// the series cover letter.
    b4_cover_requests: HashSet<String>,
}

impl FetchAgent {
    pub fn new(
        repo_path: PathBuf,
        main_tx: mpsc::Sender<Event>,
        gitlab_token: Option<String>,
        activity: Arc<ActivityRegistry>,
        cancels: Arc<CancelRegistry>,
    ) -> (Self, mpsc::Sender<FetchRequest>) {
        let (tx, rx) = mpsc::channel(100);
        (
            Self {
                repo_path,
                rx,
                main_tx,
                mr_metadata: HashMap::new(),
                gitlab_token,
                activity,
                cancels,
                commit_owners: HashMap::new(),
                b4_cover_requests: HashSet::new(),
            },
            tx,
        )
    }

    /// Whether the patchset waiting on this commit has been cancelled.
    ///
    /// A commit with no known patchset is never considered cancelled — there is
    /// nobody to have cancelled it.
    fn is_cancelled(&self, commit: &str) -> bool {
        self.commit_owners
            .get(commit)
            .and_then(|id| self.cancels.token_for(*id))
            .map(|t| t.is_cancelled())
            .unwrap_or(false)
    }

    /// Cancellation tokens for every patchset behind this batch of commits.
    fn tokens_for(&self, commits: &[String]) -> Vec<tokio_util::sync::CancellationToken> {
        commits
            .iter()
            .filter_map(|c| self.commit_owners.get(c))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .filter_map(|id| self.cancels.token_for(*id))
            .collect()
    }

    /// Runs a fetch, aborting it only once every patchset behind it is cancelled.
    ///
    /// A batch is shared: several patchsets can be waiting on the same
    /// repository. Killing the fetch because one of them was cancelled would
    /// sabotage the others, so the abort waits for all of them. `run_git` sets
    /// `kill_on_drop`, so dropping the future here terminates the `git fetch`
    /// process.
    ///
    /// Known limitation: git spawns helper processes (`git-remote-http`) that
    /// are not in our child's process group and so survive it, holding the
    /// connection until the kernel times the socket out. Signalling the whole
    /// process group would reach them, but is not safe here: the child is
    /// reaped before the group is signalled, and a recycled pid belonging to a
    /// shell — which is itself a process-group leader — is indistinguishable
    /// from our own dead child. Killing that group would take out an unrelated
    /// session. A bounded, self-healing leak is the better trade.
    async fn fetch_with_cancel<F>(
        &self,
        fetch: F,
        tokens: &[tokio_util::sync::CancellationToken],
    ) -> Result<()>
    where
        F: std::future::Future<Output = Result<()>>,
    {
        if tokens.is_empty() {
            return fetch.await;
        }

        let all_cancelled = async {
            futures::future::join_all(tokens.iter().map(|t| t.cancelled())).await;
        };

        tokio::select! {
            result = fetch => result,
            _ = all_cancelled => Err(anyhow!("{}", FETCH_CANCELLED)),
        }
    }

    /// Releases the cancellation entries for commits this agent is done with.
    fn release(&mut self, commits: &[String]) {
        for commit in commits {
            if let Some(id) = self.commit_owners.remove(commit) {
                self.cancels.unregister(id);
            }
        }
    }

    /// Reports the same phase for every commit in a batch.
    ///
    /// Fetch work is batched per repository, so a single git operation covers
    /// many commits at once; each is tracked separately so a caller holding only
    /// a patchset's SHA can still find out what is happening.
    fn mark(&self, commits: &[String], phase: Phase) {
        for commit in commits {
            self.activity
                .update(ActivityKey::Commit(commit.clone()), phase.clone());
        }
    }

    fn clear_marks(&self, commits: &[String]) {
        for commit in commits {
            self.activity.clear(&ActivityKey::Commit(commit.clone()));
        }
    }

    pub async fn run(mut self) {
        info!("FetchAgent started");
        let mut queue: HashMap<Option<String>, HashSet<String>> = HashMap::new();
        let mut ticker = interval(Duration::from_secs(10));

        loop {
            tokio::select! {
                Some(req) = self.rx.recv() => {
                    if req.mr_url.is_some() || req.mr_title.is_some() || req.mr_number.is_some() {
                        self.mr_metadata.insert(
                            req.commit_hash.clone(),
                            (req.mr_url.clone(), req.mr_title.clone(), req.mr_number)
                        );
                    }
                    self.activity.update(
                        ActivityKey::Commit(req.commit_hash.clone()),
                        Phase::Queued,
                    );
                    // Register before queueing so a cancel arriving in the gap
                    // before the fetch starts is still observed.
                    if let Some(ps_id) = req.patchset_id {
                        self.commit_owners.insert(req.commit_hash.clone(), ps_id);
                        self.cancels.register(ps_id);
                    }
                    if req.b4_cover_letter {
                        self.b4_cover_requests.insert(req.commit_hash.clone());
                    }
                    queue.entry(req.repo_url)
                        .or_default()
                        .insert(req.commit_hash);
                }
                _ = ticker.tick() => {
                    if !queue.is_empty() {
                        self.process_queue(&mut queue).await;
                    }
                }
            }
        }
    }

    async fn process_queue(&mut self, queue: &mut HashMap<Option<String>, HashSet<String>>) {
        info!("Processing fetch queue with {} repos", queue.len());

        for (url_opt, commits) in queue.drain() {
            if commits.is_empty() {
                continue;
            }

            let all_commits: Vec<String> = commits.into_iter().collect();
            let url_display = url_opt.as_deref().unwrap_or("local");

            // Checkpoint 1: drop anything already cancelled before spending work
            // on it. If that empties the batch there is nothing left to fetch.
            let (commit_list, dropped): (Vec<String>, Vec<String>) =
                all_commits.into_iter().partition(|c| !self.is_cancelled(c));
            if !dropped.is_empty() {
                info!("Skipping {} cancelled commit(s)", dropped.len());
                self.clear_marks(&dropped);
                self.release(&dropped);
            }
            if commit_list.is_empty() {
                continue;
            }

            info!(
                "Processing {} commits for remote {}",
                commit_list.len(),
                url_display
            );

            // For ranges (base..head), we need to check both endpoints individually
            let mut commits_to_check = Vec::new();
            for commit_or_range in &commit_list {
                if commit_or_range.contains("..") {
                    let parts: Vec<&str> = commit_or_range.split("..").collect();
                    if parts.len() == 2 {
                        commits_to_check.push(parts[0].to_string());
                        commits_to_check.push(parts[1].to_string());
                    }
                } else {
                    commits_to_check.push(commit_or_range.clone());
                }
            }

            self.mark(
                &commit_list,
                Phase::GitOp {
                    op: "git rev-parse (checking commit presence)".to_string(),
                },
            );

            let mut missing_commits = Vec::new();
            for commit in &commits_to_check {
                if !self.is_present(commit).await {
                    missing_commits.push(commit.clone());
                }
            }

            if missing_commits.is_empty() {
                info!(
                    "All commits present locally, skipping fetch for {}",
                    url_display
                );
            } else if let Some(url) = url_opt {
                // Remote fetch logic
                let remote_name = self.get_remote_name(&url);

                // Check if repo is local (same as self.repo_path)
                let is_local = {
                    let url_path = PathBuf::from(&url);
                    if let (Ok(canon_url), Ok(canon_repo)) = (
                        std::fs::canonicalize(&url_path),
                        std::fs::canonicalize(&self.repo_path),
                    ) {
                        canon_url == canon_repo
                    } else {
                        false
                    }
                };

                if is_local {
                    warn!(
                        "Repository is local but commits are missing: {:?}. Cannot fetch.",
                        missing_commits
                    );
                    // Do not continue here; let it fall through to Step 3 where it will fail individually
                } else {
                    self.mark(
                        &commit_list,
                        Phase::GitOp {
                            op: format!("git remote setup for {}", redact_secret(&url)),
                        },
                    );
                    if let Err(e) = self.ensure_remote(&remote_name, &url).await {
                        error!("Failed to ensure remote {}: {}", url, e);
                        self.clear_marks(&commit_list);
                        self.release(&commit_list);
                        for commit in &missing_commits {
                            let _ = self
                                .main_tx
                                .send(Event::IngestionFailed {
                                    article_id: commit.clone(),
                                    error: format!("Failed to set up remote {}: {}", url, e),
                                    source: MessageSource::GitFetch,
                                })
                                .await;
                        }
                        continue;
                    }

                    // 1. Try optimistic fetch (fetch specific commits)
                    self.mark(
                        &commit_list,
                        Phase::Fetching {
                            remote: redact_secret(&url),
                            commits: missing_commits.len(),
                        },
                    );
                    let batch_tokens = self.tokens_for(&commit_list);

                    // Pull request heads live outside refs/heads. A PR from a
                    // fork -- or from a branch since deleted -- is reachable
                    // only through the forge's own ref namespace, so neither the
                    // optimistic fetch nor the all-heads fallback can see it.
                    // Best-effort: a remote that is not a forge simply has no
                    // such refs, which is not an error worth failing over.
                    // Keyed off `commit_list`, not `missing_commits`: a range is
                    // split into its two endpoints for the presence check, so
                    // the bare SHAs there never match the range the pull request
                    // metadata is filed under.
                    self.fetch_pull_refs(&remote_name, &commit_list, &batch_tokens)
                        .await;

                    if let Err(e) = self
                        .fetch_with_cancel(
                            self.fetch_commits(&remote_name, &missing_commits),
                            &batch_tokens,
                        )
                        .await
                    {
                        // A cancelled fetch is not a reason to try a bigger one.
                        if is_cancellation(&e) {
                            info!("Fetch for {} cancelled; not falling back", url);
                            self.clear_marks(&commit_list);
                            self.release(&commit_list);
                            for commit in &missing_commits {
                                let _ = self
                                    .main_tx
                                    .send(Event::IngestionFailed {
                                        article_id: commit.clone(),
                                        error: format!("Fetch cancelled for {}", url),
                                        source: MessageSource::GitFetch,
                                    })
                                    .await;
                            }
                            continue;
                        }
                        warn!(
                            "Optimistic fetch failed for {}: {}. Falling back to full fetch.",
                            url, e
                        );
                        // 2. Fallback: Fetch everything (heads)
                        self.mark(
                            &commit_list,
                            Phase::Fetching {
                                remote: format!("{} (all heads)", redact_secret(&url)),
                                commits: missing_commits.len(),
                            },
                        );
                        if let Err(e) = self
                            .fetch_with_cancel(self.fetch_all(&remote_name), &batch_tokens)
                            .await
                        {
                            error!("Full fetch failed for {}: {}", url, e);
                            self.clear_marks(&commit_list);
                            self.release(&commit_list);
                            for commit in &missing_commits {
                                let _ = self
                                    .main_tx
                                    .send(Event::IngestionFailed {
                                        article_id: commit.clone(),
                                        error: format!("Failed to fetch from {}: {}", url, e),
                                        source: MessageSource::GitFetch,
                                    })
                                    .await;
                            }
                            continue;
                        }
                    }
                }
            } else {
                // Local repo, but commits are missing
                warn!(
                    "Local repository missing commits: {:?}. Cannot fetch.",
                    missing_commits
                );
            }

            // Checkpoint 3: a cancel can land during a long fetch, so re-check
            // before turning anything into patches. Otherwise a cancelled
            // patchset would still be queued for review.
            let (commit_list, dropped_late): (Vec<String>, Vec<String>) =
                commit_list.into_iter().partition(|c| !self.is_cancelled(c));
            if !dropped_late.is_empty() {
                info!(
                    "Dropping {} commit(s) cancelled during the fetch",
                    dropped_late.len()
                );
                self.clear_marks(&dropped_late);
                self.release(&dropped_late);
            }
            if commit_list.is_empty() {
                continue;
            }

            // 3. Process each commit or range
            self.mark(
                &commit_list,
                Phase::GitOp {
                    op: "extracting patch metadata".to_string(),
                },
            );
            // The loop consumes commit_list, so keep a copy to clear afterwards.
            let processed = commit_list.clone();

            for commit_or_range in commit_list {
                if commit_or_range.contains("..") {
                    // It's a range
                    let range = &commit_or_range;

                    let shas = match crate::git_ops::resolve_git_range(&self.repo_path, range).await
                    {
                        Ok(shas) => shas,
                        Err(e) => {
                            let _ = self
                                .main_tx
                                .send(Event::IngestionFailed {
                                    article_id: range.clone(),
                                    error: format!("Failed to resolve git range: {}", e),
                                    source: MessageSource::GitFetch,
                                })
                                .await;
                            continue;
                        }
                    };
                    let count = shas.len() as u32;

                    // Process each SHA
                    let (mr_url, mr_title, mr_number) = self
                        .mr_metadata
                        .get(range)
                        .cloned()
                        .unwrap_or((None, None, None));

                    let article_id = if let Some(number) = mr_number {
                        format!("mr-{}-{}", number, range)
                    } else {
                        range.to_string()
                    };

                    // Decided before the loop rather than per commit, because
                    // adopting the tracker changes how many parts the series has.
                    // A patchset whose received parts never reach its total sits
                    // in Incomplete forever and is never reviewed at all.
                    //
                    // Only the first commit is ever adopted (a b4 tracker is
                    // first by convention), it must genuinely be empty, and a
                    // lone commit is never adopted — that would leave nothing to
                    // review.
                    let adopt_cover = self.b4_cover_requests.contains(range)
                        && count > 1
                        && match shas.first() {
                            Some(first) => {
                                crate::git_ops::extract_patch_metadata(&self.repo_path, first)
                                    .await
                                    .map(|m| m.diff.trim().is_empty())
                                    .unwrap_or(false)
                            }
                            None => false,
                        };

                    // The cover letter is not one of the parts, matching how a
                    // mailing-list 0/N cover letter is counted.
                    let total_parts = if adopt_cover { count - 1 } else { count };

                    for (i, sha) in shas.iter().enumerate() {
                        match self
                            .extract_patch(
                                sha,
                                &article_id,
                                // Cover letter at 0, real patches from 1, as the
                                // mailing-list path numbers them.
                                if adopt_cover {
                                    i as u32
                                } else {
                                    (i + 1) as u32
                                },
                                total_parts,
                                mr_url.as_ref(),
                                mr_title.as_ref(),
                                mr_number,
                                adopt_cover && i == 0,
                            )
                            .await
                        {
                            Ok(mut event) => {
                                if let Event::PatchSubmitted {
                                    ref mut message_id, ..
                                } = event
                                {
                                    *message_id = sha.clone();
                                }
                                if let Err(e) = self.main_tx.send(event).await {
                                    error!("Failed to send PatchSubmitted event: {}", e);
                                }
                            }
                            Err(e) => {
                                error!(
                                    "Failed to extract patch {} from range {}: {}",
                                    sha, range, e
                                );
                            }
                        }
                    }
                    info!("Successfully submitted remote range {}", range);
                } else {
                    // Single commit
                    let full_sha = match self.resolve_sha(&commit_or_range).await {
                        Ok(sha) => sha,
                        Err(e) => {
                            let _ = self
                                .main_tx
                                .send(Event::IngestionFailed {
                                    article_id: commit_or_range.clone(),
                                    error: format!("Failed to resolve SHA: {}", e),
                                    source: MessageSource::GitFetch,
                                })
                                .await;
                            continue;
                        }
                    };

                    let (mr_url, mr_title, mr_number) = self
                        .mr_metadata
                        .get(&commit_or_range)
                        .cloned()
                        .unwrap_or((None, None, None));

                    let article_id = if let Some(number) = mr_number {
                        format!("mr-{}-{}", number, &commit_or_range)
                    } else {
                        commit_or_range.clone()
                    };

                    match self
                        .extract_patch(
                            &full_sha,
                            &article_id,
                            1,
                            1,
                            mr_url.as_ref(),
                            mr_title.as_ref(),
                            mr_number,
                            // A lone commit cannot be a series cover letter;
                            // adopting it would leave nothing to review. If it is
                            // empty, the reviewer skips it.
                            false,
                        )
                        .await
                    {
                        Ok(mut event) => {
                            if let Event::PatchSubmitted {
                                ref mut message_id, ..
                            } = event
                            {
                                *message_id = full_sha.clone();
                            }
                            if let Err(e) = self.main_tx.send(event).await {
                                error!("Failed to send PatchSubmitted event: {}", e);
                            } else {
                                info!("Successfully submitted remote patch {}", commit_or_range);
                            }
                        }
                        Err(e) => {
                            error!("Failed to extract patch {}: {}", commit_or_range, e);
                            let _ = self
                                .main_tx
                                .send(Event::IngestionFailed {
                                    article_id: commit_or_range,
                                    error: format!("Failed to extract patch: {}", e),
                                    source: MessageSource::GitFetch,
                                })
                                .await;
                        }
                    }
                }
            }

            // Fetch work for this repo is done, however it went. Anything still
            // outstanding is now the reviewer's concern, not the fetcher's.
            self.clear_marks(&processed);
            self.release(&processed);
        }
    }

    fn get_remote_name(&self, url: &str) -> String {
        // Use a hash of the URL to ensure safe and unique remote names
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        url.hash(&mut hasher);
        format!("fetcher-{:x}", hasher.finish())
    }

    async fn ensure_remote(&self, name: &str, url: &str) -> Result<()> {
        // Inject GitLab token if available
        let authenticated_url = if let Some(token) = &self.gitlab_token {
            if url.contains("gitlab.com") && url.starts_with("https://") {
                url.replace("https://", &format!("https://oauth2:{}@", token))
            } else {
                url.to_string()
            }
        } else {
            url.to_string()
        };

        // Check if remote exists. A single get-url serves both purposes: exit
        // status tells us whether it exists, stdout tells us where it points.
        let mut get_url = Command::new("git");
        get_url
            .current_dir(&self.repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .args(["remote", "get-url", name]);
        let existing = run_git(get_url, LOCAL_OP_TIMEOUT, "git remote get-url").await?;

        if existing.status.success() {
            let current_url = String::from_utf8_lossy(&existing.stdout).trim().to_string();

            if current_url != authenticated_url {
                info!(
                    "Updating remote {} from {} to {}",
                    name,
                    redact_secret(&current_url),
                    redact_secret(&authenticated_url)
                );
                let mut set_url = Command::new("git");
                set_url
                    .current_dir(&self.repo_path)
                    .args(["-c", "safe.bareRepository=all"])
                    .args(["remote", "set-url", name, &authenticated_url]);
                run_git(set_url, LOCAL_OP_TIMEOUT, "git remote set-url").await?;
            }
        } else {
            info!(
                "Adding remote {} -> {}",
                name,
                redact_secret(&authenticated_url)
            );
            let mut add = Command::new("git");
            add.current_dir(&self.repo_path)
                .args(["-c", "safe.bareRepository=all"])
                .args(["remote", "add", name, &authenticated_url]);
            let output = run_git(add, LOCAL_OP_TIMEOUT, "git remote add").await?;

            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to add remote: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        Ok(())
    }

    /// Fetches the forge ref for any pull request in this batch.
    ///
    /// `git fetch <remote>` fetches `refs/heads/*`. A pull request head is not
    /// there: GitHub publishes it at `refs/pull/<n>/head` and GitLab at
    /// `refs/merge-requests/<n>/head`. Without this, a same-repository PR works
    /// by accident -- its branch is in refs/heads -- and a fork PR fails to
    /// resolve its range at all.
    ///
    /// Both namespaces are tried because the fetcher does not know which forge a
    /// remote belongs to, and a missing ref fails the whole fetch, so they
    /// cannot be combined into one call. Failures are logged, not propagated:
    /// most remotes have neither namespace, and the caller's own fetch is what
    /// decides whether the commits arrived.
    async fn fetch_pull_refs(
        &self,
        remote: &str,
        commits: &[String],
        tokens: &[tokio_util::sync::CancellationToken],
    ) {
        for commit_or_range in commits {
            let Some(number) = self
                .mr_metadata
                .get(commit_or_range)
                .and_then(|(_, _, number)| *number)
            else {
                continue;
            };

            let mut failures = Vec::new();
            for namespace in ["pull", "merge-requests"] {
                let refspec = format!(
                    "+refs/{}/{}/head:refs/sashiko/{}/{}",
                    namespace, number, namespace, number
                );
                let fetched = self
                    .fetch_with_cancel(
                        async {
                            let output = self.fetch_with_graph_retry(&[remote, &refspec]).await?;
                            if output.status.success() {
                                Ok(())
                            } else {
                                Err(anyhow!(
                                    "{}",
                                    String::from_utf8_lossy(&output.stderr).trim()
                                ))
                            }
                        },
                        tokens,
                    )
                    .await;

                match fetched {
                    Ok(()) => {
                        info!("Fetched {} #{} head from {}", namespace, number, remote);
                        failures.clear();
                        break;
                    }
                    Err(e) if is_cancellation(&e) => return,
                    Err(e) => {
                        debug!(
                            "No {} ref for #{} on {}: {}",
                            namespace,
                            number,
                            remote,
                            e.to_string().trim()
                        );
                        failures.push(format!("{}: {}", namespace, e.to_string().trim()));
                    }
                }
            }

            // We know this is a pull request -- it has a number -- so both
            // namespaces failing is worth saying out loud. Left at debug, a
            // private repository with no credentials on the host looks
            // identical to a healthy fetch until the range fails to resolve two
            // stages later, naming a SHA instead of the reason.
            if !failures.is_empty() {
                warn!(
                    "Could not fetch the forge ref for #{} from {}; if {} is private, the daemon \
                     host needs git credentials for it (`gh auth login && gh auth setup-git`, or \
                     an SSH remote). Tried {}",
                    number,
                    remote,
                    remote,
                    failures.join("; ")
                );
            }
        }
    }

    /// Runs one `git fetch`, dropping a stale commit-graph and trying
    /// again when that is what turned the fetch away.  Fetch-pack
    /// rejects the graph before it opens a connection, so that retry
    /// repeats no transfer; the wording the commit parse emits can
    /// come after one, and then the retry pays for it again.  Returns
    /// git's own output either way; the caller words the failure.
    async fn fetch_with_graph_retry(&self, args: &[&str]) -> Result<Output> {
        let mut dropped_graph = false;

        loop {
            let mut cmd = Command::new("git");
            cmd.current_dir(&self.repo_path)
                .args(crate::git_ops::GIT_PROTOCOL_RESTRICTIONS)
                .arg("fetch")
                .args(args);

            let output = run_git(cmd, NETWORK_OP_TIMEOUT, "git fetch").await?;

            if output.status.success() || dropped_graph {
                if dropped_graph {
                    crate::git_ops::schedule_commit_graph_rebuild(&self.repo_path);
                }
                return Ok(output);
            }

            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            if !crate::git_ops::is_stale_commit_graph(&stderr) {
                return Ok(output);
            }

            warn!("Fetch found a stale commit-graph; dropping it");
            if let Err(e) = crate::git_ops::drop_commit_graph(&self.repo_path).await {
                warn!("Failed to drop the commit-graph: {}", e);
                return Ok(output);
            }
            dropped_graph = true;
        }
    }

    async fn fetch_commits(&self, remote: &str, commits: &[String]) -> Result<()> {
        let mut args = vec![remote];
        args.extend(commits.iter().map(String::as_str));

        let output = self.fetch_with_graph_retry(&args).await?;
        if !output.status.success() {
            return Err(anyhow!(
                "Fetch failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    async fn fetch_all(&self, remote: &str) -> Result<()> {
        let output = self.fetch_with_graph_retry(&[remote]).await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Fetch all failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    async fn is_present(&self, commit_or_range: &str) -> bool {
        let mut args = vec!["-c", "safe.bareRepository=all"];
        let arg_str: String;

        if let Some((start, end)) = commit_or_range.split_once("..") {
            arg_str = format!("{}^{{commit}}..{}^{{commit}}", start, end);
            args.extend(["rev-list", "-n", "1", &arg_str]);
        } else {
            arg_str = format!("{}^{{commit}}", commit_or_range);
            args.extend(["rev-parse", "--verify", &arg_str]);
        };

        let mut cmd = Command::new("git");
        cmd.current_dir(&self.repo_path).args(&args);
        let output = run_git(cmd, LOCAL_OP_TIMEOUT, "git rev-parse/rev-list").await;

        match output {
            Ok(s) => {
                let success = s.status.success();
                if success {
                    info!("is_present: {} is present", commit_or_range);
                } else {
                    info!(
                        "is_present: {} is missing or not a commit. stderr: {}",
                        commit_or_range,
                        String::from_utf8_lossy(&s.stderr)
                    );
                }
                success
            }
            Err(e) => {
                error!("is_present: {} check failed: {}", commit_or_range, e);
                false
            }
        }
    }

    async fn resolve_sha(&self, commit: &str) -> Result<String> {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .args(["rev-parse", "--verify", commit]);
        let output = run_git(cmd, LOCAL_OP_TIMEOUT, "git rev-parse --verify").await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to resolve SHA for {}: {}",
                commit,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    #[allow(clippy::too_many_arguments)]
    async fn extract_patch(
        &self,
        commit: &str,
        article_id: &str,
        index: u32,
        total: u32,
        mr_url: Option<&String>,
        mr_title: Option<&String>,
        mr_number: Option<i64>,
        is_cover_letter: bool,
    ) -> Result<Event> {
        let meta = crate::git_ops::extract_patch_metadata(&self.repo_path, commit).await?;

        if is_cover_letter {
            info!(
                "Adopting empty commit {} as the series cover letter",
                commit
            );
        }

        Ok(Event::PatchSubmitted {
            group: "git-fetch".to_string(),
            article_id: article_id.to_string(),
            message_id: String::new(), // Set by caller
            subject: meta.subject,
            author: meta.author,
            message: meta.message,
            diff: meta.diff,
            base_commit: meta.base_commit,
            timestamp: meta.timestamp,
            index,
            total,
            mr_url: mr_url.cloned(),
            mr_title: mr_title.cloned(),
            mr_number,
            is_cover_letter,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[tokio::test]
    async fn test_fetch_agent_lifecycle() {
        let (tx, _rx) = mpsc::channel(1);
        let repo_path = PathBuf::from("/tmp");
        let (_agent, _sender) = FetchAgent::new(
            repo_path,
            tx,
            None,
            ActivityRegistry::new(),
            CancelRegistry::new(),
        );
    }

    /// One fetch batch can serve several patchsets, so cancelling one must not
    /// abort a fetch the others are still waiting on.
    #[tokio::test]
    async fn test_batch_fetch_aborts_only_when_every_patchset_is_cancelled() {
        let cancels = CancelRegistry::new();
        let (tx, _rx) = mpsc::channel(1);
        let (agent, _) = FetchAgent::new(
            PathBuf::from("/tmp"),
            tx,
            None,
            ActivityRegistry::new(),
            cancels.clone(),
        );

        let a = cancels.register(1);
        let b = cancels.register(2);
        let tokens = vec![a.clone(), b.clone()];

        // One cancelled, one still waiting: the fetch must be allowed to finish.
        a.cancel();
        let mut cmd = Command::new("true");
        let started = std::time::Instant::now();
        let result = agent
            .fetch_with_cancel(
                async {
                    run_git(cmd, LOCAL_OP_TIMEOUT, "true").await?;
                    Ok(())
                },
                &tokens,
            )
            .await;
        assert!(
            result.is_ok(),
            "a peer still needs this fetch: {:?}",
            result.err()
        );

        // Both cancelled: now the fetch may be abandoned, promptly.
        b.cancel();
        cmd = Command::new("sleep");
        cmd.arg("60");
        let result = agent
            .fetch_with_cancel(
                async {
                    run_git(cmd, NETWORK_OP_TIMEOUT, "sleep").await?;
                    Ok(())
                },
                &tokens,
            )
            .await;
        let err = result.expect_err("nothing wants this fetch any more");
        assert!(err.to_string().contains("cancelled"), "got: {}", err);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "abort must not wait out the fetch timeout"
        );
    }

    #[tokio::test]
    async fn test_cancelled_commits_are_recognised() {
        let cancels = CancelRegistry::new();
        let (tx, _rx) = mpsc::channel(1);
        let (mut agent, _) = FetchAgent::new(
            PathBuf::from("/tmp"),
            tx,
            None,
            ActivityRegistry::new(),
            cancels.clone(),
        );

        agent.commit_owners.insert("aaa".to_string(), 1);
        agent.commit_owners.insert("bbb".to_string(), 2);
        cancels.register(1);
        cancels.register(2);

        assert!(!agent.is_cancelled("aaa"));
        cancels.cancel(1);
        assert!(agent.is_cancelled("aaa"));
        assert!(
            !agent.is_cancelled("bbb"),
            "cancelling one must not affect another"
        );

        // A commit with no known patchset has nobody to cancel it.
        assert!(!agent.is_cancelled("unknown"));

        // Releasing drops both the mapping and the registry entry, so a later
        // review of the same patchset starts from a clean slate.
        agent.release(&["aaa".to_string()]);
        assert!(!agent.is_cancelled("aaa"));
        assert!(!cancels.is_registered(1));
        assert!(cancels.is_registered(2));
    }

    #[tokio::test]
    async fn test_run_git_times_out_instead_of_hanging() {
        // `sleep` stands in for a `git fetch` against a blackholed remote: it never
        // returns on its own, so only the timeout can end it. Before this guard,
        // such a command wedged the entire FetchAgent.
        let mut cmd = Command::new("sleep");
        cmd.arg("60");

        let started = std::time::Instant::now();
        let result = run_git(cmd, Duration::from_millis(200), "sleep").await;
        let elapsed = started.elapsed();

        let err = result.expect_err("a hung command must return an error");
        assert!(
            err.to_string().contains("timed out"),
            "expected a timeout error, got: {}",
            err
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "run_git waited {:?}; it should have given up at the limit",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_run_git_returns_output_on_success() -> Result<()> {
        let mut cmd = Command::new("git");
        cmd.arg("--version");

        let output = run_git(cmd, LOCAL_OP_TIMEOUT, "git --version").await?;

        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("git version"));
        Ok(())
    }

    /// A pull request head lives outside refs/heads, so neither the optimistic
    /// fetch nor the all-heads fallback can reach it. This is the only thing
    /// that does.
    ///
    /// Also pins the keying: the presence check splits `base..head` into its two
    /// endpoints, so the pull request metadata -- filed under the range -- is
    /// not findable from the list of missing SHAs.
    #[tokio::test]
    async fn pull_request_heads_are_fetched_from_the_forge_ref() -> Result<()> {
        let upstream_dir = tempfile::tempdir()?;
        let upstream = upstream_dir.path().to_path_buf();
        let local_dir = tempfile::tempdir()?;
        let local = local_dir.path().to_path_buf();

        let git = async |repo: &std::path::Path, args: Vec<&str>| -> Result<String> {
            let out = Command::new("git")
                .current_dir(repo)
                .args(&args)
                .output()
                .await?;
            assert!(
                out.status.success(),
                "git {args:?}: {:?}",
                String::from_utf8_lossy(&out.stderr)
            );
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        };

        for repo in [&upstream, &local] {
            git(repo, vec!["init", "-q"]).await?;
            git(repo, vec!["config", "user.name", "T"]).await?;
            git(repo, vec!["config", "user.email", "t@e"]).await?;
        }

        git(
            &upstream,
            vec!["commit", "-q", "--allow-empty", "-m", "base"],
        )
        .await?;
        let base = git(&upstream, vec!["rev-parse", "HEAD"]).await?;

        // The PR head exists only under refs/pull/7/head, exactly as a forge
        // publishes it for a fork PR: not on any branch.
        git(&upstream, vec!["checkout", "-q", "-b", "pr-work"]).await?;
        git(
            &upstream,
            vec!["commit", "-q", "--allow-empty", "-m", "pr commit"],
        )
        .await?;
        let head = git(&upstream, vec!["rev-parse", "HEAD"]).await?;
        git(&upstream, vec!["update-ref", "refs/pull/7/head", &head]).await?;
        git(&upstream, vec!["checkout", "-q", "master"]).await?;
        git(&upstream, vec!["branch", "-q", "-D", "pr-work"]).await?;

        let (tx, _rx) = mpsc::channel(1);
        let (mut agent, _) = FetchAgent::new(
            local.clone(),
            tx,
            None,
            ActivityRegistry::new(),
            CancelRegistry::new(),
        );

        git(
            &local,
            vec!["remote", "add", "origin", upstream.to_str().unwrap()],
        )
        .await?;
        git(&local, vec!["fetch", "-q", "origin"]).await?;

        let range = format!("{base}..{head}");
        assert!(
            !agent.is_present(&range).await,
            "the PR head must not be reachable from refs/heads alone"
        );

        agent
            .mr_metadata
            .insert(range.clone(), (None, None, Some(7)));
        agent
            .fetch_pull_refs("origin", std::slice::from_ref(&range), &[])
            .await;

        assert!(
            agent.is_present(&range).await,
            "the forge ref must make the range resolvable"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_all_reports_unreachable_remote_as_error() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        Command::new("git")
            .current_dir(&repo_path)
            .arg("init")
            .output()
            .await?;

        let (tx, _rx) = mpsc::channel(1);
        let (agent, _) = FetchAgent::new(
            repo_path.clone(),
            tx,
            None,
            ActivityRegistry::new(),
            CancelRegistry::new(),
        );

        // Port 1 on loopback refuses immediately, so this exercises the failure
        // path without waiting out NETWORK_OP_TIMEOUT.
        agent
            .ensure_remote("broken", "http://127.0.0.1:1/nonexistent.git")
            .await?;

        let result = agent.fetch_all("broken").await;
        assert!(
            result.is_err(),
            "fetch against an unreachable remote must surface an error"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_extract_patch_parsing() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // Setup dummy repo
        Command::new("git")
            .current_dir(&repo_path)
            .arg("init")
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;

        let file_path = repo_path.join("file.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "content")?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Subject Line\n\nBody Line"])
            .output()
            .await?;

        let (tx, _rx) = mpsc::channel(1);
        let (agent, _) = FetchAgent::new(
            repo_path.clone(),
            tx,
            None,
            ActivityRegistry::new(),
            CancelRegistry::new(),
        );

        let output = Command::new("git")
            .current_dir(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let head = String::from_utf8(output.stdout)?.trim().to_string();

        let event = agent
            .extract_patch(&head, &head, 1, 1, None, None, None, false)
            .await?;

        match event {
            Event::PatchSubmitted {
                subject,
                author,
                message,
                diff,
                article_id,
                ..
            } => {
                assert_eq!(subject, "Subject Line");
                assert_eq!(author, "Test User <test@example.com>");
                assert!(message.contains("Body Line"));
                assert!(diff.contains("diff --git"));
                assert_eq!(article_id, head);
            }
            _ => panic!("Wrong event type"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_is_present_with_tree_sha() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // Setup dummy repo
        Command::new("git")
            .current_dir(&repo_path)
            .arg("init")
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;

        let file_path = repo_path.join("file.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "content")?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Subject Line"])
            .output()
            .await?;

        let (tx, _rx) = mpsc::channel(1);
        let (agent, _) = FetchAgent::new(
            repo_path.clone(),
            tx,
            None,
            ActivityRegistry::new(),
            CancelRegistry::new(),
        );

        let output = Command::new("git")
            .current_dir(&repo_path)
            .args(["rev-parse", "HEAD^{tree}"])
            .output()
            .await?;
        let tree_sha = String::from_utf8(output.stdout)?.trim().to_string();

        assert!(
            !agent.is_present(&tree_sha).await,
            "Tree SHA should not be considered a present commit"
        );

        let output = Command::new("git")
            .current_dir(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let commit_sha = String::from_utf8(output.stdout)?.trim().to_string();

        assert!(
            agent.is_present(&commit_sha).await,
            "Commit SHA should be considered present"
        );

        Ok(())
    }
}
