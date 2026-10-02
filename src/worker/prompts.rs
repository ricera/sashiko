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

use crate::ai::{AiErrorClass, AiMessage, AiProvider, ClassifyAiError};
use crate::api::ReviewLogEntry;
use crate::toolbox::ToolBox;
use anyhow::Result;

/// Typed errors that must not be silently retried.
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    /// The AI exceeded its per-review turn limit.  Retrying with the same
    /// limit will just hit the cap again — fail fast.
    #[error("Max interactions exceeded")]
    LimitExceeded,
    /// A token budget was exceeded.  Retrying wastes tokens for no gain.
    #[error("Token budget exceeded: {0}")]
    BudgetExceeded(String),
    /// The AI produced output that failed format validation.  The retry
    /// should use an augmented prompt that reminds the model of the
    /// violated constraint rather than repeating the identical request.
    #[error("Format validation failed: {0}")]
    FormatRejection(String),
    /// The review was cancelled (e.g., superseded by a newer patchset version).
    #[error("Review cancelled")]
    Cancelled,
}

impl ClassifyAiError for ReviewError {
    fn ai_error_class(&self) -> AiErrorClass {
        match self {
            ReviewError::LimitExceeded => AiErrorClass::Fatal,
            ReviewError::BudgetExceeded(_) => AiErrorClass::Fatal,
            ReviewError::FormatRejection(_) => AiErrorClass::Fatal,
            ReviewError::Cancelled => AiErrorClass::Fatal,
        }
    }
}

use crate::activity::StageFailureRecord;
use crate::project::ProjectId;
use crate::workflow::{StageFailure, StageStopCause, WorkflowEngine, WorkflowEnv, WorkflowEvent};
use crate::workflows::linux_patch_review::{
    LinuxPatchReviewState, build_linux_patch_review_workflow_with_options, linux_system_prompt,
};
use crate::workflows::sashiko_patch_review::{
    build_sashiko_patch_review_workflow_with_options, sashiko_system_prompt,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

/// System identity prompt - used across all AI interactions
pub const SYSTEM_IDENTITY: &str = "";

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct PatchInput {
    pub index: i64,
    pub diff: String,
    pub subject: Option<String>,
    pub author: Option<String>,
    pub date: Option<i64>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub commit_id: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct ReviewInput {
    pub id: i64,
    pub subject: String,
    pub patches: Vec<PatchInput>,
}

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub project: ProjectId,
    pub max_input_tokens: usize,
    pub max_interactions: usize,
    pub temperature: f32,
    pub custom_prompt: Option<String>,
    pub series_range: Option<String>,
    pub baseline_sha: Option<String>,
    pub stages: Option<Vec<String>>,
    pub skip_report: bool,
}

#[derive(Debug, Clone)]
pub enum WorkerProgressEvent {
    PreScreenStarted,
    PlanningStarted,
    ReviewStarted {
        planned_stages: Vec<String>,
    },
    StageStarted {
        stage: String,
    },
    /// A stage finished. `cut_short` is set when the review ran out of time
    /// while it gathered evidence, so its output covers less than a full run.
    StageFinished {
        stage: String,
        cut_short: bool,
    },
    StageTurn {
        stage: String,
        turn: usize,
        max_turns: usize,
    },
    /// A stage started running tools, or finished (empty `tools`).
    ///
    /// Splits a turn's elapsed time into waiting on the model versus running
    /// git, which are indistinguishable from outside without this.
    StageTools {
        stage: String,
        tools: Vec<String>,
        turn: usize,
        max_turns: usize,
    },
    /// A stage is backing off after the provider asked us to slow down.
    /// `retry_in_seconds` is `None` once the wait is over.
    StageBackoff {
        stage: String,
        retry_in_seconds: Option<u64>,
        turn: usize,
        max_turns: usize,
    },
    /// A stage stopped without finishing.
    ///
    /// The counterpart to [`WorkerProgressEvent::StageFinished`], which is only
    /// reached on the success path. Without this, a stage that failed kept its
    /// last reported turn on display -- claiming to still be running -- for as
    /// long as the rest of the review took.
    StageFailed {
        stage: String,
        reason: String,
        cause: StageStopCause,
    },
    /// One message joining the conversation log, streamed so a running review
    /// can be read before it finishes. `stage` is `None` only for the system
    /// message the log opens with, which belongs to no stage.
    ///
    /// Carries the message itself rather than a flattened rendering of it. The
    /// finished log stores `AiMessage`s, so sharing the type is what keeps the
    /// live view and the finished view from drifting apart -- they drifted
    /// before precisely because nothing tied the two shapes together.
    LogEntry {
        stage: Option<String>,
        message: AiMessage,
    },
}

/// The copy of a message that is streamed to the live log.
///
/// The message the finished log will hold, changed only as the finished log
/// changes it: thought signatures are dropped, as
/// [`crate::ai::scrub_thought_signatures`] drops them from the stored history,
/// because they are opaque provider state, not conversation. Nothing else is
/// reshaped for a reader. The page redraws a running review's log as the
/// finished one when the review ends, and any difference between the two
/// copies would show as every changed block changing shape under the reader;
/// a display transform belongs in the page's one renderer, where both logs get
/// it. Content is carried whole for the same reason, and costs little: the
/// daemon proxies every request, so the same tool result already crosses the
/// pipe as part of the history.
pub(crate) fn live_log_message(msg: &AiMessage) -> AiMessage {
    AiMessage {
        role: msg.role.clone(),
        content: msg.content.clone(),
        thought: msg.thought.clone(),
        thought_signature: None,
        tool_calls: msg.tool_calls.as_ref().map(|calls| {
            calls
                .iter()
                .map(|c| {
                    // Scrubbed through the arguments too, as the finished log's
                    // scrub walks the whole history.
                    let mut arguments = c.arguments.clone();
                    crate::ai::scrub_thought_signatures(&mut arguments);
                    crate::ai::ToolCall {
                        id: c.id.clone(),
                        function_name: c.function_name.clone(),
                        arguments,
                        thought_signature: None,
                    }
                })
                .collect()
        }),
        tool_call_id: msg.tool_call_id.clone(),
    }
}

/// The progress event the worker reports for a workflow event, if any.
///
/// Kept apart from the callback so the mapping can be tested on its own: it is
/// the hop that carries a stopped stage's cause from the engine to the worker's
/// caller, and an engine test can only drive it with the causes the engine
/// produces today.
fn worker_progress_event(
    project: ProjectId,
    skip_report: bool,
    event: WorkflowEvent,
) -> Option<WorkerProgressEvent> {
    let counted = |stage_name: &str| crate::workflows::is_counted_stage(project, stage_name);
    match event {
        WorkflowEvent::StageStarted { stage_name } => match stage_name {
            "pre-screen" => Some(WorkerProgressEvent::PreScreenStarted),
            "planning" => Some(WorkerProgressEvent::PlanningStarted),
            _ if counted(stage_name) => Some(WorkerProgressEvent::StageStarted {
                stage: stage_name.to_string(),
            }),
            _ => None,
        },
        WorkflowEvent::ParallelResolved { stage_names } => {
            let mut planned_stages = crate::workflows::planned_stages_from(project, &stage_names);
            if skip_report {
                planned_stages.retain(|s| s != "report" && s != "summary");
            }
            Some(WorkerProgressEvent::ReviewStarted { planned_stages })
        }
        WorkflowEvent::StageFinished {
            stage_name,
            cut_short,
            ..
        } if counted(stage_name) => Some(WorkerProgressEvent::StageFinished {
            stage: stage_name.to_string(),
            cut_short,
        }),
        WorkflowEvent::StageTurn {
            stage_name,
            turn,
            max_turns,
        } if counted(stage_name) => Some(WorkerProgressEvent::StageTurn {
            stage: stage_name.to_string(),
            turn,
            max_turns,
        }),
        WorkflowEvent::StageTools {
            stage_name,
            tools,
            turn,
            max_turns,
        } if counted(stage_name) => Some(WorkerProgressEvent::StageTools {
            stage: stage_name.to_string(),
            tools,
            turn,
            max_turns,
        }),
        WorkflowEvent::StageBackoff {
            stage_name,
            retry_in_seconds,
            turn,
            max_turns,
        } if counted(stage_name) => Some(WorkerProgressEvent::StageBackoff {
            stage: stage_name.to_string(),
            retry_in_seconds,
            turn,
            max_turns,
        }),
        WorkflowEvent::StageFailed {
            stage_name,
            reason,
            cause,
        } if counted(stage_name) => Some(WorkerProgressEvent::StageFailed {
            stage: stage_name.to_string(),
            reason,
            cause,
        }),
        // Every stage, not only the counted ones. The finished log holds the
        // pre-screen, planning and consolidation conversations too; the
        // counted-stage gate above is about which stages count on the progress
        // bar, and applying it here would make the live log a subset of the
        // finished one.
        WorkflowEvent::StageMessage {
            stage_name,
            message,
        } => Some(WorkerProgressEvent::LogEntry {
            stage: Some(stage_name.to_string()),
            message: live_log_message(&message),
        }),
        _ => None,
    }
}

/// The `stage_failures` array of the worker's output, which the caller stores
/// as `reviews.stage_failures`.
///
/// Built from [`StageFailureRecord`], the shape the daemon writes for an
/// attempt that ended without a review, so both writers agree. The patch is
/// left unset: one workflow run reviews one patch, and the caller that
/// gathers the runs of a series marks each entry with its patch.
fn stage_failures_json(failures: &[StageFailure]) -> Vec<StageFailureRecord> {
    failures
        .iter()
        .map(|f| StageFailureRecord {
            stage: f.stage_name.to_string(),
            reason: f.reason.clone(),
            cause: f.cause,
            patch_index: None,
        })
        .collect()
}

pub struct WorkerResult {
    pub output: Option<Value>,
    pub error: Option<String>,
    pub input_context: String,
    /// The review's conversation in the shape a stored log keeps: the
    /// system prompt, which belongs to no stage, then each stage's messages
    /// marked with their stage.
    pub history: Vec<ReviewLogEntry>,
    pub history_before_pruning: Vec<ReviewLogEntry>,
    pub history_after_pruning: Vec<ReviewLogEntry>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
}

pub struct PromptRegistry {
    pub base_dir: PathBuf,
}

impl PromptRegistry {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn get_system_identity() -> &'static str {
        SYSTEM_IDENTITY
    }

    pub fn calculate_content_hash<T: serde::Serialize>(
        &self,
        content: &str,
        tools: Option<&[T]>,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        if let Some(tools) = tools
            && let Ok(json) = serde_json::to_string(tools)
        {
            hasher.update(json);
        }
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }
}

