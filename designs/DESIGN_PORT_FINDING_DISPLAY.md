# Design: Port Step 4 -- What Each Finding Is and Why It Got Its Severity

Status: approved, 2026-10-05, after two review rounds and a check of every
data claim against the three deployment databases (§1.3). Built on
`port/upstream` as commits 2 to 7 of §7 (plus two follow-ups from the
independent review: a safer CLI finding line, and a web row that agrees
with the counters). The §6.5 measurement of commits 2 and 3 is done and its
results are recorded there.
Branch: `port/upstream` (upstream `origin/main` 2a7e748b1680 plus the ported
fork features). The fork source is `feat/prior-revision-findings`, commit
46d809932fa9 ("web, review: say what each finding is and why it got its
severity").

Numbering: the port plan calls this step 4. `DESIGN_PORT_FORK_FIXES.md` is
step 3b (Q1).

## 1. Context

### 1.1 What the fork commit did

The fork's review card counted findings by severity and showed nothing else
about them. To learn what had been found, a reader had to open the whole
inline review. The commit made three changes:

1. **A `headline` per finding.** This is one sentence of at most 120
   characters.
   - The verification stage asks for it, in its key list and in its worked
     example.
   - It is stored in a new nullable `findings.headline` column (via
     `schema.sql` and `try_add_column`).
   - `get_completed_reviews_for_release` returns it.
   - The reason: fork `problem` is a long argument. In the deployments it
     is 148 to 4695 characters (median 1316) and none of 747 is 80 or
     shorter. Its first sentence is 120 characters or less in only 170 of
     747, and the first 160 characters hold a backtick in 438 (§1.3).
2. **A "Findings (N)" table on the review card.** It starts collapsed, lists
   the worst severity first, and each row expands to show:
   - the full `problem`;
   - `severity_explanation`, split on the rubric labels "Consequence:",
     "Triggering path:" and "Reachability:" from
     `third_party/prompts/kernel/severity.md`;
   - the locations, as `file:function:line`.
3. **"(speculative)" and "(preexisting)" tags on rows.** The speculative tag
   is guessed from the explanation text and only checked for Low and
   Medium, because High and Critical explanations often say "not capped as
   speculative".

It added a unit test that every key in the verification key list also
appears in the worked example, and about 25 checks to the UI verifier. It did
not touch the CLI, `server.rs` or `api.rs`.

### 1.2 What changed upstream since

- **`problem` is already a short title.** Upstream made it a "short naming
  string" in both workflows: a1065edde105 for Linux, and 6cd535482699, which
  added the Sashiko workflow, for Sashiko.
  - Linux (`src/workflows/linux_patch_review.rs:1184`, BUG NAME RULES):
    under 80 characters, a subsystem prefix, no backquotes, `fn_name()`
    form, root cause rather than consequence. Example:
    "mm: memory leak in func_x() due to unmet condition Y".
  - Sashiko (`src/workflows/sashiko_patch_review.rs:988` and
    `STAGE_VERIFICATION_INSTRUCTION` item 8): under 80 characters, with a
    component prefix such as `workflow:`.

  So upstream `problem` fills the role of the fork's `headline`, with a
  stricter rule: the fork asked for one sentence of at most 120
  characters, with no prefix or `fn_name()` form. None of the 540 stored
  fork headlines has a subsystem prefix, and their median is 113
  characters. The long argument that fork `problem` carried now lives only
  in `severity_explanation` and in each location's
  `why_this_location_matters`.
- **Preexisting findings no longer stay in the findings list.** Both
  verification reduces move them into `state.concerns`, which goes to the
  bug pipeline (`linux_patch_review.rs:1223-1244`,
  `sashiko_patch_review.rs:1005-1026`). The same two commits brought this
  split: a1065edde105 for Linux, 6cd535482699 for Sashiko.
- **The schema uses numbered migrations.** There is no `src/schema.sql`. The
  latest migration is `016_review_log_entries.sql`.
- **Sashiko reviews have a summary and a severity bullet list.**
  - The summary stage (887b2de0bede) writes `reviews.summary`. Its prompt
    says to describe the change and *not* to list findings
    (`STAGE_SUMMARY_INSTRUCTION`, `sashiko_patch_review.rs:243-258`).
  - The report stage writes the inline review as a bullet list of every
    finding, `- [CRITICAL] ...` down to `- [LOW] ...`
    (`STAGE_REPORT_INSTRUCTION`, `github-summary-template.md`,
    `validate_github_summary_format`). `escapeHtml(.., true, true)` colours
    those bullets by severity (9c69a21182ce).
  - Linux reviews have no summary stage, so their summary is empty. Their
    inline review is the LKML email, with `[Severity: X]` tags.

  The caller's brief says the bullets sit in the *summary*. They do not:
  they are the Sashiko *inline review*. §4.4 is built on that.
- **`prompts/sashiko/severity.md` has its own calibration headings**
  (bf7111d270ce): "Blast radius and irreversibility", "Likelihood and
  preconditions", "Speculative findings" and "Non-primary LLM providers".
  The kernel rubric is the same in the fork and upstream.

### 1.3 Data behind the decisions

These are read-only queries of copies of the three deployment databases
(kernel-fork, driver-a and driver-b on the deployment host, which
run `feat/prior-revision-findings`). The copies were taken with SQLite's
`.backup`; `response_cache.db` was not copied. They hold 747 findings
(186, 219 and 342), all kernel reviews of ionic and pds code, written by
claude-opus-5 (6 by a Sonnet model) between 2026-09-03 and 2026-10-05.
There are no Sashiko-project rows.

- **`problem` is long, and newer rows carry a separate title.** `problem`
  is 148 to 4695 characters, median 1316. 540 of the 747 rows have a
  `findings.headline` (69 to 161 characters, median 113; 131 over the
  fork's 120 cap, 4 at 80 or less), and so do 531 of the 738 findings in
  the stored output JSON: every finding since 2026-09-22. The other 207
  rows predate 46d809932fa9 and have no title at all. When the headline
  arrived, `problem` grew (median 883 to 1456) and `severity_explanation`
  shrank (1146 to 732): the model moves text between fields as the schema
  changes.
- **The labels are mostly present.** 734 of 747 explanations contain
  "Consequence:", 653 "Triggering path:" and 642 "Reachability:". 719
  (96%) open with "Consequence:". No Sashiko-rubric heading and no bold
  `**Label**` form appears. The fork's worked example was never
  relabelled (it still says "1. Condition Y is met."), so the model wrote
  these labels without being shown them.
- **The labels are not always in one form.**
  - About 60 explanations put a qualifier between the label and the
    colon: "Triggering path (verified, not speculative):" (14),
    "Triggering path (verified):" (9), "Triggering path verified in code:"
    (13), "Consequence first:" (7), "Consequence (speculative):" (2),
    "Consequence if real:", "Reachability caveat:", "Reachability caps the
    level:" and others.
  - 15 explanations are numbered lists, "1. Consequence: ...\n2.
    Triggering path: ...\n3. Reachability: ...", some on one line ("...
    class. 2. Triggering path:").
  - Labels follow a newline in 17 explanations, and two spaces in 6.
  - Most labels still start the text or follow ". ".
- **What each label rule makes of them.** The strict rule of the first
  draft (start of text, line start, or after `.`, `;` or `)` and spaces,
  then the label and a colon) finds all three kernel sections in 584 of
  747 (78%). It swallows qualified labels into the previous section, and in
  the numbered lists it leaves a stray "2" or "3" at the end of 11
  sections. The rule in §4.4, which also accepts a list number at a
  boundary and a short qualifier before the colon, finds all three in 636
  (85%) and leaves no stray numbers.
- **The fork's parser has two kinds of false match.** Its optional list
  marker swallows a number that ends the previous sentence, so in 6
  driver-a findings a body loses it ("... leaves it at" instead of "...
  leaves it at 1."; driver-a #42, #202). Being case-insensitive, it
  also opens a section at the prose "Not lowered for reachability:"
  (driver-a #182). The §4.4 rule does neither.
- **Guessing "speculative" from text gets it wrong both ways.** About 79
  Low or Medium explanations mention "speculative"; the fork's
  `isSpeculativeFinding` tags 64. Read by hand, 57 of the 64 are right
  (89% precision) and about 57 of 68 truly speculative findings are found
  (84% recall).
  - Its false positives are mostly "capped at Medium" for a reason other
    than speculation (driver-a #163, #165). The model caps for other
    reasons in 23 of 68 "capped" explanations.
  - Its misses include driver-a #69 and #181 and driver-b #8.
  - 15 explanations call a finding "partly speculative", and about 7 call
    only its consequence speculative ("Consequence (speculative):",
    driver-b #88, #89). §4.1 says how the typed key treats them.

  Both rubrics already tell the model to "mark it speculative", but no field
  exists to mark it in. None of the 738 stored output findings has a
  `speculative` key.
- **Preexisting findings dominate the old Highs.** 120 of the 172 High or
  Critical findings are preexisting (69 of 82 in driver-a). Old cards
  list them; reviews made on the port route them to concerns (§1.2).
- **The deployment databases will move to the port.** They report
  `PRAGMA user_version = 1`, carry the fork's extra columns
  (`findings.headline`, `findings.prior`, `reviews.stage_outputs` and
  others), and already have the tables that migrations 015 and 016 create.
  The user decided on 2026-10-05 that the deployments keep their
  databases, so a later port step adopts a fork database (D14).

### Goals

1. A finished review card lists every finding: severity, title and tags,
   worst first. Each finding opens to show why it got its level and where it
   is.
2. A card never shows the same findings twice when it first loads (§4.4).
3. On a review made after this step, a reader can tell a finding capped
   because it is speculative from a finding that is Medium on its merits.
   The model states which it is in a typed field; it is not guessed from
   prose. A review made before this step cannot make that distinction, and
   its card says so once instead of implying that nothing was speculative
   (D13).
4. Findings stored before upstream's short-title rule still get a usable
   one-line title, and their full text can still be reached. For the 540
   deployment findings that carry a fork headline, the title is that
   headline, put into `problem` when the fork database is adopted (D14).
   The 207 older ones have no title anywhere; theirs is `problem` clipped,
   with the full text in the body.
5. `sashiko-cli` prints what each finding is. Today it prints an empty
   description for each one (§2).
6. This holds for both workflows, `linux` and `sashiko`.

### Non-goals

- A separate `headline` field or column (D1).
- A validator for the verification stage's per-finding keys. The stage has
  none today, and adding one is its own item (D6).
- Restoring a long `description` field that upstream dropped (D2).
- Prior-revision findings (`findings.prior`), fork 5cf550d2bec6. That is a
  later step; §4.6 records what it inherits from this one.
- Reading fork-era `headline` keys in display code. The fork-database
  adoption step moves them into `problem` once (D14), so the page and the
  CLIs only ever read `problem`.
- Adopting a fork database. That is its own port step, after the feature
  steps, because it has to know every column the port ends up with. This
  step only states what the adoption must do with findings (D14).
- Per-finding content in email or forge PR comments. Email reads finding
  rows only to count the new ones (`queue_notifications`,
  `reviewer.rs:4546`). The forge PR comment (`queue_forge_pr_comment`,
  `reviewer.rs:534-600`) does not read findings at all; it posts each
  review's `summary` and `inline_review`.

## 2. Baseline

What `port/upstream` already has that this step builds on:

- **Verification output** is `VerificationOutput { findings: Vec<Value> }`
  (`linux_patch_review.rs:201-204`), and both workflows reuse it. It has
  `OutputFormat::json()` and no validator. Keys inside a finding are
  free-form.
  - Linux keys: problem, severity, severity_explanation, preexisting,
    locations. There is a worked example, whose `severity_explanation` is
    "1. Condition Y is met.\n2. ...", without the labels the rubric asks
    for.
  - Sashiko asks for the same keys and has no example.
- **The worker's review JSON** (`src/worker/prompts.rs:664-673`) carries
  `summary`, `findings` (that is, `state.findings`), `concerns`,
  `review_inline` and the rest. The daemon stores it as
  `ai_interactions.output_raw`. The API serves it as each review's `output`
  (`db.rs:7735-7775`, `7994-8030`). The web page and `sashiko-cli` read
  findings from there, not from the `findings` table.
- **Finding rows.**
  - `Reviewer::store_review_findings` (`reviewer.rs:2663-2688`) writes them.
    Both the Reviewed path and the salvaged-Partial path use it.
  - The only code that reads them back is `get_completed_reviews_for_release`
    (`db.rs:8888`; its findings SELECT is at 8930). Its callers are embargo
    release, which passes the findings to `queue_notifications` to be
    counted, and the forge PR comment, which never reads them.
  - `src/bin/benchmark.rs:549` reads `problem` and `severity_explanation`
    for its judge.
- **Review card** (`static/index.html:4764`, `renderReviewCard`).
  - `outputJson` is already declared outside the `try` (line 4866), as the
    fork had to arrange itself.
  - The Result line counts findings. A patchset-level card leaves out
    preexisting ones. A patch card shows them in parentheses.
  - Then come Summary (`r.summary || output.summary`, open, line 4972) and
    Inline Review (open when findings were parsed and there are any, line
    4986).
  - The page has helpers this step reuses: `getSeverityColor` (2406, a
    whitelist mapping onto `--sev-*`), `escapeHtml` (2497) and
    `toggleCollapsible` (2783).
  - The bug list already shows "(no description)" for an empty problem
    (line 3221).
- **UI verifier** (`scripts/verify_stage_progress_ui.mjs`). It loads the
  functions named in `NAMES` through `load(sources)`. It runs check groups
  (`stageChecks`, `partialChecks`, ...) and then mutation checks
  (`mutate(name, from, to)`), each of which must make a check group fail.
  Two existing checks touch this step:
  - "card: a Partial review that found something opens it" (`▼ Inline
    Review`);
  - the mutation "a Partial card that opens an empty inline review is
    caught".

  It is run by hand through `scripts/verify_stage_progress_ui.sh`, which
  finds a node (or skips when there is none). It is not part of
  `make check-pr`. `load()` defines only the functions named in `NAMES`, so
  any page function a new function calls must be added there too.
- **CLIs.**
  - `sashiko review` (`src/main.rs:2534`, `print_finding`) prints
    `[Severity] problem`.
  - `sashiko-cli`'s `print_findings_summary` (`src/bin/sashiko-cli.rs:2941`)
    reads `file`, `line`, `problem_description` and `recommended_fix`. No
    workflow emits those keys (upstream ad95b48706f4), so each finding
    prints as a bare `[High]`. `designs/DESIGN_LOCAL_REVIEW_UX.md` §3
    describes the same mismatch, and fixed it only in `main.rs`.

## 3. Verdict per Part of the Fork Commit

| Part | Verdict | Reason |
| --- | --- | --- |
| `headline` key in the verification prompt and example | Drop | Upstream `problem` is already a short title. A second title field would make two near-identical fields and break "Unambiguous Naming" and "No Dead Outputs" (D1). |
| `findings.headline` column, `try_add_column`, `Finding.headline` | Drop | Nothing to store. No migration in this step. Stored fork headlines become `problem` when a fork database is adopted (D14). |
| `headline` parsing in `reviewer.rs` | Drop | Same reason. |
| `headline` in `get_completed_reviews_for_release` and the `test_pending.rs` assertions | Drop | Same reason. That JSON already carries `problem`. |
| Test that the key list and the worked example agree | Adapt | Linux only, because Sashiko has no example. Checks the upstream keys plus `speculative` (§4.1). |
| `renderFindingsTable` | Adapt | Keep the ordering, counting, escaping and "no rows, no block". Change three things: whether it opens (§4.4), the severity colour (whitelisted through `getSeverityColor`, D8), and `<details>` per finding instead of hidden `<tr>` rows (D7). |
| `findingHeadline` | Adapt as `findingTitle` | It reads `problem` and clips over-long text (§4.4). |
| `parseSeverityCalibration` / `renderSeverityCalibration` | Adapt | Case-sensitive, boundary-anchored label rule fitted to the forms the data shows (§1.3), plus the Sashiko rubric's headings (D4). |
| `isSpeculativeFinding` (text guess) | Replace | A typed `speculative` boolean from the verification stage (§4.1, D3). |
| `renderFindingLocations` | Adapt | `file:line (function)`, the same as the CLI, plus `why_this_location_matters`, which now carries part of the argument that left `problem` (D2). |
| `toggleFindingReasoning` | Drop | Replaced by `<details>` (D7). |
| `(preexisting)` tag | Keep | It is cheap and matches the Result line, which already counts preexisting findings that are present in `output` (§4.4). |
| Hoisting `outputJson` | Already upstream | Line 4866. |
| UI verifier checks | Adapt | Reworked for the changes above, and registered in this harness's `NAMES` / check-group / mutation structure (§6.3). |
| `findings_sum_test.rs` `headline: None` literals | Drop | No field. |

## 4. Design

### 4.1 Prompt and schema

**One new key, `speculative`, in both verification stages.** The rubrics
already tell the model to "mark it speculative". This gives the mark a place
to go, so no reader has to guess it from prose (D3).

**Where the key goes.** In both key lists, and in the Linux example,
`speculative` comes right after `problem` and *before* `severity`. The cap
is a condition on the level, so the model commits to it before it picks the
level. Upstream already lists `severity` before `severity_explanation`; this
step does not reorder those two.

**The wording is the rubric's own**, so each definition has exactly one
reading and its two halves do not overlap ("Unambiguous Naming", "Shared
Vocabulary").

Linux (`third_party/prompts/kernel/severity.md` lines 17-30, Triggering
path and the paragraph after it):

> "speculative" (a boolean: true if you could not lay out a concrete
> triggering path because it rests on an unproven assumption, or on an ABI,
> register, or convention you might be misreading; the severity definitions
> cap a speculative finding at Medium. If any part of the triggering path
> rests on such an assumption, answer true. false if the triggering path is
> established from the code. Uncertainty about reachability or about the
> consequence alone does not make a finding speculative and never lowers
> its level)

Sashiko (`prompts/sashiko/severity.md`, "Likelihood and preconditions" and
"Speculative findings"):

> "speculative" (boolean: true if the concrete path and preconditions that
> trigger the bug rest on an unverified assumption; severity.md caps a
> speculative finding at Medium, or Low if the impact is minor. If any
> part of that path rests on one, answer true. false if that path and its
> preconditions are established from the code. A low likelihood that is
> established from the code is reflected in the level, not in this key)

Both definitions are about one question, whether the triggering path is
established, so `true` and `false` cover every case without an "unknown"
value. A path that is partly unproven is not established, which is what
the "any part" sentence says; it settles the 15 stored "partly
speculative" findings (§1.3). There is no blanket "if unsure, say true": a
model unsure about reachability must not reach for the cap the rubric
forbids for that reason.

**A behaviour change this brings.** About 7 stored findings call only
their consequence speculative ("Consequence (speculative):", as in
driver-b #88 and #89) and were capped for it. Under these definitions they are
`speculative: false`, and the kernel rubric does not cap them, so they may
come back a level higher. That follows the rubric, which caps only an
unproven trigger, but it is a change in what reviews say, so §6.5
measures it.

**An absent key means "not marked".** That holds for every review stored
before this change and for any run where the model leaves the key out,
since the stage has no validator (D6). The display treats "not marked"
like `false` on each row (no tag), and says once per card when no finding
carries the key at all (D13). Code that keeps the value rather than
displaying it, such as a later storage step (§4.6), reads it as
`Option<bool>` and keeps `None` for "not marked"; it never turns absent
into `false`.

**The Linux worked example matches its own instructions** (D12).
Instruction item 5 and the kernel `severity.md` ask the model to lead
`severity_explanation` with consequence, triggering path and reachability.
The example does not do this. It becomes:

```json
"problem": "mm: memory leak in func_x() due to unmet condition Y",
"speculative": false,
"severity": "High",
"severity_explanation": "Consequence: the buffer allocated in func_x() leaks on every call that meets condition Y. Triggering path: a caller passes Y, func_x() returns early before kfree(). Reachability: reachable from the ioctl handler by an unprivileged user.",
```

The example keeps `"severity": "High"` and stays consistent with
`speculative: false`. A fork-era model already wrote these labels without
being shown them (§1.3), and its numbered "1. Consequence:" hybrids look
like the unlabelled numbered example mixing with the rubric. The example
makes the plain form dependable, which the parser in §4.4 relies on. This changes Linux review output, so commits 2
and 3 are measured before they land (§6.5).

**Sashiko gets no worked example in this step** (D5). It has only the key
list, so the key list and the example cannot disagree.

**Reduces are unchanged.**
- `speculative` passes through on each finding in `state.findings`, and from
  there into the worker's review JSON, `output_raw`, the API's `output`, and
  the agent-mode JSON from `format_agent_review_output`, which passes
  findings through as they are.
- A preexisting finding is rebuilt by hand into a concern
  (`type`/`description`/`reasoning`/`preexisting`/`locations`), and
  `speculative` is dropped there. That is intended: the bug pipeline
  calibrates its own severity.
- Both report stages receive the findings JSON, so they now see
  `speculative`. No report prompt is changed in the plan. Whether the LKML
  email or the Sashiko bullets change because of it is measured before
  commit 3 lands (§6.5, Q3); if the wording changes in a way nobody wants,
  commit 3 also adds one line to each report instruction.

**Not changed:**
- `severity_explanation` stays one string. Splitting it into
  `consequence`/`triggering_path`/`reachability` fields would not fit the
  Sashiko rubric. It would also break the bug pipeline (`reasoning`), the
  benchmark judge and `findings.severity_explanation`.
- `problem` keeps upstream's title rule.
- No validator (D6).

**Docs.** Update `designs/MULTI_STAGE_REVIEW.md` "Stage 10" output JSON to the
current keys, adding `locations` and `speculative`. It is already out of
date: it has no `locations`.

### 4.2 Storage

**None.** No migration and no new column. The next free number, 017, stays
open for the prior-revision step (§4.6).

`speculative` is not stored in `findings`, because no row reader needs it:
- the web and both CLIs read `output`;
- embargo release only counts findings, and the forge PR comment does not
  read them;
- the benchmark judge reads problem and explanation.

A column nobody reads would be a dead output. The step that first needs it
in SQL adds it.

### 4.3 API and wire

**None.** The new key rides inside the review's `output` JSON, which the API
already serves untouched. `src/api.rs` and `src/server.rs` do not change.

### 4.4 Web page

**Where each thing goes, and why nothing is shown twice.** Order inside the
finished-card details block:

| Block | Linux card | Sashiko card | Starts |
| --- | --- | --- | --- |
| Summary (`summaryHtml`) | absent, since summary is empty | what the change does, with no findings | open, as today |
| **Findings (N)** (new) | one line per finding, expandable | one line per finding, expandable | open when N > 0 |
| Inline Review (`inlineHtml`) | the LKML email | the `- [SEVERITY]` bullet list | collapsed when the Findings block is shown, otherwise as today |

- **Sashiko card.** The bullet list in the inline review names the same
  findings as the new block, and each bullet already names the file and
  function or symbol and says why the problem matters
  (`prompts/sashiko/github-summary-template.md`). With the inline review
  collapsed, the card opens with each finding listed once. What the block
  adds over a bullet is the line number and `why_this_location_matters`
  for every location (a bullet names one place, in prose), the severity
  reasoning split by the rubric, and the typed speculative tag. The bullets
  stay one click away as the text that was posted.
- **Linux card.** The email quotes code around each comment and is what was
  (or would be) sent. The block is the index into it, with the reasoning
  behind each level.
- **Summary.** It never lists findings, so nothing overlaps it.
- **Old reviews.** A card whose `output` has no parseable `findings` array
  renders no Findings block and keeps today's Inline Review rule. Its
  behaviour does not change (D9).

**Which findings are listed.** These are the findings the Result line
counts, so "Findings (N)" never disagrees with the counts beside it:
- On a patchset-level card (`patch_id == null`), preexisting findings are
  left out, as the counts leave them out.
- On a patch card they are listed with "(preexisting)", as the counts show
  them in parentheses.
- On reviews made after a1065edde105 (Linux) or 6cd535482699 (Sashiko),
  `output.findings` holds no preexisting entries, so the tag only appears
  on older output.
- **Malformed entries.** The stage has no validator, so `output.findings`
  can hold entries that are not objects. The block is built only when the
  counting `try` succeeded (`parsedFindings`), and it lists exactly the
  entries the counter counted:
  - A `null` entry makes the counter throw (`null.preexisting`,
    `null.severity`), so `parsedFindings` stays false and no block is
    built. The card falls back to today's Result and Inline Review rules.
  - Any other non-object entry (a string, a number) is counted by today's
    counter as Low. The list renders it as an empty finding: Low,
    "(no description)", no body. So "Findings (N)" still matches the
    Result line.
  - Every helper below reads a finding through one guard,
    `f && typeof f === 'object' ? f : {}`, so none of them can throw on
    such an entry.

**Functions** (`static/index.html`, next to `renderFindingsBadges`):

- `findingTitle(finding)` returns `{ title, clipped }`.
  - `problem` is trimmed. Empty gives "(no description)".
  - A title longer than `FINDING_TITLE_MAX_CHARS` (160 code points, so a
    surrogate pair is never split) is cut to that length, trailing space
    is trimmed, "…" is added and `clipped` is true.
  - New titles are asked to be under 80 characters, but nothing enforces
    it (D6). Under the fork's 120-character rule the same model wrote a
    median of 113 and went over in 24% of cases, with a maximum of 161
    (§1.3). So expect a minority of new titles between 80 and 120, which
    are shown whole, and the clip to fire mostly on the 207 old findings
    that never had a title. That holds once a fork database has been
    adopted (D14). Before that, the page reads only `problem`, which in
    every stored fork finding is the long argument: run over the deployment
    copies, 810 of 811 titles clip, including rows that have a headline.
  - One function decides both the row text and whether the full text goes
    into the expansion.
- `parseSeverityCalibration(text)`, adapted from the fork (D4):
  - Returns `null` for empty or whitespace text.
  - Labels: `Consequence`, `Triggering path`, `Reachability`,
    `Blast radius( and irreversibility)?` and
    `Likelihood( and preconditions)?`.
  - A label counts only in one of these exact forms. Anything else is
    prose.
    - **Spelling.** Capitalised as listed, then an optional qualifier,
      then `:`. The bold form from the Sashiko rubric is also accepted:
      `**Label**:` or `**Label:**`. The colon is required either way.
    - **Qualifier.** Either a parenthetical of 1 to 80 characters with no
      newline or nested parenthesis ("Triggering path (verified, not
      speculative):"), or an optional comma, spaces, and up to 48
      characters starting with a lowercase letter and holding no `.`,
      `:`, `;`, newline or parenthesis ("Triggering path verified in
      code:", "Consequence first:", "Reachability caps the level:"). Real
      explanations write about 60 of these (§1.3). The qualifier stays in
      the shown label, because "verified, not speculative" is information.
    - **Position.** A label opens a section only when it is at one of:
      1. the start of the (trimmed) text, optionally followed by one
         bullet marker (`- ` or `* `) or list number (`1. ` or `1) `);
      2. the start of a line, that is right after `\n`, optionally followed
         by spaces and one bullet marker (`- ` or `* `) or list number;
      3. after `.`, `;` or `)` followed by one or more spaces, optionally
         followed by a list number ("... class. 2. Triggering path:").

      A label after any other character, after unpunctuated text on the
      same line, or in lowercase is prose. "reachability is speculative"
      and "Not lowered for reachability:" are prose. A sentence ending in
      a number keeps it: in "leaves it at 1. Triggering path:" the
      boundary is the ". " after the 1, so the previous body ends "at 1".
      A `)` boundary is the exception: it closes the previous text and
      stays in it, so "crash (null deref) Triggering path:" gives the
      body "crash (null deref)".
    - As a regex, built per call, with the boundary matched and not looked
      behind:
      `(^(?:[-*][ \t]+|\d+[.)][ \t]+)?|\n[ \t]*(?:[-*][ \t]+|\d+[.)][ \t]+)?|[.;)][ \t]+(?:\d+[.)][ \t]+)?)(?:\*\*(LABELS)QUAL(?::\*\*|\*\*:)|(LABELS)QUAL:)`
      where `LABELS` is the alternation above and `QUAL` is
      `(?:[ \t]*\([^()\n]{1,80}\)|,?[ \t]+[a-z][^:;.\n()]{0,47})?`. The
      bold and plain forms are separate alternatives, so a half-bold
      `**Label:` or `Label**:` is prose. `^` is start-of-text (no `m`
      flag), so position 2 is spelled out.
    - On the 747 deployment explanations this finds all three kernel
      sections in 636 (85%), against 584 for the same rule without list
      numbers and qualifiers, and leaves no stray list number at the end
      of a body (§1.3). The shipped page code reproduces this exactly
      (636 of 747; 685 of 811 on a later copy of the databases), parses
      every named case, and throws on none. About nine explanations use a
      qualifier the rule does not accept, such as "Triggering path for the
      underlying bug:"; they render as prose, so no text is lost. The shipped page code reproduces this exactly
      (636 of 747; 685 of 811 on a later copy of the databases), parses
      every named case, and throws on none.
  - The regex is built per call, so `lastIndex` never carries over between
    findings.
  - Only the first occurrence of each label counts.
  - Returns `{ preamble, sections: [{ label, body }] }`, dropping sections
    with an empty body. `label` is the label text with its qualifier,
    without `**`, a list number or `:`. A section's body ends where the
    next label's boundary begins, so a trailing bullet marker, list number
    or newline is not carried into the previous body. The preamble and
    each body are trimmed. With no labels, the whole text becomes the
    preamble.
  - Until Q2 shows how Sashiko explanations are actually written, a Sashiko
    explanation that uses none of these forms renders as one preamble,
    which loses nothing.
- `renderSeverityCalibration(text)`: the escaped preamble, then one line per
  section. The label is in muted bold, as written in the text, followed by
  the escaped body.
- `renderFindingLocations(locations)`:
  - Each location is shown as `file:line (function_or_symbol)`, with absent
    parts dropped: `file (fn)`, `file:12`, or just `fn`. Text parts are
    trimmed and blank ones dropped. A line is shown only when it is a
    non-negative integral number up to 2^53 - 1 (`Number.isSafeInteger`),
    so a float such as 12.5, a negative, a string and a number the page
    would round are all dropped, and 12.0, which JSON parsing turns into
    12, is shown.
  - Under it, `why_this_location_matters` in muted text, escaped.
  - `code_snippet` is left out. The inline review quotes code, and a
    snippet here would bury the reasoning next to it.
  - Entries that are not objects are skipped.
- `findingTags(finding)`: an array of tag HTML.
  - "(speculative)" when `finding.speculative === true`, with the tooltip
    "The reviewer could not establish how this bug is triggered, so its
    severity is capped". Absent, `false` or a non-boolean gives no tag
    (§4.1, "not marked").
  - "(preexisting)" when `finding.preexisting === true`.
  - The tag list lives in one function so the prior-revision step can add
    its tag in one place (§4.6).
  - The speculative tag shows whatever the severity. A High finding marked
    speculative breaks the rubric, and the reader should see that (D10).
- `renderFindingsList(findings, uniqueId, { includePreexisting })` returns ''
  when there are no findings to list. Otherwise:
  - **Order.** Critical, High, Medium, Low. An unknown severity ranks as
    Low, as the counts treat it. A stable index tiebreak keeps the model's
    order within a level.
  - **Row.** Each finding is a `<details class="finding">`. Its `<summary>`
    holds the severity, then the tags, then the title:
    - The severity text is escaped. Its colour is
      `getSeverityColor(severity)`, as in the bug list (line 3207).
    - The title is escaped and its `title=` attribute escaped too.
    - The severity cell has a fixed `min-width`, so the titles line up.
  - **Body.** In order: the full `problem`, only when `clipped`; then
    `renderSeverityCalibration(severity_explanation)`; then
    `renderFindingLocations(locations)`.
  - **No body.** A finding with an empty body renders as a plain `<div>` in
    the same layout. A control that opens nothing is worse than no control.
  - **Not-marked note** (D13). When no listed finding has a boolean
    `speculative`, the list opens with one muted line: "These findings do
    not say whether they are speculative." It appears once per card, not
    per row. On a review made before this step that is every card with
    findings; on a later one it shows only if the model left the key out
    of every finding.
  - **Wrapper.** A `toggleCollapsible` block like its siblings, with the
    button "▼ Findings (N)" and `id="findings-${uniqueId}"`, and class
    `collapsible open`.

**`renderReviewCard` wiring.**
- After the counting `try`, and only when `parsedFindings` is true, build
  `findingsHtml` with `includePreexisting` set to `!isPatchset`.
  Otherwise `findingsHtml` is ''. `isPatchset` is lifted out of the `try`,
  next to `outputJson`.
- The inline review's open rule becomes
  `parsedFindings && !foundNothing && !findingsHtml`. The rule still holds
  for output with no findings array, and no case opens both blocks.
- Insert `${findingsHtml}` between `${summaryHtml}` and `${inlineHtml}`.
- **Partial cards.** The note "Any findings below come only from the stages
  that finished." sits above the details block. It now covers the Findings
  block as well, and its wording stays true.

**What the code does that the list above left open.**
- Two small helpers hold the guard and the row: `findingObject(finding)`
  (the guard `f && typeof f === 'object' ? f : {}`) and
  `renderFindingRow(finding)` (one `<details>` or plain `<div>`).
- A missing, non-string or blank severity shows as Low, so the row agrees
  with the Result line, which counts it as Low. A padded severity such as
  " High " is shown as written, in the unknown colour, because the counts,
  the ordering and the CLIs all compare the text exactly; trimming it in
  the row alone made the row say High while the Result line counted Low.
- A finding is listed by the counters' own truthiness test for
  `preexisting`, while the "(preexisting)" tag needs a JSON `true`. A
  string such as "yes" is therefore counted and listed on a patch card,
  and carries no tag.
- The closing-parenthesis rule of the label parser (§4.4, above) was added
  with the parser: a `)` boundary closes the previous text and stays in it.

**CSS.** One rule in the existing style block for `details.finding > summary`:
pointer cursor and no list marker, with the disclosure triangle drawn as
`▶`/`▼` to match the page's other toggles. Plus spacing for the body.

### 4.5 CLI

**A shared, typed view of one finding.** A new module, `src/finding_view.rs`,
unconditional so it builds under `--no-default-features`, exported from
`lib.rs`:

```rust
/// How long a finding title may be before a one-line view clips it. Matches
/// FINDING_TITLE_MAX_CHARS in static/index.html.
pub const TITLE_MAX_CHARS: usize = 160;

/// What a one-line view shows for one finding of a review's output JSON.
pub struct FindingLine {
    pub severity: Severity,       // db::Severity; Unknown for anything else
    pub title: String,            // problem, clipped at TITLE_MAX_CHARS
    pub speculative: Option<bool>, // None: absent or not a JSON boolean
    pub preexisting: bool,        // only a JSON true counts, as in the counts
    pub location: Option<String>, // first location naming a file or function
}

impl FindingLine {
    /// A non-object finding reads as an empty one: Unknown, "(no
    /// description)", nothing else. Title and location text are made safe
    /// for one terminal line (see below).
    pub fn from_json(finding: &serde_json::Value) -> Self;
}

/// True when no finding carries a boolean speculative key (§4.1, D13).
pub fn none_marked_speculative(findings: &[serde_json::Value]) -> bool;

/// The D13 note both CLIs print, and the function that prints it as one dim
/// line when the list is non-empty and none of its findings is marked.
pub const NOT_MARKED_NOTE: &str = "These findings do not say whether they are speculative.";
pub fn write_not_marked_note(out: &mut impl WriteColor, findings: &[serde_json::Value]) -> io::Result<()>;
```

What the code does that this sketch left open:

- **Severity** is read by the rule the CLIs' counts and exit code already
  use: one of the four names, compared without ASCII case and without
  trimming; anything else is `Unknown`. `db::Severity::from_str` is not
  used because it also accepts prefixes such as "High (capped)", which the
  counts skip, so a label read that way could print a red `[High]` for a
  finding no count includes and that does not fail the exit code.
- **Safe for one line.** Model text goes to a terminal, so the title and the
  file and function of a location have every control character (newline,
  tab, ESC), line or paragraph separator, zero-width or directional mark
  (U+200B to U+200F) and bidirectional embedding, override or isolate
  (U+202A to U+202E, U+2066 to U+2069) replaced by a space, and are
  trimmed, before the title is clipped. One finding is always one printed
  line, no escape sequence reaches the terminal, and a path cannot be made
  to read differently from what it is.
- **Line number.** The same rule as the page (§4.4): a non-negative
  integral number up to 2^53 - 1, whether written as an integer or as a
  float such as 12.0. A string, a negative, a fraction and anything larger
  are dropped.
- **Location.** The first entry that names a file or a function, not
  literally the first entry.
- **Order.** Findings print in stored order, not worst first; the page
  does the ordering.
- **Dropped output.** The old `sashiko-cli` loop also printed
  "Fix: <recommended_fix>". No workflow writes that key, so nothing a
  reader could see is lost.

`speculative` keeps "not marked" (`None`) apart from `Some(false)`, so a
later step that reuses `FindingLine` to store or summarise findings cannot
turn an unmarked finding into a verified one. Only the printer collapses
the two, as the web page does.

- `src/main.rs::print_finding` (`sashiko review`) builds a `FindingLine` and
  prints `[Severity] title`. It adds the tags, in the web's order and
  wording: ` (speculative)` for `Some(true)` and ` (preexisting)` when
  `preexisting` is true. On the next line it prints the location, indented.
  The preexisting tag matters in `sashiko-cli`'s patch mode, which lists
  preexisting findings and counts them separately in parentheses; without
  the tag a reader could not tell which lines those counts mean.
- When `none_marked_speculative` holds for the listed findings, both CLIs
  print one dim line after the list, "These findings do not say whether
  they are speculative.", as the web does (D13).
- `src/bin/sashiko-cli.rs::print_findings_summary` replaces its per-finding
  loop with the same printer. The printer moves into `finding_view` as
  `pub fn write_finding_line(out: &mut impl WriteColor, line: &FindingLine)`
  so both binaries share it. The reads of `file`, `line`,
  `problem_description` and `recommended_fix` go away; no workflow emits
  those keys.
- Severity colours stay as they are in both binaries: red for
  Critical/High, yellow for Medium, cyan for Low.
- Both CLIs still print the inline review after the list. In a terminal
  nothing can be collapsed, so the titles appear in both places for a
  Sashiko review. That is the price of a list a reader can scan; the
  bullets already name file and symbol, and the list adds the line number
  and the tags (D11).
- `sashiko-cli` does not print the severity reasoning. It is in `output`,
  and `--format json` users have it.

### 4.6 Interaction with the prior-revision port (5cf550d2bec6)

That later step injects an earlier revision's findings into verification and
report, tags repeats, and stores `findings.prior`. What it inherits from
this step:

- **No `headline`.** Wherever the fork uses `headline`, the port uses
  `problem`:
  - the de-duplication key in `get_prior_revision_findings`, compared
    trimmed and case-folded;
  - the compact summary line;
  - the tag. The fork's nested key was `headline`; on the port it becomes
    `"prior": {"relation", "revision", "earlier_problem"}`, not
    `"problem"`, so a finding never carries two keys named `problem` with
    different meanings ("Unambiguous Naming").

  The fork sent `problem` only when `headline` was missing. That fallback
  goes away. On an adopted fork database, `problem` already holds the
  stored headline (D14); the 207 findings that never had one go through
  the `TITLE_MAX_CHARS` clip, applied in Rust with `finding_view`'s rule.
- **De-duplication by title will rarely merge repeats.** In the
  deployments, an earlier finding's stored `prior.headline` equals the
  repeating finding's own headline in only 36 of 179 cases: the model
  rewords a repeat four times in five. The prior step should treat the
  exact-title key as a guard against literal duplicates only, not as a way
  to recognise the same defect across revisions.
- **Legacy `prior` JSON.** All 179 stored `prior` values are `{headline,
  relation, revision}` (160 repeats, 19 fix regressions). The adoption
  step (D14) renames `headline` to `earlier_problem` in them, so the prior
  step reads one key.
- **The chain explanation does not go in `problem`.** The fork's REPEAT and
  FIX REGRESSION rules tell the model to write the story into `problem`
  ("Say in \"problem\" that it was reported on an earlier revision ...";
  "In \"problem\", name the earlier finding and explain how the change
  made for it causes this one"). Fork `problem` was a long argument, so
  that fit. On the port, `problem` is the under-80-character title that
  the Findings row, the CLI line and the de-duplication key all rely on.
  The prior step must send that narrative to `severity_explanation`
  instead, and leave `problem` as the title. The relation itself is shown
  by the `prior` tag ("(repeat of r3)"), so the title does not need to say
  it.
- **Migration.** That step is the first on the port to need schema
  changes from 5cf550d2bec6, and the fork made three, all of them absent
  from `src/migrations/` 001-016: `patchsets.mr_body TEXT`,
  `CREATE INDEX idx_patchsets_mr_url ON patchsets(mr_url)` and
  `findings.prior TEXT`. They become one migration,
  `017_prior_revision_findings.sql`, the next free number (this step adds
  none). If that step splits the pull-request body into its own commit,
  it splits the migration too: `017_patchsets_mr_body.sql` (column and
  index) and `018_findings_prior.sql`.
- **Cutting the consequence.** The fork cuts the explanation at
  "Triggering path:". The port needs a Rust splitter that uses this step's
  label rule (§4.4), with the same label list, including the Sashiko
  headings, so a Sashiko-project prior summary is cut at "Likelihood". It
  belongs in `finding_view`, with tests that use the same cases as the UI
  verifier.
- **`speculative`.** If the prior summary should say an earlier finding was
  speculative, that step adds a nullable `findings.speculative` column and
  writes it in `store_review_findings` from the JSON as `Option<bool>`:
  NULL for "not marked", never `false` (§4.1). This step stores nothing
  (§4.2).
- **Web.** The fork's prior step did not touch the page. On the port, a
  `prior` relation becomes one more entry in `findingTags`, such as
  "(repeat of r3)" or "(caused by the fix for r3)".

## 5. Type-Driven State

| Concept | Representation | Replaces |
| --- | --- | --- |
| Speculative finding | `speculative: bool` in the verification output, written by the model; `Option<bool>` wherever it is read back, `None` meaning not marked | regex over `severity_explanation` |
| Finding title | `problem` (upstream's short title) | fork `headline` column and field |
| Severity in a CLI line | `db::Severity`, read by the counts' own rule (one of four names, ASCII case ignored, no trimming) | raw string match per binary |
| One finding's CLI line | `finding_view::FindingLine` | two hand-rolled readers, one reading keys nobody emits |

The page is JavaScript and reads JSON, so the type there is the shape
`findingTitle`, `findingTags` and `renderFindingsList` agree on, and the UI
verifier tests it.

## 6. Testing Plan

Run `make check-pr` from a throwaway clone (AGENTS.md, Testing). After the
web commits, run the UI verifier (`scripts/verify_stage_progress_ui.sh`)
and the ignored server tests, as the port plan asks after every step.
Commits 2 and 3 also need the review-output validation in §6.5.

### 6.1 Prompts (`src/workflows/*.rs` tests)

- `linux_patch_review.rs`,
  `verification_key_list_and_example_name_the_same_keys`. This adapts the
  fork's test. It renders
  `verification_stage(10, 1.0).user_prompt.render_for_log(&LinuxPatchReviewState::default())`
  and cuts out two spans:
  - the **key list**: from "Each object in the 'findings' array MUST use
    exactly the following keys:" up to "Example Output:". The text before
    it is not used, because instruction item 6 already says
    `"preexisting": true`, which would satisfy the check whatever the key
    list says;
  - the **example**: the fenced `json` block after "Example Output:".

  In the key-list span each key must appear in its key-list form, `"key" (`
  (the quoted name followed by its parenthesised definition). In the
  example each must appear as `"key":`. The keys are `"problem"`,
  `"speculative"`, `"severity"`, `"severity_explanation"`, `"preexisting"`
  and `"locations"`. The doc comment gives the fork's reason: the stage has
  no validator, so a key that drifts out of one half silently stops being
  emitted.
- `linux_patch_review.rs`, `verification_asks_for_speculative_before_severity`.
  In both spans, `"speculative"` comes before `"severity"`.
- `linux_patch_review.rs`, `verification_example_labels_its_severity_reasoning`.
  The example span (the fenced block only, so the rubric file and the
  instructions cannot satisfy it) contains "Consequence:", "Triggering
  path:" and "Reachability:", in that order.
- `sashiko_patch_review.rs`, `verification_asks_whether_a_finding_is_speculative`.
  The key list names `"speculative" (`, before `"severity" (`.
- In each workflow, a reduce test next to the existing preexisting tests
  (`linux_patch_review.rs:1971`, `sashiko_patch_review.rs:1402`). A finding
  with `"speculative": true` reaches `state.findings` with the key intact.
  A preexisting one still becomes a concern.

### 6.2 `src/finding_view.rs`

- A short `problem` is used as it is. An empty one gives "(no description)".
- A 400-character `problem` is clipped to 160 code points plus "…". A
  multibyte character at the cut is not split.
- Severity: "critical", "High" and "MEDIUM" parse. "bogus" and a missing
  severity give `Unknown`.
- `speculative`: JSON `true` gives `Some(true)`, JSON `false` gives
  `Some(false)`. Absent and the string `"true"` give `None`.
- `preexisting`: only JSON `true` gives true.
- A non-object finding (`null`, a string) gives Unknown, "(no
  description)", `None`, false, `None`.
- `none_marked_speculative`: true for `[{}, {"speculative": "true"}]`,
  false as soon as one finding has `"speculative": false`.
- Location: `file:12 (fn)`, `file (fn)`, `file:12` and `fn`. A non-object
  location or an empty array gives `None`.
- `write_finding_line` into a `termcolor::Buffer::no_color()` prints
  `[High] title (speculative)` and the indented location line;
  `[Low] title (preexisting)` for a preexisting one; `Some(false)` and
  `None` both print no tag.

### 6.3 UI verifier (`scripts/verify_stage_progress_ui.mjs`)

Run it through `scripts/verify_stage_progress_ui.sh`.

Add `findingTitle`, `parseSeverityCalibration`, `renderSeverityCalibration`,
`renderFindingLocations`, `findingTags`, `renderFindingsList` and
`getSeverityColor` to `NAMES`. `getSeverityColor` is a top-level page
function (`static/index.html:2406`) that `renderFindingsList` calls; it is
not in `NAMES` today, and `load()` defines only what `NAMES` lists, so
without it every card with findings would throw a ReferenceError inside
`guard()`, including the existing `partialChecks` card. It is also what
the D8 mutation is measured against. `toggleCollapsible` is only named
inside `onclick` strings, so it is not needed.
`FINDING_TITLE_MAX_CHARS` is a single-line module `const`. The harness has
only `grabLet` today, so add a `grabConst` twin and a `CONSTS` list, loaded
by `load()` the same way. Add a `findingChecks(fns)` group.

Parsing:
- With all three kernel labels, the sections come out in rubric order, and
  each body stops at the next label.
- The two Sashiko headings are found, in the plain form and in the bold
  bullet form the Sashiko rubric uses:
  "- **Blast radius and irreversibility**: ...\n- **Likelihood and
  preconditions**: ...". The bodies carry no `**`, `-` or newline.
- Labels on their own lines, "Consequence: a.\nTriggering path: b.", give
  two sections.
- "Consequence: nothing, because reachability is speculative here." gives
  one section.
- Real shapes from the deployments (§1.3):
  - a numbered list, "1. Consequence: a.\n2. Triggering path: b.\n3.
    Reachability: c.", gives three sections, an empty preamble, and no
    "2" or "3" at the end of a body (kernel-fork #35, driver-a #59);
  - the same list on one line, "x. 1. Consequence: a. 2. Triggering path:
    b.", gives two sections;
  - "Triggering path (verified, not speculative): b" and "Triggering path
    verified in code: b" open a section whose label keeps the qualifier;
  - "Consequence first: a" opens a section (driver-a #153, which the
    first draft's rule parsed into no sections);
  - ".  Triggering path: b" (two spaces) and "\n\nReachability: c" (after
    a blank line) open sections;
  - "it leaves it at 1. Triggering path: b" keeps "at 1" in the previous
    body (driver-a #42; the fork's parser dropped the 1).
- Prose does not open a section: "Not lowered for reachability: x" (the
  fork parser's false match, driver-a #182), a lowercase "consequence:
  x" after ". ", a label after unpunctuated text on the same line ("the
  Reachability: x"), and a half-bold "**Consequence: x".
- An unlabelled "1. Condition Y is met.\n2. ..." becomes the preamble.
- Whitespace gives `null`.
- Three calls in a row give the same result, so there is no `lastIndex`
  state.

Title and tags:
- A 70-character problem is shown as it is and is not repeated in the body.
- A 400-character problem is clipped, ends in "…", and its full text is in
  the body.
- `speculative: true` tags the finding. `false`, absent and `"true"` do not.
  `preexisting: true` gives "(preexisting)".
- The not-marked note appears once when no finding has a boolean
  `speculative`, and not at all when one finding has `"speculative":
  false`.

Card:
- "Findings (3)" appears, listed severest first, with equal severities
  keeping their order.
- The Findings block starts open. When it is shown, Inline Review starts
  collapsed (`▶ Inline Review`).
- A patchset-level card leaves preexisting findings out, and its count
  matches the Result line. A patch card lists them.
- A finding with no body gets no `<details>`.
- No findings, unparseable output and output with no `findings` array each
  give no block. In the last two cases the Inline Review rule is unchanged.
- Malformed entries: findings `[null, {severity: "High", problem: "x"}]`
  render the card without throwing, with no Findings block and today's
  fallback Result. Findings `["junk", {severity: "High", problem: "x"}]`
  render "Findings (2)", with the string listed as a Low "(no
  description)" row, matching the Result line's Low 1 and High 1.
- HTML in problem, explanation, location and `why_this_location_matters` is
  escaped.
- A severity of `low;" onmouseover="x` is neither injected into a style
  attribute nor coloured. It gets `--sev-unknown` (D8).
- A location renders as `f.c:12 (fn)`.

Existing checks:
- "card: a Partial review that found something opens it" becomes "opens its
  findings".
- The `openOnNothing` mutation is retargeted at the new open rule.

Mutations, each of which must fail `findingChecks`:
- ranking by index only;
- dropping the clip;
- `escapeHtml` replaced by identity in the row;
- the colour taken from `var(--sev-${level})` instead of
  `getSeverityColor`;
- the inline review opened alongside the findings;
- `includePreexisting` ignored;
- the speculative tag read from `severity_explanation`;
- the findings block built outside the `parsedFindings` guard (caught by
  the `[null, ...]` card);
- the not-marked note shown per row instead of once;
- the list-number boundary removed (caught by the numbered list);
- the qualifier removed from the label pattern (caught by "Triggering path
  (verified, not speculative):");
- the label match made case-insensitive (caught by the lowercase
  "consequence:" after ". "; "Not lowered for reachability:" has no
  boundary before its label, so it is prose with or without the flag);
- the closing parenthesis dropped from a body before a same-line label
  (caught by "crash (null deref) Triggering path:").

### 6.4 By hand

On the demo setup (`/tmp/sashiko-demo`, or a stub-model daemon), look at
three cards:
- a Linux review with three findings;
- a Sashiko review whose inline review is the bullet list;
- an old review whose `output` has long `problem` text and no
  `speculative` key (the demo's PR 968 data, copied from driver-a
  before any adoption, is one).

On each, check that every finding is listed once on load and that the
inline review is one click away.

### 6.5 Review-output validation for commits 2 and 3

Commits 2 and 3 change the Linux verification prompt, which every Linux
review runs. `prompts/sashiko/severity.md` rates a change like that High
when its commit message has no validation evidence, and `speculative`
carries the rubric's cap, so it can move severities and not only add a
tag. So these two commits are measured before they land.

The user approved a targeted re-review and its model spend on 2026-10-05
(Q4); no benchmark suite is run.

- **Patches.** Six small, already reviewed deployment patches whose stored
  reviews have findings, chosen so the sample holds the shapes this step
  is about:

  | DB, review | Commit | Why |
  | --- | --- | --- |
  | driver-b r22 | 542040c9f230 | consequence-only speculative (#88, #89); cheapest, so it is the pilot |
  | driver-b r39 | 57e783f8153a | a partly speculative High (#182) |
  | kernel-fork r3 | adbedfa045e0 | mentions speculative; numbered labels (#1) |
  | kernel-fork r12 | cd0b3a1a0f31 | numbered and qualified labels |
  | driver-a r225 | 939ac8077af0 | a 2-line diff with a preexisting High |
  | driver-a r190 | 70982b614610 | numbered labels (#59-62) |

- **Settings, as the deployments run them:** provider `claude`, model
  claude-opus-5 with no extended thinking, `max_input_tokens` 40000,
  `[ai.claude] max_tokens` 64000 and prompt caching, `max_interactions`
  150 for kernel-fork and 100 for the others, and a review timeout of
  21600 seconds. `sashiko review --format json`, not `--agent`, because
  `--agent` skips the report stage that Q3 compares. Kernel references are
  not configured; that gap is the same in every arm.
- **Arms.** One throwaway clone builds three binaries: the commit before
  2 (base), 2 and 3.
  - **Noise.** The same patch reviewed six times by the deployment came
    back with six different sets of findings, so one fresh run per arm
    cannot tell a prompt effect from chance.
  - **Replay.** The base arm runs once in full with the response cache on.
    The c2 and c3 arms reuse that cache. Every stage before verification
    sends byte-identical requests and replays from it, so only
    verification and report, whose prompts changed, call the model. All
    three binaries load one fixed prompts directory, so the bundle path
    cannot break the replay. The pilot patch is run first, and its c2 log
    must show cache hits for the analysis stages before anything else
    runs.
  - **A/A control.** A copy of the cache without its verification and
    report entries is run again on the base binary. This gives a second
    base sample of the two changed stages, which measures their noise
    floor.
  - Expected cost, by the stored token counts: about $165 for the base
    arm and $17-35 for each of the other three, $215-270 in all.
- **What is compared**, each arm against the one before and against the
  A/A spread. The stored fork reviews are not a baseline: the port routes
  preexisting findings to concerns and has no prior-revision stage.
  - the severity distribution (Critical/High/Medium/Low counts);
  - after commit 3, the share of findings with `"speculative": true`, the
    share with the key absent, and every finding that is both speculative
    and above Medium (a calibration error the D10 tag would show);
  - after commit 3, the level of the findings that match the stored
    consequence-only and partly speculative ones (§4.1);
  - after commit 2, the share of explanations the §4.4 rule splits into
    all three sections, and how many use the numbered hybrid or a
    qualified label;
  - the length of `problem`: how many are 80 characters or less, carry a
    subsystem prefix, or contain backticks;
  - the report: the LKML emails, read for wording that changed because of
    `speculative`, and the count of `[Severity: X]` tags against the
    findings (Q3).
- **Gate.** The results go into the body of commits 2 and 3. Commit 3
  does not land until the report comparison (Q3) is done. If it shows
  wording nobody wants, commit 3 also adds one line to each report
  instruction, and the comparison is repeated for that line.

### 6.5.1 Results (2026-10-06)

Run as planned, on the five of the six patches that reach verification
(the sixth, a driver-a XSK pool patch, had no findings in the base run, so
verification never ran on it). Every run replayed all turns before
verification from the cache, 18 of 18 first runs and 75 of 75 repeats, so
each difference below comes from the verification and report stages alone.
The old prompt has 7 samples (the base build once, the A/A control once, and
five A/A repeats), commit 2 has 6 and commit 3 has 6; one sample is the five
patches reviewed once, and a repeat starts from a fresh copy of the cache
without its verification and report rows. Cost: about $246 at Opus 5 list
price, of which the single full base arm was $150.

| Per sample (5 patches) | old prompt | commit 2 | commit 3 |
| --- | --- | --- | --- |
| findings | 15.4 +- 1.6 | 16.0 +- 1.5 | 15.3 +- 1.4 |
| High | 1.4 +- 0.5 | 1.2 +- 0.4 | 1.3 +- 0.7 |
| Medium | 4.7 +- 0.9 | 4.5 +- 1.0 | 5.0 +- 1.2 |
| Low | 9.3 +- 1.2 | 10.3 +- 2.3 | 9.0 +- 1.0 |
| explanations fully split (§4.4) | 54% | 85% | 73% |
| numbered "1. Consequence:" hybrids | 12% to 29% of findings | none | none |
| speculative true / false / absent | 0 / 0 / all | 0 / 0 / all | 25 / 67 / 0 |
| speculative true above Medium | 0 | 0 | 0 |
| median length of problem | 88.5 | 91.5 | 96.5 |
| problem 80 characters or less | 20% | 15% | 15% |

What this shows, and how sure it is:

- **Commit 2 does what it was for.** The share of explanations that split
  into all three rubric sections rose from 54% to 85% (permutation test
  p = 0.005), and the numbered hybrids that every old sample contained
  (2 to 4 of 13 to 18 findings) did not occur in any of the 12 new ones.
  Commit 3 sits between (73%, p = 0.058 against the old prompt, and
  p = 0.086 against commit 2), so its gain is smaller but is not shown to
  differ from commit 2's.
- **Severity did not move.** Finding counts and the High, Medium and Low
  counts differ by less than the spread between identical builds, and
  High plus Medium per sample is 6.1 old and 6.3 commit 3 (p = 0.84). The
  one High that looked suppressed in a first single comparison (a NULL
  dereference reached from ethtool on a driver-a patch, reported by both
  of the first two old runs and by neither new run) turned out to be
  noise: it appears in 3 of 7 old samples and in 2 of 12 new ones. The
  other High, on a driver-b patch, appears in every sample of every arm.
  The sample is small, so a shift smaller than about one finding per
  sample cannot be excluded.
- **`speculative` is emitted reliably and calibrated.** It is present on all
  92 findings in the commit 3 samples (25 true), and no finding marked
  speculative is above Medium. The consequence-only and partly speculative
  cases of §4.1 are too rare to measure separately here; the High plus
  Medium counts show no sign that the definition lifts them.
- **The reports did not change.** The word speculative appeared in 0.3
  old reports per sample and 0.2 commit 3 reports, and no finding left the
  email, so Q3 needs no report instruction line. Reports carry more
  [Severity] tags than the finding count in every arm, including the old
  one, because a finding anchored at two code sites gets a comment at
  each.
- **A small cost in problem length.** The median problem is 8 characters
  longer under commit 3 (p = 0.004), while the share at 80 characters or
  less (the rule's limit) is 20% before and 15% after. The model does not
  keep to the 80-character rule in any arm: only 15% to 20% of titles
  do, and 93% to 98% are under 120. Only a validator, which D6 leaves to its
  own item, would enforce it.
- **Limits.** Five patches and about 95 findings per arm, one model, and
  no kernel reference tree (the same in every arm). The replay design
  measures the two changed stages and not a full re-review, which is the
  point, and a full-review effect from a different analysis sample is not
  in these numbers.

**Setup lesson.** A first attempt replayed nothing: the toolbox emitted its
tool declarations in hash order, so two runs of one request differed and
the response cache never hit. The measurement builds sort them
(a one-line change in `ToolRegistry`, with a test); it is not part of this
step. Two separate processes differed on the same request, so on this
evidence a response cache cannot replay across process runs, which a
deployment's cache would also need to; whether a retry inside one process
differs too was not measured.

## 7. Commit Breakdown

Each commit is one consistent change. Each has `Signed-off-by`, body lines of
72 characters or fewer, and no backticks. `make check-pr` passes before each
commit.

1. **designs: plan the port of the per-finding display.** This document.
2. **review: show the severity rubric labels in the verification example.**
   The Linux example's `severity_explanation` (D12), the key-list/example
   agreement test (without `speculative` yet), and the labels test. The
   body records the §6.5 results for this commit.
3. **review: say whether each finding is speculative.**
   - The `speculative` key in both verification prompts, before
     `severity`, and in the Linux example.
   - The agreement test extended to it, the ordering test, the Sashiko
     prompt test, and both reduce tests.
   - `designs/MULTI_STAGE_REVIEW.md` Stage 10.
   - The body records the §6.5 results, including the report comparison
     (Q3), which gates this commit.
4. **cli: print what each finding is and where.**
   - `src/finding_view.rs` and its tests.
   - `print_finding` in `main.rs`, and the per-finding loop of
     `print_findings_summary` in `sashiko-cli.rs`, with the
     "(speculative)" and "(preexisting)" tags and the not-marked line.
   - The AGENTS.md Project Map line for the new module.
5. **web: list each finding on its review card.**
   - `findingTitle`, `renderFindingLocations`, `findingTags` with
     "(preexisting)" only, `renderFindingsList`, the CSS and the
     `renderReviewCard` wiring, including the `parsedFindings` guard and
     the inline review's new open rule. The body shows
     `severity_explanation` as one escaped block.
   - The verifier: `getSeverityColor` and the new functions in `NAMES`,
     `grabConst`, the checks for these, the malformed-entry checks, and
     the two reworked existing checks.
6. **web: split a finding's severity reasoning into its rubric steps.**
   `parseSeverityCalibration`, `renderSeverityCalibration`, and their
   verifier checks and mutations.
7. **web: mark findings the reviewer could not fully verify.** The
   "(speculative)" entry in `findingTags` and the once-per-card
   not-marked note (D13), with their checks and mutations.

Commits 2-3 come first so the field exists before anything reads it. Commit
4 is independent of 5-7. Commit 5 alone already does what the fork set out
to do; 6 and 7 each add one thing a reader can see.

## 8. Open Decisions

Each decision records the chosen option. The user approved D1-D13 as
recommended on 2026-10-05 and chose D14 the same day.

**D1. A separate headline.**
- (a) Drop `headline`. `problem`, upstream's under-80-character title, is the
  row title. Stored fork headlines become `problem` when a fork database
  is adopted (D14), and text with no title is clipped on display.
- (b) Port `headline` as the fork had it: a prompt key and migration 017.

*Chosen: (a).* Upstream's title rule asks for what the fork's `headline`
asked for, and more strictly: short, on its own line, and naming the root
cause. Two such fields would have no single reasonable reading
("Unambiguous Naming"), and nothing would read one that the other does not
already say ("No Dead Outputs"). The fork's reason for `headline`, a long
`problem`, no longer exists for reviews made on the port. It does exist
for the 747 stored deployment findings (§1.3), which is what D14 is for.

**D2. Where the argument goes now that `problem` is short.**
- (a) The body shows `severity_explanation`, plus `why_this_location_matters`
  for each location.
- (b) Also add a `description` key to verification, which brings back the
  fork's long problem.

*Chosen: (a).* Upstream deliberately put the argument into
`severity_explanation`, which instruction item 7 (SPECIFICITY REQUIREMENT)
asks to name the function, file and trigger. `why_this_location_matters`
is already written per location. (b) would add a third text field that
overlaps both.

**D3. How "speculative" is known.**
- (a) A typed `speculative` boolean from verification.
- (b) The fork's regex over `severity_explanation`, limited to Low and
  Medium.
- (c) (a), and fall back to (b) when the key is absent.

*Chosen: (a).* On the deployments' Low and Medium findings, (b) is wrong
about one time in ten, in both directions (89% precision, 84% recall), and
it cannot tell a speculative cap from the 23 caps the model gives for
other reasons (§1.3). Both rubrics already ask the model to "mark" it. (c)
would keep the guesser alive only for old rows, which would then disagree
with new ones in how they decide.

**D4. Which labels the explanation is split on.**
- (a) One list holding both rubrics' headings (Consequence, Triggering path,
  Reachability, Blast radius, Likelihood), with the case-sensitive,
  boundary-anchored rule of §4.4, which also accepts a list number at a
  boundary and a short qualifier before the colon. The user chose to
  accept those two forms on 2026-10-05, after the deployment data showed
  them in about 75 explanations (§1.3).
- (b) Only the kernel labels, as in the fork. Sashiko explanations then
  render as one block.
- (c) (a), and also tell the Sashiko verification stage to lead with literal
  "Blast radius:" and "Likelihood:" labels.

*Chosen: (a).* It costs two more label alternatives, plus the bold and
start-of-line forms that the Sashiko rubric's own layout suggests
(`- **Blast radius and irreversibility**: ...`, §4.4). Whether the Sashiko
model writes any of those forms is unknown: the stored data is all kernel
reviews (§1.3). Until Q2 answers it, a Sashiko explanation is expected to
render as one preamble at worst, which loses nothing. (c) changes Sashiko
review output on a guess. There are no stored Sashiko-project explanations
to measure against yet (Q2).

**D5. A worked example for the Sashiko verification stage.**
- (a) None in this step.
- (b) Add one that mirrors the key list.

*Chosen: (a).* The key-list/example agreement test exists to stop the two
drifting apart. With no example there is nothing to drift. An example would
be a prompt change to self-review output that this step does not need. The
Linux example is treated differently (D12) because it exists already and
contradicts its own instructions; Sashiko has no example to contradict
anything.

**D6. A validator for verification findings.**
- (a) None in this step. A missing or non-boolean `speculative` reads as
  "not marked" (§4.1): no tag on the row, and the once-per-card note when
  no finding is marked (D13).
- (b) Add a `text_with_validator`-style JSON validator that requires each
  key, a known severity, a boolean `speculative`, and no `speculative`
  above Medium, with feedback to the model.

*Chosen: (a)* for this step. A missing key costs one tag. A validator
touches every review's retry behaviour and both workflows, and deserves its
own item, which would also enforce upstream's under-80-character title rule.

**D7. Row expansion.**
- (a) `<details>` per finding.
- (b) The fork's table with hidden `<tr>` rows and `toggleFindingReasoning`.

*Chosen: (a).* It is native, keyboard-accessible, and needs no JavaScript
toggle. The card's stage table already uses `<details>`. (b) needs a
special toggle because `.collapsible` would pull a `<tr>` out of its table.

**D8. Severity colour.**
- (a) `getSeverityColor(severity)`, the same whitelist as the bug list.
- (b) `var(--sev-${level})`, as in the fork.

*Chosen: (a).* (b) puts model-written text into a `style` attribute
without escaping. A severity such as `low;" onmouseover="..."` would break out
of the attribute. (a) can only produce one of five fixed values.

**D9. Which block starts open.**
- (a) Findings open. Inline Review collapsed whenever the Findings block is
  shown, and today's rule otherwise.
- (b) Findings collapsed and Inline Review open, as in the fork.
- (c) Both open.

*Chosen: (a).* It is the only option that lists each finding once on load
on a Sashiko card, where the inline review is itself a list of the
findings. The bullets already name file and symbol and say why each
problem matters; what the open block adds is the line numbers,
`why_this_location_matters` per location, the rubric-split severity
reasoning and the typed speculative tag (§4.4). (b) hides the new block
behind a click and leaves the Linux reader with only the long email. (c)
shows the Sashiko findings twice.

**D10. The speculative tag on High or Critical.**
- (a) Show it whenever `speculative === true`.
- (b) Show it only on Low or Medium, as the fork's gate did.

*Chosen: (a).* The fork's gate protected a text guess from "not capped as
speculative". With a typed field, a High finding marked speculative is the
model breaking the rubric's cap. Hiding that would hide a calibration
error the reader should see.

**D11. Fix `sashiko-cli`'s per-finding lines in this step.**
- (a) Yes, through the shared `finding_view`.
- (b) Leave it as an upstream bug.

*Chosen: (a).* It is the terminal half of Goal 1. It prints a bare
`[High]` per finding today. `main.rs` already fixed the same mismatch on its
own, so sharing one printer removes the duplicate reader too.

**D12. The Linux verification stage's worked example.**
- (a) Keep it as it is: `"1. Condition Y is met.\n2. ..."`, as upstream and
  the fork both have it.
- (b) Relabel it: "Consequence: ... Triggering path: ... Reachability:
  ...", and (commit 3) add `"speculative": false` before `"severity"`.

*Chosen: (b).* The example contradicts instruction item 5 and the kernel
rubric, which both ask for consequence, triggering path and reachability
at the start of `severity_explanation`. A model shown one shape and told
another is being asked an ambiguous question. Unlike D4 (c) and D5, this
corrects a prompt that already exists rather than adding one on a guess.
It does change Linux review output, which is why §6.5 measures it before
it lands. If (a) is chosen, nothing breaks: the parser renders unlabelled
text whole, and §1.3 shows the model mostly writes the labels anyway.

**D13. How a card shows findings that do not say whether they are
speculative.**
- (a) Like `false`: no tag, nothing else. Goal 3 is narrowed to reviews
  whose findings carry the key.
- (b) No tag per row, plus one muted line on the Findings block (and one
  dim line in the CLI) when no listed finding has a boolean
  `speculative`: "These findings do not say whether they are
  speculative."
- (c) A "(not marked)" tag on each such row.

*Chosen: (b).* With (a), every card stored before this step looks like
a card whose findings were all checked and found solid, which is the
confusion Goal 3 exists to remove. (c) puts the same tag on every row of
every old card, which is noise. (b) says it once, costs one line of
markup and one verifier check, and stays silent on new reviews where the
model marked at least one finding.

**D14. Titles of stored fork findings.** The deployments keep their
databases (§1.3). 540 of their 747 findings have a good title in
`findings.headline` and in the stored output JSON's `headline` key, and a
long argument in `problem`.
- (a) The fork-database adoption step rewrites them once. For each finding
  whose `headline` is a non-empty string, in the `findings` row and in the
  stored review output JSON (`ai_interactions.output_raw`, which is what
  the page and the CLIs read):
  - `problem` becomes the headline;
  - `severity_explanation` becomes the old `problem`, a blank line, and the
    old `severity_explanation`, so no text is lost and the argument shows
    as the preamble above the rubric sections;
  - the `headline` key is removed from the JSON.

  The same step renames the `headline` key inside each stored `prior`
  value to `earlier_problem` (§4.6). The 207 findings with no headline are
  left as they are and get the display clip.
- (b) Display code reads a non-empty `headline` key before `problem`, in
  `findingTitle` and `FindingLine::from_json`. No data changes.
- (c) Nothing: old cards show `problem` clipped at 160 characters, which
  on 746 of 747 findings is a mid-sentence fragment of code argument.

*Chosen: (a)*, the user's choice. The display code then has one notion of
a title, the one new reviews write, and the step that already has to
reconcile the fork's schema with the port's does it once. Two costs,
recorded so the adoption step does not rediscover them:
- 8 of the 540 old `problem` texts contain a rubric label at a boundary,
  so once moved in front of the explanation, their sections split at the
  wrong label. All the text is still shown.
- The output JSON is the worker's review result, not the model's
  transcript, which stays in `reviews.logs` unchanged, so rewriting it
  keeps what the model actually said on record.

This step builds none of it; it is written down here because this step
is what makes it necessary.

### Open questions

- **Q1. Step numbering.** Resolved: `DESIGN_PORT_FORK_FIXES.md` is
  retitled "Port Step 3b" in its own docs commit, to match the plan.
- **Q2. Sashiko label shape.** Once self-reviews have run under this step,
  check how many Sashiko `severity_explanation` values open with "Blast
  radius:" or "Likelihood:". If few do, revisit D4 (c).
- **Q3. Does `speculative` change the reports?** Resolved by §6.5.1: no.
  No new wording and no finding dropped from the email, so commit 3 adds
  no report instruction line.
- **Q4. Which §6.5 run.** Resolved on 2026-10-05: the targeted
  re-review on deployment patches, with its model spend approved.

## Feynman Summary

The fork wanted each finding on the review page to show a one-line name, its
severity, and, on a click, why it got that severity. Its findings had no short
name, so it invented a `headline` field.

Upstream has since made the existing `problem` field that short name. So the
port drops `headline` and needs no database change for new reviews. The
deployments' stored findings still have the long text in `problem` and the
name in `headline`; when their databases move to the port, a later step
swaps the name into `problem` and keeps the long text in the reasoning, so
the page only ever has one kind of title to show. The page lists every
finding, worst first, by `problem`. Each one opens to show its severity reasoning,
broken into the rubric's steps, and the places in the code it points at.
The rule that finds those steps was checked against 747 real explanations
and accepts the numbered and annotated forms the model actually writes.

The fork guessed whether a finding was "speculative" from its wording, and on
real data it guessed wrong both ways. The port asks the model for a yes/no
`speculative` field, as the rubric already tells it to mark, using the
rubric's own words: speculative means "I could not show how this bug gets
triggered", not "I am not sure who can reach it". The model answers that
before it picks the severity, because the answer caps the severity. Old
reviews never answered, so their cards say so once instead of pretending.

Because this changes what every Linux review is told, the two prompt
commits are measured on real patches, with the user's go-ahead, before
they land.

A Sashiko review's inline text is itself a bullet list of the findings. So
when the new list appears, that text starts folded, and nothing shows twice.
The command-line tool, which prints a blank line for each finding today,
prints the same title, tags and location.
