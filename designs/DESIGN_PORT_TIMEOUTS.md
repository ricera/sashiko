# Design: Port Step 3 -- Review Timeouts, Salvage and Truncated Answers

Status: approved 2026-10-01. Every recommended option in §8 is accepted,
including D17 (a): a series with a patch that failed or timed out with
nothing salvaged is Failed, not Partial. Revised after the
first critique round (incident record, worker error outputs, restart guard,
truncation budgets, commit split). Revised again after review of D6:
`Partial` is now a `ReviewStatus` used by the salvaged review row, its patch
and the patchset, instead of a patch-only status under a `Failed` review and
a `Failed` patchset (§3.8, §3.11, D6, D15-D20).
Branch: `port/upstream` (upstream `origin/main` 2a7e748b1680 plus the ported
fork features). The fork source is `feat/prior-revision-findings`.

## 1. Context and Motivation

Two kinds of failure throw away review work that has already been paid for.

**A review runs out of time.** The daemon gives each worker run a wall-clock
deadline (`review.timeout_seconds`). When the deadline passes, the worker is
killed and everything it gathered is lost:

- In a driver-a series, four patches each timed out four times. That cost
  about twelve hours per patch and produced nothing. Every failed review row
  had `logs`, `stage_durations` and `stage_failures` unset, so nobody could
  tell which stage had been stuck (fork dcdcdf5ac937).
- Salvage with too short a window still loses everything. The last patch of
  driver-a PR 968 spent 21,885 s of a 21,600 s budget in the hardware
  analysis stage. With the fork's wind-down already active, it finished
  deduplication inside the 900 s salvage window and was killed five minutes
  into conflict resolution. Review 193 kept only its stage timings: no logs,
  no output, no findings (fork 1d10318d403b).
- The opposite mistake also happened. After a first salvage rule was added, a
  review that finished all 11 stages with 9 findings was marked Failed. Its
  consolidation stages overran the deadline by 7 minutes and lost nothing
  (fork df962e0e26d0).

**A model answer is cut off.** When a provider stops at its output limit,
`SessionRunner` gives up on the whole stage:

- One patch lost three analysis stages in a single review (resources, security
  and execution-flow). Each had done about an hour of tool calls (fork
  d4af7a8388f0).
- The Claude client sends `effort` inside `thinking`, where the Messages API
  ignores it. So the one setting that controls how much of `max_tokens` goes
  on reasoning has no effect (fork 10e9bd0c3b3f).
- The truncation warning does not say what used up the budget, so an operator
  cannot tell whether to raise `max_tokens`, lower `effort`, or ask for a
  shorter answer (fork 36935e97144e, 059f01bc541c).

Stages still running when a worker dies also leave stale live rows such as
"turn 7/100, awaiting model". The worker that would have closed them is gone
(fork 56de4b78ea37, the deferred half).

### Goals

1. A truncated answer is asked for again, more briefly, instead of failing
   the stage.
2. When a review reaches its deadline, the stages that gather concerns stop,
   and the stages that turn concerns into findings finish. The findings are
   stored, and the review, its patch and (D17) its patchset are marked
   `Partial`, not `Failed`.
3. A review that ran late but lost no coverage counts as an ordinary
   `Reviewed` review.
4. Every attempt records what it did, however it ended: stage timings, which
   stages were still running, and the conversation streamed so far.
5. Outcomes are typed (enums and error types). Nothing is decided by matching
   strings.

### Non-goals

- Cooperative cancellation over stdin. Cancelling still works the upstream
  way: the daemon polls the DB status and kills the worker. The fork's
  `cancel` message and `GRACEFUL_CANCEL_WAIT` belong to the later cancel port
  step (see D1).
- Truncation detection in providers that do not report it today: Bedrock,
  Ollama and the CLI providers (see D10).
- A wall-clock deadline for local `sashiko review`. There the deadline only
  stops retries, so the wind-down signal is never sent.
- Posting partial reviews to email or GitHub (see D6).
- Surviving a consolidation that overruns the hard limit. If the
  non-interruptible stages do not finish within `salvage_seconds`, the worker
  is still killed and the analysis output is lost, exactly as in the PR 968
  incident above. This step records the conversation and stage timings of
  such an attempt (§3.9) but not the stage outputs. The fork's answer,
  per-stage output persistence and `rerun --resume` (1d10318d403b,
  eaa5f78b889b), is a later port step. A larger default window (D4) is this
  step's only mitigation.

---

## 2. Baseline and Fork Delta

### 2.1 What `port/upstream` already does

| Area | Behaviour on HEAD | Code |
|---|---|---|
| Deadline | One `Instant` per worker spawn (`timeout_seconds`, default 3600; Settings.toml uses 7200). It is shared with `DeadlineBudget`, which adds back only time spent waiting at the quota gate. | `reviewer.rs:2753-2770`, `ai/backoff_provider.rs:64-116, 193-201` |
| Timeout | The I/O loop returns the untyped error "Review tool timed out (active time exceeded)" (`reviewer.rs:2847-2852`). That text is matched again at `:3220-3225`, and the child is SIGKILLed (`:3233-3239`). The worker gets no warning. | as listed |
| After a timeout | `complete_review(Failed, "Tool error: ...", logs=None)` runs, which deletes the streamed live log (`db.rs:2216-2221`). The attempt is retried up to `max_retries` (3), each time with a fresh full deadline. | `reviewer.rs:2426-2444` |
| Stage timings | Written only when the attempt returned Ok. | `reviewer.rs:3182-3205` |
| Unfinished stages | `StageTracker` finalises a stage only on `StageFinished`. Rows of a killed worker keep their last turn until the patchset guard drops. | `activity.rs:1419-1490, 944-962` |
| Patch's own row | Cleared when its worker ends. This half of 56de4b78 is already ported. | `reviewer.rs:3256-3259` |
| Slot-observer gate | Only stages that report a row get one. Also already ported from 56de4b78. | `reviewer.rs:2505-2512` |
| Cancellation | The patchset status is polled every 2 s, and the worker is killed on `Cancelled`. Superseded revisions are cancelled automatically. | `reviewer.rs:2823-2845, 2871-2879` |
| Worker stdin | Accepts only `ai_response` and `error`. Any other type makes the worker `exit(1)`. | `ai/mod.rs:909-967` |
| Final turn | On `turns == max_turns` the runner pushes "TURN BUDGET EXHAUSTED" and sends `tools=None`. Validation and provider-error retries give back their turn, so the budget message is pushed again on every retry of the final turn. | `ai/session.rs:319-336, 402, 505` |
| Truncation | `SessionRunner` bails with a plain error, classified Fatal, before it counts usage or logs anything. `ReviewError::OutputTruncated` exists but is never constructed. | `ai/session.rs:411-413`, `worker/prompts.rs:34-36, 48` |
| Response cache | Stores every successful response, including truncated ones. The cache identity already includes `max_tokens`. | `ai/cache.rs:160-190`, `ai/claude.rs:650-664` |
| Effort | Claude sends `{thinking:{type, effort}}` (`claude.rs:97-108, 486-490`), and Vertex Claude reuses that struct (`vertex.rs:68-79, 266`). Bedrock already sends `output_config.effort` (`bedrock.rs:146-160`). | as listed |
| BestEffort | Each failed parallel stage becomes a `StageFailure{cancelled:false}`. The whole group is an error only if every stage failed. | `workflow/engine.rs:44-52, 196-248` |
| Partial coverage | On Ok, the worker's `review.stage_failures` is stored and the review is still `Reviewed`. | `reviewer.rs:1943-1970` |
| Status values | One `ReviewStatus` enum (Incomplete, Pending, In Review, Cancelled, Skipped, Reviewed, Failed, Failed To Apply) spells the `reviews`, `patches` and `patchsets` status columns. The patchset's final status is binary: Reviewed when no patch failed, else Failed. | `lib.rs:72-137`, `reviewer.rs:1052-1098` |
| UI card | Already ported: "Failed after X total", "on attempt N", and the "did not complete" banner. | `static/index.html:4581-4645` |

### 2.2 Verdict per fork commit

Commits are listed in the order they landed on the fork.

| Fork commit | Verdict | Reason |
|---|---|---|
| dcdcdf5ac937 salvage | **Adapt** | Keep the idea, the stdin message, `interruptible`, `salvage_seconds`, `Partial`, no retry of timeouts, and record-on-failure. Change these: (a) the deadline becomes a typed two-phase `ReviewDeadline` instead of the fork pushing a shared `Instant` forward; (b) `SESSION_WOUND_DOWN` string matching becomes a typed `SessionStopped`, and the worker reports a deadline failure as a typed `review_stopped_by` instead of the daemon inferring it from timing; (c) `"Partial"` becomes `ReviewStatus::Partial`, used by the review row, the patch and the patchset (the fork set it on the patch only, under a `Failed` review and patchset), and the patch gains a `TimedOut` outcome so the restart guard can skip both (D16, D17); (d) logs are promoted through the port's `complete_review(logs=...)`, so the fork's `review_has_logs`/`set_review_logs` are not needed, and promotion is decided by `AttemptEnd`, so an error returned as `Ok` is covered; (e) the interruptible-stage behaviour is reworked to use upstream's final-turn synthesis (D2). |
| 36935e97144e output-token count | **Keep, squashed with 528e6d** | Both rewrite the same warning line. |
| 56de4b78ea37 settle (deferred half) | **Adapt** | Unfinished stages come from `StageTracker` state, not from reading back registry rows. Their cause is typed by how the attempt ended. The rest of the commit is already ported. |
| 10e9bd0c3b3f effort placement | **Keep, and extend to Vertex** | Vertex Claude shares the request struct and has the same bug. |
| d4af7a8388f0 ask again on truncation | **Adapt** | Keep the retry and its feedback text verbatim. Change: truncations are counted per turn instead of sharing the session-wide recitation budget, and a truncation that wrote nothing gets a reasoning-specific feedback once and then fails with the remedy (D7). Add: push the turn-budget prompt only once (otherwise a truncated final turn stacks prompts), log the discarded answer, count its usage, and treat a cached truncated entry as a miss on read. |
| 059f01bc541c truncation remedy | **Keep** (squashed, see above) | |
| df962e0e26d0 late is not failure | **Adapt** | Keep the rule that a deadline alone is not a failure. Instead of "any `stage_failures`", decide salvage from typed stop causes. Otherwise a stage that failed on its own error before the deadline would turn a late but otherwise complete review into a salvage (D3). |

Nothing is dropped outright. The fork's cooperative `cancel` message
(50ae5dbb6f61) is deferred to the cancel step.

---

## 3. Combined Design

### 3.1 Effort placement (Claude and Vertex)

`ClaudeRequest` gains a sibling field:

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub output_config: Option<OutputConfig>,

#[derive(Debug, Serialize, Deserialize)]
pub struct OutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}
```

- `ThinkingConfig` loses `effort`.
- `translate_ai_request` sets `thinking: thinking.map(|t| ThinkingConfig { thinking: Some(t) })` and `output_config: effort.map(|e| OutputConfig { effort: Some(e) })`.
- `VertexClaudeRequest` gains the same `output_config` field, copied from `claude_req`.
- `thinking = "enabled"` is still passed through unchanged, as in the fork. Silently remapping a configured value is worse than the 400 error the API returns for it.
- `cache_identity` already includes both settings, so nothing changes there.

### 3.2 Truncation diagnostics

`claude::translate_ai_response` logs one line:

```
Claude response truncated at max_tokens after {usage.output_tokens} output
tokens ({answer_chars} chars of answer, {tool_arg_chars} chars of tool
arguments).{remedy}
```

`fn truncation_remedy(answer_chars, tool_arg_chars) -> &'static str` returns
" Nothing was written, so the budget went on reasoning: raise max_tokens or
lower effort rather than asking for less." when both counts are 0, and ""
otherwise.

Thinking blocks are not counted. Opus 5 sends them empty while still billing
for them, so counting them would make every truncation look like a long
answer. Other providers keep their existing warnings.

### 3.3 Truncation recovery in `SessionRunner`

**Typed error.** Add `ai::OutputTruncated` (thiserror, `ClassifyAiError ->
Fatal`) and wire it into `classify_ai_error`. Remove the unused
`ReviewError::OutputTruncated` (`worker/prompts.rs:34-36, 48`). The error
carries the same counts as the §3.2 diagnostic, so the recovery can act on
them without parsing text:

```rust
/// A response the provider cut off at its output limit.
#[derive(Debug, thiserror::Error)]
#[error("LLM output was truncated by provider (e.g. hit max tokens) after \
         {answer_chars} chars of answer and {tool_arg_chars} chars of tool arguments.{}",
        truncation_remedy(*.answer_chars, *.tool_arg_chars))]
pub struct OutputTruncated { pub answer_chars: usize, pub tool_arg_chars: usize }

impl OutputTruncated {
    /// Nothing visible was written: the budget went on reasoning.
    pub fn spent_on_reasoning(&self) -> bool {
        self.answer_chars == 0 && self.tool_arg_chars == 0
    }
}
```

