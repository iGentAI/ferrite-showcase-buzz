//! The model behind a turn: the transport, the chat backend and parsing what the
//! model answers.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use ferrite_app::serde::Deserialize;
use ferrite_app::serde_json;
use ferrite_app::AppContext;
use crate::agents::compaction::Summarizer;
use crate::agents::compaction::SUMMARIZATION_PROMPT;
use crate::agents::conv::ConvMessage;
use crate::agents::conv::ConvRole;
use crate::agents::model_client::ChatBackend;
use crate::agents::model_client::ChatOutcome;
use crate::agents::model_client::TokenUsage;
use crate::agents::model_client::ToolCallRequest;
use crate::agents::tools::coding_tool_schemas;
use crate::agents::tools::ToolSchema;

/// The model boundary the turn pipeline drives. Production is [`DirectLlm`]
/// (an in-process call into [`crate::llm`]); gates inject a scripted
/// implementation. Dyn-safe (boxed futures) so the registry can carry it
/// through the `delegate` tool's async recursion without generics infection.
pub trait LlmTransport: Send + Sync {
    /// One completion.
    fn chat<'a>(
        &'a self,
        ctx: &'a AppContext,
        request: crate::llm::ChatRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<crate::llm::ChatReply, String>>
                + Send
                + 'a,
        >,
    >;
    /// Model metadata (resolution, bounds, tool protocol).
    fn model<'a>(
        &'a self,
        ctx: &'a AppContext,
        model: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<crate::llm::ModelReply, String>>
                + Send
                + 'a,
        >,
    >;
}

/// The production transport: plain function calls into [`crate::llm`].
pub struct DirectLlm;

impl LlmTransport for DirectLlm {
    fn chat<'a>(
        &'a self,
        ctx: &'a AppContext,
        request: crate::llm::ChatRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<crate::llm::ChatReply, String>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            match crate::llm::complete(ctx, request).await {
                Ok(reply) => Ok(reply),
                Err(failure) => {
                    // A 4xx is the CALLER's (an unsupported model, an oversized
                    // prompt) and belongs in the turn's own answer. Anything
                    // above that is the OPERATOR's — a missing or rejected
                    // credential, a denied egress, a provider outage — and it
                    // makes every persona in the workspace mute until it clears,
                    // so it goes where an operator will see it.
                    if failure.status >= 424 {
                        crate::relay::alerts::operational(
                            ctx,
                            "error",
                            crate::relay::alerts::PROVIDER_UNAVAILABLE,
                            &format!(
                                "the model provider boundary refused a turn, so personas \
                                 cannot answer: {failure}"
                            ),
                        )
                        .await;
                    }
                    Err(failure.to_string())
                }
            }
        })
    }

    fn model<'a>(
        &'a self,
        ctx: &'a AppContext,
        model: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<crate::llm::ModelReply, String>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            crate::llm::model_info(ctx, model)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

pub(super) fn usage_to_tokens(usage: &crate::llm::Usage) -> TokenUsage {
    TokenUsage {
        input_tokens: i64::from(usage.input_tokens)
            .saturating_add(i64::from(usage.cache_write_tokens)),
        cached_input_tokens: i64::from(usage.cache_read_tokens),
        output_tokens: i64::from(usage.output_tokens),
    }
}

#[derive(Debug, Clone)]
pub(super) struct ModelMeta {
    pub(super) context_window: i64,
}

/// The turn's model backend: the gateway chat contract
/// (system-prompt assembly, the native tool protocol, the JSON fallback for
/// non-native models) over an [`LlmTransport`] instead of request/reply
/// queues.
pub(super) struct LlmBackend<'a> {
    pub(super) ctx: &'a AppContext,
    pub(super) llm: Arc<dyn LlmTransport>,
    pub(super) model: String,
    /// The model-visible tool surface.
    pub(super) tool_schemas: Vec<ToolSchema>,
    /// Whether the boundary speaks native tool-use (learned from
    /// `model_metadata`; [`crate::llm::model_info`] always says yes, a
    /// scripted transport may say no to exercise the JSON path).
    pub(super) native_tools: AtomicBool,
}

