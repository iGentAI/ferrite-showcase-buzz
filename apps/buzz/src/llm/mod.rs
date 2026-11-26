//! The model-provider boundary: [`complete`] and [`model_info`] are plain async
//! functions called in-process by the agent runtime (`crate::agents::turns`),
//! with no client-key gate because the only caller is this application.
//!
//! Holds the model catalog, per-model route overrides and the default-model key
//! in the `gateway-config` store, provider credentials injected as secrets
//! (`provider-key-{provider}`) with the store fallback, Anthropic/OpenAI/Fireworks
//! wire building and parsing, prompt-size and reply-size bounds, and cache
//! accounting. Providers are reached ONLY through the app's scoped
//! `provider-egress` handle; the egress plane's destination allowlist, not this
//! module, is the authority on where bytes may go.

use std::io;

use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::{serde_json, AppContext, EgressOutcome};

use crate::relay::planes;

/// Store binding holding routing overrides, the default model, and the
/// operator fallback for provider credentials.
pub const CONFIG: &str = "gateway-config";
/// Egress binding scoped to the model providers.
pub const PROVIDER_EGRESS: &str = "provider-egress";
const DEFAULT_MODEL_KEY: &[u8] = b"default-model";
const DEFAULT_MODEL: &str = "claude-opus-4-8";
const ROUTE_PREFIX: &str = "model-route\x1f";
const MAX_PROVIDER_ERROR_BYTES: usize = 1024;
const MAX_PROMPT_BYTES: usize = 1_000_000;
const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;

/// One completion request. `prompt` is the string path (used by
/// summarization); `messages` + `tools` is the native tool-use path the agent
/// loop drives. It carries no client key: the caller is this app.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ChatRequest {
    pub model: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub system: Option<String>,
    /// Native tool-use: a PROVIDER-NEUTRAL message list. When present, the
    /// provider request is built from it (with `tools`) instead of wrapping
    /// `prompt` in a single user turn, and structured `tool_calls` come back.
    #[serde(default)]
    pub messages: Option<Vec<NeutralMsg>>,
    /// Tool schemas offered to the model. Only consulted when `messages` is
    /// present.
    #[serde(default)]
    pub tools: Option<Vec<ToolSpec>>,
}

/// One provider-neutral chat message. Roles: `system`, `user`, `assistant`,
/// `tool`. An assistant turn may carry `tool_calls`; a `tool` turn carries
/// the matching `tool_call_id` and the tool result as `content`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct NeutralMsg {
    pub role: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<NeutralToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

/// One tool call inside an assistant message on the neutral path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct NeutralToolCall {
    pub id: String,
    pub name: String,
    /// JSON-encoded arguments.
    pub arguments: String,
}

/// A tool offered to the model on the native tool-use path. Provider-neutral;
/// mapped to Anthropic `tools` / OpenAI `functions` shape at build time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema for the arguments (an object schema).
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

/// A tool call the model emitted on the native path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ToolCallOut {
    pub id: String,
    pub name: String,
    /// Raw JSON-encoded arguments, as the provider returned them.
    pub arguments: String,
}

/// One completion reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ChatReply {
    pub model: String,
    pub completion: String,
    pub usage: Usage,
    pub finish_reason: String,
    pub context_window: u32,
    pub cached: bool,
    /// Structured tool calls (native path). Empty on the prompt-string path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallOut>,
}

/// What a model id resolves to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ModelReply {
    pub model: String,
    pub provider: String,
    pub max_output_tokens: u32,
    pub context_window: u32,
    /// This boundary understands the native tool-use protocol (neutral
    /// `messages` + `tools` in, structured `tool_calls` out).
    pub native_tools: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(crate = "ferrite_app::serde")]
struct RouteOverride {
    provider: String,
    base_url: String,
    #[serde(default)]
    wire_model: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Debug, Clone, Copy)]
struct ModelSpec {
    id: &'static str,
    wire_id: Option<&'static str>,
    provider: &'static str,
    base_url: &'static str,
    max_output_tokens: u32,
    context_window: u32,
}

#[derive(Debug, Clone)]
struct Route {
    public_model: String,
    wire_model: String,
    provider: String,
    base_url: String,
    max_output_tokens: u32,
    context_window: u32,
}

/// Token accounting for one completion, including provider cache reads/writes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_write_tokens: u32,
    #[serde(default)]
    pub cost_micros: i64,
}

impl Usage {
    fn used_cache(&self) -> bool {
        self.cache_read_tokens > 0 || self.cache_write_tokens > 0
    }
}

