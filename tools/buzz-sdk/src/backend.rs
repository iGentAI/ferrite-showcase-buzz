//! Model access — one trait, one method.
//!
//! The agent runtime never talks to a provider. It assembles a
//! [`CompletionRequest`] (system prompt, messages, tool specs, model id, output
//! bound) and hands it to a [`Backend`]; whatever comes back is a
//! [`Completion`]. That is the whole surface, which is why the same runtime works
//! against Anthropic, against a gateway that speaks the same shape, and against
//! [`MockBackend`] in a test with no network at all.
//!
//! One method, not five: `complete(request)`. The request is a struct rather than
//! a parameter list so that adding a field later — a cache hint, a reasoning
//! budget — does not break every implementor.
//!
//! ## What the SDK does not do here
//!
//! It embeds no credentials and picks no provider for you. [`AnthropicBackend`]
//! reads `ANTHROPIC_API_KEY` from the environment when you ask it to and honors
//! `ANTHROPIC_BASE_URL` so a gateway can stand in front of it. Nothing is stored,
//! logged, or sent anywhere else.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::transport::BoxFuture;

/// Default model when a builder does not name one.
pub const DEFAULT_MODEL: &str = "claude-opus-5";

/// Default output bound for one model call.
///
/// Sized to stay comfortably inside a non-streaming HTTP timeout; a backend that
/// streams may raise it.
pub const DEFAULT_MAX_TOKENS: u32 = 16_000;

/// Who said a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A workspace member, or a tool result being handed back.
    User,
    /// The agent.
    Assistant,
}

/// A model's request to run one tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned id. Echoed back on the matching [`ToolResult`]; a
    /// mismatch is what makes a provider reject the next turn.
    pub id: String,
    /// The tool's registered name.
    pub name: String,
    /// The arguments, as the model produced them.
    pub arguments: Value,
}

/// The outcome of running one tool, going back to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// The [`ToolCall::id`] this answers.
    pub id: String,
    /// What the tool produced, or the error text if it failed.
    pub content: String,
    /// Whether `content` is an error. A failed tool is reported, never dropped:
    /// the model needs to see the failure to route around it.
    #[serde(default)]
    pub is_error: bool,
}

/// One turn of conversation.
///
/// Three shapes exist, and they are mutually exclusive in practice: a plain text
/// message, an assistant message carrying tool calls, and a user message carrying
/// tool results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Who is speaking.
    pub role: Role,
    /// The prose, which may be empty when the message is only tool traffic.
    #[serde(default)]
    pub text: String,
    /// Tool calls the assistant asked for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Tool results being returned.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResult>,
}

impl Message {
    /// A message from a workspace member.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        }
    }

    /// A message the agent already said.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        }
    }

    /// The assistant turn that asked for tools, echoed back so the provider can
    /// pair the results with the calls.
    #[must_use]
    pub fn assistant_calls(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
            tool_calls,
            tool_results: Vec::new(),
        }
    }

    /// The user turn that carries every tool result for one assistant turn.
    ///
    /// All results for one turn belong in **one** message: splitting them teaches
    /// the model to stop asking for tools in parallel.
    #[must_use]
    pub fn results(tool_results: Vec<ToolResult>) -> Self {
        Self {
            role: Role::User,
            text: String::new(),
            tool_calls: Vec::new(),
            tool_results,
        }
    }

    /// Whether this message carries nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.tool_calls.is_empty() && self.tool_results.is_empty()
    }
}

/// A tool as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The name the model must use to call it.
    pub name: String,
    /// Prose the model reads to decide when to call it. With the SDK's default
    /// permissive schema, this is where the argument shape gets documented — see
    /// [`tool`](fn@crate::tool).
    pub description: String,
    /// JSON Schema for the arguments.
    pub input_schema: Value,
}

/// One call to a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Provider model id, e.g. `claude-opus-5`.
    pub model: String,
    /// The system prompt: who the agent is, what it knows, what it may do.
    #[serde(default)]
    pub system: String,
    /// Conversation so far, oldest first.
    pub messages: Vec<Message>,
    /// Tools the model may call this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    /// Hard bound on output tokens.
    pub max_tokens: u32,
}

impl CompletionRequest {
    /// A request with the SDK's defaults and no tools.
    #[must_use]
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            system: String::new(),
            messages,
            tools: Vec::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// Set the system prompt.
    #[must_use]
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = system.into();
        self
    }

    /// Offer tools.
    #[must_use]
    pub fn tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    /// Bound the output.
    #[must_use]
    pub const fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

