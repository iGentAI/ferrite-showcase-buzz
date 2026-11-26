//! buzz's MCP surface: consuming third-party MCP servers, and being one.
//!
//! Both directions run over [`crate::mcp`], the source package, because a
//! deployable app may depend on exactly one crate and so cannot link an SDK.
//!
//! # As a CLIENT: the authenticate button
//!
//! The token exchange runs HERE, never in the browser. A tab must never hold a
//! client credential or a provider access token, so the flow is:
//!
//! 1. `POST /buzz/api/mcp/authorize` — discover the connector's Protected
//!    Resource Metadata, then its authorization server's metadata, derive a PKCE
//!    verifier and `state` by HMAC, record a [`creds::Pending`], answer with a
//!    URL.
//! 2. the user consents at the provider.
//! 3. `GET /buzz/api/mcp/callback` — validate `state`, validate `iss` against the
//!    issuer recorded from VALIDATED metadata, redeem the code with `resource`
//!    attached, store a [`creds::Credential`].
//!
//! **Connecting is operator-only and the credential is workspace-level.** That is
//! deliberate: connecting Linear grants every agent in this workspace access to
//! that Linear account, which is authority over an external system rather than a
//! personal preference. It also means the callback needs no member identity — the
//! single-use `state` is what proves the flow, which is just as well, since a
//! browser redirect carries no NIP-98 header.
//!
//! # As a SERVER: two read tools
//!
//! `POST /mcp` speaks 2026-07-28 and exposes `buzz_search` and
//! `buzz_read_channel`, each re-authorized per call through the same gates a
//! client read uses.
//!
//! No WRITE tool is exposed, and that is a limit rather than an oversight: a
//! message must be attributed to somebody, and this relay holds no member's
//! signing key. Posting as the relay would put a person's words under the
//! relay's identity. So the surface is read-only until a persona-scoped write
//! path exists, and [`SCOPES_SUPPORTED`] advertises accordingly.

use ferrite_app::client::StoreClusterClient;
use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json::{self, Value};
use ferrite_app::{AppContext, EgressHandle, IngressRequest};

use crate::mcp::client::{McpClient, NoInput};
use crate::mcp::fetch::{FetchError, HttpFetch, Response};
use crate::mcp::server::ResourceServer;
use crate::mcp::tokens::{Claims, SCOPE_READ, SCOPE_TOOLS};
use crate::mcp::{catalog, creds, oauth, proto, server, tokens};
use crate::relay::error::{BuzzError, Result};
use crate::relay::ingress::{Reply, HEADER_WWW_AUTHENTICATE};
use crate::relay::{planes, store};

/// The HMAC secret PKCE verifiers, `state` values and self-issued tokens are
/// derived under. Secret first, config-store fallback — the [`crate::relay::ingress`]
/// pattern, for the same reason: a dev deployment should not need a secrets round
/// trip.
const MCP_KEY_SECRET: &str = "buzz-mcp-key";
const MCP_KEY_CONFIG: &[u8] = b"buzz-mcp-key";

/// This deployment's canonical MCP resource identifier (RFC 8707), e.g.
/// `https://buzz.example.com/mcp`.
///
/// Operator-configured and FAIL-CLOSED. It is the audience of every token this
/// app mints and half of every credential key, so taking it from a request header
/// would let a caller choose it — and an unconfigured deployment must refuse
/// rather than invent one.
const MCP_RESOURCE_SECRET: &str = "buzz-mcp-resource";
const MCP_RESOURCE_CONFIG: &[u8] = b"buzz-mcp-resource";

/// The OAuth client identifier this deployment presents to a connector's
/// authorization server. A Client ID Metadata Document URL is preferred; a
/// registered client id also works.
const MCP_CLIENT_ID_SECRET: &str = "buzz-mcp-client-id";
const MCP_CLIENT_ID_CONFIG: &[u8] = b"buzz-mcp-client-id";

/// The scopes a client may request of THIS server.
///
/// No `mcp:write`, because no exposed tool mutates. Advertising a scope nothing
/// uses would invite a client to request authority this server cannot honour.
pub const SCOPES_SUPPORTED: &[&str] = &[SCOPE_READ, SCOPE_TOOLS];

/// How long a self-issued access token lives.
const TOKEN_TTL_MS: u64 = 60 * 60 * 1000;

/// Freshness hint on `server/discover`.
const DISCOVER_TTL_MS: u64 = 5 * 60 * 1000;

/// Freshness hint on `tools/list` (SEP-2549).
///
/// This server's tool surface is a compile-time constant, so a longer window
/// would also be honest; five minutes matches discovery so a client refreshes
/// both together rather than on two schedules.
const TOOLS_TTL_MS: u64 = 5 * 60 * 1000;

/// Upper bound on a tool result relayed back to a caller.
const MAX_TOOL_RESULT_BYTES: usize = 256 * 1024;

/// Upper bound on the tools one connector listing aggregates across pages.
const MAX_CONNECTOR_TOOLS: usize = 200;

/// Pages of `tools/list` one listing will follow — an aggregate cap, so a
/// server that keeps handing out cursors cannot walk this relay's egress
/// budget page by page.
const MAX_TOOL_PAGES: usize = 16;

/// The version this app reports as its MCP client and server identity.
///
/// A LITERAL, not `env!("CARGO_PKG_VERSION")`: the platform's code gate refuses a
/// build-time environment read in a deployed app (`CODE_BUILD_TIME_READ`) — the
/// same reason `relay_info` spells its NIP-11 version out. One constant rather
/// than two call sites, so the client and the server cannot disagree about who
/// they are.
const IMPLEMENTATION_VERSION: &str = "0.1.0";

/// The index row that makes a credential findable by connector.
///
/// A credential is keyed by **(issuer, resource)** — that is what stops a token
/// minted for one resource being presented to another, and it is not something to
/// give up. But neither value is known from a catalog id alone: the issuer comes
/// from the connector's own metadata and is discovered during authorization.
///
/// So the validated pair is recorded here at the moment it is learned, and every
/// lookup resolves through it. Without this row the store and the lookups used
/// different keys, and nothing was ever found.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
struct ConnectorLink {
    issuer: String,
    resource: String,
}

fn link_key(community: &str, connector: &str) -> String {
    format!("mcpidx/{community}/{connector}")
}

/// One in-flight authorization.
///
/// Wraps the package's [`creds::Pending`] with what buzz needs on top: which
/// catalog connector it belongs to, the SERVER-allocated nonce the PKCE verifier
/// was derived from, and whether it has been redeemed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
struct Flow {
    connector: String,
    /// `{connector}:{nonce}` — the exact string the verifier and state were
    /// derived from, stored rather than recomputed so the two can never diverge.
    flow_id: String,
    /// Set once redeemed. A replay reads this and is refused.
    #[serde(default)]
    consumed: bool,
    /// WHICH request consumed it — the gateway's per-request correlation id.
    ///
    /// This exists because of the store's documented ambiguous-retry contract: a
    /// conditional write can report a mismatch AFTER it was applied, when the
    /// response was lost. Treating every mismatch as "somebody else won" would
    /// permanently consume a flow this caller had in fact claimed, stranding a
    /// valid authorization that nobody can then redeem. With an owner recorded, a
    /// lost CAS re-reads and can tell its own applied claim from a real loss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    consumed_by: Option<String>,
    /// The settled reply of the redemption, success or refusal, recorded by
    /// the request that claimed it. A hedged or redelivered copy of that same
    /// request waits for and returns it instead of posting the single-use code
    /// a second time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outcome: Option<FlowOutcome>,
    pending: creds::Pending,
}

/// How a claimed redemption ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
struct FlowOutcome {
    /// The reply body on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reply: Option<String>,
    /// The refusal on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// How long a settled flow record is kept for late copies of its callback.
const SETTLED_FLOW_TTL_MS: u64 = 10 * 60 * 1000;
/// How long a hedged copy waits for the claiming request to settle.
const HEDGE_WAIT_MS: u64 = 25_000;

/// The monotonic nonce source for a connector's authorizations.
///
/// SERVER-allocated. A caller-supplied counter would make `state` and the PKCE
/// verifier permanently deterministic for a connector, so starting a later
/// authorization would recreate an already exposed pair. An over-count under retry is harmless
/// here — it skips a nonce, which is exactly what freshness wants.
fn nonce_key(community: &str, connector: &str) -> String {
    format!("mcpnonce/{community}/{connector}")
}

/// The production fetcher: the injected, destination-scoped egress handle.
pub struct EgressFetch {
    egress: EgressHandle,
}

impl EgressFetch {
    /// Wrap ONE named MCP egress handle.
    pub fn for_handle(ctx: &AppContext, handle: &str) -> Result<Self> {
        let egress = ctx.egress(handle).map_err(|_| {
            BuzzError::NotFound(format!(
                "this deployment declares no {handle} handle, so it cannot reach \
                 the MCP connectors that grant covers"
            ))
        })?;
        Ok(Self { egress })
    }

    /// The handle whose grant covers this connector's host.
    ///
    /// The ONLY way a connector path builds a fetcher, so the grant reached for
    /// is always the one the catalog names for that connector — a connector
    /// cannot be dialled through another provider's grant by accident, and a
    /// deployment that declared one grant and not the other gets a refusal
    /// naming the handle it is missing.
    pub fn for_connector(ctx: &AppContext, connector: &catalog::Connector) -> Result<Self> {
        Self::for_handle(ctx, connector.egress)
    }
}

/// Which of the catalog's egress handles this deployment actually carries.
///
/// Read from the injected payload rather than by building a handle, so it is a
/// cheap fact about the deployment rather than a dial attempt.
fn bound_egress_handles(ctx: &AppContext) -> Vec<&'static str> {
    catalog::EGRESS_HANDLES
        .iter()
        .copied()
        .filter(|handle| ctx.payload().services.contains_key(*handle))
        .collect()
}