#[derive(Debug, Clone)]
struct ProviderRequest {
    path: &'static str,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
struct ProviderReply {
    model: String,
    content: String,
    usage: Usage,
    finish_reason: String,
    tool_calls: Vec<ToolCallOut>,
}

static MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "claude-fable-5",
        wire_id: None,
        provider: "anthropic",
        base_url: "https://api.anthropic.com",
        max_output_tokens: 128_000,
        context_window: 1_000_000,
    },
    ModelSpec {
        id: "claude-opus-5",
        wire_id: None,
        provider: "anthropic",
        base_url: "https://api.anthropic.com",
        max_output_tokens: 128_000,
        context_window: 1_000_000,
    },
    ModelSpec {
        id: "claude-opus-4-8",
        wire_id: None,
        provider: "anthropic",
        base_url: "https://api.anthropic.com",
        max_output_tokens: 128_000,
        context_window: 1_000_000,
    },
    ModelSpec {
        id: "claude-sonnet-4-6",
        wire_id: None,
        provider: "anthropic",
        base_url: "https://api.anthropic.com",
        max_output_tokens: 64_000,
        context_window: 1_000_000,
    },
    ModelSpec {
        id: "claude-sonnet-5",
        wire_id: None,
        provider: "anthropic",
        base_url: "https://api.anthropic.com",
        max_output_tokens: 128_000,
        context_window: 1_000_000,
    },
    ModelSpec {
        id: "claude-haiku-4-5",
        wire_id: None,
        provider: "anthropic",
        base_url: "https://api.anthropic.com",
        max_output_tokens: 64_000,
        context_window: 200_000,
    },
    ModelSpec {
        id: "gpt-5.5",
        wire_id: None,
        provider: "openai",
        base_url: "https://api.openai.com",
        max_output_tokens: 128_000,
        context_window: 1_050_000,
    },
    ModelSpec {
        id: "gpt-5.6-sol",
        wire_id: None,
        provider: "openai",
        base_url: "https://api.openai.com",
        max_output_tokens: 128_000,
        context_window: 1_050_000,
    },
    ModelSpec {
        id: "gpt-5.6-terra",
        wire_id: None,
        provider: "openai",
        base_url: "https://api.openai.com",
        max_output_tokens: 128_000,
        context_window: 1_050_000,
    },
    ModelSpec {
        id: "gpt-5.6-luna",
        wire_id: None,
        provider: "openai",
        base_url: "https://api.openai.com",
        max_output_tokens: 128_000,
        context_window: 400_000,
    },
    ModelSpec {
        id: "gpt-5.4-mini",
        wire_id: None,
        provider: "openai",
        base_url: "https://api.openai.com",
        max_output_tokens: 128_000,
        context_window: 400_000,
    },
    ModelSpec {
        id: "gpt-5-nano",
        wire_id: None,
        provider: "openai",
        base_url: "https://api.openai.com",
        max_output_tokens: 128_000,
        context_window: 400_000,
    },
    ModelSpec {
        id: "glm-5.2",
        wire_id: Some("accounts/fireworks/models/glm-5p2"),
        provider: "fireworks",
        base_url: "https://api.fireworks.ai/inference",
        max_output_tokens: 65_536,
        context_window: 1_000_000,
    },
    ModelSpec {
        id: "minimax-m3",
        wire_id: Some("accounts/fireworks/models/minimax-m3"),
        provider: "fireworks",
        base_url: "https://api.fireworks.ai/inference",
        max_output_tokens: 65_536,
        context_window: 1_000_000,
    },
    ModelSpec {
        id: "kimi-k2.7-code",
        wire_id: Some("accounts/fireworks/models/kimi-k2p7-code"),
        provider: "fireworks",
        base_url: "https://api.fireworks.ai/inference",
        max_output_tokens: 65_536,
        context_window: 1_000_000,
    },
];

/// Why a completion (or model lookup) failed. `status` is an
/// HTTP-shaped severity so callers can tell caller errors (4xx) from
/// dependency/provider failures (424/502).
#[derive(Debug)]
pub struct LlmFailure {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for LlmFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.code, self.status, self.message)
    }
}

impl From<LlmFailure> for io::Error {
    fn from(e: LlmFailure) -> Self {
        io::Error::other(e.to_string())
    }
}

impl LlmFailure {
    fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: 400,
            code,
            message: message.into(),
        }
    }

    fn failed_dependency(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: 424,
            code,
            message: message.into(),
        }
    }

    fn bad_gateway(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: 502,
            code,
            message: message.into(),
        }
    }
}

/// One model this deployment can route to — what a client's model picker
/// populates from (`GET /buzz/api/agent-models`).
///
/// The catalog is COMPILED (the [`MODELS`] table). Per-model routing
/// overrides in the `gateway-config` store change WHERE a model is reached,
/// never WHICH models exist, so a picker built from this list can never offer
/// an id [`complete`] would refuse as `unsupported_model` — the empty picker
/// this replaced was worse than a wrong one: it offered nothing at all.
#[derive(Debug, Clone, Copy)]
pub struct CatalogEntry {
    pub id: &'static str,
    pub provider: &'static str,
    pub max_output_tokens: u32,
    pub context_window: u32,
}

/// Every model this deployment can route to, in declaration order.
///
/// The COMPILED table. A per-model override can DISABLE a model, and
/// [`complete`] refuses a disabled one — so a client picker should populate
/// from [`routable_catalog`], which asks the config too.
#[must_use]
pub fn catalog() -> Vec<CatalogEntry> {
    MODELS
        .iter()
        .map(|m| CatalogEntry {
            id: m.id,
            provider: m.provider,
            max_output_tokens: m.max_output_tokens,
            context_window: m.context_window,
        })
        .collect()
}

/// Is `id` routable right now? An override may carry `enabled: false`, which
/// [`resolve_route`] refuses — a picker must not offer such a model. A config
/// READ failure resolves to routable: the compiled model is real, and an
/// unreachable store must not empty the picker.
async fn model_is_enabled(ctx: &AppContext, id: &str) -> bool {
    match read_route_override(ctx, id).await {
        Ok(Some(route)) => route.enabled != Some(false),
        Ok(None) | Err(_) => true,
    }
}

/// The models a client may actually pick: the compiled catalog minus any an
/// operator override has disabled.
pub async fn routable_catalog(ctx: &AppContext) -> Vec<CatalogEntry> {
    let mut out = Vec::new();
    for entry in catalog() {
        if model_is_enabled(ctx, entry.id).await {
            out.push(entry);
        }
    }
    out
}

/// The model an agent gets when it names none: the operator's `default-model`
/// row when it names a model this deployment can actually route to, else the
/// compiled default. Both guards matter — a typo'd or retired default would
/// otherwise be written into a new agent's definition and surface as
/// `unsupported_model` on that agent's FIRST turn, i.e. as a dead teammate
/// rather than a bad configuration value.
pub async fn configured_default_model(ctx: &AppContext) -> String {
    let configured = read_default_model(ctx)
        .await
        .ok()
        .filter(|model| !model.is_empty());
    if let Some(model) = configured {
        if catalog().iter().any(|m| m.id == model) && model_is_enabled(ctx, &model).await {
            return model;
        }
    }
    // The COMPILED default can itself be disabled by an override, so it is not
    // an unconditional fallback: pick the first model this deployment can
    // actually route to. Only when an override has disabled EVERYTHING does
    // the compiled id come back — there is nothing selectable to name, and
    // naming it keeps the eventual failure legible instead of empty.
    if model_is_enabled(ctx, DEFAULT_MODEL).await {
        return DEFAULT_MODEL.to_string();
    }
    routable_catalog(ctx)
        .await
        .first()
        .map_or_else(|| DEFAULT_MODEL.to_string(), |m| m.id.to_string())
}