impl<'a> LlmBackend<'a> {
    pub(super) fn new(ctx: &'a AppContext, llm: Arc<dyn LlmTransport>, model: impl Into<String>) -> Self {
        Self {
            ctx,
            llm,
            model: model.into(),
            tool_schemas: coding_tool_schemas(),
            native_tools: AtomicBool::new(false),
        }
    }

    pub(super) fn with_tool_schemas(mut self, tool_schemas: Vec<ToolSchema>) -> Self {
        self.tool_schemas = tool_schemas;
        self
    }

    pub(super) fn system_prompt(&self, messages: &[ConvMessage]) -> (String, usize) {
        let tools = &self.tool_schemas;
        let mut system = String::new();
        system.push_str("You are running inside Ferrite as a constrained app.\n");
        system.push_str("Return exactly one JSON object and no surrounding prose.\n");
        system.push_str("Allowed response shapes:\n");
        system.push_str(r#"{"final":"short final answer"}"#);
        system.push('\n');
        system.push_str(r#"{"tool_calls":[{"id":"stable-id","tool":"tool_name","args":{...}}]}"#);
        system.push('\n');
        system.push_str("Use tools when work requires files, memory, search, or durable state. Use end_turn only when the requested work is complete.\n");
        system.push_str("Available tools:\n");
        system.push_str(&render_tools(tools));

        let mut consumed = 0usize;
        for m in messages {
            if m.role != ConvRole::System || !m.tool_calls.is_empty() || m.tool_call_id.is_some() {
                break;
            }
            system.push('\n');
            system.push_str(&m.text);
            consumed += 1;
        }
        (system, consumed)
    }

    /// The native-path system prompt: base agent behavior WITHOUT the
    /// JSON-response contract (tools ride the `tools` param).
    pub(super) fn native_system_prompt(&self, messages: &[ConvMessage]) -> (String, usize) {
        let mut system = String::new();
        system.push_str("You are running inside Ferrite as a constrained app.\n");
        system.push_str(
            "Use the provided tools when work requires files, memory, search, code, or durable state. Call end_turn only when the requested work is complete.\n",
        );
        let mut consumed = 0usize;
        for m in messages {
            if m.role != ConvRole::System || !m.tool_calls.is_empty() || m.tool_call_id.is_some() {
                break;
            }
            system.push('\n');
            system.push_str(&m.text);
            consumed += 1;
        }
        (system, consumed)
    }

    /// The folded conversation as the provider-neutral message list.
    pub(super) fn neutral_messages(
        &self,
        messages: &[ConvMessage],
        skip: usize,
    ) -> Vec<crate::llm::NeutralMsg> {
        messages
            .iter()
            .skip(skip)
            .map(|m| crate::llm::NeutralMsg {
                role: match m.role {
                    ConvRole::System => "system",
                    ConvRole::User => "user",
                    ConvRole::Assistant => "assistant",
                    ConvRole::Tool => "tool",
                }
                .to_string(),
                content: m.text.clone(),
                tool_calls: m
                    .tool_calls
                    .iter()
                    .map(|t| crate::llm::NeutralToolCall {
                        id: t.id.clone(),
                        name: t.name.clone(),
                        arguments: t.args.clone(),
                    })
                    .collect(),
                tool_call_id: m.tool_call_id.clone(),
            })
            .collect()
    }

    pub(super) fn tool_specs(&self) -> Vec<crate::llm::ToolSpec> {
        self.tool_schemas
            .iter()
            .map(|t| crate::llm::ToolSpec {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            })
            .collect()
    }

    pub(super) fn conversation_prompt(&self, messages: &[ConvMessage], skip: usize) -> String {
        let mut prompt = String::new();
        prompt.push_str("Conversation:\n");
        for m in messages.iter().skip(skip) {
            prompt.push_str(match m.role {
                ConvRole::System => "system",
                ConvRole::User => "user",
                ConvRole::Assistant => "assistant",
                ConvRole::Tool => "tool",
            });
            prompt.push_str(": ");
            prompt.push_str(&m.text);
            prompt.push('\n');
        }
        prompt
    }

    pub(super) async fn llm_chat(
        &self,
        request: crate::llm::ChatRequest,
    ) -> crate::agents::error::Result<crate::llm::ChatReply> {
        let reply = self.llm.chat(self.ctx, request).await.map_err(|e| {
            crate::agents::error::AgentError::Plane {
                message: format!("llm: {e}"),
            }
        })?;
        if reply.context_window == 0 {
            return Err(crate::agents::error::AgentError::Plane {
                message: "llm returned an invalid context window".to_string(),
            });
        }
        Ok(reply)
    }

    pub(super) async fn model_metadata(&self) -> crate::agents::error::Result<ModelMeta> {
        let info = self.llm.model(self.ctx, &self.model).await.map_err(|e| {
            crate::agents::error::AgentError::Plane {
                message: format!("llm model: {e}"),
            }
        })?;
        if info.context_window == 0 {
            return Err(crate::agents::error::AgentError::Plane {
                message: "llm returned an invalid context window".to_string(),
            });
        }
        // Learn the boundary's tool-use protocol for this backend's turns.
        self.native_tools
            .store(info.native_tools, Ordering::Relaxed);
        Ok(ModelMeta {
            context_window: i64::from(info.context_window),
        })
    }
}

impl Summarizer for LlmBackend<'_> {
    async fn summarize(&self, transcript: &str) -> crate::agents::error::Result<String> {
        let prompt =
            format!("{SUMMARIZATION_PROMPT}\n\n<conversation>\n{transcript}\n</conversation>");
        let reply = self
            .llm_chat(crate::llm::ChatRequest {
                model: self.model.clone(),
                prompt,
                ..Default::default()
            })
            .await?;
        Ok(reply.completion)
    }
}

impl ChatBackend for LlmBackend<'_> {
    async fn complete(
        &self,
        messages: &[ConvMessage],
    ) -> crate::agents::error::Result<ChatOutcome> {
        if self.native_tools.load(Ordering::Relaxed) {
            let (system, skip) = self.native_system_prompt(messages);
            let reply = self
                .llm_chat(crate::llm::ChatRequest {
                    model: self.model.clone(),
                    prompt: String::new(),
                    system: (!system.is_empty()).then_some(system),
                    messages: Some(self.neutral_messages(messages, skip)),
                    tools: Some(self.tool_specs()),
                    max_tokens: None,
                })
                .await?;
            let tool_calls = reply
                .tool_calls
                .iter()
                .map(|c| ToolCallRequest {
                    id: c.id.clone(),
                    tool: c.name.clone(),
                    args: if c.arguments.trim().is_empty() {
                        b"{}".to_vec()
                    } else {
                        c.arguments.clone().into_bytes()
                    },
                })
                .collect::<Vec<_>>();
            return Ok(ChatOutcome {
                text: reply.completion.clone(),
                tool_calls,
                usage: usage_to_tokens(&reply.usage),
            });
        }
        let (system, skip) = self.system_prompt(messages);
        let reply = self
            .llm_chat(crate::llm::ChatRequest {
                model: self.model.clone(),
                prompt: self.conversation_prompt(messages, skip),
                system: (!system.is_empty()).then_some(system),
                ..Default::default()
            })
            .await?;
        let usage = usage_to_tokens(&reply.usage);
        let parsed = match parse_agent_json_completion(&reply.completion)? {
            Some(parsed) => parsed,
            None => {
                return Ok(ChatOutcome {
                    text: reply.completion,
                    tool_calls: Vec::new(),
                    usage,
                })
            }
        };
        let tool_calls = parsed
            .tool_calls
            .into_iter()
            .map(|call| ToolCallRequest {
                id: call.id,
                tool: call.tool,
                args: call.args.to_string().into_bytes(),
            })
            .collect();
        Ok(ChatOutcome {
            text: parsed.final_text.unwrap_or_default(),
            tool_calls,
            usage,
        })
    }
}

