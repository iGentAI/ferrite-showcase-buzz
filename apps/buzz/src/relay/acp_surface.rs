//! **buzz as an ACP AGENT**: a persona reachable from any editor.
//!
//! ACP connects an editor to an agent, so this is the face an editor drives. It is
//! perpendicular to [`crate::relay::mcp_surface`], which connects an agent to tools.
//!
//! # What a prompt actually does
//!
//! `session/prompt` runs a REAL persona turn — the same [`crate::agents::turns`]
//! pipeline a mention drives, with the same session engine, tool surface and model
//! routing. An editor talking to a buzz persona gets the persona, not a thin
//! reimplementation of one.
//!
//! Because that turn acts with the persona's access, a valid token is not enough
//! to drive one: [`authorize_driver`] requires the caller to pass the write gate
//! and to hold the persona's management authority, on every prompt.
//!
//! # The connection id is a real response header
//!
//! ACP's HTTP binding identifies a connection with an `Acp-Connection-Id` RESPONSE
//! header. This surface could not set one while `IngressReply` carried
//! `{correlation_id, status, body}` and nothing else, so the id travelled in the
//! `initialize` response's `_meta` and the deviation was recorded here.
//!
//! The pinned SDK carries `headers` and the gateway writes them, so `initialize`
//! now answers with the header the binding specifies. The `_meta` copy is KEPT
//! beside it for one release: a client written against the header-less shape
//! reads its connection id from either place and interoperates. Requests were
//! always conformant — the `acp-connection-id` REQUEST header is forwarded by the
//! route declaration and read on every later call.

use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json::{self, Value};
use ferrite_app::{AppContext, IngressRequest};

use crate::acp::agent::{self, AgentCall};
use crate::acp::proto::{self, Frame, Message, RequestId, RpcError};
use crate::acp::schema::{
    AgentCapabilities, ContentBlock, Implementation, McpCapabilities, MethodCapability,
    PromptCapabilities, SessionCapabilities, SessionNotification, SessionUpdate, StopReason,
};
use crate::acp::session::negotiate_version;
use crate::acp::transport::HEADER_CONNECTION_ID;
use crate::relay::ingress::Reply;
use crate::relay::{planes, store, BuzzError, Result, DEFAULT_COMMUNITY};

/// What this implementation calls itself in `initialize`.
const AGENT_NAME: &str = "buzz";
/// Kept in step with the MCP surface's literal, and for the same reason: the code
/// gate refuses `env!("CARGO_PKG_VERSION")` as a build-time read.
const AGENT_VERSION: &str = "0.1.0";

/// The most sessions one connection may open.
///
/// A session costs a durable row and a conversation thread. The cap bounds how many
/// one connection can hold AT ONCE; [`IDLE_TTL_MS`] bounds how long an abandoned one
/// persists. Generous enough that no real editor notices.
const MAX_SESSIONS_PER_CONNECTION: usize = 64;

/// How long an unused connection or session row lives.
///
/// A connection or session row with no TTL or teardown would let an editor that
/// leaks connections grow the store forever. The rows expire like the turn rows do,
/// and every authenticated USE refreshes
/// the expiry ([`require_connection`], [`require_session`]), so an active editor
/// never notices while an abandoned row costs one bounded window. An editor idle
/// past the window initializes again, which is one round trip.
///
/// What this does NOT bound, stated plainly: a caller can still write connection
/// rows as fast as it can call `initialize`. The expiry turns that from growth by
/// TOTAL into growth by RATE × window — the same trade [`TURN_TTL_MS`] makes —
/// and [`MAX_SESSIONS_PER_CONNECTION`] separately caps sessions per connection.
/// [`ACP_CONNECT_RATE_LIMIT`] bounds that rate per subject.
///
/// `pub` so the gate can drive the manual clock past it rather than restating it.
pub const IDLE_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// How often a hedged delivery re-reads the row while the winner works.
const WAIT_POLL_MS: u64 = 250;

/// How long to wait when the request carries no deadline — a turn-sized budget,
/// because that is what is being waited for.
const DEFAULT_WAIT_MS: u64 = 240_000;

/// A ceiling regardless of what deadline a caller asks for, so one prompt cannot hold
/// an in-flight slot indefinitely.
const MAX_WAIT_MS: u64 = 300_000;

/// How many model turns one subject may start over ACP per [`ACP_PROMPT_RATE_WINDOW_MS`].
///
/// A prompt runs a real persona turn on this deployment's provider credentials,
/// and [`authorize_driver`] decides only WHO may do that, not how often. Before
/// this bound the manifest's per-client route limit (600/min) was the only
/// ceiling. Every other spending path has a per-subject window like this one
/// (messages 60/min, schedules 30/hr, agent creation 20/hr); 20 turns a minute is
/// more than a person typing into an editor asks for. A retry answered from its
/// stored turn reaches no model and is not counted.
///
/// `pub` so the gate can fill the window instead of restating it.
pub const ACP_PROMPT_RATE_LIMIT: i64 = 20;
/// The fixed window [`ACP_PROMPT_RATE_LIMIT`] counts over.
const ACP_PROMPT_RATE_WINDOW_MS: u64 = 60 * 1000;
/// Rate class for [`ACP_PROMPT_RATE_LIMIT`] under `keys::rate`. `pub` for the gate.
pub const ACP_PROMPT_RATE_CLASS: &str = "acpturn";

/// How many connections one subject may open per [`ACP_CONNECT_RATE_WINDOW_MS`].
///
/// [`IDLE_TTL_MS`] turned leaked connection rows from growth by total into growth
/// by rate; this bounds the rate. Generous: an editor initializes once per start.
pub const ACP_CONNECT_RATE_LIMIT: i64 = 120;
/// The fixed window [`ACP_CONNECT_RATE_LIMIT`] counts over.
const ACP_CONNECT_RATE_WINDOW_MS: u64 = 60 * 60 * 1000;
/// Rate class for [`ACP_CONNECT_RATE_LIMIT`]. `pub` for the gate.
pub const ACP_CONNECT_RATE_CLASS: &str = "acpconn";

/// Margin left before the caller's deadline, so the answer is published while there is
/// still somebody to read it.
const WAIT_MARGIN_MS: u64 = 1_000;

