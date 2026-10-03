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

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use super::{
    AiErrorClass, AiMessage, AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiTool,
    AiUsage, OutputTruncated, ReasoningEffort, ToolCall, WindDownSignal, classify_ai_error,
};

/// The unified result of executing an [`LlmSession`].
pub struct SessionResult<T> {
    /// The validated output of the session.
    pub output: T,
    /// The full conversation history.
    pub history: Vec<AiMessage>,
    /// Accumulated token usage statistics.
    pub usage: AiUsage,
    /// The review ran out of time while this session was gathering evidence,
    /// so its output came from one forced tool-less turn and covers less than
    /// a full run would have.
    pub cut_short: bool,
}

/// Why a session stopped short of its own end.
///
/// Serialised in snake case: the worker reports it as `review_stopped_by` on a
/// patch whose review it ended, and the daemon reads it back from there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopCause {
    /// The review reached its deadline and was told to wind down.
    OutOfTime,
}

impl StopCause {
    fn describe(self) -> &'static str {
        match self {
            Self::OutOfTime => "the review ran out of time",
        }
    }
}

/// A session that stopped because of something outside it, not a fault of
/// its own.
///
/// Typed so the engine and the worker can tell a stage the deadline stopped
/// from one that failed, by `downcast_ref`, which also finds it beneath
/// further context. It is the whole error when the session stopped before
/// spending anything, and the context of the session's own error when the
/// session failed while winding down, so that failure is still put down to
/// the deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("Session stopped: {}", cause.describe())]
pub struct SessionStopped {
    pub cause: StopCause,
}

/// Error returned when an [`LlmSession`] fails, preserving partial conversation
/// history and token usage accumulated before the failure.
#[derive(Debug, Clone)]
pub struct SessionFailure {
    pub message: String,
    pub history: Vec<AiMessage>,
    pub usage: AiUsage,
}

impl std::fmt::Display for SessionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SessionFailure {}

/// Result of validating a session's final response.
#[derive(Debug)]
pub enum ValidationError {
    /// The response was invalid but can be retried.
    /// Contains a feedback message to append to the LLM prompt.
    FormatViolation(String),
    /// A fatal error that cannot be resolved by retrying.
    Fatal(String),
}

/// Action to take upon encountering a provider error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorAction {
    /// Retry the request after appending the feedback message to the prompt history.
    RetryWithFeedback(String),
    /// Abort the session immediately.
    Fail,
}

/// Represents a stateful, task-oriented interaction session with an LLM.
#[async_trait]
pub trait LlmSession: Send {
    /// The final output type returned by the session after validation.
    type Output: Send;

    /// The system prompt guiding the LLM.
    fn system_prompt(&self) -> String;

    /// The initial user prompt.
    fn initial_user_prompt(&self) -> String;

    /// The user prompt to store in history/logs (for space saving).
    /// Defaults to `initial_user_prompt()`.
    fn log_user_prompt(&self) -> String {
        self.initial_user_prompt()
    }

    /// Customizes the validation feedback message.
    fn format_validation_feedback(&self, violation: &str) -> String {
        format!(
            "Previous attempt was rejected: {}. Please correct your output format.",
            violation
        )
    }

    /// Optional list of tools available in this session.
    fn tools(&self) -> Option<Vec<AiTool>> {
        None
    }

    /// Optional temperature override.
    fn temperature(&self) -> Option<f32> {
        None
    }

    /// Optional context tag for logging.
    fn context_tag(&self) -> Option<String> {
        None
    }

    /// Optional expected response format.
    fn response_format(&self) -> Option<AiResponseFormat> {
        None
    }

    /// Optional reasoning effort override for this session.
    fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        None
    }

    /// Executes a tool call requested by the LLM.
    async fn call_tool(&mut self, name: &str, _args: Value) -> Result<Value> {
        anyhow::bail!("Tool execution not implemented for this session: {}", name)
    }

    /// Executes multiple tool calls requested by the LLM.
    /// Default implementation runs them sequentially and formats errors as tool responses.
    async fn call_tools(&mut self, calls: Vec<ToolCall>) -> Result<Vec<(String, Value)>> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let res = match self.call_tool(&call.function_name, call.arguments).await {
                Ok(val) => val,
                Err(e) => serde_json::json!({
                    "error": e.to_string(),
                }),
            };
            results.push((call.id, res));
        }
        Ok(results)
    }

    /// Validates the final response content.
    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError>;

    /// Hook to handle provider errors (e.g. safety blocks, rate limits).
    fn handle_provider_error(&mut self, error: &anyhow::Error, _attempt: usize) -> ErrorAction {
        let err_str = error.to_string();
        if err_str.contains("RECITATION") || err_str.contains("blocked") {
            ErrorAction::RetryWithFeedback(
                "IMPORTANT: Your previous response was blocked by a recitation filter. \
                 Please do NOT copy large blocks of code verbatim in your response. \
                 Describe changes in prose, or use highly simplified pseudo-code if you must show code structure."
                    .to_string(),
            )
        } else {
            ErrorAction::Fail
        }
    }
}

/// Why the runner is forcing a tool-less verdict.
///
/// Once set it stays set for the rest of the session, because a retry that
/// gives back its turn re-enters the final turn. Recomputing it from the turn
/// counter pushed the budget prompt again on every such retry, so the model
/// saw the same instruction stacked up once per rejected answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinalTurn {
    /// The session used up `max_turns`.
    TurnBudget,
    /// The review reached its deadline while this session was gathering
    /// evidence, and the session may be interrupted.
    OutOfTime,
}

/// Sent once when a multi-turn session reaches its last turn.
const TURN_BUDGET_PROMPT: &str = "TURN BUDGET EXHAUSTED: You have reached the maximum allowed investigation turns. Do NOT call any tools. Synthesize your final JSON verdict now based on the evidence gathered so far.";

/// Sent once when the review runs out of time during a session that has
/// already gathered evidence.
const OUT_OF_TIME_PROMPT: &str = "TIME BUDGET EXHAUSTED: The review has run out of time. Do NOT call any tools. Synthesize your final JSON verdict now from the evidence gathered so far.";

impl FinalTurn {
    /// The message that announces this final turn, if any.
    ///
    /// A single-turn session is asked its one question without tools from the
    /// start, so telling it that its budget ran out would only confuse it.
    fn prompt(self, max_turns: usize) -> Option<&'static str> {
        match self {
            Self::TurnBudget => (max_turns > 1).then_some(TURN_BUDGET_PROMPT),
            Self::OutOfTime => Some(OUT_OF_TIME_PROMPT),
        }
    }
}

/// Sent after an answer the provider cut off part way through.
///
/// Asking again unchanged would most likely be cut off again, because the
/// output limit has not moved. The model has to answer differently, so it is
/// told why it is asked again and to be shorter.
const OUTPUT_TRUNCATED_FEEDBACK: &str = "IMPORTANT: Your previous response hit the provider's output limit and was discarded \
     unread. Answer again, shorter. Do not quote long passages of code -- cite file and \
     line and describe what is there. Keep reasoning to the conclusions you have reached. \
     If the work does not fit in one response, do the first part only and continue on the \
     next turn.";

/// Sent after an answer that used the whole output limit before writing
/// anything.
///
/// Asking for a shorter answer misses the point there, because there was no
/// answer. The model has to stop deliberating and act on what it has.
const OUTPUT_TRUNCATED_REASONING_FEEDBACK: &str = "IMPORTANT: Your previous response spent the provider's whole output limit on \
     reasoning before writing anything, and was discarded unread. Do not deliberate \
     further. State the conclusions you have already reached and act on them now: call \
     the next tool, or write the answer.";

/// Truncated answers since the last accepted response.
///
/// Counted per turn rather than per session, so a long stage that is cut off
/// now and then over many turns keeps going: every accepted response starts
/// the count again. `max_turns` still bounds the total.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TurnTruncations {
    /// Truncated answers in a row.
    count: usize,
    /// Whether one of them already got the reasoning feedback.
    reasoning_retried: bool,
}

impl TurnTruncations {
    /// Counts one more truncated answer that is being asked for again.
    fn record(&mut self, truncated: &OutputTruncated) {
        self.count += 1;
        if truncated.spent_on_reasoning() {
            self.reasoning_retried = true;
        }
    }
}