/// Why the model stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// It finished its answer.
    End,
    /// It wants tools run; `tool_calls` is populated.
    ToolUse,
    /// It hit the output bound. The answer is truncated.
    MaxTokens,
    /// It declined the request. Content is absent or partial and must not be
    /// treated as an answer.
    Refusal,
    /// Anything a provider reports that this SDK does not model.
    Other(String),
}

/// Token accounting for one or more model calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens sent.
    #[serde(default)]
    pub input_tokens: u32,
    /// Tokens generated.
    #[serde(default)]
    pub output_tokens: u32,
}

impl Usage {
    /// Accumulate another call's usage into this one.
    pub fn add(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }

    /// Total tokens.
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// What a model returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completion {
    /// The prose, concatenated across text blocks.
    #[serde(default)]
    pub text: String,
    /// Tools the model wants run.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Why it stopped.
    pub stop: StopReason,
    /// What it cost.
    #[serde(default)]
    pub usage: Usage,
}

impl Completion {
    /// A finished answer.
    #[must_use]
    pub fn answer(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tool_calls: Vec::new(),
            stop: StopReason::End,
            usage: Usage::default(),
        }
    }

    /// A turn that asks for tools.
    #[must_use]
    pub fn calling(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            text: String::new(),
            tool_calls,
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        }
    }

    /// Attach usage, for tests and for backends that fill it in separately.
    #[must_use]
    pub const fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }

    /// Attach prose to a tool-calling turn (models often narrate before calling).
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// Whether this completion asks for tools to be run.
    #[must_use]
    pub fn wants_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// Model access.
///
/// `Send + Sync` because one backend is shared by every concurrent turn.
pub trait Backend: Send + Sync {
    /// Run one completion.
    ///
    /// Implementations should map a provider's refusal onto
    /// [`StopReason::Refusal`] rather than returning empty text: the runtime
    /// checks the stop reason before it reads content, and a refusal reported as
    /// an empty answer would be posted into a channel as silence.
    fn complete<'a>(&'a self, request: &'a CompletionRequest) -> BoxFuture<'a, Result<Completion>>;
}

impl<T: Backend + ?Sized> Backend for std::sync::Arc<T> {
    fn complete<'a>(&'a self, request: &'a CompletionRequest) -> BoxFuture<'a, Result<Completion>> {
        (**self).complete(request)
    }
}

/// A [`Backend`] that returns canned completions and records what it was asked.
///
/// Scripted completions are returned in order; when the script runs out, the
/// fallback is returned for every later call. That is deliberate: a tool-use test
/// scripts *one* tool call and lets the fallback be the final answer, so the loop
/// terminates whatever the runtime does.
pub struct MockBackend {
    script: std::sync::Mutex<std::collections::VecDeque<Completion>>,
    fallback: Completion,
    seen: std::sync::Mutex<Vec<CompletionRequest>>,
}

impl core::fmt::Debug for MockBackend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MockBackend")
            .field("calls", &self.calls())
            .field("scripted_remaining", &self.remaining())
            .finish_non_exhaustive()
    }
}

impl MockBackend {
    /// A backend that always answers with `text`.
    #[must_use]
    pub fn answering(text: impl Into<String>) -> Self {
        Self {
            script: std::sync::Mutex::new(std::collections::VecDeque::new()),
            fallback: Completion::answer(text),
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// A backend that plays `script` in order, then falls back to `answering`'s
    /// behavior with `final_text`.
    #[must_use]
    pub fn scripted(script: Vec<Completion>, final_text: impl Into<String>) -> Self {
        Self {
            script: std::sync::Mutex::new(script.into()),
            fallback: Completion::answer(final_text),
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Append one more scripted completion.
    pub fn push(&self, completion: Completion) {
        if let Ok(mut script) = self.script.lock() {
            script.push_back(completion);
        }
    }

    /// Every request the runtime made, in order — the assertion surface for
    /// prompt assembly.
    #[must_use]
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.seen.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// How many completions were requested.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.seen.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// How many scripted completions are left unplayed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.script.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// The most recent request, if any.
    #[must_use]
    pub fn last_request(&self) -> Option<CompletionRequest> {
        self.seen.lock().ok().and_then(|s| s.last().cloned())
    }
}

impl Backend for MockBackend {
    fn complete<'a>(&'a self, request: &'a CompletionRequest) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(request.clone());
            }
            let next = self.script.lock().ok().and_then(|mut s| s.pop_front());
            Ok(next.unwrap_or_else(|| self.fallback.clone()))
        })
    }
}