impl HttpFetch for EgressFetch {
    async fn get(
        &mut self,
        url: &str,
        headers: &[(String, String)],
    ) -> std::result::Result<Response, FetchError> {
        match self
            .egress
            .deliver_http(
                ferrite_app::EgressHttpMethod::Get,
                url,
                &crate::egress_headers(headers),
                &[],
            )
            .await
        {
            Ok(ferrite_app::EgressOutcome::Delivered { status, body, .. }) => {
                Ok(Response::new(status, body))
            }
            Ok(ferrite_app::EgressOutcome::Denied { reason }) => Err(FetchError::Denied(reason)),
            Err(e) => Err(FetchError::Transport(e.to_string())),
        }
    }

    async fn post(
        &mut self,
        url: &str,
        body: &[u8],
        headers: &[(String, String)],
    ) -> std::result::Result<Response, FetchError> {
        match self
            .egress
            .deliver_http(
                ferrite_app::EgressHttpMethod::Post,
                url,
                &crate::egress_headers(headers),
                body,
            )
            .await
        {
            Ok(ferrite_app::EgressOutcome::Delivered { status, body, .. }) => {
                Ok(Response::new(status, body))
            }
            Ok(ferrite_app::EgressOutcome::Denied { reason }) => Err(FetchError::Denied(reason)),
            Err(e) => Err(FetchError::Transport(e.to_string())),
        }
    }
}

async fn two_place(
    ctx: &AppContext,
    config: &mut StoreClusterClient,
    secret: &str,
    key: &[u8],
) -> Result<Option<String>> {
    if let Some(injected) = ctx.secret(secret) {
        return Ok(Some(injected));
    }
    match store::get_raw(config, key).await? {
        Some(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).trim().to_string())),
        None => Ok(None),
    }
}

/// The HMAC secret. Fail-closed: without it nothing unguessable can be derived,
/// and deriving from a constant would make every verifier predictable.
async fn mcp_key(ctx: &AppContext, config: &mut StoreClusterClient) -> Result<Vec<u8>> {
    match two_place(ctx, config, MCP_KEY_SECRET, MCP_KEY_CONFIG).await? {
        Some(v) if v.len() >= 32 => Ok(v.into_bytes()),
        _ => Err(BuzzError::Internal(
            "buzz-mcp-key is not configured (needs at least 32 bytes); the MCP \
             surface is unavailable"
                .into(),
        )),
    }
}

/// The secret and canonical resource a SELF-ISSUED token is verified against.
///
/// Shared with [`crate::relay::acp_surface`], whose editors authenticate with the
/// same bearer. One identity system rather than two, because two token systems for
/// two protocols would be two places to get authorization wrong — and returning the
/// pair together means the ACP surface verifies against exactly what this one mints
/// against, so the issuer and audience cannot drift apart.
pub(crate) async fn self_issued_identity(
    ctx: &AppContext,
    config: &mut StoreClusterClient,
) -> Result<(Vec<u8>, String)> {
    let secret = mcp_key(ctx, config).await?;
    let resource = resource_uri(ctx, config).await?;
    Ok((secret, resource))
}

/// This deployment's canonical resource identifier, or a refusal.
async fn resource_uri(ctx: &AppContext, config: &mut StoreClusterClient) -> Result<String> {
    let raw = two_place(ctx, config, MCP_RESOURCE_SECRET, MCP_RESOURCE_CONFIG)
        .await?
        .ok_or_else(|| {
            BuzzError::Internal(
                "buzz-mcp-resource is not configured; set it to this deployment's \
                 MCP endpoint URL (e.g. https://host/mcp)"
                    .into(),
            )
        })?;
    // Canonicalised HERE so the value used as an audience and as a credential key
    // has exactly one spelling, whatever an operator typed.
    oauth::canonical_resource(&raw).ok_or_else(|| {
        BuzzError::Internal(format!(
            "buzz-mcp-resource is not a canonical http(s) URL without userinfo: {raw}"
        ))
    })
}

async fn client_id(ctx: &AppContext, config: &mut StoreClusterClient) -> Result<String> {
    two_place(ctx, config, MCP_CLIENT_ID_SECRET, MCP_CLIENT_ID_CONFIG)
        .await?
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            BuzzError::Internal(
                "buzz-mcp-client-id is not configured; set it to this deployment's \
                 Client ID Metadata Document URL"
                    .into(),
            )
        })
}

/// Where a connector's authorization server sends the user back.
///
/// Derived from the resource identifier's ORIGIN, so it cannot point anywhere
/// this deployment does not serve.
fn redirect_uri(resource: &str) -> Result<String> {
    let rest = resource
        .strip_prefix("https://")
        .or_else(|| resource.strip_prefix("http://"));
    let (scheme, rest) = match rest {
        Some(r) if resource.starts_with("https://") => ("https", r),
        Some(r) => ("http", r),
        None => return Err(BuzzError::Internal("the MCP resource is not a URL".into())),
    };
    // The authority ends at the first `/`, `?` or `#` (RFC 3986), so a
    // canonical resource with a query and no path keeps the callback a path.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    Ok(format!("{scheme}://{authority}/buzz/api/mcp/callback"))
}

async fn resource_server(
    ctx: &AppContext,
    config: &mut StoreClusterClient,
) -> Result<ResourceServer> {
    let resource = resource_uri(ctx, config).await?;
    Ok(ResourceServer {
        // Self-issued: this app IS the authorization server for its own endpoint.
        authorization_servers: vec![resource.clone()],
        resource,
        scopes_supported: SCOPES_SUPPORTED.iter().map(|s| (*s).to_string()).collect(),
        identity: proto::Identity::new("buzz", IMPLEMENTATION_VERSION),
        instructions: Some(
            "A signed-event workspace relay. Read tools only: search messages and \
             read a channel's recent messages. Both are re-authorized per call \
             against the calling subject's own room access."
                .to_string(),
        ),
    })
}

/// The tools this server exposes. Read-only — see the module docs.
fn tool_list() -> Value {
    serde_json::json!([
        {
            "name": "buzz_search",
            "description": "Full-text search the workspace's messages. Terms are a \
                            conjunction. Only rooms the caller may read are returned.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "search terms" },
                    "channel": { "type": "string", "description": "optional channel id to restrict to" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
                },
                "required": ["query"]
            }
        },
        {
            "name": "buzz_read_channel",
            "description": "Read a channel's recent messages, newest first. Returns \
                            nothing for a room the caller may not read.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
                },
                "required": ["channel"]
            }
        }
    ])
}

/// Does a named tool exist on this server?
///
/// Asked against the ADVERTISED list, so the answer cannot drift from what a
/// client was told it could call.
fn tool_exists(name: &str) -> bool {
    tool_list().as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|t| t.get("name").and_then(Value::as_str) == Some(name))
    })
}

/// Does a named tool mutate? Nothing here does, and the answer is the SERVER's —
/// a caller cannot declare its own tool harmless.
fn tool_writes(_name: &str) -> bool {
    false
}

/// `GET /mcp` — refused with **405**, not the generic 404.
///
/// The endpoint is POST-only: 2026-07-28 removed the SSE half of the streamable
/// HTTP transport, so there is nothing a GET could open. The 404 fallthrough
/// told a client the ENDPOINT was absent, which sends it away from an address
/// that is right; 405 says the address is right and the method is wrong, and
/// the body is a JSON-RPC error object so an MCP client reads the refusal in
/// the shape it already parses.
pub(crate) fn method_not_allowed() -> Result<Reply> {
    let body = encode(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": Value::Null,
        "error": {
            "code": proto::INVALID_REQUEST,
            "message": "the MCP endpoint accepts POST only",
        },
    }))?;
    Ok(Reply::json(405, body))
}

