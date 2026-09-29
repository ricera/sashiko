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

use std::collections::HashMap;

use anyhow::Result;
use serde_json::Value;
use tracing::{info, warn};

use crate::ai::AiMessage;

use super::events::WorkflowEvent;
use super::graph::{Workflow, WorkflowStep};
use super::policy::ParallelPolicy;
use super::stage::{ExecutableStage, StageOutcome, StateMutation, WorkflowEnv};

/// Execution outcome and aggregated metrics from running a workflow.
#[derive(Debug, Clone, Default)]
pub struct WorkflowOutcome {
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    pub history: Vec<AiMessage>,
    pub early_exit: bool,
    pub early_exit_reason: Option<&'static str>,
    /// Stages that did not complete, under `BestEffort`. Recorded rather than
    /// only logged: a review that silently skipped a stage but presents as
    /// complete is worse than one that fails outright, because nothing signals
    /// the missing coverage.
    pub stage_failures: Vec<StageFailure>,
}

/// A stage that failed or was cancelled while its siblings kept running.
#[derive(Debug, Clone)]
pub struct StageFailure {
    pub stage_name: &'static str,
    pub reason: String,
    /// Cancellation is not a defect, and is classified apart from one.
    pub cancelled: bool,
}

/// Stage outputs saved from an earlier run of the same review, to be folded
/// into the state in place of running those stages again.
///
/// Only a prefix of the workflow can be replayed. A stage's input is what the
/// stages before it left in the state, so once any stage actually runs -- and
/// may answer differently than it did before -- every saved output after it
/// describes a state that no longer exists. The first step in which a stage
/// runs or fails therefore closes the replay for the rest of the workflow.
struct Replay {
    outputs: HashMap<String, Value>,
    open: bool,
}

impl Replay {
    fn new(outputs: HashMap<String, Value>) -> Self {
        let open = !outputs.is_empty();
        Self { outputs, open }
    }

    /// Folds a stage's saved output into a state mutation, if there is one and
    /// the replay is still open. An output the stage can no longer read is
    /// dropped with a warning, and the stage runs instead.
    fn take<S: Send + Sync + 'static>(
        &mut self,
        stage: &dyn ExecutableStage<S>,
        event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
    ) -> Option<StateMutation<S>> {
        if !self.open {
            return None;
        }
        let output = self.outputs.remove(stage.name())?;
        match stage.replay(&output) {
            Ok(mutation) => {
                info!("Replaying stage '{}' from its saved output", stage.name());
                if let Some(cb) = event_cb {
                    cb(WorkflowEvent::StageReplayed {
                        stage_name: stage.name(),
                        output,
                    });
                }
                Some(mutation)
            }
            Err(e) => {
                warn!(
                    "Saved output of stage '{}' no longer fits it ({:#}); running the stage",
                    stage.name(),
                    e
                );
                None
            }
        }
    }

    fn close(&mut self) {
        if self.open && !self.outputs.is_empty() {
            info!(
                "Not replaying {} later saved stage(s): an earlier stage ran again",
                self.outputs.len()
            );
        }
        self.open = false;
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
        Self::execute_with_replay(workflow, env, state, event_cb, HashMap::new()).await
    }

    /// Executes a workflow, replaying the stages whose outputs were saved by an
    /// earlier run instead of running them, for as long as that is sound. See
    /// [`Replay`] for where it stops.
    pub async fn execute_with_replay<S: Send + Sync + 'static>(
        workflow: &Workflow<S>,
        env: &WorkflowEnv<'_>,
        state: &mut S,
        event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
        saved_outputs: HashMap<String, Value>,
    ) -> Result<WorkflowOutcome> {
        let mut replay = Replay::new(saved_outputs);
        Self::run(workflow, env, state, event_cb, &mut replay).await
    }

    async fn run<S: Send + Sync + 'static>(
        workflow: &Workflow<S>,
        env: &WorkflowEnv<'_>,
        state: &mut S,
        event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
        replay: &mut Replay,
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
                    execute_or_replay(stage.as_ref(), env, state, event_cb, replay, &mut outcome)
                        .await?;
                }

                WorkflowStep::Parallel { stages, policy } => {
                    execute_parallel_batch(
                        stages,
                        *policy,
                        env,
                        state,
                        event_cb,
                        &mut outcome,
                        replay,
                    )
                    .await?;
                }

                WorkflowStep::DynamicParallel {
                    planner,
                    resolver,
                    policy,
                } => {
                    execute_or_replay(planner.as_ref(), env, state, event_cb, replay, &mut outcome)
                        .await?;

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
                            replay,
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
                        Box::pin(Self::run(then_flow, env, state, event_cb, replay)).await?
                    } else if let Some(else_flow) = else_flow {
                        Box::pin(Self::run(else_flow, env, state, event_cb, replay)).await?
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

/// Runs one stage, or folds in its saved output when the replay allows.
async fn execute_or_replay<S: Send + Sync + 'static>(
    stage: &dyn ExecutableStage<S>,
    env: &WorkflowEnv<'_>,
    state: &mut S,
    event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
    replay: &mut Replay,
    outcome: &mut WorkflowOutcome,
) -> Result<()> {
    if let Some(mutation) = replay.take(stage, event_cb) {
        mutation(state);
        return Ok(());
    }
    let stage_outcome = stage.execute(env, state, event_cb).await?;
    if stage_outcome.ran {
        replay.close();
    }
    absorb(outcome, stage_outcome);
    Ok(())
}