`SessionRunner` builds it from the response it already holds: the length of
`content` and the summed length of every tool call's arguments. Thinking
blocks are not counted, for the reason given in §3.2. The message reuses
`truncation_remedy`, so the log line and the error say the same thing. For
that reason `truncation_remedy` lives in `ai/mod.rs` from commit 2 on, not
in `claude.rs`.

**Routing.** A truncated response is a *successful* response, so it never
reaches the provider-error arm by itself. When `resp.truncated` is set
(`session.rs:411`), the runner:

1. Adds the response's usage to the totals, because those tokens were billed.
2. Appends the truncated assistant message to `log_history` only, via
   `push_log`, never to `history`. The conversation log then shows what was
   cut, and the model never sees a half-written tool call.
3. Builds `OutputTruncated` and calls
   `truncation_action(&truncated, &turn_truncations)`.
4. On `RetryWithFeedback(text)`:
   - Records the truncation in `turn_truncations`. Past its bound (below) it
     bails with
     "Session failed after N truncated answers in one turn. Last error: LLM
     output was truncated by provider ...", which includes the remedy when
     the budget went on reasoning.
   - Pushes the feedback through a new `push_feedback(&mut history, &mut log_history, text)` helper, which the existing recitation path also uses.
   - Refunds the turn (`turns = turns.saturating_sub(1)`) and continues.
5. On `Fail`, returns the error.

```rust
/// Truncated answers since the last accepted response.
#[derive(Default)]
pub struct TurnTruncations { pub count: usize, pub reasoning_retried: bool }
```

`turn_truncations` is reset to its default whenever a response is accepted
(not truncated). Recitation keeps its own session-wide
`provider_error_retries`, unchanged.

**Decision.** `fn truncation_action(&OutputTruncated, &TurnTruncations) ->
ErrorAction` decides from the typed counts. Both kinds count toward the same
per-turn bound of `max_provider_error_retries`:

| What the truncated answer contained | Action |
|---|---|
| Some answer text or tool arguments | `RetryWithFeedback(OUTPUT_TRUNCATED_FEEDBACK)` |
| Nothing (`spent_on_reasoning()`) and `!reasoning_retried` | `RetryWithFeedback(OUTPUT_TRUNCATED_REASONING_FEEDBACK)`; sets `reasoning_retried` |
| Nothing, and `reasoning_retried` | `Fail`. Asking for less cannot help: the remedy in the error tells the operator to raise `max_tokens` or lower `effort` |

The runner calls it directly. The fork routed truncation through
`LlmSession::handle_provider_error`, which forced both the trait default
(`session.rs:123-136`) and the `Stage` override (`workflow/stage.rs:366-382`)
to repeat the same branch. Here that is not needed: truncation is detected
by the runner, not raised by the provider; no session handles it
differently; and the per-turn state lives in the runner. Both
`handle_provider_error` implementations stay as they are, recitation only.

`OUTPUT_TRUNCATED_FEEDBACK` is the fork's constant, verbatim
(d4af7a8388f0, `src/ai/session.rs`):

> IMPORTANT: Your previous response hit the provider's output limit and was
> discarded unread. Answer again, shorter. Do not quote long passages of code
> -- cite file and line and describe what is there. Keep reasoning to the
> conclusions you have reached. If the work does not fit in one response, do
> the first part only and continue on the next turn.

`OUTPUT_TRUNCATED_REASONING_FEEDBACK` is new. It addresses the case the fork
text only touches in one sentence:

> IMPORTANT: Your previous response spent the provider's whole output limit
> on reasoning before writing anything, and was discarded unread. Do not
> deliberate further. State the conclusions you have already reached and act
> on them now: call the next tool, or write the answer.

**Budgets.** Truncations are counted per turn, not per session (D7). A long
analysis stage that truncates now and then over a hundred turns keeps going,
because each accepted turn resets the count. The total stays bounded: a turn
costs at most 1 + 3 model calls for truncation, and `max_turns` bounds the
accepted turns. Daemon-spawned workers have no nested retries: upstream
returns early in `decorate_provider` for stdio providers, and
`worker_max_attempts` is 1. So these retries happen in one layer only.

When the bound is reached the stage fails, as today. It does not fall back to
a forced tool-less synthesis turn: that turn writes the stage's full verdict,
the longest answer the stage produces, and the model has just failed several
times to fit a shorter one.

**Interaction with the forced final turn.** Today the final-turn block runs
every time `turns == max_turns`: it sends `tools=None`, and when
`turns > 1` it also pushes "TURN BUDGET EXHAUSTED" (`session.rs:318-319`).
Any retry that gives back a turn re-enters it and pushes the prompt again.
That already happens with validation retries, and truncation would make it
worse. The runner therefore keeps
`final_turn: Option<FinalTurn>`:

```rust
/// Why the runner is forcing a tool-less verdict.
enum FinalTurn { TurnBudget, OutOfTime }
```

`final_turn` becomes `Some(TurnBudget)` when `turns == max_turns`, for every
`max_turns`. From then on every iteration sends `tools=None`. The prompt is
pushed once, at the moment `final_turn` first becomes `Some`, and for
`TurnBudget` only when `max_turns > 1`, which keeps upstream's guard.
Single-turn stages (`max_turns: 1` at `linux_patch_review.rs:615`,
`:670`; `sashiko_patch_review.rs:728`, `:776`, `:1083`) are asked their one
question with no tools and no "TURN BUDGET EXHAUSTED" message, exactly as
today. A truncated final turn gets the truncation feedback and one more
tool-less turn, under the same retry bound. `OutOfTime` is used by §3.6.