/// `POST /mcp` — the MCP endpoint.
///
/// # The challenge is a real `WWW-Authenticate` header
///
/// It was not always. [`ferrite_app::IngressReply`] carried
/// `{correlation_id, status, body}` and nothing else, so this surface put the
/// RFC 9728 challenge in a `www_authenticate` body field and said so here. The
/// pinned SDK now carries `headers`, the gateway validates and writes them
/// (the platform gateway's `validate_reply_headers`, whose
/// `is_forbidden_reply_header` denylist does not name `www-authenticate`), and
/// [`crate::relay::ingress::Reply`] threads them out of here, so a client that
/// follows the specification's own discovery path — read the challenge, fetch
/// the resource metadata it names — finds what it looks for. That is proven in
/// `tests/severe_mcp_gate.rs` against a real fleet; it takes effect at the
/// public edge on the next deploy, not before.
///
/// The body field is KEPT beside the header for one release, so a client
/// written against the header-less shape does not break on this deploy.
///
/// The mirror-image gap on EGRESS is still open: [`crate::mcp::fetch`] cannot
/// read an upstream response header, because `EgressHandle` exposes no head.
pub(crate) async fn serve_mcp(
    ctx: &AppContext,
    req: &IngressRequest,
    authorization: Option<&str>,
) -> Result<Reply> {
    let now_ms = ctx.now_ms();
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;
    let rs = resource_server(ctx, &mut config).await?;
    let secret = mcp_key(ctx, &mut config).await?;
    let metadata_url = rs.metadata_url();

    let body: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(e) => {
            let r = proto::Rejection {
                code: proto::PARSE_ERROR,
                message: format!("request body is not JSON: {e}"),
                data: None,
            };
            return Ok(Reply::json(r.http_status(), encode(&r.to_response(None))?));
        }
    };

    // All three standard request headers this revision defines. The `/mcp` route
    // declares them as forwarded in `ferrite.json`; without that the app could
    // not see them at all, and SEP-2243 would be unenforceable however correct
    // the package was. `http` states the transport, so a request that omits
    // `MCP-Protocol-Version` is refused rather than treated as stdio.
    let headers = proto::RequestHeaders::http(
        crate::relay::ingress::request_header(req, proto::HEADER_PROTOCOL_VERSION),
        crate::relay::ingress::request_header(req, proto::HEADER_METHOD),
        crate::relay::ingress::request_header(req, proto::HEADER_NAME),
    );
    let bearer = server::bearer_of(authorization);

    let admitted =
        match rs.admit_self_issued(&headers, &body, bearer, &secret, now_ms, &tool_writes) {
            Ok(a) => a,
            Err(refusal) => {
                let status = refusal.http_status();
                let challenge = refusal.challenge(&metadata_url);
                let bytes = match refusal.body(body.get("id")) {
                    // A protocol refusal IS a JSON-RPC error.
                    Some(wire) => encode(&wire)?,
                    // An authorization refusal has no JSON-RPC form: the status
                    // and the challenge are the answer. The challenge is carried
                    // BOTH ways for one release — as the RFC 9728 header below,
                    // and as this body field for a client written against the
                    // shape that predates reply headers.
                    None => encode(&serde_json::json!({
                        "error": "unauthorized",
                        "www_authenticate": challenge.clone(),
                        "resource_metadata": metadata_url,
                    }))?,
                };
                let reply = Reply::json(status, bytes);
                // Only an AUTHORIZATION refusal has a challenge; a protocol
                // refusal answers `None` and gets no header, which is what RFC
                // 9110 §11 requires — `WWW-Authenticate` on a non-401 that is
                // not an authorization refusal is noise a client may act on.
                return Ok(match challenge {
                    Some(challenge) => reply.with_header(HEADER_WWW_AUTHENTICATE, challenge),
                    None => reply,
                });
            }
        };

    let id = admitted.accepted.id.clone();
    let method = admitted.accepted.method.as_str();
    let params = &admitted.accepted.params;

    // Every answer's status comes from the CODE it carries, so the two cannot
    // disagree and the correct answer for an unimplemented method (404, per
    // SEP-2575) is reachable.
    let (status, result) = match method {
        proto::M_DISCOVER => (
            200,
            rs.discover(
                &id,
                serde_json::json!({ "tools": { "listChanged": false } }),
                DISCOVER_TTL_MS,
            ),
        ),
        proto::M_TOOLS_LIST => {
            let mut out = serde_json::Map::new();
            out.insert("tools".into(), tool_list());
            // SEP-2549. `public` is a claim about THIS list: it is a compile-time
            // constant, identical for every caller, so caching it across
            // authorization contexts leaks nothing. A server whose tools varied
            // by subject must say private, or one caller's surface would be
            // cached and served to another.
            (200, rs.complete_cacheable(&id, out, TOOLS_TTL_MS, true))
        }
        proto::M_TOOLS_CALL => {
            // Anonymous callers cannot reach here: every tool needs a scope, so
            // admission required a token and `subject` is present.
            let Some(subject) = admitted.subject.as_deref() else {
                let challenge = tokens::Denial::Missing.challenge(&metadata_url);
                let reply = Reply::json(
                    401,
                    encode(&serde_json::json!({
                        "error": "unauthorized",
                        "www_authenticate": challenge.clone(),
                        "resource_metadata": metadata_url,
                    }))?,
                );
                return Ok(match challenge {
                    Some(challenge) => reply.with_header(HEADER_WWW_AUTHENTICATE, challenge),
                    None => reply,
                });
            };
            // An unknown tool NAME is a protocol error, not a tool that ran and
            // failed. `isError` means the tool EXECUTED, so reporting a
            // nonexistent name that way tells a model its call was attempted — it
            // retries, and retrying cannot help.
            let named = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !tool_exists(named) {
                (
                    proto::http_status_for(proto::INVALID_PARAMS),
                    rs.error(
                        &id,
                        proto::INVALID_PARAMS,
                        &format!("no tool named {named}; call tools/list for the surface"),
                    ),
                )
            } else {
                match call_tool(ctx, subject, params, now_ms).await {
                    Ok(content) => {
                        let mut out = serde_json::Map::new();
                        out.insert("content".into(), content);
                        out.insert("isError".into(), serde_json::json!(false));
                        (200, rs.complete(&id, out))
                    }
                    // A tool that RAN and failed is reported as tool CONTENT with
                    // isError, not as a protocol error: the call was well formed
                    // and the model needs to see what went wrong.
                    Err(e) => {
                        let mut out = serde_json::Map::new();
                        out.insert(
                            "content".into(),
                            serde_json::json!([{ "type": "text", "text": e.to_string() }]),
                        );
                        out.insert("isError".into(), serde_json::json!(true));
                        (200, rs.complete(&id, out))
                    }
                }
            }
        }
        other => (
            // SEP-2575: 404 together with -32601. An intermediary reads the
            // status, so 200-with-the-error-inside is not equivalent.
            proto::http_status_for(proto::METHOD_NOT_FOUND),
            rs.error(
                &id,
                proto::METHOD_NOT_FOUND,
                &format!("this server implements discover, tools/list and tools/call; not {other}"),
            ),
        ),
    };
    Ok(Reply::json(status, encode(&result)?))
}

/// Run one exposed tool as `subject`.
async fn call_tool(ctx: &AppContext, subject: &str, params: &Value, now_ms: u64) -> Result<Value> {
    let community = crate::relay::DEFAULT_COMMUNITY;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    // The subject is a workspace pubkey. Access is resolved against IT, so an
    // MCP caller sees exactly the rooms that identity may see.
    let reader = crate::relay::crypto::from_hex32(subject).ok_or_else(|| {
        BuzzError::Unauthorized("this token's subject is not a workspace identity".into())
    })?;
    crate::relay::ingress::enforce_read_standing(ctx, community, &reader, now_ms).await?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(20, |n| usize::try_from(n).unwrap_or(20))
        .clamp(1, 50);

    match name {
        "buzz_search" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let channel = args.get("channel").and_then(Value::as_str);
            let found = crate::relay::ingress::mcp_search(
                ctx, community, &reader, query, channel, limit, now_ms,
            )
            .await?;
            Ok(serde_json::json!([{ "type": "text", "text": bounded(&found.to_string()) }]))
        }
        "buzz_read_channel" => {
            let channel = args.get("channel").and_then(Value::as_str).ok_or_else(|| {
                BuzzError::InvalidEvent("buzz_read_channel needs a 'channel'".into())
            })?;
            let page = crate::relay::ingress::window_page(
                ctx, community, channel, &reader, None, limit, now_ms,
            )
            .await?;
            Ok(serde_json::json!([{ "type": "text", "text": bounded(&page.to_string()) }]))
        }
        other => Err(BuzzError::NotFound(format!("no tool named {other}"))),
    }
}

/// Truncate to [`MAX_TOOL_RESULT_BYTES`] **bytes**, on a character boundary.
///
/// By bytes rather than characters: a `chars().take(n)` bound admits up to four
/// times the cap for non-ASCII content, which is the opposite of a bound.
fn bounded(s: &str) -> String {
    if s.len() <= MAX_TOOL_RESULT_BYTES {
        return s.to_string();
    }
    let mut end = MAX_TOOL_RESULT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default().to_string()
}

/// `GET /.well-known/oauth-protected-resource[/<resource path>]` — RFC 9728
/// metadata.
///
/// Public by specification: this is how a client learns where to authenticate.
///
/// **Served at exactly one path, and that path is DERIVED.** The resource
/// identifier is operator-configured, so its well-known location is whatever
/// [`oauth::resource_metadata_url`] computes from it — the same function a client
/// uses to find this document. Anything else is a 404, including a legal-looking
/// suffix: the route's predicate is a cheap two-shape filter, and this is where
/// the exact answer is decided.
pub(crate) async fn protected_resource_metadata(ctx: &AppContext, path: &str) -> Result<Vec<u8>> {
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;
    let rs = resource_server(ctx, &mut config).await?;
    let expected = rs.metadata_url();
    // Compare PATHS: the configured resource carries an origin this request did
    // not, and the app is never told the Host it was reached on.
    let expected_path = url_path_of(&expected);
    if path != expected_path {
        return Err(BuzzError::NotFound(format!(
            "this deployment publishes its protected-resource metadata at {expected_path}"
        )));
    }
    encode(&rs.prm_document())
}

/// The path component of an absolute URL, or `/` when it has none.
///
/// A query is dropped: this is a PATH extractor, and the value is compared
/// against the query-stripped path ingress hands over.
fn url_path_of(url: &str) -> &str {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return url;
    };
    let path = match rest.find('/') {
        Some(at) => rest.get(at..).unwrap_or("/"),
        None => "/",
    };
    path.split_once(['?', '#']).map_or(path, |(path, _)| path)
}