pub(super) fn render_tools(tools: &[ToolSchema]) -> String {
    let mut out = String::new();
    for tool in tools {
        out.push_str("- ");
        out.push_str(&tool.name);
        out.push_str(": ");
        out.push_str(&tool.description);
        out.push_str(" schema=");
        out.push_str(&tool.input_schema.to_string());
        out.push('\n');
    }
    out
}

#[derive(Debug, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub(super) struct ModelJson {
    #[serde(default, rename = "final")]
    pub(super) final_text: Option<String>,
    #[serde(default)]
    pub(super) tool_calls: Vec<ModelToolCall>,
}

#[derive(Debug, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub(super) struct ModelToolCall {
    pub(super) id: String,
    pub(super) tool: String,
    pub(super) args: serde_json::Value,
}

pub(super) fn json_value_slice(input: &str, start: usize) -> Option<&str> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    for (rel, ch) in input[start..].char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => depth = depth.saturating_add(1),
            '}' | ']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let end = start + rel + ch.len_utf8();
                    return Some(&input[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

pub(super) fn parse_agent_json_completion(
    completion: &str,
) -> crate::agents::error::Result<Option<ModelJson>> {
    let trimmed = completion.trim();
    if let Some(parsed) = parse_tool_calls_wrapper(trimmed)? {
        return Ok(Some(parsed));
    }
    if trimmed.starts_with('[') {
        let slice = json_value_slice(trimmed, 0).ok_or_else(|| {
            crate::agents::error::AgentError::Plane {
                message: "model completion was not valid tool-call JSON: unterminated array"
                    .to_string(),
            }
        })?;
        return serde_json::from_str::<Vec<ModelToolCall>>(slice)
            .map(|tool_calls| {
                Some(ModelJson {
                    final_text: None,
                    tool_calls,
                })
            })
            .map_err(|e| crate::agents::error::AgentError::Plane {
                message: format!("model completion was not valid tool-call JSON: {e}"),
            });
    }
    if trimmed.starts_with('{') {
        let slice = json_value_slice(trimmed, 0).ok_or_else(|| {
            crate::agents::error::AgentError::Plane {
                message: "model completion was not valid agent JSON: unterminated object"
                    .to_string(),
            }
        })?;
        return serde_json::from_str(slice).map(Some).map_err(|e| {
            crate::agents::error::AgentError::Plane {
                message: format!("model completion was not valid agent JSON: {e}"),
            }
        });
    }
    for (idx, ch) in trimmed.char_indices() {
        if ch != '{' {
            continue;
        }
        let Some(slice) = json_value_slice(trimmed, idx) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(slice) else {
            continue;
        };
        let has_agent_shape = value.get("final").is_some() || value.get("tool_calls").is_some();
        if !has_agent_shape {
            continue;
        }
        return serde_json::from_value(value).map(Some).map_err(|e| {
            crate::agents::error::AgentError::Plane {
                message: format!("model completion was not valid agent JSON: {e}"),
            }
        });
    }
    Ok(None)
}

pub(super) fn parse_tool_calls_wrapper(trimmed: &str) -> crate::agents::error::Result<Option<ModelJson>> {
    // Only a completion that STARTS with the `tool_calls:` token is the
    // non-native wrapper protocol. A prose answer that merely mentions the
    // substring (e.g. a persona explaining the response format) must fall
    // through to the normal object/array parsing in the caller, which already
    // handles a clean `{"final":...}` and the top-level `{"tool_calls":[...]}`
    // object-key form.
    if !trimmed.starts_with("tool_calls:") {
        return Ok(None);
    }
    let Some(array_start) = trimmed.find('[') else {
        return Ok(None);
    };
    let Some(slice) = json_value_slice(trimmed, array_start) else {
        return Ok(None);
    };
    let calls: Vec<ModelToolCall> =
        serde_json::from_str(slice).map_err(|e| crate::agents::error::AgentError::Plane {
            message: format!("model completion was not valid tool_calls JSON: {e}"),
        })?;
    Ok(Some(ModelJson {
        final_text: None,
        tool_calls: calls,
    }))
}