**Adjacent upstream defect (D12).** On the final turn, an assistant message
that carries `tool_calls` is pushed to `history` even though the calls are
ignored (`session.rs:429-439`). If validation then fails, the next request has
a `tool_use` without a `tool_result`, which Claude rejects. On the final turn,
the runner drops `tool_calls` from the copy that goes into `history` and keeps
them in `log_history`. An answer that held only tool calls would then have no
content, and Claude rejects an assistant message with an empty content array
while Bedrock fails to build it (`bedrock.rs`, "Failed to build assistant
message"). So when the copy is left with no text, its content becomes
`IGNORED_TOOL_CALLS_NOTE` ("[Tool calls omitted: no tools are run on the final
turn, so none were made.]"). The note goes into `history` only; text the model
did write is kept unchanged.

**Response cache.** `CachingAiProvider::generate_content` does not store a
response with `truncated == true`. It also treats a stored entry with
`truncated == true` as a miss, which covers entries written by older builds.
The cache is daemon-side only (`ai/mod.rs:426-436`). The retry is already a
different request (the feedback is appended), so this matters for later
reruns of the same patch rather than for the retry itself.

### 3.4 The review deadline as a type

`Arc<Mutex<Instant>>` is replaced by a typed clock shared by the I/O loop and
`DeadlineBudget`. It lives in `ai/backoff_provider.rs`, which is compiled
without the `server` feature:

```rust
/// The wall-clock limits of one worker run.
pub struct ReviewDeadline { state: std::sync::Mutex<DeadlineState> }

struct DeadlineState {
    /// When the review is asked to wind down.
    soft: tokio::time::Instant,
    /// Extra time granted after `soft` to finish consolidation.
    salvage: Duration,
}

impl ReviewDeadline {
    pub fn new(timeout: Duration, salvage: Duration) -> Self;
    pub fn soft(&self) -> Instant;
    /// `soft + salvage`; equal to `soft` when salvage is disabled.
    pub fn hard(&self) -> Instant;
    /// Credits a quota-gate wait: moves both limits.
    pub fn credit(&self, d: Duration);
}
```

- `DeadlineBudget::new(Arc<ReviewDeadline>)` keeps its credit de-duplication.
  `check()` fails only after `hard()`, so a request in backoff during the
  salvage window is not killed at `soft()`. Without this, a consolidation
  stage's request could race the wind-down and fail with the timeout error.
- `check()` returns the typed `ReviewTimedOut` (§4).
- Local review (`local_review.rs:1015-1022`) builds
  `ReviewDeadline::new(timeout, Duration::ZERO)`, so its behaviour does not
  change.
- Rate-limit credit applies during the salvage window too. Time spent parked
  at the quota gate is not active time in either phase.

### 3.5 Wind-down: wire message and worker signal

**Wire (daemon to worker).** One new NDJSON envelope:

```json
{"type":"wind_down","tx_id":0,"payload":null}
```

- `tx_id` 0 is never issued; `IpcRegistry` starts at 1 (`ai/mod.rs:774`).
- `IpcEnvelope.msg_type` becomes a typed enum:
  ```rust
  #[derive(Deserialize, Serialize)] #[serde(rename_all = "snake_case")]
  pub(crate) enum IpcMessageType { AiResponse, Error, WindDown }
  ```
  An unknown type now fails to parse and takes the existing "malformed"
  `exit(1)` path, so the rule is unchanged: fail fast on protocol violations.
- Daemon and worker are the same binary (`default_worker_command` uses
  `current_exe`), so there is no version skew in normal operation.

**Worker signal.** The worker build has no `tokio-util`, because it is
server-only (`Cargo.toml:62`). The signal is therefore a small type:

```rust
/// Set once, read at turn boundaries.
#[derive(Default)]
pub struct WindDownSignal(std::sync::atomic::AtomicBool);
```

- A process-wide instance (`OnceLock<Arc<WindDownSignal>>`) is set by the
  stdin reader on `WindDown`.
- `Worker::run` passes it into `WorkflowEnv` as a new field
  `wind_down: Option<Arc<WindDownSignal>>`. Tests inject their own, and the
  other 16 construction sites pass `None`.
- The signal is never cleared. A worker reviews exactly one patch per spawn
  (`review_index`).

### 3.6 Interruptible stages and the out-of-time turn

**`StagePolicy.interruptible: bool`** defaults to `true`.
`Stage::execute_isolated` passes it to
`SessionRunner::with_wind_down(env.wind_down.clone(), self.policy.interruptible)`.

**Runner behaviour**, checked at the top of each loop iteration, before
`turns += 1`:

| Situation | Action |
|---|---|
| Not interruptible, or no signal | Unchanged. |
| Interruptible, signal set, no model call made yet | Bail with `SessionStopped { cause: StopCause::OutOfTime }`. Nothing is spent. |
| Interruptible, signal set, at least one model call made | Set `final_turn = Some(FinalTurn::OutOfTime)`. Push once: "TIME BUDGET EXHAUSTED: The review has run out of time. Do NOT call any tools. Synthesize your final JSON verdict now from the evidence gathered so far." Send `tools=None`. |
| In `OutOfTime` and the session then fails (validation attempts used up, provider error) | Wrap the error as `SessionStopped { cause: StopCause::OutOfTime }` with the original error as context, so the stop is still attributed to the deadline. |
| In `OutOfTime` and validation succeeds | Return `SessionResult { cut_short: true, .. }`. |

This adapts the fork, which always bailed (D2). A stage that has spent hours
gathering evidence turns it into concerns in one more tool-less call. That
call reuses upstream's final-turn machinery and validation, including the
required top-level keys from f0768d7fb3cf. Stages that had not started stop
at no cost.

The check runs at turn boundaries only. A model call or tool batch already in
progress finishes first. One turn takes minutes at most, which fits inside
the salvage window.

**Which stages may be interrupted:**

| Workflow | Interruptible (default) | `interruptible: false` |
|---|---|---|
| `linux_patch_review` (`workflows/linux_patch_review.rs:1295-1327`) | prescreen, planning, every analysis stage in the BestEffort fan-out | deduplication, conflict_resolution, verification, report |
| `sashiko_patch_review` (`workflows/sashiko_patch_review.rs:1108-1122`) | prescreen, planning, every analysis stage | deduplication, conflict_resolution, verification, report, summary |

Prescreen and planning are sequential. If either is stopped, the error goes up
through `?` (`engine.rs:75, 92`) and the run ends with nothing to salvage,
which is correct because no concerns exist yet. A test iterates both
workflows' stage policies by name, so a new consolidation stage cannot be left
interruptible by accident.

**Live progress.**

- `WorkflowEvent::StageFinished` gains `cut_short: bool`. It travels the
  whole progress chain: `WorkerProgressEvent::StageFinished`
  (`worker/prompts.rs:114`, mapped at `:512-514`), then
  `local_review::ProgressEvent::AiReviewStageFinished` (`local_review.rs:187`,
  mapped at `:1320`), then `ProgressWire::StageFinished { cut_short }` (serde
  default `false`, mapped at `:718`), then `Phase::StageDone { cut_short }`.
  The local CLI printer (`main.rs:2256`) prints "cut short" for it. The live
  row and the stored record then describe the same event the same way.
- `StageFailed` carries a typed `cause` instead of `cancelled` (§3.7). The
  comment at `stage.rs:519-526` ("always false for now") goes.

### 3.7 Why a stage stopped: `StageStopCause`

Every `cancelled: bool` that describes a stage stop is replaced with:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStopCause {
    /// The stage failed on its own: provider, validation or tool error.
    Error,
    /// The review was cancelled while the stage ran.
    Cancelled,
    /// Stopped by the wind-down before it produced output.
    OutOfTime,
    /// Wound down after gathering evidence; its output was kept but covers
    /// less than a full run.
    CutShort,
    /// Still running when the worker stopped; reported by the daemon, not the
    /// worker.
    Interrupted,
}
impl StageStopCause { pub fn is_deadline(self) -> bool { matches!(self, Self::OutOfTime | Self::CutShort) } }
```

It replaces `cancelled` on six types along the progress chain, plus the
stored JSON:

| Type | Site |
|---|---|
| `engine::StageFailure` | `workflow/engine.rs:44-52`, set at `:220-224` |
| `WorkflowEvent::StageFailed` | `workflow/events.rs:65`, emitted at `stage.rs:440, 531, 927, 987` |
| `WorkerProgressEvent::StageFailed` | `worker/prompts.rs:146-150`, mapped at `:556-566`; the worker output's `"cancelled"` at `:615` |
| `local_review::ProgressEvent::AiReviewStageFailed` | `local_review.rs:194-198`, mapped at `:721-731` and `:1326-1335`; printed at `main.rs:2267` |
| `ProgressWire::StageFailed` | the `stage_failed` wire kind |
| `Phase::StageFailed` | `activity.rs:261`; its description text at `:429-433` (`if *cancelled { "cancelled" } else { "failed" }`) becomes the cause label of §5 |
| stored `reviews.stage_failures` | `{stage, reason, cause}` |

The description text matters on its own: the CLI and the public activity
endpoints show it, so it must use the same labels as the UI.

In the engine's BestEffort arm (`engine.rs:196-248`):

- An `Err` whose chain downcasts to `SessionStopped` records that error's
  cause. Any other `Err` records `Error`.
- An `Ok` with `cut_short` records a `StageFailure { cause: CutShort, reason: "cut short: the review ran out of time; its findings so far were kept" }` and still applies the stage's mutation.
- The rule that a group in which every stage failed is an error stays.
  `CutShort` counts as a success, so it only applies when no analysis stage
  produced anything. In that case there is nothing to salvage anyway.
- That group error becomes typed. Today it is the first failure with a text
  context (`engine.rs:226-231`). It becomes the first failure with a typed
  context, `err.context(ParallelGroupFailed { failures })`, whose `Display` is
  today's text. `ParallelGroupFailed::stopped_by_deadline()` is true when any
  of its failures has a deadline cause. The worker reads it through
  `downcast_ref` (§3.8), so it no longer matters which stage failed first.

### 3.8 Daemon: how an attempt ended

**I/O loop** (`reviewer.rs:2826-2871`). The timeout arm waits on
`deadline.soft()` until the wind-down is sent, and on `deadline.hard()`
after that. It is extracted into a helper so the loop stays readable.

```rust
enum DeadlinePhase { Running, WindingDown { since: Instant } }
```

1. When `soft` expires in `Running` and `salvage_seconds > 0`: write the
   `wind_down` envelope through `stdin_writer`, log "Review {id} reached its
   deadline after {N}s; asking the worker to wind down within {M}s", set
   `WindingDown`, and keep reading.
   - A failed write is not an error. It happens when the worker printed its
     result and exited just before `soft` (EPIPE). The failure is logged at
     warn, as the reply writes are (`:3027-3030`), the loop still moves to
     `WindingDown`, and it keeps reading stdout. A result already in the pipe
     is then read and classified like any other. The write does not use `?`,
     unlike the initial payload write (`:2808-2810`), because propagating the
     error would turn a finished review into a failed, retried attempt.
2. When `hard` expires in `WindingDown`, or `soft` expires with
   `salvage_seconds == 0`: return
   `ReviewTimedOut { active_seconds, salvage_seconds }`. The kill path logs
   "timed out after N active seconds and did not salvage its findings within
   a further Ms".
3. The 2 s cancel poll keeps running in both phases. Cancelling, including the
   automatic cancel of a superseded revision, still kills the worker at once.

`run_review_tool[_with_cmd]` returns `Result<ReviewToolOutcome>`, where
`ReviewToolOutcome { output: Value, wound_down: bool }`. The string match at
`:3220-3225` becomes `e.downcast_ref::<ReviewTimedOut>()`.

**How a stopped review reaches the daemon.** There are three shapes, and only
the first is an `Err` of the I/O loop:

- The hard limit passed: `Err(ReviewTimedOut)`.
- The worker's review of the patch failed: an `Ok` output with an `"error"`
  field, no review for the patch and `history: []`
  (`local_review.rs:1724-1731, 1876-1881`). The engine's stage failures and
  the stage histories are not in it, because they are built only from
  successful patch results (`:1749-1855`).
- The worker failed before reviewing: `{"patchset_id":0,"error":...}`
  (`main.rs:411-416`), also `Ok`.

The worker's error text is `err.to_string()`, which keeps only the outermost
context (`local_review.rs:1727`), so the daemon cannot learn from it why the
review failed. The worker therefore adds one typed field to the failed
patch's entry in `patches[]`, next to `review_status: "incomplete"` and
`review_error` (`mark_patch_review_incomplete`, `local_review.rs:1436-1445`):

```json
{"index": 3, "review_status": "incomplete", "review_error": "...",
 "review_stopped_by": "out_of_time"}
```

`review_stopped_by` is a serialised `StopCause`. The worker sets it only when
the patch's error, read with `downcast_ref` (which walks anyhow's context
chain), is a `SessionStopped { cause: OutOfTime }`, or carries a
`ParallelGroupFailed` context whose `stopped_by_deadline()` is true (§3.7).
Any other error leaves it absent. The daemon deserialises it into
`Option<StopCause>` and never reads `review_error` to decide anything.

**Classification** in `process_patch_review`, done once, right after the
existing post-run cancel check:

```rust
enum AttemptEnd {
    /// The worker returned a review for the patch and no stage was stopped
    /// by the deadline. `late` is true if the wind-down was sent (logged at
    /// info, recorded as Reviewed).
    Finished { output: Value, late: bool },
    /// The worker returned a review for the patch after a wind-down, and at
    /// least one stage was stopped by the deadline.
    Salvaged { output: Value, stopped: Vec<StageFailureRecord> },
    /// The deadline ended the review with no review to store: the hard
    /// limit passed, or the worker marked the patch
    /// `review_stopped_by: out_of_time`. `output` is the worker's error
    /// output in the second case, kept for its token counts.
    TimedOut { timeout: ReviewTimedOut, output: Option<Value> },
    /// The daemon's token budget stopped the worker
    /// (`ReviewError::BudgetExceeded`, `reviewer.rs:2971, 2993`).
    BudgetExceeded(anyhow::Error),
    Cancelled,
    /// Any other end with no review for the patch: another I/O loop error,
    /// an error output without `review_stopped_by`, or a missing or null
    /// review. `wound_down` decides the retry (D5).
    Failed { reason: String, wound_down: bool, output: Option<Value> },
}
```

`fn classify(result, wound_down) -> AttemptEnd` uses
`fn incomplete_stages(&Value) -> Vec<StageFailureRecord>` (reads
`review.stage_failures`, then the top level) and
`fn is_salvage(wound_down, &[StageFailureRecord]) = wound_down && any(cause.is_deadline())`.

This replaces the fork's "wound down and `stage_failures` not empty" rule.
A stage that failed on its own before the deadline is a coverage gap, and it
stays one exactly as upstream records it today (`Reviewed` with
`stage_failures`). It is not a salvage (D3).

Timing alone never makes a failure a timeout. An error output after a
wind-down is `TimedOut` only when it carries `review_stopped_by:
out_of_time`, for example when every analysis stage was stopped before its
first call, or when planning was stopped. A non-interruptible stage that
fails validation on its own during the salvage window is `Failed` with its
real reason.

**What each outcome records:**

| AttemptEnd | Review row | Patch (`PatchOutcome`) | PatchResult | Retry | Notifications |
|---|---|---|---|---|---|
| Finished, `late = false` | Reviewed (unchanged path) | Reviewed | Success | -- | as today |
| Finished, `late = true` | Reviewed | Reviewed | Success | -- | as today |
| Salvaged | **Partial**, result "Review ran out of time; findings salvaged from the stages that finished". Keeps summary, interaction, inline_review and the worker's history as logs. Findings are stored by the same helper as Reviewed (D15 for where they are counted). Concerns are not handed to `linux_bug` (D14). | **Partial** | `Partial` (the patchset rule of §3.11) | no | none for this review (no email, no forge comment, D6) |
| TimedOut | Failed, "Tool error: Review tool timed out (active time exceeded)", plus the worker's error text when there is one. Daemon record (§3.9). | **TimedOut** (D16) | ReviewFailed | **no** (D5) | none |
| BudgetExceeded | Failed, "Tool error: ..." as today. Daemon record. | Failed | ReviewFailed | **no** (D13) | none |
| Cancelled | Cancelled (unchanged). Daemon record. | Cancelled | Cancelled | -- | none |
| Failed, not wound down | Failed (unchanged text). Daemon record. | Failed | ReviewFailed | yes, up to `max_retries` | none |
| Failed, wound down | Failed with its real reason. Daemon record. | Failed | ReviewFailed | **no** (D5) | none |

"Daemon record" means the attempt has no review from the worker for the
patch, so the daemon writes the stage failures and the conversation itself
(§3.9). This is decided by the `AttemptEnd` variant, not by whether the I/O
loop returned `Err`.

- Timeouts are not retried (D5). A deadline that has already proved too small
  says nothing good about an identical second attempt, and retrying is how
  one patch came to spend twelve hours.
- A salvaged review row is `Partial`, not `Failed`. It is not a failure: it
  produced verified findings. It is not `Reviewed` either: some analysis
  stages were stopped. The value deliberately avoids `ReviewStatus::Incomplete`,
  which already means "not yet fully received".
- Every gate that asks "was this reviewed?" compares with `'Reviewed'`
  exactly, so a `Partial` row passes none of them, which is the intent. The
  audit found four such gates, and none changes:
  - `count_successful_reviews` (`db.rs:2134`): a Partial review does not
    count toward `target_review_count`, so it never stands in for a full one.
  - `CLEAN_PATCHSET_PREDICATE` (`db.rs:281-311`): a patch whose only review
    is Partial has no `Reviewed` review, so its patchset is never "clean" and
    never released early from an embargo.
  - `get_completed_reviews_for_release` (`db.rs:8709-8721`): embargo-release
    emails and the forge PR comment are built from `Reviewed` rows only, so
    the salvaged review never leaves the server (D6).
  - `get_patchset_review_outcome` (`db.rs:8614`): any patchset status other
    than Reviewed gives `PatchsetReviewOutcome::Incomplete`, which is the
    right answer for Partial. It has no production caller today; only the
    embargo tests call it (`db.rs:11717`, `11757`, `11833`).

  Each of these gets a one-line comment saying `Partial` is excluded on
  purpose, so a later edit does not "fix" it. The one `'Reviewed'` filter
  this step does widen is the patchset list's severity counts, by decision
  (D15).

  A fifth gate asks the opposite question, "did this fail?":
  `has_failed_review` (`db.rs:2155-2169`) compares with `'Failed'` (and
  `'FailedToApply'`), matching rows with no interaction only. A `Partial`
  row does not match it either, but it does not stay as it is: it is
  replaced by `prior_final_outcome` (below), which is where the Partial skip
  comes from.
- The opposite kind of gate lists the *finished* statuses, and every one of
  those must gain `Partial`, or a finished Partial patchset reads as open or
  as still running. The full list is in §3.11. The one that would misbehave
  silently is `is_closed_to_new_parts` (`db.rs:5912-5920`, whose `_ => false`
  arm would reopen a finished Partial series to late parts and to a new
  embargo), with its SQL twin `CLOSED_TO_NEW_PARTS_SQL` (`db.rs:60-62`). Two
  would hang: `sashiko-cli show --watch` (`bin/sashiko-cli.rs:1536-1539`) and
  `scripts/verify_live_log.sh:216`. The resubmittable statuses and the
  embargo release are decisions (D18, D19).
- The patchset's final status is no longer binary. A Partial patch makes the
  patchset Partial unless another patch failed (§3.11, D17). The forge
  comment and the immediate embargo release stay gated on a final status of
  `Reviewed` (`reviewer.rs:1127-1146`).
- To keep `process_patch_review` from growing, three pieces are extracted
  into helpers: storing findings (`store_review_findings`), the daemon record
  (`record_unfinished_attempt`, §3.9), and finishing a review row
  (`finish_review_row`).

**Processing the patchset again.** A daemon restart puts `Reviewing`
patchsets back to Pending (`db.rs:9383-9396`), and `process_patch_review`
then reviews every patch that neither `count_successful_reviews` (only
`'Reviewed'`, `db.rs:2134`) nor `has_failed_review` (only `Failed` rows with
`interaction_id IS NULL`, `db.rs:2155-2169`) stops. A Partial row is not
`'Reviewed'` and has an interaction, and so does the Failed row of a timeout
that came from a worker error output, so both patches would be reviewed again
from scratch, which D5 forbids. The guard therefore becomes
`prior_final_outcome(patchset_id, patch_id) -> Option<PatchOutcome>`:

- `Some(Partial)` or `Some(TimedOut)` when `patches.status` is `'Partial'`
  or `'TimedOut'`. Only the attempt outcomes above write those values, and
  nothing else overwrites them before a new attempt runs;
- `Some(Failed)` for today's rule (a `Failed`/`FailedToApply` row with no
  interaction);
- `None` otherwise.

The guard reads `patches.status`, not `reviews.status = 'Partial'`, although
the review row now carries `Partial` too. The review row is history: it keeps
its findings, logs and interaction after a rerun, as `Reviewed` rows do. The
patch status is the current verdict, which an explicit rerun or resubmission
clears (§3.11). A guard keyed on the review row would skip the patch for
ever after a rerun unless the rerun deleted the salvaged review and its
findings. It also could not see `TimedOut`, whose review row is an ordinary
`Failed`.

The skip path (`reviewer.rs:1720-1731`) keeps the stored status instead of
writing `"Failed"` over it, and returns `PatchResult::Partial` for `Partial`
and `ReviewFailed` otherwise (D16). A patchset processed again after a
restart therefore ends with the same status it would have had (§3.11).

`has_failed_review` also matches the literal `'FailedToApply'`, but no
production code writes a failed-to-apply status to a review row, in either
spelling. The only writer of `ReviewStatus::FailedToApply` (whose string is
`'Failed To Apply'`) sets the patchset's status (`reviewer.rs:1162`), not a
review's. That half of the `IN` list therefore matches only rows the tests
write with `update_review_status(.., "FailedToApply", ..)` (`db.rs:12687`,
`12964`). `prior_final_outcome` keeps today's rule byte for byte, so commit
13 changes no behaviour. Whether the dead literal should go is left to a
separate cleanup.

### 3.9 Recording an attempt however it ended

**Stage timings.** These are written for every attempt, not only Ok ones
(D8). The comment at `reviewer.rs:3182-3184` is replaced. Each attempt has
its own review row, so the timings never mix attempts, and on a failed row
they answer "which stage used up the time".

They list every stage that ran, not only the ones that finished. A stage
that did not run to completion keeps its elapsed seconds and its turns up to
the moment it stopped, and is marked with what stopped it:

```rust
pub struct StageTiming {
    pub stage: String, pub seconds: u64, pub turns: u64,
    /// `None` for a stage that ran to completion; left out of the JSON then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<StageStopCause>,
}
```

Without this the one stage the timings left out was the one that used up
the time: an end-to-end run killed deduplication at the hard limit after
about 18 s, and that time survived only inside its failure reason's text.
The clock of a stage stops where the stage stops: at the worker's
`StageFailed` for a failure it reports, and in `settle` for a stage the
worker left running, which measures once so the timing and the reason give
the same time. Because a running stage has no final time until then, the
daemon writes the timings in `settle_attempt`, after `settle`, rather than
when the I/O loop returns. Each stage is one timing whatever it went
through: a stage that wound down and finished is listed once, with
`stopped_by: cut_short`, never as a finished timing and a stopped one. A
stage that stopped without ever reporting a start has no timing, as a
finish with no start never had. Every timing with `stopped_by` has a
matching entry in `stopped()` with the same cause.

**`StageTracker`** (`activity.rs:1419-1490`) gains typed per-stage state, so
the daemon knows which stages are still running without reading registry
rows back:

```rust
/// How far a stage got.
struct StageRun { seconds: u64, turns: u64 }

enum TrackedStage {
    Running { key: ActivityKey, started: Instant, turns: u64 },
    /// `cut_short` comes from `StageFinished { cut_short }` (§3.6).
    Done { run: StageRun, cut_short: bool },
    /// `run` is `None` when the stage never reported a start.
    Failed { reason: String, cause: StageStopCause, run: Option<StageRun> },
}
```

- A new `ProgressUpdate::Failed { key, stage, reason, cause }` replaces
  routing `StageFailed` through `ProgressUpdate::Set`.
- `fn settle(&mut self, cause: StageStopCause) -> Vec<StageFailureRecord>`
  switches every `Running` stage's live row to
  `Phase::StageFailed { cause, reason }`, keeps the time it ran, and returns
  the records. The reason is "still running when the review stopped, after
  {format_elapsed} and {N} turn(s)". `Done` and `Failed` rows are left alone.
- `fn timings(&self) -> Vec<StageTiming>` returns every `Done` stage and
  every `Failed` stage with a `run`, each with its `stopped_by` as above.
- `fn stopped(&self) -> Vec<StageFailureRecord>` returns every stage that
  did not run to completion: each `Failed` stage with its own cause, and
  each `Done { cut_short: true }` stage with cause `CutShort` and the reason
  from §3.7. Without the second part, a stage that wound down and was then
  killed at `hard()` would be recorded as an ordinary finished stage, and the
  UI's "cut short after Xs" row (§5) could never appear on a timed-out row.

After every worker run, `settle` runs with a cause taken from the
`AttemptEnd`: `TimedOut` gives `OutOfTime`, `Cancelled` gives `Cancelled`,
and every other variant gives `Interrupted`. This always runs, because it is
also what clears stale live rows.

**`record_unfinished_attempt`** runs for every "daemon record" outcome in the
§3.8 table (`TimedOut`, `BudgetExceeded`, `Cancelled`, `Failed`), whichever
of the three shapes the attempt ended in:

- `reviews.stage_failures` is written as `tracker.stopped() ++ settled`,
  sorted by `workflows::stage_order(project, ..)`. The tracker saw every
  `StageFailed` the worker sent (each stage reports its own failure,
  `stage.rs:440, 531`), so this is complete even when the worker's output
  carries no list.
- The live log is promoted (below) and becomes the row's `logs`. Today the
  Ok-with-error path stores the worker's empty `history` as `"[]"`
  (`reviewer.rs:1972-1978`) and `complete_review` then deletes the live log
  (`db.rs:2216-2221`), so the row keeps nothing. That stops.

For `Finished` and `Salvaged`, the worker's `stage_failures` and `history`
stay the authority, as today (`:1943-1970`). `settle`'s records are dropped
there; in practice nothing is still running when the worker returns a review.

**Live log promotion.** `promoted_live_log(db, review_id)`:

- reads `get_review_log_entries(review_id, None)`;
- prepends a `system` message: "[The review did not finish. This is the
  conversation streamed while it ran{, capped at 2000 entries}.]";
- returns the JSON array, which goes into `complete_review`'s `logs`.

`complete_review` deletes the live rows in the same call, as it does today
(`db.rs:2216-2221`). Because the logs are read before that call, the existing
comment there already describes this case. No new DB functions are needed.

The live log has the same shape as the finished log (`live_log_message`,
`worker/prompts.rs:178-203`), so the page draws both with one renderer.

### 3.10 How this fits with the rest of the system

- **Cancellation and superseded revisions.** The mechanism is unchanged. A
  cancel during the salvage window kills the worker and records `Cancelled`.
  The newer revision gets its own full review.
- **Token budgets** (`reviewer.rs:2948-2996`). Still checked during the
  salvage window. Each attempt still starts with fresh counters
  (`:2817-2818`); what changes is that an attempt stopped by the budget is no
  longer retried (D13), so a patch spends at most one budget.
- **Activity registry.** `settle` replaces the stale-row problem fixed in
  56de4b78. The patchset guard still clears rows at patchset end.
- **Live log.** Streaming is unchanged. Entries are cleared at attempt start
  (`:2785-2800`) and swept at startup for reviews that are not running
  (`db.rs:2351`). Promotion happens before the sweep can drop them.
- **Response cache.** See §3.3. Salvage adds nothing new to the cache: the
  out-of-time prompt makes the synthesis call a new request.
- **Local `sashiko review`.** Truncation recovery applies, because it lives in
  `SessionRunner`. Wind-down never fires.
- **`linux_bug` workflow.** Runs in the daemon with no worker and no signal,
  so it is unaffected apart from truncation recovery. The concern hand-off
  from reviews into it is D14.
- **Rerun and resubmission.** `rerun_patchset` and the resubmission reset in
  `create_fetching_patchset` already exist on HEAD, so this step makes them
  clear the `Partial` and `TimedOut` patch statuses that the §3.8 guard
  reads (§3.11). Without that, a rerun of a Partial patchset would skip its
  Partial patch and end Partial again having reviewed nothing. Rerun's
  cleanup still deletes only `Failed` reviews with no interaction
  (`db.rs:9168-9193`), so a Partial review row survives a rerun next to the
  new review, as `Reviewed` rows do. Port step 7 (`rerun --resume`) builds on
  this and must keep both rules.

### 3.11 `Partial` at the review, patch and patchset levels

**The value.** `ReviewStatus` (`lib.rs:72-137`) gains one variant, placed
between `Reviewed` and `Failed`, with `Display`, `FromStr` and `as_str` all
giving `"Partial"`:

```rust
/// The review ran out of time. Its findings come from the stages that
/// finished, so its coverage is incomplete. Not a failure, not `Reviewed`.
Partial,
```

The three status columns are `TEXT` with no `CHECK` constraint, so no
migration is needed. `TimedOut` stays a patch-only value of `PatchOutcome`
(§4): its review row is an ordinary `Failed`, and it is never a patchset
status.

| Level | `Partial` means | Written by |
|---|---|---|
| Review row | This attempt was salvaged: at least one stage was stopped by the deadline, and its findings come from the stages that finished. | the `Salvaged` arm (§3.8) |
| Patch | The patch's current verdict is a salvaged review. | the `Salvaged` arm; kept by the restart guard; cleared by rerun and resubmission |
| Patchset | Every patch that was not skipped has a review, and at least one of them is only partial (D17). | the patchset's final status rule, below |

**The patchset rule (D17).** Each spawned per-patch task returns a tally
instead of a bare failure count (`reviewer.rs:983-995, 1063-1072`), and the
main loop keeps one too (`:1052-1058`):

```rust
/// How the patches of one patchset run ended.
#[derive(Default)]
struct PatchsetTally { failed: usize, partial: usize }

impl PatchsetTally {
    /// The patchset's status, unless it was cancelled meanwhile.
    fn final_status(&self) -> ReviewStatus {
        if self.failed > 0 { ReviewStatus::Failed }
        else if self.partial > 0 { ReviewStatus::Partial }
        else { ReviewStatus::Reviewed }
    }
}
```

- `PatchResult::Success` adds nothing, `PatchResult::Partial` adds to
  `partial`, `PatchResult::Cancelled` clears the queue as today, and anything
  else (`ReviewFailed`, which covers `TimedOut`, `BudgetExceeded` and a
  guard-skipped prior failure, or an `Err`) adds to `failed`. A panicked task
  adds to `failed`, as today.
- `review_success: bool` goes. The final status is
  `tally.final_status()`, still written through `update_patchset_status`,
  which never overwrites `Cancelled` (`db.rs:8795-8812`). The precedence is
  Cancelled > Failed > Partial > Reviewed.
- Skipped patches return `Success` and are neutral. The patchset-level exits
  that happen before any patch is reviewed keep their status: `Failed` when
  the diffs cannot be fetched (`:635`), `Failed To Apply` when no baseline
  applies (`:1162`).
- The forge comment and the immediate embargo release (`:1127-1146`) run
  only when the final status is `Reviewed`.

| Patches | Patchset |
|---|---|
| All Reviewed or Skipped | Reviewed |
| Four Reviewed, one Partial | **Partial** |
| Four Reviewed, one TimedOut | Failed |
| Three Reviewed, one Partial, one Failed | Failed |
| Processed again after a restart, one patch already Partial | Partial (the guard returns `PatchResult::Partial`) |

**Embargo (D18).** Release is gated on `p.status = 'Reviewed'`
(`db.rs:8539`, `8688`). A non-embargoed Partial patchset has already emailed
each of its `Reviewed` patches as it finished (`reviewer.rs:2252-2300`); an
embargoed one would hold those emails for ever. Both queries accept
`p.status IN ('Reviewed', 'Partial')`:

- `CLEAN_PATCHSET_PREDICATE` is false for a patchset with a Partial patch,
  so a Partial patchset is released at `embargo_until` only, never early.
- The release emails `get_completed_reviews_for_release` rows, which are
  `Reviewed` only. The salvaged review stays unmailed (D6). No forge outbox
  row exists to release, because the forge comment is gated on `Reviewed`.
- While the embargo holds, the displays that show `Reviewed` as `Embargoed` do
  the same for `Partial`, so the status does not say early how the review
  went. Two of the five masks are on the patch status (`db.rs:7542-7544` in
  `get_patchset_details`, `7807-7809` in `get_patchset_summary`) and three on
  the patchset status (`7248-7250` in `get_patchsets`, and `7631-7633` and
  `7893-7895` in the same two functions). All five change in commit 15, before
  commit 18 first writes a `Partial` patch status, so an embargoed Partial
  patch is never shown unmasked. (That would not be a new leak, because a
  `Failed` patch is shown unmasked on HEAD, but there is no reason to open the
  gap.)

**Rerun and resubmission (D19).**

- `rerun_patchset` (`db.rs:9130-9196`) adds
  `UPDATE patches SET status = NULL WHERE patchset_id = ? AND status IN ('Partial', 'TimedOut')`,
  which returns those patches to "not yet reviewed" (`NULL` is what
  `count_pending_patches` reads as pending). `should_increment` stays
  `status == 'Reviewed'`: incrementing for a Partial patchset would raise
  `target_review_count` and re-review every patch that already has a full
  review, while the rerun should redo only the patches that lacked one.
- `has_patchset_by_msgid` (`db.rs:9211`, `9222`) and the reset branch of
  `create_fetching_patchset` (`db.rs:9288-9292`) list the statuses that may be
  resubmitted. Both gain `Partial`, which keeps today's behaviour: a review
  cut short by its deadline ends `Failed` on HEAD and can be resubmitted. The
  reset branch also clears the `Partial` and `TimedOut` patch statuses, as
  rerun does, so a resubmitted `Failed` patchset with a `TimedOut` patch is
  not skipped by the guard either. The two lists are joined into one SQL
  constant with a Rust twin and an agreement test, in the style of
  `CLOSED_TO_NEW_PARTS_SQL`, so they cannot drift (commit 14). `Fetching`
  stays outside the shared list: the reset branch also resets a `Fetching`
  patchset, through its own term, while `has_patchset_by_msgid` counts a
  `Fetching` patchset as already ingested.

**Statistics (D20).** `get_review_stats` counts failures with
`status LIKE '%failed%' OR LIKE '%error%'` (`db.rs:5099-5110`), so Partial
reviews stop counting as failures, which is the point of the change. The
stats page computes the success rate as `Reviewed / finished`
(`index.html:6205-6212`), so without more, Partial would sit in the
denominator and in no card. `get_review_stats` adds `total_partial`
(`status = 'Partial'`) and the page shows it as its own card. The success
rate stays `Reviewed / finished`. The timeline and per-model breakdowns group
by status and gain a `Partial` series by themselves; their chart colours
need `--status-partial` (§5).

**Every status site the audit found that changes:**

| Site | Change |
|---|---|
| `lib.rs:72-137` `ReviewStatus` | Add `Partial` with `Display`, `FromStr`, `as_str`. |
| `db.rs:60-62` `CLOSED_TO_NEW_PARTS_SQL` | `status IN ('Reviewed', 'Partial', 'Failed', 'Failed To Apply')`. |
| `db.rs:5912-5920` `is_closed_to_new_parts` | Add `Partial` to the `received >= total` arm, and list every variant instead of `_`, so the next new variant is a compile error. |
| `db.rs:19257-19267` agreement test | Add `"Partial"` to the statuses. |
| `db.rs:2155-2169` `has_failed_review` | Becomes `prior_final_outcome` (§3.8), reading `patches.status` for `Partial`/`TimedOut`. |
| `db.rs:7190-7196` patchset-list severity counts | `r.status IN ('Reviewed', 'Partial')` (D15). |
| `db.rs:8539`, `8688` embargo release | `p.status IN ('Reviewed', 'Partial')` (D18). |
| `db.rs:7248`, `7542`, `7631`, `7807`, `7893` embargo masks | Mask `Partial` as `Embargoed` like `Reviewed` (D18), in commit 15. |
| `db.rs:9130-9196` `rerun_patchset` | Clear `Partial`/`TimedOut` patch statuses; no increment for Partial (D19). |
| `db.rs:9211`, `9222` `has_patchset_by_msgid` | Add `'Partial'` to `NOT IN`, through the shared constant (D19). |
| `db.rs:9288-9292` `create_fetching_patchset` | Accept `Partial` for reset; clear `Partial`/`TimedOut` patch statuses (D19). |
| `db.rs:5099-5110` `get_review_stats` | Add `total_partial` (D20). |
| `db.rs:2122-2153`, `281-311`, `8709-8721`, `8598-8683` | Comment only: `Partial` is excluded on purpose (§3.8). |
| `reviewer.rs:61-65` `PatchResult` | Add `Partial`. |
| `reviewer.rs:852-1075` per-patch tallies | `PatchsetTally { failed, partial }` from workers and the main loop. |
| `reviewer.rs:1092-1098` final status | `tally.final_status()` (D17). |
| `reviewer.rs:1127-1146` forge comment, immediate release | Gate on final status `Reviewed`. |
| `reviewer.rs:1702-1731` restart guard | `prior_final_outcome`; keep stored status; `Partial` gives `PatchResult::Partial`. |
| `reviewer.rs` salvage arm (§3.8) | `complete_review(ReviewStatus::Partial, ..)` and `update_patch_status(PatchOutcome::Partial)`. |
| `server.rs:982` `UNFINISHED_PATCHSET_STATUSES` | Doc comment names `Partial` as finished; the list is unchanged. |
| `server.rs:2907-2924` activity test | Add `"Partial"` to the finished statuses. |
| `bin/sashiko-cli.rs:1536-1539` `show --watch` | Add `Partial` to the terminal set (it polls for ever otherwise). |
| `bin/sashiko-cli.rs:1572` | Fetch the review log for `Partial` too. |
| `bin/sashiko-cli.rs:1008-1015`, `1124-1137` status colours | `Partial` and `TimedOut` yellow (the patch fallback prints green today). |
| `bin/sashiko-cli.rs:1101-1107` `review_result_label` | Look at the status first: a `Partial` review is labelled "Partial", or "Partial, Issues Found" when it has new issues. Today the label returns "Issues Found" for any review with new issues before it reads the status, so a Partial review with findings would read as a complete one. The label is shared by `show` (`:1891`) and the compare output (`:2048-2094`), which change with it. |
| `bin/sashiko-cli.rs:1139-1152` review badge | `Partial` yellow, with or without issues, labelled by `review_result_label`. |
| `bin/sashiko-cli.rs:1086-1098` `find_best_review_for_patch_refs` | Prefer Reviewed, then Partial, then the latest of any status. |
| `bin/sashiko-cli.rs:1770-1774` `show_summary` best review | Its own choice (the latest `Reviewed`, else `matching.last()`) is replaced by a call to `find_best_review_for_patch_refs(p_id, reviews)`, so the summary picks the same Reviewed > Partial > latest review as every other view. |
| `bin/sashiko-cli.rs:1176-1196` attempt note | `Partial`: "stopped after X (out of time)", no "succeeded". |
| `bin/sashiko-cli.rs:1769-1795` `show_summary` counters | A `partial` counter in text and JSON (counted as "in review" today). |
| `scripts/verify_live_log.sh:216` | Add `Partial` to the terminal `case` pattern (the loop spins to its timeout otherwise). Line 192 only reads the status. |
| `static/index.html` eight theme blocks, `:717-747`, `:848-863` | `--status-partial`, `.status-Partial`, `.status-dot.status-Partial` (§5). |
| `static/index.html:4166-4222` per-patch summary table | A `Partial` review renders its finding badges with a "partial" tag (it shows "In progress..." today). Precedence Reviewed > Partial > errors. |
| `static/index.html:4571-4596` `renderReviewCard` | `partial` joins `showDetails`; its own wording (§5). |
| `static/index.html:6205-6212`, `6292-6298` stats | The Partial card (D20) and its chart colour. |
| `scripts/verify_stage_progress_ui.mjs` | New cases (§6). |

About seventy other status sites need no change: they either compare with
in-flight values only (`Pending`, `In Review`, `Fetching`, `Incomplete`),
pass the string through unread, group by status, belong to another status
domain (bug lifecycle, outbox, patch application), or already treat any
unlisted status as finished, which is right for `Partial`.

---

## 4. Type-Driven State

| New type | Replaces |
|---|---|
| `ai::OutputTruncated { answer_chars, tool_arg_chars }` (error) | the `bail!` string at `session.rs:411` and the dead `ReviewError::OutputTruncated`; the counts decide the retry (§3.3) |
| `ai::session::FinalTurn { TurnBudget, OutOfTime }` | the `is_final_turn` boolean recomputed each turn |
| `ai::session::SessionStopped { cause: StopCause }` (error) | the fork's `SESSION_WOUND_DOWN` string and `contains` |
| `ai::WindDownSignal` | the fork's process-global `CancellationToken` (`tokio-util` is server-only) |
| `ai::IpcMessageType` | `msg_type: String` matching (its own commit, §7) |
| `ai::backoff_provider::ReviewDeadline` | `Arc<Mutex<Instant>>` |
| `ReviewTimedOut { active_seconds, salvage_seconds }` (error) | the text "Review tool timed out (active time exceeded)" matched at `reviewer.rs:3220-3225` |
| `workflow::StageStopCause` | `cancelled: bool` on six types (§3.7 table) and in the stored JSON |
| `workflow::ParallelGroupFailed { failures }` (error context) | the text context of an all-failed BestEffort group (`engine.rs:226-231`) |
| `review_stopped_by: StopCause` on the worker's per-patch entry | inferring a timeout from when an error output arrived |
| `activity::TrackedStage` (with `Done { cut_short }`) | the `starts`/`turns` maps in `StageTracker` |
| `reviewer::DeadlinePhase`, `ReviewToolOutcome`, `AttemptEnd` | the fork's `Option<WindDown>`, `timed_out` and `wound_down` booleans, and HEAD's choice of what to record by `Ok`/`Err` |
| `ReviewStatus::Partial` | the fork's patch-only `"Partial"` string under a `Failed` review and patchset |
| `PatchOutcome { Reviewed, Partial, TimedOut, Failed, Skipped, Cancelled }` | raw strings passed to `update_patch_status` (13 call sites in reviewer.rs) |
| `prior_final_outcome -> Option<PatchOutcome>` | `has_failed_review -> bool` (§3.8) |
| `PatchResult::Partial` | the fork returning `ReviewFailed` for a salvage |
| `reviewer::PatchsetTally { failed, partial }` with `final_status() -> ReviewStatus` | the `failed_patches` count and the `review_success` boolean (§3.11) |

**`ReviewStatus` and `PatchOutcome`.** `ReviewStatus` stays the one
vocabulary of the three status columns, and `Partial` is spelled there
once. `PatchOutcome` is kept as a narrower type for what the reviewer may
write to a patch: it rules out `Pending`, `In Review` and `Incomplete` at
compile time, and it carries `TimedOut`, which is a patch verdict and never a
review or patchset status. Its shared values do not repeat their strings:

```rust
impl PatchOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reviewed => ReviewStatus::Reviewed.as_str(),
            Self::Partial => ReviewStatus::Partial.as_str(),
            Self::Failed => ReviewStatus::Failed.as_str(),
            Self::Skipped => ReviewStatus::Skipped.as_str(),
            Self::Cancelled => ReviewStatus::Cancelled.as_str(),
            Self::TimedOut => "TimedOut",
        }
    }
}
```

The alternative, a `ReviewStatus::TimedOut` variant, would put a value that
never appears on a review or patchset into every match over the shared enum,
including `is_closed_to_new_parts`. It is not taken.

`PatchOutcome` covers only the values the reviewer writes. Apply statuses come
from the worker and go through `update_patch_application_status`
(`db.rs:9369`), which is not changed. `update_patch_status` takes
`PatchOutcome` and writes `as_str()`. The SQL gates that read `patches.status`
compare against `'Reviewed'` and `'Skipped'` only, so `'Partial'` and
`'TimedOut'` pass none of them, which is the intent.

---

## 5. Data, Schema, Wire, API, UI and Settings Changes

**Schema: no migration.** `reviews.status`, `patches.status` and
`patchsets.status` are `TEXT` with no `CHECK` constraint.
`stage_failures`, `stage_durations` and `logs` already exist (migration 015
and earlier). If D9 is decided differently, nothing here changes, because
only JSON content moves.

**Stored JSON.**

- `reviews.stage_failures` entries become
  `{stage, reason, cause, patch_index?}` instead of `{stage, reason, cancelled}`.
- `reviews.stage_durations` entries gain `stopped_by` (a `StageStopCause`)
  for a stage that did not run to completion (§3.9). A finished stage's
  entry is unchanged, `{stage, seconds, turns}`, so rows written before it
  read as finished. The page joins a duration with `stopped_by` to its
  stage's failure as one stopped row showing the time it ran, and never
  shows or counts it as done; the CLI prints the cause label after it.
- Readers (the UI and the CLI `print_coverage_warning`,
  `bin/sashiko-cli.rs:1202-1229`) fall back to `cancelled` when `cause` is
  missing: `cause ?? (cancelled ? 'stopped' : 'error')`. A legacy
  `cancelled: true` gets the neutral label "stopped", not the `cancelled`
  cause label. The fork wrote `cancelled: true` mostly for stages that ran
  out of time or were still running when the worker died (dcdcdf5ac937,
  56de4b78ea37), and HEAD's UI already renders the flag as "stopped"
  (`index.html:3736`), so old rows read as they do today (D9).
- `patchset_activity.phase` is durable, not transient: it is the
  write-through copy read back after a restart (`activity.rs:21-27`,
  `get_patchset_activity`, `db.rs:8239-8280`, served at `server.rs:1030`).
  Rows written before the upgrade are handled as follows:
  - `Phase::StageDone` gains `cut_short` with `#[serde(default)]`, so old
    rows still parse.
  - `Phase::StageFailed` replaces `cancelled` with `cause`, so an old row no
    longer parses. `get_patchset_activity` already drops a phase it cannot
    read (`phase: serde_json::from_str(&phase_raw).ok()`) and keeps the
    stored `description`, which says "{stage} cancelled: ..." or
    "{stage} failed: ...". Such a row renders from its description. It
    exists only for a patchset interrupted by a restart before the upgrade,
    and is replaced when that patchset next runs. No serde shim is added
    for it.