/// How long a turn's stored answer stays available to a retry.
///
/// A TTL rather than a cleanup sweep, and the reasoning is worth keeping. Extracting
/// each answer into its own row took it out of the session's lifecycle, so deleting a
/// session would otherwise leave its turns behind. Sweeping the prefix on close is the
/// obvious repair and the wrong one: it needs a scan that can exhaust its budget
/// half-way, it races a prompt still writing after the sweep, and it does nothing for
/// the case that actually accumulates — a session nobody ever closes.
///
/// An expiry answers all three at once, bounding storage by RATE rather than by total
/// and needing nothing to be got right at delete time.
///
/// The cost, stated plainly: a retry arriving after this window re-runs the model. An
/// hour is far beyond any transport retry — the gateway's hedge fires in under a
/// second — and a client returning an hour later is asking again, not retrying.
const TURN_TTL_MS: u64 = 60 * 60 * 1000;

/// A connection, created by `initialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub(crate) struct ConnectionRow {
    pub(crate) id: String,
    pub(crate) protocol_version: u16,
    /// Who authenticated. Every session on this connection acts as them.
    pub(crate) subject: String,
    pub(crate) client_name: Option<String>,
    pub(crate) created_ms: u64,
}

/// A session, created by `session/new`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub(crate) struct SessionRow {
    pub(crate) id: String,
    /// The connection that created it — the check that keeps two editors apart.
    pub(crate) connection_id: String,
    /// The persona that answers here.
    pub(crate) agent: String,
    pub(crate) cwd: String,
    pub(crate) created_ms: u64,
}

/// One completed turn, stored under a key only that turn writes.
///
/// Not a field on the session row, and that is the point. Two overlapping turns
/// rewriting one row lose each other's work — the later write erases the earlier
/// answer, so its retry finds nothing and runs the model again. A key per turn has
/// no such contention: concurrent turns have different retry keys, so they write
/// different rows and neither can overwrite the other.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub(crate) struct TurnRow {
    pub(crate) answer: String,
}

fn connection_key(community: &str, id: &str) -> String {
    format!("acpconn/{community}/{id}")
}

/// `acpsess/{community}/{connection}/{session}` — a session lives UNDER its
/// connection.
///
/// Two things fall out of that, and both were defects before it.
///
/// Enumeration becomes complete: a connection's sessions are a prefix of their own,
/// bounded by [`MAX_SESSIONS_PER_CONNECTION`], so listing cannot silently truncate
/// and the cap cannot be bypassed by key ordering. Scanning one global page and
/// filtering could do both.
///
/// And ownership becomes STRUCTURAL. A caller composes the key from the connection it
/// is calling on, so somebody else's session is not refused — it is not addressable.
/// A check can be forgotten at a new call site; a key cannot.
fn session_key(community: &str, connection_id: &str, session_id: &str) -> String {
    format!("acpsess/{community}/{connection_id}/{session_id}")
}

/// Every session belonging to one connection.
fn session_prefix(community: &str, connection_id: &str) -> String {
    format!("acpsess/{community}/{connection_id}/")
}

/// `acpturn/{community}/{connection}/{session}/{retry}` — one turn's answer.
///
/// A key only that turn writes, which is what makes concurrent turns safe: two
/// requests in flight at once have different retry keys, so they write different
/// rows. Keeping the answer on the session row instead meant the later writer erased
/// the earlier one's, and that turn's retry then re-ran the model.
fn turn_key(community: &str, connection_id: &str, session_id: &str, retry_key: &str) -> String {
    format!("acpturn/{community}/{connection_id}/{session_id}/{retry_key}")
}

/// A stable id from stable inputs.
///
/// The platform gives an app no entropy, so identity is DERIVED rather than drawn.
/// That is not a workaround: it makes a retried call land on the same id, which is
/// what stops a redelivered prompt from opening a second session.
fn derive_id(prefix: &str, parts: &[&str]) -> String {
    let mut buf = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            buf.push(0x1f);
        }
        buf.extend_from_slice(part.as_bytes());
    }
    let digest = crate::mcp::sha256::sha256(&buf);
    let hex = crate::relay::crypto::to_hex(&digest);
    format!("{prefix}_{}", &hex[..32])
}

/// What this agent tells an editor it can do.
///
/// Honest rather than aspirational: `loadSession` is false because a buzz session's
/// history lives in the turn engine and is not re-openable by id from outside, and
/// claiming it would make an editor offer a resume that fails.
fn capabilities() -> AgentCapabilities {
    AgentCapabilities {
        load_session: false,
        prompt_capabilities: PromptCapabilities {
            image: false,
            audio: false,
            embedded_context: false,
        },
        mcp_capabilities: McpCapabilities {
            http: false,
            sse: false,
        },
        session_capabilities: SessionCapabilities {
            list: MethodCapability::supported(),
            delete: MethodCapability::supported(),
            close: MethodCapability::supported(),
            // Absent rather than false: this agent does not offer them, and the
            // schema says omission IS the way to say so.
            additional_directories: None,
            resume: None,
            meta: None,
        },
        auth: None,
        meta: None,
    }
}

/// What one call produces: its answer, and any notifications that must precede it.
///
/// Notifications exist because ACP delivers a turn's CONTENT through
/// `session/update`, not through the prompt's response — `PromptResponse` carries
/// only `stopReason`. Returning them beside the result lets the POST answer with the
/// transport's own grouped-reply shape, so the answer reaches a client that opened no
/// stream.
struct Outcome {
    result: Value,
    updates: Vec<Value>,
    /// Response headers this call contributes to the POST answer. Only
    /// `initialize` sets one — the `Acp-Connection-Id` the HTTP binding names.
    headers: Vec<(String, String)>,
}

