# Design: Port Step 5 -- How Much Looking Each Patch Review Did, by Repository

Status: approved, revision 2, 2026-10-07: the user accepted D1 to D8 as
recommended and answered Q1. Nothing in it is built yet. Revision 2 corrects the facts a review of revision 1 found wrong
(duplicate-call guard, what a reused row's duration and tokens cover, the
rerun and retry data, p90) and changes four decisions as a result: D4 (no
per-reference clause on a review that made no calls), D7 (a reused row
starts its calls afresh), the attempt stamp, and the daemon no longer
storing `reads`.

Branch: `port/upstream` (upstream `origin/main` 2a7e748b1680 plus the ported
fork features). The fork source is `feat/prior-revision-findings`, commit
2fbca333d7fa ("web: report tool calls per patch review, split by
repository").

Related fork commit 8fbb7d3656d9 ("toolbox: record which revision a
reference read was served") is already on the branch as part of step 1
(36295e73fd9c; `git log -S REPO_READ_KEY` finds it there). In
`src/toolbox/mod.rs`: `REVISION_READ_KEY` (line 119), `REPO_READ_KEY`
(126), `served_revision` (290), `add_pin_note` (358). This step does not
change any of it, and none of it reaches `tool_usages`: `record_tool_calls`
stores the model's raw arguments, not the toolbox's annotated reply.

## 1. Context

### 1.1 What the fork commit did

`tool_usages` already held one row per tool call per review. The only thing
that read it was `/api/stats/tools`, which totals the last 1000 reviews by
provider and model. Nothing showed how much looking one patch review did.
The commit changed six files (+369/-7):

1. **A `repo` column on `tool_usages`.** It is nullable `TEXT`, added at
   startup by an unversioned `try_add_column` with errors ignored.
   `reviewer.rs` fills it from the raw model arguments as
   `call.arguments["repo"].as_str().unwrap_or("review")`. That happens
   before the toolbox runs the call, so a call the toolbox later refuses is
   still recorded with the name it asked for.
2. **A backfill, run on every startup.** Rows whose arguments contain
   `"repo":"kernel"` (a `LIKE` match) become `kernel`. Every other NULL
   row becomes `review`.
3. **Counts per review.** `get_tool_call_counts(patchset_id)` returns an
   untyped JSON array of `{review_id, patch_id, total, reference}`, where
   `reference` counts rows with `repo = 'kernel'`. The two patchset
   queries gain correlated subqueries for `tool_calls` and
   `reference_tool_calls`.
4. **Counts on the activity poll.** `/api/patchset/activity` gains
   `tool_calls`. On a database error it serves `{}`, an object where the
   success case serves an array.
5. **Page.** The stage-progress summary gets an empty
   `<span id="stage-tool-calls-N">`. After painting, `applyToolCallCounts`
   sums every review of the patch into it, as " — 47 (3 to the kernel tree)
   tool calls". The finished card gets a "Tool calls:" line under "Tokens
   used:".
6. **Verifier case 11** (four checks) and two Rust tests.

**The code does not do what the message says about zero.** The message says
the reference clause is "hidden entirely when it is zero and no reference
repository is in play". It also says that on an out-of-tree review, zero
"means the kernel tree was configured, available, and never consulted", and
that this is the failure the count exists to show. But `formatToolCalls`
hides the clause whenever the count is 0, and the verifier enforces that
(`{total: 12, reference: 0}` must render " — 12 tool calls"). So the page
can never show the one zero the message calls an alarm.

Three later fork commits touch this code, none of them in substance:
- eaa5f78b889b appends `r.resumed_from` after the two subqueries.
- 670636505f34 moves the slot into `stageTableParts(rows, live, patchId)`,
  so the finished card's recorded table gets none.
- 46d809932fa9 extends the verifier's `NAMES`.

### 1.2 What the port has

- **Several named references, not one tree.** `[[git.references]]` entries
  have a name, kind, path, revision, `applies_to` and description. A
  reference is attached only to the patches its `applies_to` matches
  (212d7582bf4e). The others are dormant for that patch: they are not in
  the `repo` enum the model is offered (`repo_arg_schema_for`,
  `toolbox/mod.rs:332`), and with none attached the `repo` argument is
  removed from every declaration (`get_declarations_generic`, `:556`).
- **The toolbox decides which repository a call reads.** This is
  `ToolBox::call` (`toolbox/mod.rs:593-700`):
  - Only tools whose schema declares `repo` read one. Those are the eight
    `git_*` tools; `read_prompt` does not, and ignores a stray `repo`.
  - An absent `repo`, or `"review"`, is the code under review.
  - An attached name is that reference.
  - A dormant or unknown name, or any name when nothing is attached, is
    refused with an error (`repo_target`, `:187-238`).
  - The name `review` is reserved (`references.rs:202`).
- **Not every call the model makes reaches the toolbox.** The stage
  session (`StageSession::call_tool` and `call_tools`,
  `workflow/stage.rs:300-343`) blocks a call whose `(function_name,
  arguments)` equals the stage's previous call. It answers "Duplicate tool
  call blocked..." and never calls `ToolBox::call`. On a final turn the
  session sends no tools and ignores any calls the model makes anyway
  (`ai/session.rs:676`, `:767`).
- **Per-reference read counts exist, but only at the end.**
  `ToolBox::reference_reads` counts the calls that resolved to each attached
  reference. Cache hits count, and so do calls git then fails (a bad path,
  an unknown commit), because the count is taken right after `repo_target`
  accepts the name. Calls refused by `repo_target`, blocked duplicates and
  ignored final-turn calls do not count. The count is carried in
  `reviews.reference_repos` (migration 014, `ReviewReferences` in
  `references.rs:676-730`):
  - The worker's early hand-off (`reviewer.rs:3859`) stores the record
    without `reads`.
  - The worker's final output replaces it with `reads` filled in
    (`store_final_reference_repos`, `reviewer.rs:4511`). The two records
    are built by the same `PatchReferences::record` and differ only in
    `reads`.
  - A worker that is killed (timeout, cancellation) never writes a final
    output, so its review keeps the record with `reads` missing.
  - The card shows the record as "References: kernel @ v6.18 (1a2b3c4d5e6f),
    34 reads; ..." (`renderReviewReferences`, `static/index.html:4915`).
    It shows attached references only, and the line is drawn outside the
    status-gated details block.
  - There is no total, no count for the review tree, and nothing while the
    review runs.
- **A NULL record means two things.** `PatchReferences::record` returns
  `None` when no reference is configured (`references.rs:616`), so an
  unconfigured deployment stores NULL. Rows from before migration 014,
  fork rows and reviews whose hand-off was lost are NULL too. On an
  unconfigured deployment what was attached is known (nothing); on the
  others it is not.
- **Rows are written only by the daemon.** `record_tool_calls`
  (`reviewer.rs:3469`) runs once per non-truncated model response, after the
  budget check, for every call in the response. It writes
  `arguments = call.arguments.to_string()`: the model's own JSON, compact
  and in the model's key order (serde_json has `preserve_order`). Arguments
  that do not parse are stored as `null`. There is no `repo` column
  (`001_initial.sql:225-236`), and `ToolUsage` (`db.rs:442`) has no `repo`
  field. Local reviews (`sashiko review`, `sashiko-cli local`) write no rows.
  The daemon records a call before the session decides whether to run it,
  so blocked duplicates and final-turn calls are recorded too.
- **Output lengths are filled by matching, not by call id.** On every
  request the daemon walks the whole history and calls
  `update_tool_usage_length` (`db.rs:2538`) for each tool reply, which
  fills the newest row matching `(review_id, tool_name, arguments,
  output_length = 0)`. A blocked duplicate's row is therefore filled with
  the length of the earlier identical call's reply, and does not show as
  `output_length = 0`. A final-turn call gets no reply and stays at 0.
- **Readers of `tool_usages` today:**
  - `get_tool_usage_stats` (`db.rs:5293`, `/api/stats/tools`);
  - `src/bin/benchmark.rs:530`, which counts turns;
  - the rerun path, which deletes the rows of Failed reviews that have no
    interaction (`db.rs:9356`);
  - `update_tool_usage_length`, above.
- **The activity poll is typed.** `PatchsetActivityResponse {live,
  entries, finished}` lives in `src/api.rs:450`. It is served by
  `server.rs:985` and also deserialized by `sashiko-cli`
  (`fetch_patchset_activity`, `sashiko-cli.rs:1340`). Its entries carry
  `patch_id`. The route is public, embargoed patchsets included, and its
  comment (`server.rs:330`) lists what it may carry: progress metadata, no
  review text. The page polls it every 5 s per open patchset tab
  (`index.html:4467`).
- **Attempts.**
  - Each daemon retry is a new review row (`reviewer.rs:1923-1950`).
    `create_review` inserts it with `attempt` NULL and status Pending; the
    loop then sets it In Review and only then stamps
    `set_review_attempt(review_id, retries + 1)`. Older rows (before
    migration 015) and Skipped rows have NULL.
  - A daemon worker runs exactly one attempt (`local_review.rs:839`).
  - On startup `reset_reviewing_status` (`db.rs:9627`) puts In Review rows
    back to Pending. The next run reuses such a row through
    `get_pending_review_id` (`db.rs:2165`, `reviewer.rs:1901` and, for a
    Skipped patch, `:1848`), stamps it attempt 1 again, and today keeps the
    interrupted run's `tool_usages`. Its live log entries are cleared when
    the new worker starts (`reviewer.rs:3645-3668`), and its early
    hand-off overwrites `reference_repos` unconditionally
    (`set_review_reference_repos`, `db.rs:8215`).
  - What a reused row's other figures cover: `duration_seconds` is timed
    from `review_started = Instant::now()` in the current process
    (`reviewer.rs:1899`) and overwritten, not added to, after each attempt
    (`set_review_duration`, `db.rs:8265`). It covers every attempt of the
    current process's retry loop, which is why the card says "total" on a
    retried row, but none of the interrupted run. Only the patchset clock,
    `patchsets.review_duration_seconds`, carries time across a restart
    (the comment at `db.rs:9327`). A row's tokens are its own attempt's,
    from the new run's trace (`record_interaction`, `reviewer.rs:2380`).
  - A rerun deletes Failed rows that have no interaction, with their tool
    usages, and keeps every other review.
- **Migrations end at 016.** `DESIGN_PORT_FINDING_DISPLAY.md` §4.2 and §4.6
  reserve 017 for the prior-revision step.

### 1.3 Data behind the decisions

These are read-only queries of copies of the three deployment databases
(kernel-fork, driver-a and driver-b, which run the fork), under
`~/step4-measure/deploy-dbs/`, opened with `mode=ro`.

- **The fork column never said more than the arguments.** All three
  databases have `tool_usages.repo`. In driver-a and driver-b it was
  appended by `ALTER`; in kernel-fork it is in the `CREATE` body. No row
  is NULL, and the only values are `review` and `kernel`:

  | DB | review | kernel |
  | --- | --- | --- |
  | kernel-fork | 14,073 | 0 |
  | driver-a | 43,747 | 12,384 |
  | driver-b | 10,973 | 3,713 |

  `COALESCE(json_extract(arguments, '$.repo'), 'review')` equals the column
  on all 84,890 rows. Every `arguments` value is valid plain JSON text
  (`json_valid` holds on all of them, none is gzip). The longest is 725
  bytes.
- **The fork's `LIKE` rule is wrong.** Five driver-a rows
  (`git_read_files` in reviews 267 and 300) match `'%"repo":"kernel"%'`
  but have no top-level `repo`. The model put `"repo":"kernel"` inside a
  `files[]` entry, and the call was served from the review tree (137-159
  byte outputs). They were written after the column existed, so the fork's
  backfill never touched them. A backfill that used `LIKE` would count them
  as kernel. Top-level `json_extract` gets them right.
- **Explicit `"repo":"review"`** appears on 949 driver-a and 723
  driver-b rows. Absent and `review` mean the same thing.
- **No call names any other repository.** The fork had one reference, and
  its data has no dormant or unknown names, so it cannot tell us how often
  the port's refusals happen.
- **How much looking a review does.** Over reviews with at least one call
  (49 / 162 / 72 reviews), calls per review are (median / p90 / max, p90
  as `sorted[int(0.9 n)]` on all three):

  | DB | calls per review | kernel share of calls |
  | --- | --- | --- |
  | kernel-fork | 243 / 497 / 715 | 0% |
  | driver-a | 349.5 / 485 / 813 | 22.1% |
  | driver-b | 189 / 261 / 651 | 25.3% |

  Kernel calls per out-of-tree review (median / p90 / max) are 72 / 150 /
  248 for driver-a and 44 / 97 / 298 for driver-b.
- **The zero the fork could not show is real.** The fork's out-of-tree
  prompt block ("=== Out-of-Tree Module === ... pinned at v7.2") is in the
  logs of 131 driver-a and 71 driver-b reviews. In 7 of those 202 the
  kernel tree was there and never read:
  - 4 driver-a reviews, all Reviewed with 0 findings, each patch touching
    only `eth/ionic/ionic_lif.c`, with 98-156 calls each;
  - 3 driver-b reviews, all with findings, with 73-167 calls each.
- **Retries and reruns pull a per-patch sum in opposite directions.**
  - driver-a has 40 patches with 4 reviews each, 160 rows. All four
    attempts failed on every one of them: 85 rows "Session cancelled by
    supervisor" and 75 "Tool error: Review tool ignored cancellation and
    was killed", counted over all 160. 156 of the 160 rows made no call,
    and 36 of the 40 patches made no call on any attempt. For the other 4,
    attempt 1 made calls and attempts 2-4 made none, so their last review
    shows 0.
  - kernel-fork has 11 patches with two reviews. One is a retry: patch
    12 is reviews 11 (attempt 1, Failed) and 12 (attempt 2, Reviewed),
    same `prompts_hash`, 3 hours apart. The other 10 are reruns, each
    review attempt 1: 7 (patches 13, 17, 19-22, 24) days apart with a
    different `prompts_hash`, and 3 (patches 14-16) about 4.9 hours apart
    with the same one. Over the 10 reruns, summing every review of the
    patch roughly doubles the count (sum over last: median 1.93, max 3.95).
- **Calls the toolbox never saw.** Consecutive identical `(tool_name,
  arguments)` rows within one review, an upper bound on blocked duplicates
  because the guard resets per stage: kernel-fork 26, driver-a 102
  (10 naming kernel), driver-b 48 (2 naming kernel), 176 of 84,890 rows.
  Rows with `output_length = 0` (final-turn calls, calls cut off by a kill)
  number 2, 24 and 4: 30 of 84,890. Together they are about 0.24% of rows.
- **Several references, in practice.** Under the documented driver-a
  `applies_to` globs (`docs/configuration.md:254-266`), rdma-core would
  attach to only 3 of 235 driver-a patches. So the multi-reference
  display must be correct, but the kernel reference will dominate.
- **Fork databases have no `reviews.reference_repos`.** The only per-review
  evidence that a kernel reference was attached is the prompt block above,
  plus any kernel call. 187 driver-a reviews have no logs, and 30 of
  those made kernel calls.

### Goals

1. While a patch is reviewed, its stage-progress summary shows how many tool
   calls its current run has made so far, rising with each poll. Once any
   call is made, it also shows, for each reference attached to the patch,
   how many of those calls were aimed at that reference.
2. A finished review card shows the same numbers for the attempt it
   describes. A Failed, timed-out or Partial review shows them too,
   including a review whose worker was killed.
3. An attached reference that was never read, on a review that made calls,
   shows as 0. That is the alarm the fork promised and did not deliver
   (§1.1). A dormant reference is never shown, and a patch with nothing
   attached shows no per-reference clause.
4. A recorded call is classified against a repository by the toolbox's own
   rule (§1.2). A call the toolbox refuses, or a stray `repo` on a tool that
   reads no repository, is never shown as a read of a reference. The count
   is of calls the model made, as the daemon recorded them; it includes the
   few the session declined to run (§1.3, 0.24% of rows), and says so.
5. One source, `tool_usages`, for the live summary, the card and the CLI.
   The live figure and the CLI describe a run; a card describes one attempt
   and says so when the run had more than one. For a run of one attempt,
   the number a reader watched rise is the number the card keeps.
6. Rows written by the fork, which a later step adopts, count correctly
   without a fork-specific rule.

### Non-goals

- Splitting `/api/stats/tools` by repository. It stays as it is.
- Counts for local reviews (`sashiko review`, `sashiko-cli local`). They
  write no `tool_usages`, and their worker JSON keeps
  `reference_repos[].reads`.
- Not recording calls the session declines to run. `record_tool_calls`
  cannot tell a final turn from a stage with no tools, and runs before the
  stage's duplicate guard (§1.2), so it records both kinds. At 206 of
  84,890 rows at most (§1.3) the effect is under one call per review on
  average. Fixing it means telling the daemon which calls the session ran,
  which is its own change. The effect on the zero alarm is bounded below
  (§4.1).
- Resuming a review from its completed stages (fork eaa5f78b889b). When
  that is ported, it decides whether a resumed review's run includes its
  source review (§4.6).
- Adopting a fork database. That is its own step. This document only says
  what it must do with `tool_usages.repo` and how fork reviews get a
  per-reference split (D8).

## 2. Verdict per Part of the Fork Commit

| Part | Verdict | Reason |
| --- | --- | --- |
| `tool_usages.repo` column, `try_add_column`, `ToolUsage.repo` | Drop | The repository is already on the row, in `arguments`, verbatim. On 84,890 deployment rows the column never said anything else (§1.3). A second copy can only disagree with the first. No migration (D1). |
| Startup backfill (`LIKE '%"repo":"kernel"%'`) | Drop | Nothing to backfill without a column. The rule is also wrong on 5 real rows (§1.3). |
| Classifying a call by its raw `repo` argument, defaulting to `review` | Adapt | Classify with the toolbox's own rule: only tools that take `repo`; attached names count against their reference; dormant and unknown names are calls to unattached repositories, not reads (§4.1, D2). |
| `get_tool_call_counts` as untyped JSON `{total, reference}`, `tu.repo = 'kernel'` | Adapt | Typed `ToolCallCounts` with one count per attached reference, by name, in configured order, and an explicit "no record" state (§4.2, §4.3). |
| Correlated subqueries in two review queries | Adapt | One grouped query per payload, attached to every review in all three review queries (summary, details, single review), as the 014/015 fields already are (§4.2). |
| `tool_calls` on `/api/patchset/activity`, `{}` on error | Adapt | Typed `Vec<PatchToolCalls>` on `PatchsetActivityResponse`, `#[serde(default)]`, for the patches the entries name. On error it logs and serves an empty list, so the shape never changes (§4.3). |
| Live sum over every review of the patch | Adapt | Sum over the current run only: the newest attempt-1 row and the retries after it (§4.5, D5). |
| Slot `<span id>` plus `applyToolCallCounts` after painting | Adapt | The counts go straight into `stageTableParts` as an argument. That needs no id and no second pass, and the recorded table passes none, as in fork 670636505f34 (§4.4). |
| "Tool calls:" line on the finished card | Keep | Placed under "Tokens used:" as in the fork. It counts the card's own attempt, says "on this attempt" when the run had more than one, and carries the per-reference split, which the References line stops showing (D3). |
| `formatToolCalls` hiding a zero reference count | Replace | Every attached reference is shown, 0 included, once the review has made a call. That is what the commit message asked for (D4). |
| Wording "(N to the kernel tree)" | Adapt | "(72 to kernel, 0 to rdma-core)", using the reference names (§4.4). |
| Verifier case 11 | Adapt | Rewritten for named references, the zero rule, attempts and the typed payload. It takes the free case number 11 (§6.3). |
| `El.innerHTML` id registration in the verifier | Already on the branch | The harness already keeps `textContent` in step and registers `id="..."` (`verify_stage_progress_ui.mjs:108-117`). |
| Two Rust tests | Adapt | The backfill test goes (no backfill). The per-review counting test is kept and widened (§6.1). |

## 3. Baseline Checks Made for This Design

- `record_tool_calls` stores `call.arguments.to_string()`. `ToolCall`
  stores unparseable or blank arguments as JSON `null`
  (`ai/mod.rs:92-97`), which serializes as `null`, so every port row is
  valid JSON.
- `ToolBox::call` normalizes the tool name with `trim().to_lowercase()`
  before it decides `takes_repo`. It increments `reference_reads` only for
  `RepoTarget::Reference`, after `repo_target` has accepted the name and
  before the cache lookup and the git call. No tool's `normalize_args`
  touches `repo`, so the raw argument and the one `repo_target` reads are
  the same.
- `repo_target` reads `args["repo"].as_str()`, so a non-string `repo` (a
  number, an object) reads as the review tree. An empty string is an
  unknown name.
- The duplicate guard compares a call with the stage's previous call, and
  sets that previous call before the call reaches `ToolBox::call`. So a
  blocked duplicate always repeats a call that did reach the toolbox, with
  the same name and arguments, and therefore the same classification.
- The early hand-off is sent before the first attempt runs
  (`local_review.rs:1148`), so a review's reference record exists before
  any of its tool calls are recorded.
- SQLite `CASE` evaluates only the branch it takes. `json_type` on invalid
  JSON raises an error, but under `CASE WHEN json_valid(x) THEN ... END`
  it is never reached.

## 4. Design

### 4.1 Which repository a call was aimed at

A new pure function in `src/toolbox/mod.rs`, next to `repo_target`. It
encodes the same rule `ToolBox::call` applies, so the two classify any call
the same way:

```rust
/// Which repository a recorded tool call was aimed at, by the rule
/// ToolBox::call applies to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallTarget {
    /// The tool reads no repository (read_prompt, an unknown tool name). A
    /// stray repo argument on it is ignored, as the toolbox ignores it.
    NoRepository,
    /// The code under review: repo absent, "review", or not a string.
    Review,
    /// The attached reference at this index of the list passed in.
    Reference(usize),
    /// A name the review did not have attached: dormant, unknown, or any
    /// name when nothing is attached. The toolbox refuses these calls.
    Unattached,
}

/// Classifies one recorded call. `repo` is the top-level repo argument
/// when it is a JSON string; `attached` is the review's attached reference
/// names in configured order.
pub fn call_target(tool_name: &str, repo: Option<&str>, attached: &[&str]) -> CallTarget;

/// Whether a tool, named as the model named it, takes the repo argument.
/// Normalised as ToolBox::call normalises it.
pub fn tool_reads_repo(tool_name: &str) -> bool;
```

- `tool_reads_repo` answers from the review registry itself, not from a
  list kept here. `ToolBox::new`'s registrations move into
  `fn review_registry(with_prompts: bool) -> ToolRegistry<SashikoToolContext>`.
  A `LazyLock` holds the set of tool names whose `parameters()` declare
  `REPO_ARG`. A tool added later is classified correctly without an edit
  here.
- `ToolBox::call` itself is not changed. A test drives one toolbox and
  `call_target` with the same calls and checks that they agree (§6.1).
- Only the top-level `repo` counts. The five driver-a rows with `repo`
  inside `files[]` (§1.3) are `Review`, which is what was served.

**What the count is, exactly.** The classification agrees with the
toolbox. The *count* is of recorded calls, and the daemon records two kinds
of call the toolbox never sees (§1.2): blocked duplicates and final-turn
calls. So a reference's count can exceed the reads the toolbox served, by
at most 12 kernel-naming rows in the deployment data (§1.3). This matters
for the zero alarm only one way:
- A blocked duplicate repeats a call that reached the toolbox (§3). It can
  raise "kernel 3" to "kernel 4", but it cannot turn a true "kernel 0"
  into a nonzero.
- A final-turn call that names a reference, on a review that otherwise
  never read it, would turn the alarm's 0 into a 1. With 30 zero-length
  rows across all three deployments, this is accepted and the hover says
  the count includes calls the session declined to run.

### 4.2 Storage and queries

**No migration and no new column** (D1). The repository is read back from
`arguments` at query time. Migration 017 stays reserved for the
prior-revision step, and `DESIGN_PORT_FINDING_DISPLAY.md` §4.2 and §4.6
stand as written. If D1 (b) is chosen instead, §4.7 says what changes.

**One grouped query per payload**, in `src/db.rs`:

```sql
SELECT tu.review_id,
       tu.tool_name,
       CASE WHEN json_valid(tu.arguments) THEN
           CASE WHEN json_type(tu.arguments, '$.repo') = 'text'
                THEN json_extract(tu.arguments, '$.repo') END
       END AS repo,
       COUNT(*)
FROM tool_usages tu
WHERE tu.review_id IN (SELECT value FROM json_each(?))
GROUP BY 1, 2, 3
```

- It uses the `json_each(?)` id-list form the bug queries already use
  (`db.rs:3131`), and the existing index `idx_tool_usages_review`.
- The nested `CASE` means a NULL, invalid or non-object `arguments`, or a
  non-string `repo`, yields NULL (`Review` for a tool that reads a repo)
  without raising an error. No port or fork row is invalid today (§1.3),
  but the counts must not fail a page if one ever is.
- Grouping by tool name and repo keeps the result small: a review has about
  ten tools and one to three repo values, so a few dozen rows rather than
  hundreds.

The reviews' attached names come from `reviews.reference_repos`, parsed
with `references::parse_record`. A record that is missing or does not parse
gives no split (see below).

```rust
// src/db.rs
/// Tool calls recorded for each of these reviews, by repository.
pub async fn tool_call_counts(&self, review_ids: &[i64]) -> Result<HashMap<i64, ToolCallCounts>>;
```

It reads `id, reference_repos` for the ids, runs the query above, and folds
each group through `call_target`:

- Every group adds its count to `total`.
- `Reference(i)` adds it to `references[i].calls`.
- `Unattached` adds it to `unattached`.
- `Review` and `NoRepository` add only to `total`. No reader needs them
  apart (D2).
- **A review with no record** (no references configured, a row from before
  migration 014, a fork row, or a lost hand-off) gets `by_reference: None`
  (§4.3). Its calls go to `total` only. The type says the split is unknown,
  so no reader can show a zero for it, and no call is guessed to be to an
  unattached repository. On an unconfigured deployment this is a slight
  loss, accepted: there the `repo` argument is not even declared (§1.2),
  so a call naming one is rare, and it is counted in `total`.
- **A review with a record but nothing attached** (every reference dormant
  for the patch) gets `by_reference: Some` with no references. Its calls
  naming a dormant reference count as `unattached`.
- A review with no calls gets `total = 0`, and, with a record, a zero for
  each attached reference. It does not get an absent entry. Whether those
  zeros are shown is the page's rule (D4).

**Review payloads.** `get_patchset_summary` (`db.rs:7845`),
`get_patchset_details` (`db.rs:7582`) and `get_review_details`
(`db.rs:8149`) each call `tool_call_counts` with the ids they selected, and
set `"tool_calls"` on each review object to the serialized counts. The
summary and details queries filter by the patches on the current page, so
the counts cover the same reviews and no others. Under embargo the reviews
array is emptied as today, and the counts go with it.

### 4.3 API types and the activity poll

In `src/api.rs`, which is unconditional, so `sashiko-cli` builds with
`--no-default-features`:

```rust
/// Tool calls recorded for one review attempt, or summed over one run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCallCounts {
    /// Every call the model made that the daemon recorded, of every tool,
    /// to every repository, including calls to unattached repositories and
    /// the few the session declined to run.
    pub total: u64,
    /// The calls split by reference, or None when it is not known which
    /// references the review had: no reference record (none configured, a
    /// row from before migration 014, a fork row, a lost hand-off).
    #[serde(default)]
    pub by_reference: Option<ReferenceSplit>,
}

/// How a review's calls fell across the references it had.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReferenceSplit {
    /// One entry per reference attached to the review, in configured
    /// order, zero counts included. Empty when every reference was dormant.
    pub references: Vec<ReferenceCalls>,
    /// Calls naming a repository the review did not have attached. The
    /// toolbox refused them; they read nothing.
    pub unattached: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceCalls {
    pub name: String,
    /// Calls aimed at this reference: cache hits and calls git rejected
    /// included.
    pub calls: u64,
}

/// One patch's tool calls on the activity poll.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatchToolCalls {
    pub patch_id: i64,
    /// Review rows in the patch's current run (§4.5); more than one when
    /// the run was retried.
    pub attempts: u32,
    #[serde(flatten)]
    pub counts: ToolCallCounts,
}

pub struct PatchsetActivityResponse {
    pub live: bool,
    pub entries: Vec<ActivityEntry>,
    #[serde(default)]
    pub finished: bool,
    /// Tool calls of each patch named by `entries`, over its current run.
    /// Defaulted so a client can read an older daemon.
    #[serde(default)]
    pub tool_calls: Vec<PatchToolCalls>,
}
```

Also in `src/api.rs`, shared by the server and the CLI (§4.5, §4.6):
- `ToolCallCounts::sum(counts) -> ToolCallCounts`. `total` adds up.
  `by_reference` is `Some` when every summed review that made a call has
  one; references merge by name, in the order each name first appears,
  and `unattached` adds up. Otherwise it is `None`: a run with an attempt
  whose split is unknown has an unknown split, rather than a zero that
  attempt might contradict. Reviews with no calls are ignored for this
  purpose; they add nothing to any count.
- `review_runs(rows: &[ReviewAttempt]) -> Vec<Vec<i64>>`, one patch's
  review ids, in id order, split into runs (§4.5).
- `ToolCallCounts::breakdown(&self) -> String`, the plain-text clause the
  CLI prints. It returns an empty string when `total` is 0, when
  `by_reference` is `None`, or when the split has no references and
  `unattached` is 0. Otherwise, for example: `(72 to kernel, 0 to
  rdma-core)`, `(72 to kernel; 3 to unattached repositories)`, `(1 to an
  unattached repository)`. It is the same wording as the page (§4.4), and
  tests on both sides pin the same strings.

**Handler** (`server.rs:985`).
- When `entries` is non-empty (live or persisted), it collects the
  distinct `patch_id`s of the entries and fills `tool_calls` from
  `Database::patch_tool_calls(patchset_id, &patch_ids)` (§4.5). The query
  reads only those patches' reviews, so the payload matches its doc and a
  poll after a restart, whose persisted entries may cover only some
  patches, computes nothing that no reader uses.
- With no entries there is no stage table to put counts on, so
  `tool_calls` stays empty and the poll of a finished patchset costs
  nothing extra.
- A database error is logged with `warn!` and gives an empty list. The
  counts are advisory, and the stage table must not go blank because of
  them. Today's handler fails the whole poll on any database error; this
  one query does not.

**Public route.** The comment at `server.rs:330` lists what the activity
routes carry. It gains "tool call counts per patch, with the names of the
references attached to it". A count is progress metadata like a turn
count. The names come from the operator's configuration, and which ones are
attached follows from the patch's file list, which the patchset page already
shows. Neither is review text.

### 4.4 Web page

**Wording, one place.** Two new functions next to `stageTableParts`:

- `toolCallBreakdown(counts)` returns `''` or an escaped clause. It returns
  `''` when `counts.total` is not a positive safe integer (D4), or when
  `counts.by_reference` is not an object. Otherwise:
  - ` (72 to kernel, 0 to rdma-core)` — every entry of
    `by_reference.references`, in order, zeros included;
  - ` (72 to kernel, 0 to rdma-core; 3 to unattached repositories)` —
    `unattached` when it is a positive integer, singular as `1 to an
    unattached repository`;
  - ` (3 to unattached repositories)` — calls to unattached repositories
    with nothing attached.

  Entries whose `name` is not a string or whose `calls` is not a
  non-negative safe integer are skipped. Names are escaped.
- `toolCallsText(counts, attempts)` returns `''` unless `counts.total` is a
  non-negative safe integer. Otherwise it returns `N tool call` or
  `N tool calls`, then ` over K attempts` when `attempts > 1`, then the
  breakdown.
- `toolCallsTitle()` returns the one explanation both places use: "Tool
  calls the model made, as the daemon recorded them, including any the
  session declined to run (a repeat of the call just before it, or a call
  on the final turn). N to a reference: calls aimed at that reference,
  cache hits and calls git rejected included. To unattached repositories:
  calls naming a repository this review did not have; they were refused
  and read nothing. The rest went to the code under review or to tools
  that read no repository."

**Live** (Goal 1).
- `stageTableParts(rows, live, toolCalls)` takes the patch's
  `PatchToolCalls` (or `null`). When `toolCallsText` is non-empty it
  appends ` — ${text}` to `summaryHtml`. A reader sees:
  "Stage progress (2 running, 3 done) — 120 tool calls over 2 attempts
  (30 to kernel, 0 to rdma-core)".
- `paintStageProgress(host, entries, live, toolCalls)` passes it through.
  When the text is non-empty, the summary's title gains a second sentence:
  "Tool calls: this patch's current run, every attempt." followed by
  `toolCallsTitle()`. With no counts the title is exactly today's.
- `refreshActivity` builds a `Map` from `data.tool_calls`, if it is an
  array, keyed by `Number(patch_id)`, and gives each host its patch's
  entry. A response with no `tool_calls`, from an older daemon, paints
  exactly as today.
- The persisted ("stopped") table shows the counts too. They come from the
  database, so they are still true after a restart.
- `renderReviewCard`'s recorded table passes `null` (as fork 670636505f34
  does), so the card's breakdown never repeats the number in its own
  summary.

**Card** (Goal 2).
- Under "Tokens used:", inside the same `showDetails` block (Reviewed,
  Partial, Failed, Skipped; a timed-out review's row is Failed), add:
  `<strong>Tool calls:</strong> 350 (72 to kernel, 0 to rdma-core)`.
- The value is `r.tool_calls.total`, then ` on this attempt` when
  `r.attempt > 1`, then `toolCallBreakdown`. It is "—" when `r.tool_calls`
  is missing (an older daemon). The attempt marker mirrors the duration's
  " total" on the same card: the duration covers the run, the tokens, the
  stage table and the tool calls cover this attempt.
- Its title is `toolCallsTitle()`.
- A Cancelled or In Review card shows no tokens and no tool calls, as
  today. While a review runs, the live summary carries its count.

**References line** (D3).
- `renderReviewReferences` stops printing ", N reads". The per-reference
  count now sits on the "Tool calls:" line, from the same query as the live
  figure.
- The line keeps the name, revision, short commit, why it was attached,
  kind and full commit in the hover. The hover drops "reads count ..." and
  "read count not reported ...".
- Nothing else about the line changes: attached references only, dormant
  ones never named (81c18acf1683), drawn for every status.

**Zero and absent** (D4):

| Situation | Live summary | Card |
| --- | --- | --- |
| No reference record (none configured, pre-014 row, fork row, lost hand-off) | `— 350 tool calls` | `Tool calls: 350` |
| Record, nothing attached (all dormant) | `— 350 tool calls`, plus `(3 to unattached repositories)` if any | same |
| kernel attached, never read, other calls made | `— 350 tool calls (0 to kernel)` | `Tool calls: 350 (0 to kernel)` |
| kernel and rdma-core attached | `(72 to kernel, 0 to rdma-core)` | same |
| References attached, no call made yet / at all | `— 0 tool calls` | `Tool calls: 0` |
| A run with an attempt whose split is unknown | total only | per attempt, as above |
| A dormant reference | never named | never named |
| `unattached = 0` | no unattached clause | no unattached clause |
| Attempt 3 of a retried run | the run: `— 47 tool calls over 3 attempts (...)` | `Tool calls: 7 on this attempt (...)` |

A per-reference zero is shown exactly when the reference was attached and
the review made calls, because only then does it say the review looked
elsewhere and not there.

**`docs/configuration.md`** ("Recorded on each review", lines 405-425):
- The card example becomes the References line without reads, and the
  "Tool calls:" line is added, with what each part counts.
- The sentences about reads change: a daemon review's record no longer
  carries read counts (§4.8); the local worker JSON does, summed over the
  worker's attempts, and there a read is any call the toolbox resolved to
  the reference, cache hits and calls git rejected included. The old
  wording "calls git refused" goes, so "refused" is not used in two senses.

### 4.5 Attempts, retries, Partial and timed-out reviews

**A card counts its own review row.** Each daemon attempt is its own row
and its own card ("on attempt 3" in the run note), so each card shows that
attempt's calls, marked "on this attempt" when it is not attempt 1. In the
driver-a example (§1.3) the attempt-1 card shows the calls and the
attempt-2 to 4 cards show "Tool calls: 0 on this attempt".

**Runs.** `api::review_runs` splits one patch's rows, in id order, into
runs. A row whose `attempt` is NULL or 1 starts a new run; a row with
`attempt > 1` continues the run before it. A NULL is only ever a row from
before migration 015 or a Skipped row (§4.5.1 removes the in-flight NULL),
and each of those is a whole run.

**The live figure counts the current run** (D5).
`Database::patch_tool_calls(patchset_id, patch_ids)`:
1. Reads `id, patch_id, attempt` for the reviews of those patches, ordered
   by `id`.
2. Takes the last run of each patch from `review_runs`.
3. Sums `tool_call_counts` over that run's rows with
   `ToolCallCounts::sum`; `attempts` is the number of rows in the run.

On the deployment data this gives:
- driver-a: the 4 patches whose attempt 1 made the calls show them "over
  4 attempts", not the last attempt's 0. The other 36 show "0 tool calls
  over 4 attempts".
- kernel-fork's 10 reruns show only the latest run, not double.
- kernel-fork patch 12 (Failed on attempt 1, Reviewed on attempt 2)
  shows both attempts.

#### 4.5.1 Stamping the attempt when the row is created

Today a new retry row is inserted with `attempt` NULL and stamped a moment
later (§1.2). A poll in that window would see a NULL row, start a new run
with it, and show "0 tool calls" for a run that has made many. So
`create_review` takes the attempt and writes it in the `INSERT`:
`create_review(..., prompts_hash, attempt: Option<u32>)`. The retry loop
passes `Some(retries + 1)`; the Skipped path and the tests pass `None`.
`set_review_attempt` stays for a reused Pending row. No in-flight row is
ever NULL after this.

#### 4.5.2 A row reused after a restart (D7)

When the retry loop, or the Skipped path, takes a Pending row a restart
left behind, it deletes that row's `tool_usages` before stamping it. The
getter `get_pending_review_id` becomes `take_pending_review_id`, which
returns the id and deletes the row's tool usages; its two callers are the
only ones. A row not reused has none to delete.

After this, everything a reused row reports describes the new run: its
tokens, its stage table, its live log (cleared at worker start today), its
reference record (overwritten by the new hand-off), and its tool calls.
That includes the classification: the calls are judged against the record
of the run that made them, even when the restart was the one that applied
a `[[git.references]]` change. The interrupted run's calls leave
`/api/stats/tools` too; its tokens were never recorded either.

#### 4.5.3 Statuses

- **Partial.** A salvaged review is one row. Its calls are everything it
  made up to the wind-down, and the card shows them.
- **Timed out.** The patch verdict is TimedOut and the row is Failed. Calls
  made before the kill are recorded, so the card shows them. A per-reference
  number now appears for a review whose `reads` were never reported.
- **Failed, retried.** Each row is counted on its own card. A rerun deletes
  Failed rows with no interaction together with their calls (`db.rs:9356`),
  so they leave both the card and the live run.
- **Cancelled.** Counted live while it runs. Its card shows no details, as
  today.

### 4.6 CLI

`sashiko-cli`'s `print_patch_timings` (`sashiko-cli.rs:1509`) prints one
line under each patch's stage table (D6):

```
    Tool calls: 120 over 2 attempts (30 to kernel, 0 to rdma-core)
```

- When the patch has live entries, the line uses the activity response's
  `PatchToolCalls` for the patch, the current run.
- Otherwise it uses the run that contains the review
  `find_best_review_for_patch_refs` picks, the same review whose recorded
  stage table it prints. The CLI already has every review of the patch,
  each with `attempt` and `tool_calls`, so it splits them with
  `api::review_runs` and sums with `ToolCallCounts::sum`, as the daemon
  does. A run of all-failed attempts therefore prints "N over 4 attempts",
  not the last attempt's 0, during the run and after it alike.
- ` over K attempts` is added when the run has more than one row.
- No line is printed when neither source has counts (an older daemon).
- `PatchActivity` gains `tool_calls: HashMap<i64, PatchToolCalls>`.
- The breakdown comes from `ToolCallCounts::breakdown`.

When the resume port (fork eaa5f78b889b) lands, a resumed review is a new
row that reuses completed stages without re-running their calls. That step
decides whether its run includes the source review. This step's run rule
does not include it, because resumed rows are not attempts > 1.

### 4.7 If D1 (b) is chosen: a stored column

For the record, so a reviewer can compare. This is not the recommendation.

- **Migration `017_tool_usages_repo.sql`.** Following migrations 014 and
  015, the `ALTER TABLE tool_usages ADD COLUMN repo TEXT` runs from Rust
  only when `PRAGMA table_info` lacks it, because fork databases already
  have it.
- **Write.** `record_tool_calls` stores the top-level string `repo`, or
  NULL when it is absent or not a string.
- **Port rows.** Backfilled with the nested `CASE` of §4.2:
  `UPDATE tool_usages SET repo = <that> WHERE repo IS NULL`.
- **Fork rows.** `review` would have to be rewritten to NULL wherever the
  argument has no `repo`, so the column means one thing. Its `kernel`
  values agree with the arguments (§1.3).
- **Classification** still needs the review's attached list, so §4.1 and
  §4.2 are unchanged except that they read the column.
- **Renumbering.** The prior-revision step moves to 018:
  `018_prior_revision_findings.sql`, or `018_patchsets_mr_body.sql` and
  `019_findings_prior.sql` if it splits. `DESIGN_PORT_FINDING_DISPLAY.md`
  §4.2 ("017 stays open") and §4.6 (the 017 file names) are amended in this
  step's design commit.

### 4.8 The daemon stops storing `reads`

After D3 (a) nothing in the repository reads a daemon review's
`reference_repos[].reads`: the page stops rendering it, and `sashiko-cli`
never did. Kept, it would be a second per-reference count with no reader,
which can disagree with the `tool_usages` count (§4.1), exactly what D1
rejects. So `store_final_reference_repos` (`reviewer.rs:4511`) and its call
(`:4088`) go. The early hand-off already stores the same record without
`reads` (§1.2), and it is the one the code was built to rely on: it is
awaited inline before the final line is handled.

- The local worker JSON keeps `reads`. There it is the only count, summed
  over the worker's in-process attempts (`attempts_counting_reads`).
- `AttachedReference.reads`'s doc changes to say that only the local
  worker fills it, and that daemon rows written before this step may
  carry it.
- Existing port rows that carry `reads` are left as they are; the API
  serves them, nothing renders them.
- What is lost: the final record was also a fallback when the early
  hand-off was not stored (a database error, a mismatched patch index).
  Such a review now keeps a NULL record and shows its total only, which
  §4.2 already handles.

## 5. Type-Driven State

| Concept | Representation | Replaces |
| --- | --- | --- |
| Which repository a call was aimed at | `toolbox::CallTarget` from `call_target(tool, repo, attached)` | fork's string column, `kernel` or `review` |
| Whether a tool reads a repository | `toolbox::tool_reads_repo`, derived from the registered schemas | nothing (the fork counted any `repo`) |
| One review's counts | `api::ToolCallCounts { total, by_reference: Option<ReferenceSplit> }` | fork's untyped `{total, reference}` |
| Whether the split is known | `by_reference: None` vs `Some(ReferenceSplit { references, unattached })` | a zero that meant both "none" and "not known" |
| One patch's live counts | `api::PatchToolCalls { patch_id, attempts, counts }` on `PatchsetActivityResponse.tool_calls` | fork's `json!` field, `{}` on error |
| A patch's runs | `api::review_runs`: a row with attempt NULL or 1 starts a run; no in-flight row is NULL | fork: every review of the patch |

The page reads JSON. Its type is the shape `toolCallBreakdown` and
`toolCallsText` agree on, and the verifier tests it.

## 6. Testing Plan

Run `make check-pr` from a throwaway clone, not under a git hook (AGENTS.md,
Testing). After each web commit, run `scripts/verify_stage_progress_ui.sh`.
After the step, run the ignored server tests as well.

### 6.1 Rust

`src/toolbox/` (`tools_test.rs`):
- `call_target_agrees_with_the_toolbox`. Build a toolbox with two attached
  references (kernel, rdma-core) and one dormant (iproute2), as
  `reference_reads_are_counted_per_reference` does, and run these calls
  through `ToolBox::call`:
  - repo absent, `"review"`, `"kernel"` twice (the second a cache hit),
    `"rdma-core"`, `"iproute2"` (dormant), `"nope"` (unknown), `""`, and a
    number;
  - `read_prompt` with `repo: "kernel"`;
  - an unknown tool name;
  - `GIT_GREP` in upper case.

  Classify the same calls with `call_target`. The per-reference tallies
  must equal `toolbox.reference_reads()`, and a call must be `Unattached`
  exactly when `ToolBox::call` returned the repo error. This test covers
  classification only; it does not go through the stage session, whose
  declined calls §4.1 accounts for separately.
- `call_target_with_nothing_attached`: any name is `Unattached` for a tool
  that reads a repo, and `NoRepository` for one that does not.
- `tool_reads_repo_matches_every_declaration`: for every tool in the review
  registry, `tool_reads_repo(name)` is true exactly when its `parameters()`
  declare `repo`. `read_prompt` is false; all eight `git_*` tools are true.

`src/db.rs` tests:
- `tool_calls_are_counted_per_review_by_repository` (adapts the fork's
  test).
  - A review with a record attaching kernel and rdma-core reports total 0,
    `by_reference` `Some` with [0, 0] and unattached 0 before any call.
  - After rows for review (absent and explicit), kernel x2, a dormant name,
    `read_prompt` with a stray `repo` and `git_read_files` with `repo`
    only inside `files[]`, the counts are total 7, kernel 2, rdma-core 0,
    unattached 1.
- `tool_calls_without_a_reference_record_are_totalled_only`: a review with
  a NULL `reference_repos` and a row naming kernel gives total 1 and
  `by_reference` `None`.
- `tool_call_counts_survive_unreadable_arguments`: rows with NULL
  arguments, `null`, `not json`, `[1,2]` and `{"repo":5}` are each counted
  in `total`, and the query does not fail.
- `review_payloads_carry_tool_calls`: `get_patchset_summary`,
  `get_patchset_details` and `get_review_details` give the same
  `tool_calls` for the same review, and a review on another page of a
  paginated summary is not counted.
- `create_review_stamps_the_attempt`: a row created with `Some(2)` reads
  attempt 2 straight after the insert; with `None`, NULL.
- `taking_a_pending_review_clears_its_tool_calls`: a Pending row with
  three tool usages, taken by `take_pending_review_id`, has none, and
  another review's rows are untouched. A row whose record is then replaced
  by a hand-off attaching a different reference counts new calls against
  the new record only.
- `patch_tool_calls_sum_the_current_run`: three attempts with 40, 0 and 7
  calls give `attempts = 3` and total 47, and an earlier rerun's Reviewed
  row is not included. Two attempts whose records attach different names
  merge by name. A run in which one attempt with calls has no record gives
  `by_reference` `None`; one in which only a zero-call attempt has no
  record keeps the split. Only the patches passed in are returned.

`src/reviewer.rs` tests:
- `recorded_arguments_keep_the_repo_argument`. `record_tool_calls` on a
  response with `{"pattern":"x","repo":"rdma-core"}` stores text whose
  `json_extract(arguments, '$.repo')` is `rdma-core`. This pins the storage
  contract §4.2 reads, so a later change that truncates or compresses
  `arguments` fails here.
- The tests of `store_final_reference_repos` go with it (§4.8). A test
  that a worker's final output carrying `reads` leaves the stored record
  without them replaces them.

`src/api.rs` tests:
- An activity response without `tool_calls` (an older daemon) still
  deserializes, with an empty list. A review's `tool_calls` without
  `by_reference` deserializes with `None`.
- `review_runs` (pure): `[1, 2, 3, 4]` is one run of four; `[1, 1]` gives
  two runs; `[NULL, 2]` is one run; `[1, 2, 1]` gives the last row alone;
  a freshly created attempt-1 row with no calls after a finished run is a
  run of its own.
- `ToolCallCounts::sum`: the merge rules of §4.3.
- `ToolCallCounts::breakdown` gives `(72 to kernel, 0 to rdma-core)`,
  `(72 to kernel; 3 to unattached repositories)`, `(1 to an unattached
  repository)`, and `""` for total 0, for `by_reference` `None`, and for
  an empty split.

`src/bin/sashiko-cli.rs` test:
- `tool_calls_line_sums_the_chosen_review_run`: four Failed attempts,
  only the first with calls, print `Tool calls: 120 over 4 attempts (30 to
  kernel)` with no live entries. A Reviewed review from an earlier run
  plus a failed newer run prints the earlier run's figure, under the
  earlier run's stage table.

`src/server.rs` tests (ignored, run by hand like the others):
- `/api/patchset/activity` for a patchset with a live stage entry for one
  of two patches carries `tool_calls` for that patch only, with the counts
  from the database.
- A patchset with no entries carries an empty list.

### 6.2 Migration

None in this step (D1 (a)). Every existing migration test is untouched. If
D1 (b) is chosen, add:
- a test that 017 adds the column to a port database;
- a test that 017 leaves a fork-shaped `tool_usages` (with `repo`, `kernel`
  and `review` values) with the same values after normalization;
- a test that running it twice changes nothing.

### 6.3 UI verifier (`scripts/verify_stage_progress_ui.mjs`)

- Add `toolCallBreakdown`, `toolCallsText` and `toolCallsTitle` to
  `NAMES`. `stageTableParts`, `paintStageProgress`, `refreshActivity` and
  `renderReviewCard` are already there.
- Add a `toolCallChecks(fns)` group, run as **case 11: tool calls**.

Wording:
- `{total: 1}` gives "1 tool call"; `{total: 350}` gives "350 tool calls".
- Two references, one at zero, give `(72 to kernel, 0 to rdma-core)`, in
  the order given.
- `unattached: 3` adds `; 3 to unattached repositories`; `unattached: 1`
  adds `; 1 to an unattached repository`; `unattached: 0` adds nothing.
  Calls to unattached repositories with no references give ` (3 to
  unattached repositories)`.
- `total: 0` with references attached gives "0 tool calls" and no clause.
- `by_reference: null` gives the total alone.
- `attempts: 3` adds ` over 3 attempts`; `attempts: 1` adds nothing.
- A name `<script>` is escaped.
- The word "refused" appears nowhere in the clause or the title.
- Malformed input renders nothing and does not throw: `null`, a string,
  `{total: -1}`, `{total: 1.5}`, `{total: '3'}`, a `by_reference` whose
  references are not an array, and an entry with no name.

Live:
- `paintStageProgress(host, entries, true, {patch_id: 20, attempts: 2,
  total: 47, by_reference: {references: [{name: 'kernel', calls: 3}],
  unattached: 0}})` gives a summary ending in ` — 47 tool calls over 2
  attempts (3 to kernel)`, and a title that contains today's sentence and
  "current run, every attempt".
- With `null` counts, the summary and the title are exactly today's.
- `live = false` (persisted) still shows the counts.
- `refreshActivity`, with a fetch stub that returns entries for patches 20
  and 21 and `tool_calls` for both, puts each count on its own host.
- A response with no `tool_calls` leaves both summaries as the label alone.
- A `tool_calls` that is not an array is ignored without throwing.

Card:
- A Reviewed card with `tool_calls: {total: 350, by_reference:
  {references: [{name: 'kernel', calls: 72}, {name: 'rdma-core', calls:
  0}], unattached: 0}}` shows "Tool calls:" and "350 (72 to kernel, 0 to
  rdma-core)", with `toolCallsTitle()` as its title.
- A Failed card with `attempt: 3` and `{total: 7, ...}` shows "7 on this
  attempt (...)"; with `attempt: 1` or none, no marker.
- A Failed card with total 0 and kernel attached shows "Tool calls: 0" and
  no "0 to kernel".
- Partial and Failed cards show the line. In Review and Cancelled cards do
  not.
- A card with no `tool_calls` shows "Tool calls:" with "—".
- The recorded stage table's summary carries no tool-call text.

References line (reworks the existing `referenceChecks`):
- 'present: read count' and 'present: one read is singular' are replaced
  by "the line shows no read count", which holds even when the record
  carries `reads: 34`.
- The two 'missing reads' checks are replaced by "the title says nothing
  about reads".
- Every other reference check stays.

Mutations, each of which must fail `toolCallChecks`:
- zero references dropped (`.filter(r => r.calls > 0)` added): caught by
  `0 to rdma-core`;
- the total > 0 condition removed: caught by the total-0 card;
- `escapeHtml` removed from the name;
- the unattached clause shown when it is 0;
- the attempts note shown for one attempt, and never shown;
- the card's "on this attempt" marker removed, and shown on attempt 1;
- `stageTableParts` ignoring its `toolCalls` argument;
- the live title left unchanged when counts are present;
- `refreshActivity` passing `null` to every host;
- `refreshActivity` keying by `review_id` instead of `patch_id`;
- the card line removed from `renderReviewCard`;
- the recorded table passed the counts.

And one that must fail `referenceChecks`: `renderReviewReferences` still
appending `${a.reads} reads`.

### 6.4 By hand

On the demo setup, with a stub-model daemon and kernel plus rdma-core
references, review a patch under `rdma/drv/` and one under `eth/ionic/`:
- During the run, the first shows rising counts for kernel and rdma-core.
  The second shows kernel only; rdma-core is dormant there. Before the
  first call, each shows "0 tool calls" with no per-reference clause.
- After the run, for a run of one attempt, each card's "Tool calls:" line
  matches the last live figure, and `sashiko-cli` prints the same.
- Make the stub fail the first attempt of a third patch after some calls,
  and succeed on the second. The live figure shows the sum "over 2
  attempts"; the attempt-1 card shows its own calls; the attempt-2 card
  shows "on this attempt"; the two cards add up to the live figure;
  `sashiko-cli` prints the sum "over 2 attempts".
- Kill the worker of a fourth review to time it out. Its Failed card shows
  the counts recorded before the kill.
- Restart the daemon during a fifth review. After the restart, its live
  figure starts from 0 and its finished card counts only the new run.

## 7. Commit Breakdown

Each commit is one consistent change. Each has `Signed-off-by`, body lines of
72 characters or fewer, and no backticks. `make check-pr` passes before each
commit.

1. **designs: plan the port of per-review tool call counts.** This
   document.
2. **toolbox: say which repository a tool call is aimed at.**
   `CallTarget`, `call_target`, `tool_reads_repo`, `review_registry`, and
   the §6.1 toolbox tests. Nothing reads them yet, and the agreement test
   is the reason to land them alone.
3. **reviewer: stamp a review's attempt when its row is created.**
   `create_review` takes the attempt (§4.5.1), with its test. It fixes the
   existing "on attempt N" note's window too, and stands alone.
4. **reviewer: start a reused review's tool calls afresh.**
   `take_pending_review_id` (§4.5.2, D7), with its test.
5. **db: count each review's tool calls by repository.**
   - `ToolCallCounts`, `ReferenceSplit`, `ReferenceCalls`,
     `ToolCallCounts::breakdown` in `api.rs`.
   - `Database::tool_call_counts`, and `tool_calls` on the reviews of the
     three review payloads.
   - The db, reviewer and `breakdown` tests.
6. **api: carry each patch's tool calls on the activity poll.**
   - `PatchToolCalls`, `review_runs`, `ToolCallCounts::sum`,
     `Database::patch_tool_calls`.
   - `PatchsetActivityResponse.tool_calls` and the handler.
   - The public-route comment, and the api and server tests.
7. **web: show a review's tool calls on its card.**
   - `toolCallBreakdown`, `toolCallsText`, `toolCallsTitle`, the "Tool
     calls:" line.
   - The References line without reads.
   - The card, wording and references verifier checks and their
     mutations.
8. **reviewer: stop storing reference reads on daemon reviews.** §4.8:
   `store_final_reference_repos` goes, `AttachedReference.reads`'s doc,
   and `docs/configuration.md` "Recorded on each review". After commit 7,
   so the page never shows "read count not reported" for new reviews.
9. **web: show a patch's tool calls while it is reviewed.**
   `stageTableParts`, `paintStageProgress` and `refreshActivity` carry the
   counts and the title, with the live verifier checks and mutations.
10. **cli: print each patch's tool calls.** `print_patch_timings` and
    `PatchActivity` (D6), with its test.
11. **designs: record what the tool-call port built.** Status, and any
    difference from this plan.

Commits 2-6 come first so the data exists, and is right across retries and
restarts, before anything shows it. Commit 7 alone already shows the counts
on finished reviews. Commit 9 adds the live figure, and commit 10 the
terminal.

## 8. Open Decisions

Each decision has options and a recommendation. None is chosen yet.

**D1. Where the repository of a call is stored.**
- (a) Nowhere new. Read the top-level `repo` from `arguments` at query time
  (nested `CASE`, §4.2) and classify it in Rust. No migration; 017 stays
  with the prior-revision step.
- (b) Migration 017 adds `tool_usages.repo` with the raw top-level name, as
  in §4.7. The prior-revision step moves to 018.
- (c) Migration 017 adds a column holding the classification, decided by the
  daemon from the request's `repo` enum when it records the call.

*Recommendation: (a).* `arguments` already holds the name, exactly as the
model sent it. On 84,890 deployment rows the fork's column never said
anything else (§1.3). A second copy needs a migration, a guard for fork
databases, a backfill, and a rule for keeping the two in step, and the
fork's own backfill shows how they drift: its `LIKE` would mislabel 5
real rows. (c) stores a judgement that depends on the review's attached
list, which is already on the review row. It would also be wrong for a stage
whose request carries no tools (§1.2). The cost of (a) is one
`json_extract` per row read. That is about 350 rows per review, grouped, and
only for the reviews on the page or in the polled patches. A test pins the
storage contract it relies on (§6.1).

**D2. What a call counts as.**
- (a) The toolbox's rule (§4.1), with calls to unattached repositories
  shown as "N to unattached repositories" when there are any.
- (b) The toolbox's rule, with those calls only in the total.
- (c) The fork's rule: whatever `repo` says, on any tool, defaulting to
  review.

*Recommendation: (a).* (c) can show a call the toolbox refused, or a
`read_prompt` call it ignored, as a read of a reference: exactly the "used
the tree" signal the count is for, and false. Between (a) and (b): the
model can know a dormant name from another patch of the series, which is
why `repo_target` explains dormant names (`toolbox/mod.rs:200`). Such a
call costs a turn and reads nothing, and a reader looking at "0 to kernel"
should be able to see "3 to unattached repositories" beside it. The clause
is shown only when nonzero, so it costs nothing on a normal review. It is
not called "refused", because a reference's own count includes calls git
rejected, and one word must not mean two things.

**D3. Where the per-reference count sits on the card, and from which
source.**
- (a) On the "Tool calls:" line, from `tool_usages`, the same query as the
  live figure. The References line drops "N reads", and the daemon stops
  storing `reads` (§4.8). The local worker JSON keeps it.
- (b) On the References line as "N calls" from `tool_usages`, in place of
  reads. The "Tool calls:" line shows the total only.
- (c) Keep "N reads" on the References line. The "Tool calls:" line shows
  the total only, and the per-reference split from `tool_usages` appears
  only live.

*Recommendation: (a).*
- One source means the live figure and the card count the same calls in
  the same way. For a run of one attempt they are the same number; for a
  retried run the card says "on this attempt" and the cards add up to the
  live figure. With (c), a review killed at its deadline shows
  per-reference counts while it runs and "read count not reported"
  afterwards.
- For a review whose worker wrote a final output, `reads` and the
  `tool_usages` count classify every call the same way (§4.1, tested in
  §6.1). They differ only on calls the session declined to run, which the
  daemon records and the toolbox never sees: at most 12 kernel-naming
  blocked duplicates and 30 zero-length rows across all three deployments
  (§1.3). A blocked duplicate cannot hide a true zero (§4.1). So (a) loses
  nothing a reader relied on, and gains a count for every killed review.
- (a) over (b): the split then sits next to the total it is a part of. It
  reads the same as the live summary, and the References line goes back to
  saying only what was attached and why.
- Keeping `reads` stored on daemon reviews with no reader would be a dead
  output and a second count that can disagree with the first, so (a)
  includes §4.8. Q1 confirmed that nothing outside reads it.

**D4. When a per-reference zero is shown.**
- (a) Every attached reference is shown, with 0 included, live and on the
  card, once the review (or run) has made at least one call. With no call
  at all there is no clause. Dormant references are never named. No
  record, or nothing attached, means no per-reference clause.
- (b) The fork's rule: hide any zero.
- (c) Show the zero on the finished card only; live shows only nonzero
  counts.
- (d) As (a), but show the zeros on a review with no calls too.

*Recommendation: (a).* A zero is shown exactly when it could have been
something else and the review was looking somewhere: the rule the fork's
message stated and its code dropped. 7 of 202 out-of-tree deployment
reviews are that zero (§1.3). (b) hides them. (d) puts "0 to kernel" on an
attempt that failed before its first call, which says nothing about the
reference; on driver-a that is 156 of the 160 rows of its 40 retried
patches (§1.3), each of which would raise the alarm falsely. (c) avoids a
live "0 to kernel" in a run's first minutes, but once the model has made
calls that zero is true: the tree has not been read yet. A number that
appears only once it is nonzero reads as a new row arriving rather than a
count rising.

**D5. What the live figure sums.**
- (a) The current run: the newest row with attempt 1 (or NULL) and the
  retries after it, with "over K attempts" when there is more than one.
- (b) The newest review row only.
- (c) Every review of the patch, as the fork did.

*Recommendation: (a).* (b) shows 0 for the 4 driver-a patches whose
later attempts were cancelled at once while attempt 1 did the work, which
reads as the never-consulted alarm. (c) roughly doubles the 10
kernel-fork reruns (§1.3: 7 days apart, 3 the same day with the same
prompts), adding a finished review's cost to a new run. (a) is right for
both, and for the one retry (patch 12). The attempts note says why the
figure can exceed what the stage table beside it shows, and why it can
exceed the card.

**D6. The CLI.**
- (a) `sashiko-cli` prints one "Tool calls:" line per patch in its stage
  timings: the current run while live, else the run of the review whose
  stage table it prints, both summed by the shared `api` functions (§4.6).
- (b) As (a), but after the run print the chosen review's own count, with
  its attempt number.
- (c) Not in this step.

*Recommendation: (a).* The CLI already polls the same response and prints
the same stage tables. Without the line it would drop the new field it
deserializes. (b) would print "0" for the 4 driver-a patches once their
run ended, the false alarm D5 avoids, and the same patch would print a
different number during the run and after it. With (a) the CLI agrees with
the live summary and needs no rule of its own. It is one line and one
commit.

**D7. A review row reused after a restart.**
- (a) Its count keeps the interrupted run's calls, classified by the new
  run's reference record.
- (b) Delete the row's `tool_usages` when it is reused (§4.5.2).

*Recommendation: (b).* A reused row's duration and tokens describe only
the new run (§1.2), and so do its stage table, live log and reference
record. Under (a) the tool-call count would be the one figure on the card
that includes the interrupted run, and its calls would be judged against a
record that may not be the one they ran under: a restart is how a
`[[git.references]]` change takes effect, so a renamed or newly dormant
reference would turn old reads into "to unattached repositories", and a
newly attached one would show a 0 for calls made without it. (b) removes
both. Its cost: `/api/stats/tools` and the benchmark lose the interrupted
run's calls. The benchmark runs a fresh daemon and does not restart it,
and the interrupted run's tokens were never recorded either, so the
statistics stay consistent with each other.

**D8. Fork rows, for the adoption step.** No backfill is needed: fork rows
have the same `arguments` JSON, and §4.2 counts them as it counts port
rows. Two things remain for that step:
- (a) Drop the fork's `tool_usages.repo` after checking it agrees with the
  arguments (it does on every row today). Synthesize a `reference_repos`
  record for each fork review whose logs carry the out-of-tree block or
  which made a kernel call: kernel, kind `kernel`, revision `v7.2`, its
  commit resolved from the configured kernel reference, attached to every
  patch, no `reads`. The 7 never-read reviews then show "0 to kernel".
- (b) Drop the column and synthesize nothing. Fork reviews show the total
  only.
- (c) Leave the column; the port never reads it.

*Recommendation: (a)*, decided by that step, which has the reference
clones. (c) leaves a column that is half NULL from the first port-written
row. (b) is safe but hides the 7 reviews the count exists to show. The
157 driver-a reviews with no logs and no kernel call cannot be shown
to have had a reference, and get no record, so they show the total only
(`by_reference` `None`).

### Open questions

- **Q1.** Does any external consumer read `reviews.reference_repos[].reads`
  from the API? Resolved 2026-10-07: the user knows of none, and nothing in
  this repository reads it besides the page (§1.2). So §4.8 lands as
  written.

## Feynman Summary

Every time the reviewing model asks to look at code, the daemon writes one
row. Each row records which tool was used and the exact arguments, including
which repository it asked for. The fork wanted to show those rows as a
count: how much looking a patch review did, and how much of it was in the
kernel tree, because a kernel tree that was available and never opened
means the review was not grounded. The fork copied the repository name into
a new column, guessed it for old rows with a text search, assumed there was
only one reference tree, and then hid the count whenever it was zero, which
was the one case it existed to show.

The port has several named reference trees, each attached only to the
patches it matters for. So the port does not add a column. It reads the
repository name back from the arguments the row already stores, which on
all 84,890 real rows says exactly what the fork's column said. It then asks
the toolbox's own question: did this call aim at the code under review, an
attached tree, or a name the review did not have? The answer gives a total,
a count per attached tree (zero included), and a count of calls to trees
that were not attached. A handful of rows are calls the model made that
the session then declined to run, such as an exact repeat; they are
counted, and they can never fake a read of a tree nobody opened.

While a patch is reviewed, its progress line shows that count rising,
summed over the attempts of the current run. Each finished attempt's card
shows its own share, marked "on this attempt" when there were several, so
a failed retry that did nothing says 0 without hiding the earlier attempt
that did the work. A review that made no calls at all shows no per-tree
zeros, because a zero only means "never looked there" if it was looking
somewhere. A restart starts a reused review's count over, like its tokens
and its log. A tree that was not attached to a patch is never mentioned,
and an attached tree nobody read shows as 0.
