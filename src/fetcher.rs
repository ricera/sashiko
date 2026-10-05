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
use crate::events::{Event, MessageSource};
use crate::utils::redact_secret;
use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{Duration, interval};
use tracing::{error, info, warn};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FetchRequest {
    pub repo_url: Option<String>,
    pub commit_hash: String,
    pub mr_url: Option<String>,
    pub mr_title: Option<String>,
    pub mr_number: Option<i64>,
}

pub struct FetchAgent {
    repo_path: PathBuf,
    rx: mpsc::Receiver<FetchRequest>,
    main_tx: mpsc::Sender<Event>,
    #[allow(clippy::type_complexity)]
    mr_metadata: HashMap<String, (Option<String>, Option<String>, Option<i64>)>,
    gitlab_token: Option<String>,
    /// Where each requested commit or range reports the git operation in
    /// flight. A fetch against a slow or unreachable remote otherwise leaves
    /// its patchset sitting in Fetching with nothing to say why.
    activity: Arc<ActivityRegistry>,
}

/// The fetch activity of one repository's batch, cleared when the batch is
/// done however it ends: the loop over a batch leaves early on every failure,
/// and an entry left behind would claim a fetch is still running.
struct BatchMarks<'a> {
    activity: &'a ActivityRegistry,
    commits: Vec<String>,
}

impl BatchMarks<'_> {
    /// Keyed by what was asked for, a commit or a range, because that is all
    /// a fetch knows; the API finds the patchset through its placeholder.
    fn mark(&self, phase: Phase) {
        for commit in &self.commits {
            self.activity
                .update(ActivityKey::Commit(commit.clone()), phase.clone());
        }
    }
}

impl Drop for BatchMarks<'_> {
    fn drop(&mut self) {
        for commit in &self.commits {
            self.activity.clear(&ActivityKey::Commit(commit.clone()));
        }
    }
}

/// One requested commit or range, with the commits it needs present.
struct BatchRequest {
    /// The commit or `base..head` range as it was requested.
    commit_or_range: String,
    /// The commit itself, or both ends of a range.
    endpoints: Vec<String>,
}

impl BatchRequest {
    fn new(commit_or_range: String) -> Self {
        let endpoints = if commit_or_range.contains("..") {
            let parts: Vec<&str> = commit_or_range.split("..").collect();
            match parts.as_slice() {
                [base, head] => [*base, head.trim_start_matches('.')]
                    .into_iter()
                    .filter(|endpoint| !endpoint.is_empty())
                    .map(str::to_string)
                    .collect(),
                _ => Vec::new(),
            }
        } else {
            vec![commit_or_range.clone()]
        };
        Self {
            commit_or_range,
            endpoints,
        }
    }

    /// Whether the request needs any of the `missing` commits.
    fn needs_any(&self, missing: &[String]) -> bool {
        self.endpoints.iter().any(|e| missing.contains(e))
    }
}

