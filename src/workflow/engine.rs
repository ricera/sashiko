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

//! Runtime execution engine for declarative workflows.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::ai::session::{SessionStopped, StopCause};
use crate::api::ReviewLogEntry;

use super::events::WorkflowEvent;
use super::graph::{Workflow, WorkflowStep};
use super::policy::ParallelPolicy;
use super::stage::{ExecutableStage, StageOutcome, WorkflowEnv};

/// Execution telemetry and transcript slice bounds for a single stage.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct StageRunRecord {
    pub name: String,
    pub skipped: bool,
    pub turns: usize,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    pub prompts_read: Vec<String>,
    pub history_start: usize,
    pub history_end: usize,
}

/// Execution outcome and aggregated metrics from running a workflow.
#[derive(Debug, Clone, Default)]
pub struct WorkflowOutcome {
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    /// Every stage's conversation, each message marked with the stage it
    /// belongs to: the shape a review's stored log keeps, so the log a worker
    /// returns and the one the daemon keeps from the stream agree.
    ///
    /// One entry per message, so each StageRunRecord's
    /// history_start..history_end still indexes this vector.
    pub history: Vec<ReviewLogEntry>,
    pub stage_runs: Vec<StageRunRecord>,
    pub early_exit: bool,
    pub early_exit_reason: Option<&'static str>,
    /// Stages that did not complete under `BestEffort` while a sibling did,
    /// and stages the review's deadline cut short.
    /// Recorded rather than only logged: a review that silently skipped a stage
    /// but presents as complete is worse than one that fails outright, because
    /// nothing signals the missing coverage. A batch where every stage failed is
    /// still an error, so this only ever describes partial coverage.
    pub stage_failures: Vec<StageFailure>,
}

/// The reason recorded for a stage that wound down at the review's deadline
/// and still produced output. One text, so the daemon's record of such a
/// stage reads the same as the engine's.
pub const CUT_SHORT_REASON: &str =
    "cut short: the review ran out of time; its findings so far were kept";

/// Why a stage stopped without completing.
///
/// Carried by every hop from the workflow to the stored review, so the reader
/// of a stopped stage learns whether to look for a defect, a cancel or a
/// deadline without parsing the reason text. Serialised in snake case, which
/// is the spelling the stored `stage_failures` JSON and the page use.
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

impl StageStopCause {
    /// Whether the review's deadline, rather than a fault or a cancel, is what
    /// stopped the stage.
    pub fn is_deadline(self) -> bool {
        matches!(self, Self::OutOfTime | Self::CutShort)
    }

    /// The words that follow a stage's name wherever a stopped stage is shown:
    /// the activity description, the local progress line and the CLI. The web
    /// page spells the same labels in `stageStopLabel`, so a stage reads the
    /// same in every view.
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "failed",
            Self::Cancelled => "cancelled",
            Self::OutOfTime => "stopped: out of time",
            Self::CutShort => "cut short",
            Self::Interrupted => "still running when the review stopped",
        }
    }
}

impl From<StopCause> for StageStopCause {
    fn from(cause: StopCause) -> Self {
        match cause {
            StopCause::OutOfTime => Self::OutOfTime,
        }
    }
}

/// Why the stage whose error this is stopped.
///
/// A session the deadline stopped says so with a typed [`SessionStopped`],
/// found anywhere in the error's chain; every other error is the stage's own.
pub fn stop_cause(err: &anyhow::Error) -> StageStopCause {
    err.downcast_ref::<SessionStopped>()
        .map_or(StageStopCause::Error, |stopped| stopped.cause.into())
}

/// The reason a stage reports for the error that stopped it: every message
/// in the error's chain, outermost first, joined by ": ".
///
/// A link that repeats the one before it is left out. The session wraps each
/// error it returns in a [`crate::ai::SessionFailure`] whose message is that
/// error's own, to carry the partial history; listed in full, the chain
/// would say every reason twice.
pub fn failure_reason(err: &anyhow::Error) -> String {
    let mut links: Vec<String> = Vec::new();
    for link in err.chain().map(ToString::to_string) {
        if links.last() != Some(&link) {
            links.push(link);
        }
    }
    links.join(": ")
}

/// A stage that failed while its siblings kept running.
#[derive(Debug, Clone)]
pub struct StageFailure {
    pub stage_name: &'static str,
    pub reason: String,
    /// Why it stopped. A cancel or a deadline is not a defect in the stage, and
    /// is classified apart from one.
    pub cause: StageStopCause,
}

/// The context of the error a `BestEffort` group returns when none of its
/// stages produced anything.
///
/// Typed so a caller can ask why the group failed with `downcast_ref` instead
/// of reading the text, and so the answer covers every stage of the group
/// rather than only the one whose error the context wraps.
#[derive(Debug, Clone)]
pub struct ParallelGroupFailed {
    /// Every stage of the group, in the order the group lists them.
    pub failures: Vec<StageFailure>,
}

impl ParallelGroupFailed {
    /// Whether the deadline stopped any stage of the group. Asked of the whole
    /// group because the stage whose error is wrapped is only the first to
    /// fail, and a deadline stop further down the list counts as much.
    pub fn stopped_by_deadline(&self) -> bool {
        self.failures.iter().any(|f| f.cause.is_deadline())
    }
}