pub struct Worker {
    project: ProjectId,
    provider: Arc<dyn AiProvider>,
    tools: Arc<ToolBox>,
    prompts: PromptRegistry,
    global_history: Vec<ReviewLogEntry>,
    max_interactions: usize,
    temperature: f32,
    series_range: Option<String>,
    baseline_sha: Option<String>,
    context_tag: Option<String>,
    stages: Option<Vec<String>>,
    custom_prompt: Option<String>,
    skip_report: bool,
    /// Set when the daemon says the review reached its deadline. `None`
    /// unless the daemon spawned this worker.
    wind_down: Option<Arc<crate::ai::WindDownSignal>>,
}

impl Worker {
    pub fn new(
        provider: Arc<dyn AiProvider>,
        tools: Arc<ToolBox>,
        prompts: PromptRegistry,
        config: WorkerConfig,
    ) -> Self {
        Self {
            project: config.project,
            provider,
            tools,
            prompts,
            global_history: Vec::new(),
            max_interactions: config.max_interactions,
            temperature: config.temperature,
            series_range: config.series_range,
            baseline_sha: config.baseline_sha,
            context_tag: None,
            stages: config.stages,
            custom_prompt: config.custom_prompt,
            skip_report: config.skip_report,
            wind_down: None,
        }
    }

    /// Lets the review's deadline wind this worker's review down.
    ///
    /// Only a daemon-spawned worker has a daemon on stdin to send the
    /// wind-down, so only it passes the signal its stdin reader sets.
    pub fn with_wind_down(mut self, signal: Arc<crate::ai::WindDownSignal>) -> Self {
        self.wind_down = Some(signal);
        self
    }

    pub async fn run(
        &mut self,
        patchset: Value,
        progress: Option<&(dyn Fn(WorkerProgressEvent) + Send + Sync)>,
    ) -> Result<WorkerResult> {
        let mut target_commit_diff = String::new();
        let mut target_commit_diff_only = String::new();

        let ps_id = patchset["id"]
            .as_i64()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let p_id = patchset["patch_index"]
            .as_i64()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "multi".to_string());
        self.context_tag = Some(format!("[ps:{} p:{}] ", ps_id, p_id));