/// `POST /buzz/api/mcp/token` — mint a self-issued access token for the caller.
///
/// The bridge between buzz's own authentication (NIP-98) and MCP's (bearer): a
/// member proves who they are with a signed event and receives a token their MCP
/// client can present. The token's subject is their pubkey, so every tool call
/// resolves access against the person who asked for it.
pub(crate) async fn mint_token(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = crate::relay::DEFAULT_COMMUNITY;
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;
    let caller = crate::relay::ingress::authenticate_request(&mut config, community, req, now_ms).await?;
    let rs = resource_server(ctx, &mut config).await?;
    let secret = mcp_key(ctx, &mut config).await?;
    let subject = crate::relay::crypto::to_hex(&caller.pubkey);
    let exp = now_ms.saturating_add(TOKEN_TTL_MS);
    // The jti is derived, not drawn: this app has no entropy, and a digest over
    // (subject, expiry) is unique per issued token without one.
    let jti = crate::relay::crypto::to_hex(&crate::mcp::sha256::sha256(
        format!("{subject}:{exp}").as_bytes(),
    ));
    let token = tokens::mint(
        &secret,
        &Claims {
            iss: rs.resource.clone(),
            aud: rs.resource.clone(),
            sub: subject,
            scope: SCOPES_SUPPORTED.iter().map(|s| (*s).to_string()).collect(),
            iat: now_ms,
            exp,
            jti,
        },
    )
    .map_err(|e| BuzzError::Internal(format!("token mint: {e}")))?;

    encode(&serde_json::json!({
        "ok": true,
        "access_token": token,
        "token_type": "Bearer",
        "expires_in": TOKEN_TTL_MS / 1000,
        "scope": SCOPES_SUPPORTED.join(" "),
        "resource": rs.resource,
        "endpoint": rs.resource,
    }))
}

/// `GET /buzz/api/mcp/connectors` — the catalog, and which are connected.
///
/// Members may read it: knowing which external systems a workspace's agents can
/// reach is not privileged information, and a client needs it to render the
/// settings surface.
pub(crate) async fn connectors(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = crate::relay::DEFAULT_COMMUNITY;
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;
    let caller = crate::relay::ingress::authenticate_request(&mut config, community, req, now_ms).await?;

    // Whether THIS caller may act on what follows, answered in the same read that
    // lists it. The alternative is a button that discovers its own 403, which is
    // the shape this relay has removed repeatedly: a control that looks available
    // and is not.
    //
    // A non-operator still receives the whole catalog, deliberately. Which
    // connectors a workspace has connected is useful for a member to know and
    // harmless to reveal — the CREDENTIAL is what confers access to the external
    // account, and that never leaves this process.
    let operator = crate::relay::ingress::is_operator(ctx, &mut config, &caller.pubkey).await?;

    let bound = bound_egress_handles(ctx);
    let mut rows = Vec::with_capacity(catalog::CONNECTORS.len());
    for c in catalog::CONNECTORS {
        // Resolved through to the CREDENTIAL, not merely the index row. A partial
        // disconnect — credential deleted, index delete failed — would otherwise
        // advertise a connection whose token is gone, and every call through it
        // would then fail as unauthorized with nothing explaining why.
        //
        // The error PROPAGATES. `credential_for` already answers `Ok(None)` for
        // that partial-disconnect case, so anything else is a storage failure or a
        // mis-keyed record — and reporting either as "not connected" would be a
        // plausible-looking lie about the workspace's state.
        let connected = stored_credential(&mut config, community, c)
            .await?
            .is_some();
        rows.push(serde_json::json!({
            "id": c.id,
            "name": c.name,
            "endpoint": c.endpoint,
            "summary": c.summary,
            // Whether the PROVIDER documents this endpoint, or only a community
            // list does. An operator enabling an unverified one should know.
            "provenance": match c.provenance {
                catalog::Provenance::Documented => "documented",
                catalog::Provenance::Community => "community",
            },
            // The grant that covers it, and whether this deployment has it.
            // DISTINCT from `connected`, and conflating them sends an operator to
            // the wrong fix: not connected means press authorize, not reachable
            // means nobody declared that grant and authorizing cannot help.
            "egress": c.egress,
            "reachable": bound.contains(&c.egress),
            "connected": connected,
        }));
    }
    let unavailable: Vec<Value> = catalog::UNAVAILABLE
        .iter()
        .map(|u| serde_json::json!({ "id": u.id, "name": u.name, "reason": u.reason }))
        .collect();

    encode(&serde_json::json!({
        "ok": true,
        // What this caller may DO, so a client can disable a control rather than
        // let somebody discover its refusal by pressing it.
        "operator": operator,
        "egress_available": !bound.is_empty(),
        "egress_handles": bound,
        "connectors": rows,
        "unavailable": unavailable,
    }))
}

/// Resolve a connector's metadata: its PRM, then its authorization server's.
///
/// Both documents are bound to what was asked for: the PRM must name
/// `endpoint` as its resource (RFC 9728 §3.3), so the flow never starts for
/// an audience this deployment does not own, and the AS metadata must name
/// the issuer the PRM pointed at (RFC 8414 §3.3), so the issuer recorded for
/// the callback's `iss` check is not one the metadata chose for itself.
async fn discover_connector(
    fetch: &mut EgressFetch,
    endpoint: &str,
) -> Result<(oauth::ResourceMetadata, oauth::AuthServerMetadata)> {
    let prm_url = oauth::resource_metadata_url(endpoint)
        .ok_or_else(|| BuzzError::Internal("connector endpoint is not a URL".into()))?;
    let prm = fetch_json(fetch, &prm_url).await?;
    let resource_meta = oauth::parse_resource_metadata(&prm, endpoint).ok_or_else(|| {
        BuzzError::NotFound(format!(
            "{prm_url} did not answer with usable Protected Resource Metadata for {endpoint}"
        ))
    })?;
    let issuer = resource_meta
        .authorization_servers
        .first()
        .cloned()
        .ok_or_else(|| {
            BuzzError::NotFound("that connector names no authorization server".into())
        })?;

    // BOTH discovery mechanisms, in order: RFC 8414 inserts the well-known
    // segment before the issuer's path and OIDC appends it, so an authorization
    // server with a tenant path is only reachable if both are tried.
    let mut last = String::new();
    for url in oauth::as_metadata_urls(&issuer) {
        match fetch_json(fetch, &url).await {
            Ok(doc) => {
                if let Some(meta) = oauth::parse_as_metadata(&doc, &issuer) {
                    return Ok((resource_meta, meta));
                }
                last = format!("{url} answered without usable metadata for issuer {issuer}");
            }
            Err(e) => last = format!("{url}: {e}"),
        }
    }
    Err(BuzzError::NotFound(format!(
        "no authorization server metadata for {issuer} ({last})"
    )))
}

async fn fetch_json(fetch: &mut EgressFetch, url: &str) -> Result<Value> {
    let response = fetch
        .get(
            url,
            &[("accept".to_string(), "application/json".to_string())],
        )
        .await;
    let response = match response {
        Ok(r) => r,
        Err(FetchError::Denied(reason)) => {
            return Err(BuzzError::Unauthorized(format!(
                "the platform refused {url}: {reason}. Its host must be a declared \
                 egress destination in ferrite.json."
            )))
        }
        Err(FetchError::Transport(why)) => return Err(BuzzError::Storage(format!("{url}: {why}"))),
    };
    if !response.is_success() {
        return Err(BuzzError::NotFound(format!(
            "{url} answered {}",
            response.status
        )));
    }
    serde_json::from_slice(&response.body)
        .map_err(|e| BuzzError::Storage(format!("{url} is not JSON: {e}")))
}

/// `POST /buzz/api/mcp/authorize` — begin authorization for a connector.
///
/// **Operator only.** Connecting grants every agent in this workspace access to
/// that external account.
pub(crate) async fn authorize(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = crate::relay::DEFAULT_COMMUNITY;
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;
    let caller = crate::relay::ingress::authenticate_request(&mut config, community, req, now_ms).await?;
    if !crate::relay::ingress::is_operator(ctx, &mut config, &caller.pubkey).await? {
        return Err(BuzzError::Unauthorized(
            "connecting an MCP server is an operator action: it grants every agent \
             in this workspace access to that external account"
                .into(),
        ));
    }
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("authorize body: {e}")))?;
    let id = body
        .get("connector")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let connector = catalog::by_id(id).ok_or_else(|| match catalog::unavailable(id) {
        // A provider that cannot work says why, rather than reading as a typo.
        Some(u) => BuzzError::NotFound(u.reason.to_string()),
        None => BuzzError::NotFound(format!("no connector named {id}")),
    })?;

    let secret = mcp_key(ctx, &mut config).await?;
    let resource = resource_uri(ctx, &mut config).await?;
    let redirect = redirect_uri(&resource)?;
    let client = client_id(ctx, &mut config).await?;

    let mut fetch = EgressFetch::for_connector(ctx, connector)?;
    let (resource_meta, as_meta) = discover_connector(&mut fetch, connector.endpoint).await?;

    // The nonce is allocated by the SERVER, atomically. A caller-supplied counter
    // made every authorization for a connector derive the same state and
    // verifier, so a later flow reused a value that had already been exposed.
    let nonce = store::bump_expiring_counter(
        &mut config,
        nonce_key(community, connector.id).as_bytes(),
        1,
        0,
    )
    .await?;
    let flow_id = format!("{}:{nonce}", connector.id);
    let verifier = oauth::derive_verifier(&secret, &flow_id, 0);
    let state = oauth::derive_state(&secret, &flow_id, 0);
    let scopes = oauth::initial_scopes(&[], &resource_meta.scopes_supported);

    // Recorded BEFORE the user leaves. `discover_connector` checked that the
    // metadata's issuer is the one the connector's PRM named, which is what
    // makes the callback's `iss` check meaningful rather than circular.
    let flow = Flow {
        connector: connector.id.to_string(),
        flow_id,
        consumed: false,
        consumed_by: None,
        outcome: None,
        pending: creds::Pending {
            issuer: as_meta.issuer.clone(),
            resource: resource_meta.resource.clone(),
            client_id: client.clone(),
            token_endpoint: as_meta.token_endpoint.clone(),
            redirect_uri: redirect.clone(),
            iss_advertised: as_meta.iss_parameter_supported,
            requested_scopes: scopes.clone(),
            attempt: 0,
            started_ms: now_ms,
        },
    };
    store::put_doc(
        &mut config,
        creds::pending_key(community, &state).as_bytes(),
        &flow,
    )
    .await?;

    let url = oauth::authorize_url(&oauth::AuthorizeRequest {
        metadata: &as_meta,
        client_id: &client,
        redirect_uri: &redirect,
        scopes: &scopes,
        resource: &resource_meta.resource,
        state: &state,
        code_challenge: &oauth::code_challenge(&verifier),
    });

    encode(&serde_json::json!({
        "ok": true,
        "connector": connector.id,
        "authorization_url": url,
        "issuer": as_meta.issuer,
        "scopes": scopes,
    }))
}