impl Outcome {
    fn just(result: Value) -> Self {
        Self {
            result,
            updates: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// Contribute one response header to the reply this call is answered in.
    fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }
}

/// Serve one `POST /acp` frame.
///
/// Always `Ok` with an HTTP status: a JSON-RPC error is a 200 carrying an error
/// object, because the transport succeeded even when the call did not. Only a
/// transport-level refusal (unauthenticated, unparseable) gets a non-200.
pub(crate) async fn serve_acp(
    ctx: &AppContext,
    req: &IngressRequest,
    authorization: Option<&str>,
) -> Result<Reply> {
    // Identity FIRST: an unauthenticated caller learns nothing about this
    // deployment, not even whether a session id exists.
    let subject = match authenticate(ctx, authorization).await? {
        Some(subject) => subject,
        None => {
            return Ok(Reply::json(
                401,
                encode(&serde_json::json!({
                    "error": "an ACP connection requires a bearer token",
                    "how": "mint one at POST /buzz/api/mcp/token with a NIP-98 signature"
                }))?,
            ))
        }
    };

    let frame = match Frame::parse(&req.body) {
        Ok(frame) => frame,
        Err(e) => {
            return Ok(Reply::json(
                400,
                encode(&error_response(RequestId::Null, &e))?,
            ))
        }
    };

    let connection_id = crate::relay::ingress::request_header(req, HEADER_CONNECTION_ID);
    let mut answers: Vec<Value> = Vec::new();
    // Response headers the calls in this frame contribute — in practice the one
    // `Acp-Connection-Id` an `initialize` produces.
    let mut headers: Vec<(String, String)> = Vec::new();

    // Entries that did not parse are answered individually — JSON-RPC §6, and the
    // reason `Frame` keeps them rather than rejecting the batch.
    for invalid in frame.invalid() {
        answers.push(error_response(RequestId::Null, invalid));
    }

    // Every entry of a batch shares the request's correlation id, so each carries its
    // position too: two `session/new` entries of one batch are two sessions.
    for (batch_index, message) in frame.messages().into_iter().enumerate() {
        let (produced, contributed) = dispatch(
            ctx,
            &subject,
            connection_id,
            &req.correlation_id,
            batch_index,
            req.deadline_ms,
            message,
        )
        .await?;
        answers.extend(produced);
        headers.extend(contributed);
    }

    // A frame of nothing but notifications has no reply at all, which is what 202
    // means here.
    if answers.is_empty() {
        return Ok(Reply::bodyless(202));
    }
    let reply = if answers.len() == 1 && matches!(frame, Frame::Single(_)) {
        Reply::json(200, encode(&answers.remove(0))?)
    } else {
        Reply::json(200, encode(&Value::Array(answers))?)
    };
    // A batch carrying two `initialize` calls would contribute two ids. Repeated
    // header names survive the envelope and the gateway
    // (`validate_reply_headers` preserves them), so both are emitted rather
    // than one silently winning; each connection id is also in its own
    // response's `_meta`, which is where a batching client reads it.
    Ok(headers
        .into_iter()
        .fold(reply, |reply, (name, value)| reply.with_header(&name, value)))
}

/// One message in, its notifications and at most one answer out, plus any
/// response headers the call contributes to the POST answer.
async fn dispatch(
    ctx: &AppContext,
    subject: &str,
    connection_id: Option<&str>,
    correlation_id: &str,
    batch_index: usize,
    deadline_ms: u64,
    message: &Message,
) -> Result<(Vec<Value>, Vec<(String, String)>)> {
    let (id, method, params) = match message {
        Message::Request { id, method, params } => (Some(id.id().clone()), method, params),
        Message::Notification { method, params } => (None, method, params),
        // A response arriving at the agent face answers nothing we asked.
        Message::Response { .. } => return Ok((Vec::new(), Vec::new())),
    };

    let call = match agent::parse_call(method, params.as_ref()) {
        Ok(call) => call,
        Err(e) => {
            return Ok((
                id.map(|id| vec![error_response(id, &e)]).unwrap_or_default(),
                Vec::new(),
            ))
        }
    };

    // A notification is never answered, however it turns out. Answering one puts an
    // unmatched response on the wire that the peer must discard.
    let answered = id.is_some();
    // The request id, as it appeared on the wire. A RETRY resends the identical
    // frame, id included; a fresh question carries a fresh id — which is what lets
    // the prompt cache tell one from the other.
    let request_id = id.as_ref().map(|id| id.to_value().to_string());
    let outcome = run(
        ctx,
        subject,
        connection_id,
        correlation_id,
        batch_index,
        deadline_ms,
        request_id.as_deref(),
        call,
    )
    .await;
    if !answered {
        return Ok((Vec::new(), Vec::new()));
    }
    let id = id.unwrap_or(RequestId::Null);
    Ok(match outcome {
        Ok(outcome) => {
            // Updates FIRST: a client reading the array in order sees the turn's
            // content before the stopReason that ends it.
            let mut out = outcome.updates;
            out.push(result_response(id, outcome.result));
            (out, outcome.headers)
        }
        // A failed call contributes no header: the connection it would name
        // was never created.
        Err(e) => (vec![error_response(id, &e)], Vec::new()),
    })
}

/// Execute one parsed call.
async fn run(
    ctx: &AppContext,
    subject: &str,
    connection_id: Option<&str>,
    correlation_id: &str,
    batch_index: usize,
    deadline_ms: u64,
    request_id: Option<&str>,
    call: AgentCall,
) -> std::result::Result<Outcome, RpcError> {
    let community = DEFAULT_COMMUNITY;
    let now_ms = ctx.now_ms();

    match call {
        AgentCall::Initialize(request) => {
            let version = negotiate_version(request.protocol_version).ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "protocol version {} is below the minimum this agent speaks",
                    request.protocol_version
                ))
            })?;
            admit_rate(
                ctx,
                subject,
                ACP_CONNECT_RATE_CLASS,
                ACP_CONNECT_RATE_LIMIT,
                ACP_CONNECT_RATE_WINDOW_MS,
                now_ms,
            )
            .await?;
            // The correlation id is unique per request and platform-provided, so two
            // initializes by the same subject in the same millisecond are two
            // connections. Without it they merged — and merged connections share
            // sessions, which is the isolation this surface exists to provide.
            // Derived from the request alone (its correlation id, its place in a batch and
            // its JSON-RPC id), never the clock: a hedged or redelivered copy of the same
            // request lands on the same connection, and two entries of one batch do not.
            let id = derive_id(
                "conn",
                &[subject, correlation_id, &batch_index.to_string(), request_id.unwrap_or("-")],
            );
            let row = ConnectionRow {
                id: id.clone(),
                protocol_version: version,
                subject: subject.to_string(),
                client_name: request.client_info.as_ref().map(|i| i.name.clone()),
                created_ms: now_ms,
            };
            // Expiring, not plain: an abandoned connection must not be a permanent
            // row. Use refreshes the window — see `IDLE_TTL_MS`.
            put_expiring(
                ctx,
                &connection_key(community, &id),
                &row,
                now_ms.saturating_add(IDLE_TTL_MS),
            )
            .await?;

            let mut response =
                ferrite_app::serde_json::to_value(crate::acp::schema::InitializeResponse {
                    protocol_version: version,
                    agent_capabilities: capabilities(),
                    auth_methods: Vec::new(),
                    agent_info: Some(Implementation {
                        name: AGENT_NAME.to_string(),
                        title: Some("buzz workspace personas".to_string()),
                        version: AGENT_VERSION.to_string(),
                        meta: None,
                    }),
                    meta: None,
                })
                .map_err(|e| RpcError::new(proto::INTERNAL_ERROR, e.to_string()))?;
            // The connection id travels BOTH ways for one release: as the
            // `Acp-Connection-Id` response header the HTTP binding specifies
            // (set below, now that `IngressReply` carries headers), and as this
            // `_meta` copy, which is what a client written against the
            // header-less shape reads. See the module docs.
            response
                .as_object_mut()
                .ok_or_else(|| {
                    RpcError::new(
                        proto::INTERNAL_ERROR,
                        "initialize response did not serialize as an object",
                    )
                })?
                .insert(
                    "_meta".to_string(),
                    serde_json::json!({
                        "acp": { "connectionId": id.clone() },
                        "note": "send this as the acp-connection-id request header on later calls"
                    }),
                );
            Ok(Outcome::just(response).with_header(HEADER_CONNECTION_ID, id))
        }

        AgentCall::NewSession(request) => {
            let connection = require_connection(ctx, connection_id, subject).await?;
            let open = count_sessions(ctx, &connection.id).await?;
            if open >= MAX_SESSIONS_PER_CONNECTION {
                return Err(RpcError::invalid_params(format!(
                    "this connection already has {open} sessions open (max {MAX_SESSIONS_PER_CONNECTION})"
                )));
            }
            // Which persona answers. Named in `_meta`, or the deployment's default.
            let agent_id = request
                .meta
                .as_ref()
                .and_then(|m| m.get("buzz"))
                .and_then(|b| b.get("agent"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let agent_id = resolve_persona(ctx, agent_id.as_deref()).await?;
            authorize_driver(ctx, subject, &agent_id, now_ms).await?;

            // As for the connection: the request names the session, and the persona is part
            // of it, so two sessions on one cwd for two personas are two sessions.
            let id = derive_id(
                "sess",
                &[
                    &connection.id,
                    &request.cwd,
                    &agent_id,
                    correlation_id,
                    &batch_index.to_string(),
                    request_id.unwrap_or("-"),
                ],
            );
            let row = SessionRow {
                id: id.clone(),
                connection_id: connection.id.clone(),
                agent: agent_id.clone(),
                cwd: request.cwd.clone(),
                created_ms: now_ms,
            };
            // Expiring for the same reason the connection row is: a session nobody
            // ever closes must cost a window, not a permanent row.
            put_expiring(
                ctx,
                &session_key(community, &connection.id, &id),
                &row,
                now_ms.saturating_add(IDLE_TTL_MS),
            )
            .await?;
            Ok(Outcome::just(serde_json::json!({
                "sessionId": id,
                "_meta": { "buzz": { "agent": agent_id } }
            })))
        }

        AgentCall::Prompt(request) => {
            let connection = require_connection(ctx, connection_id, subject).await?;
            let session = require_session(ctx, &request.session_id, &connection.id).await?;
            // Again on every turn, not only when the session opened: standing and
            // ownership can change in between, and a retry that would be answered
            // from a stored turn is refused too, since that answer is persona output.
            authorize_driver(ctx, subject, &session.agent, now_ms).await?;

            // Only the textual blocks: this agent declares neither image nor audio
            // prompt capability, so acting on one would be answering a question it
            // told the client it could not read.
            let text: String = request
                .prompt
                .iter()
                .filter_map(ContentBlock::as_text)
                .collect::<Vec<_>>()
                .join("\n");
            if text.trim().is_empty() {
                return Err(RpcError::invalid_params(
                    "the prompt carried no text content this agent can read",
                ));
            }

            // ONE identity, used for the cache key and the engine admission alike, so
            // the two cannot disagree. It is `(session, request id, prompt text)`: the
            // REQUEST ID is what separates a retry from a repeat, because a retry
            // resends the identical frame while asking again carries a fresh id.
            //
            // No turn POSITION is needed. It was there so a repeat would not meet the
            // earlier admission, and the request id already guarantees that — while a
            // counter is shared mutable state two concurrent turns would fight over.
            let retry_key = derive_id("turn", &[&session.id, request_id.unwrap_or("-"), &text]);
            let turn_row = turn_key(community, &connection.id, &session.id, &retry_key);

            // A retry is answered from the turn's own row, without reaching the model.
            // A client retries precisely when it did NOT receive the answer, so a
            // "duplicate" placeholder would leave it no better off.
            let existing: Option<TurnRow> = get(ctx, &turn_row).await.map_err(internal)?;
            let (answer, replayed) = match existing {
                Some(row) => (row.answer, true),
                None => {
                    // Counted only here, where a model is actually reached: a
                    // retry answered from its stored turn above costs nothing.
                    // The gateway re-issues a slow prompt while its first delivery
                    // still runs; those copies find no turn row yet. One charge per
                    // request: the first delivery claims it, and a copy that finds the
                    // claim goes straight to waiting for the winner, charging nothing
                    // (and never answering "rate-limited" for a prompt that is running).
                    let charge_key = format!("{turn_row}/charged");
                    let mut config = planes::store(ctx, store::CONFIG).map_err(|e| internal(storage(e)))?;
                    let first = store::put_expiring_doc_if_version(
                        &mut config,
                        charge_key.as_bytes(),
                        &true,
                        None,
                        now_ms.saturating_add(TURN_TTL_MS),
                    )
                    .await
                    .map_err(internal)?
                    .is_ok();
                    if first {
                        if let Err(refused) = admit_rate(
                            ctx,
                            subject,
                            ACP_PROMPT_RATE_CLASS,
                            ACP_PROMPT_RATE_LIMIT,
                            ACP_PROMPT_RATE_WINDOW_MS,
                            now_ms,
                        )
                        .await
                        {
                            // Not admitted, so nothing runs: a later retry is charged anew.
                            let _ = delete(ctx, &charge_key).await;
                            return Err(refused);
                        }
                    }
                    let thread = format!("acp-{}", session.id);
                    let llm: std::sync::Arc<dyn crate::agents::turns::LlmTransport> =
                        std::sync::Arc::new(crate::agents::turns::DirectLlm);
                    // `run_turn` loads the persona's definition, so its tool
                    // filter, approvals, step budget and instructions apply here
                    // exactly as on a mention.
                    let drive = crate::agents::turns::drive_prompt(
                        ctx,
                        &llm,
                        &session.agent,
                        &thread,
                        &retry_key,
                        text.as_bytes(),
                        // The driver is the member the persona acts for: its
                        // reads and connector calls are bounded by the driver's.
                        Some(subject),
                    )
                    .await;

                    match drive {
                        crate::agents::turns::PromptDrive::Answered { answer, lease } => {
                            let answer = String::from_utf8_lossy(&answer).to_string();
                            // The row first, then the lease: a settled lease
                            // promises a retry finds the row.
                            put_expiring(
                                ctx,
                                &turn_row,
                                &TurnRow {
                                    answer: answer.clone(),
                                },
                                now_ms.saturating_add(TURN_TTL_MS),
                            )
                            .await?;
                            lease.settle(ctx).await;
                            (answer, false)
                        }
                        // Another delivery of THIS request holds the lease — and
                        // on this route that is routine, not rare: the gateway
                        // re-issues a request that has not replied within its
                        // hedge window, and a model turn is always slower. Wait
                        // for its row; a driver that died lets its lease lapse,
                        // and the next retry takes the turn over.
                        crate::agents::turns::PromptDrive::Busy => {
                            match await_winner(ctx, &turn_row, deadline_ms).await? {
                                Some(row) => (row.answer, true),
                                None => {
                                    return Err(RpcError::new(
                                        proto::INTERNAL_ERROR,
                                        "this prompt is still running; ask again in a moment",
                                    ))
                                }
                            }
                        }
                        crate::agents::turns::PromptDrive::Retry(reason) => {
                            ferrite_app::tracing::warn!(
                                "acp prompt {retry_key} did not finish: {reason}"
                            );
                            return Err(RpcError::new(
                                proto::INTERNAL_ERROR,
                                "this prompt did not finish; ask again in a moment",
                            ));
                        }
                    }
                }
            };

            // The answer travels as ACP CONTENT, not as private metadata: a turn's
            // content is delivered by `session/update`, and `PromptResponse` carries
            // only a stopReason. Returned beside the response so the POST answers with
            // the transport's grouped-reply shape and a client that opened no stream
            // still receives it.
            let update = Message::notification(
                "session/update",
                ferrite_app::serde_json::to_value(SessionNotification {
                    session_id: session.id.clone(),
                    update: SessionUpdate::AgentMessageChunk {
                        content: ContentBlock::text(answer),
                    },
                    meta: None,
                })
                .map_err(|e| RpcError::new(proto::INTERNAL_ERROR, e.to_string()))?,
            )
            .to_value();

            Ok(Outcome {
                result: serde_json::json!({
                    "stopReason": StopReason::EndTurn,
                    // `replayed` is the protocol's reserved extension point doing what
                    // it is for. It states outright whether this call reached the
                    // model, which a client cannot otherwise tell — and it is a fact
                    // about THIS turn's own row, so no concurrent turn can make it
                    // wrong the way a shared counter could.
                    "_meta": { "buzz": { "agent": session.agent, "replayed": replayed } }
                }),
                updates: vec![update],
                headers: Vec::new(),
            })
        }

        AgentCall::Cancel { session_id } => {
            // Accepted and DISCARDED, and that is the honest description: nothing
            // here interrupts a running turn, and no path in this surface ever
            // yields `stopReason: "cancelled"`. A turn already dispatched runs to
            // completion at the model whatever the editor does. That is stated
            // here rather than dressed up as best-effort.
            let _ = session_id;
            Ok(Outcome::just(Value::Null))
        }

        AgentCall::ListSessions => {
            let connection = require_connection(ctx, connection_id, subject).await?;
            let sessions = sessions_of(ctx, &connection.id).await?;
            Ok(Outcome::just(serde_json::json!({
                "sessions": sessions
                    .iter()
                    .map(|s| serde_json::json!({ "sessionId": s.id, "cwd": s.cwd }))
                    .collect::<Vec<_>>()
            })))
        }

        AgentCall::DeleteSession { session_id } | AgentCall::CloseSession { session_id } => {
            let connection = require_connection(ctx, connection_id, subject).await?;
            let session = require_session(ctx, &session_id, &connection.id).await?;
            delete(ctx, &session_key(community, &connection.id, &session.id)).await?;
            Ok(Outcome::just(serde_json::json!({})))
        }

        // Declared unsupported in `capabilities()`, so a conformant client never
        // sends these. Answered by name rather than ignored, for one that does.
        AgentCall::LoadSession { .. } => Err(RpcError::method_not_found("session/load")),
        AgentCall::ResumeSession { .. } => Err(RpcError::method_not_found("session/resume")),
        AgentCall::SetMode { .. } => Err(RpcError::method_not_found("session/set_mode")),
        AgentCall::SetConfigOption { .. } => {
            Err(RpcError::method_not_found("session/set_config_option"))
        }

        // Authentication happens at the transport, so there is nothing to do here —
        // and saying so is better than a refusal a client would retry.
        AgentCall::Authenticate { .. } | AgentCall::Logout => Ok(Outcome::just(serde_json::json!({}))),
    }
}