        let baseline_sha = self
            .baseline_sha
            .clone()
            .or_else(|| {
                patchset
                    .get("baseline")
                    .and_then(|b| b.as_str())
                    .map(|s| s.to_string())
            })
            .or_else(|| {
                self.series_range.as_ref().and_then(|range| {
                    let parts: Vec<&str> = range.split("..").collect();
                    if !parts.is_empty() && !parts[0].is_empty() {
                        Some(parts[0].to_string())
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| "unknown".to_string());

        let mut target_commit_sha = "unknown".to_string();
        if let Some(patches) = patchset["patches"].as_array() {
            if let Some(idx) = patchset["patch_index"].as_i64()
                && let Some(p) = patches.iter().find(|p| p["index"].as_i64() == Some(idx))
                && let Some(sha) = p["commit_id"].as_str()
            {
                target_commit_sha = sha.to_string();
            }
            if target_commit_sha == "unknown"
                && !patches.is_empty()
                && let Some(sha) = patches[0]["commit_id"].as_str()
            {
                target_commit_sha = sha.to_string();
            }
        }

        if let Some(patches) = patchset["patches"].as_array() {
            let target_patches: Vec<&Value> = if let Some(idx) = patchset["patch_index"].as_i64() {
                let filtered: Vec<&Value> = patches
                    .iter()
                    .filter(|p| p["index"].as_i64() == Some(idx))
                    .collect();
                if filtered.is_empty() {
                    patches.iter().collect()
                } else {
                    filtered
                }
            } else {
                patches.iter().collect()
            };

            for p in target_patches {
                let diff_body = p["diff"].as_str().unwrap_or("");
                let changelog_opt = crate::patch::extract_changelog_from_body(diff_body);

                if let Some(show) = p["git_show"].as_str() {
                    if let Some(ref changelog) = changelog_opt {
                        let enriched_show =
                            crate::patch::inject_changelog_into_git_show(show, changelog);
                        target_commit_diff.push_str(&enriched_show);
                    } else {
                        target_commit_diff.push_str(show);
                    }
                    target_commit_diff.push('\n');
                } else {
                    target_commit_diff.push_str(diff_body);
                    target_commit_diff.push('\n');
                }

                if let Some(diff) = p["diff"].as_str() {
                    target_commit_diff_only.push_str(diff);
                    target_commit_diff_only.push('\n');
                }
            }
        }

        let worktree_path = self.tools.get_worktree_path();
        let (prefetched_context, prefetch_failed) = match crate::worker::prefetch::prefetch_context(
            worktree_path,
            &target_commit_sha,
            &target_commit_diff,
        )
        .await
        {
            Ok(context) => (context, false),
            Err(error) => {
                tracing::warn!(
                    target_commit = %target_commit_sha,
                    %error,
                    "Source prefetch failed; review must retrieve context with Git tools"
                );
                (String::new(), true)
            }
        };

        let follow_up_series_context = match self.project {
            ProjectId::Linux => build_follow_up_series_context(
                self.series_range.as_deref(),
                &patchset,
                &target_commit_sha,
            ),
            ProjectId::Sashiko => build_sashiko_series_context(
                self.series_range.as_deref(),
                &patchset,
                &target_commit_sha,
            ),
        };

        let mut state = LinuxPatchReviewState {
            ps_id,
            p_id,
            target_commit_sha,
            baseline_sha,
            target_commit_diff,
            target_commit_diff_only,
            prefetched_context,
            prefetch_failed,
            series_range: self.series_range.clone(),
            follow_up_series_context,
            references: self.tools.references().to_vec(),
            selected_guides: Vec::new(),
            manual_stages: self.stages.clone(),
            custom_prompt: self.custom_prompt.clone(),
            planned_stages: Vec::new(),
            skip_report: self.skip_report,
            all_concerns: Vec::new(),
            all_dismissed_concerns: Vec::new(),
            deduplicated_concerns: Vec::new(),
            deduplicated_dismissed_concerns: Vec::new(),
            patch_concerns: Vec::new(),
            concerns: Vec::new(),
            findings: Vec::new(),
            summary: String::new(),
            review_inline: String::new(),
            fixes: String::new(),
        };

        if self.global_history.is_empty() {
            let sys_template = match self.project {
                ProjectId::Linux => linux_system_prompt(true),
                ProjectId::Sashiko => sashiko_system_prompt(true),
            };
            let rendered_sys = sys_template.render_for_log(&state);
            let system = AiMessage {
                role: crate::ai::AiRole::System,
                content: Some(rendered_sys),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            };
            // The finished log opens with this message, so the live one does
            // too. It belongs to no stage.
            if let Some(progress_cb) = progress {
                progress_cb(WorkerProgressEvent::LogEntry {
                    stage: None,
                    message: live_log_message(&system),
                });
            }
            self.global_history.push(ReviewLogEntry::new(None, system));
        }

        let workflow = match self.project {
            ProjectId::Linux => build_linux_patch_review_workflow_with_options(
                self.max_interactions,
                self.temperature,
            ),
            ProjectId::Sashiko => build_sashiko_patch_review_workflow_with_options(
                self.max_interactions,
                self.temperature,
            ),
        };
        let env = WorkflowEnv {
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            base_dir: &self.prompts.base_dir,
            context_tag: self.context_tag.clone(),
            wind_down: self.wind_down.clone(),
        };

        let project = self.project;
        let skip_report = self.skip_report;
        let event_cb = move |event: WorkflowEvent| {
            if let Some(progress_cb) = progress
                && let Some(event) = worker_progress_event(project, skip_report, event)
            {
                progress_cb(event);
            }
        };

        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, Some(&event_cb)).await?;
        self.global_history.extend(outcome.history.clone());

        let concerns_count = state.all_concerns.len();
        let dismissed_concerns = if !state.deduplicated_dismissed_concerns.is_empty() {
            state.deduplicated_dismissed_concerns.clone()
        } else {
            state.all_dismissed_concerns.clone()
        };
        let dismissed_concerns_count = dismissed_concerns.len();

        let review_inline = if state.review_inline.is_empty() && !self.skip_report {
            "No issues found.".to_string()
        } else {
            state.review_inline
        };

        // Stages that did not complete while their siblings did, and stages
        // the deadline cut short, so the caller can report which parts of the
        // review are missing instead of presenting partial coverage as full.
        // A batch where every stage failed never gets here: the engine turns
        // that into an error.
        let stage_failures = stage_failures_json(&outcome.stage_failures);

        if !stage_failures.is_empty() {
            tracing::warn!(
                "{} review stages did not complete: {:?}",
                stage_failures.len(),
                outcome
                    .stage_failures
                    .iter()
                    .map(|f| f.stage_name)
                    .collect::<Vec<_>>()
            );
        }

        let final_output = json!({
            "summary": state.summary,
            "findings": state.findings,
            "dismissed_concerns": dismissed_concerns,
            "concerns": state.concerns,
            "review_inline": review_inline,
            "fixes": state.fixes,
            "concerns_count": concerns_count,
            "stage_failures": stage_failures,
            "dismissed_concerns_count": dismissed_concerns_count,
        });

        Ok(WorkerResult {
            output: Some(final_output),
            error: None,
            input_context: "Multi-stage execution completed".to_string(),
            history: self.global_history.clone(),
            history_before_pruning: self.global_history.clone(),
            history_after_pruning: self.global_history.clone(),
            tokens_in: outcome.tokens_in,
            tokens_out: outcome.tokens_out,
            tokens_cached: outcome.tokens_cached,
        })
    }
}

pub fn calculate_series_range(
    patches: &[PatchInput],
    patches_to_review: &[PatchInput],
    patch_shas: &std::collections::HashMap<i64, String>,
    baseline_sha: &str,
) -> Option<String> {
    if patches.is_empty() {
        return None;
    }

    let max_patch_index = patches.iter().map(|p| p.index).max().unwrap_or(0);
    let is_last_patch_review =
        patches_to_review.len() == 1 && patches_to_review[0].index == max_patch_index;

    if is_last_patch_review {
        None
    } else {
        patches
            .iter()
            .map(|p| p.index)
            .max()
            .and_then(|max_idx| {
                patches
                    .iter()
                    .find(|p| p.index == max_idx)
                    .and_then(|p| p.commit_id.clone())
                    .or_else(|| patch_shas.get(&max_idx).cloned())
            })
            .map(|end_sha| format!("{}..{}", baseline_sha, end_sha))
    }
}

pub fn build_follow_up_series_context(
    series_range: Option<&str>,
    patchset: &Value,
    target_commit_sha: &str,
) -> Option<String> {
    let range = series_range?;
    let end_sha = range.split("..").nth(1)?;
    if end_sha.is_empty() {
        return None;
    }

    let current_idx = patchset["patch_index"].as_i64().unwrap_or(1);
    let patches = patchset["patches"].as_array()?;
    let total_patches = patches.len();

    let current_subject = patches
        .iter()
        .find(|p| p["index"].as_i64() == Some(current_idx))
        .and_then(|p| p["subject"].as_str())
        .unwrap_or("unknown");

    let mut follow_ups = Vec::new();
    for p in patches {
        let idx = p["index"].as_i64().unwrap_or(0);
        if idx > current_idx {
            let subj = p["subject"].as_str().unwrap_or("");
            let commit_id = p["commit_id"].as_str();
            follow_ups.push((idx, commit_id, subj));
        }
    }

    if follow_ups.is_empty() {
        return None;
    }

    follow_ups.sort_by_key(|(idx, _, _)| *idx);

    let mut block = String::new();
    block.push_str("\n\n=== Follow-Up Patches in Series ===\n");
    block.push_str(&format!(
        "Current Patch Under Review: [Patch {} of {}] - {}\n",
        current_idx, total_patches, current_subject
    ));
    block.push_str(&format!("Series End Commit (Final State): {}\n\n", end_sha));
    block.push_str("Subsequent patches in this series:\n");

    for (idx, commit_id, subj) in follow_ups {
        if let Some(sha) = commit_id {
            block.push_str(&format!(
                "- [Patch {} of {}] (commit {}): {}\n",
                idx, total_patches, sha, subj
            ));
        } else {
            block.push_str(&format!(
                "- [Patch {} of {}]: {}\n",
                idx, total_patches, subj
            ));
        }
    }

    let diff_directive = if target_commit_sha != "unknown" && !target_commit_sha.is_empty() {
        format!(
            "Use tools (e.g., git_diff with base_revision=\"{}\", target_revision=\"{}\", or git_read_files with revision=\"{}\") to inspect the final code state at the end of the series.",
            target_commit_sha, end_sha, end_sha
        )
    } else {
        format!(
            "Use tools (e.g., git_diff with target_revision=\"{}\", or git_read_files with revision=\"{}\") to inspect the final code state at the end of the series.",
            end_sha, end_sha
        )
    };

    block.push_str("\nSERIES VERIFICATION DIRECTIVE:\n");
    block.push_str(&format!(
        "Verify if any candidate concern raised against this patch is fixed, refactored, or resolved in the subsequent patches listed above. {} If a concern is resolved by follow-up patches in this series, discard it as a false positive.\n",
        diff_directive
    ));
    block.push_str("===================================\n");

    Some(block)
}

pub fn build_sashiko_series_context(
    series_range: Option<&str>,
    patchset: &Value,
    target_commit_sha: &str,
) -> Option<String> {
    let patches = patchset["patches"].as_array()?;
    let total_patches = patches.len();
    if total_patches <= 1 {
        return None;
    }

    let current_idx = patchset["patch_index"].as_i64().unwrap_or(1);
    let current_subject = patches
        .iter()
        .find(|p| p["index"].as_i64() == Some(current_idx))
        .and_then(|p| p["subject"].as_str())
        .unwrap_or("unknown");

    let mut preceding = Vec::new();
    let mut follow_ups = Vec::new();
    for p in patches {
        let idx = p["index"].as_i64().unwrap_or(0);
        let subj = p["subject"].as_str().unwrap_or("");
        let commit_id = p["commit_id"].as_str();
        if idx > 0 && idx < current_idx {
            preceding.push((idx, commit_id, subj));
        } else if idx > current_idx {
            follow_ups.push((idx, commit_id, subj));
        }
    }

    if preceding.is_empty() && follow_ups.is_empty() {
        return None;
    }

    preceding.sort_by_key(|(idx, _, _)| *idx);
    follow_ups.sort_by_key(|(idx, _, _)| *idx);

    let end_sha = series_range
        .and_then(|r| r.split("..").nth(1))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            patches
                .iter()
                .max_by_key(|p| p["index"].as_i64().unwrap_or(0))
                .and_then(|p| p["commit_id"].as_str())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or(target_commit_sha);

    let mut block = String::new();
    block.push_str("\n\n=== Patch Series Context ===\n");
    block.push_str(&format!(
        "Current Patch Under Review: [Patch {} of {}] - {}\n",
        current_idx, total_patches, current_subject
    ));
    if !end_sha.is_empty() && end_sha != "unknown" {
        block.push_str(&format!("Series End Commit (Final State): {}\n", end_sha));
    }

    if !preceding.is_empty() {
        block.push_str(
            "\nPreceding patches in this series (already applied in HEAD / current revision):\n",
        );
        for (idx, commit_id, subj) in &preceding {
            if let Some(sha) = commit_id {
                block.push_str(&format!(
                    "- [Patch {} of {}] (commit {}): {}\n",
                    idx, total_patches, sha, subj
                ));
            } else {
                block.push_str(&format!(
                    "- [Patch {} of {}]: {}\n",
                    idx, total_patches, subj
                ));
            }
        }
    }

    if !follow_ups.is_empty() {
        block.push_str("\nSubsequent patches in this series:\n");
        for (idx, commit_id, subj) in &follow_ups {
            if let Some(sha) = commit_id {
                block.push_str(&format!(
                    "- [Patch {} of {}] (commit {}): {}\n",
                    idx, total_patches, sha, subj
                ));
            } else {
                block.push_str(&format!(
                    "- [Patch {} of {}]: {}\n",
                    idx, total_patches, subj
                ));
            }
        }
    }

    let end_ref = if !end_sha.is_empty() && end_sha != "unknown" {
        end_sha
    } else {
        "HEAD"
    };

    block.push_str("\nSERIES VERIFICATION DIRECTIVE:\n");
    block.push_str(&format!(
        "1. If this patch introduces foundational types, helpers, schema changes, or configuration fields whose callers, endpoints, or CLI flags are wired up in subsequent patches of this series, or if a candidate concern is fixed or refactored by the end of the series, inspect the final code state at revision \"{}\" (e.g., using git_read_files with revision=\"{}\" or git_diff with target_revision=\"{}\") and discard the concern as a false positive.\n",
        end_ref, end_ref, end_ref
    ));
    block.push_str(&format!(
        "2. If this patch adds or updates documentation or a design document (such as designs/*.md) describing changes implemented in other patches of this series, verify any code snippets or architectural claims against the actual Rust implementation in src/ at revision \"{}\" before reporting an issue. If the actual Rust implementation is sound, discard concerns about abbreviated documentation snippets.\n",
        end_ref
    ));
    block.push_str("===================================\n");

    Some(block)
}

#[cfg(test)]
fn append_stage_items(
    target: &mut Vec<Value>,
    items: &[Value],
    stage: &str,
    default_type: &str,
    default_text_key: &str,
) {
    for item in items {
        if let Some(item) = normalize_stage_item(item, stage, default_type, default_text_key) {
            target.push(item);
        }
    }
}

#[cfg(test)]
fn append_stage_dismissed_concerns(target: &mut Vec<Value>, items: &[Value], stage: &str) {
    append_stage_items(target, items, stage, "General", "description");
}

#[cfg(test)]
fn normalize_stage_item(
    item: &Value,
    stage: &str,
    default_type: &str,
    default_text_key: &str,
) -> Option<Value> {
    if let Some(obj) = item.as_object() {
        let mut with_stage = obj.clone();
        with_stage.insert("source_stage".to_string(), json!(stage));
        Some(Value::Object(with_stage))
    } else {
        item.as_str().map(|s| {
            let mut obj = serde_json::Map::new();
            obj.insert("source_stage".to_string(), json!(stage));
            obj.insert("type".to_string(), json!(default_type));
            obj.insert(default_text_key.to_string(), json!(s));
            Value::Object(obj)
        })
    }
}

#[cfg(test)]
mod prefetch_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_planned_stages_follow_the_resolved_fan_out() {
        assert_eq!(
            crate::workflows::planned_stages_from(
                ProjectId::Linux,
                &["goal", "implementation", "locking"]
            ),
            [
                "goal",
                "implementation",
                "locking",
                "deduplication",
                "conflict-resolution",
                "verification",
                "report"
            ]
        );
        assert_eq!(
            crate::workflows::planned_stages_from(
                ProjectId::Sashiko,
                &["goal", "implementation", "concurrency", "llm-pipeline"]
            ),
            [
                "goal",
                "implementation",
                "concurrency",
                "llm-pipeline",
                "deduplication",
                "conflict-resolution",
                "verification",
                "report",
                "summary"
            ]
        );
        assert_eq!(
            crate::workflows::planned_stages_from(ProjectId::Linux, &[]),
            Vec::<String>::new()
        );
        // Only analysis stages come through the fan-out.
        assert_eq!(
            crate::workflows::planned_stages_from(ProjectId::Linux, &["planning"]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_every_planned_stage_reports_its_progress() {
        for stage in crate::workflows::planned_stages_from(ProjectId::Linux, &["goal", "locking"]) {
            assert!(
                crate::workflows::is_counted_stage(ProjectId::Linux, &stage),
                "{stage} is counted but silent"
            );
        }
        for stage in
            crate::workflows::planned_stages_from(ProjectId::Sashiko, &["goal", "llm-pipeline"])
        {
            assert!(
                crate::workflows::is_counted_stage(ProjectId::Sashiko, &stage),
                "{stage} is counted but silent"
            );
        }

        // The pre-screen and the planner count as well, from the moment they
        // start: each does a model call, and a bar that leaves them out sits at
        // zero while two stages come and go.
        assert!(crate::workflows::is_counted_stage(
            ProjectId::Linux,
            "pre-screen"
        ));
        assert!(crate::workflows::is_counted_stage(
            ProjectId::Sashiko,
            "planning"
        ));
    }

    #[test]
    fn test_append_stage_dismissed_concerns_preserves_category_type() {
        let mut items = Vec::new();
        let input = vec![json!({
            "type": "Resource Management",
            "description": "suspected cross-zone page leak does not apply",
            "reasoning": "hugetlb_free_cross_zone_pages() runs before HVO init"
        })];

        append_stage_dismissed_concerns(&mut items, &input, "goal");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], "goal");
        assert_eq!(items[0]["type"], "Resource Management");
        assert_eq!(
            items[0]["reasoning"],
            "hugetlb_free_cross_zone_pages() runs before HVO init"
        );
    }

    #[test]
    fn test_append_stage_dismissed_concerns_normalizes_string_items() {
        let mut items = Vec::new();
        let input = vec![json!("suspected missing cleanup does not apply")];

        append_stage_dismissed_concerns(&mut items, &input, "implementation");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], "implementation");
        assert_eq!(items[0]["type"], "General");
        assert_eq!(
            items[0]["description"],
            "suspected missing cleanup does not apply"
        );
    }

