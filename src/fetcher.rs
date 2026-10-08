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
use std::process::Output;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{Duration, interval};
use tracing::{error, info, warn};

/// How long a fetcher git command may run before it is killed.
///
/// `FetchAgent::run` awaits each batch inline, so one git command that never
/// returns, such as a fetch from a remote that stopped answering, would stop
/// every later fetch and leave its patchsets in Fetching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitOp {
    /// rev-parse, rev-list, remote get-url/set-url/add/remove.
    Local,
    /// One `git fetch` attempt against a remote that existed before this
    /// batch.
    Fetch,
    /// One `git fetch` attempt against a remote `ensure_remote` just added.
    /// It may have to transfer much more, as `refresh_remote` allows for.
    FirstFetch,
}

impl GitOp {
    fn limit(self) -> Duration {
        match self {
            // Local commands never touch the network, so anything this slow
            // is pathological.
            Self::Local => Duration::from_secs(30),
            Self::Fetch => Duration::from_secs(300),
            // The budget `git_ops::refresh_remote` gives the same kind of
            // fetch: a remote it just added.
            Self::FirstFetch => Duration::from_secs(1800),
        }
    }
}

/// What `ensure_remote` found, which sets the budget of the first fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteSetup {
    /// The remote did not exist and was added for this batch.
    Added,
    /// The remote was already configured; its URL now matches the request.
    Existing,
}

impl RemoteSetup {
    /// The budget each fetch attempt from this remote runs under.
    fn fetch_op(self) -> GitOp {
        match self {
            Self::Added => GitOp::FirstFetch,
            Self::Existing => GitOp::Fetch,
        }
    }
}

/// A fetcher git command that ran past its limit and was killed.
///
/// The limit prints as `Duration`'s `Debug`, "300s" for the real limits and
/// "300ms" for a test's, so a sub-second limit does not read as "0s".
#[derive(Debug, thiserror::Error)]
#[error("{what} timed out after {limit:?}")]
struct GitTimedOut {
    what: String,
    limit: Duration,
}

/// Options that make curl end an https transfer moving under 1000 bytes a
/// second for 60 seconds. A stalled transfer then ends in about a minute,
/// in `git` and in its `git-remote-https` child alike, instead of at the
/// wall-clock limit; killing `git` at that limit does not kill the child.
const LOW_SPEED_LIMIT: &[&str] = &[
    "-c",
    "http.lowSpeedLimit=1000",
    "-c",
    "http.lowSpeedTime=60",
];

/// Runs `cmd` and kills it when it runs past `limit`.
///
/// `kill_on_drop` matters as much as the limit: the timeout drops the
/// future, and without it the abandoned `git` would keep running, and keep
/// its repository locks, until it ended on its own. `GIT_TERMINAL_PROMPT=0`
/// makes an https remote that wants credentials fail at once instead of
/// waiting on git's terminal prompt for the whole limit; credential helpers
/// still run. ssh prompts on `/dev/tty` itself and ignores the variable, so
/// an ssh remote that needs a prompt still waits out the limit. stdin needs
/// no setting because `output()` already closes it.
async fn run_git_within(mut cmd: Command, limit: Duration, what: &str) -> Result<Output> {
    cmd.kill_on_drop(true).env("GIT_TERMINAL_PROMPT", "0");
    match tokio::time::timeout(limit, cmd.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(anyhow!("Failed to execute {}: {}", what, e)),
        Err(_) => Err(GitTimedOut {
            what: what.to_string(),
            limit,
        }
        .into()),
    }
}

/// The arguments of one `git fetch` with `args`, global options first.
fn fetch_args<'a>(args: &[&'a str]) -> Vec<&'a str> {
    crate::git_ops::GIT_PROTOCOL_RESTRICTIONS
        .iter()
        .chain(LOW_SPEED_LIMIT)
        .copied()
        .chain(std::iter::once("fetch"))
        .chain(args.iter().copied())
        .collect()
}

/// Whether `error` is a git command killed at its limit, as opposed to one
/// that ended on its own. A remote that stopped answering is handled
/// differently from one that answered with a failure.
fn timed_out(error: &anyhow::Error) -> bool {
    error.downcast_ref::<GitTimedOut>().is_some()
}

/// A ref under which a forge publishes the head of a pull or merge request
/// on the base repository, where a head pushed to a fork is found too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForgeHeadRef {
    /// GitHub's `refs/pull/N/head`.
    Pull,
    /// GitLab's `refs/merge-requests/N/head`.
    MergeRequest,
}

impl ForgeHeadRef {
    /// The order the refs are tried in. GitHub's comes first because it is
    /// the forge the deployments use; each is its own fetch, so the other
    /// forge's ref costs one failed command only when the first fails.
    const ORDER: [Self; 2] = [Self::Pull, Self::MergeRequest];

    /// The full ref name for request `number`.
    fn name(self, number: i64) -> String {
        match self {
            Self::Pull => format!("refs/pull/{}/head", number),
            Self::MergeRequest => format!("refs/merge-requests/{}/head", number),
        }
    }
}

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
    /// The limit each `GitOp` runs under: `GitOp::limit`, except in tests
    /// that need a hung fetch to time out in milliseconds.
    git_limit: fn(GitOp) -> Duration,
    /// The arguments after the remote name of every `git fetch` run, in
    /// order, so tests can tell which fallbacks ran and in what order.
    #[cfg(test)]
    fetches: std::sync::Mutex<Vec<Vec<String>>>,
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

/// The batch requests for one pull or merge request, whose head the forge
/// also publishes under a ref (`ForgeHeadRef`) on the repository fetched
/// from. Several requests share a number when an older head of the pull
/// request is still queued beside the newer one, and the ref is fetched
/// once for all of them.
struct ForgeHead<'a> {
    number: i64,
    requests: Vec<&'a BatchRequest>,
}

impl ForgeHead<'_> {
    /// Every endpoint of the requests, each once, in request order.
    fn endpoints(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.requests
            .iter()
            .flat_map(|request| &request.endpoints)
            .filter(|endpoint| seen.insert(endpoint.as_str()))
            .cloned()
            .collect()
    }
}

/// What fetching one pull request's forge refs came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForgeHeadFetch {
    /// Every request for the pull request has all its commits.
    Complete,
    /// The forge served this ref, but a request still misses a commit: an
    /// older head that was force-pushed away, which the ref no longer
    /// names, or a base the head does not reach, left to the heads fetch.
    /// The forge answered, so read access is not what is missing.
    Served(ForgeHeadRef),
    /// Every ref fetch failed, and a request still misses a commit.
    Refused,
}