impl std::fmt::Display for ParallelGroupFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "All {} parallel stages failed under BestEffort policy",
            self.failures.len()
        )?;
        match self.failures.first() {
            Some(first) => write!(f, " (first failure in stage '{}')", first.stage_name),
            None => Ok(()),
        }
    }
}

/// Error wrapper attached by [`WorkflowEngine::execute`] when a workflow aborts,
/// preserving the partial [`WorkflowOutcome`] accumulated across all completed
/// and failed stages up to the point of failure.
#[derive(Debug, Clone)]
pub struct WorkflowFailure {
    pub message: String,
    pub outcome: WorkflowOutcome,
}

impl std::fmt::Display for WorkflowFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WorkflowFailure {}

fn stage_outcome_from_error(err: &anyhow::Error) -> StageOutcome {
    if let Some(failure) = err.downcast_ref::<crate::ai::SessionFailure>() {
        StageOutcome {
            tokens_in: u32::try_from(failure.usage.prompt_tokens).unwrap_or(u32::MAX),
            tokens_out: u32::try_from(failure.usage.completion_tokens).unwrap_or(u32::MAX),
            tokens_cached: u32::try_from(failure.usage.cached_tokens.unwrap_or(0))
                .unwrap_or(u32::MAX),
            history: failure.history.clone(),
            skipped: false,
            cut_short: false,
        }
    } else {
        StageOutcome {
            skipped: false,
            ..Default::default()
        }
    }
}

/// Adds one stage's metrics, history and run record to the workflow outcome.
///
/// A stage the deadline cut short is also listed in `stage_failures`,
/// although it finished, because its output covers less than a full run.
/// Without that, a review whose analysis wound down would read as one with
/// full coverage, and the daemon could not tell it was salvaged.
fn record_stage_outcome(
    outcome: &mut WorkflowOutcome,
    stage_name: &'static str,
    stage_outcome: StageOutcome,
) {
    if stage_outcome.cut_short {
        info!(
            "Stage '{}' was cut short: the review ran out of time",
            stage_name
        );
        outcome.stage_failures.push(StageFailure {
            stage_name,
            reason: CUT_SHORT_REASON.to_string(),
            cause: StageStopCause::CutShort,
        });
    }
    let history_start = outcome.history.len();
    let skipped = stage_outcome.skipped();
    let turns = stage_outcome.turns();
    let prompts_read = stage_outcome.read_prompts();
    outcome.tokens_in = outcome.tokens_in.saturating_add(stage_outcome.tokens_in);
    outcome.tokens_out = outcome.tokens_out.saturating_add(stage_outcome.tokens_out);
    outcome.tokens_cached = outcome
        .tokens_cached
        .saturating_add(stage_outcome.tokens_cached);
    outcome.history.extend(
        stage_outcome
            .history
            .into_iter()
            .map(|message| ReviewLogEntry::new(Some(stage_name), message)),
    );
    let history_end = outcome.history.len();
    outcome.stage_runs.push(StageRunRecord {
        name: stage_name.to_string(),
        skipped,
        turns,
        tokens_in: stage_outcome.tokens_in,
        tokens_out: stage_outcome.tokens_out,
        tokens_cached: stage_outcome.tokens_cached,
        prompts_read,
        history_start,
        history_end,
    });
}

fn merge_workflow_outcome(outcome: &mut WorkflowOutcome, branch_outcome: WorkflowOutcome) {
    let base_history_len = outcome.history.len();
    outcome.tokens_in = outcome.tokens_in.saturating_add(branch_outcome.tokens_in);
    outcome.tokens_out = outcome.tokens_out.saturating_add(branch_outcome.tokens_out);
    outcome.tokens_cached = outcome
        .tokens_cached
        .saturating_add(branch_outcome.tokens_cached);
    outcome.history.extend(branch_outcome.history);
    outcome.stage_failures.extend(branch_outcome.stage_failures);
    for mut rec in branch_outcome.stage_runs {
        rec.history_start = rec.history_start.saturating_add(base_history_len);
        rec.history_end = rec.history_end.saturating_add(base_history_len);
        outcome.stage_runs.push(rec);
    }
}

/// The runtime engine that drives workflow execution.
pub struct WorkflowEngine;

