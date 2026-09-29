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
    // Truncation is not here. It was declared as a variant of this enum and
    // never constructed, because only the session loop can see a response come
    // back half-written and only it can do anything about it -- and this layer
    // sits above that one. It lives in `ai` as `OutputTruncated`, where it is
    // raised and recovered from.
}

impl ClassifyAiError for ReviewError {
    fn ai_error_class(&self) -> AiErrorClass {
        match self {
            ReviewError::LimitExceeded => AiErrorClass::Fatal,
            ReviewError::BudgetExceeded(_) => AiErrorClass::Fatal,
            ReviewError::FormatRejection(_) => AiErrorClass::Fatal,
        }
    }
}

use crate::worker::kernel_workflow::{
    KernelReviewState, build_kernel_review_workflow_with_options, kernel_system_prompt,
};
use crate::workflow::{WorkflowEngine, WorkflowEnv, WorkflowEvent};
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
    /// The series cover letter, when the submission had one. Context about
    /// intent, not something to review.
    #[serde(default)]
    pub cover_letter: Option<String>,
    pub patches: Vec<PatchInput>,
    /// Findings from reviews of earlier revisions of the same pull request, as
    /// returned by `Database::get_prior_revision_findings`. Every patch gets
    /// the whole list; `build_prior_review_context` picks out its own part.
    #[serde(default)]
    pub prior_revisions: Option<Value>,
}

pub struct WorkerConfig {
    pub max_input_tokens: usize,
    pub max_interactions: usize,
    pub temperature: f32,
    pub custom_prompt: Option<String>,
    pub series_range: Option<String>,
    pub baseline_sha: Option<String>,
    pub stages: Option<Vec<String>>,
}

/// Token budget for the series cover letter in stage context.
///
/// Generous enough for a real kernel cover letter, bounded so it cannot crowd
/// out the diff that is actually under review.
pub(crate) const COVER_LETTER_TOKENS: usize = 2000;

/// Token budget for the findings of earlier revisions in stage context.
///
/// Each finding is a few lines -- headline, location, consequence -- so this
/// holds a hundred or so: every finding of a pull request that has been round
/// the loop several times. Whole findings are dropped when it runs out, those
/// about other patches before those about this one, and the oldest first
/// within each.
pub(crate) const PRIOR_REVIEWS_TOKENS: usize = 12000;

/// Token budget for the previous revision's diff, shown so a claim that a fix
/// caused a new problem can be checked against what the fix actually changed.
pub(crate) const PRIOR_REVISION_DIFF_TOKENS: usize = 3000;

/// Test-only accessor for [`log_entry_event`].
#[cfg(test)]
pub fn log_entry_event_for_test(stage: String, msg: &crate::ai::AiMessage) -> WorkerProgressEvent {
    log_entry_event(stage, msg)
}

/// Bounds tool-call arguments for the wire while keeping their shape.
///
/// Bounded per argument rather than as a whole: a long `pattern` must not push
/// the `revision` that gives it meaning off the end. The object structure
/// survives, because the log view renders arguments as fields rather than as a
/// sentence -- flattening them to text here is what made the live log look
/// nothing like the finished one.
fn compact_args(args: &Value) -> Value {
    const MAX_ARG_CHARS: usize = 120;

    fn clip(value: &Value) -> Value {
        match value {
            Value::String(s) if s.chars().count() > MAX_ARG_CHARS => {
                let head: String = s.chars().take(MAX_ARG_CHARS).collect();
                Value::String(format!("{head}..."))
            }
            Value::Array(items) => Value::Array(items.iter().map(clip).collect()),
            Value::Object(fields) => {
                Value::Object(fields.iter().map(|(k, v)| (k.clone(), clip(v))).collect())
            }
            other => other.clone(),
        }
    }

    let Some(obj) = args.as_object() else {
        return clip(args);
    };

    Value::Object(
        obj.iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (k.clone(), clip(v)))
            .collect(),
    )
}