/// `GET /buzz/api/mcp/callback` — finish authorization.
///
/// Deliberately UNauthenticated in the NIP-98 sense: this is a browser redirect
/// from the provider and carries no signed header. The single-use `state` is what
/// proves the flow — it is HMAC-derived under an injected secret, so it cannot be
/// guessed, and the record it names is deleted whether the exchange succeeds or
/// fails.
pub(crate) async fn callback(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = crate::relay::DEFAULT_COMMUNITY;
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;

    let state = crate::relay::ingress::query_param(&req.path, "state")
        .ok_or_else(|| BuzzError::InvalidEvent("the callback carries no state".into()))?;
    let pending_key = creds::pending_key(community, &state);
    let (mut flow, version) = store::get_doc_versioned::<Flow>(&mut config, pending_key.as_bytes())
        .await?
        .ok_or_else(|| {
            BuzzError::NotFound("that authorization is unknown or has already been used".into())
        })?;
    // The gateway hedges a slow request by re-publishing the SAME envelope, so a
    // correlation id names a request, not one handler invocation. The claim is
    // therefore the correlation id plus a random part this invocation alone
    // knows: a hedged copy sees a claim from its own request that is not its own
    // and waits for that invocation's settled outcome rather than posting the
    // single-use code again (finding_94d39bbb20f16773).
    let mine = format!(
        "{}#{}",
        req.correlation_id,
        crate::crypto_kernel::random_token(16)
            .map_err(|e| BuzzError::Internal(format!("claim token: {e}")))?
    );
    let same_request = |claim: Option<&str>| {
        claim.is_some_and(|c| {
            c.split_once('#').map_or(c, |(id, _)| id) == req.correlation_id.as_str()
        })
    };
    if flow.consumed {
        if !same_request(flow.consumed_by.as_deref()) {
            return Err(BuzzError::NotFound(
                "that authorization has already been used".into(),
            ));
        }
        return await_settled_flow(ctx, &mut config, &pending_key, flow).await;
    }

    // CLAIM it atomically, recording WHICH invocation claimed it. A lost CAS
    // response (the store's ambiguous-retry contract) re-reads: this
    // invocation's own claim proceeds, another copy of the same request waits
    // for that copy's outcome, and anything else is a genuine loss.
    flow.consumed = true;
    flow.consumed_by = Some(mine.clone());
    if store::put_doc_if_version(&mut config, pending_key.as_bytes(), &flow, Some(version))
        .await?
        .is_err()
    {
        let reread = store::get_doc::<Flow>(&mut config, pending_key.as_bytes()).await?;
        match reread {
            Some(current) if current.consumed_by.as_deref() == Some(mine.as_str()) => {
                flow = current;
            }
            Some(current) if same_request(current.consumed_by.as_deref()) => {
                return await_settled_flow(ctx, &mut config, &pending_key, current).await;
            }
            _ => {
                return Err(BuzzError::Conflict(
                    "that authorization is being completed by another request".into(),
                ))
            }
        }
    }

    let result = redeem(ctx, req, &mut config, &flow, now_ms, community).await;
    // Record the settled outcome for late copies, and let the record expire:
    // the flow is spent either way and must not linger in the store.
    flow.outcome = Some(match &result {
        Ok(body) => FlowOutcome {
            reply: Some(String::from_utf8_lossy(body).into_owned()),
            error: None,
        },
        Err(e) => FlowOutcome {
            reply: None,
            error: Some(e.to_string()),
        },
    });
    let _ = store::put_expiring_doc(
        &mut config,
        pending_key.as_bytes(),
        &flow,
        now_ms.saturating_add(SETTLED_FLOW_TTL_MS),
    )
    .await;
    result
}

/// Wait for the invocation that claimed `flow` to settle it, and answer with
/// its outcome. Bounded: a claimant that died leaves a Conflict, never a second
/// redemption of the code.
async fn await_settled_flow(
    ctx: &AppContext,
    config: &mut StoreClusterClient,
    pending_key: &str,
    mut flow: Flow,
) -> Result<Vec<u8>> {
    let deadline = ctx.now_ms().saturating_add(HEDGE_WAIT_MS);
    loop {
        if let Some(outcome) = flow.outcome.take() {
            return match (outcome.reply, outcome.error) {
                (Some(reply), _) => Ok(reply.into_bytes()),
                (None, Some(error)) => Err(BuzzError::Unauthorized(error)),
                (None, None) => Err(BuzzError::Conflict("that authorization did not settle".into())),
            };
        }
        if ctx.now_ms() >= deadline {
            return Err(BuzzError::Conflict(
                "that authorization is being completed by another request".into(),
            ));
        }
        ferrite_app::rt::time::sleep(std::time::Duration::from_millis(200)).await;
        match store::get_doc::<Flow>(config, pending_key.as_bytes()).await? {
            Some(current) => flow = current,
            None => {
                return Err(BuzzError::NotFound(
                    "that authorization is unknown or has already been used".into(),
                ))
            }
        }
    }
}

/// Redeem a claimed flow's authorization code and store the credential.
async fn redeem(
    ctx: &AppContext,
    req: &IngressRequest,
    config: &mut StoreClusterClient,
    flow: &Flow,
    now_ms: u64,
    community: &str,
) -> Result<Vec<u8>> {
    let pending = flow.pending.clone();

    if pending.is_stale(now_ms) {
        return Err(BuzzError::NotFound(
            "that authorization was started too long ago; start again".into(),
        ));
    }

    // RFC 9207, byte-exact against the issuer from VALIDATED metadata. On a
    // mismatch nothing from the response is acted on OR displayed — an
    // `error_description` from an unverified issuer is attacker-supplied text.
    let iss = crate::relay::ingress::query_param(&req.path, "iss");
    if let oauth::IssuerCheck::Reject(why) =
        oauth::validate_issuer(&pending.issuer, iss.as_deref(), pending.iss_advertised)
    {
        return Err(BuzzError::Unauthorized(why.to_string()));
    }

    let code = crate::relay::ingress::query_param(&req.path, "code").ok_or_else(|| {
        // The provider may have sent an error instead. Its description is only
        // safe to surface because the issuer check above passed.
        let described = crate::relay::ingress::query_param(&req.path, "error")
            .unwrap_or_else(|| "no code".to_string());
        BuzzError::Unauthorized(format!("authorization did not complete: {described}"))
    })?;

    let secret = mcp_key(ctx, config).await?;
    let verifier = oauth::derive_verifier(&secret, &flow.flow_id, 0);
    let form = oauth::token_request_form(
        &code,
        &pending.redirect_uri,
        &pending.client_id,
        &verifier,
        &pending.resource,
    );

    // The token endpoint is reached through the grant that covers THIS flow's
    // connector, recovered from the flow rather than assumed — a callback must not
    // be able to redeem through another provider's grant.
    let connector = catalog::by_id(&flow.connector).ok_or_else(|| {
        BuzzError::NotFound(format!(
            "connector {} is no longer in the catalog, so its authorization cannot \
             be completed",
            flow.connector
        ))
    })?;
    let mut fetch = EgressFetch::for_connector(ctx, connector)?;
    let response = fetch
        .post(
            &pending.token_endpoint,
            form.as_bytes(),
            &[
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("accept".to_string(), "application/json".to_string()),
            ],
        )
        .await
        .map_err(|e| match e {
            FetchError::Denied(reason) => BuzzError::Unauthorized(format!(
                "the platform refused the token endpoint: {reason}"
            )),
            FetchError::Transport(why) => BuzzError::Storage(format!("token request: {why}")),
        })?;
    if !response.is_success() {
        return Err(BuzzError::Unauthorized(format!(
            "the authorization server answered {} to the token request",
            response.status
        )));
    }
    let doc: Value = serde_json::from_slice(&response.body)
        .map_err(|e| BuzzError::Storage(format!("token response is not JSON: {e}")))?;
    let grant = oauth::parse_token_response(&doc)
        .map_err(|why| BuzzError::Unauthorized(why.to_string()))?;

    let credential = creds::Credential {
        issuer: pending.issuer.clone(),
        resource: pending.resource.clone(),
        client_id: pending.client_id.clone(),
        access_token: grant.access_token,
        refresh_token: grant.refresh_token,
        scopes: if grant.scope.is_empty() {
            pending.requested_scopes.clone()
        } else {
            grant.scope
        },
        expires_at_ms: grant
            .expires_in
            .map(|s| now_ms.saturating_add(s.saturating_mul(1000))),
        // Recorded so a later refresh has somewhere to go — this is the
        // endpoint from VALIDATED metadata, the one the code was redeemed at.
        token_endpoint: Some(pending.token_endpoint.clone()),
    };
    let bytes = encode(&credential)?;
    store::put_raw(
        config,
        credential.store_key(community).as_bytes(),
        &bytes,
    )
    .await?;
    // The index that makes it findable from a catalog id. Written AFTER the
    // credential, so an index row never names a record that does not exist.
    store::put_doc(
        config,
        link_key(community, &flow.connector).as_bytes(),
        &ConnectorLink {
            issuer: credential.issuer.clone(),
            resource: credential.resource.clone(),
        },
    )
    .await?;

    encode(&serde_json::json!({
        "ok": true,
        "connected": true,
        "connector": flow.connector,
        "resource": credential.resource,
        "scopes": credential.scopes,
    }))
}