impl ForgeHeadFetch {
    /// The outcome once the refs were tried: `served` is the ref the forge
    /// answered, if any, and `complete` whether every request now has all
    /// its commits.
    fn after(served: Option<ForgeHeadRef>, complete: bool) -> Self {
        match (complete, served) {
            (true, _) => Self::Complete,
            (false, Some(kind)) => Self::Served(kind),
            (false, None) => Self::Refused,
        }
    }

    /// Logs what an operator can act on: a WARN with the credentials fix
    /// when the forge refused every ref, since then the daemon's user may
    /// lack read access, and an info line when a served ref still left a
    /// commit missing, since credentials cannot help there.
    fn report(self, remote: &str, number: i64) {
        match self {
            Self::Complete => {}
            Self::Served(kind) => info!(
                "{} from {} did not bring every commit of pull request {}: the head of an \
                 older request was force-pushed away, or the base is left to the heads fetch.",
                kind.name(number),
                remote,
                number
            ),
            Self::Refused => warn!(
                "Could not fetch the head of pull request {} from {}: the forge refused the \
                 fetch by SHA and both {} and {}. The daemon's user needs read credentials \
                 for {}: run gh auth setup-git as that user, or add a \
                 url.<ssh-url>.insteadOf {} rule.",
                number,
                remote,
                ForgeHeadRef::Pull.name(number),
                ForgeHeadRef::MergeRequest.name(number),
                remote,
                remote
            ),
        }
    }
}