/// Decides what to do about a truncated answer, given the turn's earlier ones.
///
/// An answer that wrote something is asked for again, shorter. One that wrote
/// nothing is asked once to stop deliberating; a second such answer in the
/// same turn fails, because asking again resends the whole history to no
/// purpose, and the error names the setting to change instead.
fn truncation_action(truncated: &OutputTruncated, so_far: &TurnTruncations) -> ErrorAction {
    if !truncated.spent_on_reasoning() {
        ErrorAction::RetryWithFeedback(OUTPUT_TRUNCATED_FEEDBACK.to_string())
    } else if !so_far.reasoning_retried {
        ErrorAction::RetryWithFeedback(OUTPUT_TRUNCATED_REASONING_FEEDBACK.to_string())
    } else {
        ErrorAction::Fail
    }
}

/// The assistant message that records a response in the conversation.
fn assistant_message(resp: &AiResponse) -> AiMessage {
    AiMessage {
        role: AiRole::Assistant,
        content: resp.content.clone(),
        thought: resp.thought.clone(),
        thought_signature: resp.thought_signature.clone(),
        tool_calls: resp.tool_calls.clone(),
        tool_call_id: None,
    }
}

/// Stands in for a final-turn answer that held nothing but ignored tool calls.
const IGNORED_TOOL_CALLS_NOTE: &str =
    "[Tool calls omitted: no tools are run on the final turn, so none were made.]";

/// The copy of an assistant answer that is sent back to the model.
///
/// Tool calls made on the final turn are ignored, so they never get a result.
/// Claude rejects a request whose `tool_use` has no matching `tool_result`
/// (OpenAI does the same), so a validation retry after such an answer failed
/// on the provider instead of reaching the model. The copy therefore drops the
/// calls; the conversation log keeps them. An answer left with no text gets a
/// short note instead, because Claude and Bedrock also reject an assistant
/// message with no content.
fn reply_for_history(answer: &AiMessage, final_turn: Option<FinalTurn>) -> AiMessage {
    let has_tool_calls = answer.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
    if final_turn.is_none() || !has_tool_calls {
        return answer.clone();
    }
    let content = match answer.content.as_deref() {
        Some(text) if !text.is_empty() => text.to_string(),
        _ => IGNORED_TOOL_CALLS_NOTE.to_string(),
    };
    AiMessage {
        content: Some(content),
        tool_calls: None,
        ..answer.clone()
    }
}

/// Fired around tool execution with the tool names, current turn, and turn cap.
/// An empty name slice signals the tools finished.
type ToolsCallback<'a> = dyn Fn(&[String], usize, usize) + Send + Sync + 'a;

/// Fired around provider backoff with the wait in seconds (`None` once over),
/// plus the current turn and turn cap.
type BackoffCallback<'a> = dyn Fn(Option<u64>, usize, usize) + Send + Sync + 'a;

/// Fired with each message as it joins the conversation log.
type MessageCallback<'a> = dyn Fn(&AiMessage) + Send + Sync + 'a;

/// Orchestrates the execution of an [`LlmSession`].
pub struct SessionRunner<'a> {
    provider: &'a dyn AiProvider,
    max_turns: usize,
    max_validation_attempts: usize,
    max_transient_retries: usize,
    max_provider_error_retries: usize,
    on_turn: Option<Box<dyn Fn(usize, usize) + Send + Sync + 'a>>,
    /// Called with the tool names when a turn starts executing tools, and with
    /// an empty slice when they finish.
    on_tools: Option<Box<ToolsCallback<'a>>>,
    /// Called with the backoff duration when the provider asks us to slow down,
    /// and with `None` once the wait is over.
    on_backoff: Option<Box<BackoffCallback<'a>>>,
    /// Called with every message that joins the logged history, in order.
    on_message: Option<Box<MessageCallback<'a>>>,
    /// Set when the review reaches its deadline. `None` where nothing can
    /// send a wind-down: a local review, or a session outside a review.
    wind_down: Option<Arc<WindDownSignal>>,
    /// Whether the wind-down stops this session. The stages that turn
    /// gathered concerns into findings are not interruptible, because a
    /// salvage has nothing to show without them.
    interruptible: bool,
}

impl<'a> SessionRunner<'a> {
    /// Creates a new `SessionRunner` with default limits.
    pub fn new(provider: &'a dyn AiProvider) -> Self {
        Self {
            provider,
            max_turns: 15,
            max_validation_attempts: 3,
            max_transient_retries: 5,
            max_provider_error_retries: 3,
            on_turn: None,
            on_tools: None,
            on_backoff: None,
            on_message: None,
            wind_down: None,
            interruptible: true,
        }
    }

    /// Lets the review's deadline stop this session.
    ///
    /// When `signal` is set and the session is `interruptible`, the session
    /// stops at its next turn boundary: at once if it has had no answer from
    /// the model yet, or else after one forced tool-less turn that turns the
    /// evidence it has gathered into its verdict. A session that is not
    /// interruptible ignores the signal.
    pub fn with_wind_down(
        mut self,
        signal: Option<Arc<WindDownSignal>>,
        interruptible: bool,
    ) -> Self {
        self.wind_down = signal;
        self.interruptible = interruptible;
        self
    }

    /// Whether this session has been told to wind down.
    fn winding_down(&self) -> bool {
        self.interruptible
            && self
                .wind_down
                .as_ref()
                .is_some_and(|signal| signal.is_requested())
    }

    /// Acts on the wind-down at a turn boundary.
    ///
    /// Returns `Err(SessionStopped)` when the session has had no answer from
    /// the model, because there is no evidence to synthesise and stopping now
    /// spends nothing. Otherwise switches the session to its out-of-time
    /// final turn, once. A session already on its turn-budget final turn is
    /// left there: it is asking for its verdict already, and the evidence it
    /// gathered was not cut short.
    fn check_wind_down(
        &self,
        answered: bool,
        final_turn: &mut Option<FinalTurn>,
        history: &mut Vec<AiMessage>,
        log_history: &mut Vec<AiMessage>,
    ) -> Result<()> {
        if final_turn.is_some() || !self.winding_down() {
            return Ok(());
        }
        if !answered {
            return Err(SessionStopped {
                cause: StopCause::OutOfTime,
            }
            .into());
        }
        tracing::info!("The review ran out of time; asking the stage for its verdict now");
        *final_turn = Some(FinalTurn::OutOfTime);
        self.announce_final_turn(FinalTurn::OutOfTime, history, log_history);
        Ok(())
    }

    /// Configures the maximum validation retries.
    pub fn with_max_validation_attempts(mut self, attempts: usize) -> Self {
        self.max_validation_attempts = attempts;
        self
    }

    /// Configures the maximum conversational turns.
    pub fn with_max_turns(mut self, turns: usize) -> Self {
        self.max_turns = turns;
        self
    }

    /// Configures the maximum transient and rate-limit retries.
    pub fn with_max_transient_retries(mut self, retries: usize) -> Self {
        self.max_transient_retries = retries;
        self
    }

    /// Configures the maximum provider error retries.
    pub fn with_max_provider_error_retries(mut self, retries: usize) -> Self {
        self.max_provider_error_retries = retries;
        self
    }

    /// Configures a callback fired around tool execution.
    ///
    /// A turn's elapsed time is split between waiting on the model and running
    /// git; without this the two are indistinguishable from outside, which is
    /// the difference between "the model is slow" and "a tree-wide grep is
    /// chewing through the kernel".
    pub fn with_tools_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(&[String], usize, usize) + Send + Sync + 'a,
    {
        self.on_tools = Some(Box::new(cb));
        self
    }

    /// Configures a callback fired around rate-limit and transient backoff.
    ///
    /// A rate limit with no `Retry-After` header sleeps a flat 60s, which from
    /// outside is indistinguishable from a very slow model -- and calls for a
    /// completely different response. In a daemon-spawned worker this path is
    /// rare, because the daemon's `BackoffProvider` retries before the error
    /// ever reaches the worker; it still fires when one is relayed.
    pub fn with_backoff_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(Option<u64>, usize, usize) + Send + Sync + 'a,
    {
        self.on_backoff = Some(Box::new(cb));
        self
    }

