# DESIGN: Claude Provider

## Context

Sashiko accesses Claude models through multiple paths: the direct Anthropic
Messages API, the Claude Code CLI, AWS Bedrock (Converse API), and Google
Cloud Vertex AI (rawPredict). This document covers the direct API and CLI
providers. Vertex AI and Bedrock have their own design documents.

## Architecture

### Direct API (`src/ai/claude.rs`)

`ClaudeClient` sends HTTP requests to the Anthropic Messages API
(`https://api.anthropic.com/v1/messages`). The client handles:

- Request/response translation between Sashiko's generic `AiRequest`/`AiResponse`
  and Claude's wire format (`ClaudeRequest`/`ClaudeResponse`)
- Prompt caching via ephemeral cache control markers
- Extended thinking via `ThinkingConfig`
- Rate limit and overload retry signaling

#### Wire Format Types

All Claude-compatible providers (direct API, Vertex AI) share these types:

| Type | Purpose |
|------|---------|
| `ClaudeRequest` | Top-level request body (model, messages, system, tools, thinking) |
| `ClaudeMessage` | A message with role and content blocks |
| `ClaudeContent` | Tagged enum: Text, Thinking, ToolUse, ToolResult |
| `ClaudeResponse` | Response with content blocks and usage |
| `ThinkingConfig` | Optional thinking type and effort level |
| `SystemBlock` | System prompt text with optional cache control |
| `ClaudeTool` | Tool definition with input schema |
| `ClaudeError` | Typed errors: rate limit, overload, invalid request, auth |

#### Translation Functions

These are `pub` for reuse by Vertex AI:

- `translate_ai_request()` -- converts `AiRequest` to `ClaudeRequest`
- `translate_ai_response()` -- converts `ClaudeResponse` to `AiResponse`
- `apply_cache_control()` -- adds ephemeral cache markers to last system/tool/message
- `estimate_tokens_generic()` -- token estimation using cl100k_base tokenizer

#### ThinkingConfig Handling

The `ThinkingConfig` struct has two optional fields: `type` (renamed via serde
from `thinking`) and `effort`. Both use `skip_serializing_if = "Option::is_none"`.

The outer `ClaudeRequest.thinking` field is `Option<ThinkingConfig>` with
`skip_serializing_if = "Option::is_none"`. When both inner fields are None,
the entire config is set to `None` to avoid emitting an empty `"thinking": {}`
object, which the API rejects.

#### Authentication

Uses `ANTHROPIC_API_KEY` or `LLM_API_KEY` environment variables. Sent as
`x-api-key` header alongside `anthropic-version: 2023-06-01`.

### Prompt Caching

Cache control markers are applied to the last element of each category:
1. Last system block
2. Last tool definition
3. Last message content block

The Anthropic API provides a 5-minute TTL ephemeral cache. Cached tokens
appear in the response usage as `cache_read_input_tokens` and
`cache_creation_input_tokens`.

### Claude CLI Provider (`src/ai/claude_cli.rs`)

`ClaudeCliProvider` shells out to the `claude --print` command, which runs
in text-completion mode (no tools, no file access, no session persistence).
The CLI reads a prompt from stdin and writes JSON to stdout.

#### Prompt Construction

`build_prompt()` constructs a text prompt with XML-style tags:
- `<system>` for system prompts
- `<user>`, `<assistant>`, `<tool_result>`, `<tool_call>` for messages
- `<available_tools>` for tool definitions
- `RESPONSE FORMAT` instructions for JSON output

JSON format instructions are emitted when:
1. Tools are present (tool_calls/content format)
2. `response_format` is `AiResponseFormat::Json` without tools (schema-aware)

#### Error Handling

The CLI writes JSON to stdout even on non-zero exit. The provider reads stdout
before checking exit status and extracts the `result` field from the JSON
for structured error messages.

## Settings

```toml
[ai.claude]
prompt_caching = true              # Default: true
max_tokens = 4096                  # Max output tokens per request
base_url = "https://..."           # Override API endpoint
thinking = "enabled"               # "enabled" or "adaptive"
effort = "high"                    # "low", "medium", "high"

[ai]
api_timeout_secs = 300             # Minimum per-request deadline
```

### Request deadline

Each request has a deadline, set once on the HTTP client by
`ClaudeClient::new`:

```text
deadline = max(api_timeout_secs, max_tokens / 10 + 120) seconds
```

- Without a deadline a stalled connection parks the stage inside
  `generate_content` until the review deadline ends the whole review, holding
  an LLM slot the whole time. A local review would hang forever. The turn cap
  cannot help: it is checked between round trips.