/// Resolve what a model id maps to (provider, wire id, bounds). The
/// `native_tools` flag is always true: this boundary speaks the neutral
/// tool protocol by construction.
pub async fn model_info(ctx: &AppContext, model: &str) -> Result<ModelReply, LlmFailure> {
    let route = resolve_route(ctx, model).await?;
    Ok(ModelReply {
        model: route.public_model,
        provider: route.provider,
        max_output_tokens: route.max_output_tokens,
        context_window: route.context_window,
        native_tools: true,
    })
}

/// Bytes of the caller-controlled `system` + `tools` fields (each forwarded
/// verbatim to the provider), for the request size guard.
fn system_and_tools_bytes(body: &ChatRequest) -> usize {
    body.system.as_deref().map_or(0, str::len)
        + body.tools.as_ref().map_or(0, |ts| {
            ts.iter()
                .map(|t| {
                    t.name.len()
                        + t.description.len()
                        + serde_json::to_string(&t.input_schema)
                            .map(|s| s.len())
                            .unwrap_or(0)
                })
                .sum::<usize>()
        })
}

/// One model completion over the app's scoped provider egress.
pub async fn complete(ctx: &AppContext, body: ChatRequest) -> Result<ChatReply, LlmFailure> {
    // Native tool-use path when `messages` is present; else the prompt-string
    // path (which requires a non-empty prompt, exactly as before).
    let native = body.messages.as_ref().is_some_and(|m| !m.is_empty());
    if !native {
        if body.prompt.is_empty() {
            return Err(LlmFailure::bad_request("empty_prompt", "prompt is empty"));
        }
        // Bound the WHOLE caller-controlled payload, not just `prompt`:
        // `system` and `tools` are also forwarded to the provider verbatim.
        let payload = body.prompt.len() + system_and_tools_bytes(&body);
        if payload > MAX_PROMPT_BYTES {
            return Err(LlmFailure::bad_request(
                "prompt_too_large",
                format!("request is {payload} bytes; limit is {MAX_PROMPT_BYTES}"),
            ));
        }
    } else {
        // Native path: bound message content + tool-call args PLUS the tool
        // schemas and system (each a caller-controlled field forwarded to the
        // provider). Missing any of these leaves the largest surface
        // unbounded.
        let approx: usize = body
            .messages
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| {
                m.content.len()
                    + m.tool_calls
                        .iter()
                        .map(|t| t.arguments.len() + t.name.len())
                        .sum::<usize>()
            })
            .sum::<usize>()
            + system_and_tools_bytes(&body);
        if approx > MAX_PROMPT_BYTES {
            return Err(LlmFailure::bad_request(
                "prompt_too_large",
                format!("request is {approx} bytes; limit is {MAX_PROMPT_BYTES}"),
            ));
        }
    }
    let route = resolve_route(ctx, &body.model).await?;
    let api_key = provider_key(ctx, &route.provider).await?;
    let max_tokens = body.max_tokens.unwrap_or(4096).min(route.max_output_tokens);
    let messages = body.messages.as_deref();
    let tools = body.tools.as_deref();
    let provider_request = match route.provider.as_str() {
        "anthropic" => build_anthropic(
            &route,
            &api_key,
            body.system.as_deref(),
            &body.prompt,
            max_tokens,
            messages,
            tools,
        )?,
        _ => build_openai(
            &route,
            &api_key,
            body.system.as_deref(),
            &body.prompt,
            max_tokens,
            messages,
            tools,
        )?,
    };
    let destination = format!(
        "{}{}",
        route.base_url.trim_end_matches('/'),
        provider_request.path
    );
    let mut egress = ctx.egress(PROVIDER_EGRESS).map_err(|e| {
        LlmFailure::failed_dependency("egress_binding", format!("provider egress missing: {e}"))
    })?;
    let outcome = egress
        .deliver_http(
            ferrite_app::EgressHttpMethod::Post,
            &destination,
            &crate::egress_headers(&provider_request.headers),
            &provider_request.body,
        )
        .await
        .map_err(|e| LlmFailure::bad_gateway("egress_transport", e.to_string()))?;
    let bytes = match outcome {
        EgressOutcome::Delivered { status, body, .. } if (200..300).contains(&status) => {
            if body.len() > MAX_REPLY_BYTES {
                return Err(LlmFailure::bad_gateway(
                    "provider_reply_too_large",
                    format!("provider reply is {} bytes", body.len()),
                ));
            }
            body
        }
        EgressOutcome::Delivered { status, body, .. } => {
            return Err(LlmFailure::bad_gateway(
                "provider_status",
                format!(
                    "provider returned {status}: {}",
                    excerpt(&body, MAX_PROVIDER_ERROR_BYTES)
                ),
            ));
        }
        EgressOutcome::Denied { reason } => {
            return Err(LlmFailure::failed_dependency("egress_denied", reason));
        }
    };
    let provider_reply = match route.provider.as_str() {
        "anthropic" => parse_anthropic(&bytes)?,
        _ => parse_openai(&bytes)?,
    };
    let cached = provider_reply.usage.used_cache();
    Ok(ChatReply {
        model: if provider_reply.model.is_empty() {
            route.public_model
        } else {
            provider_reply.model
        },
        completion: provider_reply.content,
        usage: provider_reply.usage,
        finish_reason: provider_reply.finish_reason,
        context_window: route.context_window,
        cached,
        tool_calls: provider_reply.tool_calls,
    })
}

