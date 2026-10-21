# Design: Say Which Repository a Finding's Location Is In

Status: draft for review, 2026-10-21. Branch: `port/v0.6.1` at
8168ea0b4af1 ("review: keep an out-of-tree module's call chains in the
module").

## 1. Context

### 1.1 What a location is today

Every concern, finding, hard case and dismissed concern carries
`locations`: objects with `file`, `function_or_symbol`, `line`,
`code_snippet` and `why_this_location_matters`
(`STAGE_JSON_SCHEMA_EXAMPLE`, `src/workflows/linux_patch_review.rs:457`).
They are the review's evidence. Verification and post-verification hold
findings to a proof bar stated in terms of them (`has_valid_proof_location`,
`:515`). The report cites them, the page shows them (`renderFindingLocations`,
the review map's Code Trace), the CLIs print them (`finding_view.rs`,
`main.rs:2890`), and later revisions' reviews are told about them
(`prior_revisions.rs`).

A review of an out-of-tree module has read-only reference repositories
attached (`[[git.references]]`, `ToolBox::references`), most often the kernel
it builds against. The tools take `repo = "review"` (the default) or a
reference's name, and the prompt asks the model to "cite repo:file:line" when
it claims a contract with a reference (`reference_context_block`, `:86`). That
request reaches the prose only. A location has no repository, so a kernel
path and a module path look the same in it.

### 1.2 What went wrong (driver-b #24, review 153, 2026-10-20)

The finding "pds-vfio-pci: RESET on any PF marks every PF's dirty containers
stale" has five locations:

| `file` | Actually in |
| --- | --- |
| `vfio/dirty.c` | the module (review) |
| `vfio/pci_drv.c` | the module |
| `drivers/net/ethernet/amd/pds_core/core.c` | the kernel only |
| `include/linux/pds/pds_adminq.h` | **both**; the snippet is the kernel's |
| `vfio/dirty.c` | the module |

driver-b ships its own pds_core (`core/core.c:23` defines `pdsc_notify()`)
and its own `include/linux/pds/pds_adminq.h`. The in-tree copies are not
loaded while the module runs, and the module never calls into them. The
finding's call chain still runs from the module's notifier into the kernel's
`pdsc_notify()`.

Post-verification-3, the stage meant to catch this, made exactly these
reads:

```
git_grep repo=kernel path=drivers/net/ethernet/amd/pds_core/  pdsc_notify(|pds_notify_chain|PDS_EVENT_RESET
git_grep repo=kernel path=include/linux/pds/                   struct pds_core_reset_event
git_grep repo=review path=vfio/                                register_notify|pds_vfio_recovery(
```

It re-checked the claim in the tree the earlier stage had used and never
looked at the module's own copies. It confirmed a chain that does not
execute.

8168ea0b4af1 fixed the prompt's part in this: the out-of-tree caveats now
say the module replaces the kernel's version of whatever it ships, and that
call chains used as evidence stay in the module. This design fixes the
data's part. A location must say which tree it is in, and the stages that
decide findings must be able to check it, including deterministically.

### 1.3 Other readers of a location's path

The missing label misleads more than verification (a survey of every
reader, 2026-10-21):

- `prior_revisions::is_cited_by` decides which earlier findings are about a
  patch by matching location files against the files the diff touches. A
  kernel `include/linux/pds/pds_adminq.h` location matches a diff that
  touches the module's header of the same name.
- `Reviewer::hand_off_concern` turns every location's `file` into a bug's
  `source_files`, which the bug pipeline treats as mainline paths.
- `vector_search::extract_bug_vector` turns `file` into path features, so a
  kernel path and a module path with the same name collide.
- The page and the CLIs show `file:line` with nothing to say which tree.
- The planned GitHub line comments (anchored on `findings.locations`) would
  anchor kernel paths onto the pull request's diff.

## 2. Goals and Non-goals

### Goals

- G1. Every location in a review that has reference repositories attached
  names its repository, with the same vocabulary the tools use: `"review"`
  or an attached reference's name.
- G2. The stages that decide findings (verification and post-verification)
  re-check each location in the repository it names, and are told to.
- G3. A location that cites a reference's copy of code the module under
  review carries itself is caught deterministically, not left to the
  model's attention: its validator rejects it with a message that says
  which module file to cite instead.
- G4. A location's repository is shown wherever the location is shown (page,
  review map, CLIs, report), and readers that treat a location's `file` as a
  path in the code under review (prior-revision matching, bug hand-off,
  vector features, future PR line comments) use only `"review"` locations.
