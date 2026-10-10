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

//! Single declarative workflow definition for Sashiko's Linux Kernel Code Review.
//!
//! This module specifies the multi-stage review pipeline as a declarative [`Workflow`]
//! operating over [`LinuxPatchReviewState`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cover_letter::{CoverLetter, cover_letter_block};
use crate::prior_revisions::{PriorReviewContext, ground_prior_link};
use crate::settings::ReferenceKind;
use crate::toolbox::{REPO_ARG, REPO_ARG_REVIEW, ReferenceRepo};
use crate::workflow::graph::Workflow;
use crate::workflow::output::OutputFormat;
use crate::workflow::policy::{ParallelPolicy, RecitationPolicy, StagePolicy, ToolScope};
use crate::workflow::prompt::PromptTemplate;
use crate::workflow::stage::{ExecutableStage, Stage};

/// What the prompt says about a kernel reference configured without a
/// description.
const KERNEL_REFERENCE_DESCRIPTION: &str = "The Linux kernel tree the code under review builds \
     against. Verify API contracts, struct layouts and helper semantics by reading it rather than \
     recalling them -- your recollection is not pinned to this version.";

/// The prompt block describing the attached reference repositories, or
/// nothing when there are none.
///
/// Two blocks, because they say different things. The list applies to any
/// reference: the model has somewhere else to look, which it will not use
/// unless told, and each tree needs saying what it is for. The out-of-tree
/// caveats are about the repository under review being an out-of-tree kernel
/// module, and several in-tree conventions the model would otherwise enforce do
/// not exist there. Only a kernel reference implies that, so a configuration
/// with only a userspace tree such as rdma-core must not claim it: telling an
/// in-tree review that Fixes: tags do not apply would suppress real findings.
///
/// Only the references attached to this patch reach here; those left dormant
/// by their `applies_to` are not passed in. So a kernel entry scoped away from
/// a patch gives that patch neither its list entry nor the out-of-tree
/// caveats, and the patch is framed as in-tree. That is why the kernel entry of
/// an out-of-tree module should not be scoped.
fn reference_context_block(references: &[ReferenceRepo]) -> String {
    if references.is_empty() {
        return String::new();
    }
    let mut block = format!(
        "\n\n=== Read-only Reference Repositories ===\n\
         The change under review exists only in {REPO_ARG}=\"{REPO_ARG_REVIEW}\". The repositories \
         below are read-only context pinned for this review; pass {REPO_ARG}=\"<name>\" to any \
         git tool to read one. In them, \"HEAD\", \"@\" or an omitted revision means that \
         repository's pin. Never search them for the change itself.\n"
    );
    for r in references {
        let description = match (r.description.as_deref().map(str::trim), r.kind) {
            (Some(d), _) if !d.is_empty() => d.to_string(),
            (_, ReferenceKind::Kernel) => KERNEL_REFERENCE_DESCRIPTION.to_string(),
            // Validation requires a description for every other kind.
            _ => String::new(),
        };
        let short = r.commit.get(..12).unwrap_or(&r.commit);
        block.push_str(&format!(
            "- {REPO_ARG}=\"{}\" (pinned at {}, commit {short}): {}\n",
            r.name,
            r.revision,
            description.replace('\n', "\n  ")
        ));
    }
    block.push_str(
        "When you claim a contract with one of these repositories, cite repo:file:line. A \
         mismatch is a finding only if this patch introduces it, or changes one side of an \
         interface without accounting for existing users of the other side.\n\
         ===========================",
    );

    if let Some(kernel) = references.iter().find(|r| r.kind == ReferenceKind::Kernel) {
        block.push_str(&format!(
            "\n\n=== Out-of-Tree Module ===\n\
             The repository under review is an out-of-tree Linux kernel module, not a \
             mainline kernel tree. It contains the module's own sources and nothing else: \
             there is no include/linux/, no kernel core code, and no mainline history.\n\n\
             The kernel this module builds against is available as the read-only repository \
             {REPO_ARG}=\"{}\", pinned at {}. Verify API contracts, struct layouts, and helper \
             semantics by reading that tree rather than recalling them -- your recollection is \
             not pinned to this kernel version and may describe a different one.\n\n\
             Consequences of being out-of-tree, all of which change what counts as a \
             finding:\n\
             - Fixes: tags, lore threads, and 'look forward in git history for the fix' do \
             not apply. There is no mainline history in this repository to search, and a \
             missing Fixes: tag is not a defect here.\n\
             - Compatibility shims and kernel-version #ifdefs are expected in this \
             codebase; they exist because the module supports several kernel releases. Do \
             not report them as unnecessary complexity unless the specific logic is wrong.\n\
             - File paths will not match mainline paths. Do not conclude that code is \
             missing or misplaced from its path alone.\n\
             ===========================",
            kernel.name, kernel.revision
        ));
    }
    block
}

/// Complete execution state of a Linux kernel patch review.
#[derive(Clone, Debug, Default)]
pub struct LinuxPatchReviewState {
    pub ps_id: String,
    pub p_id: String,
    pub target_commit_sha: String,
    pub baseline_sha: String,
    pub target_commit_diff: String,
    pub target_commit_diff_only: String,
    pub prefetched_context: String,
    /// Source prefetch failed; stages must gather target context through Git tools.
    pub prefetch_failed: bool,
    pub series_range: Option<String>,
    pub follow_up_series_context: Option<String>,
    /// The series cover letter, given to the stages that read the commit
    /// message; `None` when the series has none.
    pub cover_letter: Option<CoverLetter>,
    /// What the earlier revisions of the series' pull request were told,
    /// given to the stages that declare `wants_prior_reviews`; `None` when
    /// no earlier revision has a finding to show.
    pub prior_reviews: Option<PriorReviewContext>,
    /// The read-only reference repositories attached to this review, in
    /// configured order. A kind "kernel" entry among them is what marks the
    /// review as out-of-tree: the repository has no mainline history and no
    /// kernel headers of its own.
    pub references: Vec<ReferenceRepo>,

    /// Subsystem guide markdown files selected during the pre-screen and
    /// shared with every stage.
    pub selected_guides: Vec<String>,
    /// Optional manual stages filter (e.g. `--stages goal,locking`).
    pub manual_stages: Option<Vec<String>>,
    /// Caller-supplied instructions appended to the shared system prompt.
    pub custom_prompt: Option<String>,
    /// Stages selected by dynamic planning (or overridden by manual_stages).
    pub planned_stages: Vec<String>,
    /// Skip plain-text report and summary generation stages (e.g. in `--agent` mode).
    pub skip_report: bool,

    /// Aggregated raw concerns collected from the analysis stages.
    pub all_concerns: Vec<Value>,
    /// Aggregated raw dismissed concerns collected from the analysis stages.
    pub all_dismissed_concerns: Vec<Value>,

    /// Deduplicated concerns from the deduplication stage.
    pub deduplicated_concerns: Vec<Value>,
    /// Deduplicated dismissed concerns from the deduplication stage.
    pub deduplicated_dismissed_concerns: Vec<Value>,

    /// Filtered patch-introduced concerns after the conflict-resolution stage.
    pub patch_concerns: Vec<Value>,
    /// Candidate pre-existing concerns extracted after the conflict-resolution
    /// stage for separate processing.
    pub concerns: Vec<Value>,

    /// Verified findings from the verification stage.
    pub findings: Vec<Value>,

    /// Concise plain-text summary of the change generated at the end of review.
    pub summary: String,
    /// Generated LKML plain-text review from the report stage.
    pub review_inline: String,
    /// Fix suggestions.
    pub fixes: String,
}

// ---------------------------------------------------------------------------
// Typed Output Structures for Stage Serialization
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct PrescreenOutput {
    pub selected_prompts: Vec<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct PlanningOutput {
    pub relevant_stages: Vec<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct StageConcernsOutput {
    pub concerns: Vec<Value>,
    #[serde(default)]
    pub dismissed_concerns: Vec<Value>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct ConflictResolutionOutput {
    pub concerns: Vec<Value>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct VerificationOutput {
    pub findings: Vec<Value>,
}

// ---------------------------------------------------------------------------
// Common System Prompt Template
// ---------------------------------------------------------------------------

/// Where the system prompt carries the series cover letter: only in the
/// variant with the commit message, because the cover letter is the same
/// kind of information, the author's statement of intent, and the stages
/// that read the hunks on their own terms get neither.
pub(crate) fn cover_letter_placeholder(use_log: bool) -> &'static str {
    if use_log {
        "{{cover_letter_block}}"
    } else {
        ""
    }
}

pub fn linux_system_prompt(use_log: bool) -> PromptTemplate<LinuxPatchReviewState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
    let diff_var = if use_log {
        "{{target_commit_diff}}"
    } else {
        "{{target_commit_diff_only}}"
    };
    let cover_letter_var = cover_letter_placeholder(use_log);

    PromptTemplate::<LinuxPatchReviewState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references (e.g., 'today', 'last week', 'next year') strictly on this current date.

You are an expert Linux kernel maintainer. Your goal is to perform a deep, rigorous review of a proposed kernel change to ensure safety, performance, and adherence to subsystem standards.

TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.

If tool output is truncated ('truncated': true), page only if directly relevant to your active concerns.

<global_review_guidelines>
The following documents contain the official technical patterns, architectural rules, and subsystem-specific guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying anti-patterns and violations.
@includes
</global_review_guidelines>

=== Active Git Metadata ===
Target Commit SHA: {{{{target_commit_sha}}}}
Baseline SHA: {{{{baseline_sha}}}}
==========================={{{{reference_context_block}}}}{cover_letter_var}

Target Commit:
{diff_var}
{{{{prefetched_block}}}}{{{{custom_prompt_block}}}}"#
    ))
    .with_var("target_commit_sha", |s: &LinuxPatchReviewState| s.target_commit_sha.clone())
    .with_var("baseline_sha", |s: &LinuxPatchReviewState| s.baseline_sha.clone())
    .with_var("target_commit_diff", |s: &LinuxPatchReviewState| s.target_commit_diff.clone())
    .with_var("target_commit_diff_only", |s: &LinuxPatchReviewState| s.target_commit_diff_only.clone())
    .with_var("reference_context_block", |s: &LinuxPatchReviewState| {
        reference_context_block(&s.references)
    })
    .with_var("cover_letter_block", |s: &LinuxPatchReviewState| {
        cover_letter_block(s.cover_letter.as_ref())
    })
    .with_var("prefetched_block", |s: &LinuxPatchReviewState| {
        if s.prefetch_failed {
            format!(
                "\n\nAutomatic source prefetch failed for target commit {}. Before analyzing the code, use git_read_files and git_grep at that revision to gather the source context. Do not infer source contents from the physical checkout.\n",
                s.target_commit_sha
            )
        } else if s.prefetched_context.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<pre_fetched_context>\nThe following source excerpts were fetched from the target commit identified by Source revision below, based on the modified lines in the patch. They include modified definitions and selected dependencies. Parent and series-final revisions must be inspected separately with Git tools.\nIf it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n{}\n</pre_fetched_context>",
                s.prefetched_context
            )
        }
    })
    .with_var("custom_prompt_block", |s: &LinuxPatchReviewState| {
        s.custom_prompt.as_deref().map(str::trim).filter(|p| !p.is_empty()).map_or_else(String::new, |p| {
            format!("\n\n<custom_instructions>\n{p}\n</custom_instructions>")
        })
    })
    .include_files_from_state(|s: &LinuxPatchReviewState| {
        let mut paths = Vec::new();
        if !s.selected_guides.is_empty() {
            for guide in &s.selected_guides {
                paths.push(PathBuf::from("subsystem").join(guide));
                paths.push(PathBuf::from("patterns").join(guide));
            }
        }
        paths
    })
}

// ---------------------------------------------------------------------------
// Stage Builders
// ---------------------------------------------------------------------------

const STAGE_GOAL_INSTRUCTION: &str = r#"# Analyze commit main goal

You are a senior Linux kernel maintainer evaluating the high-level intent of a proposed commit. Analyze the commit message and the conceptual change. Focus on the big picture: Are there architectural flaws, UAPI breakages, backwards compatibility issues, or fundamentally flawed concepts? Consider the long-term maintainability and system-wide implications of this design. If the core idea is dangerous, incorrect, or violates established kernel principles, raise a concern. Be open-minded but thorough; question assumptions made by the author and consider alternative, simpler designs."#;

const STAGE_IMPLEMENTATION_INSTRUCTION: &str = r#"# High-level implementation verification

You are verifying if the provided code changes actually implement what the commit message claims. Look for undocumented side-effects, missing pieces (e.g., a core change without updating corresponding callers, or changing a struct without updating all initializers), and unhandled corner cases related to the feature's logic. Explicitly check for missing API callbacks and interface omissions: when defining or modifying structures containing function pointers, verify that all logically required callbacks are implemented. Verify that all claims in the commit message are fully realized in the code. Identify any incomplete implementations, implicit behavioral changes, or API contract violations. Furthermore, verify that the logic is mathematically and semantically sound. Check for off-by-one errors in bounds, incorrect bitwise operations (e.g., bitwise arithmetic that incorrectly shifts values leading to overlapping masks or clobbering adjacent fields), and verify that all arguments passed to external subsystems (like kobjects or netdevs) are valid and semantically correct (e.g., non-empty strings, correct sizes, correct format specifiers). Don't trust the commit message without verifying each claim. Assume that the message might be incorrect or even intentionally malicious. Do not focus on low-level memory or locking errors yet."#;

const STAGE_EXECUTION_FLOW_INSTRUCTION: &str = r#"# Execution flow verification

You are a static analysis engine tracing execution flow in C or Rust code. Carefully trace the control flow of the provided patch. Exhaustively examine logic errors, incorrect loop conditions, unhandled error paths, missing return value checks, and off-by-one errors. Check every branch, switch statement, and conditional. Specifically look for missing teardown/restore of state in error paths (e.g. failing to cleanly restore global state or struct fields that were temporarily modified, such as resetting ID/minor values to -1 before returning on failure). Verify that mathematical operations, sizing, or rounding algorithms don't bypass capacity checks or enable out-of-bounds reads/writes. Specifically look for NULL pointer dereferences (remember: reading a pointer field is not a dereference, only accessing its contents is). Be extremely detail-oriented; explore every error handling path (goto cleanup;) to ensure it behaves correctly under failure conditions. Additionally, verify preprocessor macro correctness and spelling (e.g., ensuring CONFIG_ prefixes are used where expected instead of HAVE_). Check that static/inline declarations or section placements won't cause linker errors or Link-Time Optimization (LTO) symbol loss."#;

const STAGE_RESOURCES_INSTRUCTION: &str = r#"# Resource management

