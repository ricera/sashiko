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
use serde_json::Value;

use super::{
    AiErrorClass, AiMessage, AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiTool,
    AiUsage, ToolCall, classify_ai_error,
};

/// The unified result of executing an [`LlmSession`].
pub struct SessionResult<T> {
    /// The validated output of the session.
    pub output: T,
    /// The full conversation history.
    pub history: Vec<AiMessage>,
    /// Accumulated token usage statistics.
    pub usage: AiUsage,
}

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

    /// Executes a tool call requested by the LLM.
    async fn call_tool(&mut self, name: &str, _args: Value) -> Result<Value> {
        anyhow::bail!("Tool execution not implemented for this session: {}", name)
    }

    /// Executes multiple tool calls requested by the LLM.
    /// Default implementation runs them sequentially, and propagates a tool
    /// error rather than reporting it to the model, which ends the session.
    /// A session that wants the model to see and correct its own bad calls
    /// must override this.
    async fn call_tools(&mut self, calls: Vec<ToolCall>) -> Result<Vec<(String, Value)>> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let res = self.call_tool(&call.function_name, call.arguments).await?;
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

/// Marker in the error text of a session stopped by supervisor cancellation.
///
/// Stage results are collected as `Result`s, so consumers need to tell "this
/// stage was cancelled" apart from "this stage genuinely failed" when reporting
/// which parts of the review did not run.
pub const SESSION_CANCELLED: &str = "Session cancelled by supervisor";

/// Fired around tool execution with the tool names, current turn, and turn cap.
/// An empty name slice signals the tools finished.
type ToolsCallback<'a> = dyn Fn(&[String], usize, usize) + Send + Sync + 'a;

/// Fired around provider backoff with the wait in seconds (`None` once over),
/// plus the current turn and turn cap.
type BackoffCallback<'a> = dyn Fn(Option<u64>, usize, usize) + Send + Sync + 'a;

