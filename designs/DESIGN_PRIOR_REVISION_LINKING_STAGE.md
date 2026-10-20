# Design: Link to Earlier Revisions in a Stage of Its Own

Status: draft for review, 2026-10-20. Branch: `port/v0.6.1` at
9ec1eddbf66a (upstream 0.6.1 plus the port).

## 1. Context

### 1.1 What the port does today

When a pull request is pushed again, the daemon gives the review the
findings of its earlier revisions (`DESIGN_PORT_PULL_REQUESTS.md` §4.5,
port commits "review: show verification what earlier revisions were told"
to "review: say exactly where a repeat finding's history sentence goes").
The history reaches the model through three channels:

1. **Verification** is shown the earlier-revisions block
   (`VERIFICATION.wants_prior_reviews`, `with_prior_reviews`) with
   `PRIOR_REVIEWS_VERIFICATION_RULES`. It classifies the concerns with that
   block in view, adds a `"prior"` link to a finding or hard case that
   repeats an earlier finding or that the fix for one caused, and writes a
   history sentence into the finding's `severity_explanation` or the hard
   case's `concern_arguments`. A validator rejects a bad link once
   (`prior_link_validator`), and the reduce grounds every link against the
   block (`ground_verification_links`).
2. **Post-verification** is not shown the block, but a linked hard case
   reaches it with its link and its history sentence, and the batch is
   given `PRIOR_LINK_POST_VERIFICATION_RULES` and, whenever the pull request
   had earlier revisions, `PRIOR_SERIES_POST_VERIFICATION_RULE`. A finding
   it reports inherits the link of the hard case it names
   (`inherit_prior_links`).
3. **The report** is given the earlier findings its findings link to
   (`with_prior_links`, `PriorReviewContext::render_linked`) and states each
   link in the comment.

The links are stored in `findings.prior` (migration 018), tagged on the web
card and review map ("(repeat of v2)", hover "Earlier finding (v2, head
...): ...") and in both CLIs.

### 1.2 What the measurement found (2026-10-20)

`~/v06-measure` reviewed step 6's five pull-request samples with
Claude Opus 5.5 at effort high, on the port's tip:

| Arm | Runs | Findings per run | Cost per run |
| --- | --- | --- | --- |
| T: PR description and earlier revisions shown | 2 | 14, 14 | $28.18, $28.88 |
| B: PR description only (control, same binary) | 3 | 9, 11, 9 | $24.51, $27.80, $25.35 |

Blind judging (`~/v06-measure/judge`, $14.34; gemini-4-argon
majority of 3 votes per issue, gpt-6-astra as second judge; judges never
see which arm found an issue or any wording about earlier revisions)
grouped the 57 findings into 20 issues. Gemini's gateway refused every
vote on three of them, which grok-4.7 then decided with 3 votes each on the
same blinded prompts:

| Found by | Issues | Judged real |
| --- | --- | --- |
| T only | 5 | 2 |
| both arms | 11 | 11 |
| B only | 4 | 2 |

- Every T-only issue is a repeat of an earlier revision's finding. T made no
  new discovery.
- B found 2 real issues (both driver-b) that neither T run reported.
- The four issues T tagged most often were found by all three B runs
  without any history: the tag labelled them, it did not find them.
- Real issues per run: T 11.5, B 8.7. T's edge is two real repeats (one
  High, below) and steadier recall on shared issues; its cost is 3 extra
  findings per run that are not defects, and about $2.70 (+11%) per run.