fn absorb(outcome: &mut WorkflowOutcome, stage_outcome: StageOutcome) {
    outcome.tokens_in += stage_outcome.tokens_in;
    outcome.tokens_out += stage_outcome.tokens_out;
    outcome.tokens_cached += stage_outcome.tokens_cached;
    outcome.history.extend(stage_outcome.history);
}

async fn execute_parallel_batch<S: Send + Sync + 'static>(
    stages: &[Box<dyn ExecutableStage<S>>],
    policy: ParallelPolicy,
    env: &WorkflowEnv<'_>,
    state: &mut S,
    event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
    outcome: &mut WorkflowOutcome,
    replay: &mut Replay,
) -> Result<()> {
    // Siblings do not read each other's output, so each is replayed or run on
    // its own. Mutations are still applied in stage order below, whichever way
    // each was obtained, so a partly replayed batch leaves the state in the
    // same order a full run would.
    let mut replayed: Vec<Option<StateMutation<S>>> = stages
        .iter()
        .map(|stage| replay.take(stage.as_ref(), event_cb))
        .collect();
    let to_run: Vec<usize> = (0..stages.len())
        .filter(|&i| replayed[i].is_none())
        .collect();
    if to_run.is_empty() {
        for mutation in replayed.into_iter().flatten() {
            mutation(state);
        }
        return Ok(());
    }

    info!("Running {} stages concurrently", to_run.len());

    match policy {
        ParallelPolicy::FailFast => {
            let futures = to_run
                .iter()
                .map(|&i| stages[i].execute_isolated(env, state, event_cb));
            let results = futures::future::try_join_all(futures).await?;
            if results.iter().any(|(stage_outcome, _)| stage_outcome.ran) {
                replay.close();
            }

            let mut results = results.into_iter();
            for slot in replayed.iter_mut() {
                match slot.take() {
                    Some(mutation) => mutation(state),
                    None => {
                        let (stage_outcome, mutation) =
                            results.next().expect("one result per stage run");
                        mutation(state);
                        absorb(outcome, stage_outcome);
                    }
                }
            }
        }

        ParallelPolicy::BestEffort => {
            let futures = to_run
                .iter()
                .map(|&i| stages[i].execute_isolated(env, state, event_cb));
            let results = futures::future::join_all(futures).await;
            // Whether the stages that ran succeeded or not, what follows no
            // longer sees the state the saved outputs were produced from.
            if results.iter().any(|res| {
                res.as_ref()
                    .map_or(true, |(stage_outcome, _)| stage_outcome.ran)
            }) {
                replay.close();
            }

            let mut results = to_run.iter().copied().zip(results);
            for (i, slot) in replayed.iter_mut().enumerate() {
                if let Some(mutation) = slot.take() {
                    mutation(state);
                    continue;
                }
                let (idx, res) = results.next().expect("one result per stage run");
                debug_assert_eq!(idx, i);
                let stage = &stages[i];
                match res {
                    Ok((stage_outcome, mutation)) => {
                        mutation(state);
                        absorb(outcome, stage_outcome);
                    }
                    Err(err) => {
                        let reason = format!("{:#}", err);
                        // Neither a cancellation nor a wind-down is a fault in
                        // the stage, so neither is reported as one -- but both
                        // are coverage the review did not get.
                        let wound_down = reason.contains(crate::ai::session::SESSION_WOUND_DOWN);
                        let cancelled =
                            wound_down || reason.contains(crate::ai::session::SESSION_CANCELLED);
                        if wound_down {
                            info!(
                                "Parallel stage '{}' stopped: the review ran out of time",
                                stage.name()
                            );
                        } else if cancelled {
                            info!("Parallel stage '{}' cancelled", stage.name());
                        } else {
                            warn!(
                                "Parallel stage '{}' failed under BestEffort policy: {}",
                                stage.name(),
                                reason
                            );
                        }
                        outcome.stage_failures.push(StageFailure {
                            stage_name: stage.name(),
                            reason,
                            cancelled,
                        });
                    }
                }
            }
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
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default, Clone)]
    struct DummyState {
        concerns: Vec<String>,
        #[allow(dead_code)]
        findings: Vec<String>,
        selected_stages: Vec<u8>,
    }

    #[derive(Deserialize, Serialize, Debug)]
    struct DummyConcernsOutput {
        items: Vec<String>,
    }

    #[derive(Deserialize, Serialize, Debug)]
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
            Ok(AiResponse {
                content: Some(content),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            0
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

    fn env_with(provider: Arc<MockProvider>, tmp: &tempfile::TempDir) -> WorkflowEnv<'_> {
        WorkflowEnv {
            provider,
            tools: Arc::new(ToolBox::new(tmp.path().to_path_buf(), None)),
            base_dir: tmp.path(),
            context_tag: None,
        }
    }

    fn concerns_stage(name: &'static str) -> Stage<DummyState, DummyConcernsOutput> {
        Stage::builder(name)
            .user_prompt(PromptTemplate::new("Analyze"))
            .output_format(OutputFormat::json())
            .reduce(move |s: &mut DummyState, out: DummyConcernsOutput| {
                for item in out.items {
                    s.concerns.push(format!("{}: {}", name, item));
                }
            })
            .build()
    }

    /// Plans stages 4 and 5, runs them side by side, then consolidates: the
    /// shape of the kernel review, small enough to count every model call.
    fn planned_flow() -> Workflow<DummyState> {
        Workflow::builder("planned_flow")
            .dynamic_parallel(
                Stage::builder("planner")
                    .user_prompt(PromptTemplate::new("Plan stages"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyPlanningOutput| {
                        s.selected_stages = out.stages;
                    })
                    .build(),
                |s| {
                    s.selected_stages
                        .iter()
                        .map(|&n| -> Box<dyn ExecutableStage<DummyState>> {
                            Box::new(concerns_stage(if n == 4 { "stage_4" } else { "stage_5" }))
                        })
                        .collect()
                },
                ParallelPolicy::BestEffort,
            )
            .stage(concerns_stage("consolidate"))
            .build()
    }

    fn saved(entries: &[(&str, serde_json::Value)]) -> HashMap<String, Value> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    /// With every stage saved, a resume asks the model nothing and ends where
    /// the original run did.
    #[tokio::test]
    async fn test_replay_of_every_stage_calls_no_model() {
        let provider = Arc::new(MockProvider::single(r#"{"items": ["fresh"]}"#));
        let tmp = tempfile::tempdir().unwrap();
        let env = env_with(provider.clone(), &tmp);

        let replayed = std::sync::Mutex::new(Vec::new());
        let record = |event: WorkflowEvent| {
            if let WorkflowEvent::StageReplayed { stage_name, .. } = event {
                replayed.lock().unwrap().push(stage_name);
            }
        };
        let mut state = DummyState::default();
        WorkflowEngine::execute_with_replay(
            &planned_flow(),
            &env,
            &mut state,
            Some(&record),
            saved(&[
                ("planner", serde_json::json!({"stages": [4, 5]})),
                ("stage_4", serde_json::json!({"items": ["a"]})),
                ("stage_5", serde_json::json!({"items": ["b"]})),
                ("consolidate", serde_json::json!({"items": ["c"]})),
            ]),
        )
        .await
        .unwrap();

        assert_eq!(provider.call_count.load(Ordering::SeqCst), 0);
        assert_eq!(
            state.concerns,
            vec!["stage_4: a", "stage_5: b", "consolidate: c"]
        );
        let mut replayed = replayed.lock().unwrap().clone();
        replayed.sort();
        assert_eq!(
            replayed,
            vec!["consolidate", "planner", "stage_4", "stage_5"]
        );
    }

    /// The case resume exists for: one analysis stage never finished. It runs,
    /// and everything after it runs too -- the consolidation's saved output
    /// was built without that stage's concerns, so replaying it would quietly
    /// drop them.
    #[tokio::test]
    async fn test_a_stage_that_runs_again_invalidates_everything_after_it() {
        let provider = Arc::new(MockProvider::single(r#"{"items": ["fresh"]}"#));
        let tmp = tempfile::tempdir().unwrap();
        let env = env_with(provider.clone(), &tmp);

        let mut state = DummyState::default();
        WorkflowEngine::execute_with_replay(
            &planned_flow(),
            &env,
            &mut state,
            None,
            saved(&[
                ("planner", serde_json::json!({"stages": [4, 5]})),
                ("stage_4", serde_json::json!({"items": ["a"]})),
                ("consolidate", serde_json::json!({"items": ["stale"]})),
            ]),
        )
        .await
        .unwrap();

        // stage_5 and consolidate; the planner and stage_4 were replayed.
        assert_eq!(provider.call_count.load(Ordering::SeqCst), 2);
        // Replayed and fresh siblings land in stage order, as a full run's do.
        assert_eq!(
            state.concerns,
            vec!["stage_4: a", "stage_5: fresh", "consolidate: fresh"]
        );
    }

    /// A planner that runs again may choose other stages, so the analysis
    /// outputs saved under the old plan are not used either.
    #[tokio::test]
    async fn test_a_planner_that_runs_again_invalidates_the_saved_analysis() {
        let provider = Arc::new(MockProvider::queued(vec![
            r#"{"stages": [4]}"#.to_string(),
            r#"{"items": ["fresh"]}"#.to_string(),
            r#"{"items": ["fresh"]}"#.to_string(),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let env = env_with(provider.clone(), &tmp);

        let mut state = DummyState::default();
        WorkflowEngine::execute_with_replay(
            &planned_flow(),
            &env,
            &mut state,
            None,
            saved(&[("stage_4", serde_json::json!({"items": ["stale"]}))]),
        )
        .await
        .unwrap();

        assert_eq!(provider.call_count.load(Ordering::SeqCst), 3);
        assert_eq!(state.concerns, vec!["stage_4: fresh", "consolidate: fresh"]);
    }

    /// An output saved by a build whose stage produced a different shape is
    /// not forced through: the stage runs, and so does everything after it.
    #[tokio::test]
    async fn test_a_saved_output_that_no_longer_fits_runs_the_stage() {
        let provider = Arc::new(MockProvider::single(r#"{"items": ["fresh"]}"#));
        let tmp = tempfile::tempdir().unwrap();
        let env = env_with(provider.clone(), &tmp);

        let workflow = Workflow::builder("sequential")
            .stage(concerns_stage("first"))
            .stage(concerns_stage("second"))
            .build();
        let mut state = DummyState::default();
        WorkflowEngine::execute_with_replay(
            &workflow,
            &env,
            &mut state,
            None,
            saved(&[
                ("first", serde_json::json!({"renamed_field": ["a"]})),
                ("second", serde_json::json!({"items": ["stale"]})),
            ]),
        )
        .await
        .unwrap();

        assert_eq!(provider.call_count.load(Ordering::SeqCst), 2);
        assert_eq!(state.concerns, vec!["first: fresh", "second: fresh"]);
    }

    /// A finished stage hands out what it produced, parsed -- the same value
    /// its reducer receives, not the model's raw text around it.
    #[tokio::test]
    async fn test_stage_finished_carries_the_parsed_output() {
        let provider = Arc::new(MockProvider::single(
            "Here you go:\n```json\n{\"items\": [\"leak in foo\"]}\n```",
        ));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf(), None));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
        };

        let workflow = Workflow::builder("output_flow")
            .stage(
                Stage::builder("stage_1")
                    .user_prompt(PromptTemplate::new("Analyze"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.concerns.extend(out.items);
                    })
                    .build(),
            )
            .build();

        let outputs = std::sync::Mutex::new(Vec::new());
        let record = |event: WorkflowEvent| {
            if let WorkflowEvent::StageFinished {
                stage_name, output, ..
            } = event
            {
                outputs.lock().unwrap().push((stage_name, output));
            }
        };
        let mut state = DummyState::default();
        WorkflowEngine::execute(&workflow, &env, &mut state, Some(&record))
            .await
            .unwrap();

        assert_eq!(
            *outputs.lock().unwrap(),
            vec![("stage_1", serde_json::json!({"items": ["leak in foo"]}))]
        );
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
}