/// How long a hedged delivery may wait for the winner.
///
/// Three states, kept apart because collapsing them is what made a caller who had
/// already given up buy a four-minute wait:
///
/// * **no deadline** (the platform's `0` sentinel) — a turn-sized default, since
///   that is what is being waited for;
/// * **a usable deadline** — the time remaining, less a margin so the answer is
///   published while somebody is still there to read it, and capped so one prompt
///   cannot hold an in-flight slot indefinitely;
/// * **past or inside the margin** — nothing. The caller still gets one read, in
///   case the winner has already landed, and then an answer rather than a sleep.
const fn wait_budget_ms(deadline_ms: u64, now_ms: u64) -> u64 {
    if deadline_ms == 0 {
        return DEFAULT_WAIT_MS;
    }
    let remaining = deadline_ms.saturating_sub(now_ms);
    if remaining <= WAIT_MARGIN_MS {
        return 0;
    }
    let usable = remaining - WAIT_MARGIN_MS;
    if usable > MAX_WAIT_MS {
        MAX_WAIT_MS
    } else {
        usable
    }
}

/// Wait for the delivery that WON this admission to record its answer.
///
/// The gateway re-issues a request that has not replied within its hedge window, and
/// a model turn always outlasts that — so a prompt's second delivery is routine
/// rather than exceptional, and it must end up with the same answer as the first
/// instead of a refusal. Polling is the available mechanism: the two deliveries are
/// separate tasks with no channel between them, and the turn's row IS the rendezvous.
async fn await_winner(
    ctx: &AppContext,
    turn_row: &str,
    deadline_ms: u64,
) -> std::result::Result<Option<TurnRow>, RpcError> {
    let started = ctx.now_ms();
    let budget = wait_budget_ms(deadline_ms, started);
    loop {
        if let Some(row) = get::<TurnRow>(ctx, turn_row).await.map_err(internal)? {
            return Ok(Some(row));
        }
        let elapsed = ctx.now_ms().saturating_sub(started);
        let Some(left) = budget.checked_sub(elapsed).filter(|left| *left > 0) else {
            return Ok(None);
        };
        // Clamped to what remains, so the wait cannot overshoot its own cutoff by a
        // whole poll interval.
        ferrite_app::rt::time::sleep(core::time::Duration::from_millis(WAIT_POLL_MS.min(left)))
            .await;
    }
}