- G5. A review with no reference repositories attached (every in-tree Linux
  review, every Sashiko review) is byte for byte the review it is today: no
  prompt change, no validator change, no new key.

### Non-goals

- Checking that a `code_snippet` appears in the file it names. Worth doing,
  but snippets elide with `...` and re-indent freely, and a false rejection
  costs a stage retry. §8 D5 keeps it as a follow-up, measured first as an
  offline audit (§6.3).
- The bug pipeline's `relevant_code_locations` (`linux_bug.rs`). It runs on
  the mainline instance, which has no references; only the hand-off from a
  patch review is filtered (G4).
- The Sashiko workflow's prompts. Its state carries `references`, but no
  Sashiko deployment configures any, and its system prompt has no reference
  block. The shared helpers no-op without references, which keeps it out of
  scope at no cost.
- Letting a location cite the kernel's copy as a "comparison" (a fix the
  module could adopt). Locations are evidence. A comparison belongs in the
  prose, labelled as such, which is what 8168ea0b4af1 tells the model.
  §8 D2 records the alternative.

## 3. What Changes, in One Paragraph

When a patch has references attached, every location gains a required
`"repo"`: `"review"` or an attached reference's name. Before the stages run,
the worker builds a small index per patch: the paths in the code under review
at the patch's commit, the paths in each reference at its pin, and the names
the code under review defines (functions, structs, enums, unions, macros).
Every stage's validator rejects a location with a missing or unknown `repo`.
Verification's and post-verification's validators also reject a location
whose `file` does not exist in the repository it names, and a reference
location whose path, or whose `function_or_symbol`, the code under review
carries itself. Each rejection names the fix ("the module defines
pdsc_notify at core/core.c:23; cite that with repo \"review\""). A bounded
extra attempt pays for the retry. The reduce turns a `"kernel:..."` prefix
in `file` into the key, so the model's habit from the prose rule is accepted
rather than fought. Verification and post-verification are told to re-read
every location in its repository. Readers show the label and keep paths in
their own trees.

## 4. Design

### 4.1 The label

```json
{"repo": "review", "file": "vfio/dirty.c", "function_or_symbol": "pds_vfio_dirty_containers_reset",
 "line": 681, "code_snippet": "...", "why_this_location_matters": "..."}
{"repo": "kernel", "file": "include/linux/kvm_host.h", "function_or_symbol": "struct kvm", ...}
```

- The key is `repo`, the tools' own argument name (`REPO_ARG`), and its values
  are the tools' own values (`REPO_ARG_REVIEW` or a reference name). The model
  writes in the location exactly what it passed to the tool that read the
  code, so the two cannot drift in vocabulary.
- Required on every location when references are attached; never mentioned
  otherwise (G5).
- Type-driven (AGENTS.md): a `LocationRepo` enum, `Review` or
  `Reference(name)`, parsed once at the boundary from the JSON string against
  the patch's attached references. An unknown name is a parse error that
  becomes the validator's message, never a third state.
- The `commit_message` sentinel the Sashiko workflow allows for `file` is
  unaffected: that workflow has no references (§2).

### 4.2 The per-patch tree index

Validators are synchronous (`Fn(&T, &S) -> Result<(), String>`,
`workflow/output.rs:67`), so they cannot run git. The worker builds a
`TreeIndex` once per patch, in `spawn_blocking`, before the workflow starts,
and stores it in `LinuxPatchReviewState`:

- `paths(repo)`: every path from `git ls-tree -r --name-only` at the patch's
  commit for `review` and at each reference's pinned commit. The kernel is
  about 90k paths, a few MB, built in well under a second; it is the same
  commit for every patch of a series and is shared through an `Arc` keyed by
  commit.
- `defined(review)`: names the code under review defines, from one pass over
  its `.c`/`.h` files at the patch's commit with a deliberately simple
  matcher: a line at column 0 that starts a function definition (identifier,
  `(`, no trailing `;`), `struct|union|enum NAME {`, and `#define NAME`. Each
  name maps to its first `file:line`. Built only when a kernel reference is
  attached, and skipped (fail open, logged) when the code under review has
  more than 20,000 files, which only an in-tree kernel does.

The index only ever widens what the validator knows. A path or name it
misses produces no rejection. It cannot produce a false "does not exist",
because it holds every path of the tree, and the "module carries it" checks
are the only ones that rely on the matcher.

### 4.3 Validation

All stages, when references are attached (cheap, no index needed):
- Every location has `repo`, and it parses as a `LocationRepo`. Message:
  `location 3 of concern C2 has repo "linux" but the attached repositories
  are "review" and "kernel"; set "repo" to the one you read the code in`.

Verification (findings, hard cases, dismissed concerns) and post-verification
(findings, dismissed concerns), in addition:
1. **Exists.** `file` exists in `paths(repo)`. When it does not but exists in
   another attached repository, the message says so: `"drivers/net/.../core.c"
   is not in repo "review"; it is in repo "kernel". Set "repo" to where you
   read it`.
2. **Not the module's own code, by path.** A reference location whose `file`
   is also in `paths(review)`: `repo "kernel" location cites
   include/linux/pds/pds_adminq.h, which the code under review ships itself;
   the kernel's copy is not loaded while the module runs. Cite the module's
   file with repo "review"`.
3. **Not the module's own code, by name.** A reference location whose
   `function_or_symbol` (bare name, `struct X`, `X()` forms normalized) is in
   `defined(review)`: `repo "kernel" location cites pdsc_notify, which the code
   under review defines at core/core.c:23; cite that with repo "review"`.

Retry budget: like `PRIOR_LINK_REJECTIONS`, the stages that run checks 1-3 get
one extra attempt, spent only on these rejections. When a stage still fails
them after that, the output is accepted. The reduce then sets
`"repo_conflict": true` on each offending location (`check 2 or 3`) or
`"unverified_path": true` (check 1), and the page shows a warning beside it
(§4.6). A finding is never dropped by this design (§8 D3).

Analysis stages get only the cheap check. Their concerns are candidates that
verification re-checks, a retry there costs seven stages' worth of
exposure, and the mislabel that matters is the one a deciding stage accepts.

### 4.4 Normalization in the reduce

The prose rule "cite repo:file:line" predates the key, and models carry it
into `file`. Every reduce that copies locations (analysis append,
verification, post-verification) applies one helper, before validation sees
the output and again when storing it:

- `file` of the form `<name>:<path>` where `<name>` is an attached repository
  and `repo` is absent: split into `repo` and `file`.
- Same form with `repo` present and equal: strip the prefix.
- Same form with `repo` present and different: leave both; check 1 reports it.

The validator interprets the location through the same helper, so a prefixed
`file` is accepted, not rejected.

### 4.5 Prompts (Linux workflow, only when references are attached)

- Location key lists (analysis `STAGE_JSON_SCHEMA_EXAMPLE`, verification and
  post-verification key lists and examples) gain `"repo"` first:
  `"repo" ("review" for the code under review, or the name of the reference
  repository you read the code in)`. Rendered through the state, so the
  no-reference text is unchanged (G5); the example gains one
  `"repo": "kernel"` location.
- `reference_context_block`'s "cite repo:file:line" becomes "set a location's
  repo to the repository you read it in; in prose, cite repo:file:line".