/// The per-app SECRET a provider credential may be injected as, e.g.
/// `provider-key-anthropic`. Secrets are sealed at rest and injected into the
/// app's context at deploy (`ctx.secret`), which is the platform's intended
/// home for a credential.
fn provider_key_secret(provider: &str) -> String {
    format!("provider-key-{provider}")
}

/// Normalize a candidate credential: trim surrounding whitespace and reject an
/// empty result. Returns the owned key only when it carries content.
fn accept_provider_secret(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Resolve a provider credential from the sealed secrets plane ONLY.
///
/// Provider API keys are real, billable credentials, so they live exclusively
/// in the platform's secrets surface: sealed at rest and injected at deploy as
/// `ctx.secret("provider-key-<provider>")`. There is deliberately no config
/// store fallback — the config store is not sealed at rest, so a plaintext key
/// row would be recoverable from a store dump. Configure a key by sealing it
/// with `set_secret` and redeploying the source bundle.
async fn provider_key(ctx: &AppContext, provider: &str) -> Result<String, LlmFailure> {
    ctx.secret(&provider_key_secret(provider))
        .as_deref()
        .and_then(accept_provider_secret)
        .ok_or_else(|| {
            LlmFailure::failed_dependency(
                "provider_key_missing",
                format!(
                    "missing sealed provider credential for {provider}; \
                     seal secret 'provider-key-{provider}' and redeploy"
                ),
            )
        })
}

async fn resolve_route(ctx: &AppContext, requested: &str) -> Result<Route, LlmFailure> {
    let trimmed = requested.trim();
    let model = if trimmed.is_empty() || trimmed == "agent-default" {
        // A failed config read still surfaces as a dependency failure; a
        // missing, retired or disabled default falls back the way agent
        // creation and the model list do.
        read_default_model(ctx).await?;
        configured_default_model(ctx).await
    } else {
        trimmed.to_string()
    };
    if !model
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/' | ':'))
    {
        return Err(LlmFailure::bad_request("bad_model", "invalid model id"));
    }
    let spec = MODELS.iter().find(|m| m.id == model).ok_or_else(|| {
        LlmFailure::bad_request("unsupported_model", format!("unsupported model '{model}'"))
    })?;
    let override_entry = read_route_override(ctx, &model).await?;
    let (provider, base_url, wire_model) = match override_entry {
        Some(route) => {
            if route.enabled == Some(false) {
                return Err(LlmFailure::bad_request(
                    "model_disabled",
                    format!("model '{model}' is disabled"),
                ));
            }
            (
                route.provider,
                route.base_url,
                route
                    .wire_model
                    .unwrap_or_else(|| spec.wire_id.unwrap_or(spec.id).to_string()),
            )
        }
        None => (
            spec.provider.to_string(),
            spec.base_url.to_string(),
            spec.wire_id.unwrap_or(spec.id).to_string(),
        ),
    };
    Ok(Route {
        public_model: model,
        wire_model,
        provider,
        base_url,
        max_output_tokens: spec.max_output_tokens,
        context_window: spec.context_window,
    })
}

async fn read_default_model(ctx: &AppContext) -> Result<String, LlmFailure> {
    let mut store = planes::store(ctx, CONFIG).map_err(|e| {
        LlmFailure::failed_dependency("config_binding", format!("gateway config missing: {e}"))
    })?;
    match store.get(DEFAULT_MODEL_KEY).await {
        Ok(Some(raw)) => String::from_utf8(raw)
            .map(|s| s.trim().to_string())
            .map_err(|e| LlmFailure::failed_dependency("config_read", e.to_string())),
        Ok(None) => Ok(DEFAULT_MODEL.to_string()),
        Err(e) => Err(LlmFailure::failed_dependency("config_read", e.to_string())),
    }
}

async fn read_route_override(
    ctx: &AppContext,
    model: &str,
) -> Result<Option<RouteOverride>, LlmFailure> {
    let mut store = planes::store(ctx, CONFIG).map_err(|e| {
        LlmFailure::failed_dependency("config_binding", format!("gateway config missing: {e}"))
    })?;
    let key = format!("{ROUTE_PREFIX}{model}");
    match store.get(key.as_bytes()).await {
        Ok(Some(raw)) => serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|e| LlmFailure::failed_dependency("config_decode", e.to_string())),
        Ok(None) => Ok(None),
        Err(e) => Err(LlmFailure::failed_dependency("config_read", e.to_string())),
    }
}

/// Parse a JSON-encoded arguments string into an object Value (Anthropic
/// `tool_use.input` must be an object); malformed args fall back to `{}`.
fn args_object(arguments: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(v) if v.is_object() => v,
        _ => serde_json::json!({}),
    }
}

/// Translate the neutral conversation to Anthropic content-block messages.
/// System is carried separately (the `system` arg), so system-role entries
/// are skipped here. Caching is primed on the last content block.
/// Append `block` to the trailing user message, or start one. Anthropic needs
/// every `tool_result` answering one assistant turn's `tool_use` blocks in the
/// single user message that follows it, and strictly alternating roles; one
/// user message per parallel tool result was refused, wedging the session.
fn push_user_block(out: &mut Vec<serde_json::Value>, block: serde_json::Value) {
    if let Some(last) = out.last_mut() {
        if last["role"] == "user" {
            if let Some(content) = last["content"].as_array_mut() {
                content.push(block);
                return;
            }
        }
    }
    out.push(serde_json::json!({ "role": "user", "content": [block] }));
}