The mechanism, traced on driver-a #937 v3 (`out/*/driver-a-
939ac8077af0.json`): in all five runs an analysis stage raised the concern
"checking ionic_port_reset()'s return now aborts FLR/AER recovery", and
verification escalated it to a hard case. In the three B runs
post-verification checked it and dismissed it ("the intended fix of an
obvious bug"). In both T runs verification had linked it to v2's finding
and written "This path ... was reported on v2; the current revision has
the same diff" into its `concern_arguments`; post-verification, told about
the link, confirmed it. The judges split: gemini's one answered vote
called it ordinary error handling, not a defect; grok-4.7 called it a real
High (2 real, 1 unclear), because the abort fires only during recovery and
leaves the NIC dead. The analysis stages are never shown the history, so
the difference is made where the history reaches a stage that classifies:
verification, and through the hard case's arguments and the link rules,
post-verification.

That case cuts both ways. If grok is right, the history rescued a real High
that post-verification wrongly dismissed three times out of three without
it; this design would lose it (§6.3 watches for it, D8). The other T-only
issues show the cost of the same lean: three repeats judged not defects (a
debugfs-only complaint, a `_MAX` naming remark, a comment placement).

`DESIGN_PORT_PULL_REQUESTS.md` §4.5.5 rule 1 already tells verification
that an earlier finding makes a concern neither more nor less likely to be
valid. The measurement says that instruction is not enough: a model that
can see the earlier verdict leans on it.

### 1.3 The user's decision (2026-10-20)

Of the two directions offered after the judging, the user chose the second:
keep saying "this has been found before", so that whoever reads the
findings (a person, or an agent with a review ledger) can see how the issue
was handled before, but stop the history from turning old concerns into
findings. The other direction, giving the analysis stages the earlier
findings as leads to dig further, was not chosen.

The user also wants, eventually, each patch to report one more number:
**new findings only**, excluding findings that appeared in earlier reviews
(called "first reported" in the page and CLIs, §4.9)
(§4.9).

## 2. Goals and Non-goals

### Goals

- G1. No stage that decides whether a concern is a finding, a hard case or
  a dismissal is shown anything about earlier revisions. Verification and
  post-verification see exactly what they see for a pull request with no
  history.
- G2. A finding that repeats an earlier revision's finding, or that the fix
  for one caused, is still linked to it, grounded against what the review
  was shown, stored in `findings.prior`, shown on the page and in the CLIs,
  and stated in the report, as today.
- G3. Linking can only add a link to a finding the review already has. It
  cannot add, drop, merge or reclassify a finding, or change its severity.
- G4. A review of a pull request with no earlier revisions is byte for byte
  the review it is today (no new stage call, no prompt change).
- G5. Each patch reports how many of its findings are new (§4.9).
- G6. Less code and fewer prompt rules than today: the history-sentence
  placement rules (#115, #124) and the post-verification link and series
  rules (#109, #123) go away.

### Non-goals

- Showing earlier findings to the analysis stages as leads (the direction
  not chosen).
- Changing how earlier findings are retrieved or rendered
  (`get_prior_revision_findings`, `build_prior_review_context`), the link's
  JSON shape, migration 018, or the report's linked block and rules.
- Linking dismissed concerns or hard cases. A link is a property of a
  reported finding only.

## 3. What Changes, in One Paragraph

A new consolidation stage, `prior-links`, runs after post-verification and
before the report, only when the review was shown earlier findings and has
at least one finding. It is given the review's final findings (title,
severity explanation, locations, stage item id) and the earlier-revisions
block, and answers with links only: for each finding that is the same
defect as an earlier finding, or that the fix for one caused, its stage item
id and the link. The reduce grounds each link exactly as today and writes
it onto the finding. Verification loses the block, its rules, the link
validator and the extra validation attempt; post-verification loses the
link and series rules and the inheritance. The report, storage, page and
CLIs keep reading `finding.prior` as they do now.

## 4. Design

### 4.1 Placement

```
pre-screen -> planning + analysis stages -> early exit (no concerns)
  -> verification + post-verification-N -> early exit (no findings)
  -> prior-links            (new; skipped without earlier findings)
  -> report