- Verification and post-verification gain one rule, shared as a constant:

  > LOCATIONS NAME THEIR REPOSITORY. Re-read every location you keep in the
  > repository its "repo" names, with that "repo" passed to the tool. A
  > location in a reference repository is evidence only for code the code
  > under review does not carry. When the code under review has its own copy
  > of the file, function or struct a location cites from a reference,
  > re-check the claim in that copy: the reference's copy does not run.
  > A claim whose call chain passes from the code under review into a
  > reference's copy of code under review is not established.

- The report is told to cite a reference location as `<repo>:<file>` in the
  comment, which is the existing prose convention made explicit.

### 4.6 Readers

- **`finding_view::FindingLocation`** gains `repo: Option<String>`, sanitized
  like the other fields. `Display` prints `kernel:path:line (fn)` for a
  reference and the plain form for `review` or absent, so both CLIs label it.
  `main.rs::format_location` goes through the same type rather than its own
  key reads.
- **`prior_revisions`**: `PriorFinding` keeps the label. `is_cited_by` and
  `render_diff` consider only `review` (or unlabelled) locations;
  `push_where_and_what` prints the label.
- **`Reviewer::hand_off_concern`**: `source_files` from `review` (or
  unlabelled) locations only.
- **`vector_search::extract_bug_vector`**: a reference location's path
  features are prefixed with its repository, so they do not collide with a
  module path.
- **Page** (`renderFindingLocations`, `renderReviewMapCodeSnippets`): a
  reference location shows `kernel:` before its path, with the reference's pin
  in the hover from the review row's `reference_repos`; `repo_conflict` and
  `unverified_path` show a warning tag ("kernel copy of module code",
  "path not found in kernel"). Verifier checks for both.
- **Future PR line comments** (planned, memory "post findings to GitHub PR"):
  anchor only `review` locations. Recorded here so that design inherits it.

Unlabelled locations (every review before this change, and reviews without
references) read exactly as today.

### 4.7 Storage

Locations are stored as JSON (`findings.locations`, `create_finding`,
`db.rs:3831`) and copied verbatim between stages (`item_locations`,
`record_verified_findings`): the new keys survive without a migration. The
survey found no `deny_unknown_fields` or key whitelist on any location path.

## 5. Type-Driven State

- `LocationRepo { Review, Reference(String) }`, constructed only by
  `LocationRepo::parse(&str, &[ReferenceRepo])`.
- `TreeIndex` owns `HashMap<LocationRepoKey, Arc<HashSet<String>>>` for paths
  and `HashMap<String, (String, u32)>` for definitions; validators get it
  through `&LinuxPatchReviewState`.
- The two warning flags are booleans on the stored JSON location, set only
  by the reduce after the retry budget, never by the model (a model-written
  flag is stripped on the way in).

## 6. Testing and Measurement

### 6.1 Rust

- Parse: `LocationRepo` accepts `review` and attached names, rejects others
  with the message in §4.3; normalization splits `kernel:path`.
- Validators, on a fixture with a fake module tree and a fake kernel tree
  sharing `include/linux/pds/pds_adminq.h` and a `pdsc_notify` definition:
  missing repo, unknown repo, kernel-only path labelled review, module-carried
  path labelled kernel, module-defined symbol labelled kernel, and the passing
  case; each message names the fix.
- Retry budget: one extra attempt, then accepted with the flags set.
- G5: with no references, every stage's rendered prompt equals today's
  (snapshot of the rendered text) and validators accept a location without
  `repo`.
- Readers: `FindingLocation` display, `is_cited_by` ignoring a kernel location
  with a module-shaped path, hand-off filtering, vector prefixing.

### 6.2 UI verifier

The `kernel:` label on both renderers, escaping, the pin hover, the two
warning tags, and unlabelled locations unchanged.

### 6.3 Measurement (spends)