**Wire.**

- Daemon to worker: the `wind_down` envelope (§3.5).
- Worker to daemon progress:
  - `stage_finished` gains `cut_short` (default false);
  - `stage_failed` carries `cause` instead of `cancelled`. Both sides ship in
    one binary.
- Worker output: `review.stage_failures[].cause`, and
  `patches[].review_stopped_by` on a patch whose review failed because of
  the deadline (§3.8).

**Claude/Vertex request body:** top-level `output_config.effort`. `thinking`
no longer carries `effort`.

**API.** No new routes. A review's, a patch's and a patchset's `status` can
be `"Partial"`; a patch's can also be `"TimedOut"`. A Partial patchset reports
`finished: true` on the activity route, because `Partial` is not in
`UNFINISHED_PATCHSET_STATUSES` (`server.rs:982`). The stats route gains
`total_partial` (D20). `stage_failures` entries carry `cause`. Public activity
routes still strip model text from failure reasons. There is no status filter
on any list route or CLI command, so there is no filter to extend.

**Web UI** (`static/index.html`):

- `--status-partial` and `--status-timedout` added to all eight theme
  blocks, plus `.status-Partial`, `.status-TimedOut` and their
  `.status-dot` forms. Partial is amber, distinct from failed red and
  in-review orange; the four colour-blind themes take it from their own
  palettes. Badges are built as `status-${status.replace(/ /g,'')}`, so the
  one class covers the patchset header, the list row, review cards and the
  log page.