You are an expert in C and Rust resource management within the Linux kernel. Analyze the patch for memory leaks, Use-After-Free (UAF), double frees, uninitialized variables, and unbalanced lifecycle operations (alloc->init->use->cleanup->free). Pay special attention to error paths where resources might be leaked. Ensure list_add and similar APIs are used with fully initialized objects. Track the lifetime of every allocated struct and file descriptor. Verify reference counting logic (kref_get()/kref_put()) and ensure objects are not accessed after their refcount drops to zero. Crucially, pay special attention to asynchronous handoffs and teardown symmetry. Check if resources freed in one path are accessed in another concurrent or cleanup flow (UAF). If an object is handed to a background task (timers, workqueues, notifiers) or registered to a core subsystem, you must prove that the task is explicitly canceled (e.g., cancel_work_sync(), del_timer_sync() and the subsystem is unregistered BEFORE the memory is freed or the queues are destroyed."#;

const STAGE_LOCKING_INSTRUCTION: &str = r#"# Locking and synchronization

You are a world-class concurrency and locking expert auditing a Linux kernel patch.
Carefully review the proposed patch for ANY locking, concurrency, or synchronization bugs.
You MUST consider the following categories of issues and report any violations:
1. Sleeping in atomic context: Are there any calls to `mutex_lock`, `kzalloc` with `GFP_KERNEL`, `msleep`, `cond_resched`, `flush_workqueue`, `synchronize_rcu`, or `cancel_work_sync` while holding a spinlock, rwlock, or within an RCU read-side critical section (`rcu_read_lock`)?
2. Lock ordering and deadlocks: Are locks acquired in a different order than elsewhere? Does it acquire a mutex while holding another mutex that could cause AB-BA deadlocks? Are IRQs disabled (`spin_lock_irqsave`) when acquiring a lock that is used in hardirq context? Does it acquire a lock already held by a higher-level subsystem (e.g., ethtool)?
3. Race conditions and lockless access: Are shared variables, list entries, or pointers accessed without holding the appropriate lock (e.g., clearing pointers concurrently while others dereference them)? Are there missing memory barriers (`smp_mb`, `smp_wmb`, `smp_rmb`) when lockless access is intended (e.g. in lockless readers reading reused elements)? Are there TOCTOU races where a state is checked outside a lock but relied upon inside?
4. UAF / Locking Freed Memory: Are locks (`mutex_unlock`, `spin_unlock`) called on objects that have already been freed? Are works/timers destroyed before subsystems are unregistered, allowing new events to use freed works/timers? Is the protocol initialized flag set before private data is ready?
5. RCU rules: Is `list_splice_init` or similar non-RCU-safe operations used on RCU-protected lists? Is `list_for_each_rcu` used without `rcu_read_lock`?
6. Unprotected state modifications: Does the patch check state before acquiring the lock (e.g., checking power state before taking mutex)? Are hardware state, flags, or stats updated without proper protection?
7. Sequence counters: Are stats accumulations directly inside a `u64_stats_fetch_retry` loop leading to double counting? Is it possible for an interrupt to read a sequence counter while the interrupted context is modifying it (deadlock)?
8. Lock re-initialization: Does it re-initialize a lock that was already initialized, or destroy a lock on a failure path improperly?
9. Missing locking: Is a port or file exposed to userspace before the driver/TTY linking is complete? Are objects added to global/shared lists before they are fully initialized or their resources attached? Does a worker race with cleanup code leading to dropped/leaked frames?"#;

const STAGE_SECURITY_INSTRUCTION: &str = r#"# Security audit

You are a Red Team security researcher auditing a Linux kernel patch. Look for security vulnerabilities such as buffer overflows, out-of-bounds reads/writes, integer overflows, privilege escalation vectors, time-of-check to time-of-use (TOCTOU) races, and information leaks (e.g., copying uninitialized kernel memory to user-space via copy_to_user). Scrutinize all points where untrusted user input reaches sensitive functions without validation. Ensure all length checks and bounds checks are robust against malicious input. Focus heavily on attack surfaces and data boundaries."#;

const STAGE_HARDWARE_INSTRUCTION: &str = r#"# Hardware engineer's review

You are a hardware engineer reviewing device driver changes. If this patch touches driver or hardware-specific code, rigorously review register accesses, IRQ handling, DMA mapping/unmapping, memory barriers, and timing/delays. Look for missing dma_wmb()/dma_rmb() barriers, incorrect endianness conversions (cpu_to_le32), and unsafe DMA buffer allocations. Ensure the hardware state machine is handled correctly, especially during suspend/resume or device reset. Evaluate the physical state machine constraints: verify that clocks and power domains are enabled before registers are accessed, and that hardware rings/queues are actually initialized in the current hardware state before being unconditionally accessed. If the patch is purely generic software logic (e.g., VFS, core networking), return {"concerns": [], "dismissed_concerns": []}."#;

const STAGE_DEDUPLICATION_INSTRUCTION: &str = r#"# Deduplication and Consolidation

You are the lead reviewer consolidating feedback from multiple specialized analysts. You will be given lists of concerns and dismissed_concerns generated by different review stages.
Your task is to deduplicate identical or overlapping items in both lists.
1. Group concerns that refer to the same root cause or the same line of code.
2. Merge overlapping concerns into a single, comprehensive concern. Combine their reasonings if they complement each other.
3. Group dismissed_concerns that investigated and disproved the same candidate concern.
4. Merge overlapping dismissed_concerns into a single, comprehensive dismissed_concern. Combine their evidence if it complements each other.
5. Ensure the output contains only unique concerns and unique dismissed_concerns.
6. Preserve the `preexisting` flag for concerns. If you merge a pre-existing concern with a newly introduced one, flag it based on the root cause (if the root cause is new, it's not pre-existing).
7. SPECIFICITY REQUIREMENT: When merging concerns or dismissed_concerns, preserve and consolidate the most specific details: exact function names, file paths, line numbers when known, and triggering conditions. Never generalize a specific finding into a vague category.
8. Preserve and merge the `locations` arrays from the input concerns and dismissed_concerns. If multiple items describe the same root cause, keep the most precise file/function_or_symbol/line/code_snippet/why_this_location_matters locations. Do not invent line numbers; keep `line` as null when the exact line is not known.
9. dismissed_concerns do not need a `preexisting` flag."#;

const STAGE_CONFLICT_RESOLUTION_INSTRUCTION: &str = r#"# Concern/dismissed-concern conflict resolution

You are the lead reviewer reconciling consolidated concerns with consolidated dismissed_concerns.
Both `concerns` and `dismissed_concerns` are untrusted claims. Do not assume either side is correct. Treat both as hypotheses and verify them against the actual code before deciding whether to keep or discard a concern.
Your task is to identify whether any remaining concern conflicts with a dismissed_concern that investigated the same root cause, code path, or failure mode.
1. Compare each concern against the dismissed_concerns list and find conflicts or overlaps where one says the issue is real and the other says the same candidate issue is disproved.
2. For every conflict, inspect the actual code and reasoning to decide which side is correct.
3. If the concern is correct, keep it in the output. If the dismissed_concern is correct, discard that concern.
4. If there is no direct conflict for a concern, keep it unchanged.
5. Do not discard a concern merely because a dismissed_concern is vaguely related; only discard when the dismissed_concern's evidence concretely disproves that concern.
6. Preserve each retained concern's `type`, `description`, `reasoning`, `preexisting`, and `locations` fields.
7. LOCAL BOUNDARY RULE: Do not discard a defect within the modified code of the patch by assuming that surrounding caller systems, parallel execution, or legacy API layers will safely mask or prevent the issue, unless you can point to specific code that concretely proves the failure mode is structurally impossible. If you cannot prove the safety of the violation based on the specific code, you must keep the concern.
8. SYMMETRICAL PROOF BAR: A dismissed_concern cannot override or discard a concern unless the dismissed_concern cites a concrete disproving `code_snippet` in its `locations` and you have verified that disproving code in the diff or by reading the repository with tools (`git_read_files` or `git_grep`). Never discard a concern based on a dismissed_concern that relies on unverified assumptions about external callers, helpers, hardware bounds, or build configurations."#;

const STAGE_VERIFICATION_INSTRUCTION: &str = r#"# Verification and severity estimation

You are the lead reviewer validating consolidated concerns. You will be given a list of deduplicated concerns after conflict resolution.
1. Validate each concern and prove the provided reasoning. Report all valid concerns as findings. If necessary, use tools to gather additional material. Discard all false positives.
2. CRITICAL RULE: To discard a concern as a false positive, you MUST find concrete proof that explicitly invalidates the concern's reasoning. If you cannot find definitive proof that the concern is a false positive, it must be reported as a finding. If you're not sure about something and it's critical in the reasoning validation, make it obvious: if X is possible, then problem Y can occur. Always try to validate if X is possible yourself.
3. SERIES VALIDATION RULE: If follow-up patches in this series are provided in the context, check if each identified concern is resolved or fixed in the final state of the series. If the problem has been resolved, fixed, or the code was rewritten in a subsequent patch in this series, you MUST discard the concern and NOT report it as a finding. You MUST verify this by checking the actual code at the end of the series using tools; do not trust promises or claims in commit messages.
4. When referring to other patches within this series in your explanation, DO NOT use git hashes (they are ephemeral/unstable). Instead, refer to them by their patch subject (e.g., 'commit "mm: fix allocation"'). Existing historical commits in the tree should still be referenced by their standard hash.
5. Assign a severity (low, medium, high, critical) to each remaining valid finding, following the calibration guidance in the severity definitions: reason through consequence, triggering path, and reachability, and state that reasoning at the start of the finding's `severity_explanation` so the label is auditable. Raise the level for a bug reachable by untrusted or remote input, and do not lower it because you believe the code is unreachable. A finding you can only state speculatively is capped at medium but still reported, never dropped. Be rigorous in filtering out verifiable noise, but accurately report real logic flaws and edge cases.
6. If the problem is determined to have already existed in the code before the patch was applied, mark `"preexisting": true`. Pre-existing issues will be routed to a dedicated pipeline and separate review.
7. SPECIFICITY REQUIREMENT: Every finding MUST cite the exact function name(s), file path(s), line number(s) when known, and triggering conditions where the bug manifests. Vague descriptions like 'potential overflow in ring buffer calculations' are insufficient. State precisely which variable overflows, in which function, and under what input conditions. Do not invent line numbers; use `line: null` when the exact line is not known.
8. Carry forward the `locations` from the validated concern into each finding. If you gather better evidence, replace vague locations with the most precise verified locations. Do not invent line numbers; use null when exact values are unknown."#;

pub const STAGE_REPORT_INSTRUCTION: &str = r#"# LKML-friendly report generation

You are an automated review bot generating a report for the Linux Kernel Mailing List (LKML). Convert the provided JSON findings into a polite, standard, inline-commented LKML email reply.

Follow the formatting rules strictly. Do not use markdown headers or ALL CAPS shouting. Ensure the tone is constructive and professional. Do not use backticks to quote any names or expressions.

SPECIFICITY REQUIREMENT: Each inline comment MUST reference the exact function name, file, line number when known, and specific triggering condition. Prefer the finding's `locations` field when present. Do not produce vague summaries like 'potential issue in error handling'. State precisely what goes wrong, where, and under what circumstances. Do not invent line numbers; if the exact line is unavailable, anchor the comment to the nearest verified function or symbol and explain the triggering condition."#;

const STAGE_JSON_SCHEMA_EXAMPLE: &str = r#"
TodoWrite compatibility: vendored prompts may ask you to add tasks or suspected bugs to TodoWrite. Do not call or mention TodoWrite. Treat those instructions as an internal checklist only. If that checklist identifies a concrete suspected bug, carry it forward as a JSON concern with file, function_or_symbol, line when known, triggering condition, and evidence. Do not output generic checklist progress as a concern.

Once you have gathered sufficient information, return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
If you find no concerns and no dismissed concerns, return {"concerns": [], "dismissed_concerns": []}.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
- "type": A short category string.
- "description": A clear description of the problem.
- "reasoning": A step-by-step explanation.
- "preexisting": true if this bug already existed in the codebase before these patches were applied, false if the issue was newly introduced by the reviewed patchset.
- "locations": An array of objects, each containing "file", "function_or_symbol", "line", "code_snippet" and "why_this_location_matters".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations". They mean the same as above, except that "description" is the candidate concern that was investigated and disproved, "reasoning" is the evidence proving it does not apply, and "locations" MUST cite the concrete disproving code (the exact guard, lock, cleanup path, or caller/callee implementation that proves the issue cannot occur — not merely repeating the suspected line from the diff).

Use the 'dismissed_concerns' array ONLY for candidate concerns that you considered plausible, investigated, and disproved with concrete evidence. This is especially important when you first suspect a concern and then follow the evidence chain proving that it does NOT apply.

NO DISMISSAL WITHOUT VERIFIED PROOF: To place a candidate issue in 'dismissed_concerns' (or to discard a suspected issue), you MUST find concrete proof in the code ('file', 'function_or_symbol', 'line', and verbatim 'code_snippet' in 'locations') that explicitly invalidates the concern's reasoning. If the disproving code lives outside the diff (for example, in a caller, callee, macro, sysctl, or build script), you MUST verify that code first using tools ('git_read_files' or 'git_grep') and quote the verified disproving snippet in 'locations'. If you cannot find definitive code proof that the candidate issue is impossible, you MUST report it in 'concerns' (NOT 'dismissed_concerns') and make the condition explicit: if X is possible, then problem Y can occur.

SPECIFICITY REQUIREMENT: When reporting a concern or dismissed_concern, cite exact function name(s), file path(s), and line number(s) when known. Do not invent line numbers; use null when exact values are unknown.

CRITICAL REVIEW DIRECTIVE: Do NOT dismiss concerns just because you assume the surrounding system or caller handles it perfectly. Do not be overly charitable to the existing code. If there is a missing initialization, an unhandled edge case, or a brittle logic flow, report it as a concern immediately. Assume the worst-case scenario where external inputs and caller states are malformed.

Example Output:
```json
{
  "concerns": [
    {
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }
      ]
    }
  ],
  "dismissed_concerns": [
    {
      "type": "Resource Management",
      "description": "Possible missing cleanup when foo_init() fails after bar_alloc().",
      "reasoning": "The concrete code path or ordering that proves this candidate concern does not apply.",
      "locations": [
        {
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 125,
          "code_snippet": "safe_code_path();",
          "why_this_location_matters": "This is where the cleanup path proves the candidate leak does not apply."
        }
      ]
    }
  ]
}
```"#;

// ---------------------------------------------------------------------------
// Validation Logic
// ---------------------------------------------------------------------------

fn has_valid_proof_location(item: &Value) -> bool {
    let Some(locations) = item.get("locations").and_then(Value::as_array) else {
        return false;
    };
    locations.iter().any(|loc| {
        let has_file = loc
            .get("file")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_symbol = loc
            .get("function_or_symbol")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_snippet = loc
            .get("code_snippet")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        has_file && has_symbol && has_snippet
    })
}

fn validate_concerns_output(
    output: &StageConcernsOutput,
    _state: &LinuxPatchReviewState,
) -> Result<(), String> {
    for (idx, concern) in output.concerns.iter().enumerate() {
        let has_desc = concern
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_reasoning = concern
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_desc || !has_reasoning {
            return Err(format!(
                "concerns[{idx}] must have non-empty 'description' and 'reasoning' strings."
            ));
        }
    }

    for (idx, dismissed) in output.dismissed_concerns.iter().enumerate() {
        let has_desc = dismissed
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_reasoning = dismissed
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_desc || !has_reasoning {
            return Err(format!(
                "dismissed_concerns[{idx}] must have non-empty 'description' and 'reasoning' strings proving why the candidate concern does not apply."
            ));
        }
        if !has_valid_proof_location(dismissed) {
            return Err(format!(
                "dismissed_concerns[{idx}] must include at least one entry in 'locations' with non-empty 'file', 'function_or_symbol', and verbatim disproving 'code_snippet' proving the candidate concern cannot occur. If you do not have concrete code proof, move the candidate issue to 'concerns' instead."
            ));
        }
    }

    Ok(())
}

fn format_concerns_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'concerns' and 'dismissed_concerns' arrays. If there are no concerns and no dismissed concerns, return `{{\"concerns\": [], \"dismissed_concerns\": []}}`.",
        violation
    )
}

fn validate_inline_format(content: &str, _state: &LinuxPatchReviewState) -> Result<(), String> {
    if content.lines().any(|l| l.trim_start().starts_with("```")) {
        return Err("The output contains Markdown code blocks ('```'). It must be plain text as per `inline-template.md`.".to_string());
    }
    if !content.lines().any(|l| l.trim_start().starts_with('>')) {
        return Err("The output does not appear to quote any code or context using '>'. Please follow the quoting style in `inline-template.md`.".to_string());
    }
    let has_commit_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("commit "));
    if !has_commit_header {
        return Err("The output is missing the 'commit <hash>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`.".to_string());
    }
    let has_author_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("author:"));
    if !has_author_header {
        return Err("The output is missing the 'Author: <name>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`.".to_string());
    }
    let has_comments = content.lines().any(|l| {
        let trimmed = l.trim();
        if trimmed.is_empty() || trimmed.starts_with('>') {
            return false;
        }
        let lower = trimmed.to_lowercase();
        !lower.starts_with("commit ")
            && !lower.starts_with("author:")
            && !lower.starts_with("date:")
            && !lower.starts_with("link:")
    });
    if !has_comments {
        return Err("The output appears to lack any comments or summary. You must include a summary and interspersed comments explaining the findings.".to_string());
    }
    Ok(())
}

fn format_inline_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Please fix the formatting to match the standard plain text LKML review format with proper headers and '> ' quoted context.",
        violation
    )
}

fn append_stage_items(
    dest: &mut Vec<Value>,
    src: &[Value],
    stage: &str,
    default_type: &str,
    _key: &str,
) {
    for item in src {
        let mut obj = item.clone();
        if let Some(map) = obj.as_object_mut() {
            if !map.contains_key("type")
                || map
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .is_empty()
            {
                map.insert("type".to_string(), json!(default_type));
            }
            map.insert("stage".to_string(), json!(stage));
        }
        dest.push(obj);
    }
}

fn append_stage_dismissed_concerns(dest: &mut Vec<Value>, src: &[Value], stage: &str) {
    for item in src {
        let mut obj = item.clone();
        if let Some(map) = obj.as_object_mut() {
            map.insert("stage".to_string(), json!(stage));
        }
        dest.push(obj);
    }
}

// ---------------------------------------------------------------------------
// Stage Definitions
// ---------------------------------------------------------------------------

pub fn prescreen_stage() -> Stage<LinuxPatchReviewState, PrescreenOutput> {
    Stage::builder("pre-screen")
        .system_prompt(PromptTemplate::<LinuxPatchReviewState>::new(
            "You are an AI assistant preparing a Linux kernel patch review.\nReview the provided Patch and select all potentially relevant subsystem guides from the index below.\nCRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the modified code. If there is any doubt, include the file.\n\nYou MUST respond with ONLY a JSON object, no other text. Example:\n```json\n{\"selected_prompts\": [\"networking.md\", \"locking.md\"]}\n```",
        ))
        .user_prompt(
            PromptTemplate::<LinuxPatchReviewState>::new(
                "<subsystem_guide_index>\n@include(\"subsystem/subsystem.md\")\n</subsystem_guide_index>\n\n<patch>\n{{target_commit_diff}}\n</patch>",
            )
            .with_var("target_commit_diff", |s: &LinuxPatchReviewState| s.target_commit_diff.clone())
            .include_file("subsystem/subsystem.md"),
        )
        .output_format(OutputFormat::json_with_schema(json!({
            "type": "object",
            "properties": {
                "selected_prompts": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "required": ["selected_prompts"]
        })))
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            ..Default::default()
        })
        .skip_if(|s| s.manual_stages.is_some())
        .reduce(|state, out: PrescreenOutput| {
            let prompts: Vec<String> = out
                .selected_prompts
                .into_iter()
                .filter(|name| !is_stage_exclusive_guide(name))
                .filter(|name| crate::workflows::guard::sanitize_guide_name(name))
                .collect();
            state.selected_guides = prompts;
        })

        .build()
}