Three arms, each on the two out-of-tree samples in `~/v06-measure`
(driver-b and driver-a) plus driver-b #24 patch 3, two runs each, Claude
Opus 5.5 at effort high:

| Arm | Build |
| --- | --- |
| P | 43bab88bfd3f (deployed `v0.6.1-ricera.1`) |
| Q | 8168ea0b4af1 (prompt fix only) |
| R | this design |

Measured per run, offline from the stored output and the two trees:
locations carrying a label; locations whose `file` is not in the tree they
name (or, for P and Q, in neither tree's expected one); reference locations
citing module-carried paths or names; validator rejections and extra
attempts; findings, cost; and whether #24's finding still runs through the
kernel's `pdsc_notify()`. An offline snippet audit (does `code_snippet`
appear, modulo whitespace and `...`, in the file it names) runs on all three
arms and informs D5.

Estimated cost: 3 arms x 3 samples x 2 runs, about $5 per sample run, so
about $90.

## 7. Commit Breakdown

1. designs: this document.
2. review: a location's repository, parsed and normalized
   (`LocationRepo`, the prefix helper; no caller yet).
3. review: index the trees a patch's locations can name (`TreeIndex`, built
   in the worker, stored in the state).
4. review: ask every location for its repository when references are attached
   (prompt key lists, examples, reference block wording; G5 snapshot test).
5. review: check a location's repository in every stage (cheap check).
6. review: hold verification's locations to the trees they name (checks 1-3,
   retry budget, warning flags, the verification/post-verification rule).
7. finding_view, prior_revisions, reviewer, vector_search: keep reference
   locations out of the code under review (one commit per reader if they
   grow; each is self-contained).
8. web: label a location's repository (+ verifier).
9. docs: configuration.md and the reference section.
10. designs: record the measurement (§6.3 results, D-decisions settled).

## 8. Open Decisions

- D1. **Key name.** `repo` (recommended: the tools' own argument name and
  values) vs `repository`. Recommended: `repo`.
- D2. **Comparison locations.** Locations are evidence only, and a kernel copy
  cited as a fix to adopt goes in the prose (recommended), vs a
  `"role": "evidence" | "comparison"` key that lets a location cite the
  kernel's copy deliberately. The second is more expressive but gives the
  model a way to label its way past check 2/3.
- D3. **After the retry budget.** Accept and flag the location (recommended:
  a finding is never lost to a labelling dispute, and the flag makes the
  problem visible), vs drop the offending location, vs demote the finding to
  a hard case.
- D4. **Analysis stages.** Cheap check only (recommended, §4.3) vs the full
  checks everywhere.
- D5. **Snippet grounding.** Follow-up after the §6.3 audit (recommended) vs
  in this design.
- D6. **Measurement.** Spend about $90 for §6.3 before deploying, or deploy Q
  first (it is already a separate commit) and measure R against it.

## 9. Found Along the Way (separate commits, not this design)

- `linux_bug.rs:649-699` `format_known_bug_locations` reads `function` /
  `symbol`, never `function_or_symbol`, so a known bug's function names are
  never shown to bug deduplication.
- `STAGE_REPORT_INSTRUCTION` (`linux_patch_review.rs:443`) asks for line
  numbers when known, while `third_party/prompts/kernel/inline-template.md`
  (198-206) says never to quote them.

## Feynman Summary

A finding points at code to prove its point: "look at this file, this
line." When a review also has the kernel's source next to the driver being
reviewed, some of those pointers lead into the driver and some into the
kernel, but nothing says which, and some file names exist in both. The
checking stage then believed the earlier guess and checked the kernel's copy
of code the driver replaces, which never runs. This design makes every
pointer say which repository it is in, lists both trees' files and the
driver's own functions before the review starts, and has the checking stages
reject a pointer into the kernel's copy of something the driver has its own
version of, telling the model exactly which driver file to use instead.
Everything that displays or reuses those pointers shows the label and keeps
kernel paths out of places that expect driver paths.