/// The validated (issuer, resource) pair recorded for a connector, if connected.
async fn link_of(
    config: &mut StoreClusterClient,
    community: &str,
    connector: &str,
) -> Result<Option<ConnectorLink>> {
    store::get_doc::<ConnectorLink>(config, link_key(community, connector).as_bytes()).await
}

/// `POST /buzz/api/mcp/disconnect` — drop a connector's credential.
pub(crate) async fn disconnect(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = crate::relay::DEFAULT_COMMUNITY;
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;
    let caller = crate::relay::ingress::authenticate_request(&mut config, community, req, now_ms).await?;
    if !crate::relay::ingress::is_operator(ctx, &mut config, &caller.pubkey).await? {
        return Err(BuzzError::Unauthorized(
            "disconnecting an MCP server is an operator action".into(),
        ));
    }
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("disconnect body: {e}")))?;
    let id = body
        .get("connector")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let connector = catalog::by_id(id)
        .ok_or_else(|| BuzzError::NotFound(format!("no connector named {id}")))?;
    let link = link_of(&mut config, community, connector.id).await?;
    let existed = link.is_some();
    if let Some(link) = link {
        // The credential first, then its index: an index row pointing at a
        // deleted credential reads as connected-but-broken, whereas a credential
        // with no index is simply unreachable and swept by the next connect.
        store::delete_key(
            &mut config,
            creds::credential_key(community, &link.issuer, &link.resource).as_bytes(),
        )
        .await?;
        store::delete_key(&mut config, link_key(community, connector.id).as_bytes()).await?;
    }
    encode(&serde_json::json!({ "ok": true, "connector": connector.id, "was_connected": existed }))
}

/// The stored credential for a connector, if this workspace has one.
///
/// Resolved through the index row, because a credential's key is
/// (issuer, resource) and the issuer is only knowable from the connector's own
/// metadata. The pair is then re-checked against the record itself, so a
/// mis-keyed or migrated row can never be presented to the wrong server.
async fn credential_for(
    ctx: &AppContext,
    config: &mut StoreClusterClient,
    community: &str,
    connector: &catalog::Connector,
) -> Result<Option<creds::Credential>> {
    let Some((credential, version)) = stored_credential(config, community, connector).await? else {
        return Ok(None);
    };
    // Inside the skew window: rotate BEFORE the token expires mid-request,
    // rather than surfacing a 401 as "needs an operator" while a usable
    // refresh token sits in the record.
    if credential.needs_refresh(ctx.now_ms()) {
        if let Some(rotated) =
            refresh_credential(ctx, config, community, connector, &credential, version).await
        {
            return Ok(Some(rotated));
        }
    }
    Ok(Some(credential))
}

/// The stored credential for a connector, WITHOUT rotating it, and its version.
///
/// This is the read-only core [`credential_for`] adds a refresh on top of. The
/// listing paths use it directly: reporting `connected` must not fire a token
/// endpoint POST per connector — that is both an egress-budget surprise and a
/// way for one page render to present the same `refresh_token` on many
/// concurrent refreshes. Presence of a valid record (index and record agree,
/// and it names this connector's endpoint) is the connected fact; an expiring
/// token is still connected, because a refresh will carry it.
async fn stored_credential(
    config: &mut StoreClusterClient,
    community: &str,
    connector: &catalog::Connector,
) -> Result<Option<(creds::Credential, u64)>> {
    let Some(link) = link_of(config, community, connector.id).await? else {
        return Ok(None);
    };
    let key = creds::credential_key(community, &link.issuer, &link.resource);
    // Versioned, because a refresh rotates this record with a CAS against
    // exactly this read.
    let Some((bytes, version)) = store::get_raw_versioned(config, key.as_bytes()).await? else {
        return Ok(None);
    };
    let credential: creds::Credential = serde_json::from_slice(&bytes)
        .map_err(|e| BuzzError::Storage(format!("credential record: {e}")))?;
    if !credential.is_for(&link.issuer, &link.resource) {
        return Err(BuzzError::Storage(
            "the stored credential does not name the issuer and resource its index claims".into(),
        ));
    }
    // And the resource must be the connector's own endpoint: an index row that
    // pointed elsewhere would hand this connector somebody else's token.
    if credential.resource != connector.endpoint {
        return Err(BuzzError::Storage(format!(
            "the credential indexed for {} names {} instead",
            connector.id, credential.resource
        )));
    }
    Ok(Some((credential, version)))
}

/// Rotate an expiring credential through its refresh token, best-effort.
///
/// `None` means "nothing better than what you were holding": no refresh token,
/// no recorded token endpoint (an older record has none), a refusal
/// from the authorization server, or a persistence failure. In every one of
/// those the caller keeps serving the last known-good token — inside the skew
/// window it is still valid, and turning a refresh hiccup into a hard failure
/// would break a call the held token could have carried.
async fn refresh_credential(
    ctx: &AppContext,
    config: &mut StoreClusterClient,
    community: &str,
    connector: &catalog::Connector,
    current: &creds::Credential,
    version: u64,
) -> Option<creds::Credential> {
    let refresh_token = current.refresh_token.as_deref()?;
    let token_endpoint = current.token_endpoint.as_deref()?;
    let now_ms = ctx.now_ms();

    // Serialize the refresh at the AUTHORIZATION SERVER, not merely at the
    // store write below. A store CAS alone still lets two turns present the
    // same `refresh_token` to the token endpoint; under rotation-with-reuse-
    // detection the AS then revokes the whole grant family and the connector
    // silently disconnects. So CAS-claim a short-lived lease on this record's
    // (issuer, resource) BEFORE the POST. If another turn holds it, do NOT
    // POST — re-read and serve whatever it rotated in (or, inside the skew
    // window, the still-valid current token). A store error claiming the lease
    // is treated the same: never POST unserialized.
    let lease_key = creds::refresh_lease_key(community, &current.issuer, &current.resource);
    let lease_deadline = now_ms.saturating_add(creds::REFRESH_LEASE_TTL_MS);
    let record_key = current.store_key(community);
    match store::put_expiring_doc_if_version(
        config,
        lease_key.as_bytes(),
        &now_ms,
        None,
        lease_deadline,
    )
    .await
    {
        // Lease claimed: we are the single refresher, proceed to the POST.
        Ok(Ok(_)) => {}
        // Held (or unreadable): wait-and-reread rather than POST a duplicate.
        Ok(Err(_)) | Err(_) => {
            return match store::get_doc::<creds::Credential>(config, record_key.as_bytes()).await {
                Ok(Some(stored)) if stored.is_for(&current.issuer, &current.resource) => {
                    Some(stored)
                }
                _ => None,
            };
        }
    }

    // The refresh travels through the grant that covers THIS connector — the
    // same rule redemption follows in the callback.
    let mut fetch = EgressFetch::for_connector(ctx, connector).ok()?;
    let form = oauth::refresh_request_form(refresh_token, &current.client_id, &current.resource);
    let response = fetch
        .post(
            token_endpoint,
            form.as_bytes(),
            &[
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("accept".to_string(), "application/json".to_string()),
            ],
        )
        .await
        .ok()?;
    if !response.is_success() {
        return None;
    }
    let doc: Value = serde_json::from_slice(&response.body).ok()?;
    let grant = oauth::parse_token_response(&doc).ok()?;

    let rotated = creds::Credential {
        issuer: current.issuer.clone(),
        resource: current.resource.clone(),
        client_id: current.client_id.clone(),
        access_token: grant.access_token,
        // An AS may rotate the refresh token or stay silent about it; silence
        // keeps the one that just worked.
        refresh_token: grant
            .refresh_token
            .or_else(|| current.refresh_token.clone()),
        scopes: if grant.scope.is_empty() {
            current.scopes.clone()
        } else {
            grant.scope
        },
        expires_at_ms: grant
            .expires_in
            .map(|s| now_ms.saturating_add(s.saturating_mul(1000))),
        token_endpoint: Some(token_endpoint.to_string()),
    };
    // A CAS against the version this refresh read, so two concurrent turns
    // cannot both rotate: exactly one write lands on that version.
    let outcome =
        match store::put_doc_if_version(config, record_key.as_bytes(), &rotated, Some(version))
            .await
        {
            Ok(Ok(_version)) => Some(rotated),
            // The row moved: a concurrent refresh won, a disconnect removed it, or
            // this write's own response was lost (the store's ambiguous-retry
            // contract). Re-reading answers all three — serve what is stored now,
            // because the token this call minted may be the rotated-out one.
            Ok(Err(_current)) => {
                match store::get_doc::<creds::Credential>(config, record_key.as_bytes()).await {
                    Ok(Some(stored)) if stored.is_for(&current.issuer, &current.resource) => {
                        Some(stored)
                    }
                    _ => None,
                }
            }
            // Persistence failure: the rotated token was never durably recorded, so
            // serving it would strand the NEXT reader with a token the AS may have
            // invalidated while the record still names the old pair. Last known-good.
            Err(_) => None,
        };
    // Release the lease so the next legitimate refresh need not wait out the
    // whole TTL; if the delete is lost the lease self-expires regardless.
    let _ = store::delete_key(config, lease_key.as_bytes()).await;
    outcome
}

/// `POST /buzz/api/mcp/tools` — list a connector's tools.
///
/// Member-gated (the write gate): a listing changes nothing on the external
/// system. Calling a tool does, so [`connector_call`] asks for more.
pub(crate) async fn connector_tools(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let (connector, credential) = member_and_connector(ctx, req, false).await?;
    let result = connector_list_tools(ctx, connector, credential.as_ref()).await?;
    encode(&serde_json::json!({ "ok": true, "connector": connector.id, "result": result }))
}