pub fn planning_stage() -> Stage<LinuxPatchReviewState, PlanningOutput> {
    let optional_stages: Vec<&'static str> = ANALYSIS_STAGES
        .iter()
        .filter(|d| d.optional)
        .map(|d| d.name)
        .collect();

    Stage::builder("planning")
        .system_prompt(linux_system_prompt(true))
        .user_prompt(PromptTemplate::<LinuxPatchReviewState>::new(
            r#"Analyze the provided patch and determine which of the following review stages are relevant and should be executed:
- resources: Resource management
- locking: Locking and synchronization
- security: Security audit
- hardware: Hardware engineer's review

CRITICAL: Always err on the side of running more stages. If you are not absolutely sure, include the stage. If the patch is a trivial typo fix, you may omit some stages. Stages not listed above always run and should not be included in your answer.

You MUST respond with ONLY a JSON object, no other text. Use the names exactly as given above. Example:
```json
{"relevant_stages": ["resources", "locking", "security", "hardware"]}
```"#,
        ))
        .output_format(OutputFormat::json_with_schema(json!({
            "type": "object",
            "properties": {
                "relevant_stages": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": optional_stages,
                    }
                }
            },
            "required": ["relevant_stages"]
        })))
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            ..Default::default()
        })
        .skip_if(|s| s.manual_stages.is_some())
        .reduce(|state, out: PlanningOutput| {
            // The stages the planner is not asked about run regardless. Its
            // answer is then admitted only where it names an optional stage,
            // which is what stops a hallucinated name reaching the resolver.
            let mut stages: Vec<String> = ANALYSIS_STAGES
                .iter()
                .filter(|d| !d.optional)
                .map(|d| d.name.to_string())
                .collect();
            for raw_name in out.relevant_stages {
                if let Some(def) = analysis_stage_by_name(&raw_name) {
                    if def.optional && !stages.iter().any(|s| s == def.name) {
                        stages.push(def.name.to_string());
                    }
                } else {
                    tracing::warn!("Ignoring unknown planned review stage {:?}", raw_name);
                }
            }
            state.planned_stages = stages;
        })
        .build()
}

/// One analysis stage: everything that distinguishes it from its siblings.
///
/// The workflow already identifies stages by name, so `name` is the whole
/// identity: it is what `--stages` selects, what the planning stage returns,
/// what is attached to a concern to say which analyst raised it, and what the
/// progress display shows. Properties that used to be inferred from a stage's
/// number live here instead, where they cannot fall out of step with it.
pub struct AnalysisStage {
    /// Stable identifier. Lowercase, hyphenated, and never renamed casually:
    /// it appears in `--stages` and in stored review output.
    pub name: &'static str,
    /// Short label for the progress display.
    pub short: &'static str,
    pub instruction: &'static str,
    pub guides: &'static [&'static str],
    /// Whether the system prompt carries the commit message as well as the
    /// diff: the git show output with the changelog injected, rather than the
    /// hunks alone. Stages that judge the change against its stated intent
    /// need it; those reading the hunks on their own terms do not.
    pub uses_commit_log: bool,
    /// Whether the planning stage may leave this one out. The first three
    /// always run, so the planner is only ever asked about the rest.
    pub optional: bool,
    /// Whether the prompt carries the list of patches that follow this one in
    /// the series. See [`SERIES_CONTEXT_PLACEHOLDER`].
    pub wants_series_context: bool,
    /// Whether the prompt carries what earlier revisions of the pull request
    /// were told. No analysis stage does, so what the analysts raise is not
    /// anchored on what was raised before, and no analysis builder places
    /// the placeholder; a test checks every stage's rendered prompt against
    /// this flag. See [`PRIOR_REVIEWS_PLACEHOLDER`].
    pub wants_prior_reviews: bool,
}

pub static ANALYSIS_STAGES: &[AnalysisStage] = &[
    AnalysisStage {
        name: "goal",
        short: "Goal Analysis",
        instruction: STAGE_GOAL_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: false,
        wants_series_context: false,
        wants_prior_reviews: false,
    },
    AnalysisStage {
        name: "implementation",
        short: "Implementation",
        instruction: STAGE_IMPLEMENTATION_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: false,
        wants_series_context: false,
        wants_prior_reviews: false,
    },
    AnalysisStage {
        name: "execution-flow",
        short: "Execution Flow",
        instruction: STAGE_EXECUTION_FLOW_INSTRUCTION,
        guides: &["callstack.md", "technical-patterns.md"],
        uses_commit_log: false,
        optional: false,
        wants_series_context: false,
        wants_prior_reviews: false,
    },
    AnalysisStage {
        name: "resources",
        short: "Resource Mgmt",
        instruction: STAGE_RESOURCES_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
        wants_prior_reviews: false,
    },
    AnalysisStage {
        name: "locking",
        short: "Locking & Sync",
        instruction: STAGE_LOCKING_INSTRUCTION,
        guides: &["subsystem/locking.md"],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
        wants_prior_reviews: false,
    },
    AnalysisStage {
        name: "security",
        short: "Security Audit",
        instruction: STAGE_SECURITY_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
        wants_prior_reviews: false,
    },
    AnalysisStage {
        name: "hardware",
        short: "Hardware Review",
        instruction: STAGE_HARDWARE_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: true,
        wants_series_context: false,
        wants_prior_reviews: false,
    },
];

/// The consolidation stages, in the order the workflow runs them. They take no
/// per-stage configuration, so a name and a display label is all there is to
/// hold, but holding it once keeps the list from being restated wherever a
/// stage name has to be recognised or shown.
pub struct ConsolidationStage {
    pub name: &'static str,
    pub short: &'static str,
    /// Whether the prompt carries the list of patches that follow this one in
    /// the series. See [`SERIES_CONTEXT_PLACEHOLDER`].
    pub wants_series_context: bool,
    /// Whether the prompt carries what earlier revisions of the pull request
    /// were told. Only verification's builder places the placeholder; a test
    /// checks every stage's rendered prompt against this flag. See
    /// [`PRIOR_REVIEWS_PLACEHOLDER`]. The report is not shown that block; it
    /// is given only the earlier findings its findings link to, through
    /// [`PRIOR_LINKS_PLACEHOLDER`].
    pub wants_prior_reviews: bool,
}

pub static DEDUPLICATION: ConsolidationStage = ConsolidationStage {
    name: "deduplication",
    short: "Deduplication",
    wants_series_context: false,
    wants_prior_reviews: false,
};

pub static CONFLICT_RESOLUTION: ConsolidationStage = ConsolidationStage {
    name: "conflict-resolution",
    short: "Conflict Resolution",
    wants_series_context: false,
    wants_prior_reviews: false,
};

pub static VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "verification",
    short: "Severity Estimation",
    wants_series_context: true,
    wants_prior_reviews: true,
};

pub static REPORT: ConsolidationStage = ConsolidationStage {
    name: "report",
    short: "Report Generation",
    wants_series_context: false,
    wants_prior_reviews: false,
};

/// In the order the workflow runs them. Each builder refers to its own
/// definition above, so the name a stage registers under is the same string
/// this list recognises and labels.
pub static CONSOLIDATION_STAGES: &[&ConsolidationStage] =
    &[&DEDUPLICATION, &CONFLICT_RESOLUTION, &VERIFICATION, &REPORT];

/// Marks where a stage's prompt carries the list of patches that follow this
/// one in the series.
///
/// Two kinds of question need it. Where a test belongs in a series is only
/// answerable from what comes after it, and whether a concern still stands can
/// depend on a later patch reworking the code it is about. Both are declared in
/// the stage tables rather than wired up per builder, so the placeholder and
/// the variable that fills it cannot get separated.
pub const SERIES_CONTEXT_PLACEHOLDER: &str = "{{follow_up_series_section}}";

fn series_context_placeholder(wants: bool) -> &'static str {
    if wants {
        SERIES_CONTEXT_PLACEHOLDER
    } else {
        ""
    }
}

fn with_series_context(
    template: PromptTemplate<LinuxPatchReviewState>,
    wants: bool,
) -> PromptTemplate<LinuxPatchReviewState> {
    if !wants {
        return template;
    }
    template.with_var("follow_up_series_section", |s: &LinuxPatchReviewState| {
        s.follow_up_series_context
            .as_ref()
            .map(|ctx| format!("\n\n{}", ctx))
            .unwrap_or_default()
    })
}

/// Marks where a stage's prompt carries what the earlier revisions of the
/// pull request were told: the block of their findings, then the rules for
/// using it.
///
/// Only verification reads it. Whether a concern repeats an earlier finding
/// is a question about concerns that are already raised, and the analysis
/// stages go without it, so what they raise is not anchored on what was
/// raised before. Like the series context it is declared in the stage
/// tables: verification's builder reads its flag both to place the
/// placeholder and to bind the variable that fills it, so the two cannot get
/// separated. No other builder places it, and a test fails when a stage's
/// rendered prompt and its flag disagree. Both workflows share the state
/// type, so they share this placeholder and [`with_prior_reviews`].
pub const PRIOR_REVIEWS_PLACEHOLDER: &str = "{{prior_review_section}}";

pub(crate) fn prior_reviews_placeholder(wants: bool) -> &'static str {
    if wants { PRIOR_REVIEWS_PLACEHOLDER } else { "" }
}

/// Fills [`PRIOR_REVIEWS_PLACEHOLDER`] with the block of earlier findings
/// followed by `rules`, the stage's instructions for using it. Both are
/// left out when no earlier revision has a finding to show, so the prompt
/// of a first revision, or of anything that is not a pull request, is the
/// prompt it was before the block existed.
pub(crate) fn with_prior_reviews(
    template: PromptTemplate<LinuxPatchReviewState>,
    wants: bool,
    rules: &'static str,
) -> PromptTemplate<LinuxPatchReviewState> {
    if !wants {
        return template;
    }
    template.with_var("prior_review_section", move |s: &LinuxPatchReviewState| {
        s.prior_reviews
            .as_ref()
            .map(|context| format!("\n\n{}\n\n{rules}", context.rendered))
            .unwrap_or_default()
    })
}

/// Builds the verification rules for the earlier findings, which both
/// workflows share except for where rule 4 puts the history in the severity
/// explanation: each workflow names the sections of its own severity rubric,
/// so `history_at` is the section the history sentence ends and `keep_first`
/// the sentence that keeps the rubric's first section first. The rules
/// change nothing about how a concern is judged; the commits the block
/// names show what earlier revisions contained, and since a commit carried
/// over unchanged can also be in the series under review, the series and
/// its final state come only from the follow-up patches and series end
/// commit the context provides, never from the block; code an earlier
/// revision's other patch adds never stands in for a later patch of the
/// series under review; and the block's commits are read only to compare
/// the earlier code with the current code for a repeat or a fix
/// regression; a finding that repeats an
/// earlier one, or that the fix for an earlier one caused, says so in a
/// `prior` key naming it as the block listed it, and in one sentence in its
/// severity explanation rather than in its title; and an earlier finding
/// that no concern raises is not reported.
///
/// A macro rather than a function, so that each workflow's rules stay one
/// `&'static str` built at compile time.
macro_rules! prior_reviews_verification_rules {
    (history_at: $history_at:literal, keep_first: $keep_first:literal $(,)?) => {
        concat!(
            "PRIOR REVISION RULES: The findings \
     above were reported on earlier revisions of this pull request. Use them as follows.
1. Judge every concern on the current code, by the rules above. That a concern was reported \
     before makes it neither more nor less likely to be valid, and an earlier finding proves \
     nothing about the code in front of you. The commits named in \"Findings From Earlier \
     Revisions of This Pull Request\" above show what earlier revisions contained. A commit \
     carried over unchanged can also be part of the series under review, so that the block names \
     a commit says nothing either way: when you apply the SERIES VALIDATION RULE, or otherwise \
     decide whether this series fixes or changes a concern, take the series under review and its \
     final state only from \"Follow-Up Patches in Series\" and \"Series End Commit (Final \
     State)\" when they are provided in the context, never from the block. Code at a commit the \
     block names, including another patch of that revision, does not show how the series under \
     review handles a concern, even when it uses what the patch under review defines; without \
     those two sections no later patch of the series under review is available, so dismiss no \
     concern as handled by one. Beyond that, use a commit the block names only to compare the \
     earlier revision's code with the current code, under REPEAT or FIX REGRESSION below.
2. REPEAT: if a concern is the same defect as an earlier finding and the current code still has \
     it, add \"prior\": {\"relation\": \"repeat\", \"revision\": \"<the label it was listed under, e.g. \
     v3>\", \"earlier_problem\": \"<the earlier finding's title, copied exactly as listed>\"}.
3. FIX REGRESSION: if the code a concern is about was added or changed in response to an \
     earlier finding -- that finding no longer applies, and the change that resolved it creates \
     this defect -- add \"prior\" with \"relation\": \"fix_regression\". Establish it by comparing \
     the earlier revision's code (its diff above, or the commit named in its patch header, read \
     with the git tools; that commit may no longer exist) with the current code; similar wording \
     between two findings is not enough.
4. Keep \"problem\" the short title the rules above ask for; do not say in it that the issue \
     was reported before. Say it in \"severity_explanation\" instead, at the end of its ",
            $history_at,
            ", in one sentence: for a repeat, that this path was reported on <revision> \
     and what, if anything, the current revision changed around it; for a fix regression, which \
     change made for the earlier finding leads to this defect, and how. ",
            $keep_first,
            "
5. Omit \"prior\" when no finding above is the counterpart. \"prior\" may name only a finding \
     listed above, under the label it is listed under. Do not report an earlier finding that no \
     concern in this review raises: this stage validates concerns, it does not revive them.
\"prior\" is the only addition to the findings format above; every other key is unchanged."
        )
    };
}
pub(crate) use prior_reviews_verification_rules;

/// How Linux verification uses the earlier findings (see
/// [`prior_reviews_verification_rules`]): the history sentence ends the
/// Triggering path, and the Consequence stays first, as the kernel rubric
/// and the example output order them.
pub(crate) const PRIOR_REVIEWS_VERIFICATION_RULES: &str = prior_reviews_verification_rules!(
    history_at: "Triggering path",
    keep_first: "Keep the Consequence first, as the rules above ask.",
);

/// Marks where the report's prompt carries the earlier findings that its
/// findings' `prior` links name, then the rules for stating those links.
///
/// The report is not shown the block of earlier findings: it states links
/// verification made, so it is given only the findings they name, with the
/// commit, location and consequence a comment needs to say what came back
/// or what not to undo. It is filled from the review's state rather than
/// declared in the stage tables, since what it holds depends on the
/// findings verification kept, not on the stage. Without a link it is
/// empty, so a report with no linked finding gets the prompt it had before.
/// Both workflows share the state type, so they share this placeholder and
/// [`with_prior_links`]; each passes its own rules.
pub const PRIOR_LINKS_PLACEHOLDER: &str = "{{prior_links_section}}";

/// Fills [`PRIOR_LINKS_PLACEHOLDER`] with the block of the earlier findings
/// that the review's findings link to, followed by `rules`, the report's
/// instructions for stating the links. Both are left out when no finding
/// links to an earlier one.
pub(crate) fn with_prior_links(
    template: PromptTemplate<LinuxPatchReviewState>,
    rules: &'static str,
) -> PromptTemplate<LinuxPatchReviewState> {
    template.with_var("prior_links_section", move |s: &LinuxPatchReviewState| {
        s.prior_reviews
            .as_ref()
            .and_then(|context| context.render_linked(&s.findings))
            .map(|block| format!("\n\n{block}\n\n{rules}"))
            .unwrap_or_default()
    })
}

/// How the Linux report states a finding's link to an earlier finding: the
/// inline comment opens with it, and a fix regression's suggested fix may
/// not undo the fix for the earlier finding. A finding without a link says
/// nothing about earlier revisions.
pub(crate) const PRIOR_LINKS_REPORT_RULES: &str = "PRIOR REVISION RULES: A finding may carry \
     \"prior\", linking it to a finding reported on an earlier revision of this pull request, \
     listed above. In these rules, <revision> and <earlier_problem> are the values of that \
     finding's \"prior\".
- \"relation\": \"repeat\": begin that comment by saying the issue was raised on <revision> \
     (\"<earlier_problem>\") and is still present.