    #[test]
    fn test_append_stage_items_overwrites_existing_source_stage() {
        let mut items = Vec::new();
        let input = vec![json!({
            "source_stage": "execution-flow",
            "type": "Execution flow",
            "description": "already annotated"
        })];

        append_stage_items(&mut items, &input, "resources", "General", "description");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], "resources");
    }

    #[test]
    fn test_append_stage_items_normalizes_string_items() {
        let mut items = Vec::new();
        let input = vec![json!("plain concern")];

        append_stage_items(&mut items, &input, "security", "General", "description");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], "security");
        assert_eq!(items[0]["type"], "General");
        assert_eq!(items[0]["description"], "plain concern");
    }

    #[test]
    fn test_calculate_series_range_single_patch() {
        let p = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let patches = vec![p.clone()];
        let patches_to_review = vec![p.clone()];
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            None
        );
    }

    #[test]
    fn test_calculate_series_range_multi_patch_last() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p2.clone()]; // Reviewing last
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            None
        );
    }

    #[test]
    fn test_calculate_series_range_multi_patch_middle() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p1.clone()]; // Reviewing first
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            Some("base..sha2".to_string())
        );
    }

    #[test]
    fn test_calculate_series_range_use_patch_shas_map() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: None, // Missing in input
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: None, // Missing in input
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p1.clone()];

        let mut patch_shas = std::collections::HashMap::new();
        patch_shas.insert(2, "sha2_resolved".to_string());

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            Some("base..sha2_resolved".to_string())
        );
    }

    #[test]
    fn test_build_follow_up_series_context_none_when_no_range() {
        let patchset = serde_json::json!({
            "patch_index": 1,
            "patches": [{
                "index": 1,
                "subject": "Single patch",
                "commit_id": "sha1"
            }]
        });
        assert_eq!(
            build_follow_up_series_context(None, &patchset, "sha1"),
            None
        );
    }

    #[test]
    fn test_build_follow_up_series_context_none_when_last_patch() {
        let patchset = serde_json::json!({
            "patch_index": 2,
            "patches": [
                { "index": 1, "subject": "Patch 1", "commit_id": "sha1" },
                { "index": 2, "subject": "Patch 2", "commit_id": "sha2" }
            ]
        });
        assert_eq!(
            build_follow_up_series_context(Some("base..sha2"), &patchset, "sha2"),
            None
        );
    }

    #[test]
    fn test_build_follow_up_series_context_intermediate_patch() {
        let patchset = serde_json::json!({
            "patch_index": 1,
            "patches": [
                { "index": 1, "subject": "net: add foo API", "commit_id": "sha1" },
                { "index": 2, "subject": "net: add caller for foo", "commit_id": "sha2" },
                { "index": 3, "subject": "net: add docs for foo", "commit_id": "sha3" }
            ]
        });
        let ctx = build_follow_up_series_context(Some("base..sha3"), &patchset, "sha1");
        assert!(ctx.is_some());
        let content = ctx.unwrap();
        assert!(content.contains("Current Patch Under Review: [Patch 1 of 3] - net: add foo API"));
        assert!(content.contains("Series End Commit (Final State): sha3"));
        assert!(content.contains("- [Patch 2 of 3] (commit sha2): net: add caller for foo"));
        assert!(content.contains("- [Patch 3 of 3] (commit sha3): net: add docs for foo"));
        assert!(!content.contains("- [Patch 1 of 3]"));
        assert!(content.contains("SERIES VERIFICATION DIRECTIVE:"));
        assert!(content.contains("base_revision=\"sha1\""));
        assert!(content.contains("target_revision=\"sha3\""));
        assert!(content.contains("git_read_files with revision=\"sha3\""));
    }

    #[test]
    fn test_build_follow_up_series_context_unordered_patches() {
        let patchset = serde_json::json!({
            "patch_index": 1,
            "patches": [
                { "index": 3, "subject": "Patch 3", "commit_id": "sha3" },
                { "index": 1, "subject": "Patch 1", "commit_id": "sha1" },
                { "index": 2, "subject": "Patch 2", "commit_id": "sha2" }
            ]
        });
        let ctx = build_follow_up_series_context(Some("base..sha3"), &patchset, "sha1");
        assert!(ctx.is_some());
        let content = ctx.unwrap();
        let p2_pos = content.find("Patch 2 of 3").unwrap();
        let p3_pos = content.find("Patch 3 of 3").unwrap();
        assert!(
            p2_pos < p3_pos,
            "Patch 2 should appear before Patch 3 in follow-up list"
        );
    }

    #[test]
    fn test_build_follow_up_series_context_unknown_target_sha() {
        let patchset = serde_json::json!({
            "patch_index": 1,
            "patches": [
                { "index": 1, "subject": "Patch 1" },
                { "index": 2, "subject": "Patch 2", "commit_id": "sha2" }
            ]
        });
        let ctx = build_follow_up_series_context(Some("base..sha2"), &patchset, "unknown");
        assert!(ctx.is_some());
        let content = ctx.unwrap();
        assert!(!content.contains("base_revision=\"unknown\""));
        assert!(content.contains("target_revision=\"sha2\""));
    }

    struct MockProviderAlwaysFails;
    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockProviderAlwaysFails {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            anyhow::bail!("mock: simulated AI failure")
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_stage_failure_aborts_review() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let provider = std::sync::Arc::new(MockProviderAlwaysFails);
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            project: ProjectId::Linux,
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            baseline_sha: None,
            custom_prompt: None,
            stages: None,
            skip_report: false,
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
        });

        match worker.run(patchset, None).await {
            Ok(_) => panic!("Expected stage failure error, got Ok"),
            Err(e) => assert!(
                e.to_string().contains("simulated AI failure"),
                "unexpected error: {e}"
            ),
        }
    }

    /// Fails every request from one named stage and answers the rest with an
    /// empty concern list, so a batch can partly fail. The stage is read from
    /// the `[s:NAME]` context tag, the same place the daemon reads it from.
    struct MockProviderFailsOneStage {
        failing_stage: &'static str,
    }

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockProviderFailsOneStage {
        async fn generate_content(
            &self,
            request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let stage = request
                .context_tag
                .as_deref()
                .and_then(crate::activity::stage_from_context_tag);
            if stage.as_deref() == Some(self.failing_stage) {
                anyhow::bail!("mock: simulated failure in {}", self.failing_stage)
            }
            Ok(crate::ai::AiResponse {
                content: Some(r#"{"concerns": [], "dismissed_concerns": []}"#.to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    /// The concurrent analysis stages run under `BestEffort`, so one failing
    /// stage does not abort the review while a sibling succeeds. That is only
    /// acceptable because the failure is reported: partial coverage presented
    /// as full coverage would be worse than an outright failure. (A batch where
    /// every stage fails is still an error; see the test above.)
    #[tokio::test]
    async fn test_stage_failure_is_reported_not_swallowed() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let provider = std::sync::Arc::new(MockProviderFailsOneStage {
            failing_stage: "goal",
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            project: ProjectId::Linux,
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            baseline_sha: None,
            custom_prompt: None,
            // Named explicitly so the pre-screen and planning stages skip and
            // the run reaches the concurrent analysis stages, which are the
            // ones the best-effort policy covers.
            stages: Some(vec!["goal".to_string(), "implementation".to_string()]),
            skip_report: false,
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
        });

        let failed: std::sync::Mutex<Vec<(String, StageStopCause)>> =
            std::sync::Mutex::new(Vec::new());
        let record = |e: WorkerProgressEvent| {
            if let WorkerProgressEvent::StageFailed { stage, cause, .. } = e {
                failed.lock().unwrap().push((stage, cause));
            }
        };

        let res = worker
            .run(patchset, Some(&record))
            .await
            .expect("a partly failed batch is still a review");
        let output = res.output.expect("output");
        let failures = output["stage_failures"].as_array().expect("array");
        assert_eq!(failures.len(), 1, "{output}");
        assert_eq!(failures[0]["stage"], "goal");
        assert_eq!(failures[0]["cause"], "error", "{output}");
        assert!(failures[0].get("cancelled").is_none(), "{output}");
        assert!(
            failures[0]["reason"]
                .as_str()
                .unwrap()
                .contains("simulated failure in goal"),
            "{output}"
        );
        assert_eq!(
            *failed.lock().unwrap(),
            [("goal".to_string(), StageStopCause::Error)],
            "the failing stage must also report itself live, with its cause"
        );
    }

    /// Every stop cause the engine can name, in one place for the tests that
    /// must carry each of them unchanged.
    const ALL_STOP_CAUSES: [StageStopCause; 5] = [
        StageStopCause::Error,
        StageStopCause::Cancelled,
        StageStopCause::OutOfTime,
        StageStopCause::CutShort,
        StageStopCause::Interrupted,
    ];

    #[test]
    fn worker_progress_event_carries_the_stop_cause() {
        for cause in ALL_STOP_CAUSES {
            let event = worker_progress_event(
                ProjectId::Linux,
                false,
                WorkflowEvent::StageFailed {
                    stage_name: "goal",
                    reason: "stopped".to_string(),
                    cause,
                },
            );
            match event {
                Some(WorkerProgressEvent::StageFailed {
                    stage,
                    reason,
                    cause: got,
                }) => {
                    assert_eq!(stage, "goal");
                    assert_eq!(reason, "stopped");
                    assert_eq!(got, cause, "the cause must reach the worker's caller");
                }
                other => panic!("expected StageFailed for {cause:?}, got {other:?}"),
            }
        }
    }

    /// Whether a finished stage was cut short reaches the worker's caller as
    /// it was given, for both values.
    #[test]
    fn worker_progress_event_carries_whether_a_stage_was_cut_short() {
        for cut_short in [false, true] {
            let event = worker_progress_event(
                ProjectId::Linux,
                false,
                WorkflowEvent::StageFinished {
                    stage_name: "goal",
                    tokens_in: 1,
                    tokens_out: 1,
                    tokens_cached: 0,
                    cut_short,
                },
            );
            match event {
                Some(WorkerProgressEvent::StageFinished {
                    stage,
                    cut_short: got,
                }) => {
                    assert_eq!(stage, "goal");
                    assert_eq!(got, cut_short);
                }
                other => panic!("expected StageFinished, got {other:?}"),
            }
        }
    }

    #[test]
    fn stage_failures_json_writes_the_cause_in_its_stored_spelling() {
        let failures: Vec<StageFailure> = ALL_STOP_CAUSES
            .iter()
            .map(|&cause| StageFailure {
                stage_name: "goal",
                reason: "stopped".to_string(),
                cause,
            })
            .collect();
        let json = serde_json::to_value(stage_failures_json(&failures)).expect("serialises");
        let json = json.as_array().expect("an array");
        let spellings: Vec<&str> = json
            .iter()
            .map(|f| f["cause"].as_str().expect("cause is a string"))
            .collect();
        assert_eq!(
            spellings,
            [
                "error",
                "cancelled",
                "out_of_time",
                "cut_short",
                "interrupted"
            ]
        );
        for f in json {
            assert_eq!(f["stage"], "goal");
            assert_eq!(f["reason"], "stopped");
            assert!(f.get("cancelled").is_none(), "{f}");
        }
    }

    // ReviewError tests

    #[test]
    fn test_limit_exceeded_classifies_as_fatal() {
        let err = ReviewError::LimitExceeded;

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_budget_exceeded_classifies_as_fatal() {
        let err = ReviewError::BudgetExceeded("1000 tokens used (limit: 500)".to_string());

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_format_rejection_classifies_as_fatal() {
        let err = ReviewError::FormatRejection("contains markdown code blocks".to_string());

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_limit_exceeded_downcasts_as_review_error() {
        let err: anyhow::Error = ReviewError::LimitExceeded.into();
        assert!(
            err.downcast_ref::<ReviewError>().is_some(),
            "LimitExceeded must downcast to ReviewError so the retry loop can fail fast"
        );
    }

    #[test]
    fn test_budget_exceeded_downcasts_as_review_error() {
        let err: anyhow::Error =
            ReviewError::BudgetExceeded("1000 tokens used (limit: 500)".to_string()).into();
        assert!(
            err.downcast_ref::<ReviewError>().is_some(),
            "BudgetExceeded must downcast to ReviewError so the retry loop can fail fast"
        );
    }

    #[test]
    fn test_generic_error_does_not_downcast_as_review_error() {
        let err: anyhow::Error = anyhow::anyhow!("transient JSON parse failure");
        assert!(
            err.downcast_ref::<ReviewError>().is_none(),
            "Plain anyhow errors must NOT match ReviewError so they remain retryable"
        );
    }

    #[test]
    fn test_format_rejection_downcasts_as_review_error() {
        let err: anyhow::Error =
            ReviewError::FormatRejection("contains markdown code blocks".to_string()).into();
        assert!(
            err.downcast_ref::<ReviewError>().is_some(),
            "FormatRejection must downcast to ReviewError"
        );
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockBlockedProvider {
        attempts: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockBlockedProvider {
        async fn generate_content(
            &self,
            request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                anyhow::bail!(
                    "Remote AI Error: Gemini candidate blocked (finish reason: RECITATION)"
                )
            } else {
                let has_filter = request.messages.iter().any(|m| {
                    m.role == crate::ai::AiRole::User
                        && m.content
                            .as_ref()
                            .is_some_and(|c| c.contains("recitation filter"))
                });
                if has_filter {
                    return Ok(crate::ai::AiResponse {
                        content: Some(r#"{"concerns": [], "dismissed_concerns": []}"#.to_string()),
                        thought: None,
                        thought_signature: None,
                        tool_calls: None,
                        usage: None,
                        truncated: false,
                    });
                }
                anyhow::bail!(
                    "Remote AI Error: Gemini candidate blocked again (finish reason: RECITATION)"
                )
            }
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_recitation_error_triggers_prompt_perturbation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let provider = std::sync::Arc::new(MockBlockedProvider {
            attempts: AtomicUsize::new(0),
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            project: ProjectId::Linux,
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            baseline_sha: None,
            custom_prompt: None,
            stages: Some(vec!["goal".to_string()]),
            skip_report: false,
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
        });

        let res = worker.run(patchset, None).await;
        if let Err(e) = &res {
            panic!("Expected run to succeed, got error: {:?}", e);
        }
    }

    /// The reference framing must reach the model, and the out-of-tree caveats
    /// must stay away unless a kernel reference is attached -- telling an
    /// in-tree review that Fixes: tags do not apply would suppress real
    /// findings.
    #[tokio::test]
    async fn oot_context_reaches_the_stage_context_only_when_grounded() {
        use crate::settings::ReferenceKind;
        use crate::toolbox::ReferenceRepo;

        fn reference(name: &str, kind: ReferenceKind, revision: &str) -> ReferenceRepo {
            ReferenceRepo {
                name: name.to_string(),
                kind,
                path: std::path::PathBuf::from(format!("/srv/{name}")),
                revision: revision.to_string(),
                commit: "0".repeat(40),
                description: (kind == ReferenceKind::Source)
                    .then(|| "Userspace RDMA stack.".to_string()),
                applies_to: None,
            }
        }

        async fn system_prompt_for(project: ProjectId, references: Vec<ReferenceRepo>) -> String {
            let temp_dir = tempfile::tempdir().unwrap();
            let prompts_dir = temp_dir.path().join("prompts");
            std::fs::create_dir_all(&prompts_dir).unwrap();

            let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None)
                .with_references(references);

            let mut worker = Worker::new(
                std::sync::Arc::new(MockBlockedProvider {
                    attempts: AtomicUsize::new(0),
                }),
                std::sync::Arc::new(tools),
                PromptRegistry::new(prompts_dir),
                WorkerConfig {
                    project,
                    max_input_tokens: 10000,
                    max_interactions: 3,
                    temperature: 0.0,
                    series_range: None,
                    baseline_sha: None,
                    custom_prompt: None,
                    stages: Some(vec!["goal".to_string()]),
                    skip_report: false,
                },
            );

            let res = worker
                .run(
                    serde_json::json!({
                        "id": 1,
                        "patch_index": 1,
                        "patches": [{"diff": "diff --git a/x.c b/x.c\n+int x;"}]
                    }),
                    None,
                )
                .await
                .expect("review runs");
            res.history[0].message.content.clone().unwrap_or_default()
        }

        let grounded = system_prompt_for(
            ProjectId::Linux,
            vec![reference("kernel", ReferenceKind::Kernel, "v6.12")],
        )
        .await;
        for needle in [
            "Read-only Reference Repositories",
            "Out-of-Tree Module",
            "v6.12",
            r#"repo="kernel""#,
            "Fixes:",
        ] {
            assert!(grounded.contains(needle), "missing {needle}: {grounded}");
        }

        let in_tree = system_prompt_for(ProjectId::Linux, vec![]).await;
        assert!(
            !in_tree.contains("Read-only Reference Repositories"),
            "a single-repository review lists no references"
        );
        assert!(
            !in_tree.contains("Out-of-Tree Module"),
            "an in-tree review must not be told to relax in-tree conventions"
        );

        // A userspace tree alone is listed, but does not make the review
        // out-of-tree.
        let rdma_only = system_prompt_for(
            ProjectId::Linux,
            vec![reference(
                "rdma-core",
                ReferenceKind::Source,
                "ionic-rdma-v59",
            )],
        )
        .await;
        assert!(
            rdma_only.contains(r#"repo="rdma-core""#),
            "the reference must be listed: {rdma_only}"
        );
        assert!(
            !rdma_only.contains("Out-of-Tree Module"),
            "only a kernel reference implies an out-of-tree module"
        );

        // The Sashiko prompt has no reference framing. run_worker drops the
        // references for that project before it gets here; even if one slipped
        // through, the Linux-only blocks must not appear. Rendered directly
        // because the Sashiko stages do not survive the blocked mock provider.
        let sashiko = sashiko_system_prompt(true).render_for_log(&LinuxPatchReviewState {
            references: vec![reference("kernel", ReferenceKind::Kernel, "v6.12")],
            ..Default::default()
        });
        assert!(
            !sashiko.contains("Out-of-Tree Module"),
            "only the Linux prompt carries the out-of-tree block"
        );
        assert!(
            !sashiko.contains("Read-only Reference Repositories"),
            "only the Linux prompt lists reference repositories"
        );
    }

    #[tokio::test]
    async fn test_baseline_sha_in_worker_context_when_single_patch() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let provider = std::sync::Arc::new(MockBlockedProvider {
            attempts: AtomicUsize::new(0),
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            project: ProjectId::Linux,
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            baseline_sha: Some("explicit_baseline_sha".to_string()),
            custom_prompt: None,
            stages: Some(vec!["goal".to_string()]),
            skip_report: false,
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;", "commit_id": "target_sha"}]
        });

        let res = worker.run(patchset, None).await;
        assert!(res.is_ok());
        let worker_res = res.unwrap();
        assert!(!worker_res.history.is_empty());
        // System prompt contains the Active Git Metadata:
        let sys_content = worker_res.history[0]
            .message
            .content
            .as_deref()
            .unwrap_or_default();
        assert!(sys_content.contains("Baseline SHA: explicit_baseline_sha"));
        assert!(sys_content.contains("Target Commit SHA: target_sha"));
    }

    #[tokio::test]
    async fn test_multi_patch_worker_context_only_contains_target_patch_diff() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let provider = std::sync::Arc::new(MockBlockedProvider {
            attempts: AtomicUsize::new(0),
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            project: ProjectId::Linux,
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: Some("base_sha..sha2".to_string()),
            baseline_sha: Some("base_sha".to_string()),
            custom_prompt: None,
            stages: Some(vec!["goal".to_string()]),
            skip_report: false,
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 100,
            "patch_index": 1,
            "patches": [
                {
                    "index": 1,
                    "subject": "Patch 1 Subject",
                    "diff": "diff --git a/file1.c b/file1.c\n+int patch1_unique_symbol;",
                    "commit_id": "sha1"
                },
                {
                    "index": 2,
                    "subject": "Patch 2 Subject",
                    "diff": "diff --git a/file2.c b/file2.c\n+int patch2_unique_symbol;",
                    "commit_id": "sha2"
                }
            ]
        });

        let res = worker.run(patchset, None).await;
        assert!(res.is_ok());
        let worker_res = res.unwrap();
        assert!(!worker_res.history.is_empty());
        let sys_content = worker_res.history[0]
            .message
            .content
            .as_deref()
            .unwrap_or_default();
        assert!(sys_content.contains("Target Commit SHA: sha1"));
        assert!(sys_content.contains("patch1_unique_symbol"));
        assert!(!sys_content.contains("patch2_unique_symbol"));
    }

    struct MockMultiStageSeriesProvider;

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockMultiStageSeriesProvider {
        async fn generate_content(
            &self,
            request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let last_user = request
                .messages
                .iter()
                .rfind(|m| m.role == crate::ai::AiRole::User)
                .and_then(|m| m.content.as_deref())
                .unwrap_or_default();

            // Dispatch on the heading each stage's instruction opens with.
            // Analysis stages and deduplication return both lists, conflict
            // resolution only concerns, verification findings.
            let content = if last_user.contains("# Analyze commit main goal")
                || last_user.contains("# Deduplication and Consolidation")
            {
                r#"{"concerns": [{"type": "Bug", "description": "some issue", "reasoning": "reason", "preexisting": false, "locations": []}], "dismissed_concerns": []}"#
            } else if last_user.contains("# Concern/dismissed-concern conflict resolution") {
                r#"{"concerns": [{"type": "Bug", "description": "some issue", "reasoning": "reason", "preexisting": false, "locations": []}]}"#
            } else if last_user.contains("# Verification and severity estimation") {
                r#"{"findings": []}"#
            } else {
                r#"{"concerns": [], "dismissed_concerns": []}"#
            };

            Ok(crate::ai::AiResponse {
                content: Some(content.to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_verification_log_history_contains_follow_up_series_context() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let provider = std::sync::Arc::new(MockMultiStageSeriesProvider);
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            project: ProjectId::Linux,
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: Some("base_sha..sha2".to_string()),
            baseline_sha: Some("base_sha".to_string()),
            custom_prompt: None,
            stages: Some(vec!["goal".to_string()]),
            skip_report: false,
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 200,
            "patch_index": 1,
            "patches": [
                {
                    "index": 1,
                    "subject": "Patch 1 Subject",
                    "diff": "diff --git a/file1.c b/file1.c\n+int patch1;",
                    "commit_id": "sha1"
                },
                {
                    "index": 2,
                    "subject": "Patch 2 Subject",
                    "diff": "diff --git a/file2.c b/file2.c\n+int patch2;",
                    "commit_id": "sha2"
                }
            ]
        });

        let res = worker.run(patchset, None).await;
        assert!(res.is_ok());
        let worker_res = res.unwrap();
        assert!(!worker_res.history.is_empty());

        let verification_user_msg = worker_res
            .history
            .iter()
            .map(|e| &e.message)
            .find(|m| {
                m.role == crate::ai::AiRole::User
                    && m.content
                        .as_deref()
                        .unwrap_or_default()
                        .contains("# Verification and severity estimation")
            })
            .expect("verification user message should be in history");

        let content = verification_user_msg.content.as_deref().unwrap();
        assert!(content.contains("=== Follow-Up Patches in Series ==="));
        assert!(content.contains("Series End Commit (Final State): sha2"));
        assert!(content.contains("- [Patch 2 of 2] (commit sha2): Patch 2 Subject"));
        assert!(content.contains("SERIES VERIFICATION DIRECTIVE:"));
    }

    #[test]
    fn test_build_sashiko_series_context_includes_preceding_and_subsequent_patches() {
        let patchset_first = serde_json::json!({
            "id": 300,
            "patch_index": 1,
            "patches": [
                {
                    "index": 1,
                    "subject": "auth: add max_bug_access claim",
                    "commit_id": "sha1"
                },
                {
                    "index": 2,
                    "subject": "api: enforce max_bug_access on token minting",
                    "commit_id": "sha2"
                },
                {
                    "index": 3,
                    "subject": "docs: add design document for agent bug access",
                    "commit_id": "sha3"
                }
            ]
        });

        let ctx_first =
            build_sashiko_series_context(Some("base..sha3"), &patchset_first, "sha1").unwrap();
        assert!(ctx_first.contains("Current Patch Under Review: [Patch 1 of 3]"));
        assert!(ctx_first.contains("Series End Commit (Final State): sha3"));
        assert!(!ctx_first.contains("Preceding patches in this series"));
        assert!(ctx_first.contains("Subsequent patches in this series:"));
        assert!(ctx_first.contains("- [Patch 2 of 3] (commit sha2): api: enforce max_bug_access"));
        assert!(ctx_first.contains("- [Patch 3 of 3] (commit sha3): docs: add design document"));

        let patchset_last = serde_json::json!({
            "id": 300,
            "patch_index": 3,
            "patches": patchset_first["patches"].clone()
        });

        // Even when series_range is None on the final patch, preceding patches and end_sha are provided
        let ctx_last = build_sashiko_series_context(None, &patchset_last, "sha3").unwrap();
        assert!(ctx_last.contains("Current Patch Under Review: [Patch 3 of 3]"));
        assert!(ctx_last.contains("Series End Commit (Final State): sha3"));
        assert!(ctx_last.contains("Preceding patches in this series"));
        assert!(ctx_last.contains("- [Patch 1 of 3] (commit sha1): auth: add max_bug_access"));
        assert!(ctx_last.contains("- [Patch 2 of 3] (commit sha2): api: enforce max_bug_access"));
        assert!(!ctx_last.contains("Subsequent patches in this series:"));
    }

    #[tokio::test]
    async fn test_skip_report_skips_report_and_summary_stages() {
        struct MockFindingWithoutReportProvider;

        #[async_trait::async_trait]
        impl crate::ai::AiProvider for MockFindingWithoutReportProvider {
            async fn generate_content(
                &self,
                request: crate::ai::AiRequest,
            ) -> anyhow::Result<crate::ai::AiResponse> {
                let last_user = request
                    .messages
                    .iter()
                    .rfind(|m| m.role == crate::ai::AiRole::User)
                    .and_then(|m| m.content.as_deref())
                    .unwrap_or_default();

                if last_user.contains("Return raw text output, not JSON.")
                    || last_user.contains("# Generate plain-text inline review report")
                    || last_user.contains("# Summarize the proposed change")
                {
                    anyhow::bail!("report or summary stage should have been skipped");
                }

                let content = if last_user.contains("# Analyze commit main goal")
                    || last_user.contains("# Deduplication and Consolidation")
                    || last_user.contains("# Deduplicate concerns and dismissed concerns")
                {
                    r#"{"concerns": [{"type": "Bug", "description": "some issue", "reasoning": "reason", "preexisting": false, "locations": []}], "dismissed_concerns": []}"#
                } else if last_user.contains("# Concern/dismissed-concern conflict resolution")
                    || last_user
                        .contains("# Resolve conflicts between concerns and dismissed concerns")
                {
                    r#"{"concerns": [{"type": "Bug", "description": "some issue", "reasoning": "reason", "preexisting": false, "locations": []}]}"#
                } else if last_user.contains("# Verification and severity estimation")
                    || last_user.contains("# Verify remaining concerns and calibrate severity")
                {
                    r#"{"findings": [{"problem": "some issue", "severity": "High", "severity_explanation": "bad", "preexisting": false, "locations": []}]}"#
                } else {
                    r#"{"concerns": [], "dismissed_concerns": []}"#
                };

                Ok(crate::ai::AiResponse {
                    content: Some(content.to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    usage: None,
                    truncated: false,
                })
            }

            fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
                crate::ai::ProviderCapabilities {
                    model_name: "mock".to_string(),
                    context_window_size: 1000,
                }
            }
        }

        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        for project in [ProjectId::Linux, ProjectId::Sashiko] {
            let provider = std::sync::Arc::new(MockFindingWithoutReportProvider);
            let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
            let prompts = PromptRegistry::new(prompts_dir.clone());
            let config = WorkerConfig {
                project,
                max_input_tokens: 10000,
                max_interactions: 3,
                temperature: 0.0,
                series_range: None,
                baseline_sha: Some("base_sha".to_string()),
                custom_prompt: None,
                stages: Some(vec!["goal".to_string()]),
                skip_report: true,
            };
            let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

            let patchset = serde_json::json!({
                "id": 1,
                "patch_index": 1,
                "patches": [{"index": 1, "diff": "diff --git a/foo.c b/foo.c\n+int x;", "commit_id": "sha1"}]
            });

            let res = worker
                .run(patchset, None)
                .await
                .expect("worker should succeed");
            let output = res.output.expect("worker output");
            assert_eq!(output["findings"].as_array().unwrap().len(), 1);
            assert_eq!(output["review_inline"], "");
            assert_eq!(output["summary"], "");
        }
    }

    /// Every message the finished log will hold is streamed, once, starting
    /// with the system message, which belongs to no stage.
    #[tokio::test]
    async fn test_the_live_log_streams_the_whole_finished_log() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        // No stage fails, so every streamed stage also reaches the history.
        let provider = std::sync::Arc::new(MockProviderFailsOneStage {
            failing_stage: "no-such-stage",
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let config = WorkerConfig {
            project: ProjectId::Linux,
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            baseline_sha: None,
            custom_prompt: None,
            stages: Some(vec!["goal".to_string(), "implementation".to_string()]),
            skip_report: false,
        };
        let mut worker = Worker::new(
            provider,
            std::sync::Arc::new(tools),
            PromptRegistry::new(prompts_dir),
            config,
        );
        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
        });

        let streamed: std::sync::Mutex<Vec<(Option<String>, AiMessage)>> =
            std::sync::Mutex::new(Vec::new());
        let record = |e: WorkerProgressEvent| {
            if let WorkerProgressEvent::LogEntry { stage, message } = e {
                streamed.lock().unwrap().push((stage, message));
            }
        };
        let res = worker.run(patchset, Some(&record)).await.unwrap();
        let streamed = streamed.into_inner().unwrap();

        assert_eq!(streamed[0].0, None);
        assert_eq!(streamed[0].1.role, crate::ai::AiRole::System);
        assert_eq!(
            streamed.iter().filter(|(stage, _)| stage.is_none()).count(),
            1,
            "only the system message is outside a stage"
        );
        // Stages run concurrently, so the stream interleaves what the finished
        // log groups by stage. The messages are the same either way, and so is
        // the stage each belongs to: the finished log marks its entries with
        // their stage as the stream does, so the two logs have one shape.
        assert_eq!(streamed.len(), res.history.len());
        for entry in &res.history {
            let live = live_log_message(&entry.message);
            assert!(
                streamed
                    .iter()
                    .any(|(stage, m)| *m == live && *stage == entry.stage),
                "not streamed with its stage: {entry:?}"
            );
        }
        assert_eq!(res.history[0].stage, None, "the system prompt has no stage");
        assert!(
            res.history[1..].iter().all(|e| e.stage.is_some()),
            "every other entry names its stage"
        );
        // The consolidation stages run outside the concurrent block and are
        // streamed like any other.
        assert!(
            streamed
                .iter()
                .any(|(stage, _)| stage.as_deref() == Some("goal"))
        );
    }

    /// A single tool result can be a whole kernel file. Clipping it bought
    /// little, because the daemon proxies every request and the same result
    /// already crosses the pipe as part of the history. What it cost was a
    /// live view that disagreed with the finished one.
    #[test]
    fn streamed_log_entries_carry_their_content_whole() {
        let huge = "x".repeat(4_000_000);
        let msg = AiMessage {
            role: crate::ai::AiRole::Tool,
            content: Some(huge.clone()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        };
        let message = live_log_message(&msg);
        assert_eq!(message.role, crate::ai::AiRole::Tool);
        assert_eq!(
            message.content.unwrap_or_default().len(),
            huge.len(),
            "the streamed entry must hold what the finished log would hold"
        );
    }

    /// The live log must hold what the finished log will hold, so the page's
    /// redraw at the end changes nothing a reader is looking at. The finished
    /// log is the history serialised and scrubbed of thought signatures; a
    /// streamed message read back from the wire must be that message exactly,
    /// tool-result envelopes and long tool-call arguments included.
    #[test]
    fn streamed_messages_are_the_finished_logs_messages() {
        let call = AiMessage {
            role: crate::ai::AiRole::Assistant,
            content: None,
            thought: Some("thinking".to_string()),
            thought_signature: Some("opaque".to_string()),
            tool_calls: Some(vec![crate::ai::ToolCall {
                id: "c0".to_string(),
                function_name: "git_show".to_string(),
                arguments: json!({
                    "path": "x".repeat(400),
                    "unset": null,
                    "nested": [{"thought_signature": "opaque", "y": "z"}],
                }),
                thought_signature: Some("opaque".to_string()),
            }]),
            tool_call_id: None,
        };
        let result = AiMessage {
            role: crate::ai::AiRole::Tool,
            content: Some(
                json!({
                    "content": "diff --git a/x.c b/x.c\n+\tstrcpy(buf, \"name\");\n",
                    "truncated": true,
                })
                .to_string(),
            ),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: Some("c0".to_string()),
        };

        for message in [call, result] {
            let mut finished = serde_json::to_value(&message).unwrap();
            crate::ai::scrub_thought_signatures(&mut finished);
            let finished: AiMessage = serde_json::from_value(finished).unwrap();

            let wire = serde_json::to_string(&live_log_message(&message)).unwrap();
            let live: AiMessage = serde_json::from_str(&wire).unwrap();
            assert_eq!(live, finished, "{wire}");
        }
    }

    /// Thought signatures are opaque provider state, and the finished log has
    /// them scrubbed; the live log must not be the one place they show.
    #[test]
    fn streamed_messages_carry_no_thought_signature() {
        let msg = AiMessage {
            role: crate::ai::AiRole::Assistant,
            content: Some("x".to_string()),
            thought: Some("thinking".to_string()),
            thought_signature: Some("opaque".to_string()),
            tool_calls: Some(vec![crate::ai::ToolCall {
                id: "c0".to_string(),
                function_name: "git_ls".to_string(),
                arguments: json!({ "path": "." }),
                thought_signature: Some("opaque".to_string()),
            }]),
            tool_call_id: None,
        };
        let live = live_log_message(&msg);
        assert_eq!(live.thought.as_deref(), Some("thinking"));
        assert_eq!(live.thought_signature, None);
        assert_eq!(live.tool_calls.unwrap()[0].thought_signature, None);
        let wire = serde_json::to_string(&live_log_message(&msg)).unwrap();
        assert!(!wire.contains("opaque"), "{wire}");
    }
}