- The per-patch summary table (`:4166-4222`) handles a `Partial` review as
  its own case: its findings are aggregated like a Reviewed review's and
  rendered with a "partial" tag. Precedence is Reviewed > Partial > errors.
  Today such a review matches no branch and shows "In progress...".
- `renderFindingsBadges` (`:2730-2736`) keeps the check mark for `Reviewed`
  only. A Partial patchset or review with no findings shows "-", because it
  has not shown the code clean.
- The review card (`:4571-4596`) adds `partial` to `showDetails`, so its
  findings, summary and inline review are shown (today they would be hidden
  behind the live-progress host). Its time line reads "Stopped (out of time)
  after X" and never "succeeded on attempt N".
- The review-outcomes chart (`:6292-6298`) colours `Partial` with
  `--status-partial`, and the stats page gains a Partial card (D20).
- Stage rows (`:3730-3740`) are labelled by cause: `error` gives "failed",
  `cancelled` gives "cancelled", `out_of_time` gives "stopped: out of time",
  `cut_short` gives "cut short", `interrupted` gives "still running when the
  review stopped", and a legacy row with no `cause` and `cancelled: true`
  gives "stopped". `Phase::StageFailed`'s description text in `activity.rs`
  uses the same labels.
- A recorded stage that appears in both `stage_durations` and
  `stage_failures(cut_short)` is shown as one row: "cut short after Xs". Its
  duration carries `stopped_by: cut_short` (§3.9), which is how the page
  joins the two; the same join gives any stopped stage its time.