- \"relation\": \"fix_regression\": begin that comment by saying the issue appears to come \
     from the change made for <revision>'s \"<earlier_problem>\". Any fix you suggest must \
     resolve both this issue and the earlier one; do not suggest going back to the code the \
     earlier finding was about.
Findings without \"prior\" are new; do not mention earlier revisions for them.";

/// Whether verification marked a finding as present before the patchset,
/// which sends it to the concerns for the bug pipeline instead of the
/// findings.
fn is_preexisting(finding: &Value) -> bool {
    finding
        .get("preexisting")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether a review is shown the findings of earlier revisions of its pull
/// request. The worker knows it from the review's state before it builds the
/// workflow, and verification checks the `prior` links of a review that is
/// and attaches no validator to one that is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PriorHistory {
    /// The review was shown no earlier findings, so no link can name one.
    #[default]
    NotShown,
    /// The review was shown a block of earlier findings.
    Shown,
}

impl PriorHistory {
    /// The history of a review given the block of earlier findings its
    /// state carries.
    pub fn of(prior_reviews: Option<&PriorReviewContext>) -> Self {
        match prior_reviews {
            Some(_) => Self::Shown,
            None => Self::NotShown,
        }
    }
}

/// Verification's output format and validation attempts in both workflows.
///
/// A review shown earlier findings gets [`prior_link_validator`], which
/// rejects at most one output, and one validation attempt more than a
/// consolidation stage's default to pay for that rejection: the session
/// counts a validator's rejection and a JSON parse error alike, so without
/// it a link the reduce would have cleaned up could use up the attempt a
/// later parse error needed and fail the stage. A review shown none gets
/// neither.
pub(crate) fn verification_output_checks(
    history: PriorHistory,
) -> (
    OutputFormat<LinuxPatchReviewState, VerificationOutput>,
    usize,
) {
    let attempts = StagePolicy::consolidation().max_validation_attempts;
    match history {
        PriorHistory::NotShown => (OutputFormat::json(), attempts),
        PriorHistory::Shown => (
            OutputFormat::json().with_validator(prior_link_validator()),
            attempts + PRIOR_LINK_REJECTIONS,
        ),
    }
}

/// How many outputs [`prior_link_validator`] rejects in one patch review.
const PRIOR_LINK_REJECTIONS: usize = 1;

/// Verification's validator in both workflows when the review was shown
/// earlier findings: it checks only the `prior` links of the findings it
/// keeps, against the block of earlier findings the stage was shown.
///
/// A link that names no listed finding is rejected with feedback that names
/// the finding, what was wrong and what was listed, once per patch review:
/// the returned closure remembers its own rejection, and the workflow, and
/// so the closure, is built anew for each patch a worker reviews. A second
/// faulty output passes, and the reduce removes whatever still does not
/// ground ([`ground_prior_link`](crate::prior_revisions::ground_prior_link)).
/// The one rejection costs a feedback turn and one validation attempt,
/// which [`verification_output_checks`] adds to the stage's attempts.
fn prior_link_validator()
-> impl Fn(&VerificationOutput, &LinuxPatchReviewState) -> Result<(), String> + Send + Sync + 'static
{
    let rejected = AtomicBool::new(false);
    move |output, state| {
        let Some(context) = &state.prior_reviews else {
            return Ok(());
        };
        if rejected.load(Ordering::Relaxed) {
            return Ok(());
        }
        let kept = output.findings.iter().filter(|f| !is_preexisting(f));
        match context.link_feedback(kept) {
            Some(feedback) if !rejected.swap(true, Ordering::Relaxed) => Err(feedback),
            _ => Ok(()),
        }
    }
}

/// Verification's reduce in both workflows. A preexisting finding becomes a
/// concern with five keys, so it loses every other one, `prior` among them;
/// every other finding is kept, with its `prior` grounded against the block
/// of earlier findings the stage was shown, or removed.
pub(crate) fn reduce_verification(state: &mut LinuxPatchReviewState, out: VerificationOutput) {
    let mut new_findings = Vec::new();
    for mut finding in out.findings {
        if is_preexisting(&finding) {
            let concern = json!({
                "type": finding.get("problem").and_then(|v| v.as_str()).unwrap_or("Pre-existing Issue"),
                "description": finding.get("problem").and_then(|v| v.as_str()).unwrap_or(""),
                "reasoning": finding.get("severity_explanation").and_then(|v| v.as_str()).unwrap_or(""),
                "preexisting": true,
                "locations": finding.get("locations").cloned().unwrap_or(json!([])),
            });
            state.concerns.push(concern);
        } else {
            ground_prior_link(&mut finding, state.prior_reviews.as_ref());
            new_findings.push(finding);
        }
    }
    state.findings = new_findings;
}

use crate::workflows::guard::normalize_stage_name;

pub fn consolidation_stage_by_name(name: &str) -> Option<&'static ConsolidationStage> {
    let normalized = normalize_stage_name(name);
    CONSOLIDATION_STAGES
        .iter()
        .copied()
        .find(|s| s.name == normalized)
}

/// Display label for any stage the pipeline runs.
pub fn stage_short_label(name: &str) -> Option<&'static str> {
    if let Some(def) = analysis_stage_by_name(name) {
        return Some(def.short);
    }
    consolidation_stage_by_name(name).map(|s| s.short)
}

/// Rank of a stage in the order the pipeline runs them: the analysis stages in
/// table order, then the consolidation stages that always follow.
///
/// Stage identity is a name rather than a number, so listing stages in the
/// order a reader expects has to come from the table rather than from the
/// identifier. A name the tables do not know has no rank (`None`) rather than a
/// sentinel like `usize::MAX`, which a JSON reader in JavaScript could not
/// even hold exactly; callers sort unranked stages last.
pub fn stage_order(name: &str) -> Option<u32> {
    let normalized = normalize_stage_name(name);
    if let Some(i) = ANALYSIS_STAGES.iter().position(|s| s.name == normalized) {
        return Some(i as u32);
    }
    CONSOLIDATION_STAGES
        .iter()
        .position(|s| s.name == normalized)
        .map(|i| (ANALYSIS_STAGES.len() + i) as u32)
}

/// Whether a guide belongs to one stage rather than to the shared context.
///
/// The pre-screen offers a guide to the whole review, but a guide some stage
/// loads for itself would then arrive twice: once in that stage's user prompt
/// and again in every stage's system prompt. Deriving the answer from the
/// stage table means a guide claimed in the table is excluded by that fact
/// alone, with no second list to keep in step.
pub fn is_stage_exclusive_guide(name: &str) -> bool {
    ANALYSIS_STAGES
        .iter()
        .flat_map(|def| def.guides)
        .any(|guide| guide.rsplit('/').next() == Some(name))
}

pub fn analysis_stage_by_name(name: &str) -> Option<&'static AnalysisStage> {
    let normalized = normalize_stage_name(name);
    ANALYSIS_STAGES.iter().find(|s| s.name == normalized)
}

/// Every stage name a review can produce, analysis and consolidation alike,
/// for validating what a caller or the planner asked for.
pub fn is_known_stage(name: &str) -> bool {
    let normalized = normalize_stage_name(name);
    analysis_stage_by_name(&normalized).is_some()
        || consolidation_stage_by_name(&normalized).is_some()
        || matches!(normalized.as_str(), "pre-screen" | "planning")
}

fn analysis_stage(
    def: &'static AnalysisStage,
    max_turns: usize,
    temperature: f32,
) -> Box<dyn ExecutableStage<LinuxPatchReviewState>> {
    let mut user_template = PromptTemplate::<LinuxPatchReviewState>::new(format!(
        "{}\n\n{}{}",
        def.instruction,
        STAGE_JSON_SCHEMA_EXAMPLE,
        series_context_placeholder(def.wants_series_context)
    ));
    for guide in def.guides {
        user_template = user_template.include_file(*guide);
    }
    let user_template = with_series_context(user_template, def.wants_series_context);

    Box::new(
        Stage::builder(def.name)
            .system_prompt(linux_system_prompt(def.uses_commit_log))
            .user_prompt(user_template)
            .output_format(
                OutputFormat::json()
                    .with_validator(validate_concerns_output)
                    .with_feedback_formatter(format_concerns_feedback),
            )
            .policy(StagePolicy {
                tools: ToolScope::All,
                max_turns,
                temperature,
                ..Default::default()
            })
            .reduce(
                move |state: &mut LinuxPatchReviewState, out: StageConcernsOutput| {
                    append_stage_items(
                        &mut state.all_concerns,
                        &out.concerns,
                        def.name,
                        "General",
                        "description",
                    );
                    append_stage_dismissed_concerns(
                        &mut state.all_dismissed_concerns,
                        &out.dismissed_concerns,
                        def.name,
                    );
                },
            )
            .build(),
    )
}

pub fn resolve_analysis_stages_with_options(
    state: &LinuxPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<LinuxPatchReviewState>>> {
    let selected_stages: Vec<String> = if let Some(ref manual) = state.manual_stages {
        manual.clone()
    } else if !state.planned_stages.is_empty() {
        state.planned_stages.clone()
    } else {
        ANALYSIS_STAGES.iter().map(|d| d.name.to_string()).collect()
    };

    let mut stages = Vec::new();
    for name in selected_stages {
        match analysis_stage_by_name(&name) {
            Some(def) => stages.push(analysis_stage(def, max_turns, temperature)),
            // Previously an unrecognised entry was dropped in silence, so a
            // mistyped --stages looked like it had worked.
            None => tracing::warn!("Ignoring unknown review stage {:?}", name),
        }
    }
    stages
}

pub fn deduplication_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<LinuxPatchReviewState, StageConcernsOutput> {
    Stage::builder(DEDUPLICATION.name)
        .system_prompt(linux_system_prompt(true))
        .user_prompt(
            PromptTemplate::<LinuxPatchReviewState>::new(format!(
                r#"{STAGE_DEDUPLICATION_INSTRUCTION}

Aggregated Concerns:
{{{{aggregated_concerns}}}}

Aggregated Dismissed Concerns:
{{{{aggregated_dismissed_concerns}}}}

Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations".
Preserve the most precise location details from the input. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ],
  "dismissed_concerns": [
    {{
      "type": "Resource Management",
      "description": "Possible missing cleanup when foo_init() fails after bar_alloc().",
      "reasoning": "The concrete code path or ordering that proves this candidate concern does not apply.",
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 125,
          "code_snippet": "safe_code_path();",
          "why_this_location_matters": "This is where the cleanup path proves the candidate leak does not apply."
        }}
      ]
    }}
  ]
}}
```"#
            ))
            .with_var("aggregated_concerns", |s: &LinuxPatchReviewState| {
                serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
            })
            .with_var("aggregated_dismissed_concerns", |s: &LinuxPatchReviewState| {
                serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
            }),
        )
        .output_format(
            OutputFormat::json()
                .with_validator(validate_concerns_output)
                .with_feedback_formatter(format_concerns_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..StagePolicy::consolidation()
        })
        .reduce(|state, out: StageConcernsOutput| {
            state.deduplicated_concerns = out.concerns;
            state.deduplicated_dismissed_concerns = out.dismissed_concerns;
        })
        .build()
}

pub fn conflict_resolution_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<LinuxPatchReviewState, ConflictResolutionOutput> {
    Stage::builder(CONFLICT_RESOLUTION.name)
        .system_prompt(linux_system_prompt(true))
        .user_prompt(
            PromptTemplate::<LinuxPatchReviewState>::new(format!(
                r#"{STAGE_CONFLICT_RESOLUTION_INSTRUCTION}

Consolidated Concerns:
{{{{deduplicated_concerns}}}}

Consolidated Dismissed Concerns:
{{{{deduplicated_dismissed_concerns}}}}

Return ONLY a JSON object with a 'concerns' array containing the remaining concerns after resolving conflicts. Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Preserve the most precise locations from the retained concerns. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ]
}}
```"#
            ))
            .with_var("deduplicated_concerns", |s: &LinuxPatchReviewState| {
                serde_json::to_string_pretty(&s.deduplicated_concerns).unwrap_or_default()
            })
            .with_var("deduplicated_dismissed_concerns", |s: &LinuxPatchReviewState| {
                serde_json::to_string_pretty(&s.deduplicated_dismissed_concerns).unwrap_or_default()
            }),
        )
        .output_format(OutputFormat::json())
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..StagePolicy::consolidation()
        })
        .reduce(|state, out: ConflictResolutionOutput| {
            let mut new_concerns = Vec::new();
            let mut preexisting = Vec::new();
            for concern in out.concerns {
                let is_preexisting = concern
                    .get("preexisting")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if is_preexisting {
                    preexisting.push(concern);
                } else {
                    new_concerns.push(concern);
                }
            }
            state.patch_concerns = new_concerns;
            state.concerns = preexisting;
        })
        .build()
}