/// Who is calling, or `None` if nobody proved it.
async fn authenticate(ctx: &AppContext, authorization: Option<&str>) -> Result<Option<String>> {
    let Some(bearer) = crate::mcp::server::bearer_of(authorization) else {
        return Ok(None);
    };
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;
    // The issuer and audience are BOTH this deployment's canonical resource under the
    // self-issued regime, and both are checked: an authentic MAC over a claim naming
    // some other resource is genuine and untrue at once.
    let (secret, resource) =
        crate::relay::mcp_surface::self_issued_identity(ctx, &mut config).await?;
    Ok(
        crate::mcp::tokens::verify(&secret, bearer, &resource, &resource, ctx.now_ms())
            .ok()
            .map(|claims| claims.sub),
    )
}

/// The connection this call belongs to, refusing one that is not this caller's.
///
/// The check that keeps two editors apart: a connection id is a bearer capability if
/// nothing binds it to the identity that created it.
async fn require_connection(
    ctx: &AppContext,
    connection_id: Option<&str>,
    subject: &str,
) -> std::result::Result<ConnectionRow, RpcError> {
    let id = connection_id.ok_or_else(|| {
        RpcError::new(
            proto::INVALID_REQUEST,
            format!("{HEADER_CONNECTION_ID} is required — call initialize first"),
        )
    })?;
    let row: Option<ConnectionRow> = get(ctx, &connection_key(DEFAULT_COMMUNITY, id))
        .await
        .map_err(internal)?;
    let row = row.ok_or_else(|| RpcError::new(proto::RESOURCE_NOT_FOUND, "no such connection"))?;
    if row.subject != subject {
        // Reported as absent rather than forbidden: whether somebody else's
        // connection exists is not this caller's to learn.
        return Err(RpcError::new(
            proto::RESOURCE_NOT_FOUND,
            "no such connection",
        ));
    }
    // A row in USE is a row kept: refresh the idle expiry on every authenticated
    // touch, so only a connection nobody has used for `IDLE_TTL_MS` expires.
    // Best-effort — a failed refresh leaves the previous expiry standing, which
    // errs toward expiring sooner, never toward failing the call it rode on.
    let _ = put_expiring(
        ctx,
        &connection_key(DEFAULT_COMMUNITY, id),
        &row,
        ctx.now_ms().saturating_add(IDLE_TTL_MS),
    )
    .await;
    Ok(row)
}