- The live `StageDone { cut_short }` uses the same wording.
- A Partial review card adds, under the coverage banner: "Any findings below
  come only from the stages that finished."
- Severity counts in the patchset list follow D15. With the recommended
  option, the list query (`db.rs:7190-7196`) counts findings of reviews with
  `r.status IN ('Reviewed', 'Partial')`, with no join to `patches`. The
  statistics query (`db.rs:5062-5066`) has no status filter and counts
  salvaged findings already, so a salvaged finding is counted in both views.
- `scripts/verify_stage_progress_ui.mjs` gets new cases: a Partial patchset
  whose Partial patch has a critical finding (the list counts it and the
  badge is `status-Partial`); a Partial patchset with no findings (the list
  shows "-", not the check mark); a Partial review card (findings, summary
  and inline review shown, "out of time" wording, no "succeeded", the
  findings note); the summary table rendering a Partial review's badges; a
  TimedOut patch; the cause labels including the legacy "stopped"; and
  `Partial` in the per-status "View Raw Log" loop (`:1005-1010`).

**CLI.** `print_coverage_warning` prints the cause labels. `Partial` and
`TimedOut` are yellow wherever patchset, patch or review statuses are
listed; the patch-status fallback is green today, which would make a Partial
patch look fully reviewed. `show --watch` treats `Partial` as finished and
fetches its review log. `show --summary` counts Partial patches separately
(text "Partial: N", JSON `partial`) instead of as "in review", and the best
review of a patch is the latest Reviewed one, else the latest Partial one,
else the latest of any status. A Partial review's note reads "stopped after X
(out of time)". The local progress printer (`main.rs:2256, 2267`) prints "cut
short" and the cause labels.

**Settings.** One key is added and nothing is renamed:

```toml
[review]
# Extra seconds a review gets after timeout_seconds to finish consolidating
# what its analysis stages found. 0 disables salvage: the worker is killed
# at timeout_seconds, as before.
salvage_seconds = 1800
```

- `ReviewSettings.salvage_seconds: u64`, `#[serde(default = "default_salvage_seconds")]`, default 1800 (D4; the fork used 900). The type is `deny_unknown_fields`, so this is the only shape accepted.
- The repository's `Settings.toml` gets `salvage_seconds = 1800` next to
  `timeout_seconds = 7200` (line 239), with the comment above, in commit 18.
- Deployment edit needed: none required, because the default applies. A
  deployment pins another value by adding the key.
- Docs (`docs/configuration.md`):
  - review table (`:480-481`): add `salvage_seconds`, and say "timeouts are
    not retried" in the `max_retries` row;
  - Claude table (`:117-118`): "`effort` is sent as `output_config.effort`".

---

## 6. Testing Plan

**Unit tests (ported from the fork where named):**

- **Effort.** `effort_is_sent_as_output_config_not_inside_thinking`, and the
  combined test asserting `json["output_config"]["effort"] == "high"` and
  `json["thinking"].get("effort").is_none()`. Add the same for
  `VertexClaudeRequest`. Replace the three tests that pinned the nesting
  (`claude.rs:886-907`).
- **Diagnostics.** `a_truncation_that_wrote_nothing_points_at_the_budget_not_the_answer`.
- **Cache.** `a_truncated_response_is_not_cached` (2 provider calls, then a
  cached whole answer). A seeded truncated entry is served as a miss.
- **Session.**
  - `a_truncated_answer_is_asked_again_rather_than_ending_the_session`
  - `truncation_retries_are_bounded_and_say_what_ran_out`
  - `a_discarded_turn_is_not_charged_against_the_turn_cap`
  - New: a truncated final turn pushes the budget prompt exactly once.
  - New: a validation retry on the final turn pushes it exactly once.
  - New: a final-turn answer with tool calls followed by a validation retry
    sends no `tool_use` without a `tool_result`, and an answer left with no
    text carries `IGNORED_TOOL_CALLS_NOTE` in `history`.
  - New: truncated usage is counted, and the truncated answer appears in the
    log but not in the next request.
  - New: an answer truncated with nothing written gets the reasoning
    feedback once, and a second such truncation in the same turn fails with
    the remedy in the error.
  - New: truncations are counted per turn: three truncations, an accepted
    turn, then three more still succeed; a fourth in a row fails.
  - New: a `max_turns = 1` stage sends `tools=None` and no "TURN BUDGET
    EXHAUSTED" message.
- **Stage.** `test_a_stage_survives_a_truncated_answer`
  (`TruncatingOnceProvider`). Recitation tests still pass.
- **Wind-down.**
  - `test_wind_down_stops_an_interruptible_session_before_it_spends` (0 calls).
  - New: wind-down after one turn yields exactly one tool-less call and
    `cut_short`.
  - `test_wind_down_lets_a_non_interruptible_session_finish`.
  - New: a failed out-of-time synthesis surfaces `SessionStopped(OutOfTime)`.
  - `test_consolidation_stages_survive_a_wind_down`, generalised: every stage
    named in the §3.6 table has the stated `interruptible` value, in both
    workflows.
- **Engine.** BestEffort records `CutShort` and `OutOfTime` with the right
  causes, and an all-stopped group is still an error. The group error
  downcasts to `ParallelGroupFailed`, and `stopped_by_deadline()` is true
  when any one stage (not only the first) was stopped by the deadline.
- **Worker error output.** A patch whose review fails with
  `SessionStopped(OutOfTime)` behind further context, or with a deadline
  `ParallelGroupFailed`, gets `review_stopped_by: "out_of_time"`; a
  validation failure in a non-interruptible stage does not.
- **Progress chain.** `cut_short` and `cause` survive every hop from
  `WorkflowEvent` to `Phase`, including `WorkerProgressEvent` and
  `local_review::ProgressEvent`. An old `Phase::StageDone` JSON without
  `cut_short` still parses.
- **IPC.** `IpcMessageType` parses the three types and rejects unknown ones.
  The reader sets the signal on `wind_down`.
- **ReviewDeadline.** Using `tokio::time::pause` rather than wall-clock time
  (the upstream credit test is flaky): credit moves both limits, and `check()`
  passes between `soft` and `hard` and fails after `hard`.
- **Classification.** `a_review_that_only_ran_late_reports_no_incomplete_stages`
  is extended into a table over (wound_down) x (no stops, `Error` stop,
  `CutShort`/`OutOfTime` stop), plus missing, empty and top-level shapes.
- **Tracker.** `stages_left_mid_turn_stop_claiming_to_be_running`: `Running`
  stages are settled with the given cause, and `Done`/`Failed` are untouched.
  `stopped()` includes a `Done { cut_short: true }` stage with cause
  `CutShort`. `timings()` lists every stage that ran: a settled stage and a
  stage whose failure the worker reported keep the time they ran up to the
  stop, with `stopped_by`, and a cut-short stage is one timing. A finished
  timing's JSON is unchanged and an old one reads as finished.
- **PatchOutcome.** `as_str` round-trip, and the existing SQL gate tests are
  unchanged. `prior_final_outcome` returns `Partial` and `TimedOut` for those
  patch statuses and keeps today's answer for a Failed row with no
  interaction. Every shared value's string equals the `ReviewStatus` one.
- **ReviewStatus::Partial.** `Display`, `FromStr` and `as_str` round-trip.
  `test_closed_to_new_parts_agrees_with_its_sql_form` (`db.rs:19249`) lists
  `"Partial"`, and a new case shows a complete Partial patchset refuses a
  late part and a new embargo.
- **Gates that must say no.** With a patch whose only review is Partial:
  `count_successful_reviews` is 0; the patchset is not clean under
  `CLEAN_PATCHSET_PREDICATE` (extend the embargo tests at
  `db.rs:11539-11846`); `get_completed_reviews_for_release` omits the
  review; `get_patchset_review_outcome` is `Incomplete`.
- **Embargo (D18).** An embargoed Partial patchset is released at
  `embargo_until` and not before; the release emails the Reviewed patches
  only; the list and details show `Embargoed` until then.