/// A direct Anthropic Messages API [`Backend`], for local development only.
///
/// Feature-gated on `http`, because it is the only place in the SDK that opens a
/// socket to anything other than the workspace.
///
/// Sends `POST {base}/v1/messages` with `x-api-key` and
/// `anthropic-version: 2023-06-01`, carrying `model`, `max_tokens`, `system`,
/// `messages`, and `tools`. It sends no sampling parameters and no thinking
/// configuration — current models reject some of those outright, and the ones
/// that accept them are better steered by the system prompt the agent already
/// has. [`Completion::usage`] carries the provider's own token counts so an
/// agent can account for itself.
///
/// **This holds a provider key in your process and egresses straight to the
/// provider.** On this platform that boundary belongs to the AI gateway — "no app
/// but the gateway ever holds a provider key or egresses to a provider"
/// (`docs/the-ai-gateway.md`) — and the gateway additionally owns
/// response caching, single-flight coalescing, retry and backoff, per-model
/// timeouts, and cost accounting that this type does not have.
///
/// It is therefore **not** a default: [`crate::AgentBuilder::build`] refuses to
/// construct an agent unless a backend is named, so nothing gets a provider
/// connection by accident. Reach for a gateway-backed [`Backend`] in anything that
/// outlives an afternoon.
#[cfg(feature = "http")]
#[derive(Debug, Clone)]
pub struct AnthropicBackend {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

#[cfg(feature = "http")]
impl AnthropicBackend {
    /// The default endpoint.
    pub const DEFAULT_BASE_URL: &'static str = "https://api.anthropic.com";

    /// The API version header value this backend sends.
    pub const API_VERSION: &'static str = "2023-06-01";

    /// Build a backend from an explicit key.
    #[must_use]
    pub fn with_key(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: Self::DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Build a backend from the environment.
    ///
    /// Reads `ANTHROPIC_API_KEY`, and `ANTHROPIC_BASE_URL` if set — pointing the
    /// latter at a gateway is how an agent runs against a proxied or
    /// self-hosted endpoint without changing code.
    ///
    /// # Errors
    ///
    /// [`crate::SdkError::Env`] if `ANTHROPIC_API_KEY` is unset or empty.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|e| crate::SdkError::env("ANTHROPIC_API_KEY", e.to_string()))?;
        if api_key.trim().is_empty() {
            return Err(crate::SdkError::env(
                "ANTHROPIC_API_KEY",
                "is set but empty",
            ));
        }
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .map(|url| url.trim().trim_end_matches('/').to_string())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| Self::DEFAULT_BASE_URL.to_string());
        Ok(Self {
            client: reqwest::Client::new(),
            api_key: api_key.trim().to_string(),
            base_url,
        })
    }