fn anthropic_messages(ms: &[NeutralMsg]) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    for m in ms {
        match m.role.as_str() {
            "user" => push_user_block(
                &mut out,
                serde_json::json!({ "type": "text", "text": m.content }),
            ),
            "assistant" => {
                let mut blocks = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(serde_json::json!({ "type": "text", "text": m.content }));
                }
                for tc in &m.tool_calls {
                    blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": args_object(&tc.arguments),
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(serde_json::json!({ "type": "text", "text": "" }));
                }
                out.push(serde_json::json!({ "role": "assistant", "content": blocks }));
            }
            "tool" => push_user_block(
                &mut out,
                serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content,
                }),
            ),
            _ => {} // system carried separately; unknown roles dropped
        }
    }
    // Anthropic does not accept a text-only assistant message as the final
    // item in a request: it treats that shape as an unsupported assistant
    // prefill. Durable Buzz sessions can legitimately end on a previous
    // assistant response before a resumed tool loop asks the model to keep
    // working, so make that continuation explicit. Do not paper over a
    // dangling assistant tool_use; that must still be paired with its real
    // tool_result by the session fold.
    let ends_with_text_assistant = ms
        .iter()
        .rev()
        .find(|m| matches!(m.role.as_str(), "user" | "assistant" | "tool"))
        .is_some_and(|m| m.role == "assistant" && m.tool_calls.is_empty());
    if ends_with_text_assistant {
        out.push(serde_json::json!({
            "role": "user",
            "content": [{
                "type": "text",
                "text": "Continue from the previous response and take the next concrete action."
            }]
        }));
    }
    // Prime the cache on the last content block of the last message.
    if let Some(last) = out.last_mut() {
        if let Some(arr) = last["content"].as_array_mut() {
            if let Some(block) = arr.last_mut() {
                block["cache_control"] = serde_json::json!({ "type": "ephemeral" });
            }
        }
    }
    out
}

/// Translate the neutral conversation to OpenAI chat messages, prepending the
/// system message.
fn openai_messages(system: Option<&str>, ms: &[NeutralMsg]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if let Some(system) = system {
        if !system.is_empty() {
            out.push(serde_json::json!({ "role": "system", "content": system }));
        }
    }
    for m in ms {
        match m.role.as_str() {
            "user" => out.push(serde_json::json!({ "role": "user", "content": m.content })),
            "assistant" => {
                if m.tool_calls.is_empty() {
                    out.push(serde_json::json!({ "role": "assistant", "content": m.content }));
                } else {
                    out.push(serde_json::json!({
                        "role": "assistant",
                        "content": if m.content.is_empty() { serde_json::Value::Null } else { serde_json::json!(m.content) },
                        "tool_calls": m.tool_calls.iter().map(|tc| serde_json::json!({
                            "id": tc.id,
                            "type": "function",
                            "function": { "name": tc.name, "arguments": tc.arguments },
                        })).collect::<Vec<_>>(),
                    }));
                }
            }
            "tool" => out.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
                "content": m.content,
            })),
            _ => {}
        }
    }
    out
}

fn build_anthropic(
    route: &Route,
    api_key: &str,
    system: Option<&str>,
    prompt: &str,
    max_tokens: u32,
    messages: Option<&[NeutralMsg]>,
    tools: Option<&[ToolSpec]>,
) -> Result<ProviderRequest, LlmFailure> {
    // Native path: translate the neutral list to Anthropic content blocks;
    // else the single cache-primed user turn.
    let msgs = match messages {
        Some(ms) => serde_json::Value::Array(anthropic_messages(ms)),
        None => serde_json::json!([{
            "role": "user",
            "content": [{
                "type": "text",
                "text": prompt,
                "cache_control": { "type": "ephemeral" }
            }]
        }]),
    };
    let mut body = serde_json::json!({
        "model": route.wire_model,
        "max_tokens": max_tokens,
        "messages": msgs,
        "cache_control": { "type": "ephemeral" },
        "stream": false
    });
    if let Some(tools) = tools {
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(
                tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": if t.input_schema.is_object() {
                                t.input_schema.clone()
                            } else {
                                serde_json::json!({ "type": "object", "properties": {} })
                            },
                        })
                    })
                    .collect(),
            );
        }
    }
    if let Some(system) = system {
        if !system.is_empty() {
            body["system"] = serde_json::json!([{
                "type": "text",
                "text": system,
                "cache_control": { "type": "ephemeral" }
            }]);
        }
    }
    let body = serde_json::to_vec(&body)
        .map_err(|e| LlmFailure::bad_request("encode_provider_request", e.to_string()))?;
    Ok(ProviderRequest {
        path: "/v1/messages",
        body,
        headers: vec![
            ("x-api-key".to_string(), api_key.to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ],
    })
}

fn parse_anthropic(body: &[u8]) -> Result<ProviderReply, LlmFailure> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| LlmFailure::bad_gateway("provider_json", e.to_string()))?;
    if v.get("type").and_then(|t| t.as_str()) == Some("error") {
        return Err(LlmFailure::bad_gateway(
            "provider_error",
            excerpt(body, MAX_PROVIDER_ERROR_BYTES),
        ));
    }
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    if let Some(blocks) = v.get("content").and_then(|c| c.as_array()) {
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        content.push_str(text);
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(ToolCallOut {
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name: block["name"].as_str().unwrap_or_default().to_string(),
                        arguments: block
                            .get("input")
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| "{}".to_string()),
                    });
                }
                _ => {}
            }
        }
    }
    // On the native path a turn can be tool_calls-only (no text) — that is a
    // valid completion, not an error.
    if content.is_empty() && tool_calls.is_empty() {
        return Err(LlmFailure::bad_gateway(
            "provider_shape",
            "Anthropic response had no text or tool content",
        ));
    }
    let usage = &v["usage"];
    Ok(ProviderReply {
        model: v["model"].as_str().unwrap_or_default().to_string(),
        content,
        usage: Usage {
            input_tokens: usage["input_tokens"].as_u64().unwrap_or(0) as u32,
            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0) as u32,
            cache_read_tokens: usage["cache_read_input_tokens"].as_u64().unwrap_or(0) as u32,
            cache_write_tokens: usage["cache_creation_input_tokens"].as_u64().unwrap_or(0) as u32,
            cost_micros: 0,
        },
        finish_reason: v["stop_reason"].as_str().unwrap_or_default().to_string(),
        tool_calls,
    })
}