pub fn verification_stage(
    max_turns: usize,
    temperature: f32,
    history: PriorHistory,
) -> Stage<LinuxPatchReviewState, VerificationOutput> {
    let series_context = series_context_placeholder(VERIFICATION.wants_series_context);
    // After the Example Output and the two guides rather than beside the
    // series context: the rules say what may be reported as a finding and
    // refer to the guides as the rules above, so they are read after the
    // findings format and the guides they apply to. The guides are placed
    // where they were appended before, so a prompt without earlier findings
    // is unchanged.
    let prior_reviews = prior_reviews_placeholder(VERIFICATION.wants_prior_reviews);
    let user_prompt = with_series_context(
            PromptTemplate::<LinuxPatchReviewState>::new(format!(
                r#"{STAGE_VERIFICATION_INSTRUCTION}

CRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid (e.g., verifying the caller handles the edge case). If you cannot find concrete proof of safety, you must retain the concern.{series_context}

Consolidated Concerns:
{{{{patch_concerns}}}}

Return ONLY a JSON object with a 'findings' array. Each object in the 'findings' array MUST use exactly the following keys: "problem" (a short naming string containing the vulnerability description. BUG NAME RULES: 1) less than 80 characters, 2) preferably start with a short subsystem prefix like 'mm:' or 'bpf:', 3) NEVER use backquotes, 4) if referring to a function, use fn_name() format, 5) try to describe the root cause instead of the consequence of the problem), "speculative" (a boolean: true if you could not lay out a concrete triggering path because it rests on an unproven assumption, or on an ABI, register, or convention you might be misreading; the severity definitions cap a speculative finding at Medium. If any part of the triggering path rests on such an assumption, answer true. false if the triggering path is established from the code. Uncertainty about reachability or about the consequence alone does not make a finding speculative and never lowers its level), "severity" (a string: Low, Medium, High, or Critical), "severity_explanation" (a string detailing the reasoning and proof), "preexisting" (a boolean: true if the problem already existed in the codebase before these patches were applied, or false if it was newly introduced by the reviewed patchset), "locations" (an array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters). Carry forward the locations from the validated concern; if you gather better evidence, replace vague locations with the most precise verified locations. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "findings": [
    {{
      "problem": "mm: memory leak in func_x() due to unmet condition Y",
      "speculative": false,
      "severity": "High",
      "severity_explanation": "Consequence: the buffer allocated in func_x() leaks on every call that meets condition Y. Triggering path: a caller passes Y, func_x() returns early before kfree(). Reachability: reachable from the ioctl handler by an unprivileged user.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ]
}}
```@include("false-positive-guide.md")@include("severity.md"){prior_reviews}"#
            ))
            .include_file("false-positive-guide.md")
            .include_file("severity.md")
            .with_var("patch_concerns", |s: &LinuxPatchReviewState| {
                serde_json::to_string_pretty(&s.patch_concerns).unwrap_or_default()
            }),
            VERIFICATION.wants_series_context,
        );
    let (output_format, max_validation_attempts) = verification_output_checks(history);
    Stage::builder(VERIFICATION.name)
        .system_prompt(linux_system_prompt(true))
        .user_prompt(with_prior_reviews(
            user_prompt,
            VERIFICATION.wants_prior_reviews,
            PRIOR_REVIEWS_VERIFICATION_RULES,
        ))
        .output_format(output_format)
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            max_validation_attempts,
            ..StagePolicy::consolidation()
        })
        .reduce(reduce_verification)
        .build()
}

pub fn report_stage(max_turns: usize, temperature: f32) -> Stage<LinuxPatchReviewState, String> {
    Stage::builder(REPORT.name)
        .system_prompt(linux_system_prompt(true))
        .user_prompt(
            with_prior_links(
                PromptTemplate::<LinuxPatchReviewState>::new(format!(
                    r#"{STAGE_REPORT_INSTRUCTION}{PRIOR_LINKS_PLACEHOLDER}

Findings:
{{{{findings}}}}

Return raw text output, not JSON."#
                ))
                .include_file("inline-template.md")
                .with_var("findings", |s: &LinuxPatchReviewState| {
                    serde_json::to_string_pretty(&s.findings).unwrap_or_default()
                }),
                PRIOR_LINKS_REPORT_RULES,
            ),
        )
        .output_format(OutputFormat::text_with_validator(
            validate_inline_format,
            format_inline_feedback,
        ))
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            recitation_policy: RecitationPolicy::FallbackToFreeForm {
                reminder: "CRITICAL OVERRIDE: Your previous response was blocked by the API recitation filter for quoting the patch diff verbatim. Ignore the diff-quoting instructions in inline-template.md. Do NOT quote any code or diff lines with '>'. Instead, start with the Commit/Author/Subject headers, write the summary, and describe each finding directly by referencing file names, function names, and line numbers in plain prose.".to_string(),
            },
            ..StagePolicy::consolidation()
        })
        .skip_if(|s| s.skip_report)
        .reduce(|state, out: String| {
            state.review_inline = out;
        })
        .build()
}

// ---------------------------------------------------------------------------
// Complete Kernel Review Workflow Graph
// ---------------------------------------------------------------------------

/// Constructs the complete declarative workflow for Linux kernel patch review
/// of a patch shown no earlier findings.
pub fn build_linux_patch_review_workflow() -> Workflow<LinuxPatchReviewState> {
    build_linux_patch_review_workflow_with_options(20, 0.0, PriorHistory::NotShown)
}

/// Constructs the declarative workflow with custom per-stage interaction
/// limits and temperature, for a review whose earlier findings `history`
/// says whether it is shown.
pub fn build_linux_patch_review_workflow_with_options(
    max_turns: usize,
    temperature: f32,
    history: PriorHistory,
) -> Workflow<LinuxPatchReviewState> {
    Workflow::builder("linux_patch_review")
        .stage(prescreen_stage())
        .dynamic_parallel(
            planning_stage(),
            move |state| resolve_analysis_stages_with_options(state, max_turns, temperature),
            ParallelPolicy::BestEffort,
        )
        .early_exit_if(
            |s| s.all_concerns.is_empty(),
            "No concerns raised in initial analysis stages",
        )
        .stage(deduplication_stage(max_turns, temperature))
        .early_exit_if(
            |s| s.deduplicated_concerns.is_empty(),
            "No concerns remaining after deduplication",
        )
        .stage(conflict_resolution_stage(max_turns, temperature))
        .early_exit_if(
            |s| s.patch_concerns.is_empty(),
            "No concerns remaining after conflict resolution",
        )
        .stage(verification_stage(max_turns, temperature, history))
        .early_exit_if(
            |s| s.findings.is_empty(),
            "No findings validated in verification stage",
        )
        .stage(report_stage(max_turns, temperature))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding_view::CalibrationLabel;
    use regex::Regex;

    /// Which stages the review's deadline may stop decides what a salvage
    /// keeps. The stages that gather concerns stop; the ones that turn
    /// concerns into findings run on, and a new consolidation stage must
    /// opt out too, or the wind-down would stop it and leave nothing to
    /// show.
    #[test]
    fn test_only_the_stages_that_gather_concerns_can_be_interrupted() {
        // The default state resolves every analysis stage.
        let state = LinuxPatchReviewState::default();
        let stages = build_linux_patch_review_workflow().interruptible_stages(&state);

        let mut not_interruptible: Vec<&str> = stages
            .iter()
            .filter(|(_, interruptible)| !interruptible)
            .map(|(name, _)| *name)
            .collect();
        not_interruptible.sort_unstable();
        let mut expected = vec![
            "deduplication",
            "conflict-resolution",
            "verification",
            "report",
        ];
        expected.sort_unstable();
        assert_eq!(not_interruptible, expected);

        for (name, interruptible) in &stages {
            assert_eq!(
                *interruptible,
                consolidation_stage_by_name(name).is_none(),
                "{name}: only consolidation stages run past the deadline"
            );
        }
        let names: Vec<&str> = stages.iter().map(|(name, _)| *name).collect();
        for name in ["pre-screen", "planning"] {
            assert!(names.contains(&name), "{name} is listed");
        }
        for def in ANALYSIS_STAGES {
            assert!(names.contains(&def.name), "{} is listed", def.name);
        }
        for def in CONSOLIDATION_STAGES {
            assert!(names.contains(&def.name), "{} is listed", def.name);
        }
    }

    #[test]
    fn test_each_stage_declares_whether_it_needs_the_commit_message() {
        // This was a numeric range in a free function, which kept compiling
        // while meaning something else whenever the stages moved.
        for name in ["goal", "implementation", "hardware"] {
            assert!(
                analysis_stage_by_name(name).unwrap().uses_commit_log,
                "{name} judges the change against its stated intent"
            );
        }
        for name in ["execution-flow", "resources", "locking", "security"] {
            assert!(
                !analysis_stage_by_name(name).unwrap().uses_commit_log,
                "{name} reads the diff hunks on their own terms"
            );
        }
    }

    #[test]
    fn test_stage_names_are_unique_and_resolvable() {
        let mut seen = std::collections::BTreeSet::new();
        for def in ANALYSIS_STAGES {
            assert!(seen.insert(def.name), "duplicate stage name {}", def.name);
            assert!(is_known_stage(def.name));
            assert!(std::ptr::eq(analysis_stage_by_name(def.name).unwrap(), def));
        }
        assert!(analysis_stage_by_name("nonexistent").is_none());
        assert!(!is_known_stage("nonexistent"));
    }

    #[test]
    fn test_series_context_is_declared_in_the_tables() {
        // Both tables carry the flag because the need is not particular to
        // either kind of stage: verification asks whether a later patch
        // reworks the code a concern is about.
        assert!(
            consolidation_stage_by_name("verification")
                .unwrap()
                .wants_series_context
        );
        for def in CONSOLIDATION_STAGES
            .iter()
            .filter(|d| d.name != "verification")
        {
            assert!(!def.wants_series_context, "{} does not use it", def.name);
        }
        // The placeholder and the variable that fills it travel together, so a
        // stage that declares the flag cannot end up rendering it literally.
        assert_eq!(series_context_placeholder(true), SERIES_CONTEXT_PLACEHOLDER);
        assert_eq!(series_context_placeholder(false), "");
    }

    #[test]
    fn test_no_analysis_stage_asks_for_series_context_yet() {
        // Both builders honour the flag; nothing in the analysis table sets it
        // until a stage needs it. A stage that did would get the placeholder
        // and the variable together, never one without the other.
        for def in ANALYSIS_STAGES {
            assert!(!def.wants_series_context, "{} does not use it", def.name);
        }
    }

    #[test]
    fn test_stage_exclusive_guides_follow_the_stage_table() {
        // Claimed by a stage, so the pre-screen must not also broadcast them.
        assert!(is_stage_exclusive_guide("locking.md"));
        assert!(is_stage_exclusive_guide("callstack.md"));
        assert!(is_stage_exclusive_guide("technical-patterns.md"));

        // Not claimed by any stage: the pre-screen's to offer.
        assert!(!is_stage_exclusive_guide("mm-vma.md"));
        assert!(!is_stage_exclusive_guide("subsystem.md"));

        // Matched on the file name, since that is what the pre-screen returns,
        // while the table holds the path a stage includes it by.
        assert!(
            ANALYSIS_STAGES
                .iter()
                .any(|d| d.guides.contains(&"subsystem/locking.md"))
        );
        assert!(!is_stage_exclusive_guide("subsystem/locking.md"));
    }

    #[test]
    fn test_every_stage_the_workflow_builds_is_a_known_name() {
        // The builders name their stages with literals; this is what stops one
        // drifting from the table that has to recognise and display it.
        for name in [
            deduplication_stage(1, 1.0).name(),
            conflict_resolution_stage(1, 1.0).name(),
            verification_stage(1, 1.0, PriorHistory::NotShown).name(),
            report_stage(1, 1.0).name(),
        ] {
            assert!(is_known_stage(name), "{name} is not in any stage table");
            assert!(stage_short_label(name).is_some(), "{name} has no label");
        }
        assert!(is_known_stage(prescreen_stage().name()));
        assert!(is_known_stage(planning_stage().name()));
    }

    #[test]
    fn test_the_planner_is_only_asked_about_optional_stages() {
        // The prompt lists exactly the optional stages, so a name it returns
        // that is not one of them is a hallucination rather than a choice.
        let optional: Vec<&str> = ANALYSIS_STAGES
            .iter()
            .filter(|d| d.optional)
            .map(|d| d.name)
            .collect();
        assert_eq!(optional, ["resources", "locking", "security", "hardware"]);

        let required: Vec<&str> = ANALYSIS_STAGES
            .iter()
            .filter(|d| !d.optional)
            .map(|d| d.name)
            .collect();
        assert_eq!(required, ["goal", "implementation", "execution-flow"]);
    }

    #[test]
    fn test_planning_stage_schema_restricts_to_optional_stages() {
        if let OutputFormat::Json {
            schema: Some(ref s),
            ..
        } = planning_stage().output_format
        {
            let items_enum = s["properties"]["relevant_stages"]["items"]["enum"]
                .as_array()
                .expect("enum array in schema");
            let names: Vec<&str> = items_enum
                .iter()
                .map(|v| v.as_str().expect("string enum value"))
                .collect();
            let optional: Vec<&str> = ANALYSIS_STAGES
                .iter()
                .filter(|d| d.optional)
                .map(|d| d.name)
                .collect();
            assert_eq!(names, optional);
        } else {
            panic!("expected planning stage to use json_with_schema");
        }
    }

    #[test]
    fn test_stage_lookup_normalizes_casing_and_separators() {
        assert_eq!(
            analysis_stage_by_name("Locking").map(|d| d.name),
            Some("locking")
        );
        assert_eq!(
            analysis_stage_by_name(" locking ").map(|d| d.name),
            Some("locking")
        );
        assert_eq!(
            analysis_stage_by_name("stage_locking").map(|d| d.name),
            Some("locking")
        );
        assert_eq!(
            analysis_stage_by_name("stage-locking").map(|d| d.name),
            Some("locking")
        );
        assert_eq!(
            analysis_stage_by_name("execution_flow").map(|d| d.name),
            Some("execution-flow")
        );
        assert_eq!(
            analysis_stage_by_name("EXECUTION_FLOW").map(|d| d.name),
            Some("execution-flow")
        );
        assert_eq!(
            consolidation_stage_by_name("Conflict_Resolution").map(|d| d.name),
            Some("conflict-resolution")
        );
        assert_eq!(
            consolidation_stage_by_name("stage-verification").map(|d| d.name),
            Some("verification")
        );
        assert!(is_known_stage("Locking"));
        assert!(is_known_stage("execution_flow"));
        assert!(is_known_stage("stage_report"));
    }

    #[test]
    fn test_planning_stage_reduce_normalizes_and_canonicalizes() {
        let stage = planning_stage();
        let mut state = LinuxPatchReviewState::default();
        let output = PlanningOutput {
            relevant_stages: vec![
                "Locking".to_string(),
                "stage_resources".to_string(),
                "  security  ".to_string(),
                "nonexistent_stage".to_string(),
            ],
        };
        (stage.reducer)(&mut state, output);
        assert_eq!(
            state.planned_stages,
            vec![
                "goal",
                "implementation",
                "execution-flow",
                "locking",
                "resources",
                "security"
            ]
        );
    }

    #[test]
    fn test_analysis_stages_keep_the_guidance_the_schema_alone_does_not_carry() {
        // The vendored guides still tell the model to use TodoWrite, which no
        // longer exists, and the verification stage keeps an anti-charity
        // directive of its own. Both belong to the analysis stages as well.
        for required in [
            "Do not call or mention TodoWrite",
            "Do not be overly charitable to the existing code",
            "If you find no concerns and no dismissed concerns",
            "investigated, and disproved with concrete evidence",
            "\"preexisting\": true if this bug already existed",
            "\"reasoning\": A step-by-step explanation.",
            "the candidate concern that was investigated and disproved",
            "NO DISMISSAL WITHOUT VERIFIED PROOF",
            "MUST cite the concrete disproving code",
        ] {
            assert!(
                STAGE_JSON_SCHEMA_EXAMPLE.contains(required),
                "analysis stage guidance lost: {required}"
            );
        }
        assert!(
            STAGE_CONFLICT_RESOLUTION_INSTRUCTION.contains("SYMMETRICAL PROOF BAR"),
            "conflict-resolution stage must enforce symmetrical proof bar"
        );
    }

    #[test]
    fn test_validate_concerns_output_enforces_symmetrical_proof() {
        let state = LinuxPatchReviewState::default();

        // Empty output is valid.
        assert!(validate_concerns_output(&StageConcernsOutput::default(), &state).is_ok());

        // Valid concern and valid dismissed_concern with concrete disproving snippet.
        let valid_output = StageConcernsOutput {
            concerns: vec![json!({
                "type": "Memory Leak",
                "description": "Leaked buffer on error path",
                "reasoning": "Buffer is not freed before return",
                "preexisting": false,
                "locations": [{
                    "file": "drivers/foo/bar.c",
                    "function_or_symbol": "bar_probe",
                    "line": 42,
                    "code_snippet": "return -ENOMEM;",
                    "why_this_location_matters": "Early return leaks buf"
                }]
            })],
            dismissed_concerns: vec![json!({
                "type": "Locking",
                "description": "Suspected unlocked access to state",
                "reasoning": "Caller bar_ioctl holds bar_mutex across the call",
                "locations": [{
                    "file": "drivers/foo/bar.c",
                    "function_or_symbol": "bar_ioctl",
                    "line": 108,
                    "code_snippet": "guard(mutex)(&bar->mutex);\nret = bar_update(bar);",
                    "why_this_location_matters": "Proves mutex is held by caller"
                }]
            })],
        };
        assert!(validate_concerns_output(&valid_output, &state).is_ok());

        // Dismissed concern without disproving code_snippet is rejected.
        let missing_snippet = StageConcernsOutput {
            concerns: vec![],
            dismissed_concerns: vec![json!({
                "type": "Locking",
                "description": "Suspected unlocked access",
                "reasoning": "Caller probably locks it",
                "locations": [{
                    "file": "drivers/foo/bar.c",
                    "function_or_symbol": "bar_ioctl",
                    "line": null,
                    "code_snippet": "   ",
                    "why_this_location_matters": "Unverified assumption"
                }]
            })],
        };
        let err = validate_concerns_output(&missing_snippet, &state)
            .expect_err("dismissed_concern without disproving snippet must fail");
        assert!(err.contains("dismissed_concerns[0]"));
        assert!(err.contains("move the candidate issue to 'concerns'"));
    }

    #[test]
    fn test_stage_outputs_do_not_match_inner_location_on_malformed_outer_json() {
        let malformed_with_inner_location = r#"{
  "concerns": [
    {
      "type": "Configuration Bug",
      "description": "Missing core dependency",
      "reasoning": "It omits "core" and "std" from deps.",
      "preexisting": false,
      "locations": [
        {
          "file": "scripts/generate_rust_analyzer.py",
          "function_or_symbol": "generate_crates",
          "line": 149,
          "code_snippet": "append_crate(\"quote\", ...)",
          "why_this_location_matters": "Missing core dependency"
        }
      ]
    }
  ],
  "dismissed_concerns": []
}"#;

        let err = crate::workflow::output::parse_json_from_text::<StageConcernsOutput>(
            malformed_with_inner_location,
        )
        .expect_err("malformed outer JSON must not match inner location as StageConcernsOutput");
        assert!(err.contains("line 6"));

        assert!(
            crate::workflow::output::parse_json_from_text::<ConflictResolutionOutput>(
                malformed_with_inner_location
            )
            .is_err(),
            "malformed outer JSON must not match inner location as ConflictResolutionOutput"
        );
        assert!(
            crate::workflow::output::parse_json_from_text::<VerificationOutput>(
                malformed_with_inner_location
            )
            .is_err(),
            "malformed outer JSON must not match inner location as VerificationOutput"
        );
    }

    #[test]
    fn test_build_workflow_graph_structure() {
        let workflow = build_linux_patch_review_workflow();
        assert_eq!(workflow.name, "linux_patch_review");
        assert_eq!(workflow.steps.len(), 10);
    }

    fn pull_request_description(text: &str) -> Option<CoverLetter> {
        Some(CoverLetter {
            source: crate::cover_letter::CoverLetterSource::PullRequestDescription,
            text: text.to_string(),
        })
    }

    /// Both projects' system prompts, with and without the commit message,
    /// rendered for one state.
    fn system_prompts(cover_letter: Option<CoverLetter>) -> [(&'static str, String); 4] {
        use crate::workflows::sashiko_patch_review::sashiko_system_prompt;
        let state = LinuxPatchReviewState {
            cover_letter,
            ..Default::default()
        };
        [
            (
                "linux, commit message",
                linux_system_prompt(true).render_for_log(&state),
            ),
            (
                "linux, hunks only",
                linux_system_prompt(false).render_for_log(&state),
            ),
            (
                "sashiko, commit message",
                sashiko_system_prompt(true).render_for_log(&state),
            ),
            (
                "sashiko, hunks only",
                sashiko_system_prompt(false).render_for_log(&state),
            ),
        ]
    }

    /// The system prompt variant that carries the commit message carries
    /// the cover letter, after the git metadata and before the target
    /// commit; the variant with only the hunks does not change.
    #[test]
    fn cover_letter_reaches_the_stage_context() {
        let with = system_prompts(pull_request_description("Makes frob reentrant."));
        let without = system_prompts(None);
        for ((name, prompt), (_, bare)) in with.iter().zip(&without) {
            if name.ends_with("hunks only") {
                assert_eq!(prompt, bare, "{name} gets no cover letter");
                continue;
            }
            let metadata = prompt
                .find("=== Active Git Metadata ===")
                .unwrap_or(usize::MAX);
            let block = prompt
                .find("=== Series Cover Letter ===")
                .unwrap_or(usize::MAX);
            let quoted = prompt.find("| Makes frob reentrant.").unwrap_or(usize::MAX);
            let target = prompt.find("\n\nTarget Commit:").unwrap_or(usize::MAX);
            assert!(
                metadata < block && block < quoted && quoted < target && target < usize::MAX,
                "{name}: {prompt}"
            );
        }
    }

    /// The stages whose system prompt carries the cover letter, sorted,
    /// for a review whose state selects every stage.
    fn stages_given_the_cover_letter(workflow: &Workflow<LinuxPatchReviewState>) -> Vec<&str> {
        let state = LinuxPatchReviewState {
            cover_letter: pull_request_description("Makes frob reentrant."),
            ..Default::default()
        };
        let mut given: Vec<&str> = workflow
            .stage_system_prompts(&state)
            .into_iter()
            .filter(|(_, prompt)| {
                prompt
                    .as_deref()
                    .is_some_and(|p| p.contains("=== Series Cover Letter ==="))
            })
            .map(|(name, _)| name)
            .collect();
        given.sort_unstable();
        given
    }

    /// Exactly the stages that read the commit message get the cover
    /// letter: Linux's planning stage, which is built on the commit message
    /// variant, among them, and every consolidation stage. The pre-screen,
    /// Sashiko's planning stage and the analysis stages that read only the
    /// hunks get neither.
    #[test]
    fn the_stages_that_read_the_commit_message_get_the_cover_letter() {
        use crate::workflows::sashiko_patch_review::build_sashiko_patch_review_workflow;
        assert_eq!(
            stages_given_the_cover_letter(&build_linux_patch_review_workflow()),
            [
                "conflict-resolution",
                "deduplication",
                "goal",
                "hardware",
                "implementation",
                "planning",
                "report",
                "verification",
            ]
        );
        assert_eq!(
            stages_given_the_cover_letter(&build_sashiko_patch_review_workflow()),
            [
                "conflict-resolution",
                "deduplication",
                "goal",
                "implementation",
                "interfaces-compat",
                "report",
                "summary",
                "tests",
                "verification",
            ]
        );
    }

    /// A review without a cover letter, or with one that is blank once
    /// cleaned, gets the system prompt it had before the block existed.
    #[test]
    fn no_cover_letter_renders_nothing() {
        let without = system_prompts(None);
        let blank = system_prompts(pull_request_description("<!-- template -->\r\n\u{200B}"));
        for ((name, prompt), (_, bare)) in blank.iter().zip(&without) {
            assert_eq!(prompt, bare, "{name}");
            assert!(
                bare.contains("Baseline SHA: \n===========================\n\nTarget Commit:"),
                "{name}: nothing between the metadata and the target commit: {bare}"
            );
        }
    }

    /// Substitution is single-pass, so a placeholder written in a cover
    /// letter is quoted as text, never replaced.
    #[test]
    fn a_placeholder_in_a_cover_letter_stays_literal() {
        let cover = "Fold {{patch_concerns}} into {{target_commit_sha}}.";
        let state = LinuxPatchReviewState {
            target_commit_sha: "1a2b3c4d".to_string(),
            cover_letter: pull_request_description(cover),
            ..Default::default()
        };
        let prompt = linux_system_prompt(true).render_for_log(&state);
        assert!(prompt.contains(&format!("| {cover}")), "{prompt}");
        assert_eq!(prompt.matches("1a2b3c4d").count(), 1, "{prompt}");
    }

    #[test]
    fn test_custom_prompt_renders_last_and_only_when_it_has_content() {
        // A non-empty prefetched context, so that closing the prompt is
        // distinguishable from merely rendering somewhere in it.
        let render = |custom_prompt| {
            linux_system_prompt(true).render_for_log(&LinuxPatchReviewState {
                custom_prompt,
                prefetched_context: "struct foo { int bar; };".to_string(),
                ..Default::default()
            })
        };
        let without = render(None);

        for empty in [Some(String::new()), Some("  \n\t ".to_string())] {
            assert_eq!(
                render(empty),
                without,
                "an empty custom prompt renders nothing"
            );
        }

        assert_eq!(
            render(Some("  Check the locking.  ".to_string())),
            format!(
                "{without}\n\n<custom_instructions>\nCheck the locking.\n</custom_instructions>"
            ),
            "the custom prompt closes the system prompt"
        );
    }

    fn reference(name: &str, kind: ReferenceKind, revision: &str) -> ReferenceRepo {
        ReferenceRepo {
            name: name.to_string(),
            kind,
            path: PathBuf::from(format!("/srv/{name}")),
            revision: revision.to_string(),
            commit: "1a2b3c4d5e6f7a8b9c0d1a2b3c4d5e6f7a8b9c0d".to_string(),
            description: (kind == ReferenceKind::Source)
                .then(|| "Userspace RDMA stack.\nproviders/ionic is libionic.".to_string()),
            applies_to: None,
        }
    }

    fn render_with(references: Vec<ReferenceRepo>) -> String {
        linux_system_prompt(true).render_for_log(&LinuxPatchReviewState {
            references,
            ..Default::default()
        })
    }

    /// The reference framing must reach the model when a reference is attached,
    /// and must leave an in-tree review untouched -- telling an in-tree review
    /// that Fixes: tags do not apply would suppress real findings.
    #[test]
    fn reference_blocks_render_only_when_attached() {
        let in_tree = render_with(vec![]);
        assert!(
            !in_tree.contains("Read-only Reference Repositories"),
            "a single-repository review lists no references"
        );
        assert!(
            !in_tree.contains("Out-of-Tree Module"),
            "an in-tree review must not be told to relax in-tree conventions"
        );

        let grounded = render_with(vec![reference("kernel", ReferenceKind::Kernel, "v6.12")]);
        for needle in [
            "Read-only Reference Repositories",
            "Out-of-Tree Module",
            "v6.12",
            "1a2b3c4d5e6f",
            r#"repo="kernel""#,
            "Fixes:",
            // A kernel entry without a description gets the built-in text.
            "builds against",
        ] {
            assert!(grounded.contains(needle), "missing {needle}: {grounded}");
        }
        // Nothing else about the prompt moves: the blocks are inserted after
        // the git metadata and the rest renders exactly as it did in-tree.
        let terminator = "\n===========================";
        let block_start = grounded
            .find("\n\n=== Read-only Reference Repositories ===")
            .unwrap();
        let oot_start = grounded.find("\n\n=== Out-of-Tree Module ===").unwrap();
        let block_end =
            oot_start + grounded[oot_start..].find(terminator).unwrap() + terminator.len();
        assert_eq!(
            format!("{}{}", &grounded[..block_start], &grounded[block_end..]),
            in_tree
        );
    }

    /// A userspace tree alone says nothing about the repository under review
    /// being out-of-tree, so the caveats that withdraw in-tree conventions must
    /// not appear.
    #[test]
    fn rdma_only_config_renders_no_out_of_tree_caveats() {
        let prompt = render_with(vec![reference(
            "rdma-core",
            ReferenceKind::Source,
            "ionic-rdma-v59",
        )]);
        for needle in [
            "Read-only Reference Repositories",
            r#"repo="rdma-core""#,
            "ionic-rdma-v59",
            "Userspace RDMA stack.",
            // Continuation lines of a description stay inside its list entry.
            "\n  providers/ionic is libionic.",
        ] {
            assert!(prompt.contains(needle), "missing {needle}: {prompt}");
        }
        assert!(!prompt.contains("Out-of-Tree Module"), "{prompt}");
        assert!(!prompt.contains("Fixes:"), "{prompt}");
    }

    /// References are listed in configured order, and the out-of-tree caveats
    /// name the kernel entry, not whichever reference happens to come first.
    #[test]
    fn kernel_caveats_use_the_kernel_entrys_name_and_pin() {
        let prompt = render_with(vec![
            reference("rdma-core", ReferenceKind::Source, "ionic-rdma-v59"),
            reference("kernel", ReferenceKind::Kernel, "v6.18"),
        ]);
        let rdma = prompt
            .find(r#"- repo="rdma-core""#)
            .expect("rdma-core listed");
        let kernel = prompt.find(r#"- repo="kernel""#).expect("kernel listed");
        assert!(rdma < kernel, "listed in configured order: {prompt}");

        let caveats = &prompt[prompt.find("=== Out-of-Tree Module ===").unwrap()..];
        assert!(
            caveats.contains(r#"repo="kernel", pinned at v6.18"#),
            "{caveats}"
        );
        assert!(!caveats.contains("rdma-core"), "{caveats}");
        assert!(!caveats.contains("ionic-rdma-v59"), "{caveats}");
    }

    /// A kernel entry scoped away from a patch is not attached to it, so that
    /// patch gets neither the kernel's list entry nor the out-of-tree caveats;
    /// a patch it does apply to gets both.
    #[test]
    fn a_dormant_kernel_reference_renders_no_out_of_tree_caveats() {
        let mut kernel = reference("kernel", ReferenceKind::Kernel, "v6.18");
        kernel.applies_to = Some(
            crate::references::AppliesTo::new("kernel", &["rdma/drv/**".to_string()]).unwrap(),
        );
        let refs = vec![
            kernel,
            reference("rdma-core", ReferenceKind::Source, "ionic-rdma-v59"),
        ];

        let eth =
            crate::references::attach_for_patch(&refs, &["eth/ionic/ionic_lif.c".to_string()]);
        let prompt = render_with(eth.attached);
        assert!(
            prompt.contains("Read-only Reference Repositories"),
            "{prompt}"
        );
        assert!(prompt.contains(r#"repo="rdma-core""#), "{prompt}");
        for absent in ["Out-of-Tree Module", "Fixes:", r#"repo="kernel""#] {
            assert!(!prompt.contains(absent), "unexpected {absent}: {prompt}");
        }

        let rdma =
            crate::references::attach_for_patch(&refs, &["rdma/drv/ionic/ionic_fw.h".to_string()]);
        let prompt = render_with(rdma.attached);
        let caveats = &prompt[prompt
            .find("=== Out-of-Tree Module ===")
            .expect("an attached kernel entry brings the caveats")..];
        assert!(
            caveats.contains(r#"repo="kernel", pinned at v6.18"#),
            "{caveats}"
        );
    }

    /// The RDMA guide is one the pre-screen may pick -- a plain name that no
    /// stage claims for itself -- and, once picked, it loads from the bundle
    /// into the system prompt. Its wording is conditional on a userspace
    /// reference rather than naming one.
    #[tokio::test]
    async fn the_rdma_guide_is_selectable_and_loads() {
        assert!(crate::workflows::guard::sanitize_guide_name("rdma.md"));
        assert!(!is_stage_exclusive_guide("rdma.md"));

        let prompt = linux_system_prompt(true)
            .render_for_model(
                &LinuxPatchReviewState {
                    selected_guides: vec!["rdma.md".into()],
                    ..Default::default()
                },
                &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("third_party/prompts/kernel"),
            )
            .await
            .unwrap();
        assert!(prompt.contains("# subsystem/rdma.md"), "{prompt}");
        assert!(
            prompt.contains("RDMA/InfiniBand Subsystem Details"),
            "{prompt}"
        );
        assert!(
            prompt.contains("Read-only Reference Repositories"),
            "{prompt}"
        );
        assert!(
            !prompt.contains("rdma-core"),
            "the guide must not hard-code a reference name"
        );
    }

    #[test]
    fn test_verification_stage_preserves_preexisting_concerns() {
        let stage = verification_stage(20, 0.0, PriorHistory::NotShown);
        let mut state = LinuxPatchReviewState {
            concerns: vec![json!({
                "type": "Pre-existing Race",
                "description": "Old race condition",
                "preexisting": true,
            })],
            ..Default::default()
        };

        let output = VerificationOutput {
            findings: vec![
                json!({
                    "problem": "new regression",
                    "severity": "High",
                    "preexisting": false,
                }),
                json!({
                    "problem": "another pre-existing",
                    "severity": "Medium",
                    "severity_explanation": "Old leak",
                    "preexisting": true,
                }),
            ],
        };

        (stage.reducer)(&mut state, output);

        assert_eq!(state.concerns.len(), 2);
        assert_eq!(state.concerns[0]["description"], "Old race condition");
        assert_eq!(state.concerns[1]["description"], "another pre-existing");
        assert_eq!(state.concerns[1]["reasoning"], "Old leak");
        assert_eq!(state.findings.len(), 1);
        assert_eq!(state.findings[0]["problem"], "new regression");
    }

    fn verification_prompt() -> String {
        verification_stage(10, 1.0, PriorHistory::NotShown)
            .user_prompt
            .render_for_log(&LinuxPatchReviewState::default())
    }

    /// The key list, cut at its own opening sentence. The instructions
    /// before it are left out because item 6 already says
    /// "preexisting": true, which would satisfy a check on the whole prompt
    /// whatever the key list says.
    fn verification_key_list(prompt: &str) -> &str {
        let start = prompt
            .find("Each object in the 'findings' array MUST use exactly the following keys:")
            .expect("the verification prompt has a key list");
        let end = prompt[start..]
            .find("Example Output:")
            .expect("the key list is followed by the example");
        &prompt[start..start + end]
    }

    /// The fenced example only, so neither the rubric nor the instructions
    /// can satisfy a check meant for the example.
    fn verification_example(prompt: &str) -> &str {
        let start = prompt
            .find("Example Output:\n```json")
            .expect("the verification prompt has a worked example");
        let body = &prompt[start + "Example Output:\n```json".len()..];
        let end = body.find("```").expect("the example is fenced");
        &body[..end]
    }

    /// The stage's validator checks only `prior` links, so a key that drifts
    /// out of either half is silently no longer emitted, or emitted without
    /// a definition.
    #[test]
    fn verification_key_list_and_example_name_the_same_keys() {
        let prompt = verification_prompt();
        let keys = verification_key_list(&prompt);
        let example = verification_example(&prompt);
        for key in [
            "problem",
            "speculative",
            "severity",
            "severity_explanation",
            "preexisting",
            "locations",
        ] {
            assert!(
                keys.contains(&format!("\"{key}\" (")),
                "key list lacks {key}: {keys}"
            );
            assert!(
                example.contains(&format!("\"{key}\":")),
                "example lacks {key}: {example}"
            );
        }
    }

    /// The speculative cap is a condition on the level, so the model has to
    /// commit to it before it picks one.
    #[test]
    fn verification_asks_for_speculative_before_severity() {
        let prompt = verification_prompt();
        let keys = verification_key_list(&prompt);
        let example = verification_example(&prompt);
        for (span, speculative, severity) in [
            (keys, "\"speculative\" (", "\"severity\" ("),
            (example, "\"speculative\":", "\"severity\":"),
        ] {
            let speculative = span.find(speculative).expect("speculative is asked for");
            let severity = span.find(severity).expect("severity is asked for");
            assert!(speculative < severity, "speculative after severity: {span}");
        }
    }

    #[test]
    fn test_verification_stage_keeps_the_speculative_mark() {
        let stage = verification_stage(20, 0.0, PriorHistory::NotShown);
        let mut state = LinuxPatchReviewState::default();
        let output = VerificationOutput {
            findings: vec![
                json!({
                    "problem": "mm: unproven leak",
                    "speculative": true,
                    "severity": "Medium",
                    "preexisting": false,
                }),
                json!({
                    "problem": "mm: old unproven leak",
                    "speculative": true,
                    "severity": "Medium",
                    "preexisting": true,
                }),
            ],
        };

        (stage.reducer)(&mut state, output);

        assert_eq!(state.findings.len(), 1);
        assert_eq!(state.findings[0]["speculative"], true);
        assert_eq!(state.concerns.len(), 1);
        assert_eq!(state.concerns[0]["description"], "mm: old unproven leak");
    }

    /// Instruction item 5 and the kernel rubric ask for consequence,
    /// triggering path and reachability at the start of the explanation;
    /// the example has to show the same shape it asks for.
    #[test]
    fn verification_example_labels_its_severity_reasoning() {
        let prompt = verification_prompt();
        let example = verification_example(&prompt);
        let positions: Vec<usize> = ["Consequence:", "Triggering path:", "Reachability:"]
            .iter()
            .map(|label| {
                example
                    .find(label)
                    .unwrap_or_else(|| panic!("example lacks {label}: {example}"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "labels out of rubric order: {example}"
        );
    }

    /// A block of earlier findings as the worker builds it: one finding on
    /// the earlier version of the patch under review, whose title holds a
    /// placeholder of the verification template.
    fn prior_reviews() -> PriorReviewContext {
        use crate::prior_revisions::{
            PriorFinding, PriorPatch, PriorReviewTarget, PriorRevision, build_prior_review_context,
        };
        let revision = PriorRevision {
            label: "v1".to_string(),
            head: None,
            same_base: None,
            patches: vec![PriorPatch {
                index: 1,
                subject: "frob: refill the ring".to_string(),
                commit: None,
                same_change_as: None,
                diff: None,
                findings: vec![PriorFinding {
                    severity: crate::db::Severity::High,
                    problem: "frob: refill leaks {{patch_concerns}}".to_string(),
                    consequence: Some("a page leaks per failed refill.".to_string()),
                    locations: Vec::new(),
                    prior: None,
                }],
            }],
        };
        let target = PriorReviewTarget {
            index: 1,
            subject: "frob: refill the ring",
            diff: "diff --git a/ring.c b/ring.c\n+refill();\n",
        };
        build_prior_review_context(&[revision], target).expect("a finding to show")
    }

    /// Each stage's user prompt for a review whose state selects every
    /// stage, in both projects, with and without earlier findings.
    fn user_prompts(prior_reviews: Option<PriorReviewContext>) -> Vec<(String, String)> {
        user_prompts_for(&LinuxPatchReviewState {
            prior_reviews,
            patch_concerns: vec![json!({"description": "a concern"})],
            ..Default::default()
        })
    }

    /// Each stage's user prompt for `state`, in both projects, named
    /// "<project> <stage>".
    fn user_prompts_for(state: &LinuxPatchReviewState) -> Vec<(String, String)> {
        use crate::workflows::sashiko_patch_review::build_sashiko_patch_review_workflow;
        let linux = build_linux_patch_review_workflow().stage_user_prompts(state);
        let sashiko = build_sashiko_patch_review_workflow().stage_user_prompts(state);
        let tagged = |project: &str, prompts: Vec<(&str, String)>| {
            prompts
                .into_iter()
                .map(|(name, prompt)| (format!("{project} {name}"), prompt))
                .collect::<Vec<_>>()
        };
        let mut prompts = tagged("linux", linux);
        prompts.extend(tagged("sashiko", sashiko));
        prompts
    }

    /// The stages of one project that the tables say are shown what earlier
    /// revisions were told, each tagged with its project, and every stage
    /// the tables name.
    fn tabled_stages(
        project: &str,
        analysis: &[AnalysisStage],
        consolidation: &[&ConsolidationStage],
    ) -> (Vec<String>, Vec<String>) {
        let flags = analysis
            .iter()
            .map(|def| (def.name, def.wants_prior_reviews))
            .chain(
                consolidation
                    .iter()
                    .map(|def| (def.name, def.wants_prior_reviews)),
            );
        let (mut wanting, mut all) = (Vec::new(), Vec::new());
        for (name, wants) in flags {
            let tagged = format!("{project} {name}");
            if wants {
                wanting.push(tagged.clone());
            }
            all.push(tagged);
        }
        (wanting, all)
    }

    /// The rendered user prompts agree with the stage tables: exactly the
    /// stages that declare wants_prior_reviews are shown what earlier
    /// revisions were told, in both projects, so a flag set without the
    /// placeholder, or a placeholder added without the flag, fails here.
    /// Only verification declares it, so the analysts are not anchored on
    /// what was raised before, and no system prompt carries it.
    #[test]
    fn prior_reviews_reach_verification_only() {
        use crate::workflows::sashiko_patch_review as sashiko;
        let (mut wanting, mut tabled) =
            tabled_stages("linux", ANALYSIS_STAGES, CONSOLIDATION_STAGES);
        let (sashiko_wanting, sashiko_tabled) = tabled_stages(
            "sashiko",
            sashiko::ANALYSIS_STAGES,
            sashiko::CONSOLIDATION_STAGES,
        );
        wanting.extend(sashiko_wanting);
        tabled.extend(sashiko_tabled);
        assert_eq!(wanting, ["linux verification", "sashiko verification"]);
        assert_eq!(prior_reviews_placeholder(true), PRIOR_REVIEWS_PLACEHOLDER);
        assert_eq!(prior_reviews_placeholder(false), "");

        let context = prior_reviews();
        let opening = context
            .rendered
            .lines()
            .next()
            .expect("an opening line")
            .to_string();
        let prompts = user_prompts(Some(context.clone()));
        for name in &tabled {
            assert!(
                prompts.iter().any(|(stage, _)| stage == name),
                "{name} has a rendered prompt to check"
            );
        }
        let given: Vec<String> = prompts
            .into_iter()
            .filter(|(_, prompt)| prompt.contains(&opening))
            .map(|(name, _)| name)
            .collect();
        assert_eq!(given, wanting);

        use crate::workflows::sashiko_patch_review::build_sashiko_patch_review_workflow;
        let state = LinuxPatchReviewState {
            prior_reviews: Some(context),
            ..Default::default()
        };
        let system_prompts = build_linux_patch_review_workflow()
            .stage_system_prompts(&state)
            .into_iter()
            .chain(build_sashiko_patch_review_workflow().stage_system_prompts(&state));
        for (name, prompt) in system_prompts {
            assert!(
                !prompt.unwrap_or_default().contains(&opening),
                "{name}'s system prompt"
            );
        }
    }

    /// The verification rules for the earlier findings that the project a
    /// prompt is tagged with ("<project> <stage>") gives its verification.
    fn verification_rules_of(tagged: &str) -> &'static str {
        if tagged.starts_with("sashiko ") {
            crate::workflows::sashiko_patch_review::PRIOR_REVIEWS_VERIFICATION_RULES
        } else {
            PRIOR_REVIEWS_VERIFICATION_RULES
        }
    }

    /// A review with no earlier findings gets the user prompts it had
    /// before the block existed: the block and its rules are the only
    /// difference, and without history they are empty, with no placeholder
    /// left in the text.
    #[test]
    fn prior_reviews_render_nothing_without_history() {
        let context = prior_reviews();
        let with = user_prompts(Some(context.clone()));
        let without = user_prompts(None);
        for ((name, prompt), (_, bare)) in with.iter().zip(&without) {
            let inserted = format!(
                "\n\n{}\n\n{}",
                context.rendered,
                verification_rules_of(name)
            );
            assert_eq!(&prompt.replacen(&inserted, "", 1), bare, "{name}");
            assert!(!bare.contains(PRIOR_REVIEWS_PLACEHOLDER), "{name}");
            assert!(!bare.contains("PRIOR REVISION RULES"), "{name}");
        }
        for (name, bare) in without.iter().filter(|(n, _)| n.ends_with("verification")) {
            assert!(
                bare.ends_with("\n\n@false-positive-guide.md\n\n\n@severity.md\n"),
                "{name}: the guides still end the prompt: {bare}"
            );
        }
    }

    /// Verification reads the block and its rules after the findings
    /// format it asks for: after the Linux example output and after the
    /// Sashiko key list. The rules judge every concern on the current code,
    /// add "prior" as the one new key, for a repeat or a fix regression,
    /// keep the title free of the history and put it at the end of the
    /// rubric section that names the path (the Triggering path in Linux,
    /// the Likelihood and preconditions reasoning in Sashiko) with the
    /// rubric's first section kept first, and forbid reviving an earlier
    /// finding.
    #[test]
    fn verification_reads_the_prior_rules_after_its_findings_format() {
        let context = prior_reviews();
        let opening = context
            .rendered
            .lines()
            .next()
            .expect("an opening line")
            .to_string();
        let prompts = user_prompts(Some(context.clone()));
        let prompt_of = |name: &str| {
            prompts
                .iter()
                .find(|(stage, _)| stage == name)
                .map(|(_, prompt)| prompt.as_str())
                .unwrap_or_else(|| panic!("{name} is listed"))
        };
        for (name, format_end, history_rule) in [
            (
                "linux verification",
                "    }}\n  ]\n}}\n```",
                [
                    "at the end of its Triggering path, in one sentence",
                    "Keep the Consequence first, as the rules above ask.",
                ],
            ),
            (
                "sashiko verification",
                "code_snippet, and why_this_location_matters).",
                [
                    "at the end of its Likelihood and preconditions reasoning, in one sentence",
                    "Keep the Blast radius and irreversibility reasoning first, as severity.md \
                     asks.",
                ],
            ),
        ] {
            let prompt = prompt_of(name);
            let rules = verification_rules_of(name);
            let format_end = format_end.replace("}}", "}");
            let block = prompt.find(&opening).expect("the block");
            let rules_at = prompt.find(rules).expect("the rules");
            let format = prompt.find(&format_end).expect("the findings format");
            let guides = prompt
                .find("\n\n@false-positive-guide.md\n\n\n@severity.md\n")
                .expect("the guides");
            assert!(
                format < guides && guides < block && block < rules_at,
                "{name}: the rules call the guides the rules above: {prompt}"
            );
            assert!(
                prompt[block..].contains(&format!("{}\n\n{rules}", context.rendered)),
                "{name}: the rules follow the block"
            );
            let mut opened = Vec::new();
            for rule in [
                "1. Judge every concern on the current code",
                "2. REPEAT: if a concern is the same defect as an earlier finding",
                "3. FIX REGRESSION: if the code a concern is about was added or changed",
                "4. Keep \"problem\" the short title",
                "5. Omit \"prior\" when no finding above is the counterpart.",
                "\"prior\" is the only addition to the findings format above",
            ] {
                let at = rules
                    .find(&format!("\n{rule}"))
                    .unwrap_or_else(|| panic!("{name}: {rule} opens a line: {rules}"));
                opened.push(at);
            }
            assert!(opened.windows(2).all(|w| w[0] < w[1]), "{name}: {rules}");
            for asked in [
                r#""prior": {"relation": "repeat", "revision": "<the label it was listed under, e.g. v3>", "earlier_problem": "<the earlier finding's title, copied exactly as listed>"}"#,
                r#""relation": "fix_regression""#,
                "Do not report an earlier finding that no concern in this review raises",
            ]
            .into_iter()
            .chain(history_rule)
            {
                assert!(rules.contains(asked), "{name}: {asked}: {rules}");
            }
            assert!(!rules.contains("revision_head"), "the daemon adds the head");
        }
    }

    /// A commit the block names may also be in the series under review, when
    /// it was carried over unchanged, so the rules take the series and its
    /// final state only from the follow-up patches and series end commit the
    /// context provides, never from the block, and read the block's commits
    /// only to compare the earlier code with the current code. The block, the
    /// series rule, the repeat and fix regression rules, and the two headings
    /// the worker writes for the series are all named as they appear.
    #[test]
    fn verification_keeps_earlier_commits_out_of_the_series() {
        let prompts = user_prompts(Some(prior_reviews()));
        let sentence = "The commits named in \"Findings From Earlier Revisions of This Pull \
             Request\" above show what earlier revisions contained. A commit carried over \
             unchanged can also be part of the series under review, so that the block names a \
             commit says nothing either way: when you apply the SERIES VALIDATION RULE, or \
             otherwise decide whether this series fixes or changes a concern, take the series \
             under review and its final state only from \"Follow-Up Patches in Series\" and \
             \"Series End Commit (Final State)\" when they are provided in the context, never \
             from the block. Code at a commit the block names, including another patch of that \
             revision, does not show how the series under review handles a concern, even when \
             it uses what the patch under review defines; without those two sections no later \
             patch of the series under review is available, so dismiss no concern as handled \
             by one. Beyond that, use a commit the block names only to compare the earlier \
             revision's code with the current code, under REPEAT or FIX REGRESSION below.";
        let series = crate::worker::prompts::build_follow_up_series_context(
            Some("base..sha2"),
            &serde_json::json!({
                "patch_index": 1,
                "patches": [
                    { "index": 1, "subject": "net: add foo", "commit_id": "sha1" },
                    { "index": 2, "subject": "net: use foo", "commit_id": "sha2" }
                ]
            }),
            "sha1",
        )
        .expect("a follow-up block");
        assert!(
            series.contains("=== Follow-Up Patches in Series ==="),
            "{series}"
        );
        assert!(
            series.contains("Series End Commit (Final State): sha2"),
            "{series}"
        );
        for name in ["linux verification", "sashiko verification"] {
            let prompt = prompts
                .iter()
                .find(|(stage, _)| stage == name)
                .map(|(_, prompt)| prompt.as_str())
                .unwrap_or_else(|| panic!("{name} is listed"));
            let rules = verification_rules_of(name);
            let first_rule = rules
                .find("\n1. Judge every concern on the current code")
                .expect("rule 1");
            let second_rule = rules.find("\n2. REPEAT:").expect("rule 2");
            let at = rules
                .find(sentence)
                .unwrap_or_else(|| panic!("{name}: {rules}"));
            assert!(first_rule < at && at < second_rule, "{name}: {rules}");
            let block = prompt
                .find("=== Findings From Earlier Revisions of This Pull Request ===")
                .expect("the block");
            let series_rule = prompt
                .find("3. SERIES VALIDATION RULE:")
                .unwrap_or_else(|| panic!("{name}: {prompt}"));
            assert!(series_rule < block, "{name}: the series rule is above");
            assert!(rules.contains("3. FIX REGRESSION:"), "{name}: {rules}");
        }
    }

    /// The rubric labels (see `CalibrationLabel::SPELLINGS`) that `rules`
    /// names, each as a whole word, as the case-sensitive page splitter
    /// reads them.
    fn rubric_labels_named(rules: &str) -> Vec<&'static str> {
        CalibrationLabel::SPELLINGS
            .iter()
            .map(|&(spelling, _)| spelling)
            .filter(|spelling| names_word(rules, spelling))
            .collect()
    }

    /// Whether `text` holds `word` with no letter, digit or '_' on either
    /// side, so "Consequences" does not name the Consequence.
    fn names_word(text: &str, word: &str) -> bool {
        Regex::new(&format!(r"\b{}\b", regex::escape(word)))
            .expect("an escaped word is a valid pattern")
            .is_match(text)
    }

    /// Each stage's system prompt and its user prompt without earlier
    /// findings, rendered for the model with every included file expanded
    /// from `dir`: what the model reads before the prior revision rules.
    async fn prompt_before_the_rules<T>(
        stage: &Stage<LinuxPatchReviewState, T>,
        dir: &std::path::Path,
    ) -> String {
        let state = LinuxPatchReviewState {
            patch_concerns: vec![json!({"description": "a concern"})],
            findings: vec![json!({"problem": "frob: refill leaks"})],
            ..Default::default()
        };
        let system = match &stage.system_prompt {
            Some(template) => template
                .render_for_model(&state, dir)
                .await
                .expect("the system prompt renders"),
            None => String::new(),
        };
        let user = stage
            .user_prompt
            .render_for_model(&state, dir)
            .await
            .expect("the user prompt renders");
        assert!(!user.contains("PRIOR REVISION RULES"), "{user}");
        format!("{system}\n{user}")
    }

    /// The prior revision rules tell the model where in its severity
    /// explanation the history goes, by the sections of the severity
    /// rubric. Each workflow has its own rubric, so every rubric label a
    /// workflow's rules name must be one its own prompt, rubric included,
    /// already uses; a label from the other workflow's rubric points the
    /// model at a section it was never asked to write.
    #[tokio::test]
    async fn prior_rules_name_only_their_own_rubric_sections() {
        use crate::workflows::sashiko_patch_review as sashiko;
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let linux_dir = root.join("third_party/prompts/kernel");
        let sashiko_dir = root.join("prompts/sashiko");
        let linux_verification = verification_stage(10, 1.0, PriorHistory::Shown);
        let sashiko_verification = sashiko::verification_stage(10, 1.0, PriorHistory::Shown);
        let checked = [
            (
                "linux verification",
                PRIOR_REVIEWS_VERIFICATION_RULES,
                prompt_before_the_rules(&linux_verification, &linux_dir).await,
            ),
            (
                "linux report",
                PRIOR_LINKS_REPORT_RULES,
                prompt_before_the_rules(&report_stage(10, 1.0), &linux_dir).await,
            ),
            (
                "sashiko verification",
                sashiko::PRIOR_REVIEWS_VERIFICATION_RULES,
                prompt_before_the_rules(&sashiko_verification, &sashiko_dir).await,
            ),
            (
                "sashiko report",
                sashiko::PRIOR_LINKS_REPORT_RULES,
                prompt_before_the_rules(&sashiko::report_stage(10, 1.0), &sashiko_dir).await,
            ),
        ];
        for (name, rules, prompt) in &checked {
            for label in rubric_labels_named(rules) {
                assert!(
                    names_word(prompt, label),
                    "{name}: the rules name the {label} section, which this workflow's \
                     prompt never asks for: {prompt}"
                );
            }
        }
        // The rubric files were found, so the check above compared the rules
        // with the rubric rather than with a prompt missing it.
        for (name, rubric_heading) in [
            ("linux verification", "Triggering path:"),
            (
                "sashiko verification",
                "**Blast radius and irreversibility**",
            ),
        ] {
            let (_, _, prompt) = checked
                .iter()
                .find(|(stage, _, _)| *stage == name)
                .expect("the stage is checked");
            assert!(prompt.contains(rubric_heading), "{name}: {prompt}");
        }
        assert_eq!(
            rubric_labels_named(PRIOR_REVIEWS_VERIFICATION_RULES),
            ["Consequence", "Triggering path"]
        );
        assert_eq!(
            rubric_labels_named(sashiko::PRIOR_REVIEWS_VERIFICATION_RULES),
            [
                "Blast radius",
                "Blast radius and irreversibility",
                "Likelihood",
                "Likelihood and preconditions"
            ]
        );
    }

    /// Substitution is single-pass, so an earlier finding whose title holds
    /// another variable's placeholder keeps it as text, and the variable is
    /// still filled where the template puts it.
    #[test]
    fn a_placeholder_in_an_earlier_finding_stays_literal() {
        let prompts = user_prompts(Some(prior_reviews()));
        for (name, prompt) in prompts.iter().filter(|(n, _)| n.ends_with("verification")) {
            assert!(
                prompt.contains("frob: refill leaks {{patch_concerns}}"),
                "{name}: {prompt}"
            );
            assert!(prompt.contains("\"description\": \"a concern\""), "{name}");
        }
    }

    /// The verification stages of both projects, by project name, for a
    /// review whose earlier findings `history` says whether it is shown.
    fn verification_stages(
        history: PriorHistory,
    ) -> [(
        &'static str,
        Stage<LinuxPatchReviewState, VerificationOutput>,
    ); 2] {
        [
            ("linux", verification_stage(20, 0.0, history)),
            (
                "sashiko",
                crate::workflows::sashiko_patch_review::verification_stage(20, 0.0, history),
            ),
        ]
    }

    /// A finding kept by verification, linked to the finding `prior_reviews`
    /// shows unless `earlier_problem` names another.
    fn linked_finding(problem: &str, earlier_problem: &str) -> Value {
        json!({
            "problem": problem,
            "severity": "High",
            "preexisting": false,
            "prior": {"relation": "repeat", "revision": "v1", "earlier_problem": earlier_problem},
        })
    }

    /// The validator rejects an output with a link to an unlisted finding
    /// once per review, naming the finding, the fault and the titles listed
    /// under the revision it named; the next faulty output passes and the
    /// reduce removes the link. A preexisting finding's link and links that
    /// ground are never rejected. A review with no earlier findings attaches
    /// no validator and keeps the default validation attempts; one with
    /// them has one attempt more, for the one rejection.
    #[test]
    fn the_prior_validator_names_the_bad_link_once() {
        let listed = "frob: refill leaks {{patch_concerns}}";
        let state = LinuxPatchReviewState {
            prior_reviews: Some(prior_reviews()),
            ..Default::default()
        };
        let output = |findings: Vec<Value>| json!({ "findings": findings }).to_string();
        let bad = output(vec![
            linked_finding("frob: ring leaks", listed),
            linked_finding("frob: refill races", "frob: refill races"),
        ]);
        let preexisting_bad = output(vec![json!({
            "problem": "frob: old race",
            "preexisting": true,
            "prior": {"relation": "repeat", "revision": "v9", "earlier_problem": "x"},
        })]);
        let default_attempts = StagePolicy::consolidation().max_validation_attempts;
        for (project, stage) in verification_stages(PriorHistory::NotShown) {
            assert!(
                matches!(
                    stage.output_format,
                    OutputFormat::Json {
                        validator: None,
                        ..
                    }
                ),
                "{project}: no history attaches no validator"
            );
            assert_eq!(
                stage.policy.max_validation_attempts, default_attempts,
                "{project}"
            );
        }
        for (project, stage) in verification_stages(PriorHistory::Shown) {
            assert_eq!(
                stage.policy.max_validation_attempts,
                default_attempts + 1,
                "{project}"
            );
            let format = &stage.output_format;
            assert!(
                format.validate(&preexisting_bad, &state).is_ok(),
                "{project}"
            );
            let good = output(vec![linked_finding("frob: ring leaks", listed)]);
            assert!(format.validate(&good, &state).is_ok(), "{project}");

            let feedback = format
                .validate(&bad, &state)
                .expect_err("an unlisted title is rejected");
            for expected in [
                "- the finding \"frob: refill races\": earlier_problem \"frob: refill races\" \
                 is not listed under \"v1\"\n",
                "The titles listed under \"v1\" are:\n- \"frob: refill leaks {{patch_concerns}}\"\n",
            ] {
                assert!(feedback.contains(expected), "{project}: {feedback}");
            }
            assert!(
                !feedback.contains("frob: ring leaks"),
                "{project}: {feedback}"
            );
            let feedback = format.format_feedback(&feedback);
            assert!(
                feedback.ends_with("change nothing else. Please correct your output format."),
                "{project}: {feedback}"
            );

            let again = format
                .validate(&bad, &state)
                .unwrap_or_else(|e| panic!("{project}: a second rejection: {e}"));
            let mut reduced = state.clone();
            (stage.reducer)(&mut reduced, again);
            let links: Vec<Option<&Value>> =
                reduced.findings.iter().map(|f| f.get("prior")).collect();
            assert_eq!(
                links,
                [
                    Some(&json!({"relation": "repeat", "revision": "v1",
                                 "earlier_problem": listed})),
                    None,
                ],
                "{project}"
            );
        }
    }

    /// Answers each model request with the next queued text, and nothing
    /// once the queue is empty.
    struct QueuedAnswers(std::sync::Mutex<std::collections::VecDeque<String>>);

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for QueuedAnswers {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            Ok(crate::ai::AiResponse {
                content: self.0.lock().unwrap().pop_front(),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "queued".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    /// The session counts the validator's one rejection and a JSON parse
    /// error alike, so a review shown earlier findings gets one attempt more:
    /// a parse error, a rejected link and a second parse error still leave
    /// the attempt the correct answer needs, in both projects.
    #[tokio::test]
    async fn the_prior_rejection_leaves_the_attempts_a_parse_error_needs() {
        let listed = "frob: refill leaks {{patch_concerns}}";
        let answer = |earlier_problem: &str| {
            json!({ "findings": [linked_finding("frob: ring leaks", earlier_problem)] }).to_string()
        };
        let state = LinuxPatchReviewState {
            prior_reviews: Some(prior_reviews()),
            patch_concerns: vec![json!({"description": "a concern"})],
            ..Default::default()
        };
        for (project, stage) in verification_stages(PriorHistory::Shown) {
            let answers = [
                "not JSON".to_string(),
                answer("frob: refill races"),
                "still not JSON".to_string(),
                answer(listed),
            ];
            let tmp = tempfile::tempdir().unwrap();
            let env = crate::workflow::WorkflowEnv {
                provider: std::sync::Arc::new(QueuedAnswers(std::sync::Mutex::new(
                    answers.into_iter().collect(),
                ))),
                tools: std::sync::Arc::new(crate::toolbox::ToolBox::new(
                    tmp.path().to_path_buf(),
                    None,
                )),
                base_dir: tmp.path(),
                context_tag: None,
                wind_down: None,
            };
            let mut reduced = state.clone();
            stage
                .execute(&env, &mut reduced, None)
                .await
                .unwrap_or_else(|e| panic!("{project}: {e:#}"));
            assert_eq!(
                reduced.findings[0]["prior"]["earlier_problem"], listed,
                "{project}"
            );
        }
    }

    /// Both reduces ground each kept finding's link: a link to a listed
    /// finding is kept in canonical form, one to anything else, or any link
    /// in a review shown no earlier findings, is removed, and a preexisting
    /// finding becomes a concern without one.
    #[test]
    fn verification_keeps_only_links_to_listed_findings() {
        let listed = "frob: refill leaks {{patch_concerns}}";
        let findings = vec![
            linked_finding("frob: ring leaks", &listed.to_uppercase()),
            linked_finding("frob: refill races", "frob: refill races"),
            json!({
                "problem": "frob: old race",
                "severity_explanation": "Consequence: a race.",
                "preexisting": true,
                "prior": {"relation": "repeat", "revision": "v1", "earlier_problem": listed},
            }),
        ];
        for (project, stage) in verification_stages(PriorHistory::Shown) {
            for (prior_reviews, kept) in [
                (
                    Some(prior_reviews()),
                    Some(json!({"relation": "repeat", "revision": "v1",
                                "earlier_problem": listed})),
                ),
                (None, None),
            ] {
                let mut state = LinuxPatchReviewState {
                    prior_reviews,
                    ..Default::default()
                };
                let output = VerificationOutput {
                    findings: findings.clone(),
                };
                (stage.reducer)(&mut state, output);
                let links: Vec<Option<Value>> = state
                    .findings
                    .iter()
                    .map(|f| f.get("prior").cloned())
                    .collect();
                assert_eq!(links, [kept, None], "{project}");
                assert_eq!(state.concerns.len(), 1, "{project}");
                assert_eq!(state.concerns[0]["description"], "frob: old race");
                assert!(state.concerns[0].get("prior").is_none(), "{project}");
            }
        }
    }

    /// A block of earlier findings that shows three findings on the earlier
    /// version of the patch under review, each with a consequence and a
    /// location, under v2, whose patch has a recorded commit.
    fn three_earlier_findings() -> PriorReviewContext {
        use crate::finding_view::FindingLocation;
        use crate::prior_revisions::{
            PriorFinding, PriorPatch, PriorReviewTarget, PriorRevision, build_prior_review_context,
        };
        let finding = |problem: &str, function: &str, consequence: &str| PriorFinding {
            severity: crate::db::Severity::High,
            problem: problem.to_string(),
            consequence: Some(consequence.to_string()),
            locations: vec![FindingLocation {
                file: Some("drivers/frob/ring.c".to_string()),
                function_or_symbol: Some(function.to_string()),
                line: Some(812),
            }],
            prior: None,
        };
        let revision = PriorRevision {
            label: "v2".to_string(),
            head: None,
            same_base: None,
            patches: vec![PriorPatch {
                index: 1,
                subject: "frob: refill the ring".to_string(),
                commit: crate::baseline::CommitId::parse(&"ab".repeat(20)),
                same_change_as: None,
                diff: None,
                findings: vec![
                    finding("frob: ring stall", "frob_stall", "the ring stops."),
                    finding("frob: refill leaks pages", "frob_refill", "a page leaks."),
                    finding("frob: doorbell race", "frob_ring", "the doorbell is lost."),
                ],
            }],
        };
        let target = PriorReviewTarget {
            index: 1,
            subject: "frob: refill the ring",
            diff: "diff --git a/drivers/frob/ring.c b/drivers/frob/ring.c\n+frob_refill();\n",
        };
        build_prior_review_context(&[revision], target).expect("findings to show")
    }

    /// The report prompts of both projects for `state`, by project name.
    fn report_prompts(state: &LinuxPatchReviewState) -> Vec<(String, String)> {
        let prompts: Vec<(String, String)> = user_prompts_for(state)
            .into_iter()
            .filter(|(name, _)| name.ends_with(" report"))
            .collect();
        assert_eq!(prompts.len(), 2, "one report per project");
        prompts
    }

    /// Each project's rules for stating a link in its report.
    fn report_rules(name: &str) -> &'static str {
        if name.starts_with("linux") {
            PRIOR_LINKS_REPORT_RULES
        } else {
            crate::workflows::sashiko_patch_review::PRIOR_LINKS_REPORT_RULES
        }
    }

    /// The report is given only the earlier findings that verification's
    /// grounded links name, with the commit, location and consequence a
    /// comment needs, and its rules right after them; the other earlier
    /// findings and the block verification was shown stay out of it. A
    /// report whose findings link to nothing gets the prompt it had before
    /// the links existed, history or not.
    #[test]
    fn the_report_sees_only_linked_earlier_findings() {
        let context = three_earlier_findings();
        let mut state = LinuxPatchReviewState {
            prior_reviews: Some(context.clone()),
            ..Default::default()
        };
        let linked = json!({
            "problem": "frob: refill still leaks",
            "severity": "High",
            "preexisting": false,
            "prior": {"relation": "fix_regression", "revision": "V2",
                      "earlier_problem": "frob: refill leaks pages"},
        });
        let unlinked =
            json!({"problem": "frob: new race", "severity": "Low", "preexisting": false});
        reduce_verification(
            &mut state,
            VerificationOutput {
                findings: vec![linked, unlinked.clone()],
            },
        );
        let block = context
            .render_linked(&state.findings)
            .expect("one finding links to an earlier one");
        let verification_block = context.rendered.lines().next().expect("an opening line");
        let bare_state = LinuxPatchReviewState {
            prior_reviews: None,
            ..state.clone()
        };
        let bare = report_prompts(&bare_state);
        for ((name, prompt), (_, bare)) in report_prompts(&state).iter().zip(&bare) {
            for shown in [
                "  - v2: frob: refill leaks pages\n",
                &format!("    Commit: {}\n", "ab".repeat(20)),
                "    At: drivers/frob/ring.c:812 (frob_refill)\n",
                "    Consequence: a page leaks.\n",
            ] {
                assert!(prompt.contains(shown), "{name} lacks {shown:?}: {prompt}");
            }
            for unshown in [
                "frob: ring stall",
                "frob: doorbell race",
                verification_block,
            ] {
                assert!(!prompt.contains(unshown), "{name} shows {unshown:?}");
            }
            let inserted = format!("\n\n{block}\n\n{}", report_rules(name));
            assert_eq!(&prompt.replacen(&inserted, "", 1), bare, "{name}");
        }

        let unlinked_prompts = |history: Option<PriorReviewContext>| {
            report_prompts(&LinuxPatchReviewState {
                prior_reviews: history,
                findings: vec![unlinked.clone()],
                ..Default::default()
            })
        };
        let without_history = unlinked_prompts(None);
        for (name, prompt) in &without_history {
            assert!(!prompt.contains(PRIOR_LINKS_PLACEHOLDER), "{name}");
            assert!(!prompt.contains("PRIOR REVISION RULES"), "{name}");
            assert!(!prompt.contains("Earlier Findings Named"), "{name}");
        }
        assert_eq!(unlinked_prompts(Some(context)), without_history);
    }

    /// The Linux report reads the linked findings and their rules right
    /// after its instruction and before the findings they explain, and a
    /// prompt without them is the instruction followed by the findings, as
    /// before.
    #[test]
    fn the_linux_report_reads_the_linked_findings_after_its_instruction() {
        let context = three_earlier_findings();
        let findings = vec![json!({
            "problem": "frob: ring stall again",
            "prior": {"relation": "repeat", "revision": "v2",
                      "earlier_problem": "frob: ring stall"},
        })];
        let block = context.render_linked(&findings).expect("a linked finding");
        let state = LinuxPatchReviewState {
            prior_reviews: Some(context),
            findings,
            ..Default::default()
        };
        let stage = report_stage(1, 1.0);
        assert!(stage.user_prompt_for_log(&state).starts_with(&format!(
            "{STAGE_REPORT_INSTRUCTION}\n\n{block}\n\n{PRIOR_LINKS_REPORT_RULES}\n\nFindings:\n"
        )));
        let without = LinuxPatchReviewState {
            prior_reviews: None,
            ..state
        };
        assert!(
            stage
                .user_prompt_for_log(&without)
                .starts_with(&format!("{STAGE_REPORT_INSTRUCTION}\n\nFindings:\n"))
        );
    }

    /// Both reports state a repeat and a fix regression by the link's own
    /// label and title, tell a fix regression's fix not to undo the earlier
    /// fix, and say nothing about earlier revisions for an unlinked finding;
    /// the Sashiko report puts its note right after the severity marker.
    /// Neither asks for the revision head, which only the page shows.
    #[test]
    fn the_reports_state_each_link_as_their_rules_ask() {
        for rules in [report_rules("linux"), report_rules("sashiko")] {
            for asked in [
                "PRIOR REVISION RULES: A finding may carry \"prior\", linking it to a finding \
                 reported on an earlier revision of this pull request, listed above.",
                "\n- \"relation\": \"repeat\": ",
                "\n- \"relation\": \"fix_regression\": ",
                "the issue was raised on <revision> (\"<earlier_problem>\") and is still present.",
                "the issue appears to come from the change made for <revision>'s \
                 \"<earlier_problem>\". Any fix you suggest must resolve both this issue and the \
                 earlier one; do not suggest going back to the code the earlier finding was about.",
                "\nFindings without \"prior\" are new; do not mention earlier revisions for them.",
            ] {
                assert!(rules.contains(asked), "{asked}: {rules}");
            }
            assert!(!rules.contains("revision_head"), "{rules}");
            assert!(
                !rules.contains('`'),
                "the reports use no backticks: {rules}"
            );
        }
        assert!(
            PRIOR_LINKS_REPORT_RULES.contains("begin that comment by saying the issue was raised")
        );
        let sashiko = report_rules("sashiko");
        for note in [
            "write \"(repeat of <revision>)\" right after the severity marker of that finding's \
             bullet, as in \"- [HIGH] (repeat of v3) ...\"",
            "write \"(caused by the fix for <revision>)\" right after the severity marker of that \
             finding's bullet, as in \"- [MEDIUM] (caused by the fix for v3) ...\"",
        ] {
            assert!(sashiko.contains(note), "{note}: {sashiko}");
        }
    }
}