/// `POST /buzz/api/mcp/call` — call a tool on a connector.
///
/// Operator only, the same authority that connected it. The call runs with the
/// WORKSPACE's credential for the external account, and `authorize`/`disconnect`
/// are operator-only precisely because that credential is authority over the
/// external system; letting any un-banned member invoke arbitrary (including
/// mutating) tools with it was the same authority behind a lower gate, and on an
/// open community "member" means any account holder. Agents keep their own path
/// ([`agent_connector`]), which the mention dispatch already gates.
pub(crate) async fn connector_call(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let (connector, credential) = member_and_connector(ctx, req, true).await?;
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("call body: {e}")))?;
    let tool = body
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("this needs a 'tool' name".into()))?;
    let arguments = body.get("arguments").cloned().unwrap_or_else(|| serde_json::json!({}));
    let result = connector_call_tool(ctx, connector, credential.as_ref(), tool, arguments).await?;
    encode(&serde_json::json!({ "ok": true, "connector": connector.id, "result": result }))
}

/// One MCP client for a connector, carrying its credential when there is one.
fn client_for<'a>(
    fetch: &'a mut EgressFetch,
    connector: &catalog::Connector,
    credential: Option<&creds::Credential>,
) -> McpClient<'a, EgressFetch> {
    let client = McpClient::new(
        fetch,
        connector.endpoint,
        proto::Identity::new("buzz", IMPLEMENTATION_VERSION),
    );
    match credential {
        Some(c) => client.with_bearer(c.access_token.clone()),
        // No credential: a connector that needs none still answers, and one that
        // does answers 401, which the caller is told to fix by connecting.
        None => client,
    }
}

/// List a connector's tools. The core both the HTTP route and an agent use.
pub(crate) async fn connector_list_tools(
    ctx: &AppContext,
    connector: &catalog::Connector,
    credential: Option<&creds::Credential>,
) -> Result<Value> {
    let mut fetch = EgressFetch::for_connector(ctx, connector)?;
    let mut client = client_for(&mut fetch, connector, credential);
    // Follow `nextCursor` to exhaustion rather than answering with page one as
    // if it were the surface. Bounded twice — tools kept and pages walked — and
    // a cursor equal to the one just used is a loop, not more pages.
    let mut tools: Vec<Value> = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;
    let mut cursor: Option<String> = None;
    let mut pages = 0usize;
    loop {
        let page = client
            .list_tools(cursor.as_deref())
            .await
            .map_err(client_error)?;
        if let Some(listed) = page.get("tools").and_then(Value::as_array) {
            total += listed.len();
            for tool in listed {
                if tools.len() < MAX_CONNECTOR_TOOLS {
                    tools.push(tool.clone());
                } else {
                    truncated = true;
                }
            }
        }
        pages += 1;
        match page.get("nextCursor").and_then(Value::as_str) {
            None => break,
            Some(next) if cursor.as_deref() == Some(next) => {
                truncated = true;
                break;
            }
            Some(_) if pages >= MAX_TOOL_PAGES || tools.len() >= MAX_CONNECTOR_TOOLS => {
                truncated = true;
                break;
            }
            Some(next) => cursor = Some(next.to_string()),
        }
    }
    // The truncation is REPORTED, not silent: a caller planning against this
    // list must be able to tell "the server offers 40 tools" from "these are
    // the first 200 of more".
    Ok(serde_json::json!({
        "tools": tools,
        "tools_total": total,
        "tools_truncated": truncated,
    }))
}

/// Call a tool on a connector. The core both faces use.
pub(crate) async fn connector_call_tool(
    ctx: &AppContext,
    connector: &catalog::Connector,
    credential: Option<&creds::Credential>,
    tool: &str,
    arguments: Value,
) -> Result<Value> {
    let mut fetch = EgressFetch::for_connector(ctx, connector)?;
    let mut client = client_for(&mut fetch, connector, credential);
    // Validate the model-supplied arguments against the tool's advertised
    // `inputSchema` before dispatch, the same discipline an agent runtime applies
    // to its own MCP calls. A missing required field is a refusal the caller
    // can correct, where the server's would be an opaque -32602 a round trip
    // later. The schema comes from the server's own listing; a tool that is not
    // listed, or one that declares no object schema, is dispatched as-is — the
    // server stays the authority on its own surface.
    if let Some(schema) = find_tool_schema(&mut client, tool).await? {
        if let Err(reason) = validate_connector_arguments(&schema, &arguments) {
            return Err(BuzzError::InvalidEvent(reason));
        }
    }
    // NoInput: a server asking a relay for elicitation has nobody to ask, and
    // fabricating an answer would be worse than declining.
    client
        .call_tool(tool, arguments, &mut NoInput)
        .await
        .map_err(client_error)
}

/// Read the advertised `inputSchema` for `tool` from the connector's listing,
/// walking pages until it is found. `None` = the tool is not listed or declares
/// no object schema (nothing to validate against). Bounded by the same page cap
/// as [`connector_list_tools`], and a repeated cursor ends the walk.
async fn find_tool_schema(
    client: &mut McpClient<'_, EgressFetch>,
    tool: &str,
) -> Result<Option<Value>> {
    let mut cursor: Option<String> = None;
    let mut pages = 0usize;
    loop {
        let page = client
            .list_tools(cursor.as_deref())
            .await
            .map_err(client_error)?;
        if let Some(listed) = page.get("tools").and_then(Value::as_array) {
            if let Some(found) = listed
                .iter()
                .find(|t| t.get("name").and_then(Value::as_str) == Some(tool))
            {
                let schema = found.get("inputSchema").cloned().unwrap_or(Value::Null);
                return Ok(schema.is_object().then_some(schema));
            }
        }
        pages += 1;
        match page.get("nextCursor").and_then(Value::as_str) {
            None => return Ok(None),
            Some(next) if cursor.as_deref() == Some(next) => return Ok(None),
            Some(_) if pages >= MAX_TOOL_PAGES => return Ok(None),
            Some(next) => cursor = Some(next.to_string()),
        }
    }
}

/// A structural check of `arguments` against a tool's JSON-Schema `inputSchema`:
/// the arguments must be an object and every name the schema lists in `required`
/// must be present. Deliberately narrower than the builtins' `prepare_arguments`
/// (no salvage, no primitive coercion) — the cheap, always-correct subset that
/// turns the common misuse into a message the caller can act on.
fn validate_connector_arguments(
    schema: &Value,
    arguments: &Value,
) -> core::result::Result<(), String> {
    let empty = serde_json::Map::new();
    let obj = match arguments {
        Value::Object(map) => map,
        // An MCP call with no arguments is an empty object; a non-object is a
        // shape the server cannot accept.
        Value::Null => &empty,
        _ => return Err("tool arguments must be a JSON object".into()),
    };
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        let missing: Vec<&str> = required
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| !obj.contains_key(*name))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "missing required argument(s): {}",
                missing.join(", ")
            ));
        }
    }
    Ok(())
}

/// Refuse a persona's connector call unless the member it acts for is a
/// workspace operator.
///
/// `POST /buzz/api/mcp/call` is operator-only because the stored credential is
/// workspace-level authority over an external account. A persona reaches the
/// same credential, so it carries the same rule, judged against the member who
/// mentioned it (or drove it over ACP, or created its schedule). A turn with no
/// such member is refused rather than run on the persona's own say-so.
pub async fn require_operator_requester(
    ctx: &AppContext,
    requester: Option<&str>,
) -> Result<()> {
    let refused = || {
        BuzzError::Unauthorized(
            "only a workspace operator may have a persona call a connector's tools with the workspace credential"
                .into(),
        )
    };
    let who = requester
        .and_then(crate::relay::crypto::from_hex32)
        .ok_or_else(refused)?;
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;
    if crate::relay::ingress::is_operator(ctx, &mut config, &who).await? {
        Ok(())
    } else {
        Err(refused())
    }
}

/// Resolve a connector and its credential for an AGENT.
///
/// No NIP-98 and no member write gate, and neither is an omission. A persona
/// reached this point through the mention→dispatch chain, which already decided
/// who may invoke it, rate-limited that invocation and audited it — and the
/// credential is the WORKSPACE's, not the persona's, so running a member's gate
/// against a persona would be checking the wrong identity.
///
/// What still holds is everything that decides authority over the external
/// system: the connector must be in the catalog, its host must be a declared
/// egress destination, and the credential is resolved through the index and
/// re-verified against this connector's own endpoint.
pub(crate) async fn agent_connector(
    ctx: &AppContext,
    id: &str,
) -> Result<(&'static catalog::Connector, Option<creds::Credential>)> {
    let community = crate::relay::DEFAULT_COMMUNITY;
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;
    let connector = catalog::by_id(id).ok_or_else(|| match catalog::unavailable(id) {
        Some(u) => BuzzError::NotFound(u.reason.to_string()),
        None => BuzzError::NotFound(format!("no connector named {id}")),
    })?;
    let credential = credential_for(ctx, &mut config, community, connector).await?;
    Ok((connector, credential))
}

/// The connectors an agent may name, with whether each is connected.
///
/// A tool description cannot enumerate them (the catalog is data, the schema is
/// static), so this is what an agent calls to find out.
pub(crate) async fn agent_connector_list(ctx: &AppContext) -> Result<Value> {
    let community = crate::relay::DEFAULT_COMMUNITY;
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;
    let bound = bound_egress_handles(ctx);
    let mut rows = Vec::new();
    for c in catalog::CONNECTORS {
        let connected = stored_credential(&mut config, community, c)
            .await?
            .is_some();
        rows.push(serde_json::json!({
            "id": c.id,
            "name": c.name,
            "summary": c.summary,
            // Both facts, because they call for different sentences in the
            // channel: an unreachable connector needs a spec edit and a redeploy,
            // an unconnected one needs an operator to press authorize.
            "reachable": bound.contains(&c.egress),
            "connected": connected,
        }));
    }
    Ok(serde_json::json!({ "connectors": rows }))
}