```

`build_linux_patch_review_workflow_with_effort` and the Sashiko builder add
`.stage(prior_links_stage_with_effort(...))` after the second
`early_exit_if` and before the report. The stage carries
`skip_if(|s| s.prior_reviews.is_none())`. A review with no findings has
already exited, so the stage never runs with nothing to link (G4).

A new `ConsolidationStage` entry `PRIOR_LINKS` (`name: "prior-links"`,
`short: "Earlier-Revision Links"`, `wants_series_context: false`,
`wants_prior_reviews: true`) goes into `CONSOLIDATION_STAGES` before
`REPORT`, so `stage_order`, the stage rows on the page, the progress total
and `reports_stage_row` know it. `VERIFICATION.wants_prior_reviews`
becomes `false`; the test that checks each rendered prompt against the flag
then pins the move.

### 4.2 What the stage is shown

User prompt, in this order:

1. A short instruction (§4.4).
2. The earlier-revisions block, `PriorReviewContext.rendered`, unchanged:
   the same text verification is shown today, so `ShownPriorFinding`, the
   grounding and `render_linked` keep working on the same record.
3. The findings, as a JSON list built from `state.findings`: each item's
   `stage_item_id`, `problem`, `severity_explanation` and `locations`. Not
   the severity, `speculative`, `preexisting`, or anything about which stage
   made it: the stage links, it does not weigh.
4. The output format and the rules (§4.4).

System prompt: the workflow's system prompt with the cover letter, as the
other consolidation stages have it. Tools: the read-only git tools
(`ToolScope::All`), because a fix regression is established by comparing
the earlier revision's code with the current code, and the block names the
earlier commits. `max_turns` 10: the stage compares, it does not
investigate.

Untrusted input: the block is already rendered as quoted, labelled data
(§4.5.3 of the PR design); the findings are model output of this review.
Nothing new crosses the boundary.

### 4.3 What it answers

```json
{
  "links": [
    {"stage_item_id": "PVF1", "relation": "repeat",
     "revision": "v2", "earlier_problem": "<title, copied as listed>"}
  ]
}
```

- `stage_item_id` must name a finding in the list; each at most once.
- `relation` is `repeat` or `fix_regression`, as `PriorRelation` parses.
- `revision` and `earlier_problem` must name a finding the block listed
  (`PriorReviewContext::ground`).
- An empty `links` list is a valid answer and the expected one when nothing
  repeats.

No `history` sentence and no edits to the finding (G3). The link is the
whole record of the history; the report states it (§4.6) and the page
shows it.

### 4.4 Rules: one constant for both workflows

The rules no longer name rubric sections, because the stage writes nothing
into a severity explanation, so the per-workflow macros
(`prior_reviews_verification_rules!`, `prior_link_post_verification_rules!`,
`history_at`, `history_before`, `section_names`) are deleted and one
`PRIOR_LINKS_RULES` constant serves both workflows. In substance, from
today's rules 2, 3 and 5:

- REPEAT: link a finding to an earlier finding when it is the same defect
  and the current code still has it.
- FIX REGRESSION: link it with `fix_regression` when the code it is about
  was added or changed in response to an earlier finding, that finding no
  longer applies, and the change that resolved it creates this defect.
  Establish it by comparing the earlier revision's code (its diff in the
  block, or the commit named in its patch header) with the current code;
  similar wording is not enough.
- Link only to a finding the block lists, under the label it is listed
  under, with the title copied as listed. Omit a link when no earlier
  finding is the counterpart. Do not link two findings to the same earlier
  finding unless both are that defect.
- You are not judging the findings: whether a finding is valid or how
  severe it is is settled, and a link says nothing about either.

The series sentence of "review: keep an earlier revision's commits out of
series validation" is not needed: no stage that dismisses concerns sees the
block any more, so no block commit can stand in for a later patch. Its
relabelling of the block's patch header ("another patch of that revision")
stays.

### 4.5 Validation, retries and the reduce

- Output format: JSON, validated by a new `validate_prior_links_output`:
  every `stage_item_id` names a listed finding and appears at most once;
  then `PriorReviewContext::link_feedback` over the links, as today's
  `prior_link_validator`, rejecting a bad link at most once per review
  (`PRIOR_LINK_REJECTIONS`), with feedback that names the item by its
  stage item id and the fault. `max_validation_attempts` is the default
  plus that one rejection, as verification's is today.
- Reduce (`apply_prior_links_stage_output`): for each link, find the finding
  in `state.findings` with that `stage_item_id`, set `finding["prior"]`, and
  run `ground_prior_link` on it, which canonicalises a good link, adds the
  revision head and removes and logs a bad one. The same link is written
  onto the matching entry of `state.verification_findings` or
  `state.post_verification_findings`, so the review map's items carry it
  (§4.7). A preexisting finding is a concern by then and is not in
  `state.findings`; its id is rejected by the validator.
- Idempotent: the reduce sets links from the answer only, and a retry
  replaces nothing it did not write.

### 4.6 Wind-down, failure and the report

- The stage is interruptible (`interruptible: true`): at the review's
  deadline it is stopped like an analysis stage, and the review goes on to
  the report with no links. Links are optional; a Partial review without
  them is still a correct review.
- If the stage fails, the review continues without links and the failure is
  recorded in `stage_failures` like any stage's. It never fails the review.
- The report is unchanged: it runs after `prior-links` and reads
  `finding.prior` through `with_prior_links`, so a linked finding's comment
  still opens with "raised on v2 and still present" or the fix-regression
  wording.

### 4.7 Storage, page, CLIs, review map

- `findings.prior`, the reviewer's store path, `PriorLink::from_json`, the
  card's and CLIs' tags: unchanged.
- Review map: the new stage is a stage row like any other, with its log
  slice. `reviewMapThreadLink` keeps reading the link from the thread's
  final VF or PVF (the reduce writes it there, §4.5); the "link carried from
  H1" note goes, because links are no longer carried from hard cases. The
  history step shows the tag on the stage that produced the finding, and the
  `prior-links` stage's log link is how a reader sees why it was linked.

### 4.8 Sashiko workflow

The same stage, built in `sashiko_patch_review.rs` with the Sashiko system
prompt and the shared `PRIOR_LINKS_RULES`. The Sashiko report's "(repeat of
v3)" note after the severity marker is unchanged.

### 4.9 First-reported findings per patch (G5)

The page and both CLIs already call a finding "new" when it is not
preexisting: the card's "High: 2 (1)" means 2 new and 1 preexisting. So
the figure the user asked for is called **first reported**, to keep one
meaning per word: a counted (not preexisting) finding is first reported
unless its `prior` link reads as a repeat. A fix regression is first
reported (D3): the defect did not exist before; the change made for an
earlier finding caused it.

Where the number appears, only when at least one counted finding is a
repeat (otherwise it equals the counts beside it, as for every review of a
pull request with no earlier revisions):

- The review card's Result line: "... Low: 3 · First reported: 2".
- The per-patch table on the patchset page: "(2 first reported)" after the
  badges, the most of the patch's runs, as the badges take each severity's
  maximum over the runs.
- The patchset list row: "(2 first reported)" after the badges.
- `sashiko-cli show` and `sashiko review`: "First reported: 2" on the
  counts line.
- The API: `findings_first_reported` on each patchset list row and on the
  counts an embargoed row is given, from `FIRST_REPORTED_FINDING_SQL` over
  the same reviews the counts use (`FINDINGS_COUNTED_STATUSES_SQL`). It
  reads the stored link with `json_extract`; no migration.
- One rule everywhere: the page counts a link as a repeat when its tag can
  show it, the CLIs when `PriorLink::from_json` reads it, and the column
  holds only links that read.

This part is independent of the stage and is built first, on today's
links. It is listed here because the stage is what makes the figure
trustworthy: today a repeat can be a finding only because the history was
shown, which inflates exactly the number this subtracts.

### 4.10 Cost

The stage is one short conversation per patch with earlier findings: the
block (5K to 18K tokens on the samples) plus the findings, a few tool
calls at most. Verification loses the same block from a conversation of
many turns, so the net cost should drop below today's T arm. §6 measures
it.

## 5. Type-Driven State

- `PriorLinksOutput { links: Vec<ProposedLink> }`, `ProposedLink {
  stage_item_id: String, relation: String, revision: String,
  earlier_problem: String }`: the wire shape, deserialised strictly
  (`deny_unknown_fields`), so a model that adds a history field is told
  instead of silently ignored.
- Grounding returns `PriorLink` as today; the reduce writes only grounded
  links.
- `PriorHistory` (today a parameter of the verification builders, choosing
  the link check) goes away: verification no longer depends on history, so
  `build_*_workflow_with_effort` loses the parameter and the worker builds
  the workflow before the state again.

## 6. Testing and Measurement

### 6.1 Rust

- The stage is skipped without earlier findings, and a review without
  history renders every prompt byte for byte as before (`user_prompts`
  with `None`, as `prior_reviews_render_nothing_without_history` does).
- Verification's and post-verification's rendered prompts never contain
  the block, its opening line, "PRIOR REVISION", "PRIOR LINK" or
  "reported on" wording, with or without history; the flag test checks
  `prior-links` is the only stage with `wants_prior_reviews`.
- Validator: unknown or repeated `stage_item_id`, an unlisted title, an
  unknown relation, each named in the feedback; one rejection per review,
  then the reduce removes the bad link.
- Reduce: a link lands on the finding and on its VF or PVF entry, grounded
  and canonical with the revision head; nothing else in the finding
  changes (compare the finding before and after, minus `prior`).
- The stage cannot change findings: an answer with extra keys is
  rejected by the strict shape; an answer naming a preexisting concern's
  id is rejected.
- Wind-down: a stopped `prior-links` leaves the review with no links and
  the report still runs.
- New-findings count: the SQL and the page/CLI computation agree on a
  fixture with repeat, fix-regression, unlinked and preexisting findings.

### 6.2 UI verifier

- The Result line's, per-patch table's and list row's "first reported"
  figure, and its absence without a repeat.
- The review map without the "carried from" note.

### 6.3 Measurement (spends)

On `~/v06-measure`, same samples, model, effort, scratch flags and
pinned date; a new binary with this design:

- Arm L, 2 runs, both flags. Compare with B (the judged no-history
  control) and T.
- Expect: findings per run within B's range (9 to 11), because
  classification no longer sees history; links on the findings that repeat
  earlier ones; cost at or below B plus the stage's share.
- Check by hand every link L makes: does it name the right earlier finding?
  Count links T made on the same issues that L does not (lost links) and
  links on issues T never linked (new links).
- Blind-judge only issues L reports that the 20 judged issues do not cover.
- Budget: about $60 for the runs, under $10 for judging.

## 7. Commit Breakdown

Each commit builds, passes `make check-pr` and the UI verifier, and is one
change.

1. `review: count the findings a review reported first` (§4.9: SQL, API field, page,
   CLIs, tests; works on today's links).
2. `review: link earlier findings in a stage of their own` (§4.1 to §4.6:
   the stage, its rules, validator and reduce in both workflows; verification
   stops seeing the block and loses its link rules, validator and extra
   attempt; tests of §6.1). One change: moving where linking happens.
3. `review: drop post-verification's earlier-revision rules` (link and
   series rules, `inherit_prior_links`, `candidates_for_prompt`,
   `prior_link_section`, `with_prior_series_rule`; dead after 2).
