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

//! Structured lifecycle telemetry events emitted during workflow execution.

use crate::ai::AiMessage;

use super::engine::StageStopCause;

/// Lifecycle events emitted by the workflow engine.
#[derive(Debug, Clone)]
pub enum WorkflowEvent {
    /// The workflow has begun execution.
    WorkflowStarted { name: &'static str },
    /// A stage has started executing.
    StageStarted { stage_name: &'static str },
    /// A conversational turn has occurred within a stage.
    StageTurn {
        stage_name: &'static str,
        turn: usize,
        max_turns: usize,
    },
    /// A stage started running tools, or finished them (empty `tools`).
    ///
    /// Splits a turn's elapsed time into waiting on the model versus running
    /// git, which are indistinguishable from outside without this.
    StageTools {
        stage_name: &'static str,
        tools: Vec<String>,
        turn: usize,
        max_turns: usize,
    },
    /// A stage is backing off after the provider asked us to slow down.
    /// `retry_in_seconds` is `None` once the wait is over.
    StageBackoff {
        stage_name: &'static str,
        retry_in_seconds: Option<u64>,
        turn: usize,
        max_turns: usize,
    },
    /// A message joined a stage's conversation log.
    ///
    /// Emitted per message rather than per turn, so the newest messages are not
    /// held back until the turn that produced them ends: a turn spent running
    /// a tree-wide grep would otherwise hide the call that started it. The
    /// messages are the ones the stage's finished history will hold, in order.
    StageMessage {
        stage_name: &'static str,
        message: AiMessage,
    },
    /// A stage ended in an error rather than completing.
    ///
    /// The bookend to a stage is `StageFinished`, which is only reached on
    /// success. Without this a failed stage reports nothing at all and its last
    /// turn stays frozen on display, claiming to still be running.
    StageFailed {
        stage_name: &'static str,
        reason: String,
        cause: StageStopCause,
    },
    /// A stage has finished executing.
    StageFinished {
        stage_name: &'static str,
        tokens_in: u32,
        tokens_out: u32,
        tokens_cached: u32,
        /// The review ran out of time while the stage gathered evidence, so
        /// its output came from a forced final turn and covers less than a
        /// full run.
        cut_short: bool,
    },
    /// A dynamic fan-out resolved the stages it will run. Emitted whether or
    /// not the planning stage ran, so a skipped planner still reports a plan.
    ParallelResolved { stage_names: Vec<&'static str> },
    /// An early-exit condition was satisfied.
    EarlyExitTriggered { reason: &'static str },
    /// The entire workflow has completed.
    WorkflowFinished {
        name: &'static str,
        total_tokens: u32,
    },
}