- **Rerun and resubmission (D19).** Extend `test_rerun_patchset_logic`
  (`db.rs:12851`): a Partial patchset keeps `target_review_count`, its
  Partial and TimedOut patches return to `NULL`, and its Partial review row
  and findings survive. Extend
  `test_has_patchset_by_msgid_excludes_failed_and_cancelled` (`db.rs:16264`)
  and the reset-to-Fetching test (`db.rs:16538`) with `Partial`, and add an
  agreement test for the shared resubmittable-status constant.
- **List counts (D15).** `get_patchsets` counts a Partial review's findings.
- **Stats (D20).** `total_partial` counts Partial reviews, and
  `total_failures` does not.
- **Patchset rule (D17).** `PatchsetTally::final_status` over the §3.11
  table, including a panicked task counting as failed.
- **Activity.** `test_patchset_activity_says_when_it_is_finished`
  (`server.rs:2907-2924`) includes `"Partial"`.

**Daemon tests with mock shell workers** (pattern of `run_review_tool_mock*`,
`reviewer.rs:4097-4150`):

- `test_timeout_asks_worker_to_salvage_its_findings`: the worker answers
  `wind_down` with findings and a `cut_short` failure. Expect `Salvaged`,
  review row `Partial` with findings and logs, patch `Partial`, patchset
  `Partial` (from commit 19 on; `Failed` in commit 18), no retry, and no
  email or forge outbox row.
- New: a two-patch patchset where one patch is Reviewed and one is
  salvaged ends `Partial`, emails the Reviewed patch at once when not
  embargoed, and queues no forge comment. With a TimedOut second patch it
  ends `Failed`.