impl WorkflowEngine {
    /// Executes a workflow against the given environment and mutable state.
    pub async fn execute<S: Send + Sync + 'static>(
        workflow: &Workflow<S>,
        env: &WorkflowEnv<'_>,
        state: &mut S,
        event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
    ) -> Result<WorkflowOutcome> {
        if let Some(cb) = event_cb {
            cb(WorkflowEvent::WorkflowStarted {
                name: workflow.name,
            });
        }

        let mut outcome = WorkflowOutcome::default();
        if let Err(err) = Self::execute_steps(workflow, env, state, event_cb, &mut outcome).await {
            let message = err.to_string();
            return Err(err.context(WorkflowFailure { message, outcome }));
        }

        if let Some(cb) = event_cb {
            cb(WorkflowEvent::WorkflowFinished {
                name: workflow.name,
                total_tokens: outcome.tokens_in.saturating_add(outcome.tokens_out),
            });
        }

        Ok(outcome)
    }

    async fn execute_steps<S: Send + Sync + 'static>(
        workflow: &Workflow<S>,
        env: &WorkflowEnv<'_>,
        state: &mut S,
        event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
        outcome: &mut WorkflowOutcome,
    ) -> Result<()> {
        for step in &workflow.steps {
            match step {
                WorkflowStep::Stage(stage) => match stage.execute(env, state, event_cb).await {
                    Ok(stage_outcome) => record_stage_outcome(outcome, stage.name(), stage_outcome),
                    Err(err) => {
                        record_stage_outcome(outcome, stage.name(), stage_outcome_from_error(&err));
                        return Err(err);
                    }
                },

                WorkflowStep::Parallel { stages, policy } => {
                    execute_parallel_batch(stages, *policy, env, state, event_cb, outcome).await?;
                }

                WorkflowStep::DynamicParallel {
                    planner,
                    resolver,
                    policy,
                } => {
                    let planner_outcome = match planner.execute(env, state, event_cb).await {
                        Ok(out) => out,
                        Err(err) => {
                            record_stage_outcome(
                                outcome,
                                planner.name(),
                                stage_outcome_from_error(&err),
                            );
                            return Err(err);
                        }
                    };
                    record_stage_outcome(outcome, planner.name(), planner_outcome);

                    let dynamic_stages = resolver(state);
                    if let Some(cb) = event_cb {
                        cb(WorkflowEvent::ParallelResolved {
                            stage_names: dynamic_stages.iter().map(|s| s.name()).collect(),
                        });
                    }
                    if !dynamic_stages.is_empty() {
                        execute_parallel_batch(
                            &dynamic_stages,
                            *policy,
                            env,
                            state,
                            event_cb,
                            outcome,
                        )
                        .await?;
                    }
                }

                WorkflowStep::Branch {
                    condition,
                    then_flow,
                    else_flow,
                } => {
                    let branch_res = if condition(state) {
                        Some(Box::pin(Self::execute(then_flow, env, state, event_cb)).await)
                    } else if let Some(else_flow) = else_flow {
                        Some(Box::pin(Self::execute(else_flow, env, state, event_cb)).await)
                    } else {
                        None
                    };

                    if let Some(res) = branch_res {
                        match res {
                            Ok(branch_outcome) => {
                                let early_exit = branch_outcome.early_exit;
                                let early_exit_reason = branch_outcome.early_exit_reason;
                                merge_workflow_outcome(outcome, branch_outcome);
                                if early_exit {
                                    outcome.early_exit = true;
                                    outcome.early_exit_reason = early_exit_reason;
                                    break;
                                }
                            }
                            Err(err) => {
                                if let Some(wf) = err.downcast_ref::<WorkflowFailure>() {
                                    merge_workflow_outcome(outcome, wf.outcome.clone());
                                }
                                return Err(err);
                            }
                        }
                    }
                }

                WorkflowStep::EarlyExitIf { condition, reason } => {
                    if condition(state) {
                        info!("Workflow '{}' early exit: {}", workflow.name, reason);
                        if let Some(cb) = event_cb {
                            cb(WorkflowEvent::EarlyExitTriggered { reason });
                        }
                        outcome.early_exit = true;
                        outcome.early_exit_reason = Some(reason);
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

async fn execute_parallel_batch<S: Send + Sync + 'static>(
    stages: &[Box<dyn ExecutableStage<S>>],
    policy: ParallelPolicy,
    env: &WorkflowEnv<'_>,
    state: &mut S,
    event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
    outcome: &mut WorkflowOutcome,
) -> Result<()> {
    info!("Running {} stages concurrently", stages.len());

    match policy {
        ParallelPolicy::FailFast => {
            let state_ref: &S = state;
            let completed_outcomes: Vec<std::sync::Mutex<Option<StageOutcome>>> = (0..stages.len())
                .map(|_| std::sync::Mutex::new(None))
                .collect();
            let outcomes_ref = &completed_outcomes;
            let futures = stages.iter().enumerate().map(|(idx, stage)| async move {
                match stage.execute_isolated(env, state_ref, event_cb).await {
                    Ok((stage_outcome, mutation)) => {
                        if let Ok(mut slot) = outcomes_ref[idx].lock() {
                            *slot = Some(stage_outcome);
                        }
                        Ok(mutation)
                    }
                    Err(err) => {
                        if let Ok(mut slot) = outcomes_ref[idx].lock() {
                            *slot = Some(stage_outcome_from_error(&err));
                        }
                        Err(err)
                    }
                }
            });
            match futures::future::try_join_all(futures).await {
                Ok(mutations) => {
                    for ((stage, slot), mutation) in
                        stages.iter().zip(completed_outcomes).zip(mutations)
                    {
                        mutation(state);
                        if let Ok(Some(stage_outcome)) = slot.into_inner() {
                            record_stage_outcome(outcome, stage.name(), stage_outcome);
                        }
                    }
                }
                Err(err) => {
                    for (stage, slot) in stages.iter().zip(completed_outcomes) {
                        if let Ok(Some(stage_outcome)) = slot.into_inner() {
                            record_stage_outcome(outcome, stage.name(), stage_outcome);
                        }
                    }
                    return Err(err);
                }
            }
        }

        ParallelPolicy::BestEffort => {
            let futures = stages
                .iter()
                .map(|stage| stage.execute_isolated(env, state, event_cb));
            let results = futures::future::join_all(futures).await;

            let mut successes = 0usize;
            let mut first_error = None;
            // This group's failures only: `outcome` also holds earlier groups'.
            let mut failures = Vec::new();

            for (stage, res) in stages.iter().zip(results) {
                match res {
                    // A stage cut short still counts as a success: it
                    // produced output, and its mutation keeps what it found.
                    // `absorb` records the coverage it lost.
                    Ok((stage_outcome, mutation)) => {
                        successes = successes.saturating_add(1);
                        mutation(state);
                        record_stage_outcome(outcome, stage.name(), stage_outcome);
                    }
                    Err(err) => {
                        let cause = stop_cause(&err);
                        if cause.is_deadline() {
                            info!(
                                "Parallel stage '{}' stopped: the review ran out of time",
                                stage.name()
                            );
                        } else {
                            warn!(
                                "Parallel stage '{}' failed under BestEffort policy: {}",
                                stage.name(),
                                err
                            );
                        }
                        record_stage_outcome(outcome, stage.name(), stage_outcome_from_error(&err));
                        failures.push(StageFailure {
                            stage_name: stage.name(),
                            reason: failure_reason(&err),
                            cause,
                        });
                        first_error.get_or_insert(err);
                    }
                }
            }

            if !stages.is_empty()
                && successes == 0
                && let Some(err) = first_error
            {
                return Err(err.context(ParallelGroupFailed { failures }));
            }
            outcome.stage_failures.extend(failures);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiProvider, AiRequest, AiResponse, ProviderCapabilities};
    use crate::toolbox::ToolBox;
    use crate::workflow::output::OutputFormat;
    use crate::workflow::prompt::PromptTemplate;
    use crate::workflow::stage::Stage;
    use serde::Deserialize;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A link that repeats the one before it is said once, and distinct links
    /// all stay, outermost first.
    #[test]
    fn failure_reason_says_each_message_once() {
        let wrapped = |inner: anyhow::Error| {
            let message = inner.to_string();
            inner.context(crate::ai::SessionFailure {
                message,
                history: Vec::new(),
                usage: crate::ai::AiUsage::default(),
            })
        };
        assert_eq!(
            failure_reason(&wrapped(anyhow::anyhow!("refused"))),
            "refused"
        );
        assert_eq!(
            failure_reason(&wrapped(
                anyhow::anyhow!("refused").context("calling the model")
            )),
            "calling the model: refused"
        );
        assert_eq!(
            failure_reason(&wrapped(anyhow::anyhow!("refused")).context("in stage goal")),
            "in stage goal: refused"
        );
    }

    #[derive(Default, Clone)]
    struct DummyState {
        concerns: Vec<String>,
        #[allow(dead_code)]
        findings: Vec<String>,
        selected_stages: Vec<u8>,
    }

    #[derive(Deserialize, Debug)]
    struct DummyConcernsOutput {
        items: Vec<String>,
    }

    #[derive(Deserialize, Debug)]
    struct DummyPlanningOutput {
        stages: Vec<u8>,
    }

    struct MockProvider {
        response_json: String,
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
        call_count: AtomicUsize,
    }

    impl MockProvider {
        fn single(resp: &str) -> Self {
            Self {
                response_json: resp.to_string(),
                responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
                call_count: AtomicUsize::new(0),
            }
        }

        fn queued(resps: Vec<String>) -> Self {
            Self {
                response_json: String::new(),
                responses: std::sync::Mutex::new(resps.into()),
                call_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl AiProvider for MockProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let content = {
                let mut queue = self.responses.lock().unwrap();
                queue
                    .pop_front()
                    .unwrap_or_else(|| self.response_json.clone())
            };
            if content == "__ERROR__" {
                anyhow::bail!("simulated fatal provider error");
            }
            Ok(AiResponse {
                content: Some(content),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: Some(crate::ai::AiUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: None,
                }),
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_workflow_sequential_and_early_exit() {
        let provider = Arc::new(MockProvider::single(r#"{"items": ["leak in foo"]}"#));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf(), None));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
            wind_down: None,
        };

        let mut state = DummyState::default();

        let workflow = Workflow::builder("test_flow")
            .stage(
                Stage::builder("stage_1")
                    .user_prompt(PromptTemplate::new("Analyze"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.concerns.extend(out.items);
                    })
                    .build(),
            )
            .early_exit_if(|s| s.concerns.is_empty(), "no concerns")
            .stage(
                Stage::builder("stage_2")
                    .user_prompt(PromptTemplate::new("Verify"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.findings.extend(out.items);
                    })
                    .build(),
            )
            .build();

        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .unwrap();

        assert!(!outcome.early_exit);
        assert_eq!(state.concerns, vec!["leak in foo".to_string()]);
        assert_eq!(state.findings, vec!["leak in foo".to_string()]);
    }

    #[tokio::test]
    async fn test_workflow_dynamic_parallel_planning() {
        let provider = Arc::new(MockProvider::queued(vec![
            r#"{"stages": [4, 5]}"#.to_string(),
            r#"{"items": ["concern_a"]}"#.to_string(),
            r#"{"items": ["concern_b"]}"#.to_string(),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf(), None));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
            wind_down: None,
        };

        let mut state = DummyState::default();

        let workflow = Workflow::builder("planning_flow")
            .dynamic_parallel(
                Stage::builder("planner")
                    .user_prompt(PromptTemplate::new("Plan stages"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyPlanningOutput| {
                        s.selected_stages = out.stages;
                    })
                    .build(),
                |s| {
                    let mut stages: Vec<Box<dyn ExecutableStage<DummyState>>> = Vec::new();
                    for &n in &s.selected_stages {
                        let stage_name: &'static str = match n {
                            4 => "stage_4",
                            5 => "stage_5",
                            _ => "unknown",
                        };
                        stages.push(Box::new(
                            Stage::builder(stage_name)
                                .user_prompt(PromptTemplate::new("Run dynamic stage"))
                                .output_format(OutputFormat::json())
                                .reduce(move |st: &mut DummyState, out: DummyConcernsOutput| {
                                    for item in out.items {
                                        st.concerns.push(format!("{}: {}", n, item));
                                    }
                                })
                                .build(),
                        ));
                    }
                    stages
                },
                ParallelPolicy::FailFast,
            )
            .build();

        let resolved = std::sync::Mutex::new(Vec::new());
        let record = |event: WorkflowEvent| {
            if let WorkflowEvent::ParallelResolved { stage_names } = event {
                resolved.lock().unwrap().extend(stage_names);
            }
        };
        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, Some(&record))
            .await
            .unwrap();

        assert_eq!(state.selected_stages, vec![4, 5]);
        assert_eq!(state.concerns.len(), 2);
        assert!(!outcome.early_exit);
        // The plan is reported as resolved, not guessed from the stage list.
        assert_eq!(*resolved.lock().unwrap(), vec!["stage_4", "stage_5"]);
    }

    #[tokio::test]
    async fn test_workflow_best_effort_partial_success() {
        let provider = Arc::new(MockProvider::queued(vec![
            "__ERROR__".to_string(),
            r#"{"items": ["concern_ok"]}"#.to_string(),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf(), None));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
            wind_down: None,
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("best_effort_partial")
            .parallel(
                vec![
                    Box::new(
                        Stage::builder("stage_fail")
                            .user_prompt(PromptTemplate::new("Fail"))
                            .output_format(OutputFormat::json())
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.concerns.extend(out.items);
                            })
                            .build(),
                    ),
                    Box::new(
                        Stage::builder("stage_ok")
                            .user_prompt(PromptTemplate::new("Succeed"))
                            .output_format(OutputFormat::json())
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.concerns.extend(out.items);
                            })
                            .build(),
                    ),
                ],
                ParallelPolicy::BestEffort,
            )
            .build();

        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .expect("BestEffort should succeed when at least one stage succeeds");
        assert!(!outcome.early_exit);
        assert_eq!(state.concerns, vec!["concern_ok".to_string()]);
        assert_eq!(outcome.stage_runs.len(), 2);
        assert_eq!(outcome.stage_runs[0].name, "stage_fail");
        assert!(!outcome.stage_runs[0].skipped);
        assert_eq!(outcome.stage_runs[0].history_start, 0);
        assert_eq!(outcome.stage_runs[0].history_end, 1);
        assert_eq!(outcome.stage_runs[1].name, "stage_ok");
        assert!(!outcome.stage_runs[1].skipped);
        assert_eq!(outcome.stage_runs[1].history_start, 1);
        assert_eq!(outcome.stage_runs[1].history_end, 3);
        assert_eq!(outcome.stage_failures.len(), 1, "{outcome:?}");
        let failure = &outcome.stage_failures[0];
        assert_eq!(failure.stage_name, "stage_fail");
        assert_eq!(failure.cause, StageStopCause::Error);
        assert!(
            failure.reason.contains("simulated fatal provider error"),
            "{failure:?}"
        );
    }

    #[tokio::test]
    async fn test_workflow_best_effort_total_failure() {
        let provider = Arc::new(MockProvider::single("__ERROR__"));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf(), None));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
            wind_down: None,
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("best_effort_total_fail")
            .parallel(
                vec![
                    Box::new(
                        Stage::builder("stage_fail_1")
                            .user_prompt(PromptTemplate::new("Fail 1"))
                            .output_format(OutputFormat::json())
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.concerns.extend(out.items);
                            })
                            .build(),
                    ),
                    Box::new(
                        Stage::builder("stage_fail_2")
                            .user_prompt(PromptTemplate::new("Fail 2"))
                            .output_format(OutputFormat::json())
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.concerns.extend(out.items);
                            })
                            .build(),
                    ),
                ],
                ParallelPolicy::BestEffort,
            )
            .build();

        let res = WorkflowEngine::execute(&workflow, &env, &mut state, None).await;
        let err = res.expect_err("BestEffort should fail when all parallel stages fail");
        assert_eq!(
            err.to_string(),
            "All 2 parallel stages failed under BestEffort policy \
             (first failure in stage 'stage_fail_1')",
            "the group error reads as it always has"
        );
        let group = err
            .downcast_ref::<ParallelGroupFailed>()
            .expect("the group error is typed");
        let names: Vec<_> = group.failures.iter().map(|f| f.stage_name).collect();
        assert_eq!(names, ["stage_fail_1", "stage_fail_2"]);
        assert!(
            group
                .failures
                .iter()
                .all(|f| f.cause == StageStopCause::Error)
        );
        assert!(!group.stopped_by_deadline());
        assert!(
            format!("{err:#}").contains("simulated fatal provider error"),
            "the first stage's own error stays in the chain: {err:#}"
        );
    }

    #[tokio::test]
    async fn test_workflow_stage_runs_telemetry_and_history_bounds() {
        let provider = Arc::new(MockProvider::queued(vec![
            r#"{"items": ["c1"]}"#.to_string(),
            r#"{"items": ["c2"]}"#.to_string(),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf(), None));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
            wind_down: None,
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("telemetry_flow")
            .stage(
                Stage::builder("stage_run_1")
                    .user_prompt(PromptTemplate::new("First"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.concerns.extend(out.items);
                    })
                    .build(),
            )
            .stage(
                Stage::builder("stage_skipped")
                    .user_prompt(PromptTemplate::new("Skipped"))
                    .output_format(OutputFormat::json())
                    .skip_if(|_: &DummyState| true)
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.concerns.extend(out.items);
                    })
                    .build(),
            )
            .stage(
                Stage::builder("stage_run_2")
                    .user_prompt(PromptTemplate::new("Second"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.findings.extend(out.items);
                    })
                    .build(),
            )
            .build();

        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .unwrap();

        assert_eq!(outcome.stage_runs.len(), 3);
        assert_eq!(outcome.stage_runs[0].name, "stage_run_1");
        assert!(!outcome.stage_runs[0].skipped);
        assert_eq!(outcome.stage_runs[0].turns, 1);
        assert_eq!(outcome.stage_runs[0].history_start, 0);
        assert_eq!(outcome.stage_runs[0].history_end, 2);

        assert_eq!(outcome.stage_runs[1].name, "stage_skipped");
        assert!(outcome.stage_runs[1].skipped);
        assert_eq!(outcome.stage_runs[1].turns, 0);
        assert_eq!(outcome.stage_runs[1].history_start, 2);
        assert_eq!(outcome.stage_runs[1].history_end, 2);

        assert_eq!(outcome.stage_runs[2].name, "stage_run_2");
        assert!(!outcome.stage_runs[2].skipped);
        assert_eq!(outcome.stage_runs[2].turns, 1);
        assert_eq!(outcome.stage_runs[2].history_start, 2);
        assert_eq!(outcome.stage_runs[2].history_end, 4);
        assert_eq!(outcome.history.len(), 4);
        // Tagging is one entry per message, so each stage's recorded bounds
        // still cover exactly its own messages.
        for run in &outcome.stage_runs {
            assert!(
                outcome.history[run.history_start..run.history_end]
                    .iter()
                    .all(|e| e.stage.as_deref() == Some(run.name.as_str())),
                "{} owns its history slice",
                run.name
            );
        }
    }

    #[tokio::test]
    async fn test_workflow_failure_preserves_accumulated_outcome_on_sequential_error() {
        let provider = Arc::new(MockProvider::queued(vec![
            r#"{"items": ["c1"]}"#.to_string(),
            "not valid json".to_string(),
            "still not valid json".to_string(),
            "still not valid json".to_string(),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf(), None));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
            wind_down: None,
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("failure_telemetry_flow")
            .stage(
                Stage::builder("stage_ok")
                    .user_prompt(PromptTemplate::new("First"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.concerns.extend(out.items);
                    })
                    .build(),
            )
            .stage(
                Stage::builder("stage_fail")
                    .user_prompt(PromptTemplate::new("Second"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.findings.extend(out.items);
                    })
                    .build(),
            )
            .build();

        let err = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .expect_err("second stage should fail");
        let wf = err
            .downcast_ref::<WorkflowFailure>()
            .expect("WorkflowFailure should be attached to error");
        assert_eq!(wf.outcome.stage_runs.len(), 2);
        assert_eq!(wf.outcome.stage_runs[0].name, "stage_ok");
        assert_eq!(wf.outcome.stage_runs[1].name, "stage_fail");
        assert_eq!(wf.outcome.tokens_in, 40);
        assert_eq!(wf.outcome.tokens_out, 20);
        assert!(!wf.outcome.history.is_empty());
        // Tagging is one entry per message, so each stage's recorded bounds
        // still cover exactly its own messages.
        for run in &wf.outcome.stage_runs {
            assert!(
                wf.outcome.history[run.history_start..run.history_end]
                    .iter()
                    .all(|e| e.stage.as_deref() == Some(run.name.as_str())),
                "{} owns its history slice",
                run.name
            );
        }
    }

    #[tokio::test]
    async fn test_workflow_fail_fast_preserves_completed_and_failed_sibling_outcomes() {
        let provider = Arc::new(MockProvider::queued(vec![
            r#"{"items": ["c1"]}"#.to_string(),
            "not valid json".to_string(),
            "still not valid json".to_string(),
            "still not valid json".to_string(),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf(), None));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
            wind_down: None,
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("fail_fast_parallel_telemetry")
            .parallel(
                vec![
                    Box::new(
                        Stage::builder("stage_ok")
                            .user_prompt(PromptTemplate::new("First"))
                            .output_format(OutputFormat::json())
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.concerns.extend(out.items);
                            })
                            .build(),
                    ),
                    Box::new(
                        Stage::builder("stage_fail")
                            .user_prompt(PromptTemplate::new("Second"))
                            .output_format(OutputFormat::json())
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.findings.extend(out.items);
                            })
                            .build(),
                    ),
                ],
                ParallelPolicy::FailFast,
            )
            .build();

        let err = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .expect_err("FailFast parallel batch should fail when a stage fails");
        assert!(
            state.concerns.is_empty(),
            "FailFast must not apply state mutations when any sibling stage fails"
        );
        let wf = err
            .downcast_ref::<WorkflowFailure>()
            .expect("WorkflowFailure should be attached to error");
        assert_eq!(wf.outcome.stage_runs.len(), 2);
        assert_eq!(wf.outcome.stage_runs[0].name, "stage_ok");
        assert_eq!(wf.outcome.stage_runs[1].name, "stage_fail");
        assert_eq!(wf.outcome.tokens_in, 40);
        assert_eq!(wf.outcome.tokens_out, 20);
    }

    fn failure(stage_name: &'static str, cause: StageStopCause) -> StageFailure {
        StageFailure {
            stage_name,
            reason: "x".to_string(),
            cause,
        }
    }

    /// The deadline counts wherever it stopped a stage in the group, not only
    /// when it stopped the stage whose error the context wraps.
    #[test]
    fn a_group_was_stopped_by_the_deadline_if_any_stage_was() {
        for deadline in [StageStopCause::OutOfTime, StageStopCause::CutShort] {
            let group = ParallelGroupFailed {
                failures: vec![
                    failure("goal", StageStopCause::Error),
                    failure("security", deadline),
                ],
            };
            assert!(group.stopped_by_deadline(), "{deadline:?}");
        }
        let group = ParallelGroupFailed {
            failures: vec![
                failure("goal", StageStopCause::Error),
                failure("security", StageStopCause::Cancelled),
                failure("locking", StageStopCause::Interrupted),
            ],
        };
        assert!(!group.stopped_by_deadline());
        assert!(
            !ParallelGroupFailed { failures: vec![] }.stopped_by_deadline(),
            "an empty group was stopped by nothing"
        );
    }

    /// The spelling is stored in `reviews.stage_failures` and read by the
    /// page and the CLI, so it is pinned here.
    #[test]
    fn a_stop_cause_is_stored_in_snake_case() {
        for (cause, spelled, deadline) in [
            (StageStopCause::Error, "error", false),
            (StageStopCause::Cancelled, "cancelled", false),
            (StageStopCause::OutOfTime, "out_of_time", true),
            (StageStopCause::CutShort, "cut_short", true),
            (StageStopCause::Interrupted, "interrupted", false),
        ] {
            let json = serde_json::to_value(cause).unwrap();
            assert_eq!(json, serde_json::json!(spelled));
            assert_eq!(
                serde_json::from_value::<StageStopCause>(json).unwrap(),
                cause
            );
            assert_eq!(cause.is_deadline(), deadline, "{cause:?}");
        }
    }

    /// Answers its first request with text that fails validation and sets
    /// the wind-down while doing so, as if the deadline passed during that
    /// call. Every later request gets a valid answer.
    struct DeadlineOnFirstCallProvider {
        signal: Arc<crate::ai::WindDownSignal>,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl AiProvider for DeadlineOnFirstCallProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
            if first {
                self.signal.request();
            }
            Ok(AiResponse {
                content: Some(
                    if first {
                        "not json"
                    } else {
                        r#"{"items": ["late"]}"#
                    }
                    .to_string(),
                ),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "deadline".to_string(),
                context_window_size: 1000,
            }
        }
    }

    fn concerns_stage(name: &'static str) -> Box<dyn ExecutableStage<DummyState>> {
        Box::new(
            Stage::builder(name)
                .user_prompt(PromptTemplate::new("Analyze"))
                .output_format(OutputFormat::json())
                .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                    s.concerns.extend(out.items);
                })
                .build(),
        )
    }

    /// One analysis stage wound down after its first answer and the other was
    /// stopped before it spent anything. The group still succeeds, keeps what
    /// the cut-short stage found, and records both causes.
    #[tokio::test]
    async fn best_effort_records_the_stages_the_deadline_stopped() {
        let signal = Arc::new(crate::ai::WindDownSignal::default());
        let provider = Arc::new(DeadlineOnFirstCallProvider {
            signal: signal.clone(),
            calls: AtomicUsize::new(0),
        });
        let tmp = tempfile::tempdir().unwrap();
        let env = WorkflowEnv {
            provider: provider.clone(),
            tools: Arc::new(ToolBox::new(tmp.path().to_path_buf(), None)),
            base_dir: tmp.path(),
            context_tag: None,
            wind_down: Some(signal),
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("deadline")
            .parallel(
                vec![concerns_stage("goal"), concerns_stage("security")],
                ParallelPolicy::BestEffort,
            )
            .build();

        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .expect("a cut-short stage counts as a success");

        assert_eq!(state.concerns, vec!["late".to_string()]);
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "the first answer and the verdict; the stopped stage spent nothing"
        );
        let mut causes: Vec<_> = outcome.stage_failures.iter().map(|f| f.cause).collect();
        // Which stage got the first call depends on polling order, so the
        // causes are compared in a fixed order rather than by stage.
        causes.sort_by_key(|c| c.label());
        assert_eq!(
            causes,
            [StageStopCause::CutShort, StageStopCause::OutOfTime],
            "{outcome:?}"
        );
        let cut = outcome
            .stage_failures
            .iter()
            .find(|f| f.cause == StageStopCause::CutShort)
            .unwrap();
        assert_eq!(cut.reason, CUT_SHORT_REASON);
    }

    /// A group the deadline stopped entirely is still an error, typed so the
    /// worker can tell it apart from a group that failed on its own.
    #[tokio::test]
    async fn a_group_the_deadline_stopped_entirely_is_still_an_error() {
        let provider = Arc::new(MockProvider::single(r#"{"items": ["x"]}"#));
        let signal = Arc::new(crate::ai::WindDownSignal::default());
        signal.request();
        let tmp = tempfile::tempdir().unwrap();
        let env = WorkflowEnv {
            provider: provider.clone(),
            tools: Arc::new(ToolBox::new(tmp.path().to_path_buf(), None)),
            base_dir: tmp.path(),
            context_tag: None,
            wind_down: Some(signal),
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("deadline")
            .parallel(
                vec![concerns_stage("goal"), concerns_stage("security")],
                ParallelPolicy::BestEffort,
            )
            .build();

        let err = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .expect_err("a group with no output is an error");
        let group = err
            .downcast_ref::<ParallelGroupFailed>()
            .expect("the group error is typed");
        assert!(
            group
                .failures
                .iter()
                .all(|f| f.cause == StageStopCause::OutOfTime),
            "{group:?}"
        );
        assert!(group.stopped_by_deadline());
        assert_eq!(stop_cause(&err), StageStopCause::OutOfTime);
        assert_eq!(provider.call_count.load(Ordering::SeqCst), 0);
    }

    /// The deadline counts wherever it stopped a stage of the group: here the
    /// first stage failed on its own and only the second was stopped.
    #[tokio::test]
    async fn a_group_is_stopped_by_the_deadline_when_a_later_stage_was() {
        let provider = Arc::new(MockProvider::single("__ERROR__"));
        let signal = Arc::new(crate::ai::WindDownSignal::default());
        signal.request();
        let tmp = tempfile::tempdir().unwrap();
        let env = WorkflowEnv {
            provider,
            tools: Arc::new(ToolBox::new(tmp.path().to_path_buf(), None)),
            base_dir: tmp.path(),
            context_tag: None,
            wind_down: Some(signal),
        };

        // Not interruptible, so it reaches the provider and fails there.
        let own_failure: Box<dyn ExecutableStage<DummyState>> = Box::new(
            Stage::builder("goal")
                .user_prompt(PromptTemplate::new("Analyze"))
                .output_format(OutputFormat::json())
                .policy(crate::workflow::StagePolicy::consolidation())
                .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                    s.concerns.extend(out.items);
                })
                .build(),
        );
        let mut state = DummyState::default();
        let workflow = Workflow::builder("deadline")
            .parallel(
                vec![own_failure, concerns_stage("security")],
                ParallelPolicy::BestEffort,
            )
            .build();

        let err = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .expect_err("no stage produced anything");
        let group = err.downcast_ref::<ParallelGroupFailed>().unwrap();
        let causes: Vec<_> = group.failures.iter().map(|f| f.cause).collect();
        assert_eq!(causes, [StageStopCause::Error, StageStopCause::OutOfTime]);
        assert!(group.stopped_by_deadline());
        assert_eq!(
            stop_cause(&err),
            StageStopCause::Error,
            "the wrapped error is the first stage's own"
        );
    }

    #[test]
    fn stop_cause_reads_a_typed_stop_through_context() {
        use crate::ai::session::{SessionStopped, StopCause};
        let stopped = anyhow::Error::new(SessionStopped {
            cause: StopCause::OutOfTime,
        });
        assert_eq!(stop_cause(&stopped), StageStopCause::OutOfTime);
        let wrapped = anyhow::anyhow!("validation failed")
            .context(SessionStopped {
                cause: StopCause::OutOfTime,
            })
            .context("stage 'goal'");
        assert_eq!(stop_cause(&wrapped), StageStopCause::OutOfTime);
        assert_eq!(
            stop_cause(&anyhow::anyhow!("simulated fatal provider error")),
            StageStopCause::Error
        );
    }
}
