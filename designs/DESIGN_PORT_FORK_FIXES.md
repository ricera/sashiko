# Design: Port Step 4 -- Fixes Bundled into the Fork's Activity Commit

Status: approved 2026-10-05. Every recommended option in section 8 is
accepted (D1-D15).
Branch: `port/upstream` (upstream `origin/main` 2a7e748b1680 plus the ported
fork features). The fork source is `feat/prior-revision-findings`.

## 1. Context

Fork commit 50ae5dbb6f61 ("activity: make a running review say what it is
doing") was ported as activity reporting. The same commit also carried several
unrelated fixes, listed in its message under "Bounded operations" and "Also
fixed along the way". Those fixes were left out of the activity port. This
document decides, for each one, whether to port it and how.

The five items, each tied to an incident the fork saw:

| Item | Incident | Verdict |
| --- | --- | --- |
| Empty-commit skip | The empty tracker commit made by `b4 prep` was submitted as patch 1 of a range. Its review failed and was retried three times. | Adapt |
| Stranded-fetch sweep | A daemon restart during a fetch left the patchset in `Fetching` for good. | Adapt |
| Fetcher git timeouts | One `git fetch` against an unreachable remote hung and wedged the fetch agent for every patchset. | Adapt |
| Claude request deadline | The direct Claude client had no timeout, so a stalled connection parked a stage indefinitely. Transport errors were Fatal. | Port |
| Unknown-tool suggestions | The model called `git_gray` for `git_grep` and was told only "Tool not found". | Port |

No item is dropped. Every item fixes a defect that still exists on
`port/upstream`. Upstream did not fix any of them another way.

### Goals

1. A patch whose stored diff is empty is marked `Skipped` without a worker,
   and its patchset can still end `Reviewed`.
2. A patchset that a restart left in `Fetching` ends `Failed` at startup.
   Its reason says it was interrupted and, for a git fetch, what the fetch
   was doing.
3. A fetch that fails, including one that times out, ends the request it was
   for: its placeholder patchset ends `Failed` with the error as its reason.
4. Every git command that `src/fetcher.rs` spawns itself (`ensure_remote`,
   the fetch attempts, `is_present`, `resolve_sha`) has a time limit, and the
   `git` process is killed when that limit passes. The `git_ops` helpers the
   fetcher calls (`resolve_git_range`, `extract_patch_metadata`,
   `drop_commit_graph`) stay unbounded (D8). Killing `git` does not kill its
   transport helper; §3.3 says what does end that.
5. A request to the Claude Messages API has a deadline. A transport failure,
   including that deadline, is retried as a transient error.
6. An unknown tool name gets an error that names the closest registered tool
   and lists all of them.

### Non-goals

These are separate port items, even though some come from the same fork
commit:
- b4 cover-letter adoption: `--b4-cover-letter`, `is_cover_letter` on
  `PatchSubmitted`, part 0 with `total_parts = count - 1`, and the `[b4]` half
  of the fork's `scripts/verify_empty_commit.sh`.
- Fetch cancellation (`fetch_with_cancel`, `CancelRegistry`). It depends on the
  `kill_on_drop` added in §3.3, so §3.3 should land first.
- Making a plain cancel of a `Fetching` patchset force-only.
- The `worker_cancel_token` and stdin `cancel` hunks of `src/ai/mod.rs`.
- A request deadline for the Vertex client (`src/ai/vertex.rs:191`, also
  `Client::new()`). See D11. Commit 3 does change how Vertex transport errors
  are classified (§3.4).
- A git-fetched patchset that a restart caught after the fetch, while its
  patches were being extracted. Storing the first part moves it from
  `Fetching` to `Incomplete` (`src/db.rs:6312`, `6877`), and it reaches
  `Pending` only when every part has arrived. The rest of the
  `PatchSubmitted` events die with the process, so it stays `Incomplete`.
  The sweep cannot tell it from a mailing-list series still waiting for
  parts, and `create_fetching_patchset` deliberately does not reset an
  `Incomplete` row (`src/db.rs:9461-9466`). The way out is manual: a plain
  cancel accepts `Incomplete` (`src/db.rs:9036`), and `Cancelled` is
  resubmittable.
- Rejecting a tool call in a stage that advertised no tools (D13).
- Bounding ssh prompts and stalled ssh transports in the fetcher (§3.3).

## 2. Baseline

What `port/upstream` already has that these items interact with:

- **Activity reporting** (`src/activity.rs`). `ActivityRegistry::with_persistence`
  spawns a writer task. Before its first op, the writer deletes every
  commit-keyed fetch row (`Database::delete_fetch_activity`, `src/db.rs:8361`).
  `get_patchset_activity` bridges a patchset to its commit-keyed row through
  `get_patchset_fetch_commit` (`src/db.rs:8431`). The server serves persisted
  rows only for `UNFINISHED_PATCHSET_STATUSES` (`src/server.rs:984`).
- **Typed outcomes.** `PatchOutcome::Skipped` and `update_patch_status(id,
  PatchOutcome)` (`src/lib.rs`, `src/db.rs:8974`). `PatchsetTally`
  (`src/reviewer.rs:90-125`) treats a skipped patch's `PatchResult::Success`
  as neutral, so a series where every patch is reviewed or skipped ends
  `Reviewed`.
- **Partial and TimedOut** (DESIGN_PORT_TIMEOUTS.md). The review deadline, the
  wind-down and salvage window, and `ReviewStatus::Partial`. Transient AI
  errors are retried by `BackoffProvider` (`src/ai/backoff_provider.rs:310-333`)
  and the session (`src/ai/session.rs:688-718`). A worker that is killed or
  wound down aborts its in-flight IPC request tasks.
- **git_cmd safety layer** (`src/git_cmd.rs`). Every git command is built
  with `in_dir`, `in_dir_async` or `detached_async`, which strip inherited
  `GIT_DIR`-family variables. A test fails the build if a raw
  `Command::new("git")` appears elsewhere in `src/`. This layer sets no
  timeout and no `kill_on_drop`.
- **Bounded git elsewhere, not in the fetcher.** Each of these is local to its
  own module, and none reaches `src/fetcher.rs`:
  - `git_ops::fetch_remote` (`src/git_ops.rs:1020-1052`), used by the git sync worker.
  - The reviewer's base fetch (`FETCH_TIMEOUT`, `src/reviewer.rs:139`).
  - `baseline.rs:30-53` and `prerequisites.rs:78-121`.

## 3. Design

### 3.1 Patches that change no files

**Fork behaviour.** In `process_patch_review`, right after
`let files = extract_files_from_diff(diff);`:

```rust
if files.is_empty() {
    info!("Skipping review for patch {}/{} (ID: {}) as it changes no files.", ..);
    let _ = ctx.db.update_patch_status(patch_id, "Skipped").await;
    return Ok(PatchResult::Success);
}
```

It writes no review row. The unit test
`empty_commits_are_recognised_as_having_no_files` checks `""`, `"\n\n"` and a
lone `Signed-off-by:` line. The script `scripts/verify_empty_commit.sh` runs a
daemon against a stub model on a personal kernel branch.

**Upstream today.**
- **Ingestion.** The fetcher turns an empty commit into a `PatchSubmitted`
  with an empty diff, and `main.rs:641-715` stores it as an ordinary patch.
  The diff comes from `git show` in `git_ops::extract_patch_metadata`, and is
  empty for an empty commit.
- **Baseline.** Resolution takes the SHA fast path, which only checks that the
  commit exists, so the empty commit passes.
- **Pre-filter.** The pre-filter loop in `review_patchset`
  (`src/reviewer.rs:973-1034`) has no empty check.
- **Review.** In `process_patch_review`, the ignored-files rule at line 1802
  starts with `!files.is_empty() &&`, so an empty diff falls through. A worker
  is spawned with `--review-commit <sha>`, and every failed attempt is
  retried up to `review.max_retries`.
- **Dead helper.** `GitWorktree::is_empty_commit` (`src/git_ops.rs:274`) is
  called only by tests.

**Verdict: adapt.** Range-submitting a `b4 prep` branch is a normal workflow.
Today its tracker commit costs at least one full AI review of nothing. The
fork's version cannot be copied as is, for three reasons:

1. **Its predicate is wrong.** `extract_files_from_diff` matches only lines
   that start with `diff --git a/`, and a real patch can have none:
   - Under the default `core.quotePath`, git C-quotes non-ASCII paths:
     `diff --git "a/\303\251.c" "b/\303\251.c"`.
   - `git show` on a merge that resolved a conflict prints `diff --cc`.
     `resolve_git_range` uses a plain `rev-list` without `--no-merges`, so
     merges do reach review.
   - The mailing-list path accepts a plain unified diff with no git header
     (`src/patch.rs:222-227` keeps a body that has `--- `, `+++ ` and
     `@@ -`), for example a quilt-style patch.

   The fork would silently skip each of these.
2. **It writes a string status** where the port uses typed status.
3. **Its script is not portable.** It hardcodes a personal kernel branch, and
   half of it tests cover-letter adoption, which is not ported.

**Design.**

*Predicate.* The test is on the stored diff itself, not on any header or
path inside it (D2). Each patch source stores a diff as follows:
- **Git range or forge PR/MR.** The fetcher stores what
  `git_ops::extract_patch_metadata` finds after its header marker in
  `git show` output (`src/git_ops.rs:1650-1673`). That is `""` for an empty
  commit and `"\n"` for a clean merge, which introduces nothing. Both were
  checked against git. Any commit that changes a file, a mode or a binary
  prints a non-empty diff, whatever its paths look like, and so does a
  merge that resolved a conflict (`diff --cc`).
- **Mailing list.** `parse_email` stores the whole body as the diff, and only
  when it found a diff in it (`src/patch.rs:222-227`, `:273`). A stored
  mbox patch therefore never has an empty diff, so the check never skips
  one, git header or not.

The predicate is a private function in `src/reviewer.rs`, next to its only
caller:

```rust
/// Whether a stored diff is empty, so the patch changes nothing.
///
/// A `b4 prep` tracker, or any `git commit --allow-empty`, is stored with
/// an empty diff, and a clean merge with a lone newline. Every other
/// source stores a non-empty one, so this never skips a patch that has a
/// header `extract_files_from_diff` cannot read.
fn diff_is_empty(diff: &str) -> bool {
    diff.trim().is_empty()
}
```

*Skip* (`src/reviewer.rs`, `process_patch_review`, immediately before
`let files = extract_files_from_diff(diff);`):

```rust
if diff_is_empty(diff) {
    info!("Skipping review for patch {}/{} (ID: {}) as it changes no files.",
          patchset_id, index, patch_id);
    if let Err(e) = ctx.db.update_patch_status(patch_id, PatchOutcome::Skipped).await {
        warn!("Failed to mark patch {} skipped: {}", patch_id, e);
    }
    return Ok(PatchResult::Success);
}
```

- A failed status write gets a `warn!`, as in the pre-filter at
  `src/reviewer.rs:1022-1031`, instead of the fork's `let _ =`.
- The ignored-files guard `!files.is_empty() &&` stays. A diff with only
  quoted-path, combined or plain unified headers is non-empty but yields no
  extracted files, and it must still be reviewed. A comment there will say
  so.
- No review row is written, which matches the existing pre-filter skips (D1).
  `count_successful_reviews` counts only `Reviewed`, so the patch is checked
  again on a later run, which costs one string scan.
- *Placement.* Both callers of `process_patch_review` (`src/reviewer.rs:1093`
  and `:1155`) come from `valid_jobs`, so the check could also go in the
  pre-filter loop. It goes in `process_patch_review` as in the fork, because
  the existing ignored-files test harness (`test_skip_ignored_files`,
  `src/reviewer.rs:6322`) calls that function directly.

*Interplay.*
- **Patchset status.** `PatchsetTally` treats `PatchResult::Success` as
  neutral, so a series of one tracker commit and one reviewed patch ends
  `Reviewed`. A series made only of empty commits also ends `Reviewed`, which
  is the existing behaviour when every patch is skipped.
- **Partial.** A skipped patch is neutral, so it can never make a series
  `Partial`.
- **Activity.** No worker is spawned, so no review activity is written.
- **Dead helper.** `GitWorktree::is_empty_commit` stays as it is. It answers
  a different question (a commit object in a worktree), and removing it is
  unrelated cleanup.

*Docs.* Add a section "Patches that change no files" to
`designs/subject-filtering.md`, the document that defines patch-level
`Skipped`. It covers the reason, the predicate and why it is safe for each
patch source, the outcome and the scope above.

**Settings:** none.

### 3.2 Stranded fetches at startup

**Fork behaviour.** `Database::fail_stranded_fetching_patchsets` runs
`UPDATE patchsets SET status = 'Failed', failed_reason = 'Fetch interrupted
by daemon restart' WHERE status = 'Fetching'` and returns the row count.
`main.rs` calls it right after `db.migrate()`. It logs a `warn!` when the
count is non-zero and an `error!` if the call fails. Its doc comment assumes
one daemon per database.

The fork has two tests:
- `test_startup_sweep_fails_only_stranded_fetches`.
- `test_sweep_preserves_the_last_activity_for_context`, which expects the
  commit-keyed activity row to survive the sweep so the CLI can print
  "stopped while: fetching ...".

**Upstream today.**
- **Startup recovery** is only `reset_reviewing_status`
  (`src/db.rs:9560`). It moves `In Review` back to `Pending` and never
  touches `Fetching`.
- **Leaving `Fetching`.** Only the ingest paths take a patchset out of
  `Fetching` (`src/db.rs:6312`, `6317`, `6877`, `6882`, `7064`, and
  `update_patchset_error` at `9511`). All of them need a live fetch.
- **Manual ways out.** Resubmitting the same commit resets the row
  (`src/db.rs:9466`), and a plain cancel accepts `Fetching`
  (`src/db.rs:9032-9036`). Neither happens on its own.
- **Merge candidate.** An orphan stays a merge candidate for 30 days
  (`src/db.rs:9182`).
- **Activity.** The persistence writer deletes every commit-keyed fetch row
  at startup. A stranded patchset therefore reads as unfinished, with
  `live=false` and no entries, indefinitely. The comment on
  `UNFINISHED_PATCHSET_STATUSES` (`src/server.rs:971-983`) expects a ported
  sweep to keep its row. That premise does not hold on this branch.

**Verdict: adapt.** The bug is present. The sweep is one statement. The
fork's second test cannot hold here: `delete_fetch_activity` removes the row
at startup, by design. That design closed the "stranded row stays forever"
problem, and reopening it to serve a row for a `Failed` patchset would be the
wrong trade. So the sweep copies where the fetch stopped into
`failed_reason` before the writer deletes the row (D3).

**Design.**

*Database* (`src/db.rs`, next to `reset_reviewing_status`):

```rust
pub const FETCH_INTERRUPTED_REASON: &str = "Fetch interrupted by daemon restart";

/// Fails every patchset a restart left mid-fetch.
///
/// Every fetch runs as an in-process task, the FetchAgent for a git
/// fetch or the API's spawned task for a thread fetch, so it cannot
/// survive a restart, and no time-based transition ever moves a
/// `Fetching` patchset on. Assumes one daemon per database, as
/// `reset_reviewing_status` already does. Must run before
/// `ActivityRegistry::with_persistence`, whose writer deletes the fetch
/// rows this reads.
pub async fn fail_stranded_fetching_patchsets(&self) -> Result<u64>
```

Steps:
1. `SELECT id FROM patchsets WHERE status = 'Fetching'`.
2. For each id, `stranded_fetch_reason(id)`, a private helper:
   - `get_patchset_fetch_commit(id)` builds the
     `ActivityKey::Commit(sha).to_string()` key. This strips the forge
     `mr-{n}-` prefix as it already does.
   - Read the `description` of that key from `patchset_activity`.
   - Return `FETCH_INTERRUPTED_REASON`, plus ` while {description}` when a
     row exists, for example "Fetch interrupted by daemon restart while
     fetching 3 commit(s) from https://example.com/linux.git".
   - A thread-fetch placeholder (`src/server.rs:603-623`) is keyed by the
     bare message id, with no `@sashiko.local` suffix, so
     `get_patchset_fetch_commit` returns `None` and the reason is
     `FETCH_INTERRUPTED_REASON` alone. Thread fetches write no activity, so
     nothing is lost.
3. `UPDATE patchsets SET status = ?, failed_reason = ? WHERE id = ? AND
   status = 'Fetching'`, with `ReviewStatus::Failed.as_str()` as the status.
   Sum the affected rows.

`Fetching` is not a `ReviewStatus` variant. Every query in `db.rs` spells it
as a literal (`src/db.rs:6312-9496`). The new query uses one private
`const FETCHING: &str = "Fetching"`. Adding a `Fetching` variant would touch
every exhaustive match and is out of scope.

*Startup* (`src/main.rs`, between `db.ensure_project_stamp(project)` and
`ActivityRegistry::with_persistence`):

```rust
// Before the activity writer starts: it deletes the fetch rows the sweep
// reads. Also before the API and fetcher exist, so no fetch of this run
// can be mistaken for a stranded one.
match db.fail_stranded_fetching_patchsets().await {
    Ok(0) => {}
    Ok(n) => warn!("Failed {} patchset(s) stranded mid-fetch by a restart", n),
    Err(e) => error!("Failed to sweep stranded fetches: {}", e),
}
```

Running the sweep in `Reviewer::run`, next to `reset_reviewing_status`, would
be too late. It would race the writer's delete and new submissions.

*Comment updates.*
- `src/activity.rs:597-605` and the doc comment on `delete_fetch_activity`:
  the startup sweep has already copied a stranded fetch's description into
  `failed_reason`, which is why deleting the row is safe.
- `src/server.rs:971-983`: a patchset the sweep failed is finished and has no
  persisted row. Its `failed_reason` says where the fetch stopped, so status
  alone is the right test. Remove the paragraph about the unported sweep.
- `src/db.rs:8381-8384` (`get_patchset_activity`): the commit bridge serves a
  fetch that is still running or was just stopped in this run. After a
  restart, the row is gone and the reason carries it.

*Interplay.*
- **Resubmit.** `Failed` is resubmittable (`RESUBMITTABLE_STATUSES_SQL`), so
  resubmitting the same commit or range puts the row back to `Fetching` with
  `failed_reason = NULL`.
- **Merge candidates.** The swept patchset leaves the merge-candidate query
  (`src/db.rs:9182`), which is the intended effect (D5).
- **Fetcher timeouts.** §3.3 makes a restart a much rarer way out of a hung
  fetch. After §3.3, a failed or timed-out fetch ends its request's
  placeholder `Failed` through `IngestionFailed` and `update_patchset_error`.
  The sweep handles a restart (a crash, a deploy, or an operator restart)
  while the placeholder is still `Fetching`. A restart during patch
  extraction leaves the patchset `Incomplete`, which the sweep does not
  touch (Non-goals).
- **Timeouts design.** `Partial` and `TimedOut` are untouched. The sweep
  never looks at review statuses.

**Settings:** none.

### 3.3 Bounded git commands in the fetcher

**Fork behaviour.** In `src/fetcher.rs`:
- Two constants: `NETWORK_OP_TIMEOUT = 300s` and `LOCAL_OP_TIMEOUT = 30s`.
- A helper, `run_git(cmd, limit, what)`. It sets `kill_on_drop(true)`,
  wraps `cmd.output()` in `tokio::time::timeout`, and words both errors:
  "Failed to execute {what}: {e}" and "{what} timed out after {n}s".
- Every fetcher spawn goes through it:
  - `ensure_remote` uses the local limit, with a single `get-url` call where
    there used to be two.
  - Each attempt of `fetch_with_graph_retry` uses the network limit.
  - `is_present` and `resolve_sha` use the local limit.
- The commands are built with raw `Command::new("git")`.

**Upstream today.**
- Every fetcher git call is a bare `in_dir_async(..).output()` or
  `.status()`, with no timeout and no `kill_on_drop`:
  - `ensure_remote` (`src/fetcher.rs:441-482`);
  - `fetch_with_graph_retry` (`:500`);
  - `is_present` (`:582`) and `resolve_sha` (`:609`).
- `FetchAgent::run` awaits `process_queue` inline (`:117-122`). One stuck
  fetch therefore blocks every later fetch, and their patchsets stay in
  `Fetching`.
- **A failed fetch misses its placeholder.** `process_queue` splits each
  requested range into its endpoint SHAs (`:153-170`). When
  `ensure_remote` fails, or `fetch_commits` and then `fetch_all` fail, it
  sends one `IngestionFailed { article_id: <sha> }` per missing SHA
  (`:214-229`, `:244-256`) and `continue`s past the whole batch.
  `resolve_root_msg_id` turns that into `<sha>@sashiko.local`
  (`src/main.rs:3289-3290`), and `get_msgid_candidates`
  (`src/db.rs:1558-1583`) never maps it to the placeholder the API created:
  - a range is `{base}..{head}@sashiko.local` (`src/server.rs:514`, `:544`);
  - a forge PR/MR is `mr-{n}-{commit_or_range}@sashiko.local`
    (`src/server.rs:2061`), single commit or range.

  So `update_patchset_error` matches nothing and the placeholder stays
  `Fetching` until a restart. Worse, it can match an unrelated single-commit
  patchset submitted earlier for that same SHA, and its `UPDATE` has no
  status guard (`src/db.rs:9511-9526`), so a finished review can be
  overwritten as `Failed`. A request in the same batch whose commits were
  all present is dropped by the `continue` and also stays `Fetching`. The
  `resolve_git_range`, `resolve_sha` and extraction failures (`:280-289`,
  `:344-353`, `:395-404`) send the bare request without the forge `mr-{n}-`
  prefix, so a forge request misses its placeholder there too.
- **The fallback.** When the optimistic `fetch_commits` fails for any
  reason, `fetch_all` fetches every head of the remote, with tags
  auto-followed (`:234-244`). That can only move more data.
- **Budgets elsewhere.** `git_ops::refresh_remote` gives a remote it just
  added, or one with no head, 1800 s, and 300 s otherwise
  (`src/git_ops.rs:1273-1279`). The fetcher adds a remote per URL on first
  use (`ensure_remote`), so its first fetch from a remote is the same kind
  of fetch.

**Verdict: adapt.** The defect is present. The fork's raw `Command::new("git")`
breaks the one-constructor rule and the test that enforces it. Its two loose
constants are replaced by a typed budget. The fork bounded the fetch but kept
the failure path above, so a timed-out range or forge fetch would still stay
`Fetching`. The port fixes that path first, as its own commit.

**Design, part 1: fail the request, not the SHA** (`src/fetcher.rs`).

- `fn request_article_id(&self, commit_or_range: &str) -> String` returns
  `mr-{n}-{commit_or_range}` when `mr_metadata` has a number for the
  request, and the request itself otherwise. The success paths
  (`:301-305`, `:363-367`) already build exactly this, and now call it.
- `process_queue` keeps, for each requested `commit_or_range`, the endpoints
  it split out. `missing_commits` is still the union that is fetched.
- When `ensure_remote` or the fetch fails, the batch sends one
  `IngestionFailed` per request that has a missing endpoint, with
  `article_id: self.request_article_id(request)` and the existing error
  wording. Requests whose endpoints were all present go on to step 3 with
  the rest removed, instead of being dropped by the `continue`.
- The `resolve_git_range`, `resolve_sha` and extraction failures also use
  `request_article_id`.
- `update_patchset_error` and `resolve_root_msg_id` are unchanged. Every
  `IngestionFailed` from the fetcher now names a placeholder the API
  created, so the SHA-level match against an unrelated patchset can no
  longer happen from this path.

**Design, part 2: bounded commands** (`src/fetcher.rs`).

```rust
/// How long a fetcher git command may run before it is killed.
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
            Self::Local => Duration::from_secs(30),
            Self::Fetch => Duration::from_secs(300),
            Self::FirstFetch => Duration::from_secs(1800),
        }
    }
}

/// What `ensure_remote` found, which sets the budget of the first fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteSetup {
    Added,
    Existing,
}

/// A fetcher git command that ran past its limit and was killed.
#[derive(Debug, thiserror::Error)]
#[error("{what} timed out after {}s", .limit.as_secs())]
struct GitTimedOut {
    what: String,
    limit: Duration,
}

/// Runs a fetcher git command under `op`'s limit.
async fn run_git(cmd: tokio::process::Command, op: GitOp, what: &str) -> Result<Output> {
    run_git_within(cmd, op.limit(), what).await
}

/// The mechanism behind `run_git`, with the limit injectable for tests.
async fn run_git_within(mut cmd: Command, limit: Duration, what: &str) -> Result<Output> {
    cmd.kill_on_drop(true).env("GIT_TERMINAL_PROMPT", "0"); // D6
    match tokio::time::timeout(limit, cmd.output()).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(anyhow!("Failed to execute {what}: {e}")),
        Err(_) => Err(GitTimedOut { what: what.to_string(), limit }.into()),
    }
}
```

*What the limit does and does not end.*
- `kill_on_drop` sends SIGKILL to the `git` process when the timeout drops
  the future. Without it, `git fetch` keeps running, and keeps its
  repository locks, until it ends on its own.
- It does not kill git's transport child (`git-remote-https`, or `ssh`).
  That child is orphaned. It exits when it next talks to the dead `git`,
  or when its own network wait ends. `git_ops::fetch_remote`
  (`src/git_ops.rs:1024-1030`) has the same limit. For https the low-speed
  limit below ends that wait too (D14). For ssh nothing in this design does.
- `GIT_TERMINAL_PROMPT=0` stops git's own credential prompt, which is the
  https case. Credential helpers still run, so a private remote with
  configured credentials still works. ssh prompts for host keys and
  passwords on `/dev/tty` itself and ignores this variable, so an ssh
  remote that needs a prompt still waits out the limit (D6, Non-goals).
- `stdin` is not set: `Command::output` already gives the child a closed
  stdin.

Commands are still built with `crate::git_cmd::in_dir_async`, so the
environment scrubbing and `GIT_PROTOCOL_RESTRICTIONS` stay.

*Call sites.*
- `ensure_remote` returns `Result<RemoteSetup>`:
  - One `run_git(get-url, Local)` in place of the `.status()` probe and the
    second `get-url`. Use its exit status and stdout.
  - Then `set-url` (`Existing`) or `add` (`Added`) under `Local`.
  - `set-url`'s exit status is now checked. Today it is ignored.
- `fetch_commits` and `fetch_all` take the `GitOp` to fetch under:
  `FirstFetch` after `RemoteSetup::Added`, `Fetch` otherwise (D7).
- `fetch_with_graph_retry` runs each attempt as
  `run_git(cmd, op, "git fetch")`, with
  `-c http.lowSpeedLimit=1000 -c http.lowSpeedTime=60` added to the fetch
  (D14).
  - A timeout returns `Err` through `?`, so it never reaches the graph
    retry.
- In `process_queue`:
  - When `fetch_commits` fails with a `GitTimedOut`
    (`e.downcast_ref::<GitTimedOut>()`), the batch does not fall back to
    `fetch_all`. A remote that did not answer a small fetch will not answer
    a larger one. Any other failure keeps the fallback.
  - When the fetch of an `Added` remote fails other than by a timeout, the
    remote is removed again (`remote remove`, `Local`; a failure is logged
    and ignored). The next request for that URL is then a first fetch
    again, with the first-fetch budget, instead of a 300 s retry of a
    transfer that needed more.
  - When it timed out, the remote is kept. It had the whole first-fetch
    budget and a fetch does not resume, so a second 1800 s attempt would
    end the same way. Removing it would give a remote that never answers
    1800 s on every request, not once; kept, its next request runs under
    `Fetch`.
  - Either failure then ends the batch's requests through part 1.
  - `BatchMarks`' `Drop` clears the activity row.
- `is_present`: `Local`. An `Err` is already treated as "missing".
- `resolve_sha`: `Local`.
- `git_ops::drop_commit_graph`, `resolve_git_range` and
  `extract_patch_metadata` are unchanged (D8).

*Worst case per batch.* The fetch phase of one batch runs at most four
attempts: `fetch_commits` and the `fetch_all` fallback, each with one
commit-graph retry. Each attempt has its op's limit.
- A hung remote stops at the first attempt's timeout and skips the
  fallback: 300 s for a known remote, 1800 s for a new one. For https, the
  low-speed limit usually ends a stalled transfer well before that.
- The theoretical maximum, every attempt failing slowly without a timeout,
  is four limits: 1200 s, or 7200 s for a new remote. It needs a
  stale commit-graph and a server that refuses fetch-by-SHA at once.

*Interplay.*
- **Activity.** The "fetching N commit(s) from <remote>" phase now ends
  within its limit instead of never.
- **Stranded fetches.** §3.2 covers what a timeout cannot: a restart.
- **Fetch cancellation.** The later cancellation item relies on this
  `kill_on_drop`.
- **Settings.** The limits are constants, like the reviewer's
  `FETCH_TIMEOUT` and the git sync worker's budgets. No setting is added.

*Docs.* This section is the design. `designs/DESIGN_KERNEL_TREES.md` and the
git sync docs describe other paths and are not changed.

**Settings:** none.

### 3.4 Claude request deadline and transport retries

**Fork behaviour.**
- `ClaudeError::Transport(String, Duration)` is added and classifies as
  `Transient`.
- `request_timeout(api_timeout_secs, max_tokens) = max(api_timeout_secs,
  max_tokens / 10 + 120)` seconds:
  - 10 tok/s is a pessimistic output rate, and thinking tokens count as
    output.
  - 120 s is the allowance for the first token.
  - The configured value can raise the deadline but never lower it below
    this floor.
- `ClaudeClient::new` takes `api_timeout_secs` and builds the client with
  `Client::builder().timeout(..)`, calling `.expect(..)` on the result.
- In `post_request`, a send error other than a builder or redirect error
  becomes `Transport`.
- `impl ClassifyAiError for reqwest::Error` and a `reqwest::Error` downcast
  arm in `classify_ai_error`. These catch a deadline that fires while the
  body is being read.
- `create_provider_from_ai` passes `ai.api_timeout_secs`.

**Upstream today.**
- **No deadline.** `ClaudeClient::new` uses `Client::new()`
  (`src/ai/claude.rs:222`), which has no timeout.
- **Send errors are Fatal.** A send error is an untyped `bail!`
  (`:290-294`), which classifies as Fatal.
- **Body-read errors are Fatal.** `res.text().await?` (`:300`) lets a raw
  `reqwest::Error` escape, and `classify_ai_error` has no arm for it
  (`src/ai/mod.rs:438-467`).
- **`ai.api_timeout_secs` is ignored.** The "claude" arm of
  `create_provider_from_ai` (`src/ai/mod.rs:640-657`) does not pass it,
  although `docs/configuration.md:103` says it applies to every API call.
- **Other providers.**
  - `api_timeout_secs` is passed only to `openai`/`openai-compatible`,
    `ollama`, `vllm`, `kiro-cli` and `goose` (`src/ai/mod.rs:714`, `742`,
    `764`, `807`, `822`).
  - It is ignored by `gemini` (hard-coded 600 s, `src/ai/gemini.rs:450`),
    `bedrock`, `vertex` (no deadline, `src/ai/vertex.rs:191`),
    `claude-cli`, `devin-cli`, `codex-cli` and `copilot-cli`.
  - The openai, ollama, vllm and gemini clients map a send error to a
    typed `TransientError` with a 30 s `retry_after`
    (`src/ai/openai.rs:328-337`, `src/ai/gemini.rs:634-644`). Vertex wraps
    the raw `reqwest::Error` in `.context(..)` (`src/ai/vertex.rs:221-227`,
    `:297-303`), which classifies as Fatal.
- **What bounds a stall today.** In the daemon, the review deadline,
  `review.timeout_seconds` (3600 by default, 7200 in the shipped
  Settings.toml), ends it, losing the stage and holding an LLM slot the
  whole time. A local review can hang forever.

**Verdict: port.** The code transfers almost verbatim. The only adaptation is
that `ClaudeClient::new` returns `Result<Self>` instead of panicking.
`create_provider_from_ai` already returns `Result`, and AGENTS.md rules out
`.expect()` that cannot be proven safe. The fork's reason for refusing
`unwrap_or_else(Client::new)` still holds: that fallback would quietly give
back a client with no deadline.

**Design.**

*`src/ai/claude.rs`:*
- `ClaudeError::Transport(String, Duration)`, with
  `#[error("Transport error, retry after {1:?}: {0}")]`. `ai_error_class`
  maps it to `AiErrorClass::Transient { retry_after }`.
- `SLOWEST_PLAUSIBLE_OUTPUT_TOKENS_PER_SEC = 10`,
  `FIRST_TOKEN_ALLOWANCE_SECS = 120`, and
  `fn request_timeout(api_timeout_secs: u64, max_tokens: u32) -> Duration`,
  verbatim from the fork with its doc comments.
- `ClaudeClient::new(model, enable_caching, max_tokens, base_url, thinking,
  effort, api_timeout_secs: u64) -> Result<Self>`:
  - It builds `Client::builder().timeout(request_timeout(..)).build()` and
    passes a failure up with `.context(..)`.
  - It logs at info when the floor raised the configured value.
  - The deadline is not part of `cache_identity`, because it does not change
    response content.
- `post_request`:
  - A send error that `is_builder()` or `is_redirect()` keeps the existing
    `bail!` (Fatal).
  - Every other send error returns
    `ClaudeError::Transport(redact_secret(..), Duration::ZERO)`.
  - The success-path body read maps explicitly:
    `res.text().await.map_err(|e| ClaudeError::Transport(redact_secret(&e.to_string()), Duration::ZERO))?`
    (D10).
  - `retry_after = 0` is a floor: `BackoffProvider` applies exponential
    backoff with jitter and takes the larger value (D9).

*`src/ai/mod.rs`:*
- `impl ClassifyAiError for reqwest::Error`: builder or redirect errors are
  Fatal, everything else is `Transient { retry_after: 0 }`.
- A `reqwest::Error` downcast arm in `classify_ai_error`, before the final
  `Fatal`. anyhow's `downcast_ref` reaches through `.context(..)`, so the arm
  turns three kinds of error from Fatal into Transient:
  - the raw `?` body reads in `gemini.rs:329`, `:364` and `vertex.rs:232`;
  - Vertex send errors, which `vertex.rs:227` and `:303` wrap with
    `.context("Failed to send request to Vertex AI")`. This makes Vertex
    send failures retryable, as Gemini's already are. It is intended: a
    reset connection is no more fatal on Vertex than elsewhere. Vertex
    still has no deadline (D11), so this does not bound a Vertex stall;
  - any other raw `reqwest::Error` that reaches the classifier, under the
    same builder-or-redirect rule.
- The "claude" arm passes `ai.api_timeout_secs` and uses `?` on the new
  `Result`.

*Interplay with the timeouts design.*
- **Retries.** A stalled request now ends at its deadline, for example
  529 s at `max_tokens = 4096`, and is retried within the stage. It no
  longer holds the stage until the review deadline.
- **Wind-down.** During the salvage window, a consolidation request that
  stalls is retried instead of eating the window, which makes salvage more
  likely to finish.
- **Very large `max_tokens`.** The deadline can exceed the review deadline
  (128k tokens gives 12,920 s). The review deadline then stays in charge, as
  it is today (D12).
- **Local reviews.** `DeadlineBudget` still bounds only retries. Each
  in-flight request is now bounded too.
- **Errors that do not clear.** A wrong `base_url`, a refused connection or
  a TLS failure is now Transient too. In the daemon, `BackoffProvider` has a
  `DeadlineBudget` and no attempt cap (`src/ai/backoff_provider.rs:238`,
  `src/reviewer.rs:3602-3608`), so such a review retries with capped
  backoff until `review.timeout_seconds` and ends `TimedOut`, where today it
  fails within seconds. A local review with a retry budget behaves the
  same; one without stops after `MAX_ATTEMPTS` (6,
  `src/local_review.rs:589-597`). This is the behaviour the openai, ollama, vllm and gemini
  clients already have (D15).

*Docs.*
- `designs/DESIGN_CLAUDE_PROVIDER.md`:
  - Settings: the deadline formula and the reasons behind it.
  - Error handling: transport failure means `Transport`, which is Transient;
    builder and redirect errors are Fatal.
  - Testing: the new tests.
- `docs/configuration.md:103`: replace "Timeout for individual API calls"
  with a description that names the providers that use it:
  `claude`, `openai`/`openai-compatible`, `ollama`, `vllm`, `kiro-cli` and
  `goose`. For `claude` it is a minimum, raised to `max_tokens / 10 + 120`
  seconds when that is longer. The other providers ignore it.

**Settings:** no new setting and no alias. `ai.api_timeout_secs` (default
300) now applies to the `claude` provider, as documented, with the floor
above.

### 3.5 Suggestions for an unknown tool name

**Fork behaviour.** `ToolRegistry::call` fails with
`Tool not found: "<name>". Did you mean "<best>"? Available tools: a, b, c`.
- The list of tools is sorted.
- The suggestion appears only if a tool is within an edit distance of
  `(chars / 3).clamp(1, 3)`.
- Ties go to the alphabetically first name.
- `edit_distance` is a two-row Levenshtein over chars.
- Five tests use a stub registry.

**Upstream today.**
- `src/toolbox/framework.rs:112` returns
  `Tool not found in registry: {name}`.
- The callers turn that into a tool result the model reads
  (`src/workflow/stage.rs:313-316`, `:351-354`;
  `src/workflows/linux_bug.rs:2991-2994`). Each retry costs a turn against
  `max_interactions`.
- There is no edit-distance helper in `src/`. `strsim` is only a transitive
  dependency.

**Verdict: port.** The change is private to `framework.rs`, has no
dependencies, and is valid on Rust 1.90. It follows the port's rule that a
rejected call is the model's to correct (`src/workflow/stage.rs:348-350`).

**Design.** Copy `unknown_tool_error`, `closest_tool`, `edit_distance` and
the tests module verbatim from
`feat/prior-revision-findings:src/toolbox/framework.rs`. `call` uses
`anyhow!("{}", self.unknown_tool_error(name))`, and its doc comment says the
error names the closest match and lists the registered tools.

*Interplay.*
- **Name normalization.** `ToolBox::call` trims and lowercases the name
  first, so the suggestion is computed on the normalized name.
- **Cache.** Unknown names are never cached (`src/toolbox/mod.rs:625-628`),
  so the longer error is not cached either.
- **Advertised tools.** The list comes from the whole registry. A
  `ToolScope::All` stage advertises that same set. A `ToolScope::None` stage
  advertises nothing (`src/workflow/stage.rs:265-278`), but a tool call it
  emits is still dispatched to the whole registry
  (`src/ai/session.rs:766-782`), so its error would list tools the stage was
  not given (D13). No workflow uses `ToolScope::Selected` today.

**Settings:** none.

## 4. Type-Driven State

| Concept | Type | Replaces |
| --- | --- | --- |
| Patch skipped for an empty diff | `PatchOutcome::Skipped` with `PatchResult::Success` | fork's `"Skipped"` string |
| Patchset failed by the sweep | `ReviewStatus::Failed` and `FETCH_INTERRUPTED_REASON` | fork's literal SQL |
| Fetcher git budget | `GitOp { Local, Fetch, FirstFetch }` | two loose `Duration` constants |
| Whether a fetch is a remote's first | `RemoteSetup { Added, Existing }` from `ensure_remote` | `Result<()>` |
| A fetcher command killed at its limit | `GitTimedOut`, found with `downcast_ref` | a formatted string |
| Claude transport failure | `ClaudeError::Transport(String, Duration)` | untyped `bail!` (Fatal) |
| reqwest error class | `impl ClassifyAiError for reqwest::Error` | fall-through to Fatal |

## 5. Data, Schema, Wire, API, UI and Settings Changes

- **Schema:** none. The sweep writes existing columns.
- **Wire and API:** none.
  - `/api/patchset/activity` returns `finished = true` and no entries for a
    swept patchset, as it does for any `Failed` one.
  - The swept patchset's `failed_reason` is visible wherever failure reasons
    already are.
  - A range or forge request whose fetch fails now ends `Failed` with the
    fetch error, where today it stays `Fetching`.
- **UI:** none.
- **Settings:** no setting is added, renamed or aliased. One behaviour change:
  `ai.api_timeout_secs` now applies to the `claude` provider, as a minimum
  (§3.4).
- **Docs:**
  - `designs/subject-filtering.md` (§3.1);
  - `designs/DESIGN_CLAUDE_PROVIDER.md` and `docs/configuration.md` (§3.4).
    The `api_timeout_secs` row names the providers that use it.

## 6. Testing Plan

All tests are unit tests in the module they cover, plus one script. Run
`make check-pr` from a throwaway clone, never under `git rebase --exec`
(AGENTS.md, Testing).

### 6.1 Empty-commit skip (`src/reviewer.rs` tests unless noted)
- `diff_is_empty`:
  - true for `""`, `"\n"` and `"\n\n"`;
  - false for a `diff --git a/x b/x` patch;
  - false for a C-quoted `diff --git "a/\303\251.c" "b/\303\251.c"` patch,
    which the fork's predicate skips;
  - false for a `diff --cc file` merge resolution;
  - false for a plain unified diff with no git header,
    `"--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n"`.
- The premise for git-sourced patches: in a throwaway repo built with
  `git_cmd::in_dir_async`, `git_ops::extract_patch_metadata` gives a diff
  for which `diff_is_empty` is true for an `--allow-empty` commit and for a
  clean merge, and false for a commit that changes a file.
- The premise for mbox (`src/patch.rs` tests): `parse_email` on a
  quilt-style patch (`--- a/mm/x.c~name` / `+++ a/mm/x.c` / `@@ -`) with no
  `diff --git` line returns a patch whose diff is non-empty.
- A sibling of `test_skip_ignored_files`. It stores a patch with diff `""`
  and calls `process_patch_review`. It asserts:
  - the result is `PatchResult::Success`;
  - `patches.status` is `Skipped`;
  - the `reviews` table has no row for the patch, so no worker was spawned.
- The non-empty cases are covered at the predicate level only. Driving one
  through `process_patch_review` would spawn a worker, which the
  ignored-files harness does not stub.
- `scripts/verify_empty_commit.sh`, modelled on `scripts/verify_live_log.sh`
  (same `safe_kill` and timeout rules, stub model, patched Settings.toml and
  query helper):
  - It builds a throwaway repo under its `mktemp` dir: a base commit,
    `git commit --allow-empty -m tracker`, and one real commit. It checks
    with `git diff-tree --name-only -r` that the tracker changes nothing.
  - It submits the range and asserts four things:
    - there are 2 patch rows;
    - patch 1 is `Skipped`;
    - patch 1 has 0 `reviews` rows;
    - the patchset is `Reviewed`.
  - Before the review commit lands, the script shows what happens today
    (open question Q1). It is run once on each side of the change and both
    results are recorded in the commit message.
  - No `--b4-cover-letter` case and no personal paths.

### 6.2 Stranded-fetch sweep (`src/db.rs` tests)
- `test_startup_sweep_fails_only_stranded_fetches`, ported and extended.
  - Setup: one `Fetching` patchset, plus one each of `Pending`,
    `Incomplete`, `In Review` and `Failed`.
  - The sweep returns 1.
  - Only the stranded patchset becomes `Failed`, with a reason containing
    "restart".
  - The other patchsets are untouched.
- `test_sweep_names_where_the_fetch_stopped` (replaces the fork's
  `test_sweep_preserves_the_last_activity_for_context`):
  - Call `upsert_activity("commit:ctx", None, <Phase::Fetching json>, &describe, now)`
    directly, with no registry and no sleep.
  - Assert the reason is "Fetch interrupted by daemon restart while
    fetching 3 commit(s) from https://example.com/linux.git".
- A forge case: placeholder `mr-501-aaa..bbb@sashiko.local`, mr_number 501,
  key `commit:aaa..bbb`. The description is found.
- A git fetch with no activity row: the reason equals
  `FETCH_INTERRUPTED_REASON` exactly.
- A thread-fetch placeholder, a bare message id with no `@sashiko.local`
  suffix: it is swept, and its reason equals `FETCH_INTERRUPTED_REASON`
  exactly.
- Resubmission: after the sweep, `create_fetching_patchset` on the same msgid
  gives `Fetching` with a NULL `failed_reason`.
- Running the sweep twice: the second run returns 0.

### 6.3 Fetcher (`src/fetcher.rs` tests unless noted)

Failure reporting (commit 5). Each uses an agent whose remote is
`http://127.0.0.1:1/nonexistent.git`, which refuses at once, and reads the
events from the agent's channel.
- `failed_range_fetch_names_the_range`: a request for a missing range
  `<base>..<head>` produces one `IngestionFailed` whose `article_id` is
  `<base>..<head>`, and none for either endpoint SHA.
- `failed_forge_fetch_names_the_placeholder`: the same with `mr_number = 7`
  gives `mr-7-<base>..<head>`. A missing single commit with `mr_number = 7`
  gives `mr-7-<sha>`.
- `present_request_survives_a_failed_fetch`: one batch for the same URL
  holds a commit that is present and a range that is missing. The range
  gets `IngestionFailed`, and the present commit still gets
  `PatchSubmitted`.
- `src/db.rs`: `update_patchset_error` on `<base>..<head>@sashiko.local`
  and on `mr-7-<base>..<head>@sashiko.local` fails the matching
  placeholder, and leaves a `Reviewed` patchset `<base>@sashiko.local`
  untouched.

Bounds (commit 6).
- `run_git_gives_up_on_a_hung_command`: `tokio::process::Command::new("sleep")`
  with `60`, run through `run_git_within` with a 200 ms limit. It returns,
  within 5 s, an error that `downcast_ref::<GitTimedOut>()` finds and whose
  text contains "timed out". The ban covers only `Command::new("git")`.
- `run_git_returns_output_on_success`: `git_cmd::detached_async()` with
  `--version`.
- `fetch_all_reports_unreachable_remote_as_error`:
  - Init a repo with `git_cmd::in_dir_async`.
  - `ensure_remote("broken", "http://127.0.0.1:1/nonexistent.git")`.
  - `fetch_all` returns `Err` that is not a `GitTimedOut`, so the fallback
    rule would still allow `fetch_all` after it.
  - Construct the agent with the port's 4-argument
    `FetchAgent::new(repo, tx, None, ActivityRegistry::new())`.
- `ensure_remote_adds_then_updates_url`: the first call returns `Added`, a
  call with a new URL returns `Existing` and changes the URL, which covers
  the restructured `get-url` path and the newly checked `set-url` status.
- `failed_first_fetch_removes_the_remote`: after a batch against the
  refusing remote, `git remote get-url <name>` fails, so the next request
  is a first fetch again.
- `timeout_skips_the_full_fetch`: the fallback predicate is false for a
  `GitTimedOut` error and true for any other error.
- The fetch command carries `http.lowSpeedLimit` and `http.lowSpeedTime`.
  The fetch arguments come from one helper, and the test checks its output.
- `GitOp::limit` returns the three documented values.

### 6.4 Claude deadline and transport retries
- `src/ai/claude.rs` tests, verbatim from the fork:
  - `test_transport_error_classifies_as_transient`;
  - `test_request_timeout_is_never_shorter_than_the_output_allowance`, where
    `(300, 4096)` gives 529 s;
  - `test_request_timeout_honours_a_longer_configured_value`, where
    `(7200, 4096)` gives 7200 s;
  - `test_request_timeout_is_always_finite`.
- In the same file, the `client(..)` helper in
  `cache_identity_tracks_the_knobs_outside_the_request` passes `300` and
  unwraps the `Result`.
- `src/ai/mod.rs` tests:
  - `test_classify_ai_error_reqwest_transport_is_transient`: a refused
    connection to `127.0.0.1:9` classifies as Transient (D15).
  - The same `reqwest::Error` wrapped with
    `.context("Failed to send request to Vertex AI")`, the shape
    `vertex.rs` produces, classifies as Transient.
  - A builder error classifies as Fatal. The test builds a request with an
    invalid URL scheme so that reqwest reports `is_builder()`.

### 6.5 Unknown-tool suggestions
- `src/toolbox/framework.rs`: the fork's five tests, verbatim.
- `src/toolbox/tools_test.rs`: `toolbox.call("  GIT_GRAY ", json!({}))` fails
  with `Did you mean "git_grep"?`. This covers normalization through
  `ToolBox`, and a second call shows the error was not cached.

## 7. Commit Breakdown

Each commit is one consistent change. Each has `Signed-off-by`, body lines of
72 characters or fewer, and no backticks. `make check-pr` passes before each
commit.

1. **designs: plan the port of the fork's bundled fixes.** This document.
2. **toolbox: name the available tools when the model calls an unknown one.**
   §3.5, with its tests.
3. **ai: retry a request that failed in transport.**
   - `ClaudeError::Transport` and its use in `post_request`, including the
     explicit body-read mapping.
   - `impl ClassifyAiError for reqwest::Error` and the downcast arm, which
     also makes Vertex send failures retryable.
   - The classification tests, including the Vertex-shaped one.
   - The error-handling section of DESIGN_CLAUDE_PROVIDER.md.
4. **ai/claude: give each request a deadline.**
   - `request_timeout` and `ClaudeClient::new(.., api_timeout_secs) -> Result`.
   - The `create_provider_from_ai` change.
   - The deadline tests.
   - The settings section of DESIGN_CLAUDE_PROVIDER.md and the
     `api_timeout_secs` row of `docs/configuration.md`.

   It follows commit 3, so a deadline is never Fatal.
5. **fetcher: fail the request a failed fetch was for.** §3.3 part 1:
   `request_article_id`, per-request `IngestionFailed`, and the present
   requests that survive a failed fetch. Its tests.
6. **fetcher: bound every git command and kill it when abandoned.** §3.3
   part 2: `GitOp`, `RemoteSetup`, `GitTimedOut`, `run_git`, the low-speed
   limit, no fallback after a timeout, and removing a remote whose first
   fetch failed other than by a timeout. Its tests.
7. **db: fail fetches a daemon restart stranded.**
   - The sweep, its startup call and the comment updates in `activity.rs`,
     `server.rs` and `db.rs`.
   - Its tests.
8. **review: skip a patch whose diff is empty.**
   - `diff_is_empty`, the `process_patch_review` change, the comment on the
     ignored-files guard, and the §6.1 unit tests.
   - The `designs/subject-filtering.md` section.
9. **scripts: check that an empty commit is skipped end to end.**
   `scripts/verify_empty_commit.sh`.

Commits 2, 3-4, 5-7 and 8-9 are independent groups. They are ordered from
the smallest to the largest. Commit 5 precedes 6 so that a fetch the new
limit ends reaches its placeholder. Commit 6 precedes 7 so the sweep's
comments can point at the bounded fetch. The predicate is too small to be a
commit of its own, so it lands with its only caller.

## 8. Open Decisions

Each decision has a recommended option.

**D1. A review row for an empty-commit skip.**
- (a) Write only `patches.status = Skipped`, as the fork and the existing
  pre-filter skips do.
- (b) Also write a `reviews` row with status `Skipped` and a reason ("changes
  no files"), as the ignored-files rule does.

*Recommend (a).* Any skip that happens before a review starts writes no row
today. (b) would be the only such skip with a row, and the patch's subject
("tracker" or a b4 cover subject) already shows why it was skipped. If skip
reasons are wanted, they should come for every pre-review skip in one change.

**D2. The empty-commit predicate.**
- (a) The stored diff is empty after `trim()`.
- (b) Scan for change markers: `diff --git`, `diff --cc`,
  `diff --combined`, unified `--- `/`+++ `/`@@ ` lines, `Binary files` and
  `GIT binary patch`.
- (c) `extract_files_from_diff` returns nothing, as in the fork.

*Recommend (a).* It follows from how each source stores a diff (§3.1):
`extract_patch_metadata` stores nothing for an empty commit and a newline
for a clean merge, and `parse_email` never stores an empty diff. It
therefore skips exactly the patches that change nothing, whatever header
format the others use. (b) must list every header format git or quilt can
print, and a missed one silently skips a real patch. (c) misses quoted
paths, combined diffs and plain unified diffs.

**D3. What the swept patchset's reason says.**
- (a) `FETCH_INTERRUPTED_REASON` plus " while <last fetch description>".
- (b) The fixed reason only, as in the fork, and drop the fork's second test.
- (c) Keep the activity row for swept patchsets and widen the server gate.

*Recommend (a).* One durable field keeps the fork's "where it got to" intent.
It needs no change to the gate and leaves `delete_fetch_activity` doing the
job it was added for. (c) reopens the stranded-row problem.

**D4. Single-daemon assumption.**
- (a) Sweep every `Fetching` row at startup, and document the assumption.
- (b) Sweep only rows created before this process started.

*Recommend (a).* `reset_reviewing_status` already resets every `In Review`
row at startup on the same assumption, and two daemons on one database
already break that. (b) would add a timestamp comparison that guards a setup
the system does not support.

**D5. Merge candidates.** The swept patchset leaves the merge-candidate query
(`src/db.rs:9182`) because it is now `Failed`.
- (a) Accept this. A resubmission brings it back.
- (b) Keep it as a candidate.

*Recommend (a).* A patchset whose parts never arrived cannot be merged into
anything.

**D6. Terminal prompts in the fetcher.**
- (a) Set `GIT_TERMINAL_PROMPT=0` in `run_git_within`.
- (b) Leave it out, as the fork did. The limit still ends a prompt.

*Recommend (a).* `baseline.rs` and `linux_bug.rs` already do this. A daemon
started from a terminal, such as one hand-started in tmux, would otherwise
block on git's credential prompt for a private https remote until the limit,
instead of failing at once. Credential helpers are unaffected. It does not
cover ssh, which prompts on `/dev/tty` itself, and `stdin` needs no setting
because `output()` already closes it.

**D7. Fetch budget.**
- (a) 300 s per attempt for every fetch, as in the fork.
- (b) 1800 s per attempt when `ensure_remote` just added the remote, 300 s
  otherwise, as `git_ops::refresh_remote` does. A remote whose first fetch
  failed other than by a timeout is removed again, so its next request is
  again a first fetch; one whose first fetch timed out is kept, so its next
  request runs under 300 s. A timed-out `fetch_commits` does not fall back
  to `fetch_all`.
- (c) One long budget for every fetch, relying on stall detection (D14) to
  end hung remotes early.

*Recommend (b).* The first fetch from a new remote can carry much more than a
routine one, and the `fetch_all` fallback, which fetches every head with
tags, more still. Under (a) such a fetch is killed at 300 s, then the
fallback is killed at 300 s too, and every later request fails the same
way. (b) gives the first fetch the budget upstream already chose for the
same kind of fetch, keeps a hung known remote at 300 s, and the remote
removal stops a large first fetch from being retried under the short limit.
(c) lets a hung ssh or git:// remote, which D14 does not cover, hold the
queue for 30 minutes on every request. The cost of (b) is that a hung new
remote holds the queue for up to 1800 s once, unless it is https.

**D8. Bounding `resolve_git_range` and `extract_patch_metadata`.**
- (a) Leave them unbounded, as the fork and upstream do.
- (b) Give them `Local` limits too. That moves the helper into `git_ops` or
  adds a timeout to these public functions.

*Recommend (a)* for this step. Both run local commands on a local
repository. No incident involved them, and (b) widens the change beyond the
fetcher. Revisit when fetch cancellation is ported, since it also touches
these paths.

**D9. `retry_after` for a Claude transport error.**
- (a) 0 s, as in the fork.
- (b) 30 s, matching the openai, ollama, vllm and gemini clients.

*Recommend (a).* `retry_after` is a floor under `BackoffProvider`'s
exponential backoff, which supplies the real delay. 30 s would add a fixed
wait on top of a connection reset that clears at once.

**D10. The body-read error.**
- (a) Map it to `ClaudeError::Transport` in `claude.rs` and also add the
  blanket `reqwest::Error` arm in `classify_ai_error`.
- (b) Only the blanket arm, as in the fork.

*Recommend (a).* The Claude path is then typed without relying on a
catch-all. The blanket arm still fixes the same Fatal body reads in Gemini
and Vertex.

**D11. The Vertex client.**
- (a) Give Vertex no deadline in this step. Accept that commit 3's blanket
  arm makes its send and body-read failures retryable.
- (b) Also give Vertex a deadline in this step.
- (c) Narrow the blanket arm so Vertex send errors stay Fatal.

*Recommend (a).* No fork incident involved Vertex, and a deadline needs its
own floor reasoning and tests. Retrying its transport errors is the right
behaviour on its own, and it matches Gemini. (c) would keep a reset
connection fatal on one provider for no reason except scope.

**D12. Large `max_tokens`.**
- (a) No cap on the floor. A deadline longer than `review.timeout_seconds`
  leaves the review deadline in charge.
- (b) Cap the floor at some fixed maximum.

*Recommend (a).* A cap would cut off legitimate long answers, which is the
reason the floor exists. In the daemon, the review deadline already bounds
the worst case.

**D13. The tool list in the unknown-tool error.**
- (a) List the whole registry, as the fork does, including in a
  `ToolScope::None` stage.
- (b) List only the tools advertised to the current stage.
- (c) (a), and also reject every tool call in a stage that advertised none.

*Recommend (a).* In a `ToolScope::All` stage the two lists are the same. In
a `ToolScope::None` stage the model was given no tools, so it rarely emits a
call, and a call with a correct name already runs today. Listing the names
reveals less than that existing dispatch does. (c) fixes the real gap but
changes stage behaviour, so it is a separate item (Non-goals). (b) would need
the stage's declaration list passed into the registry.

**D14. Stall detection below the wall clock.**
- (a) None. The limit and `kill_on_drop` are the only bound.
- (b) Add `-c http.lowSpeedLimit=1000 -c http.lowSpeedTime=60` to the fetch,
  so curl ends an https transfer that moves under 1000 B/s for 60 s.
- (c) (b), and also pass ssh `BatchMode=yes` and `ConnectTimeout=30`.

*Recommend (b).* It ends a stalled https fetch within about a minute, in
`git` and in the orphaned `git-remote-https` alike, which makes D7's long
first-fetch budget cheap for the common case. It changes nothing for a
healthy transfer. (c) needs `-c core.sshCommand=..` or `GIT_SSH_COMMAND`,
and either one replaces an ssh command the operator configured. Leave ssh
for a separate item.

**D15. Transport errors that do not clear.**
- (a) Every Claude send error except a builder or redirect error is
  Transient, as in the fork.
- (b) Keep connect errors (`is_connect()`) Fatal, so a wrong `base_url` or
  proxy fails at once.

*Recommend (a).* A refused or reset connection while an endpoint or proxy
restarts is the case the retry exists for, and (b) would drop the review on
it. The openai, ollama, vllm and gemini clients already accept this cost.
The cost is visible: `BackoffProvider` logs a warning on every retry, and
the review ends `TimedOut` instead of `Failed`. The §6.4 test that a refused
connection is Transient encodes this choice.

### Open questions (to settle with data, not a decision)

- **Q1.** Does the empty commit fail on this branch, or does it get one full
  AI review of nothing? The fork counted "Failed To Apply", but the port
  reviews git-fetched patches with `--review-commit` after the SHA fast path,
  with no `git am`. The script in commit 9, run before commit 8, settles it.
  The skip is worth having either way.
- **Q2.** Could an mbox patch with an empty body reach the `git am` path?
  `parse_email` (`src/patch.rs:273`) declines to make a patch without a
  diff, and stores the whole body as the diff when it makes one. A stored
  mbox patch is therefore never empty, so D2's predicate never skips one.
  The §6.1 quilt test checks that premise. No change is planned.

## Feynman Summary

Five small bugs rode along in one big fork commit and never made it across.
- An empty "bookmark" commit gets a full, pointless AI review. We notice its
  stored diff is empty and mark it skipped. A real patch always has a
  non-empty diff, whatever format it is in, so none is skipped by mistake.
- A restart in the middle of downloading commits leaves a patchset stuck in
  "Fetching" forever. At startup we mark it failed and say what it was doing.
- A hung `git fetch` blocks every later download. Each git command gets a
  stopwatch and is killed when time runs out. A new remote gets a longer
  stopwatch, and a stalled https download is cut off after a minute. A
  failed fetch also has to tell the right patchset: today a failed range
  fetch reports its commit SHAs, which match no patchset, so the range would
  stay "Fetching" even after the timeout. That is fixed first.
- A stalled connection to Claude waits forever and is treated as fatal. It
  gets a deadline sized to the longest plausible answer, and connection
  failures are retried. Vertex connection failures become retryable too.
- When the model asks for a tool that does not exist, we tell it the closest
  real name and the full list, so it fixes the call in one go.

None of them needs a new setting.