/// Fired as each message joins the conversation log.
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
    /// Called as each message joins the log, so the conversation can be watched
    /// while it happens rather than only after it ends.
    on_message: Option<Box<MessageCallback<'a>>>,
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
        }
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
    /// outside is indistinguishable from a very slow model — and calls for a
    /// completely different response.
    pub fn with_backoff_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(Option<u64>, usize, usize) + Send + Sync + 'a,
    {
        self.on_backoff = Some(Box::new(cb));
        self
    }

    /// Configures a callback fired as each message is appended to the log.
    ///
    /// The full conversation is only persisted when the review ends, so without
    /// this there is nothing to show while one is running — which is exactly
    /// when you want to see what it is doing.
    pub fn with_message_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(&AiMessage) + Send + Sync + 'a,
    {
        self.on_message = Some(Box::new(cb));
        self
    }

    /// Configures a turn callback.
    pub fn with_turn_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(usize, usize) + Send + Sync + 'a,
    {
        self.on_turn = Some(Box::new(cb));
        self
    }

    /// Reports one message to the live-log callback, if one is configured.
    fn emit_message(&self, msg: &AiMessage) {
        if let Some(ref cb) = self.on_message {
            cb(msg);
        }
    }

    /// Runs the session to completion. Returns the validated output and conversation history (for logging).
    pub async fn run<S>(&self, session: &mut S) -> Result<SessionResult<S::Output>>
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

        let mut log_history = vec![AiMessage {
            role: AiRole::User,
            content: Some(session.log_user_prompt()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }];

        let mut turns = 0;
        let mut validation_attempts = 0;
        let mut transient_retries = 0;
        let mut provider_error_retries = 0;
        let mut total_prompt_tokens = 0;
        let mut total_completion_tokens = 0;
        let mut total_cached_tokens = 0;

        loop {
            // Checked before spending another LLM round trip rather than after,
            // so a cancelled review stops costing money immediately.
            if crate::ai::worker_cancel_token().is_cancelled() {
                anyhow::bail!("{}", SESSION_CANCELLED);
            }

            turns += 1;
            if turns > self.max_turns {
                anyhow::bail!("Session exceeded max turns limit ({})", self.max_turns);
            }
            if let Some(ref cb) = self.on_turn {
                cb(turns, self.max_turns);
            }

            let request = AiRequest {
                system: Some(session.system_prompt()),
                messages: history.clone(),
                tools: session.tools(),
                temperature: session.temperature(),
                response_format: session.response_format(),
                context_tag: session.context_tag(),
            };

            let resp = match self.provider.generate_content(request).await {
                Ok(r) => r,
                Err(e) => match classify_ai_error(&e) {
                    AiErrorClass::RateLimit { retry_after }
                    | AiErrorClass::Transient { retry_after } => {
                        transient_retries += 1;
                        if transient_retries > self.max_transient_retries {
                            anyhow::bail!(
                                "Session failed after {} transient/rate-limit errors. Last error: {}",
                                self.max_transient_retries,
                                e
                            );
                        }
                        tracing::warn!(
                            "API error ({}), pausing for {:?} before retry (attempt {}/{})...",
                            e,
                            retry_after,
                            transient_retries,
                            self.max_transient_retries
                        );
                        if let Some(ref cb) = self.on_backoff {
                            cb(Some(retry_after.as_secs()), turns, self.max_turns);
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
                                provider_error_retries += 1;
                                if provider_error_retries > self.max_provider_error_retries {
                                    anyhow::bail!(
                                        "Session failed after {} provider error retries. Last error: {}",
                                        self.max_provider_error_retries,
                                        e
                                    );
                                }
                                let msg = AiMessage {
                                    role: AiRole::User,
                                    content: Some(feedback.clone()),
                                    thought: None,
                                    thought_signature: None,
                                    tool_calls: None,
                                    tool_call_id: None,
                                };
                                history.push(msg.clone());
                                self.emit_message(&msg);
                                log_history.push(msg);
                                turns = turns.saturating_sub(1);
                                continue;
                            }
                            ErrorAction::Fail => return Err(e),
                        }
                    }
                },
            };

            if resp.truncated {
                anyhow::bail!("LLM output was truncated by provider (e.g. hit max tokens)");
            }

            if let Some(usage) = &resp.usage {
                total_prompt_tokens += usage.prompt_tokens;
                total_completion_tokens += usage.completion_tokens;
                total_cached_tokens += usage.cached_tokens.unwrap_or(0);
            }

            let assistant_msg = AiMessage {
                role: AiRole::Assistant,
                content: resp.content.clone(),
                thought: resp.thought.clone(),
                thought_signature: resp.thought_signature.clone(),
                tool_calls: resp.tool_calls.clone(),
                tool_call_id: None,
            };
            history.push(assistant_msg.clone());
            self.emit_message(&assistant_msg);
            log_history.push(assistant_msg);

            // Handle Tool Calls
            if let Some(tool_calls) = &resp.tool_calls {
                if let Some(ref cb) = self.on_tools {
                    let names: Vec<String> =
                        tool_calls.iter().map(|c| c.function_name.clone()).collect();
                    cb(&names, turns, self.max_turns);
                }
                let results = session.call_tools(tool_calls.clone()).await;
                if let Some(ref cb) = self.on_tools {
                    cb(&[], turns, self.max_turns);
                }
                let results = results?;
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
                    self.emit_message(&tool_msg);
                    log_history.push(tool_msg);
                }
                continue; // Loop again to feed tool results back to LLM
            }

            // No tool calls: validate response
            match session.validate(&resp) {
                Result::Ok(output) => {
                    let usage = AiUsage {
                        prompt_tokens: total_prompt_tokens,
                        completion_tokens: total_completion_tokens,
                        total_tokens: total_prompt_tokens + total_completion_tokens,
                        cached_tokens: Some(total_cached_tokens),
                    };
                    return Ok(SessionResult {
                        output,
                        history: log_history,
                        usage,
                    });
                }
                Result::Err(ValidationError::FormatViolation(violation)) => {
                    validation_attempts += 1;
                    if validation_attempts >= self.max_validation_attempts {
                        anyhow::bail!(
                            "Failed to generate valid response after {} validation attempts. Last violation: {}",
                            self.max_validation_attempts,
                            violation
                        );
                    }
                    let feedback = session.format_validation_feedback(&violation);
                    let msg = AiMessage {
                        role: AiRole::User,
                        content: Some(feedback),
                        thought: None,
                        thought_signature: None,
                        tool_calls: None,
                        tool_call_id: None,
                    };
                    history.push(msg.clone());
                    self.emit_message(&msg);
                    log_history.push(msg);
                    turns = turns.saturating_sub(1);
                }
                Result::Err(ValidationError::Fatal(err)) => {
                    anyhow::bail!("Fatal validation error: {}", err);
                }
            }
        }
    }
}