    /// Point this backend at a different endpoint.
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    /// Reuse a caller's `reqwest` client (connection pool, proxy, timeouts).
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Translate an SDK request into the Messages API body.
    ///
    /// Separated from the round trip so the mapping is unit-testable without a
    /// network: the block structure (text vs `tool_use` vs `tool_result`) is the
    /// part worth pinning.
    #[must_use]
    pub fn request_body(request: &CompletionRequest) -> Value {
        let messages: Vec<Value> = request
            .messages
            .iter()
            .filter(|m| !m.is_empty())
            .map(Self::message_body)
            .collect();
        // Built by insertion rather than by index assignment: `body["x"] = ...`
        // on a `serde_json::Value` panics if the value is not an object, and this
        // crate denies indexing that can panic.
        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), Value::String(request.model.clone()));
        body.insert("max_tokens".to_string(), Value::from(request.max_tokens));
        body.insert("messages".to_string(), Value::Array(messages));
        if !request.system.trim().is_empty() {
            body.insert("system".to_string(), Value::String(request.system.clone()));
        }
        if !request.tools.is_empty() {
            body.insert(
                "tools".to_string(),
                Value::Array(
                    request
                        .tools
                        .iter()
                        .map(|t| {
                            serde_json::json!({
                                "name": t.name,
                                "description": t.description,
                                "input_schema": t.input_schema,
                            })
                        })
                        .collect(),
                ),
            );
        }
        Value::Object(body)
    }

    /// One message as Messages-API content blocks.
    fn message_body(message: &Message) -> Value {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        // Tool results ride a user turn as blocks; everything else is either
        // plain text or text plus tool_use blocks.
        if !message.tool_results.is_empty() {
            let blocks: Vec<Value> = message
                .tool_results
                .iter()
                .map(|r| {
                    let mut block = serde_json::Map::new();
                    block.insert("type".to_string(), Value::from("tool_result"));
                    block.insert("tool_use_id".to_string(), Value::String(r.id.clone()));
                    block.insert("content".to_string(), Value::String(r.content.clone()));
                    if r.is_error {
                        block.insert("is_error".to_string(), Value::Bool(true));
                    }
                    Value::Object(block)
                })
                .collect();
            return serde_json::json!({ "role": role, "content": blocks });
        }
        if message.tool_calls.is_empty() {
            return serde_json::json!({ "role": role, "content": message.text });
        }
        let mut blocks: Vec<Value> = Vec::with_capacity(message.tool_calls.len() + 1);
        if !message.text.is_empty() {
            blocks.push(serde_json::json!({ "type": "text", "text": message.text }));
        }
        for call in &message.tool_calls {
            blocks.push(serde_json::json!({
                "type": "tool_use",
                "id": call.id,
                "name": call.name,
                "input": call.arguments,
            }));
        }
        serde_json::json!({ "role": role, "content": blocks })
    }

    /// Parse a Messages API response body into a [`Completion`].
    ///
    /// # Errors
    ///
    /// [`crate::SdkError::Encoding`] if the body is not the documented shape.
    pub fn parse_response(body: &Value) -> Result<Completion> {
        let stop = match body.get("stop_reason").and_then(Value::as_str) {
            Some("end_turn") | Some("stop_sequence") | None => StopReason::End,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("refusal") => StopReason::Refusal,
            Some(other) => StopReason::Other(other.to_string()),
        };

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in body
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(chunk) = block.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(chunk);
                    }
                }
                Some("tool_use") => {
                    let (Some(id), Some(name)) = (
                        block.get("id").and_then(Value::as_str),
                        block.get("name").and_then(Value::as_str),
                    ) else {
                        return Err(crate::SdkError::encoding(
                            "model returned a tool_use block without an id or name",
                        ));
                    };
                    tool_calls.push(ToolCall {
                        id: id.to_string(),
                        name: name.to_string(),
                        arguments: block.get("input").cloned().unwrap_or(Value::Null),
                    });
                }
                // Thinking blocks and anything else a provider adds later are
                // skipped rather than refused: an unknown block must not break a
                // working agent.
                _ => {}
            }
        }

        let usage = Usage {
            input_tokens: body
                .pointer("/usage/input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .try_into()
                .unwrap_or(u32::MAX),
            output_tokens: body
                .pointer("/usage/output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .try_into()
                .unwrap_or(u32::MAX),
        };

        Ok(Completion {
            text,
            tool_calls,
            stop,
            usage,
        })
    }
}

#[cfg(feature = "http")]
impl Backend for AnthropicBackend {
    fn complete<'a>(&'a self, request: &'a CompletionRequest) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            let body = Self::request_body(request);
            let response = self
                .client
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", Self::API_VERSION)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await?;
            let status = response.status();
            let text = response.text().await?;
            if !status.is_success() {
                return Err(crate::SdkError::http(status.as_u16(), text));
            }
            let parsed: Value = serde_json::from_str(&text)?;
            Self::parse_response(&parsed)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn the_mock_plays_its_script_then_falls_back() {
        let mock = MockBackend::scripted(
            vec![Completion::calling(vec![ToolCall {
                id: "call_1".into(),
                name: "review_patch".into(),
                arguments: json!({"target": "main"}),
            }])],
            "all done",
        );
        let request = CompletionRequest::new("test-model", vec![Message::user("hi")]);

        let first = mock.complete(&request).await.unwrap();
        assert!(first.wants_tools());
        assert_eq!(first.stop, StopReason::ToolUse);

        let second = mock.complete(&request).await.unwrap();
        assert_eq!(second.text, "all done");
        assert_eq!(second.stop, StopReason::End);

        assert_eq!(mock.calls(), 2);
        assert_eq!(mock.remaining(), 0);
        assert_eq!(mock.requests()[0].model, "test-model");
        assert_eq!(mock.last_request().unwrap().messages.len(), 1);
    }

    #[test]
    fn usage_accumulates_without_overflowing() {
        let mut total = Usage::default();
        total.add(Usage {
            input_tokens: 10,
            output_tokens: 3,
        });
        total.add(Usage {
            input_tokens: u32::MAX,
            output_tokens: 1,
        });
        assert_eq!(total.input_tokens, u32::MAX);
        assert_eq!(total.output_tokens, 4);
        assert_eq!(total.total(), u32::MAX);
    }