- Thinking tokens are output tokens, so `max_tokens`, not the difficulty of
  the question, bounds how long a legitimate response can take. 10 tokens a
  second is a pessimistic output rate, about an order of magnitude below a
  healthy connection, so the estimate only errs towards waiting longer.
- 120 s is the allowance for the first token, which on a large uncached
  prompt is not instant.
- `api_timeout_secs` can raise the deadline but never lower it below that
  floor. A lower deadline would abandon a full-length response while it was
  still arriving and turn it into a retry. The client logs at info when the
  floor raised the configured value. At the defaults (300 s, 4096 tokens) the
  deadline is 529 s.
- The floor has no cap. A large `max_tokens` (128k gives 12,920 s) can give a
  deadline longer than `review.timeout_seconds`, and the review deadline then
  stays in charge. A cap would cut off the long answers the floor exists for.
- A deadline that passes is a transport failure (see Error Handling), so the
  request is retried within the stage instead of ending it.
- The deadline is not part of `cache_identity`, because it does not change
  what a response contains.
- A failure to build the HTTP client is returned from `ClaudeClient::new`, and
  so from `create_provider_from_ai`. It is never replaced by a client with no
  deadline.

## Error Handling

| HTTP Status | Error Type | Retry Strategy |
|-------------|-----------|----------------|
| 429 | RateLimitExceeded | Use Retry-After header, default 60s |
| 529 | OverloadedError | Exponential backoff starting at 5s |
| 400 | InvalidRequest | No retry |
| 401/403 | AuthenticationError | No retry |
| None: send or body read failed | Transport | Exponential backoff, floored at 0s |
| None: request could not be built, or a redirect failed | Untyped error | No retry |

A transport failure is any error from sending the request, or from reading
the body of a successful response, other than a builder or redirect error.
That covers a refused or reset connection, a TLS failure and a request
deadline. It becomes `ClaudeError::Transport`, which classifies as
Transient, because the same request may well succeed when sent again. A
builder error (an unusable URL or header) or a redirect error (a loop or too
many hops) would fail the same way on every attempt, so it stays an untyped
error, which classifies as Fatal.

`Transport` carries a `retry_after` of zero. That is a floor, not a delay:
`BackoffProvider` waits for the larger of it and its own exponential backoff
with jitter, so a connection reset that clears at once is not held behind a
fixed wait.

A refused connection is Transient too, so a wrong `base_url`, an absent proxy
or a TLS misconfiguration is retried rather than failing at once. In the
daemon the retries run until `review.timeout_seconds`, and the review ends
`TimedOut` instead of `Failed`. `BackoffProvider` logs a warning on every
retry, so the cause stays visible. That holds only because the `Transport`
message carries reqwest's whole source chain, for example "tcp connect
error: Connection refused": reqwest's own `Display` names only the error kind
and the URL. The openai, ollama, vllm and gemini clients already treat their
send errors this way.

`classify_ai_error` also classifies a raw `reqwest::Error` by the same rule,
after every provider's typed error. anyhow's `downcast_ref` finds it under
`.context(..)`, so this covers the bare body reads of the Gemini and Vertex
clients and Vertex's send errors, which would otherwise fall through to
Fatal.

## Testing

Tests are in `src/ai/claude.rs` and `src/ai/claude_cli.rs`:
- ThinkingConfig serialization (omitted when both None, present when set)
- Request translation (system, user, assistant, tool calls, tool results)
- Response translation (text, tool calls, thinking, usage with cache)
- Cache control (applied when enabled, absent when disabled)
- `build_prompt()` format instructions (with/without tools, with/without JSON format)
- Error classification: each `ClaudeError` variant, including `Transport` as
  Transient
- Transport failures through `generate_content`: a refused connection and a
  200 whose body stops short of its length are each a `Transport` error that
  names the cause from the source chain, and a request reqwest cannot build
  is an untyped Fatal error
- Request deadline: the default `api_timeout_secs` is raised to the output
  allowance (529 s for 4096 tokens), a longer configured value is kept, and
  zero settings still get the 120 s first-token allowance
- Deadline on the wire: with tokio's clock paused, a client from
  `ClaudeClient::new` sends to a server that reads the request and never
  answers, and the request is abandoned exactly at `request_timeout` as a
  `Transport` error naming "operation timed out"

Tests in `src/ai/mod.rs` cover the raw `reqwest::Error` rule: a refused
connection is Transient, bare and under `.context(..)` applied to the send
`Result` as Vertex applies it, and a builder error is Fatal.