    /// Configures a callback fired with each message as it joins the
    /// conversation log.
    ///
    /// The finished log is only stored once the whole review ends, which for a
    /// long review is hours after the conversation it records. This lets a
    /// caller stream the same messages while they happen. The callback sees
    /// exactly what the returned history will hold, in the same order.
    pub fn with_message_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(&AiMessage) + Send + Sync + 'a,
    {
        self.on_message = Some(Box::new(cb));
        self
    }

    /// Adds a message to the logged history, reporting it first.
    ///
    /// Every push into the log goes through here. A message that reached the
    /// log without the callback is exactly how the streamed conversation and
    /// the finished one drifted apart before: the streamed copy skipped the
    /// initial prompt and the turn-budget prompt, so the two disagreed about
    /// what the model had been told.
    fn push_log(&self, log_history: &mut Vec<AiMessage>, msg: AiMessage) {
        if let Some(ref cb) = self.on_message {
            cb(&msg);
        }
        log_history.push(msg);
    }

    /// Tells the model, in both histories, that it is on its final turn.
    ///
    /// The caller invokes this only when the session first enters its final
    /// turn, so the prompt appears once however many retries follow.
    fn announce_final_turn(
        &self,
        final_turn: FinalTurn,
        history: &mut Vec<AiMessage>,
        log_history: &mut Vec<AiMessage>,
    ) {
        let Some(prompt) = final_turn.prompt(self.max_turns) else {
            return;
        };
        self.push_feedback(history, log_history, prompt.to_string());
    }

    /// Adds a user message to both the conversation and its log.
    ///
    /// Every message the runner adds for the model goes to both, so the log
    /// says why the model answered the same question again.
    fn push_feedback(
        &self,
        history: &mut Vec<AiMessage>,
        log_history: &mut Vec<AiMessage>,
        text: String,
    ) {
        let msg = AiMessage {
            role: AiRole::User,
            content: Some(text),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        };
        history.push(msg.clone());
        self.push_log(log_history, msg);
    }

    /// Handles an answer the provider cut off, returning `Ok` to ask again.
    ///
    /// The answer goes to the log only. The log then shows what was cut, and
    /// the model never sees a half-written tool call, whose arguments may not
    /// even parse. On `Ok` the feedback is in both histories and the caller
    /// gives the turn back, because a discarded answer should not spend the
    /// stage's turn cap.
    fn retry_truncated(
        &self,
        resp: &AiResponse,
        so_far: &mut TurnTruncations,
        history: &mut Vec<AiMessage>,
        log_history: &mut Vec<AiMessage>,
    ) -> Result<()> {
        self.push_log(log_history, assistant_message(resp));
        let truncated = OutputTruncated::from_response(resp);
        let ErrorAction::RetryWithFeedback(feedback) = truncation_action(&truncated, so_far) else {
            return Err(truncated.into());
        };
        so_far.record(&truncated);
        if so_far.count > self.max_provider_error_retries {
            anyhow::bail!(
                "Session failed after {} truncated answers in one turn. Last error: {}",
                so_far.count,
                truncated
            );
        }
        tracing::warn!(
            "{}; asking again (truncated answer {}/{} this turn)",
            truncated,
            so_far.count,
            self.max_provider_error_retries
        );
        self.push_feedback(history, log_history, feedback);
        Ok(())
    }

    /// Configures a turn callback.
    pub fn with_turn_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(usize, usize) + Send + Sync + 'a,
    {
        self.on_turn = Some(Box::new(cb));
        self
    }

    /// Runs the session to completion. Returns the validated output and conversation history (for logging).
    ///
    /// A session that fails on its out-of-time final turn has its error
    /// wrapped in [`SessionStopped`], so the stop is still put down to the
    /// deadline rather than read as a fault of the stage.
    pub async fn run<S>(&self, session: &mut S) -> Result<SessionResult<S::Output>>
    where
        S: LlmSession,
    {
        let mut final_turn = None;
        let result = self.run_turns(session, &mut final_turn).await;
        match (result, final_turn) {
            (Err(e), Some(FinalTurn::OutOfTime))
                if e.downcast_ref::<SessionStopped>().is_none() =>
            {
                Err(e.context(SessionStopped {
                    cause: StopCause::OutOfTime,
                }))
            }
            (result, _) => result,
        }
    }