4. `review: drop the history-sentence placement rules` (the macros'
   `history_at` / `history_before` / `section_names`, their tests and
   `CalibrationLabel` table use that only they had; dead after 2).
5. `web: show which stage linked a finding on the review map` (§4.7).
6. `docs: say how a later revision's findings are linked` (GITHUB_SETUP.md
   and the PR design's §4.5 pointer to this design).
7. `designs: record the linking stage's measurement` (§6.3 results, after
   the runs).

## 8. Open Decisions

- D1. Stage name: `prior-links` (recommended) or `earlier-revisions`.
- D2. Tools for the stage: read-only git tools with `max_turns` 10
  (recommended; fix regressions need code) or none (cheaper; the block's
  diffs only, so a fix regression is judged from the earlier diff alone).
- D3. Does a fix regression count as first reported? Approved: yes (§4.9).
- D4. Keep a one-sentence history note? Today's history sentence goes with
  §4.3. Option: the link carries an optional `note` (what changed around
  the defect since that revision), shown in the hover and the report.
  Recommended: no for now; the link names the earlier finding, and the
  ledger the user mentioned is where "how it was addressed" lives.
- D5. Order of commits 1 and 2: the count first (recommended; useful on
  today's links and independent) or the stage first.
- D6. The port's commits #107 to #124 stay in history as they are; this is
  a new series on top (recommended), not a rewrite of those commits.
- D7. Settled: the grok-4.7 re-vote (2026-10-20, $0.39) decided the three
  issues gemini refused; it moved #937's PORT_RESET issue to real (§1.2).
- D8. The #937 PORT_RESET case: a real High that only history-shown runs
  reported. This design gives it up in exchange for not reporting
  repeats that are not defects. Options: (a) accept it (recommended for
  now: one case, judged 2 real to 1 unclear by one judge and not a defect
  by another); (b) let `prior-links` flag a dismissed concern or refuted
  hard case that matches an earlier finding, as a note for the reader
  ("v2 reported this; this review dismissed it"), without making it a
  finding. (b) keeps G1 and G3 and gives the reader the hint the user
  wants; it needs the dismissed items in the stage's input and a place on
  the page. Decide after §6.3.

## Feynman Summary

When a pull request comes back with a new version, Sashiko tells the
reviewer what earlier versions were told, so it can say "this was found
before". We measured that and found a side effect: the step that decides
which concerns are real bugs leans on the old verdicts, so old complaints
come back as findings even when they are not real. This design moves the
"found before" job into its own small step at the end, after every finding
has been decided without looking at history. That step only attaches links
to findings that already exist; it cannot add or remove any. The links,
the page tags and the report wording stay as they are, the deciding steps
get simpler, and each patch can then show how many of its findings are new.