/// Authenticate a member, resolve the named connector, and load its credential.
///
/// `operator_only` additionally requires the caller to be a workspace operator,
/// checked before the credential is loaded.
async fn member_and_connector(
    ctx: &AppContext,
    req: &IngressRequest,
    operator_only: bool,
) -> Result<(&'static catalog::Connector, Option<creds::Credential>)> {
    let now_ms = ctx.now_ms();
    let community = crate::relay::DEFAULT_COMMUNITY;
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;
    let caller = crate::relay::ingress::authenticate_request(&mut config, community, req, now_ms).await?;
    // Reaching an external system on the workspace's behalf spends this
    // deployment's egress budget, so it is gated like a write.
    crate::relay::ingress::write_gate(ctx, community, &caller.pubkey, now_ms).await?;
    if operator_only
        && !crate::relay::ingress::is_operator(ctx, &mut config, &caller.pubkey).await?
    {
        return Err(BuzzError::Unauthorized(
            "only a workspace operator may call a connector's tools with the workspace credential"
                .into(),
        ));
    }

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("body: {e}")))?;
    let id = body
        .get("connector")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let connector = catalog::by_id(id).ok_or_else(|| match catalog::unavailable(id) {
        Some(u) => BuzzError::NotFound(u.reason.to_string()),
        None => BuzzError::NotFound(format!("no connector named {id}")),
    })?;
    let credential = credential_for(ctx, &mut config, community, connector).await?;
    Ok((connector, credential))
}

/// Map a client error onto an answer a caller can act on.
fn client_error(e: crate::mcp::client::ClientError) -> BuzzError {
    use crate::mcp::client::ClientError as E;
    match e {
        // The host is not in the app spec. No token will help.
        E::Denied(reason) => BuzzError::Unauthorized(format!(
            "the platform refused that destination: {reason}. Its host must be a \
             declared egress destination in ferrite.json."
        )),
        E::Unauthorized => {
            BuzzError::Unauthorized("that connector is not authenticated; connect it first".into())
        }
        E::Forbidden => BuzzError::Unauthorized(
            "that connector refused the request as under-authorized; reconnect it to \
             request wider scopes"
                .into(),
        ),
        E::Rpc { code, message, .. } => {
            BuzzError::InvalidEvent(format!("the connector answered {code}: {message}"))
        }
        E::InputUnavailable => BuzzError::InvalidEvent(
            "that tool asked for interactive input, which a relay cannot supply".into(),
        ),
        E::InputLoop => {
            BuzzError::Storage("that connector kept asking for the same input; giving up".into())
        }
        E::Http(status) => BuzzError::Storage(format!("the connector answered HTTP {status}")),
        E::Malformed(why) | E::Transport(why) => BuzzError::Storage(why),
    }
}

fn encode<T: ferrite_app::serde::Serialize>(v: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(v).map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_arguments_are_checked_against_the_advertised_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" }, "limit": { "type": "number" } },
            "required": ["path"],
        });
        // A required field present: accepted.
        assert!(validate_connector_arguments(&schema, &serde_json::json!({ "path": "/a" })).is_ok());
        // A required field missing: a message the caller can act on, naming it.
        let bounce = validate_connector_arguments(&schema, &serde_json::json!({ "limit": 3 }))
            .expect_err("missing required 'path'");
        assert!(bounce.contains("path"));
        // A non-object argument set is refused outright.
        assert!(validate_connector_arguments(&schema, &serde_json::json!("nope")).is_err());
        // No required fields: an empty (or null) argument set passes through.
        let open = serde_json::json!({ "type": "object", "properties": {} });
        assert!(validate_connector_arguments(&open, &Value::Null).is_ok());
        assert!(validate_connector_arguments(&open, &serde_json::json!({})).is_ok());
        // Null against a schema that DOES require a field is still a bounce.
        assert!(validate_connector_arguments(&schema, &Value::Null).is_err());
    }

    #[test]
    fn a_get_on_the_mcp_endpoint_is_405_with_a_json_rpc_body() {
        // 404 says "wrong address"; the address is right and the METHOD is
        // wrong, and the refusal must be readable by an MCP client.
        let reply = method_not_allowed().expect("encodes");
        assert_eq!(reply.status, 405);
        // And it says what the body is, which the header-less tier could not.
        assert_eq!(
            reply.headers,
            vec![("content-type".to_string(), "application/json".to_string())]
        );
        let v: Value = serde_json::from_slice(&reply.body).expect("json");
        assert_eq!(v.get("jsonrpc"), Some(&serde_json::json!("2.0")));
        let error = v.get("error").and_then(Value::as_object).expect("error");
        assert_eq!(error.get("code"), Some(&serde_json::json!(proto::INVALID_REQUEST)));
        assert!(error
            .get("message")
            .and_then(Value::as_str)
            .expect("a message")
            .contains("POST"));
    }

    #[test]
    fn the_redirect_is_derived_from_the_resources_own_origin() {
        // Never from a request header: a caller must not be able to choose where
        // an authorization code is sent.
        assert_eq!(
            redirect_uri("https://buzz.example.com/mcp").expect("derives"),
            "https://buzz.example.com/buzz/api/mcp/callback"
        );
        assert_eq!(
            redirect_uri("https://h:8443/mcp").expect("derives"),
            "https://h:8443/buzz/api/mcp/callback"
        );
        assert!(redirect_uri("not-a-url").is_err());
        // A query with no path still ends the authority.
        assert_eq!(
            redirect_uri("https://h.example?tenant=x").expect("derives"),
            "https://h.example/buzz/api/mcp/callback"
        );
    }

    #[test]
    fn no_exposed_tool_writes_so_no_write_scope_is_advertised() {
        // The two must agree, or a client requests authority this server cannot
        // honour — or worse, a mutating tool ships under a read scope.
        let tools = tool_list();
        for tool in tools.as_array().expect("an array") {
            let name = tool["name"].as_str().expect("a name");
            assert!(
                !tool_writes(name),
                "{name} writes but is advertised as a read"
            );
        }
        assert!(!SCOPES_SUPPORTED.contains(&crate::mcp::tokens::SCOPE_WRITE));
    }

    #[test]
    fn every_exposed_tool_has_a_schema_a_model_can_use() {
        for tool in tool_list().as_array().expect("an array") {
            assert!(tool["name"].is_string());
            assert!(tool["description"].is_string());
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], serde_json::json!("object"));
            assert!(schema["properties"].is_object());
            assert!(
                schema["required"].is_array(),
                "a tool with no required field is ambiguous"
            );
        }
    }

    #[test]
    fn the_metadata_path_is_derived_and_nothing_else_is_served() {
        // A client derives this path from the resource identifier, and so does the
        // handler — which is what makes the two agree for ANY configured resource
        // rather than only for `/mcp`.
        assert_eq!(
            url_path_of("https://buzz.example/.well-known/oauth-protected-resource/mcp"),
            "/.well-known/oauth-protected-resource/mcp"
        );
        assert_eq!(
            url_path_of("https://buzz.example/.well-known/oauth-protected-resource"),
            "/.well-known/oauth-protected-resource"
        );
        // A deployment whose resource has a longer path publishes elsewhere.
        let nested = oauth::resource_metadata_url("https://h.example/api/mcp").expect("derives");
        assert_eq!(
            url_path_of(&nested),
            "/.well-known/oauth-protected-resource/api/mcp"
        );
        // No path at all still yields a root.
        assert_eq!(url_path_of("https://h.example"), "/");
        // A queried resource is servable: the derivation drops the query, so the
        // expected path is one ingress can actually deliver.
        let queried =
            oauth::resource_metadata_url("https://h.example/mcp?tenant=x").expect("derives");
        assert_eq!(
            url_path_of(&queried),
            "/.well-known/oauth-protected-resource/mcp"
        );
        // And the lookalikes the route predicate lets through are refused here,
        // because they are not the derived path.
        let expected =
            url_path_of(&oauth::resource_metadata_url("https://h.example/mcp").expect("derives"))
                .to_string();
        for hostile in [
            "/.well-known/oauth-protected-resource/evil",
            "/.well-known/oauth-protected-resource/mcp/extra",
            "/.well-known/oauth-protected-resource",
        ] {
            assert_ne!(hostile, expected, "{hostile} must not be the served path");
        }
    }

    #[test]
    fn a_credential_is_keyed_by_issuer_and_resource_not_by_endpoint_twice() {
        // The defect this index exists to fix: the store key uses the VALIDATED
        // issuer, which a catalog id alone does not determine.
        let linear = catalog::by_id("linear").expect("linear");
        let real = creds::credential_key("main", "https://as.example", linear.endpoint);
        let wrong = creds::credential_key("main", linear.endpoint, linear.endpoint);
        assert_ne!(real, wrong, "these were the two halves that never met");
        assert_eq!(link_key("main", "linear"), "mcpidx/main/linear");
        assert_ne!(nonce_key("main", "linear"), link_key("main", "linear"));
    }

    #[test]
    fn a_tool_result_is_bounded_in_bytes_not_characters() {
        // A char-counted bound admits up to 4x the cap for non-ASCII content.
        let wide = "\u{1f41d}".repeat(MAX_TOOL_RESULT_BYTES);
        let cut = bounded(&wide);
        assert!(cut.len() <= MAX_TOOL_RESULT_BYTES, "{} bytes", cut.len());
        // Still valid UTF-8, cut on a boundary rather than mid-character.
        assert!(cut.chars().all(|c| c == '\u{1f41d}'));
        // Short input is untouched.
        assert_eq!(bounded("hello"), "hello");
    }
}