/// The session, addressed THROUGH its connection.
///
/// The key is composed from the connection making the call, so a session belonging to
/// another connection is not refused — it is not addressable at all. That is stronger
/// than a check, because a check can be forgotten at a new call site.
async fn require_session(
    ctx: &AppContext,
    session_id: &str,
    connection_id: &str,
) -> std::result::Result<SessionRow, RpcError> {
    let row: Option<SessionRow> = get(
        ctx,
        &session_key(DEFAULT_COMMUNITY, connection_id, session_id),
    )
    .await
    .map_err(internal)?;
    let row = row.ok_or_else(|| agent::unknown_session(session_id))?;
    // The same sliding window the connection row gets: addressing a session is
    // using it. `session/list` deliberately does NOT refresh (a scan-wide rewrite
    // would turn one read into sixty-four writes), so an idle session expires even
    // under a client that only ever polls the roster.
    let _ = put_expiring(
        ctx,
        &session_key(DEFAULT_COMMUNITY, connection_id, session_id),
        &row,
        ctx.now_ms().saturating_add(IDLE_TTL_MS),
    )
    .await;
    Ok(row)
}

/// May `subject` drive `agent`'s turn, right now?
///
/// A prompt runs a full persona turn: its tools post, react and read AS the
/// persona, with the persona's room access and memory, and the answer comes back
/// to the caller. Without this check any holder of a self-minted token could ask a
/// persona to read a private room it belongs to and print the result, or speak
/// under its signature, without being a member, while banned, or without any
/// relation to the persona at all. Every other path that spends a turn or acts as
/// someone else is gated, and this one is gated the same way:
///
/// 1. the subject must be a workspace identity (the token's subject is a pubkey);
/// 2. the write gate — closed-community membership, then any standing ban or
///    timeout — because a turn spends provider budget and can write;
/// 3. the persona's own management authority (its creator, a community
///    moderator, or an operator), because the turn exposes what the persona can
///    read and remember, which is exactly what that authority already guards;
/// 4. a paused persona does not answer here any more than it does to a mention.
///
/// Asked at `session/new` AND on every `session/prompt`: a sanction or a change
/// of ownership after the session opened must stop the next turn. Every failure
/// refuses; a storage error is an internal error, never a pass.
async fn authorize_driver(
    ctx: &AppContext,
    subject: &str,
    agent: &str,
    now_ms: u64,
) -> std::result::Result<(), RpcError> {
    let community = DEFAULT_COMMUNITY;
    let refused = |why: String| RpcError::new(proto::APPLICATION_ERROR, why);
    let who = crate::relay::crypto::from_hex32(subject)
        .ok_or_else(|| refused("this token's subject is not a workspace identity".into()))?;
    crate::relay::ingress::write_gate(ctx, community, &who, now_ms)
        .await
        .map_err(|e| match e {
            BuzzError::Unauthorized(_) => refused(e.to_string()),
            other => internal(other),
        })?;
    let mut config = planes::store(ctx, store::CONFIG).map_err(|e| internal(storage(e)))?;
    if !crate::relay::ingress::may_manage(ctx, &mut config, community, agent, &who)
        .await
        .map_err(internal)?
    {
        return Err(refused(format!(
            "only {agent:?}'s creator, a community moderator, or an operator may drive it over ACP"
        )));
    }
    if crate::relay::ingress::agent_paused(ctx, community, agent)
        .await
        .map_err(internal)?
    {
        return Err(refused(format!("{agent:?} is paused")));
    }
    Ok(())
}

/// The message an ACP call gets when its use could not be counted.
///
/// `pub` for the gate, which asserts on it.
pub const ACP_RATE_UNCONFIRMED: &str =
    "the rate limit could not be confirmed for this call; retry shortly";