fn build_openai(
    route: &Route,
    api_key: &str,
    system: Option<&str>,
    prompt: &str,
    max_tokens: u32,
    messages_in: Option<&[NeutralMsg]>,
    tools: Option<&[ToolSpec]>,
) -> Result<ProviderRequest, LlmFailure> {
    let messages = match messages_in {
        Some(ms) => openai_messages(system, ms),
        None => {
            let mut m = Vec::new();
            if let Some(system) = system {
                if !system.is_empty() {
                    m.push(serde_json::json!({ "role": "system", "content": system }));
                }
            }
            m.push(serde_json::json!({ "role": "user", "content": prompt }));
            m
        }
    };
    let mut body = serde_json::json!({
        "model": route.wire_model,
        "max_completion_tokens": max_tokens,
        "messages": messages,
        "stream": false
    });
    if let Some(tools) = tools {
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(
                tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": if t.input_schema.is_object() {
                                    t.input_schema.clone()
                                } else {
                                    serde_json::json!({ "type": "object", "properties": {} })
                                },
                            }
                        })
                    })
                    .collect(),
            );
            // GPT-5.6 Chat Completions supports function tools only with
            // effective reasoning `none`. The agent runtime uses this native
            // tool path, so make the compatibility requirement explicit
            // instead of inheriting the family default (`medium`).
            if route.public_model.starts_with("gpt-5.6-") {
                body["reasoning_effort"] = serde_json::json!("none");
            }
        }
    }
    let body = serde_json::to_vec(&body)
        .map_err(|e| LlmFailure::bad_request("encode_provider_request", e.to_string()))?;
    Ok(ProviderRequest {
        path: "/v1/chat/completions",
        body,
        headers: vec![
            ("authorization".to_string(), format!("Bearer {api_key}")),
            ("content-type".to_string(), "application/json".to_string()),
        ],
    })
}

fn parse_openai(body: &[u8]) -> Result<ProviderReply, LlmFailure> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| LlmFailure::bad_gateway("provider_json", e.to_string()))?;
    if v.get("error").is_some() {
        return Err(LlmFailure::bad_gateway(
            "provider_error",
            excerpt(body, MAX_PROVIDER_ERROR_BYTES),
        ));
    }
    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| {
            LlmFailure::bad_gateway("provider_shape", "OpenAI response had no choices")
        })?;
    let content = choice["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mut tool_calls = Vec::new();
    if let Some(calls) = choice["message"]["tool_calls"].as_array() {
        for call in calls {
            let f = &call["function"];
            tool_calls.push(ToolCallOut {
                id: call["id"].as_str().unwrap_or_default().to_string(),
                name: f["name"].as_str().unwrap_or_default().to_string(),
                // OpenAI returns arguments as a JSON *string*; pass through.
                arguments: f["arguments"].as_str().unwrap_or("{}").to_string(),
            });
        }
    }
    if content.is_empty() && tool_calls.is_empty() {
        return Err(LlmFailure::bad_gateway(
            "provider_shape",
            "OpenAI response had no text or tool content",
        ));
    }
    let usage = &v["usage"];
    let cached = usage["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0) as u32;
    let prompt_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0) as u32;
    Ok(ProviderReply {
        model: v["model"].as_str().unwrap_or_default().to_string(),
        content,
        usage: Usage {
            input_tokens: prompt_tokens.saturating_sub(cached),
            output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0) as u32,
            cache_read_tokens: cached,
            cache_write_tokens: 0,
            cost_micros: 0,
        },
        finish_reason: choice["finish_reason"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        tool_calls,
    })
}