impl FetchAgent {
    pub fn new(
        repo_path: PathBuf,
        main_tx: mpsc::Sender<Event>,
        gitlab_token: Option<String>,
        activity: Arc<ActivityRegistry>,
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
            },
            tx,
        )
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
                    // Batched on a ten-second tick, so a request can wait
                    // that long before anything happens to it.
                    self.activity
                        .update(ActivityKey::Commit(req.commit_hash.clone()), Phase::Queued);
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

    async fn process_queue(&self, queue: &mut HashMap<Option<String>, HashSet<String>>) {
        info!("Processing fetch queue with {} repos", queue.len());

        for (url_opt, commits) in queue.drain() {
            if commits.is_empty() {
                continue;
            }
            self.process_batch(url_opt.as_deref(), commits.into_iter().collect())
                .await;
        }
    }

    /// Fetches what one repository's batch is missing, then submits every
    /// request that can still be served.
    async fn process_batch(&self, url_opt: Option<&str>, commit_list: Vec<String>) {
        let marks = BatchMarks {
            activity: &self.activity,
            commits: commit_list.clone(),
        };
        marks.mark(Phase::GitOp {
            op: "git rev-parse (checking commit presence)".to_string(),
        });

        info!(
            "Processing {} commits for remote {}",
            commit_list.len(),
            url_opt.unwrap_or("local")
        );

        let requests: Vec<BatchRequest> = commit_list.into_iter().map(BatchRequest::new).collect();
        let missing = self.missing_endpoints(&requests).await;

        let requests = match self.fetch_missing(url_opt, &missing, &marks).await {
            Ok(()) => requests,
            Err(e) => {
                error!("{}", e);
                self.fail_unfetched(requests, &missing, &e.to_string())
                    .await
            }
        };

        marks.mark(Phase::GitOp {
            op: "extracting patch metadata".to_string(),
        });
        for request in requests {
            if request.commit_or_range.contains("..") {
                self.submit_range(&request.commit_or_range).await;
            } else {
                self.submit_commit(&request.commit_or_range).await;
            }
        }
    }

    /// The batch's endpoints that are not in the repository, each checked
    /// once, in request order. This is the union the batch fetches.
    async fn missing_endpoints(&self, requests: &[BatchRequest]) -> Vec<String> {
        let mut checked = HashSet::new();
        let mut missing = Vec::new();
        for endpoint in requests.iter().flat_map(|r| &r.endpoints) {
            if checked.insert(endpoint.as_str()) && !self.is_present(endpoint).await {
                missing.push(endpoint.clone());
            }
        }
        missing
    }

    /// Brings the missing endpoints in from the batch's remote.
    ///
    /// Returns `Ok` without fetching when nothing is missing or there is no
    /// other repository to fetch from; a request whose commits are still
    /// absent then fails on its own when it is submitted. An `Err` carries
    /// the wording that every request needing a missing commit fails with.
    async fn fetch_missing(
        &self,
        url_opt: Option<&str>,
        missing: &[String],
        marks: &BatchMarks<'_>,
    ) -> Result<()> {
        if missing.is_empty() {
            info!(
                "All commits present locally, skipping fetch for {}",
                url_opt.unwrap_or("local")
            );
            return Ok(());
        }
        let Some(url) = url_opt else {
            warn!(
                "Local repository missing commits: {:?}. Cannot fetch.",
                missing
            );
            return Ok(());
        };
        if self.is_own_repo(url) {
            warn!(
                "Repository is local but commits are missing: {:?}. Cannot fetch.",
                missing
            );
            return Ok(());
        }

        let remote_name = self.get_remote_name(url);
        // Redacted: a forge token can ride in the URL, and both the activity
        // and a failed request's reason are served to anyone who can read
        // the patchset.
        let remote = redact_secret(url);
        marks.mark(Phase::GitOp {
            op: format!("git remote setup for {}", remote),
        });
        if let Err(e) = self.ensure_remote(&remote_name, url).await {
            return Err(anyhow!(
                "Failed to set up remote {}: {}",
                remote,
                redact_secret(&e.to_string())
            ));
        }

        // 1. Try optimistic fetch (fetch specific commits)
        marks.mark(Phase::Fetching {
            remote: remote.clone(),
            commits: missing.len(),
        });
        let Err(e) = self.fetch_commits(&remote_name, missing).await else {
            return Ok(());
        };
        warn!(
            "Optimistic fetch failed for {}: {}. Falling back to full fetch.",
            remote, e
        );

        // 2. Fallback: Fetch everything (heads)
        marks.mark(Phase::Fetching {
            remote: format!("{} (all heads)", remote),
            commits: missing.len(),
        });
        self.fetch_all(&remote_name).await.map_err(|e| {
            anyhow!(
                "Failed to fetch from {}: {}",
                remote,
                redact_secret(&e.to_string())
            )
        })
    }

    /// Whether `url` names the fetcher's own repository, which has nothing
    /// to fetch from.
    fn is_own_repo(&self, url: &str) -> bool {
        match (
            std::fs::canonicalize(url),
            std::fs::canonicalize(&self.repo_path),
        ) {
            (Ok(canon_url), Ok(canon_repo)) => canon_url == canon_repo,
            _ => false,
        }
    }

    /// Fails every request that needed a commit the failed fetch was to
    /// bring, and returns the rest, whose commits were all present already
    /// and can still be submitted.
    async fn fail_unfetched(
        &self,
        requests: Vec<BatchRequest>,
        missing: &[String],
        error: &str,
    ) -> Vec<BatchRequest> {
        let (unfetched, ready): (Vec<_>, Vec<_>) =
            requests.into_iter().partition(|r| r.needs_any(missing));
        for request in unfetched {
            self.fail_request(&request.commit_or_range, error.to_string())
                .await;
        }
        ready
    }

    /// The article id every event about a request carries:
    /// `mr-{n}-{commit_or_range}` for a forge request, the request itself
    /// otherwise.
    ///
    /// The API's placeholder patchset is this id with `@sashiko.local`
    /// appended. An event named after anything else, such as one endpoint
    /// of a range, misses the placeholder and can match an unrelated
    /// patchset submitted for that commit alone.
    fn request_article_id(&self, commit_or_range: &str) -> String {
        match self.mr_fields(commit_or_range).2 {
            Some(number) => format!("mr-{}-{}", number, commit_or_range),
            None => commit_or_range.to_string(),
        }
    }

    /// The forge URL, title and number recorded for a request, if any.
    fn mr_fields(&self, commit_or_range: &str) -> (Option<String>, Option<String>, Option<i64>) {
        self.mr_metadata
            .get(commit_or_range)
            .cloned()
            .unwrap_or((None, None, None))
    }

    /// Ends a request with `IngestionFailed`, so that its placeholder
    /// patchset leaves Fetching with `error` as its reason.
    async fn fail_request(&self, commit_or_range: &str, error: String) {
        let event = Event::IngestionFailed {
            article_id: self.request_article_id(commit_or_range),
            error,
            source: MessageSource::GitFetch,
        };
        if let Err(e) = self.main_tx.send(event).await {
            error!("Failed to send IngestionFailed event: {}", e);
        }
    }

    /// Submits every commit of a `base..head` range as one series.
    async fn submit_range(&self, range: &str) {
        let shas = match crate::git_ops::resolve_git_range(&self.repo_path, range).await {
            Ok(shas) => shas,
            Err(e) => {
                self.fail_request(
                    range,
                    format!(
                        "Failed to resolve git range: {}",
                        redact_secret(&e.to_string())
                    ),
                )
                .await;
                return;
            }
        };
        let count = shas.len() as u32;
        let (mr_url, mr_title, mr_number) = self.mr_fields(range);
        let article_id = self.request_article_id(range);

        for (i, sha) in shas.iter().enumerate() {
            match self
                .extract_patch(
                    sha,
                    &article_id,
                    (i + 1) as u32,
                    count,
                    mr_url.as_ref(),
                    mr_title.as_ref(),
                    mr_number,
                )
                .await
            {
                Ok(event) => {
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
    }

    /// Submits a single commit as a one-patch series.
    async fn submit_commit(&self, commit: &str) {
        let full_sha = match self.resolve_sha(commit).await {
            Ok(sha) => sha,
            Err(e) => {
                self.fail_request(
                    commit,
                    format!("Failed to resolve SHA: {}", redact_secret(&e.to_string())),
                )
                .await;
                return;
            }
        };
        let (mr_url, mr_title, mr_number) = self.mr_fields(commit);
        let article_id = self.request_article_id(commit);

        match self
            .extract_patch(
                &full_sha,
                &article_id,
                1,
                1,
                mr_url.as_ref(),
                mr_title.as_ref(),
                mr_number,
            )
            .await
        {
            Ok(event) => {
                if let Err(e) = self.main_tx.send(event).await {
                    error!("Failed to send PatchSubmitted event: {}", e);
                } else {
                    info!("Successfully submitted remote patch {}", commit);
                }
            }
            Err(e) => {
                error!("Failed to extract patch {}: {}", commit, e);
                self.fail_request(
                    commit,
                    format!("Failed to extract patch: {}", redact_secret(&e.to_string())),
                )
                .await;
            }
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
        // Inject GitLab token via parsed url::Url to prevent parser differentials.
        let authenticated_url = if let Some(token) = &self.gitlab_token {
            if let Ok(mut parsed) = url::Url::parse(url)
                && parsed.scheme() == "https"
                && parsed
                    .host_str()
                    .is_some_and(|h| h.eq_ignore_ascii_case("gitlab.com"))
            {
                let _ = parsed.set_username("oauth2");
                let _ = parsed.set_password(Some(token));
                parsed.to_string()
            } else {
                url.to_string()
            }
        } else {
            url.to_string()
        };

        // Check if remote exists
        let status = crate::git_cmd::in_dir_async(&self.repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .args(["remote", "get-url", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;

        if status.success() {
            let output = crate::git_cmd::in_dir_async(&self.repo_path)
                .args(["-c", "safe.bareRepository=all"])
                .args(["remote", "get-url", name])
                .output()
                .await?;
            let current_url = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if current_url != authenticated_url {
                info!(
                    "Updating remote {} from {} to {}",
                    name,
                    redact_secret(&current_url),
                    redact_secret(&authenticated_url)
                );
                let output = crate::git_cmd::in_dir_async(&self.repo_path)
                    .args(["-c", "safe.bareRepository=all"])
                    .args(["remote", "set-url", name, &authenticated_url])
                    .output()
                    .await?;

                if !output.status.success() {
                    return Err(anyhow!(
                        "Failed to update remote: {}",
                        redact_secret(String::from_utf8_lossy(&output.stderr).trim())
                    ));
                }
            }
        } else {
            info!(
                "Adding remote {} -> {}",
                name,
                redact_secret(&authenticated_url)
            );
            let output = crate::git_cmd::in_dir_async(&self.repo_path)
                .args(["-c", "safe.bareRepository=all"])
                .args(["remote", "add", name, &authenticated_url])
                .output()
                .await?;

            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to add remote: {}",
                    redact_secret(String::from_utf8_lossy(&output.stderr).trim())
                ));
            }
        }
        Ok(())
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
            let output = crate::git_cmd::in_dir_async(&self.repo_path)
                .args(crate::git_ops::GIT_PROTOCOL_RESTRICTIONS)
                .arg("fetch")
                .args(args)
                .output()
                .await?;

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
        for commit in commits {
            if commit.is_empty()
                || commit.chars().any(char::is_whitespace)
                || commit.starts_with('-')
                || commit.starts_with('+')
                || commit.contains(':')
            {
                warn!("Skipping invalid commit ref in fetch batch: {}", commit);
                continue;
            }
            args.push(commit.as_str());
        }
        if args.len() == 1 {
            return Err(anyhow!("No valid commit refs to fetch"));
        }

        let output = self.fetch_with_graph_retry(&args).await?;
        if !output.status.success() {
            return Err(anyhow!(
                "Fetch failed: {}",
                redact_secret(String::from_utf8_lossy(&output.stderr).trim())
            ));
        }
        Ok(())
    }

    async fn fetch_all(&self, remote: &str) -> Result<()> {
        let output = self.fetch_with_graph_retry(&[remote]).await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Fetch all failed: {}",
                redact_secret(String::from_utf8_lossy(&output.stderr).trim())
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

        let output = crate::git_cmd::in_dir_async(&self.repo_path)
            .args(&args)
            .output()
            .await;

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
        let output = crate::git_cmd::in_dir_async(&self.repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .args(["rev-parse", "--verify", commit])
            .output()
            .await?;

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
    ) -> Result<Event> {
        let meta = crate::git_ops::extract_patch_metadata(&self.repo_path, commit).await?;

        Ok(Event::PatchSubmitted {
            group: "git-fetch".to_string(),
            article_id: article_id.to_string(),
            message_id: commit.to_string(),
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    /// A remote that refuses every connection at once, so a fetch from it
    /// fails without waiting on the network.
    const REFUSING_REMOTE: &str = "http://127.0.0.1:1/nonexistent.git";
    const MISSING_BASE: &str = "1111111111111111111111111111111111111111";
    const MISSING_HEAD: &str = "2222222222222222222222222222222222222222";

    /// Creates a repository with one commit and returns that commit's SHA.
    async fn init_repo_with_commit(repo_path: &std::path::Path) -> Result<String> {
        for args in [
            &["init"][..],
            &["config", "user.name", "Test User"],
            &["config", "user.email", "test@example.com"],
        ] {
            crate::git_cmd::in_dir_async(repo_path)
                .args(args)
                .output()
                .await?;
        }

        let mut file = File::create(repo_path.join("file.txt"))?;
        writeln!(file, "content")?;

        crate::git_cmd::in_dir_async(repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(repo_path)
            .args(["commit", "-m", "Subject Line\n\nBody Line"])
            .output()
            .await?;

        let output = crate::git_cmd::in_dir_async(repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    /// Runs one batch of `requests` against the refusing remote and returns
    /// every event the agent sent for it.
    async fn run_refused_batch(
        agent: &FetchAgent,
        rx: &mut mpsc::Receiver<Event>,
        requests: &[&str],
    ) -> Vec<Event> {
        let mut queue = HashMap::new();
        queue.insert(
            Some(REFUSING_REMOTE.to_string()),
            requests.iter().map(|r| r.to_string()).collect(),
        );
        agent.process_queue(&mut queue).await;

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// The article ids of the `IngestionFailed` events among `events`.
    fn failed_ids(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::IngestionFailed { article_id, .. } => Some(article_id.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The article ids of the `PatchSubmitted` events among `events`.
    fn submitted_ids(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::PatchSubmitted { article_id, .. } => Some(article_id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn test_fetch_agent_lifecycle() {
        let (tx, _rx) = mpsc::channel(1);
        let repo_path = PathBuf::from("/tmp");
        let (_agent, _sender) = FetchAgent::new(repo_path, tx, None, ActivityRegistry::new());
    }

    #[tokio::test]
    async fn test_extract_patch_parsing() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();
        let head = init_repo_with_commit(&repo_path).await?;

        let (tx, _rx) = mpsc::channel(1);
        let (agent, _) = FetchAgent::new(repo_path.clone(), tx, None, ActivityRegistry::new());

        let event = agent
            .extract_patch(&head, &head, 1, 1, None, None, None)
            .await?;

        match event {
            Event::PatchSubmitted {
                subject,
                author,
                message,
                diff,
                article_id,
                message_id,
                ..
            } => {
                assert_eq!(subject, "Subject Line");
                assert_eq!(author, "Test User <test@example.com>");
                assert!(message.contains("Body Line"));
                assert!(diff.contains("diff --git"));
                assert_eq!(article_id, head);
                assert_eq!(message_id, head);
            }
            _ => panic!("Wrong event type"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_is_present_with_tree_sha() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();
        let commit_sha = init_repo_with_commit(&repo_path).await?;

        let (tx, _rx) = mpsc::channel(1);
        let (agent, _) = FetchAgent::new(repo_path.clone(), tx, None, ActivityRegistry::new());

        let output = crate::git_cmd::in_dir_async(&repo_path)
            .args(["rev-parse", "HEAD^{tree}"])
            .output()
            .await?;
        let tree_sha = String::from_utf8(output.stdout)?.trim().to_string();

        assert!(
            !agent.is_present(&tree_sha).await,
            "Tree SHA should not be considered a present commit"
        );
        assert!(
            agent.is_present(&commit_sha).await,
            "Commit SHA should be considered present"
        );

        Ok(())
    }

    #[test]
    fn batch_request_splits_a_range_into_its_endpoints() {
        let range = BatchRequest::new("a..b".to_string());
        assert_eq!(range.endpoints, ["a", "b"]);
        let symmetric = BatchRequest::new("a...b".to_string());
        assert_eq!(symmetric.endpoints, ["a", "b"]);
        let commit = BatchRequest::new("a".to_string());
        assert_eq!(commit.endpoints, ["a"]);
        assert!(range.needs_any(&["b".to_string()]));
        assert!(!range.needs_any(&["c".to_string()]));
    }

    /// A failed range fetch fails the range, whose placeholder the API
    /// created, and not the endpoint SHAs, which name no placeholder.
    #[tokio::test]
    async fn failed_range_fetch_names_the_range() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        init_repo_with_commit(temp_dir.path()).await?;
        let (tx, mut rx) = mpsc::channel(16);
        let (agent, _) = FetchAgent::new(
            temp_dir.path().to_path_buf(),
            tx,
            None,
            ActivityRegistry::new(),
        );

        let range = format!("{}..{}", MISSING_BASE, MISSING_HEAD);
        let events = run_refused_batch(&agent, &mut rx, &[&range]).await;

        assert_eq!(failed_ids(&events), [range.as_str()]);
        assert!(submitted_ids(&events).is_empty());
        let Some(Event::IngestionFailed { error, .. }) = events.first() else {
            panic!("expected IngestionFailed, got {:?}", events.first());
        };
        assert!(
            error.starts_with(&format!("Failed to fetch from {}: ", REFUSING_REMOTE)),
            "the reason should be the fetch error, got: {}",
            error
        );
        Ok(())
    }

    /// A forge request's failure carries the `mr-{n}-` prefix its placeholder
    /// was created with, for a range and for a single commit alike.
    #[tokio::test]
    async fn failed_forge_fetch_names_the_placeholder() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        init_repo_with_commit(temp_dir.path()).await?;
        let (tx, mut rx) = mpsc::channel(16);
        let (mut agent, _) = FetchAgent::new(
            temp_dir.path().to_path_buf(),
            tx,
            None,
            ActivityRegistry::new(),
        );

        let range = format!("{}..{}", MISSING_BASE, MISSING_HEAD);
        let commit = "3333333333333333333333333333333333333333";
        for request in [range.as_str(), commit] {
            agent
                .mr_metadata
                .insert(request.to_string(), (None, None, Some(7)));
        }

        let events = run_refused_batch(&agent, &mut rx, &[&range]).await;
        assert_eq!(failed_ids(&events), [format!("mr-7-{}", range)]);

        let events = run_refused_batch(&agent, &mut rx, &[commit]).await;
        assert_eq!(failed_ids(&events), [format!("mr-7-{}", commit)]);
        Ok(())
    }

    /// A request whose commits were all present is still submitted when the
    /// fetch for another request in its batch fails.
    #[tokio::test]
    async fn present_request_survives_a_failed_fetch() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let head = init_repo_with_commit(temp_dir.path()).await?;
        let (tx, mut rx) = mpsc::channel(16);
        let (agent, _) = FetchAgent::new(
            temp_dir.path().to_path_buf(),
            tx,
            None,
            ActivityRegistry::new(),
        );

        let range = format!("{}..{}", MISSING_BASE, MISSING_HEAD);
        let events = run_refused_batch(&agent, &mut rx, &[&range, &head]).await;

        assert_eq!(failed_ids(&events), [range.as_str()]);
        assert_eq!(submitted_ids(&events), [head.as_str()]);
        Ok(())
    }
}