/// Count one use of `class` against `subject`'s fixed window, refusing past `limit`.
///
/// The same counter the message, schedule and agent-creation limits use
/// (`keys::rate` + `store::bump_expiring_counter`), but it fails CLOSED. These
/// two classes bound model turns and connection rows, both of which spend this
/// deployment's budget, so a use that cannot be counted is not admitted:
///
/// * a subject that is not a pubkey has no window to count against, so it is
///   refused outright. `initialize` reaches this before any authorization
///   check, so letting such a subject through uncounted made connections
///   unlimited for it;
/// * a counter that could not be read or bumped is a RETRYABLE refusal (the
///   error's `data.retryable` is `true`), because the store blip is transient
///   and the caller can simply ask again. Admitting on an error meant anybody
///   who could make the counter fail had no limit at all.
async fn admit_rate(
    ctx: &AppContext,
    subject: &str,
    class: &str,
    limit: i64,
    window_ms: u64,
    now_ms: u64,
) -> std::result::Result<(), RpcError> {
    let Some(who) = crate::relay::crypto::from_hex32(subject) else {
        return Err(RpcError::new(
            proto::APPLICATION_ERROR,
            "this token's subject is not a workspace identity, so its use cannot be counted",
        ));
    };
    let unconfirmed = || {
        RpcError::with_data(
            proto::APPLICATION_ERROR,
            ACP_RATE_UNCONFIRMED,
            serde_json::json!({ "retryable": true }),
        )
    };
    let Ok(mut config) = planes::store(ctx, store::CONFIG) else {
        ferrite_app::tracing::warn!("acp {class} limiter: the config plane is unavailable");
        return Err(unconfirmed());
    };
    let key = crate::relay::keys::rate(DEFAULT_COMMUNITY, &who, class);
    match store::bump_expiring_counter(&mut config, &key, 1, now_ms.saturating_add(window_ms))
        .await
    {
        Ok(count) if count > limit => Err(RpcError::new(
            proto::APPLICATION_ERROR,
            format!("rate-limited: at most {limit} per window for this identity; try again shortly"),
        )),
        Ok(_) => Ok(()),
        Err(e) => {
            ferrite_app::tracing::warn!("acp {class} limiter could not count this use: {e}");
            Err(unconfirmed())
        }
    }
}

/// Which persona answers, refusing one the registry does not name.
async fn resolve_persona(
    ctx: &AppContext,
    requested: Option<&str>,
) -> std::result::Result<String, RpcError> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(|e| internal(storage(e)))?;
    let bytes = store::get_raw(&mut config, crate::relay::agents::REGISTRY_CONFIG)
        .await
        .map_err(internal)?;
    // STRICT: a malformed registry must not read as "no personas", which would tell
    // an editor this workspace has none when in fact its roster is unreadable.
    let registry = match bytes {
        Some(bytes) => crate::relay::agents::Registry::parse(&bytes).map_err(internal)?,
        None => crate::relay::agents::Registry::default(),
    };
    match requested {
        Some(name) => registry
            .personas
            .iter()
            .find(|p| p.agent == name)
            .map(|p| p.agent.clone())
            .ok_or_else(|| {
                RpcError::invalid_params(format!("no agent named {name:?} is registered"))
            }),
        None => registry
            .personas
            .first()
            .map(|p| p.agent.clone())
            .ok_or_else(|| {
                RpcError::new(
                    proto::RESOURCE_NOT_FOUND,
                    "this deployment has no personas registered",
                )
            }),
    }
}

/// Every session on this connection.
///
/// A complete enumeration rather than a filtered page: the rows live under the
/// connection's own prefix, so one scan sees all of them. Filtering a global page
/// could hide a connection's sessions behind another connection's, which made both
/// the listing and the cap unreliable.
///
/// Truncation is REFUSED rather than returned. This answer feeds the cap as well as
/// the listing, so a short page would under-report a connection's sessions and let it
/// exceed the very limit those rows define — and since the limit asked for is the cap
/// plus one, a truncated scan means that invariant is already broken.
async fn sessions_of(
    ctx: &AppContext,
    connection_id: &str,
) -> std::result::Result<Vec<SessionRow>, RpcError> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(|e| internal(storage(e)))?;
    let prefix = session_prefix(DEFAULT_COMMUNITY, connection_id).into_bytes();
    let limit = u32::try_from(MAX_SESSIONS_PER_CONNECTION.saturating_add(1)).unwrap_or(u32::MAX);
    let (rows, more) = store::scan(&mut config, &prefix, None, limit)
        .await
        .map_err(internal)?;
    if more {
        return Err(RpcError::new(
            proto::INTERNAL_ERROR,
            "this connection holds more sessions than its own limit allows; close some",
        ));
    }
    Ok(rows
        .iter()
        .filter_map(|(key, value, _v)| store::decode_doc::<SessionRow>(key, value).ok())
        .collect())
}

async fn count_sessions(
    ctx: &AppContext,
    connection_id: &str,
) -> std::result::Result<usize, RpcError> {
    sessions_of(ctx, connection_id).await.map(|s| s.len())
}

/// A row that erases itself, so nothing has to remember to.
///
/// The ONLY write this surface performs — every row it owns (connection, session,
/// turn) carries an expiry, so nothing here can grow the store by total.
async fn put_expiring<T: Serialize>(
    ctx: &AppContext,
    key: &str,
    value: &T,
    expires_at_ms: u64,
) -> std::result::Result<(), RpcError> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(|e| internal(storage(e)))?;
    store::put_expiring_doc(&mut config, key.as_bytes(), value, expires_at_ms)
        .await
        .map(|_version| ())
        .map_err(internal)
}

async fn get<T: for<'de> Deserialize<'de>>(ctx: &AppContext, key: &str) -> Result<Option<T>> {
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;
    store::get_doc::<T>(&mut config, key.as_bytes()).await
}

async fn delete(ctx: &AppContext, key: &str) -> std::result::Result<(), RpcError> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(|e| internal(storage(e)))?;
    store::delete_key(&mut config, key.as_bytes())
        .await
        .map(|_version| ())
        .map_err(internal)
}

fn storage(e: std::io::Error) -> BuzzError {
    crate::relay::ingress::storage_error(e)
}

fn internal(e: BuzzError) -> RpcError {
    RpcError::new(proto::INTERNAL_ERROR, e.to_string())
}

fn result_response(id: RequestId, result: Value) -> Value {
    Message::Response {
        id,
        outcome: Box::new(Ok(result)),
    }
    .to_value()
}