- `test_timeout_is_typed_when_the_worker_never_salvages`: expect
  `ReviewTimedOut` by downcast, the live log promoted with its header,
  unfinished stages written with `out_of_time`, timings written (the
  killed stage's with `stopped_by: out_of_time`), patch `TimedOut`, and a
  single attempt.
- New: after `wind_down` the worker prints an error output whose patch
  carries `review_stopped_by: "out_of_time"` and streams a `stage_failed`
  with cause `out_of_time` first. Expect `TimedOut`, the tracker's stage
  failures stored, the live log promoted (not `"[]"`), and no retry.
- New: after `wind_down` the worker prints an error output with no
  `review_stopped_by`. Expect `Failed` with the worker's reason, the daemon
  record, and no retry (D5).
- New: before any deadline the worker prints an error output. Expect
  `Failed`, the daemon record (stage failures and promoted live log), and a
  retry as today.
- New: the worker prints its result and exits just before `soft`. The
  `wind_down` write fails, is logged, and the attempt is `Finished`.
- New: a token-budget abort is not retried and gets the daemon record.
- New: a patchset with a `Partial` patch and a `TimedOut` patch is
  processed again (as after a restart). Neither patch is reviewed again,
  both keep their status, and the patchset ends `Failed` again. With the
  `Partial` patch alone it ends `Partial` again.
- New: after `rerun_patchset` on a Partial patchset, the Partial patch is
  reviewed again and the Reviewed patches are not.
- New: a late finish (wound down, no deadline stops) gives Reviewed and
  queues notifications.
- New: a cancel during the salvage window gives Cancelled and an immediate
  kill.
- New: `salvage_seconds = 0` times out at `soft` with no `wind_down` written.
- `test_run_review_tool_timeout_reaps_stuck_child` with `salvage_seconds = 1`.

**Integration and UI:**

- `cargo test --all-features --test integration_tests -- --ignored`, which
  includes `test_activity_endpoint_reports_live_work`.
- `./scripts/verify_stage_progress_ui.sh` and `./scripts/verify_live_log.sh`.
- `make check-pr RANGE=2a7e748b1680..HEAD`, then
  `sashiko review --project sashiko --agent` on each commit.
- Manual: in `/tmp/sashiko-demo`, use a stub model with
  `timeout_seconds = 60` and `salvage_seconds = 60`. Watch the live stage
  table show the wind-down, cut-short rows and a Partial patch, then the
  stored Partial card, the Partial patchset in the list, and
  `sashiko-cli show --watch` exiting on it.

Per AGENTS.md, a commit-by-commit check runs in a throwaway clone, never
under `git rebase --exec`.

---

## 7. Commit Breakdown

Each commit compiles and passes tests on its own, and each makes one change
(AGENTS.md: no "do X and Y" commits). Fork sources are in brackets.

1. **ai: send the effort setting where the API reads it.** Claude and Vertex
   `output_config`, tests, and the docs row. [10e9bd0c3b3f, extended to
   Vertex]
2. **ai: say what a truncated response emitted and what to change.** Output
   tokens, character counts and `truncation_remedy` (in `ai/mod.rs`).
   [36935e97144e + 059f01bc541c]
3. **ai: stop caching truncated responses.** Skip on store, miss on read.
   [part of d4af7a8388f0]
4. **session: push the final-turn prompt once.** `FinalTurn` state, keeping
   the `max_turns > 1` guard; fixes the upstream duplication on validation
   retries. [new]
5. **session: keep unanswered tool calls out of a final-turn retry.** [new,
   D12]
6. **ai: ask again when an answer is cut off instead of losing the stage.**
   `OutputTruncated` with its counts, `truncation_action` with the two
   feedback texts, `TurnTruncations`, `push_feedback`,
   logging and usage of the discarded answer; removes
   `ReviewError::OutputTruncated`. [d4af7a8388f0, adapted per D7]
7. **workflow: say why a stage stopped with a typed cause.**
   `StageStopCause` through all six types of the §3.7 table, the stored
   JSON, `Phase`'s description text, the local printer, and the UI/CLI
   readers with the legacy `cancelled` fallback. Also the typed
   `ParallelGroupFailed` context. [new]
8. **review: type the review deadline.** `ReviewDeadline` (with
   `salvage = 0` everywhere for now) and the typed `ReviewTimedOut` from
   `DeadlineBudget` and the I/O loop, replacing the string match. No
   behaviour change. [dcdcdf5ac937 part]
9. **review: record what an attempt did however it ended.** Timings on every
   attempt, `TrackedStage` (with `Done { cut_short }`, always false until
   commit 17), `settle`, `stopped()`, and the first `AttemptEnd` with
   `Finished`, `TimedOut`, `Cancelled` and `Failed`. Stage failures are
   written for every daemon-record outcome, including an error output
   returned as `Ok`. `TimedOut` maps to `OutOfTime` because commit 8 already
   made it typed. [dcdcdf5ac937 part + 56de4b78ea37 deferred half]
10. **review: keep the conversation of an attempt that did not finish.**
    `promoted_live_log` for every daemon-record outcome. [dcdcdf5ac937 part]
11. **review: stop retrying review timeouts.** `TimedOut` is no longer
    retried (D5, first half). [dcdcdf5ac937 part]
12. **review: stop retrying reviews that exceed their token budget.**
    `AttemptEnd::BudgetExceeded`, not retried (D13). [new]
13. **review: type the status a review writes to its patch.** `PatchOutcome`,
    `update_patch_status(PatchOutcome)`, and `prior_final_outcome` replacing
    `has_failed_review` with today's rule only. [new]
14. **db: keep the statuses that may be resubmitted in one place.** The
    `NOT IN` lists of `has_patchset_by_msgid` (`db.rs:9211, 9222`) and the
    reset condition of `create_fetching_patchset` (`db.rs:9288-9292`, a Rust
    `==` chain) share one list of resubmittable statuses (`Failed`,
    `Cancelled`, `Failed To Apply`, `FailedToApply`): an SQL constant for the
    `NOT IN` lists and a Rust twin for the reset, with an agreement test.
    `Fetching` is not in the shared list. The reset also accepts it, as a
    separate `|| status == "Fetching"` term, while `has_patchset_by_msgid`
    treats a `Fetching` patchset as already ingested, so folding it into the
    shared list would change one of the two. No behaviour change. [new]
15. **status: add Partial and treat it as a finished status.**
    `ReviewStatus::Partial`; `is_closed_to_new_parts` (every variant listed)
    and `CLOSED_TO_NEW_PARTS_SQL` with the agreement test; `Partial` in the
    resubmittable statuses (D19); `show --watch` and its review-log fetch;
    `scripts/verify_live_log.sh`; the server's finished-status test and doc
    comment; the comments on the gates that exclude it on purpose (§3.8);
    and `Partial` in the five `Embargoed` display masks, patch and patchset
    (D18). Nothing writes the value yet, so every reader is ready before the
    first writer lands. [new]
16. **ai: type the stdin message kinds.** `IpcMessageType { AiResponse,
    Error }` replaces string matching; unknown kinds still `exit(1)`. No new
    message. [new]
17. **worker: let a review that runs out of time wind down.**
    `IpcMessageType::WindDown`, `WindDownSignal`, `WorkflowEnv.wind_down`,
    `StagePolicy.interruptible`, the out-of-time final turn,
    `SessionStopped`, `cut_short` through the progress chain, consolidation
    stages set non-interruptible in both workflows, and the worker's
    `review_stopped_by`. [dcdcdf5ac937 worker side, adapted per D2]
18. **review: salvage findings from a review that runs out of time.** The
    daemon `wind_down` write (failure only logged), `DeadlinePhase`,
    `salvage_seconds` with `salvage_seconds = 1800` in Settings.toml and the
    docs, `AttemptEnd::Salvaged`, `is_salvage`, classification of
    `review_stopped_by`, no retry after a wind-down (D5, second half), the
    review row and patch written `Partial`, the `TimedOut` patch outcome, the
    guard extended to both with rerun and resubmission clearing them (D16,
    D19), no notification and no `linux_bug` hand-off (D14), the late-review
    rule, and `PatchResult::Partial`. Until commit 19 the patchset tally
    still counts `PatchResult::Partial` as a failure, so the patchset ends
    `Failed`, as on the fork. [dcdcdf5ac937 daemon side + df962e0e26d0]
19. **review: finish a patchset as Partial when a patch was salvaged.**
    `PatchsetTally` from workers and the main loop, `final_status()` (D17),
    and the forge comment and immediate release gated on `Reviewed`. [new]
20. **review: release an embargoed Partial patchset when its embargo
    ends.** `p.status IN ('Reviewed', 'Partial')` in the two release queries
    (D18). The display masks already changed in commit 15. [new]
21. **web: show partial and timed-out reviews and stages the deadline
    stopped.** CSS, the summary-table and review-card cases, cause and
    cut-short labels, the findings note, the patchset-list counts (D15),
    `total_partial` and its stats card (D20), and the verifier cases.
    [dcdcdf5ac937 UI part not yet ported]
22. **cli: show partial and timed-out reviews.** Status colours, the review
    badge and its status-first label ("Partial, Issues Found"), the attempt
    note, the best-review order with `show --summary` reusing the shared
    helper, and the Partial count in `show --summary`. [new]

Commit 17 lands before 18, so the worker understands `wind_down` before the
daemon ever sends it. Commit 15 lands before 18, so nothing reads `Partial`
as an open or unknown status once it is written, and an embargoed Partial
patch is masked from the first write. Commit 18 is the largest. It
stays one commit because its pieces only make sense together: a `wind_down`
that is sent but whose result is not classified, or a `Partial` status that
nothing writes, would be a half-feature. If review finds it too large, the
guard extension with its rerun and resubmission resets (D16, D19) is the
part that can move to its own commit.

---

## 8. Open Decisions

Each decision has a recommended option.

**D1. Scope of the stdin protocol.**
- (a) Add only `wind_down` now, and leave cooperative cancel to the cancel
  step.
- (b) Also port the fork's `cancel` message and the 30 s
  `GRACEFUL_CANCEL_WAIT`.

*Recommend (a).* Upstream's DB-poll-and-kill cancel works. A cancel should
stop spending at once, and the reader's `IpcMessageType` makes adding
`Cancel` later a single variant.

**D2. What an interruptible stage does at wind-down.**
- (a) The fork's behaviour: bail immediately and lose what the stage gathered.
- (b) Bail only if the stage has made no model call yet; otherwise give it one
  forced tool-less synthesis turn and record it as `cut_short`.

*Recommend (b).* It reuses upstream's final-turn path and keeps hours of
evidence, at the cost of one call per running analysis stage inside the
salvage window.

**D3. The salvage rule.**
- (a) The fork's rule: wound down and any `stage_failures`.
- (b) Wound down and at least one stage stopped *by the deadline*
  (`OutOfTime` or `CutShort`).

*Recommend (b).* A stage that errored on its own long before the deadline is
already a recorded coverage gap on a `Reviewed` review. Under (a) it would
turn a late but otherwise complete review into a salvage, recorded `Partial`
instead of `Reviewed`, although the deadline stopped nothing.

**D4. Default `salvage_seconds`.**
- (a) 900, as in the fork.
- (b) 1800.

*Recommend (b).* Two incidents show 900 s is too short. The 534aee
consolidation tail alone took 26 minutes. On driver-a PR 968
(1d10318d403b) the fork's 900 s window covered deduplication and was then
used up five minutes into conflict resolution, so the hard kill took
everything. A window shorter than the consolidation tail ends in the very
kill that salvage exists to avoid. Even 1800 s is not a guarantee: a
consolidation that overruns it still loses the analysis output, and only the
later per-stage output persistence step fixes that (Non-goals). Pin a lower
value in Settings.toml if the worst-case overrun matters more. A later option
is to cap consolidation turns during wind-down.

**D5. Retrying timeouts.**
- (a) Never retry an attempt that was wound down or timed out, whatever it
  ended in: Salvaged, TimedOut, or Failed with another reason after the
  wind-down was sent.
- (b) Never retry Salvaged or TimedOut, but retry a Failed attempt that
  happened after the wind-down, as any other failure.
- (c) Keep upstream's retries with a fresh deadline each time.

*Recommend (a).* Four retries is how one patch spent twelve hours. An attempt
that reached its deadline has shown that the review does not fit, whatever
error finally ended it, and each retry costs another full deadline. Under (b)
a consolidation stage that fails on its own during the salvage window would
buy up to three more. The real reason is still recorded, and a rerun is one
command away.

**D6. How a salvaged review is shown and whether it is sent anywhere.**
Revised: the first version kept the review row and the patchset `Failed` and
marked only the patch `Partial`. Review found "Failed" too strong for a
review that produced verified findings. The status question and the
notification question are now separate.

*Status.*
- (a) `Partial` at all three levels: the review row, the patch, and the
  patchset under the D17 rule.
- (b) The first version: review `Failed`, patch `Partial`, patchset
  `Failed`.

*Recommend (a).* Partial passes none of the gates that ask "was this
reviewed?", because they all compare with `'Reviewed'` exactly (§3.8), so
the first version's reason for keeping the row `Failed` does not hold. The
cost is that every list of *finished* statuses must gain `Partial` (§3.11):
if one were missed, the closed-to-new-parts check would reopen a finished
series and two watchers would hang, which is why commit 15 teaches every
reader before anything writes the value.

*Notifications.*
- (a) The salvaged review sends nothing: no email, no patchwork check, no
  GitHub comment. The patchset's other, Reviewed patches notify as they do
  today (and after an embargo, D18).
- (b) Also post a GitHub summary or email for the salvaged review, with a
  coverage warning.

*Recommend (a) now.* The audit found no reason to change it: the email and
forge paths read `Reviewed` rows only, so (a) needs no new gate. Revisit (b)
with the planned work to post findings to the PR, because outbound partial
reviews need their own wording and gating.

**D7. Truncation retry budget.**
- (a) Share the session-wide `max_provider_error_retries` (3) with
  recitation, as in the fork.
- (b) Count truncations per turn, reset on every accepted response, bounded
  by `max_provider_error_retries` in a row; a truncation that wrote nothing
  is retried once only (§3.3).
- (c) As (a), but when the budget runs out after at least one accepted turn,
  force a tool-less synthesis turn instead of failing.

*Recommend (b).* Under (a) a long analysis stage that truncates now and then
fails outright on its fourth truncation, losing the evidence this step sets
out to keep. (b) bounds the cost per turn, and `max_turns` bounds the turns.
(c) asks for the stage's longest answer right after several shorter ones
failed to fit, so it would mostly truncate again. Retrying a reasoning-only
truncation more than once resends the full history to no purpose; the error
names the setting to change instead.

**D8. Stage timings for failed attempts.**
- (a) Write them for every attempt.
- (b) Keep HEAD's Ok-only rule.

*Recommend (a).* Each attempt is its own review row, so nothing mixes, and
on a timed-out row the timings are the answer to "what ate the time".

**D9. Stored `stage_failures` shape.**
- (a) Replace `cancelled` with `cause`, with a one-line reader fallback for
  old rows.
- (b) Write both.
- (c) Keep `cancelled` and add `cause`.

*Recommend (a).* No dead field, and old fork rows still render. The
fallback maps a legacy `cancelled: true` to "stopped", not "cancelled",
because the fork set it mostly for stages stopped by time; that is also
today's label (§5).

**D10. Truncation detection for Bedrock, Ollama and the CLI providers.**
- (a) Out of scope; follow up with Bedrock (`stopReason == "max_tokens"`)
  first.
- (b) Include it in this step.

*Recommend (a).* The deployments use the Claude API path, which already
detects truncation.

**D11. Effort on the deployment gateway.** The fix changes the wire format
for Claude and Vertex Claude.

*Recommend:* apply it to both, and confirm that the gateway
(`llm-gateway.example.com/Anthropic`) accepts `output_config` before setting
`effort` in any deployment's Settings.toml. Until then, deployments that
leave `effort` unset send nothing new. Keep passing `thinking = "enabled"`
through unchanged.

**D12. The final-turn dangling tool calls fix (commit 5).**
- (a) Include it as a separate commit in this step.
- (b) Defer it.

*Recommend (a).* The final-turn retry path this step makes busier is the
one that hits it.

**D13. Retrying an attempt that exceeded its token budget.**
- (a) Do not retry: `AttemptEnd::BudgetExceeded` is final, and the attempt
  gets the daemon record.
- (b) Keep upstream's retries. Each attempt starts with fresh token counters
  (`reviewer.rs:2817-2818`), so a patch can spend up to
  `(max_retries + 1) x max_total_tokens`.

*Recommend (a).* The budget is a cost ceiling, and retrying multiplies it by
four. An identical second attempt has no reason to need fewer tokens. This
changes upstream behaviour, so it is its own commit (12).

**D14. Concerns of a salvaged review and the `linux_bug` pipeline.** On HEAD,
storing a Reviewed review also hands its concerns to `linux_bug` when that
is enabled, with subsystem attribution and a mainline lookup
(`reviewer.rs:2075-2222`). The fork's salvage branch had no such hand-off.
- (a) Do not hand off concerns from a Partial review.
- (b) Hand them off like a Reviewed review's.

*Recommend (a).* A bug investigation costs more than the review that raised
it, and the concerns come from a review whose coverage is known to be
incomplete. The concerns stay in the stored output, so a later change can
hand them off if wanted. This matches D6: nothing leaves a Partial review
row. The review row being `Partial` rather than `Failed` does not change
this: the hand-off sits in the `Reviewed` branch only (`reviewer.rs:2075-2222`),
and the salvage helper must not call it.

**D15. Where a salvaged review's findings are counted.** The patchset list
counts only findings of `Reviewed` reviews (`db.rs:7190-7196`). The
statistics view counts all findings (`db.rs:5062-5066`). With the review row
now `Partial`, option (a) needs no join to `patches`.
- (a) Count them in both: the list query uses
  `r.status IN ('Reviewed', 'Partial')`.
- (a') As (a), but count a Partial review only while its patch is still
  `Partial` (a join to `patches`), so that after a rerun produces a Reviewed
  review its findings are not added to the old salvaged ones.
- (b) Count them in neither: the statistics query gains
  `r.status = 'Reviewed'`.
- (c) Leave both queries alone and accept the mismatch.

*Recommend (a).* Salvaged findings went through deduplication, conflict
resolution and verification like any other, so a critical one must not read
as zero in the list. The `Partial` status next to the counts says the
coverage is incomplete. The overlap (a') avoids after a rerun is the same one
the list already has for a patch with several `Reviewed` reviews
(`target_review_count > 1`), which it sums today, so (a') would treat one
kind of repeated review differently from the other. (b) also changes what
upstream's statistics count today.

**D16. A Partial or timed-out patch when the patchset is processed again.**
After a restart, `count_successful_reviews` counts `Reviewed` rows only and
`has_failed_review` only stops patches with a `Failed` row and no
interaction, so a Partial review, and a timeout reported through the
worker's error output, would be reviewed again from scratch (§3.8).
- (a) Record a timeout as patch status `TimedOut` (new) next to `Partial`,
  and have the guard skip both by reading `patches.status`, keeping their
  status. Rerun and resubmission clear both (D19).
- (a2) As (a), but detect Partial from `reviews.status = 'Partial'` now that
  the review row carries it.
- (b) Guard `Partial` only, and accept one fresh review of a patch whose
  timeout came through the worker's error output.
- (c) Accept re-reviewing both after a restart.

*Recommend (a).* It is the only option consistent with D5, and the patch
list then says why a patch failed. The Partial review row does not change
the choice of column: a review row is history that survives a rerun, so
(a2) would skip the patch for ever after a rerun unless the rerun deleted
the salvaged review and its findings, and it still could not see `TimedOut`,
whose review row is `Failed` (§3.8). The cost is one more patch status value
with its CSS, and the two resets of D19, which this step now makes itself
rather than leaving to port step 7, because `rerun_patchset` is already on
HEAD.

**D17. The patchset's status when some patches did not finish.** New. A
patch ends Reviewed, Skipped, Partial, TimedOut, Failed or Cancelled.
- (a) `Reviewed` when every patch is Reviewed or Skipped; `Partial` when no
  patch is Failed or TimedOut (and no task panicked) and at least one is
  Partial; `Failed` otherwise. Precedence Cancelled > Failed > Partial >
  Reviewed.
- (b) `Partial` whenever any patch is Partial, even if another patch Failed
  or TimedOut, because the patchset has salvaged findings to look at.
- (c) `Partial` when at least one patch has a review (Reviewed or Partial)
  and every patch that has none ran out of time (`TimedOut`); `Failed` only
  for a non-deadline failure or when no patch has a review at all.

*Recommend (a).* It gives `Partial` one meaning at every level: every patch
that was not skipped has a review, and at least one of them is incomplete. Under (b) a patchset
with a patch that failed outright, for example on a provider outage, would
wear the softer label, and the patch list would be the only place to learn a
patch has no review at all. (c) is the option to choose if "Failed is too
strong" is also meant for a series where nine patches were reviewed and the
tenth ran out of time with nothing salvaged: it calls that patchset Partial
although one patch has no review. Under (a) that series is `Failed`, and its
patch list still shows nine Reviewed patches and one TimedOut. **Question for
the reviewer:** should a timed-out patch with nothing salvaged make the
patchset `Failed` (a) or `Partial` (c)?

**D18. An embargoed Partial patchset.** New. Embargo release requires
`p.status = 'Reviewed'` (`db.rs:8539, 8688`). A non-embargoed Partial
patchset has already emailed each of its Reviewed patches as it finished
(`reviewer.rs:2252-2300`).
- (a) Release a Partial patchset at `embargo_until` (never early: the clean
  predicate is false for it). The release emails its Reviewed reviews only;
  the Partial review stays unmailed (D6). While embargoed, `Partial` is shown
  as `Embargoed`, like `Reviewed`.
- (b) Never release it, as a `Failed` patchset is never released today.

*Recommend (a).* Under (b) the embargo changes what is eventually sent: the
same series would email its Reviewed patches without an embargo and never
with one. (a) keeps D6, because `get_completed_reviews_for_release` reads
`Reviewed` rows only. The display mask keeps an embargoed Partial patchset
from saying early how its review went, as it does for Reviewed.

**D19. Rerunning and resubmitting a Partial patchset.** New.
`rerun_patchset` never touches `patches.status`, and only `Failed`,
`Cancelled` and `Failed To Apply` patchsets may be resubmitted
(`db.rs:9211, 9222, 9288-9292`). On HEAD a review cut short by its deadline
ends `Failed`, so its series can be resubmitted.
- (a) Both paths clear `Partial` and `TimedOut` patch statuses; `Partial`
  joins the resubmittable statuses; a rerun of a Partial patchset does not
  raise `target_review_count`, so only the patches without a full review are
  reviewed again.
- (b) As (a), but `Partial` is not resubmittable: it counts as already
  ingested, like `Reviewed`, and `rerun` is the only way to try again.

*Recommend (a).* It keeps today's ability to resubmit a series whose review
ran out of time, and the user's request was for a softer label, not for
fewer ways to retry. Without clearing the patch statuses, both paths would be
silent no-ops, because the D16 guard would skip exactly the patches the user
wants reviewed again.

**D20. Partial in the statistics.** New. Total Failures counts statuses
matching `failed` or `error` (`db.rs:5099-5110`), so Partial reviews leave
it; the success rate is Reviewed over all finished reviews.
- (a) Add `total_partial` and a Partial card; the success rate stays
  Reviewed over finished.
- (b) Count Partial as a success.
- (c) Count Partial as a failure.

*Recommend (a).* The cards then add up (Reviewed + Partial + Failures + the
other finished statuses), and the change is visible rather than shown only
as a fall in the failure count. (b) overstates coverage; (c) is the label
the user found too strong.