    #[test]
    fn empty_messages_are_recognized() {
        assert!(Message::user("   ").is_empty());
        assert!(!Message::user("hi").is_empty());
        assert!(!Message::results(vec![ToolResult {
            id: "1".into(),
            content: String::new(),
            is_error: false
        }])
        .is_empty());
    }

    #[cfg(feature = "http")]
    #[test]
    fn the_anthropic_body_uses_the_documented_block_shapes() {
        let request = CompletionRequest::new(
            "claude-opus-5",
            vec![
                Message::user("review main"),
                Message::assistant_calls(
                    "on it",
                    vec![ToolCall {
                        id: "toolu_1".into(),
                        name: "review_patch".into(),
                        arguments: json!({"target": "main"}),
                    }],
                ),
                Message::results(vec![ToolResult {
                    id: "toolu_1".into(),
                    content: "no issues".into(),
                    is_error: false,
                }]),
                // Empty messages are dropped rather than sent as blank turns.
                Message::assistant(""),
            ],
        )
        .system("you review patches")
        .tools(vec![ToolSpec {
            name: "review_patch".into(),
            description: "Review a patch".into(),
            input_schema: json!({"type": "object"}),
        }]);

        let body = AnthropicBackend::request_body(&request);
        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(body["system"], "you review patches");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3, "the empty assistant turn is dropped");
        assert_eq!(messages[0]["content"], "review main");

        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "text");
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[1]["content"][1]["id"], "toolu_1");

        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
        assert!(messages[2]["content"][0].get("is_error").is_none());
    }

    #[cfg(feature = "http")]
    #[test]
    fn an_errored_tool_result_is_flagged_for_the_model() {
        let request = CompletionRequest::new(
            "m",
            vec![Message::results(vec![ToolResult {
                id: "toolu_1".into(),
                content: "the linter crashed".into(),
                is_error: true,
            }])],
        );
        let body = AnthropicBackend::request_body(&request);
        assert_eq!(body["messages"][0]["content"][0]["is_error"], true);
    }

    #[cfg(feature = "http")]
    #[test]
    fn responses_parse_text_tools_stop_reasons_and_usage() {
        let body = json!({
            "content": [
                {"type": "thinking", "thinking": ""},
                {"type": "text", "text": "looking now"},
                {"type": "tool_use", "id": "toolu_9", "name": "review_patch", "input": {"target": "main"}},
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 120, "output_tokens": 7},
        });
        let completion = AnthropicBackend::parse_response(&body).unwrap();
        assert_eq!(completion.text, "looking now");
        assert_eq!(completion.stop, StopReason::ToolUse);
        assert_eq!(completion.tool_calls[0].name, "review_patch");
        assert_eq!(completion.tool_calls[0].arguments["target"], "main");
        assert_eq!(completion.usage.input_tokens, 120);

        let refusal = AnthropicBackend::parse_response(&json!({
            "content": [],
            "stop_reason": "refusal",
        }))
        .unwrap();
        assert_eq!(refusal.stop, StopReason::Refusal);

        let unknown = AnthropicBackend::parse_response(&json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "pause_turn",
        }))
        .unwrap();
        assert_eq!(unknown.stop, StopReason::Other("pause_turn".into()));

        // Several text blocks join with newlines rather than running together.
        let multi = AnthropicBackend::parse_response(&json!({
            "content": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}],
            "stop_reason": "end_turn",
        }))
        .unwrap();
        assert_eq!(multi.text, "one\ntwo");
    }

    #[cfg(feature = "http")]
    #[test]
    fn a_malformed_tool_use_block_is_refused_rather_than_guessed() {
        let err = AnthropicBackend::parse_response(&json!({
            "content": [{"type": "tool_use", "input": {}}],
            "stop_reason": "tool_use",
        }))
        .unwrap_err();
        assert!(matches!(err, crate::SdkError::Encoding(_)), "{err}");
    }

    #[cfg(feature = "http")]
    #[test]
    fn from_env_requires_a_non_empty_key_and_honors_a_gateway_url() {
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test");
        std::env::set_var("ANTHROPIC_BASE_URL", "https://gateway.example.com/");
        let backend = AnthropicBackend::from_env().unwrap();
        assert_eq!(backend.base_url, "https://gateway.example.com");

        std::env::remove_var("ANTHROPIC_BASE_URL");
        assert_eq!(
            AnthropicBackend::from_env().unwrap().base_url,
            AnthropicBackend::DEFAULT_BASE_URL
        );

        std::env::set_var("ANTHROPIC_API_KEY", "   ");
        let err = AnthropicBackend::from_env().unwrap_err();
        assert!(matches!(err, crate::SdkError::Env { .. }), "{err}");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
}