fn error_response(id: RequestId, error: &RpcError) -> Value {
    Message::Response {
        id,
        outcome: Box::new(Err(error.clone())),
    }
    .to_value()
}

fn encode(value: &Value) -> Result<Vec<u8>> {
    ferrite_app::serde_json::to_vec(value)
        .map_err(|e| BuzzError::Internal(format!("acp encode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_budget_respects_the_caller_and_never_invents_one() {
        // Only the platform's no-deadline sentinel takes the default. Treating an
        // expired or near deadline as "absent" bought a four-minute wait for a caller
        // who had already given up, holding an in-flight slot the whole time.
        assert_eq!(wait_budget_ms(0, 1_000_000), DEFAULT_WAIT_MS);

        // A usable deadline: the time remaining, less the publish margin.
        assert_eq!(
            wait_budget_ms(1_000_000 + 30_000, 1_000_000),
            30_000 - WAIT_MARGIN_MS
        );

        // Capped, so one prompt cannot hold a slot indefinitely however long a
        // caller says it will wait.
        assert_eq!(wait_budget_ms(u64::MAX, 1_000_000), MAX_WAIT_MS);

        // At or inside the margin there is no time to answer in — zero, not a
        // default. The caller still gets one read before that is reported.
        assert_eq!(wait_budget_ms(1_000_000 + WAIT_MARGIN_MS, 1_000_000), 0);
        assert_eq!(wait_budget_ms(1_000_000 + 1, 1_000_000), 0);

        // Already past, including well past.
        assert_eq!(wait_budget_ms(1_000_000, 1_000_000), 0);
        assert_eq!(wait_budget_ms(999_000, 1_000_000), 0);
        assert_eq!(wait_budget_ms(1, u64::MAX), 0);
    }

    #[test]
    fn capabilities_claim_only_what_this_agent_does() {
        // Honest rather than aspirational: claiming `loadSession` would make an editor
        // offer a resume that fails, which is worse than not offering it.
        let caps = capabilities();
        assert!(!caps.load_session);
        // Presence-encoded, per the schema: `Some({})` is supported, absence is not.
        assert!(caps.session_capabilities.resume.is_none());
        assert!(caps.session_capabilities.list.is_some());
        assert!(caps.session_capabilities.close.is_some());
        // And the prompt capabilities match what the prompt path actually reads.
        assert!(!caps.prompt_capabilities.image);
        assert!(!caps.prompt_capabilities.audio);
    }

    #[test]
    fn concurrent_turns_write_different_rows_so_neither_is_lost() {
        // The defect this closes: both turns rewrote ONE session row, so the later
        // write erased the earlier answer and that turn's retry re-ran the model.
        // A key per turn has no contention — different requests, different rows.
        let a = derive_id("turn", &["sess_1", "11", "same question"]);
        let b = derive_id("turn", &["sess_1", "12", "same question"]);
        assert_ne!(a, b);
        assert_ne!(
            turn_key("main", "c1", "sess_1", &a),
            turn_key("main", "c1", "sess_1", &b),
            "two overlapping turns must not share a row"
        );

        // And a turn row cannot be mistaken for the session it belongs to.
        assert!(turn_key("main", "c1", "s1", &a).starts_with("acpturn/"));
        assert!(!turn_key("main", "c1", "s1", &a).starts_with(&session_prefix("main", "c1")));
    }

    #[test]
    fn a_session_lives_under_its_connection_so_ownership_is_structural() {
        // Not a check that can be forgotten at a new call site: the key composed from
        // one connection simply does not address another's session.
        let mine = session_key("main", "conn_a", "sess_1");
        let theirs = session_key("main", "conn_b", "sess_1");
        assert_ne!(mine, theirs, "the same session id under two connections");
        assert!(mine.starts_with(&session_prefix("main", "conn_a")));
        assert!(!mine.starts_with(&session_prefix("main", "conn_b")));

        // And a connection's own prefix does not reach a sibling whose id merely
        // starts the same way.
        assert!(!session_key("main", "conn_ab", "s").starts_with(&session_prefix("main", "conn_a")));
    }

    #[test]
    fn ids_are_derived_so_a_retry_lands_on_the_same_one() {
        // The platform gives an app no entropy, and that turns out to be the right
        // shape: a redelivered call cannot open a second session.
        let a = derive_id("sess", &["conn1", "/w", "1000", "corr"]);
        let b = derive_id("sess", &["conn1", "/w", "1000", "corr"]);
        assert_eq!(a, b);
        assert!(a.starts_with("sess_"));

        // Different inputs, different id — including the separator case, so that
        // ("ab","c") and ("a","bc") cannot collide.
        assert_ne!(a, derive_id("sess", &["conn1", "/w", "1001", "corr"]));
        assert_ne!(
            derive_id("x", &["ab", "c"]),
            derive_id("x", &["a", "bc"]),
            "component boundaries must be part of the digest"
        );
    }

    #[test]
    fn an_idle_connection_expires_rather_than_accreting() {
        // Long enough that no editor in daily use ever re-initializes mid-thought…
        const {
            assert!(
                IDLE_TTL_MS >= 8 * 60 * 60 * 1000,
                "must outlast a working day"
            )
        };
        // …and short enough that an abandoned row is a window, not a permanence.
        const {
            assert!(
                IDLE_TTL_MS <= 7 * 24 * 60 * 60 * 1000,
                "must not be a de facto forever"
            )
        };
        // A turn's replay window must not outlive the session it belongs to, or a
        // retry could find its answer under a session that no longer exists.
        const { assert!(IDLE_TTL_MS >= TURN_TTL_MS) };
        // And it must dwarf the longest single wait, or a caller could outwait
        // its own connection.
        const { assert!(IDLE_TTL_MS > MAX_WAIT_MS) };
    }

    #[test]
    fn a_turn_row_expires_rather_than_waiting_to_be_swept() {
        // The window has to clear any transport retry by a wide margin (the gateway's
        // hedge fires in under a second) while staying short enough that an
        // unclosed session's rows are bounded by RATE rather than accumulating.
        const { assert!(TURN_TTL_MS > 60 * 1000, "must outlast any retry") };
        const {
            assert!(
                TURN_TTL_MS <= 24 * 60 * 60 * 1000,
                "must not be a de facto forever"
            )
        };
        // And it must outlast the longest wait a single prompt can take, or a turn
        // could expire while its own caller is still waiting for it.
        const { assert!(TURN_TTL_MS > MAX_WAIT_MS) };
    }
}
