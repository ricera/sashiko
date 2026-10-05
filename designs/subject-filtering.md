# Patch Subject Filtering

## Motivation
Users often want to run the AI reviewer only against specific patches in a patchset. Some patches might be minor refactors, documentation updates, or belong to subsystems they don't want AI to review right now. Providing a way to filter which patches get reviewed based on their subjects allows users to save tokens, time, and focus on the patches that matter most.

## Design

### Command Line Interface
We will extend `sashiko-cli submit` to accept two new list arguments:
- `--skip-subject <PATTERN>`: A glob pattern. Patches with subjects matching any of these patterns will be marked as `Skipped` and not reviewed by AI.
- `--only-subject <PATTERN>`: A glob pattern. If provided, *only* patches with subjects matching at least one of these patterns will be reviewed. Patches not matching any of the `only-subject` patterns will be skipped.

If both are specified, a patch must match an `only-subject` pattern and *must not* match a `skip-subject` pattern.

### API Changes
The `SubmitRequest` API models (`Inject`, `Remote`, `RemoteRange`) will be updated to include two new optional fields:
- `skip_subjects: Option<Vec<String>>`
- `only_subjects: Option<Vec<String>>`

### Database Changes
To preserve the filters for the review phase, the `patchsets` table will be updated:
- `skip_filters TEXT` (storing a JSON-serialized array)
- `only_filters TEXT` (storing a JSON-serialized array)

`PatchsetRow` will be updated to fetch and decode these values.

### Ingestion Flow
1. API receives `SubmitRequest` with filters.
2. For Mbox `Inject`, `Event::RawMboxSubmitted` carries the filters. Then they are passed into `ParsedArticle`, and finally `create_patchset` persists them to the database.
3. For `Remote` / `RemoteRange`, `create_fetching_patchset` persists the filters right at the API handler. When `FetchAgent` resolves the commits, `save_parsed_article` updates the patchset without overwriting the stored filters.

### Review Phase
In `Reviewer::review_patchset_task` (which orchestrates the AI review loops), we will extract `skip_filters` and `only_filters` from the current patchset.
Before calling `process_patch_review`, we evaluate each patch's subject against the glob patterns (translated into Regex internally).
If a patch is to be skipped:
1. It is marked as `Skipped` in the `patches` table via a new `update_patch_status` database method.
2. We `continue` to the next patch in the series, completely bypassing the AI generation call.
The overall patchset review can still conclude with a `Reviewed` status since the unskipped patches are processed normally.

## Patches that change no files
A patch whose stored diff is empty is also marked `Skipped`, without a
worker and without any subject filter.

### Reason
Range-submitting a `b4 prep` branch includes its empty tracker commit as
the first patch of the range. Before this skip, the tracker was reviewed
like any other patch: a worker was spawned for it, and every failed
attempt was retried up to `review.max_retries`, all to review nothing.

### Predicate
`diff_is_empty` in `src/reviewer.rs` is true when the stored diff is empty
after `trim()`. It tests the stored diff itself, not any header or path
inside it, so it never depends on a header format. Each patch source
stores a diff as follows:
- **Git range or forge PR/MR.** The fetcher stores what
  `git_ops::extract_patch_metadata` finds after its header marker in
  `git show` output. That is `""` for an empty commit, such as a `b4 prep`
  tracker or any `git commit --allow-empty`, and `"\n"` for a clean merge,
  which introduces nothing. Any commit that changes a file, a mode or a
  binary prints a non-empty diff, whatever its paths look like, and so does
  a merge that resolved a conflict (`diff --cc`).
- **Mailing list.** `parse_email` stores the whole body as the diff, and
  only when it found a diff in it. A stored mailing-list patch therefore
  never has an empty diff, so the check never skips one, git header or not.

The predicate deliberately does not use `extract_files_from_diff`, which
reads only `diff --git a/` lines. A C-quoted non-ASCII path
(`diff --git "a/\303\251.c" ...`), a combined merge diff (`diff --cc`) and a
plain unified diff with no git header (a quilt-style patch) all yield no
files there, yet each changes something and must be reviewed. For the
same reason, the ignored-files rule keeps its own `!files.is_empty()`
guard.

### Outcome
`process_patch_review` checks the predicate after the earlier-run checks
and before the ignored-files rule. When it holds, the patch is marked
`Skipped` through `update_patch_status(patch_id, PatchOutcome::Skipped)`,
and `process_patch_review` returns `PatchResult::Success`. A failed status
write is logged as a warning, as in the subject pre-filter.

No `reviews` row is written, as for the subject and size pre-filters in
`review_patchset_task`. The ignored-files rule is the one skip that
records a `Skipped` review row, with its reason. Because only `Reviewed`
rows count as successful reviews, the patch is checked again on a later
run, which costs one string scan.

### Scope
- The patchset can still end `Reviewed`. A skipped patch's
  `PatchResult::Success` is neutral, so a series of one tracker commit and
  one reviewed patch ends `Reviewed`, and so does a series made only of
  empty commits, as when every patch is skipped by a subject filter.
- A skipped patch can never make a series `Partial`.
- No worker is spawned, so no review activity is written for the patch.
- `GitWorktree::is_empty_commit` is unrelated. It asks whether a commit
  object in a worktree is empty, and the review path does not use it.