    /// The turn loop of [`Self::run`]. `final_turn` is the caller's, so the
    /// caller can tell how the loop ended when it returns an error.
    async fn run_turns<S>(
        &self,
        session: &mut S,
        final_turn: &mut Option<FinalTurn>,
    ) -> Result<SessionResult<S::Output>>
    where
        S: LlmSession,
    {
        let mut history = vec![AiMessage {
            role: AiRole::User,
            content: Some(session.initial_user_prompt()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }];

        let mut log_history = Vec::new();
        self.push_log(
            &mut log_history,
            AiMessage {
                role: AiRole::User,
                content: Some(session.log_user_prompt()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            },
        );

        let mut turns: usize = 0;
        let mut validation_attempts = 0;
        // Requests whose answers were rejected, forgotten only if the stage
        // gives up.
        let mut rejected: Vec<AiRequest> = Vec::new();
        let mut transient_retries: usize = 0;
        let mut provider_error_retries = 0;
        let mut total_prompt_tokens = 0;
        let mut total_completion_tokens = 0;
        let mut total_cached_tokens = 0;
        let mut turn_truncations = TurnTruncations::default();
        // Whether the model has answered at all, which decides what the
        // wind-down does: there is nothing to synthesise before the first
        // answer.
        let mut answered = false;

        let make_failure = |message: String,
                            history: Vec<AiMessage>,
                            prompt_tokens: usize,
                            completion_tokens: usize,
                            cached_tokens: usize|
         -> anyhow::Error {
            anyhow::Error::new(SessionFailure {
                message,
                history,
                usage: AiUsage {
                    prompt_tokens,
                    completion_tokens,
                    total_tokens: prompt_tokens.saturating_add(completion_tokens),
                    cached_tokens: Some(cached_tokens),
                },
            })
        };

        loop {
            // Wrapped like every other early exit, so the stage keeps the
            // history and usage it had; the typed SessionStopped still
            // downcasts through the context.
            if let Err(e) =
                self.check_wind_down(answered, final_turn, &mut history, &mut log_history)
            {
                let message = e.to_string();
                return Err(e.context(SessionFailure {
                    message,
                    history: log_history,
                    usage: AiUsage {
                        prompt_tokens: total_prompt_tokens,
                        completion_tokens: total_completion_tokens,
                        total_tokens: total_prompt_tokens.saturating_add(total_completion_tokens),
                        cached_tokens: Some(total_cached_tokens),
                    },
                }));
            }
            turns = turns.saturating_add(1);
            if turns > self.max_turns {
                return Err(make_failure(
                    format!("Session exceeded max turns limit ({})", self.max_turns),
                    log_history,
                    total_prompt_tokens,
                    total_completion_tokens,
                    total_cached_tokens,
                ));
            }
            if let Some(ref cb) = self.on_turn {
                cb(turns, self.max_turns);
            }

            if final_turn.is_none() && turns == self.max_turns {
                *final_turn = Some(FinalTurn::TurnBudget);
                self.announce_final_turn(FinalTurn::TurnBudget, &mut history, &mut log_history);
            }
            let is_final_turn = final_turn.is_some();

            let tools = if is_final_turn { None } else { session.tools() };

            let request = AiRequest {
                system: Some(session.system_prompt()),
                messages: history.clone(),
                tools,
                temperature: session.temperature(),
                response_format: session.response_format(),
                context_tag: session.context_tag(),
                reasoning_effort: session.reasoning_effort(),
            };

            // Kept so that a rejected answer can be forgotten below.
            let sent = request.clone();
            let resp = match self.provider.generate_content(request).await {
                Ok(r) => r,
                Err(e) => match classify_ai_error(&e) {
                    AiErrorClass::RateLimit { retry_after }
                    | AiErrorClass::Transient { retry_after } => {
                        transient_retries = transient_retries.saturating_add(1);
                        if transient_retries > self.max_transient_retries {
                            return Err(make_failure(
                                format!(
                                    "Session failed after {} transient/rate-limit errors. Last error: {}",
                                    self.max_transient_retries, e
                                ),
                                log_history,
                                total_prompt_tokens,
                                total_completion_tokens,
                                total_cached_tokens,
                            ));
                        }
                        tracing::warn!(
                            "API error ({}), pausing for {:?} before retry (attempt {}/{})...",
                            e,
                            retry_after,
                            transient_retries,
                            self.max_transient_retries
                        );
                        if let Some(ref cb) = self.on_backoff {
                            cb(
                                Some(crate::activity::retry_in_seconds(retry_after)),
                                turns,
                                self.max_turns,
                            );
                        }
                        tokio::time::sleep(retry_after).await;
                        if let Some(ref cb) = self.on_backoff {
                            cb(None, turns, self.max_turns);
                        }
                        turns = turns.saturating_sub(1);
                        continue;
                    }
                    AiErrorClass::Fatal => {
                        match session.handle_provider_error(&e, provider_error_retries) {
                            ErrorAction::RetryWithFeedback(feedback) => {
                                provider_error_retries = provider_error_retries.saturating_add(1);
                                if provider_error_retries > self.max_provider_error_retries {
                                    return Err(make_failure(
                                        format!(
                                            "Session failed after {} provider error retries. Last error: {}",
                                            self.max_provider_error_retries, e
                                        ),
                                        log_history,
                                        total_prompt_tokens,
                                        total_completion_tokens,
                                        total_cached_tokens,
                                    ));
                                }
                                self.push_feedback(&mut history, &mut log_history, feedback);
                                turns = turns.saturating_sub(1);
                                continue;
                            }
                            ErrorAction::Fail => {
                                let message = e.to_string();
                                return Err(e.context(SessionFailure {
                                    message,
                                    history: log_history,
                                    usage: AiUsage {
                                        prompt_tokens: total_prompt_tokens,
                                        completion_tokens: total_completion_tokens,
                                        total_tokens: total_prompt_tokens
                                            .saturating_add(total_completion_tokens),
                                        cached_tokens: Some(total_cached_tokens),
                                    },
                                }));
                            }
                        }
                    }
                },
            };

            answered = true;
            // A truncated answer was billed like any other, so its usage counts
            // even though the answer itself is discarded.
            if let Some(usage) = &resp.usage {
                total_prompt_tokens = total_prompt_tokens.saturating_add(usage.prompt_tokens);
                total_completion_tokens =
                    total_completion_tokens.saturating_add(usage.completion_tokens);
                total_cached_tokens =
                    total_cached_tokens.saturating_add(usage.cached_tokens.unwrap_or(0));
            }

            // The request succeeded and the conversation so far is intact, so
            // a cut-off answer is asked for again instead of ending the stage
            // and losing every turn it has already paid for.
            if resp.truncated {
                if let Err(e) = self.retry_truncated(
                    &resp,
                    &mut turn_truncations,
                    &mut history,
                    &mut log_history,
                ) {
                    // Giving up, as when the answers are rejected below: forget
                    // this cut-off answer and any the stage rejected before it,
                    // so that a retry asks the model again. Wrapped rather than
                    // replaced, so the typed OutputTruncated cause still
                    // downcasts while the partial history and usage survive.
                    for request in rejected.iter().chain(std::iter::once(&sent)) {
                        self.provider.forget(request).await;
                    }
                    let message = e.to_string();
                    return Err(e.context(SessionFailure {
                        message,
                        history: log_history,
                        usage: AiUsage {
                            prompt_tokens: total_prompt_tokens,
                            completion_tokens: total_completion_tokens,
                            total_tokens: total_prompt_tokens
                                .saturating_add(total_completion_tokens),
                            cached_tokens: Some(total_cached_tokens),
                        },
                    }));
                }
                turns = turns.saturating_sub(1);
                continue;
            }
            turn_truncations = TurnTruncations::default();

            let assistant_msg = assistant_message(&resp);
            history.push(reply_for_history(&assistant_msg, *final_turn));
            self.push_log(&mut log_history, assistant_msg);

            // Handle Tool Calls
            if let Some(tool_calls) = &resp.tool_calls
                && !tool_calls.is_empty()
            {
                if is_final_turn {
                    tracing::warn!(
                        "Model emitted tool calls on final turn; ignoring tools to force validation."
                    );
                } else {
                    if let Some(ref cb) = self.on_tools {
                        let names: Vec<String> =
                            tool_calls.iter().map(|c| c.function_name.clone()).collect();
                        cb(&names, turns, self.max_turns);
                    }
                    // The error is held until the "finished" report has gone
                    // out, so a failing tool batch cannot leave the stage
                    // reading as still running tools.
                    let results = session.call_tools(tool_calls.clone()).await;
                    if let Some(ref cb) = self.on_tools {
                        cb(&[], turns, self.max_turns);
                    }
                    let results = match results {
                        Ok(r) => r,
                        Err(e) => {
                            let message = e.to_string();
                            return Err(e.context(SessionFailure {
                                message,
                                history: log_history,
                                usage: AiUsage {
                                    prompt_tokens: total_prompt_tokens,
                                    completion_tokens: total_completion_tokens,
                                    total_tokens: total_prompt_tokens
                                        .saturating_add(total_completion_tokens),
                                    cached_tokens: Some(total_cached_tokens),
                                },
                            }));
                        }
                    };
                    for (call_id, result) in results {
                        let tool_msg = AiMessage {
                            role: AiRole::Tool,
                            content: Some(result.to_string()),
                            thought: None,
                            thought_signature: None,
                            tool_calls: None,
                            tool_call_id: Some(call_id),
                        };
                        history.push(tool_msg.clone());
                        self.push_log(&mut log_history, tool_msg);
                    }
                    continue; // Loop again to feed tool results back to LLM
                }
            }

            // No tool calls: validate response
            match session.validate(&resp) {
                Result::Ok(output) => {
                    let usage = AiUsage {
                        prompt_tokens: total_prompt_tokens,
                        completion_tokens: total_completion_tokens,
                        total_tokens: total_prompt_tokens.saturating_add(total_completion_tokens),
                        cached_tokens: Some(total_cached_tokens),
                    };
                    return Ok(SessionResult {
                        output,
                        history: log_history,
                        usage,
                        cut_short: *final_turn == Some(FinalTurn::OutOfTime),
                    });
                }
                Result::Err(ValidationError::FormatViolation(violation)) => {
                    rejected.push(sent);
                    validation_attempts += 1;
                    if validation_attempts >= self.max_validation_attempts {
                        // Giving up: forget the rejected answers, or a
                        // retry would be handed them and fail the same way.
                        for request in &rejected {
                            self.provider.forget(request).await;
                        }
                        return Err(make_failure(
                            format!(
                                "Failed to generate valid response after {} validation attempts. Last violation: {}",
                                self.max_validation_attempts, violation
                            ),
                            log_history,
                            total_prompt_tokens,
                            total_completion_tokens,
                            total_cached_tokens,
                        ));
                    }
                    let feedback = session.format_validation_feedback(&violation);
                    self.push_feedback(&mut history, &mut log_history, feedback);
                    turns = turns.saturating_sub(1);
                }
                Result::Err(ValidationError::Fatal(err)) => {
                    return Err(make_failure(
                        format!("Fatal validation error: {}", err),
                        log_history,
                        total_prompt_tokens,
                        total_completion_tokens,
                        total_cached_tokens,
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ProviderCapabilities;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct MockProvider {
        responses: Mutex<VecDeque<AiResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<AiResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait]
    impl AiProvider for MockProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            let mut q = self.responses.lock().unwrap();
            q.pop_front()
                .ok_or_else(|| anyhow::anyhow!("No more mock responses"))
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 4096,
            }
        }
    }

    struct DummySession;

    #[async_trait]
    impl LlmSession for DummySession {
        type Output = String;

        fn system_prompt(&self) -> String {
            "system".to_string()
        }

        fn initial_user_prompt(&self) -> String {
            "initial prompt".to_string()
        }

        async fn call_tool(&mut self, name: &str, _args: Value) -> Result<Value> {
            if name == "fail" {
                anyhow::bail!("Tool execution failed");
            }
            Ok(serde_json::json!({"result": "success"}))
        }

        fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
            Ok(response.content.clone().unwrap_or_default())
        }
    }

    #[tokio::test]
    async fn test_call_tools_captures_errors_as_json() {
        let mut session = DummySession;
        let calls = vec![
            ToolCall {
                id: "call_1".to_string(),
                function_name: "ok_tool".to_string(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            },
            ToolCall {
                id: "call_2".to_string(),
                function_name: "fail".to_string(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            },
        ];

        let results = session.call_tools(calls).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "call_1");
        assert_eq!(results[0].1, serde_json::json!({"result": "success"}));
        assert_eq!(results[1].0, "call_2");
        assert_eq!(
            results[1].1,
            serde_json::json!({"error": "Tool execution failed"})
        );
    }

    /// Fails its first request with a sub-second transient error, then
    /// answers.
    struct BriefOutageProvider {
        failed: Mutex<bool>,
    }

    #[async_trait]
    impl AiProvider for BriefOutageProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            let mut failed = self.failed.lock().unwrap();
            if !*failed {
                *failed = true;
                return Err(crate::ai::RemoteAiError {
                    message: "upstream hiccup".to_string(),
                    class: AiErrorClass::Transient {
                        retry_after: std::time::Duration::from_millis(50),
                    },
                }
                .into());
            }
            Ok(AiResponse {
                content: Some("answered".to_string()),
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
                context_window_size: 4096,
            }
        }
    }

    /// The daemon reports its own backoffs rounded up; the worker's must agree,
    /// or a sub-second retry reaches the live display as "retrying in 0s".
    #[tokio::test]
    async fn a_sub_second_backoff_is_reported_as_a_whole_second() {
        let provider = BriefOutageProvider {
            failed: Mutex::new(false),
        };
        let reported: Mutex<Vec<Option<u64>>> = Mutex::new(Vec::new());
        let runner = SessionRunner::new(&provider)
            .with_backoff_callback(|secs, _, _| reported.lock().unwrap().push(secs));
        let res = runner.run(&mut DummySession).await.unwrap();
        assert_eq!(res.output, "answered");
        assert_eq!(*reported.lock().unwrap(), vec![Some(1), None]);
    }

    #[tokio::test]
    async fn test_session_runner_survives_tool_error() {
        let responses = vec![
            AiResponse {
                content: None,
                thought: None,
                thought_signature: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_err".to_string(),
                    function_name: "fail".to_string(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                }]),
                usage: None,
                truncated: false,
            },
            AiResponse {
                content: Some("Recovered after tool error".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            },
        ];

        let provider = MockProvider::new(responses);
        let runner = SessionRunner::new(&provider);
        let mut session = DummySession;

        let res = runner.run(&mut session).await.unwrap();
        assert_eq!(res.output, "Recovered after tool error");

        // Verify history contains the error response for the tool
        let tool_msg = res.history.iter().find(|m| m.role == AiRole::Tool).unwrap();
        assert_eq!(tool_msg.tool_call_id, Some("call_err".to_string()));
        assert!(
            tool_msg
                .content
                .as_ref()
                .unwrap()
                .contains("Tool execution failed")
        );
    }

    #[tokio::test]
    async fn test_session_runner_forces_synthesis_on_max_turns() {
        let responses = vec![
            // Turn 1: tool call
            AiResponse {
                content: None,
                thought: None,
                thought_signature: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    function_name: "ok_tool".to_string(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                }]),
                usage: None,
                truncated: false,
            },
            // Turn 2 (max turns): synthesized output
            AiResponse {
                content: Some("Final synthesized verdict".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            },
        ];

        let provider = MockProvider::new(responses);
        let runner = SessionRunner::new(&provider).with_max_turns(2);
        let mut session = DummySession;

        let res = runner.run(&mut session).await.unwrap();
        assert_eq!(res.output, "Final synthesized verdict");

        // Verify history contains the budget exhausted user message
        let exhausted_msg = res.history.iter().find(|m| {
            m.role == AiRole::User
                && m.content
                    .as_deref()
                    .unwrap_or("")
                    .contains("TURN BUDGET EXHAUSTED")
        });
        assert!(exhausted_msg.is_some());
    }

    /// Logs a prompt that differs from what the model is sent, and rejects its
    /// first answer, so every kind of logged message turns up once.
    struct StreamedSession {
        rejected: bool,
    }

    #[async_trait]
    impl LlmSession for StreamedSession {
        type Output = String;

        fn system_prompt(&self) -> String {
            "system".to_string()
        }

        fn initial_user_prompt(&self) -> String {
            "prompt with the whole diff".to_string()
        }

        fn log_user_prompt(&self) -> String {
            "prompt as logged".to_string()
        }

        async fn call_tool(&mut self, _name: &str, _args: Value) -> Result<Value> {
            Ok(serde_json::json!({"content": "file"}))
        }

        fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
            if !self.rejected {
                self.rejected = true;
                return Err(ValidationError::FormatViolation("not json".to_string()));
            }
            Ok(response.content.clone().unwrap_or_default())
        }
    }

    /// The streamed conversation is the finished log, message for message.
    /// The live view and the finished view drifted apart once because some
    /// messages reached the log without being streamed.
    #[tokio::test]
    async fn every_logged_message_is_streamed_in_order() {
        let answer = |content: &str| AiResponse {
            content: Some(content.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage: None,
            truncated: false,
        };
        let provider = MockProvider::new(vec![
            AiResponse {
                content: None,
                thought: None,
                thought_signature: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    function_name: "git_grep".to_string(),
                    arguments: serde_json::json!({"pattern": "x"}),
                    thought_signature: None,
                }]),
                usage: None,
                truncated: false,
            },
            answer("first try"),
            answer("second try"),
        ]);
        let streamed: Mutex<Vec<AiMessage>> = Mutex::new(Vec::new());
        // Two turns, so the turn-budget prompt is logged as well.
        let runner = SessionRunner::new(&provider)
            .with_max_turns(2)
            .with_message_callback(|m| streamed.lock().unwrap().push(m.clone()));