fn excerpt(bytes: &[u8], max: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.len() <= max {
        s.into_owned()
    } else {
        let mut cut = max;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &s[..cut])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_provider_secret_trims_and_rejects_empty() {
        assert_eq!(
            accept_provider_secret("  sk-live-abc123  "),
            Some("sk-live-abc123".to_string())
        );
        assert_eq!(
            accept_provider_secret("sk-plain"),
            Some("sk-plain".to_string())
        );
        assert_eq!(accept_provider_secret(""), None);
        assert_eq!(accept_provider_secret("   \t\n  "), None);
    }

    #[test]
    fn anthropic_usage_marks_cache_read_and_write_tokens() {
        let body = br#"{
            "model": "claude-opus-4-8",
            "content": [{ "type": "text", "text": "ok" }],
            "usage": {
                "input_tokens": 120,
                "output_tokens": 9,
                "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 20
            },
            "stop_reason": "end_turn"
        }"#;

        let reply = parse_anthropic(body).expect("anthropic response should parse");

        assert_eq!(reply.usage.input_tokens, 120);
        assert_eq!(reply.usage.cache_read_tokens, 100);
        assert_eq!(reply.usage.cache_write_tokens, 20);
        assert!(reply.usage.used_cache());
    }

    #[test]
    fn openai_usage_marks_cached_prompt_tokens() {
        let body = br#"{
            "model": "gpt-5.5",
            "choices": [{
                "message": { "content": "ok" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 120,
                "completion_tokens": 9,
                "prompt_tokens_details": { "cached_tokens": 75 }
            }
        }"#;

        let reply = parse_openai(body).expect("openai response should parse");

        assert_eq!(reply.usage.input_tokens, 45);
        assert_eq!(reply.usage.cache_read_tokens, 75);
        assert_eq!(reply.usage.cache_write_tokens, 0);
        assert!(reply.usage.used_cache());
    }

    #[test]
    fn uncached_usage_stays_uncached() {
        let usage = Usage {
            input_tokens: 12,
            output_tokens: 3,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_micros: 0,
        };

        assert!(!usage.used_cache());
    }

    fn route() -> Route {
        Route {
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            wire_model: "claude-opus-4-8".to_string(),
            public_model: "claude-opus-4-8".to_string(),
            max_output_tokens: 4096,
            context_window: 1_000_000,
        }
    }

    fn body_json(req: &ProviderRequest) -> serde_json::Value {
        serde_json::from_slice(&req.body).expect("request body is json")
    }

    #[test]
    fn prompt_path_request_is_unchanged_by_native_fields() {
        // No messages -> exactly the single cache-primed user turn, and no
        // tools key.
        let mut r = route();
        r.provider = "anthropic".into();
        let a = build_anthropic(&r, "k", Some("sys"), "hello", 100, None, None).unwrap();
        let j = body_json(&a);
        // Full prompt-path Anthropic request shape (a dropped cache primer must fail).
        assert_eq!(
            j,
            serde_json::json!({
                "model": "claude-opus-4-8",
                "max_tokens": 100,
                "messages": [{
                    "role": "user",
                    "content": [{ "type": "text", "text": "hello", "cache_control": { "type": "ephemeral" } }]
                }],
                "cache_control": { "type": "ephemeral" },
                "stream": false,
                "system": [{ "type": "text", "text": "sys", "cache_control": { "type": "ephemeral" } }]
            }),
            "prompt-path anthropic request must keep the cache-primed shape"
        );
        r.provider = "openai".into();
        let o = build_openai(&r, "k", Some("sys"), "hello", 100, None, None).unwrap();
        let j = body_json(&o);
        assert_eq!(
            j,
            serde_json::json!({
                "model": "claude-opus-4-8",
                "max_completion_tokens": 100,
                "messages": [
                    { "role": "system", "content": "sys" },
                    { "role": "user", "content": "hello" }
                ],
                "stream": false
            }),
            "prompt-path openai request must keep the plain chat shape"
        );
    }

    #[test]
    fn native_size_bound_counts_tools_and_system() {
        // A native request with tiny messages but a huge tool schema must be
        // rejected — the guard counts tools + system, not just messages.
        let big = "x".repeat(MAX_PROMPT_BYTES + 10);
        let body = ChatRequest {
            model: "claude-opus-4-8".into(),
            prompt: String::new(),
            max_tokens: None,
            system: None,
            messages: Some(vec![nmsg("user", "hi")]),
            tools: Some(vec![ToolSpec {
                name: "t".into(),
                description: String::new(),
                input_schema: serde_json::json!({ "type": "object", "blob": big }),
            }]),
        };
        assert!(
            system_and_tools_bytes(&body) > MAX_PROMPT_BYTES,
            "the tool schema alone must exceed the cap in this fixture"
        );
    }

    fn nmsg(role: &str, content: &str) -> NeutralMsg {
        NeutralMsg {
            role: role.into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    #[test]
    fn native_path_sends_messages_and_tools() {
        let msgs = vec![nmsg("user", "weather?")];
        let tools = vec![ToolSpec {
            name: "get_weather".into(),
            description: "look up weather".into(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "city": { "type": "string" } } }),
        }];
        let a = build_anthropic(&route(), "k", None, "", 100, Some(&msgs), Some(&tools)).unwrap();
        let j = body_json(&a);
        assert_eq!(j["messages"][0]["content"][0]["text"], "weather?");
        assert_eq!(j["tools"][0]["name"], "get_weather");
        assert_eq!(
            j["tools"][0]["input_schema"]["properties"]["city"]["type"],
            "string"
        );

        let mut r = route();
        r.provider = "openai".into();
        let o = build_openai(
            &r,
            "k",
            Some("be terse"),
            "",
            100,
            Some(&msgs),
            Some(&tools),
        )
        .unwrap();
        let j = body_json(&o);
        assert_eq!(j["messages"][0]["role"], "system");
        assert_eq!(j["messages"][1]["content"], "weather?");
        assert_eq!(j["tools"][0]["type"], "function");
        assert_eq!(j["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(j["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn the_catalog_is_pickable_and_every_entry_resolves() {
        let catalog = catalog();
        assert!(
            !catalog.is_empty(),
            "an empty catalog is an empty model picker"
        );
        // Every advertised id must be one `resolve_route` accepts, or the
        // picker would offer a model `complete` refuses as unsupported.
        for entry in &catalog {
            assert!(
                MODELS.iter().any(|m| m.id == entry.id),
                "catalog entry {} is not in the routing table",
                entry.id
            );
            assert!(
                entry.context_window > 0,
                "{} has no context window",
                entry.id
            );
            assert!(
                entry.max_output_tokens > 0,
                "{} has no output bound",
                entry.id
            );
        }
        // Ids are unique: a picker keyed by id must not show two rows that
        // route differently.
        let mut ids: Vec<&str> = catalog.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        let unique = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), unique, "duplicate model ids in the catalog");
        // The compiled default is itself pickable.
        assert!(
            catalog.iter().any(|e| e.id == DEFAULT_MODEL),
            "the default model {DEFAULT_MODEL} is not in the catalog"
        );
    }

    #[test]
    fn requested_agents_catalog_is_served() {
        for id in [
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "claude-fable-5",
            "claude-opus-5",
            "claude-sonnet-5",
        ] {
            assert!(MODELS.iter().any(|model| model.id == id), "missing {id}");
        }
    }

    #[test]
    fn gpt_5_6_native_tools_force_reasoning_none() {
        let mut r = route();
        r.provider = "openai".into();
        r.public_model = "gpt-5.6-sol".into();
        r.wire_model = "gpt-5.6-sol".into();
        let tools = vec![ToolSpec {
            name: "get_weather".into(),
            description: "look up weather".into(),
            input_schema: serde_json::json!({ "type": "object" }),
        }];
        let req = build_openai(
            &r,
            "k",
            None,
            "",
            100,
            Some(&[nmsg("user", "weather?")]),
            Some(&tools),
        )
        .unwrap();
        assert_eq!(body_json(&req)["reasoning_effort"], "none");
    }

    #[test]
    fn neutral_translation_round_trips_a_tool_turn() {
        // user -> assistant(tool_call) -> tool(result): both providers.
        let convo = vec![
            nmsg("user", "weather in Paris?"),
            NeutralMsg {
                role: "assistant".into(),
                content: "checking".into(),
                tool_calls: vec![NeutralToolCall {
                    id: "tc1".into(),
                    name: "get_weather".into(),
                    arguments: r#"{"city":"Paris"}"#.into(),
                }],
                tool_call_id: None,
            },
            NeutralMsg {
                role: "tool".into(),
                content: "21C sunny".into(),
                tool_calls: Vec::new(),
                tool_call_id: Some("tc1".into()),
            },
        ];
        // Anthropic: assistant tool_use block + user tool_result block.
        let a = anthropic_messages(&convo);
        assert_eq!(a[1]["content"][1]["type"], "tool_use");
        assert_eq!(a[1]["content"][1]["input"]["city"], "Paris");
        assert_eq!(a[2]["role"], "user");
        assert_eq!(a[2]["content"][0]["type"], "tool_result");
        assert_eq!(a[2]["content"][0]["tool_use_id"], "tc1");
        // Cache primed on the last block.
        assert_eq!(a[2]["content"][0]["cache_control"]["type"], "ephemeral");
        // OpenAI: assistant.tool_calls + tool role with tool_call_id.
        let o = openai_messages(None, &convo);
        assert_eq!(
            o[1]["tool_calls"][0]["function"]["arguments"],
            r#"{"city":"Paris"}"#
        );
        assert_eq!(o[2]["role"], "tool");
        assert_eq!(o[2]["tool_call_id"], "tc1");
    }

    /// Parallel tool calls answer in ONE user message: Anthropic refuses a
    /// tool_result split from its sibling results (finding_f5251519332611f4).
    #[test]
    fn anthropic_coalesces_a_batch_of_tool_results_into_one_user_message() {
        let tool = |id: &str, content: &str| NeutralMsg {
            role: "tool".into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
        };
        let call = |id: &str| NeutralToolCall {
            id: id.into(),
            name: "get_weather".into(),
            arguments: "{}".into(),
        };
        let convo = vec![
            nmsg("user", "weather in Paris and Rome?"),
            NeutralMsg {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: vec![call("a"), call("b")],
                tool_call_id: None,
            },
            tool("a", "21C"),
            tool("b", "25C"),
            nmsg("user", "and Oslo?"),
        ];
        let a = anthropic_messages(&convo);
        assert_eq!(a.len(), 3, "user, assistant, one user message: {a:?}");
        assert_eq!(a[2]["role"], "user");
        let blocks = a[2]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["tool_use_id"], "a");
        assert_eq!(blocks[1]["tool_use_id"], "b");
        assert_eq!(blocks[2]["type"], "text");
        let roles: Vec<&str> = a.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert!(roles.windows(2).all(|w| w[0] != w[1]), "roles alternate: {roles:?}");
    }

    #[test]
    fn anthropic_text_assistant_tail_becomes_an_explicit_continuation() {
        let convo = vec![
            nmsg("user", "Start the task"),
            nmsg("assistant", "I have inspected the current state."),
        ];

        let translated = anthropic_messages(&convo);

        assert_eq!(translated.len(), 3);
        assert_eq!(translated[1]["role"], "assistant");
        assert_eq!(translated[2]["role"], "user");
        assert_eq!(
            translated[2]["content"][0]["text"],
            "Continue from the previous response and take the next concrete action."
        );
        assert_eq!(
            translated[2]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn anthropic_does_not_mask_a_dangling_tool_use() {
        let convo = vec![
            nmsg("user", "Inspect the file"),
            NeutralMsg {
                role: "assistant".into(),
                content: "I will inspect it.".into(),
                tool_calls: vec![NeutralToolCall {
                    id: "tc1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"path":"README.md"}"#.into(),
                }],
                tool_call_id: None,
            },
        ];

        let translated = anthropic_messages(&convo);

        assert_eq!(translated.len(), 2);
        assert_eq!(translated[1]["role"], "assistant");
        assert_eq!(translated[1]["content"][1]["type"], "tool_use");
    }

    #[test]
    fn anthropic_tool_use_is_parsed_and_text_only_still_works() {
        let body = br#"{
            "model": "claude-opus-4-8",
            "content": [
                { "type": "text", "text": "let me check" },
                { "type": "tool_use", "id": "tu_1", "name": "get_weather", "input": { "city": "Paris" } }
            ],
            "usage": { "input_tokens": 10, "output_tokens": 5 },
            "stop_reason": "tool_use"
        }"#;
        let reply = parse_anthropic(body).unwrap();
        assert_eq!(reply.content, "let me check");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "tu_1");
        assert_eq!(reply.tool_calls[0].name, "get_weather");
        assert_eq!(reply.tool_calls[0].arguments, r#"{"city":"Paris"}"#);
        // A tool_calls-only turn (no text) is valid, not an error.
        let only = br#"{"model":"m","content":[{"type":"tool_use","id":"t","name":"n","input":{}}],"usage":{"input_tokens":1,"output_tokens":1},"stop_reason":"tool_use"}"#;
        assert_eq!(parse_anthropic(only).unwrap().tool_calls.len(), 1);
        // No content at all is still an error.
        let empty = br#"{"model":"m","content":[],"usage":{"input_tokens":1,"output_tokens":1},"stop_reason":"end_turn"}"#;
        assert!(parse_anthropic(empty).is_err());
    }

    #[test]
    fn openai_tool_calls_are_parsed_with_string_arguments() {
        let body = br#"{
            "model": "gpt-5.5",
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 4 }
        }"#;
        let reply = parse_openai(body).unwrap();
        assert!(reply.content.is_empty());
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].name, "get_weather");
        assert_eq!(reply.tool_calls[0].arguments, r#"{"city":"Paris"}"#);
    }
}