/// Unwraps a tool result envelope for display.
///
/// Tool results reach the model as a JSON envelope serialised with
/// `to_string()`, so every quote and newline in a diff arrives
/// backslash-escaped. That is right for the model, which needs the envelope to
/// tell content from metadata, and unreadable for a person, which is all this
/// preview is for.
///
/// Three envelope shapes are in use and all of them need unwrapping:
/// `{"content": ...}` from most tools, `{"results": [...]}` from
/// `git_read_files`, and `{"entries": [...]}` from `git_ls`. Anything else is
/// passed through untouched rather than guessed at.
fn unwrap_tool_envelope(body: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let obj = parsed.as_object()?;

    let rendered = if let Some(content) = obj.get("content").and_then(Value::as_str) {
        content.to_string()
    } else if let Some(results) = obj.get("results").and_then(Value::as_array) {
        // One entry per file requested, each with its own content or error.
        // Headed by path: a run of files with no separator reads as one file
        // whose contents make no sense together.
        results
            .iter()
            .map(|r| {
                let path = r["path"].as_str().unwrap_or("(unknown path)");
                match (r["error"].as_str(), r["content"].as_str()) {
                    (Some(error), _) => format!("--- {path}: {error}"),
                    (None, Some(content)) => format!("--- {path}\n{content}"),
                    (None, None) => format!("--- {path}"),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else if let Some(entries) = obj.get("entries").and_then(Value::as_array) {
        entries
            .iter()
            .map(|e| match (e["name"].as_str(), e["type"].as_str()) {
                (Some(name), Some("tree")) => format!("{name}/"),
                (Some(name), _) => name.to_string(),
                (None, _) => e.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        return None;
    };

    // Truncation is the one flag worth keeping: without it a clipped result
    // reads as a complete one.
    Some(
        if obj.get("truncated").and_then(Value::as_bool) == Some(true) {
            format!("{rendered}\n[truncated]")
        } else {
            rendered
        },
    )
}

/// Builds a streamable log entry from a message, truncated for the wire.
fn log_entry_event(stage: String, msg: &crate::ai::AiMessage) -> WorkerProgressEvent {
    let mut body = msg.content.clone().unwrap_or_default();

    if msg.role == crate::ai::AiRole::Tool
        && let Some(unwrapped) = unwrap_tool_envelope(&body)
    {
        body = unwrapped;
    }

    WorkerProgressEvent::LogEntry {
        stage,
        message: AiMessage {
            role: msg.role.clone(),
            content: Some(body),
            thought: msg.thought.clone(),
            thought_signature: None,
            tool_calls: msg.tool_calls.as_ref().map(|calls| {
                calls
                    .iter()
                    .map(|c| crate::ai::ToolCall {
                        id: c.id.clone(),
                        function_name: c.function_name.clone(),
                        arguments: compact_args(&c.arguments),
                        thought_signature: None,
                    })
                    .collect()
            }),
            tool_call_id: msg.tool_call_id.clone(),
        },
    }
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
    StageFinished {
        stage: String,
    },
    /// What a stage produced, sent as soon as it finishes.
    ///
    /// Reported for every stage, including the pre-screen and planning that
    /// the progress events leave out: their answers -- the guides selected and
    /// the stages planned -- are part of what the later stages were given, so
    /// a record of the review's progress without them is incomplete.
    StageOutput {
        stage: String,
        output: Value,
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
    /// One message joining the conversation log, streamed so a running review
    /// can be read before it finishes. `content` is already truncated.
    /// One message joining the conversation log, streamed so a running review
    /// can be read before it finishes.
    ///
    /// Carries the message itself rather than a flattened rendering of it. The
    /// finished log stores `AiMessage`s, so sharing the type is what keeps the
    /// live view and the finished view from drifting apart -- they drifted
    /// before precisely because nothing tied the two shapes together.
    LogEntry {
        stage: String,
        message: AiMessage,
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
    /// last reported turn on display — claiming to still be running — for as
    /// long as the rest of the review took.
    StageFailed {
        stage: String,
        reason: String,
        cancelled: bool,
    },
}

pub struct WorkerResult {
    pub output: Option<Value>,
    pub error: Option<String>,
    pub input_context: String,
    pub history: Vec<AiMessage>,
    pub history_before_pruning: Vec<AiMessage>,
    pub history_after_pruning: Vec<AiMessage>,
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
    provider: Arc<dyn AiProvider>,
    tools: Arc<ToolBox>,
    prompts: PromptRegistry,
    global_history: Vec<AiMessage>,
    max_interactions: usize,
    temperature: f32,
    series_range: Option<String>,
    baseline_sha: Option<String>,
    context_tag: Option<String>,
    stages: Option<Vec<String>>,
    custom_prompt: Option<String>,
}

impl Worker {
    pub fn new(
        provider: Arc<dyn AiProvider>,
        tools: Arc<ToolBox>,
        prompts: PromptRegistry,
        config: WorkerConfig,
    ) -> Self {
        Self {
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
        }
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

        let follow_up_series_context = build_follow_up_series_context(
            self.series_range.as_deref(),
            &patchset,
            &target_commit_sha,
        );

        let prior_review_context = build_prior_review_context(&patchset);

        let mut state = KernelReviewState {
            ps_id,
            p_id,
            target_commit_sha,
            baseline_sha,
            target_commit_diff,
            target_commit_diff_only,
            prefetched_context,
            prefetch_failed,
            cover_letter: patchset["cover_letter"].as_str().map(str::to_string),
            reference_revision: self.tools.reference_revision().map(str::to_string),
            series_range: self.series_range.clone(),
            follow_up_series_context,
            prior_review_context,
            selected_guides: Vec::new(),
            manual_stages: self.stages.clone(),
            custom_prompt: self.custom_prompt.clone(),
            planned_stages: Vec::new(),
            all_concerns: Vec::new(),
            all_dismissed_concerns: Vec::new(),
            deduplicated_concerns: Vec::new(),
            deduplicated_dismissed_concerns: Vec::new(),
            conflict_resolved_concerns: Vec::new(),
            findings: Vec::new(),
            review_inline: String::new(),
            fixes: String::new(),
        };

        if self.global_history.is_empty() {
            let sys_template = kernel_system_prompt(true);
            let rendered_sys = sys_template.render_for_log(&state);
            self.global_history.push(AiMessage {
                role: crate::ai::AiRole::System,
                content: Some(rendered_sys),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }

        let workflow =
            build_kernel_review_workflow_with_options(self.max_interactions, self.temperature);
        let env = WorkflowEnv {
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            base_dir: &self.prompts.base_dir,
            context_tag: self.context_tag.clone(),
        };

        let event_cb = move |event: WorkflowEvent| {
            if let Some(progress_cb) = progress {
                match event {
                    WorkflowEvent::StageStarted { stage_name } => {
                        if stage_name == "pre-screen" {
                            progress_cb(WorkerProgressEvent::PreScreenStarted);
                        } else if stage_name == "planning" {
                            progress_cb(WorkerProgressEvent::PlanningStarted);
                        } else if is_counted_stage(stage_name) {
                            progress_cb(WorkerProgressEvent::StageStarted {
                                stage: stage_name.to_string(),
                            });
                        }
                    }
                    WorkflowEvent::ParallelResolved { stage_names } => {
                        progress_cb(WorkerProgressEvent::ReviewStarted {
                            planned_stages: planned_stages_from(&stage_names),
                        });
                    }
                    WorkflowEvent::StageFinished {
                        stage_name, output, ..
                    } => {
                        progress_cb(WorkerProgressEvent::StageOutput {
                            stage: stage_name.to_string(),
                            output,
                        });
                        if is_counted_stage(stage_name) {
                            progress_cb(WorkerProgressEvent::StageFinished {
                                stage: stage_name.to_string(),
                            });
                        }
                    }
                    WorkflowEvent::StageTurn {
                        stage_name,
                        turn,
                        max_turns,
                    } => {
                        if is_counted_stage(stage_name) {
                            progress_cb(WorkerProgressEvent::StageTurn {
                                stage: stage_name.to_string(),
                                turn,
                                max_turns,
                            });
                        }
                    }
                    WorkflowEvent::StageTools {
                        stage_name,
                        tools,
                        turn,
                        max_turns,
                    } => {
                        if is_counted_stage(stage_name) {
                            progress_cb(WorkerProgressEvent::StageTools {
                                stage: stage_name.to_string(),
                                tools,
                                turn,
                                max_turns,
                            });
                        }
                    }
                    WorkflowEvent::StageBackoff {
                        stage_name,
                        retry_in_seconds,
                        turn,
                        max_turns,
                    } => {
                        if is_counted_stage(stage_name) {
                            progress_cb(WorkerProgressEvent::StageBackoff {
                                stage: stage_name.to_string(),
                                retry_in_seconds,
                                turn,
                                max_turns,
                            });
                        }
                    }
                    WorkflowEvent::StageMessage {
                        stage_name,
                        message,
                    } => {
                        if is_counted_stage(stage_name) {
                            progress_cb(log_entry_event(stage_name.to_string(), &message));
                        }
                    }
                    WorkflowEvent::StageFailed {
                        stage_name,
                        reason,
                        cancelled,
                    } => {
                        if is_counted_stage(stage_name) {
                            progress_cb(WorkerProgressEvent::StageFailed {
                                stage: stage_name.to_string(),
                                reason,
                                cancelled,
                            });
                        }
                    }
                    _ => {}
                }
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

        let review_inline = if state.review_inline.is_empty() {
            "No issues found.".to_string()
        } else {
            state.review_inline
        };

        // Stages that did not complete, so the caller can report which parts of
        // the review are missing instead of presenting partial coverage as full.
        let stage_failures: Vec<serde_json::Value> = outcome
            .stage_failures
            .iter()
            .map(|f| {
                json!({
                    "stage": f.stage_name,
                    "reason": f.reason,
                    "cancelled": f.cancelled,
                })
            })
            .collect();

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
            "findings": state.findings,
            "dismissed_concerns": dismissed_concerns,
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

/// Whether a stage counts towards the progress display.
///
/// Exactly the set `planned_stages_from()` totals, both reading the stage
/// tables: the analysis stages and the consolidation stages that follow them.
/// A stage counted in the total has to report finishing, or the bar stops
/// short of the work it did.
fn is_counted_stage(name: &str) -> bool {
    crate::worker::kernel_workflow::stage_short_label(name).is_some()
}

/// The stages a review will run: the analysis stages the fan-out resolved, then
/// the four that always follow them. Nothing resolved means nothing planned,
/// not a bare tail.
fn planned_stages_from(stage_names: &[&'static str]) -> Vec<String> {
    let mut planned: Vec<String> = stage_names
        .iter()
        .filter(|n| crate::worker::kernel_workflow::analysis_stage_by_name(n).is_some())
        .map(|n| n.to_string())
        .collect();
    if !planned.is_empty() {
        planned.extend(
            crate::worker::kernel_workflow::CONSOLIDATION_STAGES
                .iter()
                .map(|s| s.name.to_string()),
        );
    }
    planned
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

/// What the reviews of earlier revisions of this pull request reported, or
/// `None` when there is nothing to say.
///
/// Every earlier finding is shown, not only those on this patch: code moves
/// between patches as a series is reworked, and a finding whose code moved
/// here would otherwise be lost exactly when it matters. Each is summarised by
/// its headline, where it was, and its consequence. The full argument is left
/// out; it described code that has since changed, and whether a fix caused a
/// new problem is settled by comparing the code, not by re-reading it.
///
/// Findings about this patch come first in each revision and are the last to
/// be dropped for budget. A finding is about this patch when it was reported
/// on a patch with the same subject -- which survives a rebase -- or when it
/// cites a file this patch touches or a symbol its diff names, which survive a
/// reworded or split commit.
///
/// The block is labelled as a record rather than as guidance. It was written by
/// a model about code that has since changed, so it can say what to look at
/// again but proves nothing about the current code.
pub fn build_prior_review_context(patchset: &Value) -> Option<String> {
    let revisions = patchset["prior_revisions"].as_array()?;
    let current_idx = patchset["patch_index"].as_i64().unwrap_or(1);
    let target = patchset["patches"]
        .as_array()?
        .iter()
        .find(|p| p["index"].as_i64() == Some(current_idx))?;
    let target_subject = normalized_subject(target["subject"].as_str().unwrap_or(""));
    // The forge's commit id; `commit_id` is the local re-application of it.
    let target_commit = target["message_id"].as_str().unwrap_or("");
    let target_diff = target["diff"].as_str().unwrap_or("");
    let target_files = crate::baseline::extract_files_from_diff(target_diff);

    struct Patch<'a> {
        index: i64,
        subject: &'a str,
        commit: &'a str,
        diff: Option<&'a str>,
        same_patch: bool,
        // (finding, about this patch, rendered, kept within budget)
        findings: Vec<(&'a Value, bool, String, bool)>,
    }
    struct Revision<'a> {
        label: &'a str,
        newest: bool,
        patches: Vec<Patch<'a>>,
    }

    let mut kept: Vec<Revision> = Vec::new();
    for (i, revision) in revisions.iter().enumerate() {
        let mut patches = Vec::new();
        for patch in revision["patches"].as_array().into_iter().flatten() {
            let subject = patch["subject"].as_str().unwrap_or("");
            let same_patch =
                !target_subject.is_empty() && normalized_subject(subject) == target_subject;
            let findings: Vec<_> = patch["findings"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| {
                    let about = same_patch
                        || finding_touches_any(f, &target_files)
                        || finding_names_symbol_in(f, target_diff);
                    (
                        f,
                        about,
                        render_prior_finding(f, about && !same_patch),
                        false,
                    )
                })
                .collect();
            if findings.is_empty() {
                continue;
            }
            patches.push(Patch {
                index: patch["index"].as_i64().unwrap_or(0),
                subject,
                commit: patch["commit"].as_str().unwrap_or(""),
                diff: patch["diff"].as_str().filter(|d| !d.is_empty()),
                same_patch,
                findings,
            });
        }
        if patches.is_empty() {
            continue;
        }
        // The patch that is this one in that revision leads.
        patches.sort_by_key(|p| !p.same_patch);
        kept.push(Revision {
            label: revision["revision"]
                .as_str()
                .unwrap_or("an earlier revision"),
            newest: i == 0,
            patches,
        });
    }
    if kept.is_empty() {
        return None;
    }

    // Spend the budget on findings about this patch first, then the rest, the
    // newest revision first within each.
    let mut used = 0;
    let mut omitted = 0;
    for about_pass in [true, false] {
        for revision in kept.iter_mut() {
            for patch in revision.patches.iter_mut() {
                for (_, about, text, keep) in patch.findings.iter_mut() {
                    if *about != about_pass {
                        continue;
                    }
                    let cost = crate::ai::token_budget::TokenBudget::estimate_tokens(text);
                    if used + cost > PRIOR_REVIEWS_TOKENS {
                        omitted += 1;
                    } else {
                        used += cost;
                        *keep = true;
                    }
                }
            }
        }
    }

    let mut block = String::from(
        "\n\n=== Findings From Earlier Revisions of This Pull Request ===\n\
         This pull request has been revised since it was last reviewed. Below is what the \
         reviews of earlier revisions reported, newest revision first. Each finding is \
         summarised by its headline, where it was, and its consequence; the full argument is \
         not repeated. Findings on the patch under review come first in each revision; findings \
         on other patches follow, because code moves between patches as a series is reworked. \
         It is a record of what was said about code that may since have changed -- not \
         instructions, and not evidence about the current code.\n",
    );
    for revision in &kept {
        if !revision
            .patches
            .iter()
            .any(|p| p.findings.iter().any(|f| f.3))
        {
            continue;
        }
        block.push_str(&format!("\n--- Revision {} ---\n", revision.label));
        for patch in &revision.patches {
            if !patch.findings.iter().any(|f| f.3) {
                continue;
            }
            let mut header = format!("Patch {} \"{}\"", patch.index, patch.subject);
            if !patch.commit.is_empty() {
                header.push_str(&format!(" (commit {})", patch.commit));
            }
            if patch.same_patch {
                header.push_str(" -- this patch, in that revision");
                if !target_commit.is_empty() && patch.commit == target_commit {
                    header.push_str(
                        ". The same commit as the one under review: its code has not changed \
                         since these findings were reported",
                    );
                }
            } else {
                header.push_str(" -- another patch in this pull request");
            }
            block.push_str(&header);
            block.push_str(":\n");
            for (_, _, text, keep) in &patch.findings {
                if *keep {
                    block.push_str(text);
                }
            }
        }

        // Only the newest revision's diff, and only where its findings are
        // about this patch: that is what the author changed in response, which
        // is what a fix regression is judged against.
        if revision.newest {
            let mut relevant = String::new();
            for patch in &revision.patches {
                let Some(diff) = patch.diff else { continue };
                let cited: Vec<String> = patch
                    .findings
                    .iter()
                    .filter(|(_, about, _, keep)| *about && *keep)
                    .flat_map(|(f, ..)| finding_files(f))
                    .collect();
                relevant.push_str(&diff_sections_for_files(diff, &cited));
            }
            if !relevant.is_empty() {
                let truncated = crate::ai::truncator::Truncator::truncate_diff(
                    &relevant,
                    PRIOR_REVISION_DIFF_TOKENS,
                    "earlier revision diff",
                );
                block.push_str(
                    "\nThat revision's version of the code these findings were about, limited to \
                     the files they cite. Compare it with the patch under review to see what \
                     changed in response:\n",
                );
                block.push_str(&truncated.content);
                if !truncated.content.ends_with('\n') {
                    block.push('\n');
                }
            }
        }
    }
    if omitted > 0 {
        block.push_str(&format!(
            "\n({} older finding(s) omitted to fit the context budget.)\n",
            omitted
        ));
    }
    block.push_str("===========================");

    // Model-written text is substituted into a template; a stray `{{name}}` in
    // it must not be read as a placeholder by a variable filled in after it.
    Some(block.replace("{{", "{ {"))
}

fn normalized_subject(subject: &str) -> String {
    crate::patch::clean_subject(subject).trim().to_lowercase()
}

/// The files a finding's `locations` name, as written.
fn finding_files(finding: &Value) -> Vec<String> {
    finding["locations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["file"].as_str())
        .map(|f| f.trim().trim_start_matches("./").to_string())
        .filter(|f| !f.is_empty())
        .collect()
}

/// Whether two spellings of a path name the same file. A model sometimes
/// shortens a path or roots it differently, so a match on whole trailing
/// components counts.
fn same_file(a: &str, b: &str) -> bool {
    let a = a.trim_start_matches('/');
    let b = b.trim_start_matches('/');
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

fn finding_touches_any(finding: &Value, files: &[String]) -> bool {
    finding_files(finding)
        .iter()
        .any(|f| files.iter().any(|t| same_file(f, t)))
}

/// The per-file sections of a unified diff for the given files, in the order
/// the diff has them.
fn diff_sections_for_files(diff: &str, files: &[String]) -> String {
    let mut out = String::new();
    let mut keep = false;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("diff --git a/") {
            let file = path.split_once(' ').map_or(path, |(a, _)| a);
            keep = files.iter().any(|f| same_file(f, file));
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Whether a symbol a finding cites appears, as a whole identifier, anywhere in
/// the diff. Follows code that moved to another file along with its commit's
/// title changing, which neither of the other matches can.
///
/// Only plain identifiers of a few characters are tried: a location's symbol
/// is sometimes an expression or a phrase, and a short one would match noise.
fn finding_names_symbol_in(finding: &Value, diff: &str) -> bool {
    if diff.is_empty() {
        return false;
    }
    finding["locations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["function_or_symbol"].as_str())
        .map(|s| s.trim().trim_end_matches("()"))
        .filter(|s| s.len() >= 4 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .any(|sym| contains_identifier(diff, sym))
}

fn contains_identifier(haystack: &str, ident: &str) -> bool {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    haystack.match_indices(ident).any(|(at, _)| {
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + ident.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

/// At most `max` characters of `text`, cut back to a word boundary, marked when
/// cut. The model is shown a summary, so a clean edge matters more than the
/// last few characters.
fn clip_at_word(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    let cut = head.rfind(char::is_whitespace).unwrap_or(head.len());
    format!("{}...", head[..cut].trim_end())
}

/// The consequence part of a severity explanation.
///
/// `severity.md` has the explanation state the consequence, then the
/// triggering path, then reachability, each under its own label and often in
/// one paragraph. The consequence is what the summary keeps; the rest is part
/// of the argument it leaves out. Sentences are no guide to where it ends --
/// they are full of "i.e." -- but the next label is.
fn consequence_of(explanation: &str) -> &str {
    let first = explanation.trim().lines().next().unwrap_or("").trim();
    let end = ["Triggering path:", "Reachability:"]
        .iter()
        .filter_map(|label| first.find(label))
        .filter(|&at| at > 0)
        .min()
        .unwrap_or(first.len());
    first[..end].trim_end()
}

/// One earlier finding in a few lines: headline, where it was, what goes wrong
/// if it is real, and its own history when it had one.
fn render_prior_finding(finding: &Value, touches_this_patch: bool) -> String {
    const HEADLINE_FALLBACK_CHARS: usize = 200;
    const CONSEQUENCE_CHARS: usize = 300;
    const MAX_LOCATIONS: usize = 2;

    let severity = finding["severity"].as_str().unwrap_or("Low");
    // Findings recorded before headlines existed fall back to the opening of
    // their argument.
    let headline = finding["headline"]
        .as_str()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let problem = finding["problem"].as_str().unwrap_or("").trim();
            clip_at_word(&problem.replace('\n', " "), HEADLINE_FALLBACK_CHARS)
        });
    let marker = if touches_this_patch {
        "(concerns code in the patch under review) "
    } else {
        ""
    };
    let mut text = format!("  - [{severity}] {marker}{headline}\n");

    let locations: Vec<String> = finding["locations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            let file = l["file"].as_str()?;
            let mut at = file.to_string();
            if let Some(sym) = l["function_or_symbol"].as_str().filter(|s| !s.is_empty()) {
                at.push_str(&format!(", {}()", sym.trim_end_matches("()")));
            }
            if let Some(line) = l["line"].as_i64() {
                at.push_str(&format!(", line {line}"));
            }
            Some(at)
        })
        .take(MAX_LOCATIONS)
        .collect();
    if !locations.is_empty() {
        text.push_str(&format!("    At: {}\n", locations.join("; ")));
    }

    if let Some(consequence) = finding["severity_explanation"]
        .as_str()
        .map(consequence_of)
        .filter(|c| !c.is_empty())
    {
        text.push_str(&format!(
            "    {}\n",
            clip_at_word(consequence, CONSEQUENCE_CHARS)
        ));
    }

    // This finding's own history, so a chain across several rounds stays
    // visible rather than only its latest link.
    if let Some(prior) = finding.get("prior").filter(|p| p.is_object()) {
        let earlier = prior["headline"].as_str().unwrap_or("an earlier finding");
        match prior["relation"].as_str() {
            Some("repeat") => {
                text.push_str(&format!("    History: a repeat of \"{earlier}\"\n"));
            }
            Some("fix_regression") => {
                text.push_str(&format!(
                    "    History: introduced by the change made for \"{earlier}\"\n"
                ));
            }
            _ => {}
        }
    }
    text
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
            planned_stages_from(&["goal", "implementation", "locking"]),
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
        assert_eq!(planned_stages_from(&[]), Vec::<String>::new());
        // Only analysis stages come through the fan-out.
        assert_eq!(planned_stages_from(&["planning"]), Vec::<String>::new());
    }

    #[test]
    fn test_every_planned_stage_reports_its_progress() {
        // The display divides finished stages by planned ones, so a stage
        // counted in the total that never reports finishing strands the bar
        // short of 100%. Consolidation stages are the ones easily missed:
        // they are planned, but they are not analysis stages.
        for stage in planned_stages_from(&["goal", "locking"]) {
            assert!(is_counted_stage(&stage), "{stage} is counted but silent");
        }

        // The two that report through events of their own, and are not part
        // of that total.
        assert!(!is_counted_stage("pre-screen"));
        assert!(!is_counted_stage("planning"));
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

    /// The patch under review: a PR commit touching one driver file.
    fn prior_patchset(subject: &str, revisions: Value) -> Value {
        serde_json::json!({
            "patch_index": 1,
            "patches": [{
                "index": 1,
                "subject": subject,
                "message_id": "cafe0003",
                "commit_id": "local0003",
                "diff": "diff --git a/drivers/pds/cper.c b/drivers/pds/cper.c\n@@ -40,6 +40,7 @@ int pdsc_cper_init(struct pdsc *pdsc)\n+pdsc_cper_register(pdsc);\n"
            }],
            "prior_revisions": revisions
        })
    }

    fn prior_finding(headline: &str, file: &str, symbol: &str) -> Value {
        serde_json::json!({
            "severity": "High",
            "headline": headline,
            "problem": format!("{headline}: THE FULL ARGUMENT."),
            "severity_explanation": format!(
                "Consequence: {headline} breaks things.\n1. Reasoning the summary leaves out."
            ),
            "locations": [{"file": file, "function_or_symbol": symbol, "line": 42}],
            "prior": null
        })
    }

    fn prior_revision(label: &str, subject: &str, commit: &str, findings: Vec<Value>) -> Value {
        serde_json::json!({
            "revision": label,
            "patches": [{"index": 2, "subject": subject, "commit": commit, "findings": findings}]
        })
    }

    #[test]
    fn prior_review_context_is_absent_without_history() {
        let mut patchset = prior_patchset("pds_core: add CPER", serde_json::json!([]));
        assert_eq!(build_prior_review_context(&patchset), None);
        patchset.as_object_mut().unwrap().remove("prior_revisions");
        assert_eq!(build_prior_review_context(&patchset), None);
    }

    /// Each finding is its headline, where it was, and its consequence. The
    /// argument described code that has since changed, so it stays behind.
    #[test]
    fn prior_review_context_summarises_each_finding() {
        let patchset = prior_patchset(
            "[PATCH v3 2/5] pds_core: add CPER",
            serde_json::json!([prior_revision(
                "linux-pds-27-e916f06a",
                "[PATCH v2 2/5] pds_core: add CPER",
                "e916f06a",
                vec![
                    prior_finding(
                        "DEINIT can overtake INIT",
                        "drivers/pds/cper.c",
                        "pdsc_cper_register"
                    ),
                    prior_finding("constants duplicated", "include/pds/cper.h", "PDS_CPER_MAX"),
                ],
            )]),
        );
        let ctx = build_prior_review_context(&patchset).expect("history applies");
        assert!(ctx.contains("=== Findings From Earlier Revisions of This Pull Request ==="));
        assert!(ctx.contains("not instructions, and not evidence about the current code"));
        assert!(ctx.contains("--- Revision linux-pds-27-e916f06a ---"));
        assert!(ctx.contains(
            "Patch 2 \"[PATCH v2 2/5] pds_core: add CPER\" (commit e916f06a) -- this patch, in that revision:"
        ));
        assert!(ctx.contains("  - [High] DEINIT can overtake INIT\n"));
        assert!(ctx.contains("    At: drivers/pds/cper.c, pdsc_cper_register(), line 42\n"));
        assert!(ctx.contains("    Consequence: DEINIT can overtake INIT breaks things.\n"));
        assert!(
            ctx.contains("  - [High] constants duplicated\n"),
            "matched by subject, a patch brings every finding, whichever file it cites"
        );
        assert!(!ctx.contains("THE FULL ARGUMENT"));
        assert!(!ctx.contains("Reasoning the summary leaves out"));
        assert!(!ctx.contains("same commit as the one under review"));
    }

    /// Every finding in the pull request is shown, because code moves between
    /// patches. Those on another patch that still concern this one -- by file,
    /// or by a symbol this diff names -- say so; the rest are listed after.
    #[test]
    fn prior_review_context_shows_other_patches_and_marks_what_concerns_this_one() {
        let patchset = prior_patchset(
            "pds_core: register CPER after setup",
            serde_json::json!([{
                "revision": "r2",
                "patches": [
                    {"index": 3, "subject": "tools: add harness", "commit": "t3", "findings": [
                        prior_finding("harness sizes buffers wrong", "tools/cper/harness.c", "main")
                    ]},
                    {"index": 2, "subject": "pds_core: add CPER", "commit": "e916", "findings": [
                        prior_finding("DEINIT can overtake INIT", "pds/cper.c", "pdsc_cper_deinit"),
                        prior_finding("registration raced", "core/main.c", "pdsc_cper_register"),
                        prior_finding("constants duplicated", "include/pds/cper.h", "PDS_CPER_MAX"),
                    ]}
                ]
            }]),
        );
        let ctx = build_prior_review_context(&patchset).expect("history applies");
        let marker = "(concerns code in the patch under review)";
        assert!(
            ctx.contains(&format!("  - [High] {marker} DEINIT can overtake INIT")),
            "a shortened path still names the touched file"
        );
        assert!(
            ctx.contains(&format!("  - [High] {marker} registration raced")),
            "a symbol the diff names follows code that moved to another file"
        );
        assert!(ctx.contains("  - [High] constants duplicated"));
        assert!(ctx.contains("  - [High] harness sizes buffers wrong"));
        assert!(ctx.contains(
            "Patch 3 \"tools: add harness\" (commit t3) -- another patch in this pull request:"
        ));
        assert!(
            !ctx.contains(&format!("{marker} harness")),
            "a short or unrelated symbol is not a match"
        );
    }

    #[test]
    fn the_consequence_ends_where_the_next_label_begins() {
        assert_eq!(
            consequence_of(
                "Consequence: torn records, i.e. wrong data. Triggering path: a reader races. \
                 Reachability: local root."
            ),
            "Consequence: torn records, i.e. wrong data."
        );
        assert_eq!(
            consequence_of("Consequence: none at runtime.\n\nTriggering path: the branch."),
            "Consequence: none at runtime."
        );
        assert_eq!(
            consequence_of("High, because the ring overruns."),
            "High, because the ring overruns.",
            "an explanation without the labels keeps its opening line"
        );
    }

    #[test]
    fn symbol_matching_wants_a_whole_identifier() {
        assert!(contains_identifier(
            "+\tpdsc_cper_register(pdsc);",
            "pdsc_cper_register"
        ));
        assert!(!contains_identifier(
            "+\tpdsc_cper_register_all();",
            "pdsc_cper_register"
        ));
        assert!(!contains_identifier(
            "+\t__pdsc_cper_register();",
            "pdsc_cper_register"
        ));
    }

    #[test]
    fn prior_review_context_says_when_the_commit_is_unchanged() {
        let patchset = prior_patchset(
            "pds_core: add CPER",
            serde_json::json!([prior_revision(
                "r2",
                "pds_core: add CPER",
                "cafe0003",
                vec![prior_finding(
                    "read() == 0 is overloaded",
                    "drivers/pds/cper.c",
                    "pdsc_cper_read"
                )],
            )]),
        );
        let ctx = build_prior_review_context(&patchset).unwrap();
        assert!(ctx.contains(
            "The same commit as the one under review: its code has not changed since these findings were reported"
        ));
    }

    /// The newest revision's diff is what the author changed in response to it,
    /// cut down to the files the findings about this patch cite.
    #[test]
    fn prior_review_context_shows_the_newest_revisions_diff_for_cited_files() {
        let mut newest = prior_revision(
            "r2",
            "pds_core: add CPER",
            "e916f06a",
            vec![prior_finding(
                "DEINIT can overtake INIT",
                "drivers/pds/cper.c",
                "pdsc_cper_deinit",
            )],
        );
        newest["patches"][0]["diff"] = serde_json::json!(
            "diff --git a/drivers/pds/cper.c b/drivers/pds/cper.c\n+registered = true;\n\
             diff --git a/drivers/pds/other.c b/drivers/pds/other.c\n+unrelated();\n"
        );
        let mut older = prior_revision(
            "r1",
            "pds_core: add CPER",
            "a2a14601",
            vec![prior_finding(
                "read() == 0 is overloaded",
                "drivers/pds/cper.c",
                "pdsc_cper_read",
            )],
        );
        older["patches"][0]["diff"] =
            serde_json::json!("diff --git a/drivers/pds/cper.c b/drivers/pds/cper.c\n+old();\n");

        let patchset = prior_patchset("pds_core: add CPER", serde_json::json!([newest, older]));
        let ctx = build_prior_review_context(&patchset).unwrap();
        assert!(ctx.contains("That revision's version of the code these findings were about"));
        assert!(ctx.contains("+registered = true;"));
        assert!(
            !ctx.contains("+unrelated();"),
            "only files the findings cite"
        );
        assert!(!ctx.contains("+old();"), "only the newest earlier revision");
        assert!(
            ctx.find("Revision r2").unwrap() < ctx.find("Revision r1").unwrap(),
            "newest first"
        );
    }

    #[test]
    fn prior_review_context_carries_a_findings_own_history() {
        let mut finding = prior_finding("DEINIT can overtake INIT", "drivers/pds/cper.c", "x");
        finding["prior"] = serde_json::json!({
            "relation": "fix_regression",
            "revision": "r1",
            "headline": "flag published outside the devcmd"
        });
        let patchset = prior_patchset(
            "pds_core: add CPER",
            serde_json::json!([prior_revision(
                "r2",
                "pds_core: add CPER",
                "e916",
                vec![finding]
            )]),
        );
        let ctx = build_prior_review_context(&patchset).unwrap();
        assert!(ctx.contains(
            "    History: introduced by the change made for \"flag published outside the devcmd\""
        ));
    }

    /// Findings recorded before headlines existed are named by the opening of
    /// their argument, and a long consequence is cut at a word, not mid-word.
    #[test]
    fn prior_review_context_clips_what_it_summarises() {
        let mut finding = prior_finding("unused", "drivers/pds/cper.c", "x");
        finding["headline"] = Value::Null;
        finding["problem"] =
            serde_json::json!(format!("The ring reader {}", "overruns ".repeat(40)));
        finding["severity_explanation"] = serde_json::json!(format!(
            "Consequence: i.e. {}",
            "records are lost ".repeat(40)
        ));
        let patchset = prior_patchset(
            "pds_core: add CPER",
            serde_json::json!([prior_revision(
                "r2",
                "pds_core: add CPER",
                "e916",
                vec![finding]
            )]),
        );
        let ctx = build_prior_review_context(&patchset).unwrap();
        let headline = ctx
            .lines()
            .find(|l| l.starts_with("  - [High] The ring reader"))
            .expect("the problem stands in for a missing headline");
        assert!(headline.ends_with("overruns..."), "{headline}");
        assert!(headline.len() < 230);
        let consequence = ctx
            .lines()
            .find(|l| l.trim_start().starts_with("Consequence: i.e."))
            .expect("not split at the abbreviation's period");
        assert!(
            ["records...", "are...", "lost..."]
                .iter()
                .any(|w| consequence.ends_with(w)),
            "cut after a whole word: {consequence}"
        );
        assert!(consequence.len() < 320);
    }

    /// Findings about this patch are the last to go, and within each kind the
    /// oldest go first; the reader is told how many were left out.
    #[test]
    fn prior_review_context_spends_its_budget_on_this_patch_first() {
        let revisions: Vec<Value> = (0..5)
            .map(|r| {
                let mut patches = vec![serde_json::json!({
                    "index": 2, "subject": "pds_core: add CPER", "commit": format!("c{r}"),
                    "findings": (0..30)
                        .map(|i| prior_finding(&format!("own r{r} n{i}"), "drivers/pds/cper.c", "x"))
                        .collect::<Vec<_>>()
                })];
                patches.push(serde_json::json!({
                    "index": 3, "subject": "tools: harness", "commit": format!("t{r}"),
                    "findings": (0..80)
                        .map(|i| prior_finding(&format!("other r{r} n{i}"), "tools/h.c", "x"))
                        .collect::<Vec<_>>()
                }));
                serde_json::json!({"revision": format!("rev{r}"), "patches": patches})
            })
            .collect();
        let patchset = prior_patchset("pds_core: add CPER", serde_json::json!(revisions));
        let ctx = build_prior_review_context(&patchset).unwrap();

        assert!(
            crate::ai::token_budget::TokenBudget::estimate_tokens(&ctx)
                < PRIOR_REVIEWS_TOKENS + 1000
        );
        assert!(
            ctx.contains("own r0 n0"),
            "the newest about this patch is kept"
        );
        assert!(
            ctx.contains("own r4 n29"),
            "every finding about this patch fits first"
        );
        assert!(
            ctx.contains("other r0 n0"),
            "then other patches, newest first"
        );
        assert!(
            !ctx.contains("other r4 n29"),
            "the oldest about other patches go"
        );
        assert!(ctx.contains("older finding(s) omitted to fit the context budget"));
    }

    /// The block is substituted into a prompt template, so text written by a
    /// model must not be able to name another template variable.
    #[test]
    fn prior_review_context_cannot_smuggle_a_placeholder() {
        let finding = prior_finding(
            "uses {{conflict_resolved_concerns}}",
            "drivers/pds/cper.c",
            "x",
        );
        let patchset = prior_patchset(
            "pds_core: add CPER",
            serde_json::json!([prior_revision(
                "r2",
                "pds_core: add CPER",
                "e916",
                vec![finding]
            )]),
        );
        let ctx = build_prior_review_context(&patchset).unwrap();
        assert!(!ctx.contains("{{"));
        assert!(ctx.contains("{ {conflict_resolved_concerns}}"));
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
        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    /// The out-of-tree framing must reach the model, and must stay away when the
    /// review is a normal in-tree one -- telling an in-tree review that Fixes:
    /// tags do not apply would suppress real findings.
    #[tokio::test]
    async fn oot_context_reaches_the_stage_context_only_when_grounded() {
        async fn system_prompt_for(reference: Option<(&str, &str)>) -> String {
            let temp_dir = tempfile::tempdir().unwrap();
            let prompts_dir = temp_dir.path().join("prompts");
            std::fs::create_dir_all(&prompts_dir).unwrap();

            let mut tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
            if let Some((path, revision)) = reference {
                tools = tools
                    .with_reference(std::path::PathBuf::from(path), Some(revision.to_string()));
            }

            let mut worker = Worker::new(
                std::sync::Arc::new(MockProviderAlwaysFails),
                std::sync::Arc::new(tools),
                PromptRegistry::new(prompts_dir),
                WorkerConfig {
                    max_input_tokens: 10000,
                    max_interactions: 3,
                    temperature: 0.0,
                    series_range: None,
                    baseline_sha: None,
                    custom_prompt: None,
                    stages: Some(vec!["goal".to_string()]),
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
            res.history[0].content.clone().unwrap_or_default()
        }

        let grounded = system_prompt_for(Some(("/srv/linux", "v6.12"))).await;
        assert!(
            grounded.contains("Out-of-Tree Module"),
            "the model must be told the tree it is reviewing is not mainline"
        );
        assert!(
            grounded.contains("v6.12"),
            "the pinned kernel version must be named: {grounded}"
        );
        assert!(
            grounded.contains(r#"repo="kernel""#),
            "the model must be told how to reach the kernel tree"
        );
        assert!(
            grounded.contains("Fixes:"),
            "the in-tree conventions that no longer apply must be named"
        );

        let in_tree = system_prompt_for(None).await;
        assert!(
            !in_tree.contains("Out-of-Tree Module"),
            "an in-tree review must not be told to relax in-tree conventions"
        );
    }

    /// The cover letter has to survive the whole trip into stage context, or
    /// the flag buys nothing.
    #[tokio::test]
    async fn cover_letter_reaches_the_stage_context() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let provider = std::sync::Arc::new(MockProviderAlwaysFails);
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let config = WorkerConfig {
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            baseline_sha: None,
            custom_prompt: None,
            stages: Some(vec!["goal".to_string()]),
        };
        let mut worker = Worker::new(
            provider,
            std::sync::Arc::new(tools),
            PromptRegistry::new(prompts_dir.clone()),
            config,
        );

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "cover_letter": "This series frees hwstamp queues when timestamping is disabled.",
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
        });

        let res = worker.run(patchset, None).await.expect("review runs");
        let sys = res.history[0].content.as_deref().unwrap_or_default();
        assert!(
            sys.contains("Series Cover Letter"),
            "the cover letter must be labelled as intent, not smuggled in as a patch"
        );
        assert!(sys.contains("frees hwstamp queues"), "{sys}");

        // An absent cover letter renders nothing, rather than an empty labelled
        // section the model would have to reason about.
        let mut bare_worker = Worker::new(
            std::sync::Arc::new(MockProviderAlwaysFails),
            std::sync::Arc::new(crate::toolbox::ToolBox::new(
                temp_dir.path().to_path_buf(),
                None,
            )),
            PromptRegistry::new(prompts_dir),
            WorkerConfig {
                max_input_tokens: 10000,
                max_interactions: 3,
                temperature: 0.0,
                series_range: None,
                baseline_sha: None,
                custom_prompt: None,
                stages: Some(vec!["goal".to_string()]),
            },
        );
        let bare = bare_worker
            .run(
                serde_json::json!({
                    "id": 1,
                    "patch_index": 1,
                    "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
                }),
                None,
            )
            .await
            .expect("review runs");
        assert!(
            !bare.history[0]
                .content
                .as_deref()
                .unwrap_or_default()
                .contains("Series Cover Letter")
        );
    }

    /// The concurrent analysis stages run under `BestEffort`, so a failing
    /// stage no longer aborts the review. That is only acceptable because the
    /// failure is reported: partial coverage presented as full coverage would
    /// be worse than an outright failure.
    #[tokio::test]
    async fn test_stage_failure_is_reported_not_swallowed() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let provider = std::sync::Arc::new(MockProviderAlwaysFails);
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
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
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
        });

        let result = worker
            .run(patchset, None)
            .await
            .expect("a failing stage must no longer abort the whole review");

        let output = result
            .output
            .expect("a review with failed stages still produces output");
        let failures = output["stage_failures"]
            .as_array()
            .expect("stage_failures must be present in the output");

        assert!(
            !failures.is_empty(),
            "every stage failed, so the review must say so rather than reporting clean"
        );
        assert!(
            failures.iter().all(|f| f["reason"]
                .as_str()
                .unwrap_or("")
                .contains("simulated AI failure")),
            "the reason must survive to the caller: {:?}",
            failures
        );
        assert!(
            failures
                .iter()
                .all(|f| f["stage"].as_str().is_some_and(|s| !s.is_empty())),
            "each failure must name which stage it was: {:?}",
            failures
        );
        assert!(
            failures.iter().all(|f| f["cancelled"] == false),
            "a genuine error must not be mislabelled as a cancellation"
        );
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

        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
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
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            baseline_sha: None,
            custom_prompt: None,
            stages: Some(vec!["goal".to_string()]),
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
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            baseline_sha: Some("explicit_baseline_sha".to_string()),
            custom_prompt: None,
            stages: Some(vec!["goal".to_string()]),
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
        let sys_content = worker_res.history[0].content.as_deref().unwrap_or_default();
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
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: Some("base_sha..sha2".to_string()),
            baseline_sha: Some("base_sha".to_string()),
            custom_prompt: None,
            stages: Some(vec!["goal".to_string()]),
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
        let sys_content = worker_res.history[0].content.as_deref().unwrap_or_default();
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

        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
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
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: Some("base_sha..sha2".to_string()),
            baseline_sha: Some("base_sha".to_string()),
            custom_prompt: None,
            stages: Some(vec!["goal".to_string()]),
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
}