/// The configured remote one batch fetches from, and the budget of each
/// fetch attempt against it.
struct FetchSource<'a> {
    /// The `fetcher-<hash>` remote git fetches through.
    remote_name: &'a str,
    /// The remote's URL with any secret redacted, for logs and activity.
    remote: &'a str,
    op: GitOp,
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
                git_limit: GitOp::limit,
                #[cfg(test)]
                fetches: std::sync::Mutex::new(Vec::new()),
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

        let requests = match self
            .fetch_missing(url_opt, &requests, &missing, &marks)
            .await
        {
            Ok(()) => requests,
            Err(e) => {
                error!("{}", e);
                // A pull request head fetched through its forge ref before a
                // later attempt failed is present now, so only the requests
                // still missing a commit fail.
                let unfetched = self.still_missing(&missing).await;
                self.fail_unfetched(requests, &unfetched, &e.to_string())
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
        let endpoints: Vec<String> = requests
            .iter()
            .flat_map(|r| &r.endpoints)
            .filter(|endpoint| checked.insert(endpoint.as_str()))
            .cloned()
            .collect();
        self.still_missing(&endpoints).await
    }

    /// Those of `endpoints` that are not in the repository, in order.
    async fn still_missing(&self, endpoints: &[String]) -> Vec<String> {
        let mut missing = Vec::new();
        for endpoint in endpoints {
            if !self.is_present(endpoint).await {
                missing.push(endpoint.clone());
            }
        }
        missing
    }

    /// The requests that name a pull or merge request and need one of the
    /// `missing` commits, grouped by number in order of first appearance. A
    /// number that is not positive names no forge ref and is left out.
    fn forge_heads<'a>(
        &self,
        requests: &'a [BatchRequest],
        missing: &[String],
    ) -> Vec<ForgeHead<'a>> {
        let numbered = requests
            .iter()
            .filter(|request| request.needs_any(missing))
            .filter_map(|request| {
                let number = self.mr_fields(&request.commit_or_range).2?;
                (number > 0).then_some((number, request))
            });
        let mut heads: Vec<ForgeHead<'a>> = Vec::new();
        for (number, request) in numbered {
            match heads.iter_mut().find(|head| head.number == number) {
                Some(head) => head.requests.push(request),
                None => heads.push(ForgeHead {
                    number,
                    requests: vec![request],
                }),
            }
        }
        heads
    }

    /// Brings the missing endpoints in from the batch's remote.
    ///
    /// Returns `Ok` without fetching when nothing is missing or there is no
    /// other repository to fetch from; a request whose commits are still
    /// absent then fails on its own when it is submitted. An `Err` carries
    /// the wording that every request still missing a commit when the fetch
    /// ends fails with; a request whose commits an earlier attempt brought
    /// is submitted all the same.
    async fn fetch_missing(
        &self,
        url_opt: Option<&str>,
        requests: &[BatchRequest],
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
        let setup = self.ensure_remote(&remote_name, url).await.map_err(|e| {
            anyhow!(
                "Failed to set up remote {}: {}",
                remote,
                redact_secret(&e.to_string())
            )
        })?;

        let source = FetchSource {
            remote_name: &remote_name,
            remote: &remote,
            op: setup.fetch_op(),
        };
        let forge_heads = self.forge_heads(requests, missing);
        let fetched = self.fetch_from(&source, missing, &forge_heads, marks).await;
        if let Err(e) = &fetched
            && setup == RemoteSetup::Added
            && !timed_out(e)
        {
            // The next request for this URL then adds the remote again and
            // gets the first-fetch budget, instead of retrying a transfer
            // that needed more under the shorter one. A remote whose first
            // fetch timed out is kept: it had the whole first-fetch budget,
            // a fetch does not resume, and removing it would hold the queue
            // for that budget again on every later request.
            self.remove_remote(&remote_name).await;
        }
        fetched.map_err(|e| {
            anyhow!(
                "Failed to fetch from {}: {}",
                remote,
                redact_secret(&e.to_string())
            )
        })
    }

    /// Fetches the missing commits from an already configured remote, each
    /// attempt under the source's limit: first the commits by SHA; when that
    /// fails for any reason other than a timeout, each pull request head in
    /// `forge_heads` through its forge refs, then every head. The error is
    /// the last attempt's own, so the caller can tell a timeout.
    async fn fetch_from(
        &self,
        source: &FetchSource<'_>,
        missing: &[String],
        forge_heads: &[ForgeHead<'_>],
        marks: &BatchMarks<'_>,
    ) -> Result<()> {
        marks.mark(Phase::Fetching {
            remote: source.remote.to_string(),
            commits: missing.len(),
        });
        let Err(e) = self
            .fetch_wants(source.remote_name, missing, source.op)
            .await
        else {
            return Ok(());
        };
        // A remote that did not answer a fetch of a few commits will not
        // answer a fetch of every head, and the fallback would hold the
        // queue for a second full limit.
        if timed_out(&e) {
            return Err(e);
        }
        warn!(
            "Optimistic fetch failed for {}: {}. Falling back to any pull request refs, then every head.",
            source.remote,
            redact_secret(&e.to_string())
        );

        // One refused want fails the whole fetch above, and the heads below
        // never hold a head pushed to a fork, so each pull request's head is
        // asked for by the ref its forge publishes it under.
        self.fetch_forge_heads(source, forge_heads, marks).await?;

        marks.mark(Phase::Fetching {
            remote: format!("{} (all heads)", source.remote),
            commits: self.still_missing(missing).await.len(),
        });
        self.fetch_all(source.remote_name, source.op).await
    }

    /// Fetches the head of each pull request in `forge_heads` whose requests
    /// still miss a commit through its forge refs, and reports each outcome.
    /// Only a timeout is an error, and it stops the rest: a remote that did
    /// not answer one ref will not answer the next.
    async fn fetch_forge_heads(
        &self,
        source: &FetchSource<'_>,
        forge_heads: &[ForgeHead<'_>],
        marks: &BatchMarks<'_>,
    ) -> Result<()> {
        for head in forge_heads {
            self.fetch_forge_head(source, head, marks)
                .await?
                .report(source.remote, head.number);
        }
        Ok(())
    }

    /// Fetches one pull request's head through its forge refs when any of
    /// its requests still misses a commit, and says what that came to.
    async fn fetch_forge_head(
        &self,
        source: &FetchSource<'_>,
        head: &ForgeHead<'_>,
        marks: &BatchMarks<'_>,
    ) -> Result<ForgeHeadFetch> {
        let endpoints = head.endpoints();
        let missing = self.still_missing(&endpoints).await;
        if missing.is_empty() {
            return Ok(ForgeHeadFetch::Complete);
        }
        marks.mark(Phase::Fetching {
            remote: format!("{} (pull request {} head)", source.remote, head.number),
            commits: missing.len(),
        });
        let served = self.fetch_first_forge_ref(source, head.number).await?;
        let complete = self.still_missing(&endpoints).await.is_empty();
        Ok(ForgeHeadFetch::after(served, complete))
    }

    /// Fetches the forge refs of pull request `number` in `ForgeHeadRef`
    /// order until the forge serves one, and returns that one, or `None`
    /// when it refuses them all. A forge that served one ref is not the
    /// forge the others belong to, so they are not tried after it. Each ref
    /// is its own command because a ref the forge does not have fails the
    /// whole fetch. A ref fetch names no destination, so it writes only
    /// `FETCH_HEAD`, as the fetch by SHA does, and leaves no ref behind.
    async fn fetch_first_forge_ref(
        &self,
        source: &FetchSource<'_>,
        number: i64,
    ) -> Result<Option<ForgeHeadRef>> {
        for kind in ForgeHeadRef::ORDER {
            let refname = kind.name(number);
            let wants = std::slice::from_ref(&refname);
            match self.fetch_wants(source.remote_name, wants, source.op).await {
                Ok(()) => return Ok(Some(kind)),
                Err(e) if timed_out(&e) => return Err(e),
                Err(e) => info!(
                    "Fetching {} from {} failed: {}",
                    refname,
                    source.remote,
                    redact_secret(&e.to_string())
                ),
            }
        }
        Ok(None)
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

    /// Fails every request that needs one of the `missing` commits, those
    /// still absent when the failed fetch ended, and returns the rest, whose
    /// commits are all present now, either from before the fetch or brought
    /// by an attempt that succeeded before a later one failed, and which
    /// can still be submitted.
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

    /// Runs a fetcher git command under `op`'s limit.
    async fn run_git(&self, cmd: Command, op: GitOp, what: &str) -> Result<Output> {
        run_git_within(cmd, (self.git_limit)(op), what).await
    }

    /// A git command in the fetcher's repository with `args`, which may be
    /// a bare repository.
    fn local_git(&self, args: &[&str]) -> Command {
        let mut cmd = crate::git_cmd::in_dir_async(&self.repo_path);
        cmd.args(["-c", "safe.bareRepository=all"]).args(args);
        cmd
    }

    /// The URL to configure for `url`, with the GitLab token in it when the
    /// remote is on gitlab.com.
    fn authenticated_url(&self, url: &str) -> String {
        // Inject GitLab token via parsed url::Url to prevent parser differentials.
        if let Some(token) = &self.gitlab_token {
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
        }
    }

    /// Points remote `name` at `url`, adding it when it does not exist, and
    /// says which of the two it did.
    async fn ensure_remote(&self, name: &str, url: &str) -> Result<RemoteSetup> {
        let authenticated_url = self.authenticated_url(url);

        // One get-url says both whether the remote exists, by its exit
        // status, and where it points, by its output.
        let existing = self
            .run_git(
                self.local_git(&["remote", "get-url", name]),
                GitOp::Local,
                "git remote get-url",
            )
            .await?;

        if !existing.status.success() {
            info!(
                "Adding remote {} -> {}",
                name,
                redact_secret(&authenticated_url)
            );
            let output = self
                .run_git(
                    self.local_git(&["remote", "add", name, &authenticated_url]),
                    GitOp::Local,
                    "git remote add",
                )
                .await?;
            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to add remote: {}",
                    redact_secret(String::from_utf8_lossy(&output.stderr).trim())
                ));
            }
            return Ok(RemoteSetup::Added);
        }

        let current_url = String::from_utf8_lossy(&existing.stdout).trim().to_string();
        if current_url != authenticated_url {
            info!(
                "Updating remote {} from {} to {}",
                name,
                redact_secret(&current_url),
                redact_secret(&authenticated_url)
            );
            let output = self
                .run_git(
                    self.local_git(&["remote", "set-url", name, &authenticated_url]),
                    GitOp::Local,
                    "git remote set-url",
                )
                .await?;
            // Checked because a remote left at its old URL would fetch the
            // wrong repository and report the requested commits missing.
            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to update remote URL: {}",
                    redact_secret(String::from_utf8_lossy(&output.stderr).trim())
                ));
            }
        }
        Ok(RemoteSetup::Existing)
    }

    /// Removes remote `name`. A failure is only logged: the remote then
    /// stays, and the next request for its URL fetches from it as an
    /// existing remote.
    async fn remove_remote(&self, name: &str) {
        let removed = self
            .run_git(
                self.local_git(&["remote", "remove", name]),
                GitOp::Local,
                "git remote remove",
            )
            .await;
        match removed {
            Ok(output) if output.status.success() => {
                info!("Removed remote {} after its first fetch failed", name);
            }
            Ok(output) => warn!(
                "Failed to remove remote {}: {}",
                name,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(e) => warn!("Failed to remove remote {}: {}", name, e),
        }
    }

    /// Runs one `git fetch`, dropping a stale commit-graph and trying
    /// again when that is what turned the fetch away.  Fetch-pack
    /// rejects the graph before it opens a connection, so that retry
    /// repeats no transfer; the wording the commit parse emits can
    /// come after one, and then the retry pays for it again.  Returns
    /// git's own output either way; the caller words the failure.
    ///
    /// Each attempt runs under `op`'s limit. A timeout returns `Err`, and
    /// so never reaches the commit-graph retry.
    async fn fetch_with_graph_retry(&self, args: &[&str], op: GitOp) -> Result<Output> {
        let mut dropped_graph = false;

        loop {
            #[cfg(test)]
            if let Ok(mut fetches) = self.fetches.lock() {
                fetches.push(args.iter().skip(1).map(|a| a.to_string()).collect());
            }
            let mut cmd = crate::git_cmd::in_dir_async(&self.repo_path);
            cmd.args(fetch_args(args));
            let output = self.run_git(cmd, op, "git fetch").await?;

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

    /// Fetches `wants`, commit SHAs or ref names, from `remote` in one
    /// command with no destination, so only `FETCH_HEAD` is written.
    async fn fetch_wants(&self, remote: &str, wants: &[String], op: GitOp) -> Result<()> {
        let mut args = vec![remote];
        for want in wants {
            if want.is_empty()
                || want.chars().any(char::is_whitespace)
                || want.starts_with('-')
                || want.starts_with('+')
                || want.contains(':')
            {
                warn!("Skipping invalid commit ref in fetch batch: {}", want);
                continue;
            }
            args.push(want.as_str());
        }
        if args.len() == 1 {
            return Err(anyhow!("No valid commit refs to fetch"));
        }

        let output = self.fetch_with_graph_retry(&args, op).await?;
        if !output.status.success() {
            return Err(anyhow!(
                "Fetch failed: {}",
                redact_secret(String::from_utf8_lossy(&output.stderr).trim())
            ));
        }
        Ok(())
    }

    async fn fetch_all(&self, remote: &str, op: GitOp) -> Result<()> {
        let output = self.fetch_with_graph_retry(&[remote], op).await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Fetch all failed: {}",
                redact_secret(String::from_utf8_lossy(&output.stderr).trim())
            ));
        }
        Ok(())
    }

    async fn is_present(&self, commit_or_range: &str) -> bool {
        let arg_str: String;
        let (args, what) = if let Some((start, end)) = commit_or_range.split_once("..") {
            arg_str = format!("{}^{{commit}}..{}^{{commit}}", start, end);
            (["rev-list", "-n", "1", &arg_str].to_vec(), "git rev-list")
        } else {
            arg_str = format!("{}^{{commit}}", commit_or_range);
            (
                ["rev-parse", "--verify", &arg_str].to_vec(),
                "git rev-parse",
            )
        };

        let output = self
            .run_git(self.local_git(&args), GitOp::Local, what)
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
        let output = self
            .run_git(
                self.local_git(&["rev-parse", "--verify", commit]),
                GitOp::Local,
                "git rev-parse",
            )
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
        run_batch(agent, rx, REFUSING_REMOTE, requests).await
    }

    /// Runs one batch of `requests` against `url` and returns every event
    /// the agent sent for it.
    async fn run_batch(
        agent: &FetchAgent,
        rx: &mut mpsc::Receiver<Event>,
        url: &str,
        requests: &[&str],
    ) -> Vec<Event> {
        let mut queue = HashMap::new();
        queue.insert(
            Some(url.to_string()),
            requests.iter().map(|r| r.to_string()).collect(),
        );
        agent.process_queue(&mut queue).await;
        drain(rx)
    }

    /// Every event waiting on `rx`, in order.
    fn drain(rx: &mut mpsc::Receiver<Event>) -> Vec<Event> {
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

    /// The reasons of the `IngestionFailed` events among `events`.
    fn failure_reasons(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::IngestionFailed { error, .. } => Some(error.as_str()),
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

    /// A fetcher agent over a fresh repository with one commit.
    async fn agent_with_repo(dir: &std::path::Path) -> Result<FetchAgent> {
        init_repo_with_commit(dir).await?;
        let (tx, _rx) = mpsc::channel(16);
        let (agent, _) = FetchAgent::new(dir.to_path_buf(), tx, None, ActivityRegistry::new());
        Ok(agent)
    }

    /// The URL remote `name` points at, or `None` when it does not exist.
    async fn remote_url(repo: &std::path::Path, name: &str) -> Result<Option<String>> {
        let output = crate::git_cmd::in_dir_async(repo)
            .args(["remote", "get-url", name])
            .output()
            .await?;
        Ok(output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string()))
    }

    /// Whether process `pid` is still running. A zombie has ended and only
    /// waits to be reaped, so it does not count.
    fn is_running(pid: &str) -> bool {
        std::fs::read_to_string(format!("/proc/{}/stat", pid))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(") ")
                    .map(|(_, rest)| !rest.starts_with('Z'))
            })
            .unwrap_or(false)
    }

    /// `sleep` stands in for a `git fetch` against a remote that stopped
    /// answering: it never returns on its own, so only the limit can end
    /// it. Before the limit, such a command wedged the whole agent. The
    /// command must also be killed, not merely abandoned, or it would keep
    /// running and keep its repository locks.
    #[tokio::test]
    async fn run_git_gives_up_on_a_hung_command() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let pid_file = temp_dir.path().join("pid");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo $$ > \"$1\"; exec sleep 60", "sh"])
            .arg(&pid_file);

        let started = std::time::Instant::now();
        let result = run_git_within(cmd, Duration::from_millis(500), "sleep").await;
        let elapsed = started.elapsed();

        let Err(err) = result else {
            panic!("a hung command must return an error");
        };
        assert!(
            err.downcast_ref::<GitTimedOut>().is_some(),
            "expected a GitTimedOut, got: {}",
            err
        );
        assert!(err.to_string().contains("timed out"), "got: {}", err);
        assert!(
            elapsed < Duration::from_secs(5),
            "run_git waited {:?}; it should have given up at the limit",
            elapsed
        );

        // SIGKILL is delivered asynchronously, so allow it a moment.
        let pid = std::fs::read_to_string(&pid_file)?.trim().to_string();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while is_running(&pid) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !is_running(&pid),
            "the timed-out command {} still runs",
            pid
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_git_returns_output_on_success() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let (tx, _rx) = mpsc::channel(16);
        let (agent, _) = FetchAgent::new(
            temp_dir.path().to_path_buf(),
            tx,
            None,
            ActivityRegistry::new(),
        );
        let mut cmd = crate::git_cmd::detached_async();
        cmd.arg("--version");

        let output = agent.run_git(cmd, GitOp::Local, "git --version").await?;

        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("git version"));
        Ok(())
    }

    /// A refused fetch is an ordinary failure, not a timeout, so the
    /// fallback rule would still allow `fetch_all` after it.
    #[tokio::test]
    async fn fetch_all_reports_unreachable_remote_as_error() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let agent = agent_with_repo(temp_dir.path()).await?;
        agent.ensure_remote("broken", REFUSING_REMOTE).await?;

        let Err(err) = agent.fetch_all("broken", GitOp::Fetch).await else {
            panic!("a fetch from an unreachable remote must fail");
        };
        assert!(!timed_out(&err), "got: {}", err);
        Ok(())
    }

    /// The first call adds the remote; a later call with another URL finds
    /// it and points it there, through the single get-url and the checked
    /// set-url.
    #[tokio::test]
    async fn ensure_remote_adds_then_updates_url() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let agent = agent_with_repo(temp_dir.path()).await?;
        let first = "https://example.com/first.git";
        let second = "https://example.com/second.git";

        assert_eq!(
            agent.ensure_remote("origin", first).await?,
            RemoteSetup::Added
        );
        assert_eq!(
            remote_url(temp_dir.path(), "origin").await?.as_deref(),
            Some(first)
        );

        assert_eq!(
            agent.ensure_remote("origin", second).await?,
            RemoteSetup::Existing
        );
        assert_eq!(
            remote_url(temp_dir.path(), "origin").await?.as_deref(),
            Some(second)
        );

        assert_eq!(
            agent.ensure_remote("origin", second).await?,
            RemoteSetup::Existing
        );
        Ok(())
    }

    /// A remote added for a batch whose fetch failed is removed again, so
    /// the next request for its URL is a first fetch with the longer limit.
    #[tokio::test]
    async fn failed_first_fetch_removes_the_remote() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        init_repo_with_commit(temp_dir.path()).await?;
        let (tx, mut rx) = mpsc::channel(16);
        let (agent, _) = FetchAgent::new(
            temp_dir.path().to_path_buf(),
            tx,
            None,
            ActivityRegistry::new(),
        );
        let name = agent.get_remote_name(REFUSING_REMOTE);

        let events = run_refused_batch(&agent, &mut rx, &[MISSING_HEAD]).await;

        assert_eq!(failed_ids(&events), [MISSING_HEAD]);
        assert_eq!(remote_url(temp_dir.path(), &name).await?, None);
        Ok(())
    }

    /// A remote that existed before the batch is kept when its fetch fails;
    /// only one the batch added is taken back.
    #[tokio::test]
    async fn failed_fetch_keeps_an_existing_remote() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        init_repo_with_commit(temp_dir.path()).await?;
        let (tx, mut rx) = mpsc::channel(16);
        let (agent, _) = FetchAgent::new(
            temp_dir.path().to_path_buf(),
            tx,
            None,
            ActivityRegistry::new(),
        );
        let name = agent.get_remote_name(REFUSING_REMOTE);
        agent.ensure_remote(&name, REFUSING_REMOTE).await?;

        let events = run_refused_batch(&agent, &mut rx, &[MISSING_HEAD]).await;

        assert_eq!(failed_ids(&events), [MISSING_HEAD]);
        assert_eq!(
            remote_url(temp_dir.path(), &name).await?.as_deref(),
            Some(REFUSING_REMOTE)
        );
        Ok(())
    }

    #[test]
    fn timed_out_tells_a_timeout_from_a_failure() {
        let timeout = anyhow::Error::from(GitTimedOut {
            what: "git fetch".to_string(),
            limit: GitOp::Fetch.limit(),
        });
        assert!(timed_out(&timeout));
        assert_eq!(timeout.to_string(), "git fetch timed out after 300s");
        assert!(!timed_out(&anyhow!("Fetch failed: refused")));
    }

    /// An http remote that accepts connections and never answers, as a
    /// server that stopped responding does, counting the connections.
    struct HungRemote {
        url: String,
        connections: Arc<std::sync::atomic::AtomicUsize>,
        server: tokio::task::JoinHandle<()>,
    }

    impl HungRemote {
        async fn start() -> Result<Self> {
            Self::start_after_refusing(0).await
        }

        /// A remote that closes its first `refused` connections at once, so
        /// those fetches fail without a timeout, and hangs on the rest.
        async fn start_after_refusing(refused: usize) -> Result<Self> {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let url = format!("http://{}/hung.git", listener.local_addr()?);
            let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = Arc::clone(&connections);
            let server = tokio::spawn(async move {
                // Held and never read, so each client waits for a response
                // that never comes.
                let mut held = Vec::new();
                while let Ok((stream, _)) = listener.accept().await {
                    if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= refused {
                        held.push(stream);
                    }
                }
            });
            Ok(Self {
                url,
                connections,
                server,
            })
        }

        /// The connections taken so far, once any still queued are taken.
        async fn connections(&self) -> usize {
            tokio::time::sleep(Duration::from_millis(200)).await;
            self.connections.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Drop for HungRemote {
        /// Closes the held connections, so the transport children that
        /// outlived their killed `git` exit too.
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    /// Limits short enough for a hung fetch to time out within a test, and
    /// distinct, so a timeout's message names the budget it ran under.
    fn short_limits(op: GitOp) -> Duration {
        match op {
            GitOp::Local => GitOp::Local.limit(),
            GitOp::Fetch => Duration::from_millis(700),
            GitOp::FirstFetch => Duration::from_millis(400),
        }
    }

    /// An agent over a fresh repository whose fetches time out quickly.
    async fn agent_with_short_limits(
        dir: &std::path::Path,
    ) -> Result<(FetchAgent, mpsc::Receiver<Event>)> {
        init_repo_with_commit(dir).await?;
        let (tx, rx) = mpsc::channel(16);
        let (mut agent, _) = FetchAgent::new(dir.to_path_buf(), tx, None, ActivityRegistry::new());
        agent.git_limit = short_limits;
        Ok((agent, rx))
    }

    /// A fetch of the commits that timed out is not followed by a fetch of
    /// every head: each fetch attempt opens one connection, and the remote
    /// sees only the first.
    #[tokio::test]
    async fn timeout_skips_the_full_fetch() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let (agent, mut rx) = agent_with_short_limits(temp_dir.path()).await?;
        let remote = HungRemote::start().await?;

        let events = run_batch(&agent, &mut rx, &remote.url, &[MISSING_HEAD]).await;

        assert_eq!(failed_ids(&events), [MISSING_HEAD]);
        let reasons = failure_reasons(&events);
        assert!(reasons[0].contains("timed out"), "got: {:?}", reasons);
        assert_eq!(remote.connections().await, 1);
        Ok(())
    }

    /// A new remote whose first fetch timed out is kept, so the next request
    /// for it runs under the routine budget instead of the first-fetch one
    /// again; otherwise a remote that never answers would hold the queue for
    /// the longest budget on every request.
    #[tokio::test]
    async fn timed_out_first_fetch_keeps_the_remote() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let (agent, mut rx) = agent_with_short_limits(temp_dir.path()).await?;
        let remote = HungRemote::start().await?;
        let name = agent.get_remote_name(&remote.url);
        let after = |op| format!("timed out after {:?}", short_limits(op));

        let first = run_batch(&agent, &mut rx, &remote.url, &[MISSING_HEAD]).await;
        let reasons = failure_reasons(&first);
        assert_eq!(reasons.len(), 1, "got: {:?}", reasons);
        assert!(
            reasons[0].ends_with(&after(GitOp::FirstFetch)),
            "{:?}",
            reasons
        );
        assert_eq!(
            remote_url(temp_dir.path(), &name).await?.as_deref(),
            Some(remote.url.as_str())
        );

        let second = run_batch(&agent, &mut rx, &remote.url, &[MISSING_HEAD]).await;
        let reasons = failure_reasons(&second);
        assert_eq!(reasons.len(), 1, "got: {:?}", reasons);
        assert!(reasons[0].ends_with(&after(GitOp::Fetch)), "{:?}", reasons);
        Ok(())
    }

    /// The low-speed options are global options, so they must come before
    /// the `fetch` subcommand, and the requested arguments after it.
    #[test]
    fn fetch_carries_the_low_speed_limit() {
        let args = fetch_args(&["origin", "abc"]);
        let fetch = args.iter().position(|a| *a == "fetch");
        let Some(fetch) = fetch else {
            panic!("no fetch subcommand in {:?}", args);
        };
        for option in ["http.lowSpeedLimit=1000", "http.lowSpeedTime=60"] {
            let Some(at) = args.iter().position(|a| *a == option) else {
                panic!("{} missing from {:?}", option, args);
            };
            assert!(at > 0 && args[at - 1] == "-c", "{:?}", args);
            assert!(at < fetch, "{:?}", args);
        }
        assert!(args.starts_with(crate::git_ops::GIT_PROTOCOL_RESTRICTIONS));
        assert_eq!(args[fetch + 1..], ["origin", "abc"]);
    }

    #[test]
    fn git_op_limits_are_the_documented_values() {
        assert_eq!(GitOp::Local.limit(), Duration::from_secs(30));
        assert_eq!(GitOp::Fetch.limit(), Duration::from_secs(300));
        assert_eq!(GitOp::FirstFetch.limit(), Duration::from_secs(1800));
        assert_eq!(RemoteSetup::Added.fetch_op(), GitOp::FirstFetch);
        assert_eq!(RemoteSetup::Existing.fetch_op(), GitOp::Fetch);
    }

    /// Runs git with `args` in `dir` and returns its trimmed output, as an
    /// error when git fails, so a broken fixture stops the test at once.
    async fn git_in(dir: &std::path::Path, args: &[&str]) -> Result<String> {
        let output = crate::git_cmd::in_dir_async(dir)
            .args(args)
            .output()
            .await?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    /// Clones `daemon` into `forge` and adds one commit on top that only
    /// `refname` names, as a forge holds the head of a pull request from a
    /// fork on the base repository: no branch reaches it, so fetching every
    /// head never brings it. Returns that commit.
    async fn forge_with_head_under(
        daemon: &std::path::Path,
        forge: &std::path::Path,
        refname: &str,
    ) -> Result<String> {
        let (Some(daemon_path), Some(forge_path)) = (daemon.to_str(), forge.to_str()) else {
            return Err(anyhow!("test paths must be UTF-8"));
        };
        git_in(daemon, &["clone", "--quiet", daemon_path, forge_path]).await?;
        let head = commit_file(forge, "change.txt", "Pull request change").await?;
        git_in(forge, &["update-ref", refname, &head]).await?;
        git_in(forge, &["reset", "--quiet", "--hard", "HEAD~1"]).await?;
        Ok(head)
    }

    /// Commits a file named `name` holding `message` on the branch checked
    /// out in `dir`, and returns the commit.
    async fn commit_file(dir: &std::path::Path, name: &str, message: &str) -> Result<String> {
        std::fs::write(dir.join(name), format!("{}\n", message))?;
        git_in(dir, &["add", name]).await?;
        git_in(
            dir,
            &[
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--quiet",
                "-m",
                message,
            ],
        )
        .await?;
        git_in(dir, &["rev-parse", "HEAD"]).await
    }

    /// An agent on a new repository at `daemon`, with the forge `forge`
    /// holding a pull request head under refs/pull/7/head that no branch
    /// reaches. Returns the agent, its events, the repository's one commit
    /// and the head.
    async fn agent_with_pull_head(
        daemon: &std::path::Path,
        forge: &std::path::Path,
    ) -> Result<(FetchAgent, mpsc::Receiver<Event>, String, String)> {
        let base = init_repo_with_commit(daemon).await?;
        let head = forge_with_head_under(daemon, forge, "refs/pull/7/head").await?;
        let (tx, rx) = mpsc::channel(16);
        let (agent, _) = FetchAgent::new(daemon.to_path_buf(), tx, None, ActivityRegistry::new());
        Ok((agent, rx, base, head))
    }

    /// The wants of every `git fetch` the agent ran, in order, with the
    /// first one's sorted: a batch's queue is a set, so the order of its
    /// SHAs varies from run to run.
    fn recorded_fetches(agent: &FetchAgent) -> Vec<Vec<String>> {
        let mut fetches = agent
            .fetches
            .lock()
            .map(|fetches| fetches.clone())
            .unwrap_or_default();
        if let Some(first) = fetches.first_mut() {
            first.sort();
        }
        fetches
    }

    /// `wants` sorted, to compare with a recorded SHA attempt.
    fn sorted(wants: &[&str]) -> Vec<String> {
        let mut wants: Vec<String> = wants.iter().map(|w| w.to_string()).collect();
        wants.sort();
        wants
    }

    /// The main case of the forge-ref fallback, on real git. One request in
    /// the batch is an older head the forge no longer has, which fails the
    /// multi-want fetch by SHA as a whole; the pull request head is held
    /// only under refs/pull/7/head, so the heads fallback alone would not
    /// bring it, and the ref fetch does. It leaves no ref of its own.
    #[tokio::test]
    async fn a_pull_request_head_is_fetched_through_its_ref_when_the_batch_fails() -> Result<()> {
        let daemon = tempfile::tempdir()?;
        let forge_dir = tempfile::tempdir()?;
        let forge = forge_dir.path().join("forge");
        let (mut agent, mut rx, base, head) = agent_with_pull_head(daemon.path(), &forge).await?;
        let range = format!("{}..{}", base, head);
        agent
            .mr_metadata
            .insert(range.clone(), (None, None, Some(7)));

        let url = forge.to_string_lossy().to_string();
        let events = run_batch(&agent, &mut rx, &url, &[&range, MISSING_HEAD]).await;

        assert_eq!(submitted_ids(&events), [format!("mr-7-{}", range)]);
        assert_eq!(failed_ids(&events), [MISSING_HEAD]);
        assert_eq!(
            recorded_fetches(&agent),
            [
                sorted(&[&head, MISSING_HEAD]),
                vec!["refs/pull/7/head".to_string()],
                vec![],
            ]
        );
        let naming_head = git_in(daemon.path(), &["for-each-ref", "--points-at", &head]).await?;
        assert_eq!(naming_head, "", "a ref names the fetched head");
        let left = git_in(
            daemon.path(),
            &["for-each-ref", "refs/sashiko", "refs/pull"],
        )
        .await?;
        assert_eq!(left, "", "the ref fetch left refs behind");
        Ok(())
    }

    /// The ref fetch runs only after the fetch by SHA failed, before the
    /// heads fallback, and GitLab's ref after GitHub's: a remote that
    /// refuses every connection makes each attempt fail, so all of them run.
    #[tokio::test]
    async fn a_ref_fetch_follows_a_failed_sha_attempt() -> Result<()> {
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
        agent
            .mr_metadata
            .insert(range.clone(), (None, None, Some(7)));

        let events = run_refused_batch(&agent, &mut rx, &[&range]).await;

        assert_eq!(failed_ids(&events), [format!("mr-7-{}", range)]);
        assert_eq!(
            recorded_fetches(&agent),
            [
                sorted(&[MISSING_BASE, MISSING_HEAD]),
                vec!["refs/pull/7/head".to_string()],
                vec!["refs/merge-requests/7/head".to_string()],
                vec![],
            ]
        );
        Ok(())
    }

    /// A forge that publishes the head only under GitLab's ref is reached
    /// once GitHub's ref has failed.
    #[tokio::test]
    async fn a_merge_request_head_is_tried_after_the_pull_ref() -> Result<()> {
        let daemon = tempfile::tempdir()?;
        let forge_dir = tempfile::tempdir()?;
        let forge = forge_dir.path().join("forge");
        let base = init_repo_with_commit(daemon.path()).await?;
        let head =
            forge_with_head_under(daemon.path(), &forge, "refs/merge-requests/7/head").await?;
        let (tx, mut rx) = mpsc::channel(16);
        let (mut agent, _) = FetchAgent::new(
            daemon.path().to_path_buf(),
            tx,
            None,
            ActivityRegistry::new(),
        );
        let range = format!("{}..{}", base, head);
        agent
            .mr_metadata
            .insert(range.clone(), (None, None, Some(7)));

        let url = forge.to_string_lossy().to_string();
        let events = run_batch(&agent, &mut rx, &url, &[&range, MISSING_HEAD]).await;

        assert_eq!(submitted_ids(&events), [format!("mr-7-{}", range)]);
        assert_eq!(
            recorded_fetches(&agent)[1..],
            [
                vec!["refs/pull/7/head".to_string()],
                vec!["refs/merge-requests/7/head".to_string()],
                vec![],
            ]
        );
        Ok(())
    }

    /// A ref fetch that times out ends the fallbacks, as a timed-out fetch
    /// by SHA does: neither GitLab's ref nor every head is fetched after it.
    #[tokio::test]
    async fn a_ref_fetch_timeout_stops_the_fallbacks() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let (mut agent, mut rx) = agent_with_short_limits(temp_dir.path()).await?;
        // The fetch by SHA fails at once on the refused connection; the ref
        // fetch after it hangs.
        let remote = HungRemote::start_after_refusing(1).await?;
        let range = format!("{}..{}", MISSING_BASE, MISSING_HEAD);
        agent
            .mr_metadata
            .insert(range.clone(), (None, None, Some(7)));

        let events = run_batch(&agent, &mut rx, &remote.url, &[&range]).await;

        let reasons = failure_reasons(&events);
        assert_eq!(reasons.len(), 1, "got: {:?}", reasons);
        assert!(reasons[0].contains("timed out"), "got: {:?}", reasons);
        assert_eq!(
            recorded_fetches(&agent),
            [
                sorted(&[MISSING_BASE, MISSING_HEAD]),
                vec!["refs/pull/7/head".to_string()],
            ]
        );
        assert_eq!(remote.connections().await, 2);
        Ok(())
    }

    /// A request that names no pull request has no forge ref to try, so the
    /// heads fallback follows the fetch by SHA directly, as before.
    #[tokio::test]
    async fn a_request_without_a_number_skips_the_ref_fetch() -> Result<()> {
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
        assert_eq!(
            recorded_fetches(&agent),
            [sorted(&[MISSING_BASE, MISSING_HEAD]), vec![]]
        );
        Ok(())
    }

    /// A pull request head brought by its ref is still submitted when the
    /// heads fallback after it fails; only the request still missing a
    /// commit fails. A ref at refs/remotes/<remote> makes the heads fetch
    /// fail, since git cannot create the remote's branches beneath it,
    /// while a fetch with no destination is unaffected.
    #[tokio::test]
    async fn a_head_fetched_through_its_ref_survives_a_failed_heads_fetch() -> Result<()> {
        let daemon = tempfile::tempdir()?;
        let forge_dir = tempfile::tempdir()?;
        let forge = forge_dir.path().join("forge");
        let (mut agent, mut rx, base, head) = agent_with_pull_head(daemon.path(), &forge).await?;
        let range = format!("{}..{}", base, head);
        agent
            .mr_metadata
            .insert(range.clone(), (None, None, Some(7)));
        let url = forge.to_string_lossy().to_string();
        let blocker = format!("refs/remotes/{}", agent.get_remote_name(&url));
        git_in(daemon.path(), &["update-ref", &blocker, &base]).await?;

        let events = run_batch(&agent, &mut rx, &url, &[&range, MISSING_HEAD]).await;

        assert_eq!(submitted_ids(&events), [format!("mr-7-{}", range)]);
        assert_eq!(failed_ids(&events), [MISSING_HEAD]);
        let reasons = failure_reasons(&events);
        assert!(
            reasons[0].starts_with("Failed to fetch from"),
            "the reason should be the heads fetch's error, got: {:?}",
            reasons
        );
        Ok(())
    }

    /// A base that moved on the target branch is not reached by the pull
    /// request head, so the served refs/pull/7/head leaves it missing.
    /// GitLab's ref is not tried after GitHub's was served, and the heads
    /// fetch brings the base, so the pull request is submitted.
    #[tokio::test]
    async fn a_base_the_pull_ref_does_not_reach_is_left_to_the_heads_fetch() -> Result<()> {
        let daemon = tempfile::tempdir()?;
        let forge_dir = tempfile::tempdir()?;
        let forge = forge_dir.path().join("forge");
        let (mut agent, mut rx, _, head) = agent_with_pull_head(daemon.path(), &forge).await?;
        let moved_base = commit_file(&forge, "target.txt", "Target branch change").await?;
        let range = format!("{}..{}", moved_base, head);
        agent
            .mr_metadata
            .insert(range.clone(), (None, None, Some(7)));

        let url = forge.to_string_lossy().to_string();
        let events = run_batch(&agent, &mut rx, &url, &[&range, MISSING_HEAD]).await;

        assert_eq!(submitted_ids(&events), [format!("mr-7-{}", range)]);
        assert_eq!(failed_ids(&events), [MISSING_HEAD]);
        assert_eq!(
            recorded_fetches(&agent),
            [
                sorted(&[&moved_base, &head, MISSING_HEAD]),
                vec!["refs/pull/7/head".to_string()],
                vec![],
            ]
        );
        Ok(())
    }

    /// An older request for pull request 7, whose head was force-pushed
    /// away, shares the newer request's ref fetch: refs/pull/7/head is
    /// fetched once and GitLab's ref never, whichever request the batch
    /// lists first. The newer head is submitted and the older one fails.
    /// The batch is run directly, in each order, because the queue is a set
    /// whose order varies from run to run.
    #[tokio::test]
    async fn an_older_head_of_a_pull_request_shares_its_ref_fetch() -> Result<()> {
        for newer_first in [true, false] {
            let daemon = tempfile::tempdir()?;
            let forge_dir = tempfile::tempdir()?;
            let forge = forge_dir.path().join("forge");
            let (mut agent, mut rx, base, head) =
                agent_with_pull_head(daemon.path(), &forge).await?;
            let newer = format!("{}..{}", base, head);
            let older = format!("{}..{}", base, MISSING_HEAD);
            for range in [&newer, &older] {
                agent
                    .mr_metadata
                    .insert(range.clone(), (None, None, Some(7)));
            }
            let mut batch = vec![newer.clone(), older.clone()];
            if !newer_first {
                batch.reverse();
            }

            let url = forge.to_string_lossy().to_string();
            agent.process_batch(Some(&url), batch).await;
            let events = drain(&mut rx);

            assert_eq!(submitted_ids(&events), [format!("mr-7-{}", newer)]);
            assert_eq!(failed_ids(&events), [format!("mr-7-{}", older)]);
            assert_eq!(
                recorded_fetches(&agent),
                [
                    sorted(&[&head, MISSING_HEAD]),
                    vec!["refs/pull/7/head".to_string()],
                    vec![],
                ],
                "newer first: {}",
                newer_first
            );
        }
        Ok(())
    }

    /// Fetches pull request 7's forge refs for `ranges` from `url` as a
    /// batch whose fetch by SHA failed would, and returns the outcome.
    async fn forge_head_outcome(
        agent: &FetchAgent,
        url: &str,
        ranges: &[&str],
    ) -> Result<ForgeHeadFetch> {
        let remote_name = agent.get_remote_name(url);
        let setup = agent.ensure_remote(&remote_name, url).await?;
        let requests: Vec<BatchRequest> = ranges
            .iter()
            .map(|range| BatchRequest::new(range.to_string()))
            .collect();
        let head = ForgeHead {
            number: 7,
            requests: requests.iter().collect(),
        };
        let source = FetchSource {
            remote_name: &remote_name,
            remote: url,
            op: setup.fetch_op(),
        };
        let activity = ActivityRegistry::new();
        let marks = BatchMarks {
            activity: &activity,
            commits: Vec::new(),
        };
        agent.fetch_forge_head(&source, &head, &marks).await
    }

    /// A forge that served refs/pull/7/head has answered, so a request it
    /// left short of a commit, an older head force-pushed away, is not
    /// reported as a refusal with the credentials advice. A forge that
    /// refuses both refs is.
    #[tokio::test]
    async fn a_served_ref_that_leaves_a_commit_missing_is_not_a_refusal() -> Result<()> {
        let daemon = tempfile::tempdir()?;
        let forge_dir = tempfile::tempdir()?;
        let forge = forge_dir.path().join("forge");
        let (agent, _rx, base, _) = agent_with_pull_head(daemon.path(), &forge).await?;
        let older = format!("{}..{}", base, MISSING_HEAD);

        let url = forge.to_string_lossy().to_string();
        assert_eq!(
            forge_head_outcome(&agent, &url, &[&older]).await?,
            ForgeHeadFetch::Served(ForgeHeadRef::Pull)
        );
        assert_eq!(
            forge_head_outcome(&agent, REFUSING_REMOTE, &[&older]).await?,
            ForgeHeadFetch::Refused
        );
        assert_eq!(
            forge_head_outcome(&agent, &url, &[&format!("{}..{}", base, base)]).await?,
            ForgeHeadFetch::Complete
        );
        assert_eq!(
            recorded_fetches(&agent),
            [
                vec!["refs/pull/7/head".to_string()],
                vec!["refs/pull/7/head".to_string()],
                vec!["refs/merge-requests/7/head".to_string()],
            ]
        );
        Ok(())
    }
}