        let res = runner
            .run(&mut StreamedSession { rejected: false })
            .await
            .unwrap();
        assert_eq!(res.output, "second try");

        drop(runner);
        let streamed = streamed.into_inner().unwrap();
        assert_eq!(streamed, res.history);
        assert_eq!(
            streamed[0].content.as_deref(),
            Some("prompt as logged"),
            "the stream opens with the logged prompt, as the finished log does"
        );
        let roles: Vec<AiRole> = streamed.iter().map(|m| m.role.clone()).collect();
        assert!(roles.contains(&AiRole::Tool));
        assert!(streamed.iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.contains("TURN BUDGET EXHAUSTED"))
        }));
        assert!(
            streamed.iter().filter(|m| m.role == AiRole::User).count() >= 3,
            "the prompt, the budget prompt and the validation feedback"
        );
    }

    /// Answers from a script and keeps every request it was sent.
    struct RecordingProvider {
        responses: Mutex<VecDeque<AiResponse>>,
        requests: Mutex<Vec<AiRequest>>,
    }

    impl RecordingProvider {
        fn new(responses: Vec<AiResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<AiRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AiProvider for RecordingProvider {
        async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("No more mock responses"))
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 4096,
            }
        }
    }

    /// Offers one tool and rejects its first `rejections` answers.
    struct RejectingToolSession {
        rejections: usize,
    }

    #[async_trait]
    impl LlmSession for RejectingToolSession {
        type Output = String;

        fn system_prompt(&self) -> String {
            "system".to_string()
        }

        fn initial_user_prompt(&self) -> String {
            "initial prompt".to_string()
        }

        fn tools(&self) -> Option<Vec<AiTool>> {
            Some(vec![AiTool {
                name: "git_grep".to_string(),
                description: "search".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }])
        }

        async fn call_tool(&mut self, _name: &str, _args: Value) -> Result<Value> {
            Ok(serde_json::json!({"content": "file"}))
        }

        fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
            if self.rejections > 0 {
                self.rejections -= 1;
                return Err(ValidationError::FormatViolation("not json".to_string()));
            }
            Ok(response.content.clone().unwrap_or_default())
        }
    }

    fn text_answer(content: &str) -> AiResponse {
        AiResponse {
            content: Some(content.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage: None,
            truncated: false,
        }
    }

    fn tool_answer(id: &str) -> AiResponse {
        AiResponse {
            content: None,
            thought: None,
            thought_signature: None,
            tool_calls: Some(vec![ToolCall {
                id: id.to_string(),
                function_name: "git_grep".to_string(),
                arguments: serde_json::json!({"pattern": "x"}),
                thought_signature: None,
            }]),
            usage: None,
            truncated: false,
        }
    }

    fn budget_prompts(messages: &[AiMessage]) -> usize {
        messages
            .iter()
            .filter(|m| m.content.as_deref() == Some(TURN_BUDGET_PROMPT))
            .count()
    }

    /// A rejected answer on the final turn gives the turn back, which used to
    /// re-enter the final-turn block and push the budget prompt once more for
    /// every rejection.
    #[tokio::test]
    async fn a_validation_retry_on_the_final_turn_pushes_the_budget_prompt_once() {
        let provider = RecordingProvider::new(vec![
            tool_answer("call_1"),
            text_answer("first try"),
            text_answer("second try"),
            text_answer("third try"),
        ]);
        let runner = SessionRunner::new(&provider).with_max_turns(2);

        let res = runner
            .run(&mut RejectingToolSession { rejections: 2 })
            .await
            .unwrap();
        assert_eq!(res.output, "third try");
        assert_eq!(budget_prompts(&res.history), 1);

        let requests = provider.requests();
        assert_eq!(requests.len(), 4);
        assert!(requests[0].tools.is_some(), "turn 1 may still call tools");
        for (i, req) in requests.iter().enumerate().skip(1) {
            assert!(req.tools.is_none(), "final-turn request {i} offered tools");
            assert_eq!(
                budget_prompts(&req.messages),
                1,
                "final-turn request {i} must carry the budget prompt exactly once"
            );
        }
    }

    /// A single-turn stage is asked its one question without tools, and is
    /// never told that a budget it never had has run out.
    #[tokio::test]
    async fn a_single_turn_session_gets_no_tools_and_no_budget_prompt() {
        let provider =
            RecordingProvider::new(vec![text_answer("rejected"), text_answer("verdict")]);
        let runner = SessionRunner::new(&provider).with_max_turns(1);

        let res = runner
            .run(&mut RejectingToolSession { rejections: 1 })
            .await
            .unwrap();
        assert_eq!(res.output, "verdict");
        assert_eq!(budget_prompts(&res.history), 0);

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        for req in &requests {
            assert!(req.tools.is_none());
            assert_eq!(budget_prompts(&req.messages), 0);
        }
    }

    /// The ids of tool calls in `messages` that no later tool message answers.
    fn unanswered_tool_calls(messages: &[AiMessage]) -> Vec<String> {
        messages
            .iter()
            .enumerate()
            .flat_map(|(i, m)| {
                m.tool_calls
                    .iter()
                    .flatten()
                    .map(move |call| (i, call.id.clone()))
            })
            .filter(|(i, id)| {
                !messages[i + 1..]
                    .iter()
                    .any(|m| m.role == AiRole::Tool && m.tool_call_id.as_deref() == Some(id))
            })
            .map(|(_, id)| id)
            .collect()
    }

    /// Tool calls on the final turn are ignored and never answered. Sending
    /// them back on a validation retry gave the provider a `tool_use` with no
    /// `tool_result`, which Claude rejects, so the retry never reached the
    /// model.
    #[tokio::test]
    async fn a_final_turn_retry_sends_no_unanswered_tool_calls() {
        let provider = RecordingProvider::new(vec![
            tool_answer("call_1"),
            tool_answer("call_2"),
            text_answer("verdict"),
        ]);
        let runner = SessionRunner::new(&provider).with_max_turns(2);

        let res = runner
            .run(&mut RejectingToolSession { rejections: 1 })
            .await
            .unwrap();
        assert_eq!(res.output, "verdict");

        let requests = provider.requests();
        assert_eq!(requests.len(), 3);
        for (i, req) in requests.iter().enumerate() {
            assert!(
                unanswered_tool_calls(&req.messages).is_empty(),
                "request {i} carries a tool call with no result"
            );
        }
        let retried = requests[2]
            .messages
            .iter()
            .rfind(|m| m.role == AiRole::Assistant)
            .unwrap();
        assert_eq!(
            retried.content.as_deref(),
            Some(IGNORED_TOOL_CALLS_NOTE),
            "an answer left empty must still have content for the provider"
        );
        assert_eq!(
            unanswered_tool_calls(&res.history),
            vec!["call_2".to_string()],
            "the log keeps the ignored call"
        );
    }

    /// Text that came with ignored final-turn tool calls is what the model
    /// said, so its copy in the history keeps it; only the calls go.
    #[test]
    fn a_final_turn_reply_keeps_its_text_and_drops_its_tool_calls() {
        let answer = AiMessage {
            role: AiRole::Assistant,
            content: Some("partial verdict".to_string()),
            thought: Some("thinking".to_string()),
            thought_signature: Some("sig".to_string()),
            tool_calls: tool_answer("call_1").tool_calls,
            tool_call_id: None,
        };

        let copy = reply_for_history(&answer, Some(FinalTurn::TurnBudget));
        assert_eq!(copy.content.as_deref(), Some("partial verdict"));
        assert_eq!(copy.tool_calls, None);
        assert_eq!(copy.thought, answer.thought);
        assert_eq!(copy.thought_signature, answer.thought_signature);

        assert_eq!(
            reply_for_history(&answer, None),
            answer,
            "before the final turn the calls are run and answered, so they stay"
        );
    }

    /// A response cut off after writing `content`, billed `completion` tokens.
    fn truncated_answer(content: Option<&str>, completion: usize) -> AiResponse {
        AiResponse {
            content: content.map(str::to_string),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage: Some(AiUsage {
                prompt_tokens: 10,
                completion_tokens: completion,
                total_tokens: 10 + completion,
                cached_tokens: None,
            }),
            truncated: true,
        }
    }

    fn last_user_text(request: &AiRequest) -> String {
        request
            .messages
            .last()
            .and_then(|m| m.content.clone())
            .unwrap_or_default()
    }

    /// A stage used to die outright when the provider cut its answer short.
    /// Three stages of one patch were lost that way in a single review, each
    /// after about an hour of tool calls.
    #[tokio::test]
    async fn a_truncated_answer_is_asked_again_rather_than_ending_the_session() {
        let provider = RecordingProvider::new(vec![
            truncated_answer(Some("half a"), 50),
            text_answer("done"),
        ]);

        let res = SessionRunner::new(&provider)
            .run(&mut DummySession)
            .await
            .expect("a truncated answer must not end the session");
        assert_eq!(
            res.output, "done",
            "the retry's answer is the one that counts"
        );

        // Asking again unchanged would most likely be cut off again, because
        // the output limit has not moved, so the request has to change.
        let requests = provider.requests();
        assert_eq!(requests.len(), 2, "it has to actually ask again");
        assert_ne!(requests[0].messages, requests[1].messages);
        assert_eq!(last_user_text(&requests[1]), OUTPUT_TRUNCATED_FEEDBACK);
    }

    /// A model that keeps overrunning still has to stop somewhere, and the
    /// error has to say both which bound ran out and what was cut.
    #[tokio::test]
    async fn truncation_retries_are_bounded_and_say_what_ran_out() {
        let provider = RecordingProvider::new(
            (0..10)
                .map(|_| truncated_answer(Some("half a"), 50))
                .collect(),
        );

        let err = match SessionRunner::new(&provider)
            .with_max_provider_error_retries(2)
            .run(&mut DummySession)
            .await
        {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a provider that always truncates cannot succeed"),
        };

        assert!(
            err.contains("3 truncated answers in one turn"),
            "the reason must name the bound that ran out: {err}"
        );
        assert!(
            err.contains("truncated by provider") && err.contains("6 chars of answer"),
            "the underlying cause must still be reported: {err}"
        );
        assert_eq!(provider.requests().len(), 3, "one attempt plus two retries");
    }

    /// The turn cap pays for answers, and a discarded answer is not one.
    #[tokio::test]
    async fn a_discarded_turn_is_not_charged_against_the_turn_cap() {
        let provider = RecordingProvider::new(vec![
            truncated_answer(Some("half a"), 50),
            truncated_answer(Some("half b"), 50),
            text_answer("done"),
        ]);

        let res = SessionRunner::new(&provider)
            .with_max_turns(1)
            .run(&mut DummySession)
            .await;

        assert!(
            res.is_ok(),
            "two truncated answers must not use up a one-turn budget: {:?}",
            res.err()
        );
        assert_eq!(provider.requests().len(), 3);
    }

    /// A truncated final turn gives its turn back and re-enters the final
    /// turn, which must not announce the budget a second time.
    #[tokio::test]
    async fn a_truncated_final_turn_pushes_the_budget_prompt_once() {
        let provider = RecordingProvider::new(vec![
            tool_answer("call_1"),
            truncated_answer(Some("half a verdict"), 50),
            text_answer("verdict"),
        ]);
        let runner = SessionRunner::new(&provider).with_max_turns(2);

        let res = runner
            .run(&mut RejectingToolSession { rejections: 0 })
            .await
            .unwrap();
        assert_eq!(res.output, "verdict");
        assert_eq!(budget_prompts(&res.history), 1);

        let requests = provider.requests();
        assert_eq!(requests.len(), 3);
        for (i, req) in requests.iter().enumerate().skip(1) {
            assert!(req.tools.is_none(), "final-turn request {i} offered tools");
            assert_eq!(
                budget_prompts(&req.messages),
                1,
                "final-turn request {i} must carry the budget prompt exactly once"
            );
        }
    }

    /// The discarded answer was billed, so its tokens count, and the log
    /// keeps it so a reader can see what was cut. The model never sees it
    /// again: a half-written tool call may not even parse.
    #[tokio::test]
    async fn a_truncated_answer_is_billed_and_logged_but_not_resent() {
        let mut cut = truncated_answer(Some("half a"), 4000);
        cut.tool_calls = tool_answer("call_cut").tool_calls;
        let mut whole = text_answer("done");
        whole.usage = Some(AiUsage {
            prompt_tokens: 20,
            completion_tokens: 5,
            total_tokens: 25,
            cached_tokens: None,
        });
        let provider = RecordingProvider::new(vec![cut, whole]);

        let res = SessionRunner::new(&provider)
            .run(&mut RejectingToolSession { rejections: 0 })
            .await
            .unwrap();

        assert_eq!(res.usage.prompt_tokens, 30);
        assert_eq!(res.usage.completion_tokens, 4005);
        assert_eq!(res.usage.total_tokens, 4035);

        let logged = res
            .history
            .iter()
            .position(|m| m.content.as_deref() == Some("half a"))
            .expect("the truncated answer is in the log");
        assert_eq!(
            res.history[logged + 1].content.as_deref(),
            Some(OUTPUT_TRUNCATED_FEEDBACK),
            "the log says why the question was asked again"
        );

        let retry = &provider.requests()[1];
        assert!(
            retry
                .messages
                .iter()
                .all(|m| m.content.as_deref() != Some("half a") && m.tool_calls.is_none()),
            "the cut answer must not be sent back to the model"
        );
        assert!(unanswered_tool_calls(&retry.messages).is_empty());
    }

    /// An answer that wrote nothing spent the limit on reasoning. Asking for
    /// a shorter answer cannot help, so the model is asked once to act on
    /// what it has, and a second such answer fails with the setting to
    /// change.
    #[tokio::test]
    async fn a_truncation_that_wrote_nothing_is_retried_once_then_names_the_remedy() {
        let provider = RecordingProvider::new(vec![
            truncated_answer(None, 4000),
            truncated_answer(Some(""), 4000),
            text_answer("never reached"),
        ]);

        let err = match SessionRunner::new(&provider).run(&mut DummySession).await {
            Err(e) => e,
            Ok(_) => panic!("a second reasoning-only truncation must fail"),
        };

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            last_user_text(&requests[1]),
            OUTPUT_TRUNCATED_REASONING_FEEDBACK
        );
        assert_eq!(
            err.downcast_ref::<OutputTruncated>(),
            Some(&OutputTruncated {
                answer_chars: 0,
                tool_arg_chars: 0,
            })
        );
        assert!(
            err.to_string().contains("raise max_tokens or lower effort"),
            "the error must say what to change: {err}"
        );
        assert!(matches!(classify_ai_error(&err), AiErrorClass::Fatal));
    }

    /// Truncations are counted per turn. An accepted answer starts the count
    /// again, so a long stage that is cut off now and then keeps going, while
    /// a run of them in one turn still ends it.
    #[tokio::test]
    async fn truncations_are_counted_per_turn() {
        let cut = || truncated_answer(Some("half"), 50);
        let mut script: Vec<AiResponse> = (0..3).map(|_| cut()).collect();
        script.push(tool_answer("call_1"));
        script.extend((0..3).map(|_| cut()));
        script.push(text_answer("done"));
        let provider = RecordingProvider::new(script);

        let res = SessionRunner::new(&provider)
            .run(&mut RejectingToolSession { rejections: 0 })
            .await
            .expect("three in a row, an accepted turn, then three more is allowed");
        assert_eq!(res.output, "done");
        assert_eq!(provider.requests().len(), 8);

        let mut script: Vec<AiResponse> = (0..3).map(|_| cut()).collect();
        script.push(tool_answer("call_1"));
        script.extend((0..5).map(|_| cut()));
        let provider = RecordingProvider::new(script);
        let err = SessionRunner::new(&provider)
            .run(&mut RejectingToolSession { rejections: 0 })
            .await
            .err()
            .expect("a fourth truncation in a row must fail");
        assert!(err.to_string().contains("4 truncated answers in one turn"));
        assert_eq!(provider.requests().len(), 8);
    }

    /// A tool called with no input, whose arguments are `{}`, writes no
    /// arguments, so a truncation whose only output is such calls spent its
    /// budget on reasoning and is asked to reason less, not to write less.
    /// `null` arguments are left out: on a truncated response they come from
    /// arguments cut off part-way, a known limitation described in the
    /// design, not from a call with no input.
    #[test]
    fn a_truncation_holding_only_calls_without_input_is_spent_on_reasoning() {
        let no_input = |id: &str| ToolCall {
            id: id.to_string(),
            function_name: "list_files".to_string(),
            arguments: serde_json::json!({}),
            thought_signature: None,
        };
        let mut resp = truncated_answer(None, 50);
        resp.tool_calls = Some(vec![no_input("call_1"), no_input("call_2")]);

        let truncated = OutputTruncated::from_response(&resp);
        assert!(truncated.spent_on_reasoning(), "{truncated:?}");
        assert_eq!(
            truncation_action(&truncated, &TurnTruncations::default()),
            ErrorAction::RetryWithFeedback(OUTPUT_TRUNCATED_REASONING_FEEDBACK.to_string())
        );
    }

    #[test]
    fn truncation_action_follows_what_the_answer_wrote() {
        let wrote = OutputTruncated {
            answer_chars: 10,
            tool_arg_chars: 0,
        };
        let nothing = OutputTruncated {
            answer_chars: 0,
            tool_arg_chars: 0,
        };
        let fresh = TurnTruncations::default();
        let after_reasoning = TurnTruncations {
            count: 1,
            reasoning_retried: true,
        };

        let retry = |text: &str| ErrorAction::RetryWithFeedback(text.to_string());
        assert_eq!(
            truncation_action(&wrote, &fresh),
            retry(OUTPUT_TRUNCATED_FEEDBACK)
        );
        assert_eq!(
            truncation_action(&wrote, &after_reasoning),
            retry(OUTPUT_TRUNCATED_FEEDBACK),
            "an answer that wrote something is always worth asking for again"
        );
        assert_eq!(
            truncation_action(&nothing, &fresh),
            retry(OUTPUT_TRUNCATED_REASONING_FEEDBACK)
        );
        assert_eq!(
            truncation_action(&nothing, &after_reasoning),
            ErrorAction::Fail
        );
    }

    /// Offers one tool, and the review reaches its deadline while that tool
    /// runs: the signal is set mid-turn, as a worker's stdin reader sets it.
    struct DeadlineDuringToolsSession {
        signal: Arc<WindDownSignal>,
        rejections: usize,
    }

    #[async_trait]
    impl LlmSession for DeadlineDuringToolsSession {
        type Output = String;

        fn system_prompt(&self) -> String {
            "system".to_string()
        }

        fn initial_user_prompt(&self) -> String {
            "initial prompt".to_string()
        }

        fn tools(&self) -> Option<Vec<AiTool>> {
            Some(vec![AiTool {
                name: "git_grep".to_string(),
                description: "search".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }])
        }

        async fn call_tool(&mut self, _name: &str, _args: Value) -> Result<Value> {
            self.signal.request();
            Ok(serde_json::json!({"content": "file"}))
        }

        fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
            if self.rejections > 0 {
                self.rejections -= 1;
                return Err(ValidationError::FormatViolation("not json".to_string()));
            }
            Ok(response.content.clone().unwrap_or_default())
        }
    }

    fn out_of_time_prompts(messages: &[AiMessage]) -> usize {
        messages
            .iter()
            .filter(|m| m.content.as_deref() == Some(OUT_OF_TIME_PROMPT))
            .count()
    }

    fn requested_signal() -> Arc<WindDownSignal> {
        let signal = Arc::new(WindDownSignal::default());
        signal.request();
        signal
    }

    const STOPPED_BY_DEADLINE: SessionStopped = SessionStopped {
        cause: StopCause::OutOfTime,
    };

    /// A session that has had no answer yet has gathered nothing to
    /// synthesise, so the wind-down stops it before it sends a request.
    #[tokio::test]
    async fn test_wind_down_stops_an_interruptible_session_before_it_spends() {
        let provider = RecordingProvider::new(vec![text_answer("done")]);
        let Err(err) = SessionRunner::new(&provider)
            .with_wind_down(Some(requested_signal()), true)
            .run(&mut DummySession)
            .await
        else {
            panic!("an interruptible session must stop when the review runs out of time");
        };

        assert_eq!(
            err.downcast_ref::<SessionStopped>(),
            Some(&STOPPED_BY_DEADLINE),
            "the stop is typed, so it is not read as a fault of the stage: {err:#}"
        );
        assert!(
            provider.requests().is_empty(),
            "stopping after paying for a turn would defeat the point"
        );
    }

    /// The consolidation stages run on, which is what makes a salvage
    /// possible.
    #[tokio::test]
    async fn test_wind_down_lets_a_non_interruptible_session_finish() {
        let provider = RecordingProvider::new(vec![text_answer("done")]);
        let res = SessionRunner::new(&provider)
            .with_wind_down(Some(requested_signal()), false)
            .run(&mut DummySession)
            .await
            .expect("a non-interruptible session must survive a wind-down");

        assert_eq!(res.output, "done");
        assert!(!res.cut_short, "nothing about it was cut short");
        assert_eq!(provider.requests().len(), 1);
    }

    /// An unset signal changes nothing: the session uses its tools as usual.
    #[tokio::test]
    async fn an_unset_wind_down_signal_changes_nothing() {
        let provider = RecordingProvider::new(vec![tool_answer("c1"), text_answer("verdict")]);
        let res = SessionRunner::new(&provider)
            .with_wind_down(Some(Arc::new(WindDownSignal::default())), true)
            .run(&mut RejectingToolSession { rejections: 0 })
            .await
            .unwrap();
        assert!(!res.cut_short);
        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|r| r.tools.is_some()));
        assert!(
            requests
                .iter()
                .all(|r| out_of_time_prompts(&r.messages) == 0)
        );
    }

    /// A session that has gathered evidence is asked for its verdict in one
    /// more call with no tools, and its output is kept as cut short.
    #[tokio::test]
    async fn a_wind_down_after_one_turn_asks_for_the_verdict_once_without_tools() {
        let provider = RecordingProvider::new(vec![tool_answer("c1"), text_answer("verdict")]);
        let signal = Arc::new(WindDownSignal::default());
        let res = SessionRunner::new(&provider)
            .with_max_turns(10)
            .with_wind_down(Some(signal.clone()), true)
            .run(&mut DeadlineDuringToolsSession {
                signal,
                rejections: 0,
            })
            .await
            .expect("the evidence gathered so far becomes the verdict");

        assert_eq!(res.output, "verdict");
        assert!(res.cut_short);
        let requests = provider.requests();
        assert_eq!(requests.len(), 2, "exactly one call after the deadline");
        assert!(requests[0].tools.is_some());
        assert!(
            requests[1].tools.is_none(),
            "no tools on the out-of-time turn"
        );
        assert_eq!(out_of_time_prompts(&requests[1].messages), 1);
        assert_eq!(
            budget_prompts(&requests[1].messages),
            0,
            "it is the deadline, not the turn cap, that ended the session"
        );
        assert_eq!(
            out_of_time_prompts(&res.history),
            1,
            "the log says why the model stopped using tools"
        );
    }

    /// A rejected verdict on the out-of-time turn is asked for again without
    /// the prompt being pushed a second time.
    #[tokio::test]
    async fn a_validation_retry_on_the_out_of_time_turn_pushes_its_prompt_once() {
        let provider = RecordingProvider::new(vec![
            tool_answer("c1"),
            text_answer("bad"),
            text_answer("verdict"),
        ]);
        let signal = Arc::new(WindDownSignal::default());
        let res = SessionRunner::new(&provider)
            .with_max_turns(10)
            .with_wind_down(Some(signal.clone()), true)
            .run(&mut DeadlineDuringToolsSession {
                signal,
                rejections: 1,
            })
            .await
            .unwrap();

        assert_eq!(res.output, "verdict");
        assert!(res.cut_short);
        let requests = provider.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].tools.is_none());
        assert_eq!(out_of_time_prompts(&requests[2].messages), 1);
    }

    /// A session that fails on its out-of-time turn is still put down to the
    /// deadline, with its own error kept as the cause beneath.
    #[tokio::test]
    async fn a_failed_out_of_time_verdict_is_a_stop_not_a_stage_fault() {
        let provider = RecordingProvider::new(vec![
            tool_answer("c1"),
            text_answer("bad"),
            text_answer("still bad"),
        ]);
        let signal = Arc::new(WindDownSignal::default());
        let Err(err) = SessionRunner::new(&provider)
            .with_max_turns(10)
            .with_max_validation_attempts(2)
            .with_wind_down(Some(signal.clone()), true)
            .run(&mut DeadlineDuringToolsSession {
                signal,
                rejections: 5,
            })
            .await
        else {
            panic!("two rejected verdicts must still fail the session");
        };

        assert_eq!(
            err.downcast_ref::<SessionStopped>(),
            Some(&STOPPED_BY_DEADLINE)
        );
        assert_eq!(
            err.to_string(),
            "Session stopped: the review ran out of time"
        );
        assert!(
            format!("{err:#}").contains("after 2 validation attempts"),
            "the session's own failure stays in the chain: {err:#}"
        );
        assert_eq!(provider.requests().len(), 3);
    }

    /// A failure before the deadline is the session's own and is not
    /// wrapped, even when a signal is wired in.
    #[tokio::test]
    async fn a_failure_before_the_deadline_is_not_put_down_to_it() {
        let provider = RecordingProvider::new(vec![text_answer("bad"), text_answer("still bad")]);
        let Err(err) = SessionRunner::new(&provider)
            .with_max_validation_attempts(2)
            .with_wind_down(Some(Arc::new(WindDownSignal::default())), true)
            .run(&mut RejectingToolSession { rejections: 5 })
            .await
        else {
            panic!("two rejected answers must fail the session");
        };
        assert!(err.downcast_ref::<SessionStopped>().is_none(), "{err:#}");
    }

    /// The worker writes the cause as `review_stopped_by`, and the daemon
    /// reads it back, so its spelling is pinned.
    #[test]
    fn a_stop_cause_is_written_in_snake_case() {
        assert_eq!(
            serde_json::to_value(StopCause::OutOfTime).unwrap(),
            serde_json::json!("out_of_time")
        );
        assert_eq!(
            serde_json::from_value::<StopCause>(serde_json::json!("out_of_time")).unwrap(),
            StopCause::OutOfTime
        );
    }
}
