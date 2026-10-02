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

use crate::ai::AiMessage;
use crate::ai::session::{SessionStopped, StopCause};

use super::events::WorkflowEvent;
use super::graph::{Workflow, WorkflowStep};
use super::policy::ParallelPolicy;
use super::stage::{ExecutableStage, WorkflowEnv};

/// Execution outcome and aggregated metrics from running a workflow.
#[derive(Debug, Clone, Default)]
pub struct WorkflowOutcome {
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    pub history: Vec<AiMessage>,
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

impl WorkflowOutcome {
    /// Adds one finished stage's metrics and history.
    ///
    /// A stage the deadline cut short is also listed in `stage_failures`,
    /// although it finished, because its output covers less than a full run.
    /// Without that, a review whose analysis wound down would read as one
    /// with full coverage, and the daemon could not tell it was salvaged.
    fn absorb(&mut self, stage_name: &'static str, stage: super::stage::StageOutcome) {
        self.tokens_in += stage.tokens_in;
        self.tokens_out += stage.tokens_out;
        self.tokens_cached += stage.tokens_cached;
        self.history.extend(stage.history);
        if stage.cut_short {
            info!(
                "Stage '{}' was cut short: the review ran out of time",
                stage_name
            );
            self.stage_failures.push(StageFailure {
                stage_name,
                reason: CUT_SHORT_REASON.to_string(),
                cause: StageStopCause::CutShort,
            });
        }
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

        for step in &workflow.steps {
            match step {
                WorkflowStep::Stage(stage) => {
                    let stage_outcome = stage.execute(env, state, event_cb).await?;
                    outcome.absorb(stage.name(), stage_outcome);
                }

                WorkflowStep::Parallel { stages, policy } => {
                    execute_parallel_batch(stages, *policy, env, state, event_cb, &mut outcome)
                        .await?;
                }

                WorkflowStep::DynamicParallel {
                    planner,
                    resolver,
                    policy,
                } => {
                    let planner_outcome = planner.execute(env, state, event_cb).await?;
                    outcome.absorb(planner.name(), planner_outcome);

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
                            &mut outcome,
                        )
                        .await?;
                    }
                }

                WorkflowStep::Branch {
                    condition,
                    then_flow,
                    else_flow,
                } => {
                    let branch_outcome = if condition(state) {
                        Box::pin(Self::execute(then_flow, env, state, event_cb)).await?
                    } else if let Some(else_flow) = else_flow {
                        Box::pin(Self::execute(else_flow, env, state, event_cb)).await?
                    } else {
                        WorkflowOutcome::default()
                    };

                    outcome.tokens_in += branch_outcome.tokens_in;
                    outcome.tokens_out += branch_outcome.tokens_out;
                    outcome.tokens_cached += branch_outcome.tokens_cached;
                    outcome.history.extend(branch_outcome.history);
                    outcome.stage_failures.extend(branch_outcome.stage_failures);

                    if branch_outcome.early_exit {
                        outcome.early_exit = true;
                        outcome.early_exit_reason = branch_outcome.early_exit_reason;
                        break;
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

        if let Some(cb) = event_cb {
            cb(WorkflowEvent::WorkflowFinished {
                name: workflow.name,
                total_tokens: outcome.tokens_in + outcome.tokens_out,
            });
        }

        Ok(outcome)
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
            let futures = stages
                .iter()
                .map(|stage| stage.execute_isolated(env, state, event_cb));
            let results = futures::future::try_join_all(futures).await?;

            for (stage, (stage_outcome, mutation)) in stages.iter().zip(results) {
                mutation(state);
                outcome.absorb(stage.name(), stage_outcome);
            }
        }

        ParallelPolicy::BestEffort => {
            let futures = stages
                .iter()
                .map(|stage| stage.execute_isolated(env, state, event_cb));
            let results = futures::future::join_all(futures).await;

            let mut successes = 0;
            let mut first_error = None;
            // This group's failures only: `outcome` also holds earlier groups'.
            let mut failures = Vec::new();

            for (stage, res) in stages.iter().zip(results) {
                match res {
                    // A stage cut short still counts as a success: it
                    // produced output, and its mutation keeps what it found.
                    // `absorb` records the coverage it lost.
                    Ok((stage_outcome, mutation)) => {
                        successes += 1;
                        mutation(state);
                        outcome.absorb(stage.name(), stage_outcome);
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
                        failures.push(StageFailure {
                            stage_name: stage.name(),
                            reason: format!("{:#}", err),
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
                usage: None,
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
