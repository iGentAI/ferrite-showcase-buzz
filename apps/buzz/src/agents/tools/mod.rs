//! The coding tool surface + a [`CloudToolRegistry`] that dispatches the file
//! tools against the versioned [`Workspace`] over the injected `ctx.blob()` +
//! `ctx.store()` handles — plus the single-bundle seams: buzz workspace tools
//! call [`crate::relay::persona`] IN PROCESS, delegation re-enters the turn
//! pipeline, and the Fabric A2A tools ride the app's audited egress.
//!
//! Constrained-surface notes carried from the original port:
//! * **`regex` is a banned dependency** → `glob_to_regex` is replaced by a pure
//!   [`glob_match`] (same `*`/`**`/`?` semantics, no regex engine). `search_files`
//!   with `regex:true` returns an explicit "unavailable" error rather than
//!   silently faking regex.
//! * **`tokio::sync::Mutex` is unavailable** → the registry borrows the
//!   [`AppContext`] and builds a FRESH owned [`Workspace`] inside each `invoke`
//!   (a local `&mut`), so no interior mutability is needed.
//! * **No worker / workflows / control-plane siblings exist in this bundle**:
//!   `run_code` and pack-defined tools answer with an honest "not available on
//!   this deployment", and their schemas are never advertised.

use ferrite_app::serde_json;
use ferrite_app::serde_json::Value;
use ferrite_app::AppContext;

use crate::agents::agent_loop::ToolRegistry;
use crate::agents::apply_patch;
use crate::agents::error::{AgentError, Result};
use crate::agents::workspace::Workspace;

mod args;
mod matching;
mod memory;
mod schemas;
mod web;
pub use matching::glob_match;
pub use schemas::{
    ask_question_tool_schema, buzz_tool_schemas, coding_tool_schemas, delegate_tool_schema,
    fabric_a2a_tool_schemas, load_skill_tool_schema, mcp_tool_schemas, schedule_tool_schemas,
    web_tool_schemas, ToolSchema,
};
use web::WebBinding;
#[cfg(test)]
use web::cloudflare_crawl_status_url;
#[cfg(test)]
use web::WebConfigDoc;

mod buzz_tools;
mod schedule_tools;
mod mcp_tools;
mod fabric_tools;
mod memory_tools;
mod web_tools;


/// Cap a single tool result well under the engine's 64 KiB event bound.
pub const MAX_TOOL_OUTPUT: usize = 16 * 1024;

#[derive(Clone)]
struct LocalMemoryBinding {
    store_handle: String,
    tenant: String,
    user: String,
    space: String,
}

/// The agent definition's declared skills (docs/_archive/agent-pack-definition.md,
/// Phase 4): `load_skill` reads their documents from the config store.
#[derive(Clone)]
struct SkillsBinding {
    config_store_handle: String,
    agent: String,
    names: Vec<String>,
}

/// The parent's declared subagents (delegation). A `delegate` tool call
/// re-enters `run_delegated_turn` on a synthesized child definition in an
/// isolated child scope. Fresh per turn, so the fan-out counter resets each
/// turn.
///
/// Child token spend is NOT folded into the parent's `usage_handle`: that
/// handle holds the LAST model call's usage (overwritten per call), so poking
/// it mid-loop would both be clobbered by the next parent call and mis-charge
/// the parent's budget for a child's tokens. Each child is a full turn with
/// its own bounded budget.
struct SubagentsBinding {
    subagents: Vec<crate::agents::definition::SubagentSpec>,
    parent_agent: String,
    parent_thread: String,
    parent_msg_id: String,
    /// The parent's tool policy (approval gate, allow/deny, unavailable
    /// capabilities), propagated into every child so neither the HITL gate nor
    /// the parent's tool filter can be bypassed by delegating to a subagent.
    parent_policy: crate::agents::definition::InheritedToolPolicy,
    /// Executed delegations this turn; bounds fan-out at `MAX_DELEGATIONS_PER_TURN`.
    delegations: std::sync::atomic::AtomicUsize,
}

/// Derive a child turn's thread from its parent turn and task (FNV-1a-64, 16
/// hex). Deterministic for a fixed parent turn (survives replay), distinct
/// across turns (`parent_msg_id` folded in), distinct per task. Byte-identical
/// `(subagent, task)` within one parent turn collapse onto one child thread —
/// an intended cost guard.
fn delegate_thread_hash(parent_thread: &str, parent_msg_id: &str, task: &str) -> String {
    let sep = [0x1fu8];
    let mut hash = 0xcbf29ce484222325u64;
    for b in parent_thread
        .as_bytes()
        .iter()
        .chain(sep.iter())
        .chain(parent_msg_id.as_bytes().iter())
        .chain(sep.iter())
        .chain(task.as_bytes().iter())
    {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// The buzz workspace seam — in-process: a persona command is
/// a function call into [`crate::relay::persona::execute`], which runs the
/// SAME gates as any client write (registry, key check, membership,
/// moderation, rate limits, the guarded ingest saga). No queue, no polled
/// result store, no seam token: the workspace IS this application.
struct BuzzBinding {
    /// The persona (this turn's agent id) acting in the workspace. The relay
    /// refuses ids its registry does not name, so this is a claim the
    /// persona surface verifies, not an authority.
    agent: String,
    /// The turn's admission id, folded into every command's correlation id so
    /// a redelivered driver or a model retry replays onto the SAME durable
    /// command (the relay pins the event identity under it).
    msg_id: String,
    /// Accepted `buzz_post` writes made through this binding — the turn
    /// loop reads it after the turn to decide whether the persona already
    /// answered INTO the channel, or whether the final answer must be posted
    /// for it (a trigger is fire-and-forget, so an unposted answer is
    /// silence). A write whose outcome is UNKNOWN is never counted and never
    /// recorded: it fails the call (see [`CloudToolRegistry::invoke_buzz`]), so
    /// the turn stays unsettled and its replay repeats the identical write.
    posts_made: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// The driving turn's lease, when it has one: checked before every WRITE.
    /// BEST EFFORT: the check and the relay write are separate operations, so a
    /// driver that pauses between them can still write after a reclaim. What
    /// makes that harmless is the write's identity: every driver of a turn runs
    /// the batch the log recorded, so a late write is the same correlation-pinned
    /// write the relay absorbs, never a second message.
    fence: Option<crate::agents::turns::TurnFence>,
}

/// The community segment of the relay's correlation-keyed reservation rows.
/// Single-community, like the relay's own DEFAULT_COMMUNITY.
const BUZZ_COMMUNITY: &str = "main";

fn is_buzz_tool(tool: &str) -> bool {
    matches!(tool, "buzz_post" | "buzz_react" | "buzz_read")
}

/// What a recorded or returned `buzz_post` result establishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostRecord {
    /// Accepted (`ok: true`): the post is in the channel.
    Landed,
    /// A definitive refusal or an argument error: nothing was posted.
    NotPosted,
    /// `retryable: true`: the write may or may not have committed.
    Unknown,
}

pub(crate) fn post_record(result: &Value) -> PostRecord {
    let error_text = result.get("error").and_then(Value::as_str).is_some_and(|e| !e.is_empty());
    // `retryable: true` says the outcome is not settled, whatever `ok` claims.
    if result.get("retryable").and_then(Value::as_bool) == Some(true) {
        return PostRecord::Unknown;
    }
    match result.get("ok").and_then(Value::as_bool) {
        Some(true) => PostRecord::Landed,
        // An explicit relay refusal: `ok: false`, a stated error, and an explicit
        // `retryable: false` (records written before that field existed carried
        // storage failures in the same shape, so without it nothing is proven).
        Some(false)
            if error_text && result.get("retryable").and_then(Value::as_bool) == Some(false) =>
        {
            PostRecord::NotPosted
        }
        // The tool's own argument check (`{"error": "..."}`, nothing else): the
        // relay was never called.
        None if error_text && result.as_object().is_some_and(|o| o.len() == 1) => PostRecord::NotPosted,
        // The approval gate parked the call before the relay was ever reached.
        None if result.get("approval_required").and_then(Value::as_str).is_some() => PostRecord::NotPosted,
        // Anything else (null, {}, an unexpected envelope, `retryable: true`)
        // does not establish that nothing was written.
        _ => PostRecord::Unknown,
    }
}

/// The scheduling family. Separate from [`is_buzz_tool`] because these are not
/// persona OPS — nothing is signed and nothing is posted; a row is written and a
/// wake is queued.
fn is_schedule_tool(tool: &str) -> bool {
    matches!(tool, "buzz_schedule" | "buzz_schedules" | "buzz_unschedule")
}

/// The tool result for a failed schedule WRITE (`buzz_schedule`,
/// `buzz_unschedule`). A refusal (a bound, a permission, a missing row) is
/// definite: nothing changed. A storage, internal or conflict error can follow a
/// committed row or a published wake (create publishes first, cancel commits
/// before recalling receipts), so it is reported as UNCERTAIN: the model is told
/// to read `buzz_schedules` before retrying instead of being told the write
/// failed, which would invite a duplicate schedule or a false "not cancelled".
fn schedule_write_failure(tool: &str, error: &crate::relay::error::BuzzError) -> Value {
    use crate::relay::error::BuzzError;
    match error {
        BuzzError::Conflict(_) | BuzzError::Storage(_) | BuzzError::Internal(_) => {
            serde_json::json!({
                "ok": false,
                "outcome_unknown": true,
                "retryable": true,
                "error": error.to_string(),
                "note": format!(
                    "{tool} may or may not have taken effect; call buzz_schedules to see the \
                     current state before retrying"
                ),
            })
        }
        _ => serde_json::json!({
            "ok": false,
            "retryable": false,
            "error": error.to_string(),
        }),
    }
}

/// The MCP connector seam: the third-party MCP servers this workspace
/// has connected, reached through the app's own audited `mcp-egress` handle.
///
/// A MARKER rather than a configuration, because there is nothing to configure.
/// WHICH connectors exist is [`crate::mcp::catalog`]'s answer and WHETHER each is
/// authenticated is the credential store's, and both are read per call — so a
/// connector an operator authorizes mid-conversation becomes usable with nothing
/// restarted. All this records is that the deployment carries the egress handle,
/// which is the same condition that decides whether the schemas are advertised.
struct McpBinding;

fn is_mcp_tool(tool: &str) -> bool {
    matches!(tool, "mcp_connectors" | "mcp_list_tools" | "mcp_call_tool")
}

/// The Fabric A2A seam: this app speaks A2A JSON-RPC (`message/send`,
/// `tasks/get`) to ONE configured peer through an audited egress handle,
/// authorized by a per-app client token (`ctx.secret`, never env). The peer
/// runs the turn; this side only asks, polls, and reads the answer.
struct FabricA2ABinding {
    egress_handle: String,
    endpoint: String,
    client_token: String,
}

/// Poll cadence and budgets for a Fabric A2A turn — MEASURED against the live
/// sidecar, not guessed: the limiter is 15 rpc/s per protocol (breach ⇒ HTTP
/// 429), task status only advances when someone READS it (`tasks/get` runs
/// the refresh), and observed turn latency is 30-120 s with a hard 5-minute
/// give-up. The SSE stream endpoint is deliberately unused: the proxy in
/// front of the sidecar kills it at 15 s, so polling is the only whole path.
/// The synthetic outcome state for "still non-terminal at the wall budget".
/// Deliberately not an A2A spec state — the spec has no word for it, and the
/// caller must be able to tell it apart from a peer-reported `failed`.
pub(crate) const BUDGET_EXHAUSTED: &str = "__budget_exhausted";

const FABRIC_A2A_POLL_MS: u64 = 3_000;
pub(crate) const FABRIC_A2A_WALL_MS: u64 = 300_000;
/// A `message/send` into a context whose turn still runs fails -32603 "Turn
/// already in progress"; the sidecar's own retry window (~4 s) is far shorter
/// than a turn, so the send is re-tried here on a pause instead.
const FABRIC_A2A_SEND_RETRY_MS: u64 = 5_000;
/// Pause between attempts to record a started Fabric task's ownership.
const FABRIC_TASK_RECORD_RETRY_MS: u64 = 200;

/// `messageId` must be FRESH per send (the sidecar refuses -32602 without
/// one, and the A2A face has no admission dedup — uniqueness is the contract).
/// now_ms + a process-wide sequence + the scope hash keeps ids unique across
/// concurrent turns and across instances.
static FABRIC_A2A_MSG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn is_fabric_a2a_tool(tool: &str) -> bool {
    matches!(tool, "fabric_ask" | "fabric_ask_result")
}

/// The caller-chosen A2A `contextId` for one buzz thread. The sidecar pins
/// `(client token, contextId)` to exactly ONE Fabric thread and persists the
/// mapping, so the same key reaches the same conversation across restarts.
/// `generation > 1` names a FRESH context after a FAILED task — measured: a
/// failed context can surface the PREVIOUS message's answer, so it is never
/// reused (the turn pipeline owns the generation counter).
pub(crate) fn fabric_context_id(thread_key: &str, generation: u64) -> String {
    if generation <= 1 {
        format!("buzz:{BUZZ_COMMUNITY}:{thread_key}")
    } else {
        format!("buzz:{BUZZ_COMMUNITY}:{thread_key}:g{generation}")
    }
}

/// The Fabric thread key a model-chosen `thread_key` names inside one
/// conversation (`scope` is the registry's `{agent}:{thread}`). The `~`
/// separator never occurs in the turn pipeline's own `buzz-{channel}` keys,
/// so a model key can neither reach another conversation's thread nor the
/// pipeline's own context for a channel.
fn scoped_fabric_thread_key(scope: &str, key: &str) -> String {
    format!("tool~{scope}~{key}")
}

/// Longest Fabric task id this app records or accepts.
const MAX_FABRIC_TASK_ID_BYTES: usize = 256;

/// The store key recording that the conversation `scope` started Fabric task
/// `task_id`; `None` for an empty, oversized or control-character id.
fn fabric_task_key(scope: &str, task_id: &str) -> Option<Vec<u8>> {
    if task_id.is_empty()
        || task_id.len() > MAX_FABRIC_TASK_ID_BYTES
        || task_id.chars().any(char::is_control)
    {
        return None;
    }
    Some(format!("fabric-task\u{1f}{scope}\u{1f}{task_id}").into_bytes())
}

/// The store key recording that the conversation `scope` started browser crawl
/// job `job_id`; `None` for an empty, oversized or control-character id. The
/// crawl runs under the workspace's shared Cloudflare token, so a job id is
/// polled only by the conversation that started it.
fn crawl_job_key(scope: &str, job_id: &str) -> Option<Vec<u8>> {
    if job_id.is_empty()
        || job_id.len() > MAX_FABRIC_TASK_ID_BYTES
        || job_id.chars().any(char::is_control)
    {
        return None;
    }
    Some(format!("browser-crawl\u{1f}{scope}\u{1f}{job_id}").into_bytes())
}

/// A well-formed JSON stand-in for a tool result whose envelope could not be
/// shrunk under [`MAX_TOOL_OUTPUT`]: an error, the original size, and as
/// much of the serialized result as fits, cut on a char boundary.
fn oversize_tool_result(serialized: &str) -> Vec<u8> {
    let mut keep = serialized.len().min(MAX_TOOL_OUTPUT);
    loop {
        while !serialized.is_char_boundary(keep) {
            keep -= 1;
        }
        let out = serde_json::json!({
            "error": "tool result too large to return whole; the preview is its beginning",
            "bytes": serialized.len(),
            "truncated": true,
            "preview": &serialized[..keep],
        })
        .to_string();
        if out.len() <= MAX_TOOL_OUTPUT || keep == 0 {
            return out.into_bytes();
        }
        // Each dropped input byte removes at least one output byte, so one
        // or two rounds settle.
        keep = keep.saturating_sub((out.len() - MAX_TOOL_OUTPUT).max(1));
    }
}

/// Normalize both task-state vocabularies the sidecar emits: slash methods
/// (`tasks/get`) return v030 lowercase (`working`/`completed`), while the
/// REST face and the `GetTask` alias return `TASK_STATE_*` — accept both.
fn fabric_task_state(result: &Value) -> String {
    let raw = result
        .pointer("/status/state")
        .and_then(Value::as_str)
        .unwrap_or("");
    let raw = raw.strip_prefix("TASK_STATE_").unwrap_or(raw);
    raw.to_ascii_lowercase().replace('_', "-")
}

/// Byte bound on the peer answer a fabric tool result carries back to the
/// model. `ok` turns an over-cap envelope into the explicit oversize
/// stand-in, so clamping the text at the seam keeps a long peer answer
/// returnable as a success, with the marker next to the text it cut.
const FABRIC_RESULT_TEXT_MAX_BYTES: usize = 12 * 1024;

/// Clamp a peer answer on a char boundary with a visible marker — the tool
/// counterpart of the turn pipeline's `clamp_channel_reply`.
fn clamp_fabric_result_text(text: &str) -> String {
    if text.len() <= FABRIC_RESULT_TEXT_MAX_BYTES {
        return text.to_string();
    }
    let mut cut = FABRIC_RESULT_TEXT_MAX_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[truncated]", &text[..cut])
}

/// The finished answer of a completed task: prefer `status.message.parts`,
/// fall back to the LAST artifact's parts (both carried identical text in the
/// live proof, and each new turn RESETS the artifact list — which is why the
/// answer is read before the next send, never after).
fn fabric_final_text(result: &Value) -> Option<String> {
    fn parts_text(parts: &Value) -> Option<String> {
        let texts: Vec<&str> = parts
            .as_array()?
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect();
        if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n"))
        }
    }
    if let Some(text) = result.pointer("/status/message/parts").and_then(parts_text) {
        return Some(text);
    }
    result
        .get("artifacts")
        .and_then(Value::as_array)
        .and_then(|artifacts| artifacts.last())
        .and_then(|artifact| artifact.get("parts"))
        .and_then(parts_text)
}

/// The outcome of one driven Fabric A2A turn (the turn pipeline's harness
/// path): the terminal (or last-seen) state plus the answer text when the
/// task completed.
pub(crate) struct FabricTurnOutcome {
    pub(crate) task_id: String,
    pub(crate) state: String,
    pub(crate) text: Option<String>,
}

/// A [`ToolRegistry`] dispatching the coding file tools against a versioned
/// [`Workspace`] built fresh (from the injected context) inside each `invoke`.
pub struct CloudToolRegistry<'a> {
    ctx: &'a AppContext,
    blob_handle: String,
    store_handle: String,
    scope: String,
    local_memory: Option<LocalMemoryBinding>,
    web: Option<WebBinding>,
    skills: Option<SkillsBinding>,
    subagents: Option<SubagentsBinding>,
    buzz: Option<BuzzBinding>,
    fabric_a2a: Option<FabricA2ABinding>,
    mcp: Option<McpBinding>,
    /// The model boundary, carried so the `delegate` tool can re-enter the
    /// turn pipeline with the SAME transport the parent turn runs on.
    llm: Option<std::sync::Arc<dyn crate::agents::turns::LlmTransport>>,
    /// The member this turn acts for (64-hex), when one started it: persona
    /// reads are intersected with that member's access and connector calls
    /// need that member to be an operator. See `TurnRequest::requester`.
    requester: Option<String>,
}

impl<'a> CloudToolRegistry<'a> {
    pub fn new(
        ctx: &'a AppContext,
        blob_handle: impl Into<String>,
        store_handle: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            ctx,
            blob_handle: blob_handle.into(),
            store_handle: store_handle.into(),
            scope: scope.into(),
            local_memory: None,
            web: None,
            skills: None,
            subagents: None,
            buzz: None,
            fabric_a2a: None,
            mcp: None,
            llm: None,
            requester: None,
        }
    }

    /// Name the member this turn acts for (see the `requester` field).
    pub fn with_requester(mut self, requester: Option<&str>) -> Self {
        self.requester = requester.map(str::to_string);
        self
    }

    /// Wire the buzz workspace seam — in-process: the persona's
    /// SIGNING never happens here. The relay holds the key, executes the
    /// write through its own gates, and this binding only names WHO is
    /// acting (`agent`) and carries the turn's admission id (`msg_id`) so
    /// retried calls stay deterministic.
    pub fn with_buzz(mut self, agent: impl Into<String>, msg_id: impl Into<String>) -> Self {
        self.buzz = Some(BuzzBinding {
            agent: agent.into(),
            msg_id: msg_id.into(),
            posts_made: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            fence: None,
        });
        self
    }

    /// Fence the buzz seam's writes to a turn's lease (no-op without
    /// [`Self::with_buzz`]). A write attempted after the lease is lost fails as
    /// a plane error: the loop stops WITHOUT recording a result for the call, so
    /// the driver that now owns the turn makes that call itself on replay.
    pub(crate) fn with_turn_fence(mut self, fence: Option<crate::agents::turns::TurnFence>) -> Self {
        if let Some(buzz) = self.buzz.as_mut() {
            buzz.fence = fence;
        }
        self
    }

    /// The live counter of accepted `buzz_post` writes through this
    /// registry's buzz binding, cloneable BEFORE the registry is wrapped and
    /// borrowed by the tool loop (`None` when the seam is not wired).
    pub(crate) fn buzz_posts_handle(
        &self,
    ) -> Option<std::sync::Arc<std::sync::atomic::AtomicUsize>> {
        self.buzz
            .as_ref()
            .map(|binding| std::sync::Arc::clone(&binding.posts_made))
    }

    /// Wire the model boundary for delegation: the `delegate` tool re-enters
    /// the turn pipeline with this transport.
    pub fn with_llm(mut self, llm: std::sync::Arc<dyn crate::agents::turns::LlmTransport>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Wire the Fabric A2A seam: `fabric_ask`/`fabric_ask_result` become
    /// dispatchable against the ONE configured peer. The endpoint and client
    /// token arrive from the caller (manifest egress + `ctx.secret`) so this
    /// registry compiles no credential in; absent either, the seam is simply
    /// never wired and the tools answer with an honest "not configured".
    pub fn with_fabric_a2a(
        mut self,
        egress_handle: impl Into<String>,
        endpoint: impl Into<String>,
        client_token: impl Into<String>,
    ) -> Self {
        self.fabric_a2a = Some(FabricA2ABinding {
            egress_handle: egress_handle.into(),
            endpoint: endpoint.into(),
            client_token: client_token.into(),
        });
        self
    }

    /// Wire the agent definition's subagents (delegation). `delegate` becomes
    /// dispatchable; a call re-enters a child turn in an isolated scope.
    pub fn with_subagents(
        mut self,
        subagents: Vec<crate::agents::definition::SubagentSpec>,
        parent_agent: impl Into<String>,
        parent_thread: impl Into<String>,
        parent_msg_id: impl Into<String>,
        parent_policy: crate::agents::definition::InheritedToolPolicy,
    ) -> Self {
        self.subagents = Some(SubagentsBinding {
            subagents,
            parent_agent: parent_agent.into(),
            parent_thread: parent_thread.into(),
            parent_msg_id: parent_msg_id.into(),
            parent_policy,
            delegations: std::sync::atomic::AtomicUsize::new(0),
        });
        self
    }

    /// Wire the agent definition's skills (docs/_archive/agent-pack-definition.md,
    /// Phase 4): `load_skill` reads `agent-skill:v1:{agent}:{name}` documents
    /// from the given config store. `names` is the definition's declared
    /// list — only declared skills are loadable, so a stale store document
    /// for an undeclared skill is unreachable.
    pub fn with_skills(
        mut self,
        config_store_handle: impl Into<String>,
        agent: impl Into<String>,
        names: Vec<String>,
    ) -> Self {
        self.skills = Some(SkillsBinding {
            config_store_handle: config_store_handle.into(),
            agent: agent.into(),
            names,
        });
        self
    }

    pub fn with_local_memory(
        mut self,
        store_handle: impl Into<String>,
        tenant: impl Into<String>,
        user: impl Into<String>,
        space: impl Into<String>,
    ) -> Self {
        self.local_memory = Some(LocalMemoryBinding {
            store_handle: store_handle.into(),
            tenant: tenant.into(),
            user: user.into(),
            space: space.into(),
        });
        self
    }

    pub fn with_web(
        mut self,
        egress_handle: impl Into<String>,
        config_store_handle: impl Into<String>,
    ) -> Self {
        self.web = Some(WebBinding {
            egress_handle: egress_handle.into(),
            config_store_handle: config_store_handle.into(),
        });
        self
    }

    fn workspace(&self) -> Result<Workspace> {
        let blob = self.ctx.blob(&self.blob_handle)?;
        let store = self.ctx.store(&self.store_handle)?;
        Ok(Workspace::new(blob, store))
    }

    fn ok(v: Value) -> Result<Vec<u8>> {
        let s = v.to_string();
        if s.len() <= MAX_TOOL_OUTPUT {
            return Ok(s.into_bytes());
        }
        // Over the cap: the whole result becomes the explicit oversize
        // stand-in — an error, the original size, `truncated: true` and a
        // char-safe preview of its beginning — never a success envelope with
        // a value cut in place. Nothing here can prove a value is display
        // text: the MCP seam wraps whatever a connector returned, and CSV,
        // YAML, XML, base64 or an id under `content` or `body` look like any
        // other string. A value shortened inside an envelope that still says
        // ok would be read as complete; the stand-in says plainly that it is
        // not. Cutting the serialized bytes instead would record invalid
        // JSON, possibly invalid UTF-8, as the durable result.
        Ok(oversize_tool_result(&s))
    }

    fn err(msg: impl Into<String>) -> Result<Vec<u8>> {
        Self::ok(serde_json::json!({"error": msg.into()}))
    }

    /// Dispatch a `delegate` call: run one declared subagent as an isolated
    /// child turn and return its answer. ALWAYS `Ok` — every failure (bad
    /// args, unknown subagent, fan-out cap, child turn error) is a soft tool
    /// result, never a plane error or parent crash.
    ///
    /// Returns a CONCRETE boxed `Send` future rather than being an `async fn`.
    /// This is mandatory: `invoke` → `invoke_delegate` → `run_delegated_turn` →
    /// `run_tool_loop` → `FilteredTools::invoke` → `invoke` is a type-level
    /// async recursion cycle; an opaque `async fn` here makes rustc unable to
    /// resolve `Send` for the cycle. A declared `Box<dyn Future + Send>` erases
    /// the type and breaks the cycle.
    fn invoke_delegate<'s>(
        &'s self,
        args: &'s [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>>> + Send + 's>> {
        Box::pin(async move {
            let Some(binding) = &self.subagents else {
                return Self::err("delegation is not available");
            };
            let Some(llm) = &self.llm else {
                return Self::err("delegation is not available: no model boundary is wired");
            };
            let v: Value = serde_json::from_slice(args).unwrap_or_else(|_| serde_json::json!({}));
            let requested = v
                .get("subagent")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim();
            let task = v.get("task").and_then(|t| t.as_str()).unwrap_or("").trim();
            if requested.is_empty() || task.is_empty() {
                return Self::err("delegate requires a 'subagent' id and a non-empty 'task'");
            }
            // Fan-out cap (per parent turn; the registry — and this counter — is
            // fresh each turn). Count the attempt before resolving so an unknown
            // id cannot be retried past the cap.
            let n = binding
                .delegations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n >= crate::agents::definition::MAX_DELEGATIONS_PER_TURN {
                return Self::err(format!(
                    "delegation limit reached (max {} per turn)",
                    crate::agents::definition::MAX_DELEGATIONS_PER_TURN
                ));
            }
            let Some(spec) = binding.subagents.iter().find(|s| s.id == requested) else {
                return Self::err(format!("unknown subagent: {requested}"));
            };
            // Child scope. `{parent}.sub.{id}` is a legal Scope field (`.`
            // allowed) but NOT a valid registry agent id (no `.`), so it can
            // never be addressed as a persona — an isolated, non-addressable
            // namespace. Length ≤ 101 < MAX_SCOPE_FIELD_BYTES (128).
            let child_agent = format!("{}.sub.{}", binding.parent_agent, spec.id);
            let child_thread = format!(
                "d.{}",
                delegate_thread_hash(&binding.parent_thread, &binding.parent_msg_id, task)
            );
            // Deterministic child msg id → a redelivered/replayed parent turn
            // re-admits the same id (Duplicate) and the child runs exactly once.
            let child_msg_id = child_thread.clone();
            // Propagate the parent's approval gate and tool filter into the
            // child so a gated or filtered tool cannot be run by delegating it.
            let child_def = crate::agents::definition::synth_child_def(
                spec,
                child_agent.clone(),
                binding.parent_policy.clone(),
            );
            // RunAnyway keeps the replay shape: a replayed parent gets a FRESH
            // child answer (the durable child log stays single via admission
            // dedup). Skip would hand the parent a "duplicate admission"
            // placeholder instead of the child's answer.
            let r = crate::agents::turns::run_delegated_turn(
                self.ctx,
                llm,
                &child_agent,
                &child_thread,
                &child_msg_id,
                None,
                task.as_bytes(),
                child_def,
                crate::agents::turns::DuplicatePolicy::RunAnyway,
                // A delegated child acts for the same member as its parent.
                self.requester.as_deref(),
            )
            .await;
            Self::ok(serde_json::json!({
                "subagent": spec.id,
                "answer": String::from_utf8_lossy(&r.answer),
            }))
        })
    }

}

/// The memory tools the registry serves (and the older bare names), so an
/// unavailable `memory` capability withholds every one of them.
pub(crate) fn is_memory_tool(tool: &str) -> bool {
    matches!(
        tool,
        "remember"
            | "recall"
            | "remember_memory"
            | "get_memory"
            | "list_memory"
            | "correct_memory"
            | "supersede_memory"
            | "forget_memory"
            | "recall_memory"
    )
}

pub(crate) fn is_web_tool(tool: &str) -> bool {
    matches!(
        tool,
        "web_search"
            | "news_search"
            | "perplexity_ask"
            | "perplexity_search"
            | "browser_content"
            | "browser_markdown"
            | "browser_links"
            | "browser_scrape"
            | "browser_json"
            | "browser_snapshot"
            | "browser_screenshot"
            | "browser_pdf"
            | "browser_crawl"
            | "browser_crawl_status"
            | "browser_automate"
    )
}

fn perplexity_headers(key: &str) -> Vec<(String, String)> {
    vec![
        ("authorization".to_string(), format!("Bearer {key}")),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ]
}

fn excerpt_bytes(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(400)
        .collect::<String>()
}

impl ToolRegistry for CloudToolRegistry<'_> {
    /// A delegation settled from the log (recorded before a driver restart)
    /// consumes a fan-out slot exactly as an executed one did, so a fresh
    /// registry replaying the same parent turn cannot exceed
    /// `MAX_DELEGATIONS_PER_TURN`.
    fn note_recorded(&self, tool: &str) {
        if tool == crate::agents::definition::DELEGATE_TOOL {
            if let Some(binding) = &self.subagents {
                binding.delegations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// Reads, and writes whose identity is pinned so a repeat is absorbed (the
    /// buzz seam's correlation id). Unschedule is not (a schedule can be
    /// revived under the same id, and a repeated cancel would stop the new
    /// revision).
    /// Schedule creation (its due time is computed from the clock at each
    /// call, so a repeat is a different request and can create a second
    /// wake), whole-file writes (a repeat can overwrite a newer write),
    /// delegation (a repeat re-runs a child that may itself have side
    /// effects), edits, patches, memory mutations, connector (MCP) and peer
    /// (A2A) calls, crawls and browser automation are not: their interrupted
    /// calls are answered "outcome unknown" rather than repeated, and the
    /// model re-reads state.
    fn replay_safe(&self, tool: &str) -> bool {
        matches!(
            tool,
            "buzz_post"
                | "buzz_react"
                | "buzz_read"
                | "buzz_schedules"
                | "read_file"
                | "list_files"
                | "search_files"
                | "find_files"
                | "file_history"
                | "read_file_version"
                | "get_memory"
                | "list_memory"
                | "recall_memory"
                | "load_skill"
                | "end_turn"
                | "web_search"
                | "news_search"
                | "perplexity_ask"
                | "perplexity_search"
                | "browser_content"
                | "browser_markdown"
                | "browser_links"
                | "browser_scrape"
                | "browser_json"
                | "browser_snapshot"
                | "browser_screenshot"
                | "browser_pdf"
                | "browser_crawl_status"
        )
    }

    async fn invoke(&self, tool: &str, args: &[u8]) -> Result<Vec<u8>> {
        let v: Value = serde_json::from_slice(args).unwrap_or(serde_json::json!({}));
        // The workspace is opened only by the file tools that use it: web, MCP,
        // scheduling, memory and end_turn must not fail because a workspace
        // handle is missing.
        let scope = self.scope.clone();
        match tool {
            "list_files" => {
                let mut ws = self.workspace()?;
                let mut paths: Vec<String> =
                    ws.list(&scope).await?.into_iter().map(|(p, _)| p).collect();
                paths.sort();
                Self::ok(serde_json::json!({ "files": paths }))
            }
            "read_file" => {
                let path = v.get("path").and_then(|s| s.as_str()).unwrap_or("");
                if path.is_empty() {
                    return Self::err("missing 'path'");
                }
                let mut ws = self.workspace()?;
                // An invalid path the model chose is the model's error to fix,
                // not a plane fault that fails the recorded call and wedges the
                // turn; only a plane fault propagates.
                match ws.read(&scope, path).await {
                    Ok(Some(c)) => Self::ok(serde_json::json!({"path":path,"content":String::from_utf8_lossy(&c)})),
                    Ok(None) => Self::err(format!("file not found: {path}")),
                    Err(AgentError::RecordInvalid { reason }) => {
                        Self::err(format!("invalid path {path:?}: {reason}"))
                    }
                    Err(e) => Err(e),
                }
            }
            "write_file" => {
                let path = v.get("path").and_then(|s| s.as_str()).unwrap_or("");
                if path.is_empty() {
                    return Self::err("missing 'path'");
                }
                // REQUIRED, not defaulted: a missing or non-string content used to
                // overwrite the file with nothing. An explicit "" is still allowed.
                let Some(content) = v.get("content").and_then(Value::as_str) else {
                    return Self::err("write_file requires a string 'content' (use \"\" to empty a file)");
                };
                let mut ws = self.workspace()?;
                match ws.write(&scope, path, content.as_bytes()).await {
                    Ok(ver) => Self::ok(serde_json::json!({"ok":true,"path":path,"version":ver})),
                    Err(e) => Self::err(format!("write failed: {e}")),
                }
            }
            "edit_file" => {
                let path = v.get("path").and_then(|s| s.as_str()).unwrap_or("");
                let old = v.get("old_str").and_then(|s| s.as_str()).unwrap_or("");
                if path.is_empty() || old.is_empty() {
                    return Self::err("edit_file requires 'path' and non-empty 'old_str'");
                }
                // REQUIRED: treating a missing or non-string new_str as empty
                // would delete old_str.
                let Some(new) = v.get("new_str").and_then(Value::as_str) else {
                    return Self::err("edit_file requires a string 'new_str' (use \"\" to delete the match)");
                };
                let mut ws = self.workspace()?;
                // The version this edit is based on: the write below commits only
                // while it is still the head, so a concurrent edit that landed in
                // between is never silently overwritten.
                let base = match ws.head_version(&scope, path).await {
                    Ok(head) => head.unwrap_or(0),
                    Err(AgentError::RecordInvalid { reason }) => {
                        return Self::err(format!("invalid path {path:?}: {reason}"))
                    }
                    Err(e) => return Err(e),
                };
                let read = match ws.read_version(&scope, path, base).await {
                    Ok(read) => read,
                    Err(AgentError::RecordInvalid { reason }) => {
                        return Self::err(format!("invalid path {path:?}: {reason}"))
                    }
                    Err(e) => return Err(e),
                };
                let cur = match read {
                    // Text editing only: rewriting a non-UTF-8 file through a lossy
                    // conversion would silently replace bytes outside the match.
                    Some(c) if base > 0 => match String::from_utf8(c) {
                        Ok(text) => text,
                        Err(_) => {
                            return Self::err(format!(
                                "{path} is not UTF-8 text; edit_file only edits text files (use write_file to replace it whole)"
                            ))
                        }
                    },
                    _ => return Self::err(format!("file not found: {path}")),
                };
                let count = cur.matches(old).count();
                if count == 0 {
                    return Self::err(format!(
                        "old_str not found in {path}; read the file and copy an exact substring"
                    ));
                }
                if count > 1 {
                    return Self::err(format!(
                        "old_str appears {count} times in {path}; include more surrounding context to make it unique"
                    ));
                }
                let updated = cur.replacen(old, new, 1);
                match ws.write_if_head(&scope, path, updated.as_bytes(), base).await {
                    Ok(Ok(ver)) => Self::ok(serde_json::json!({"ok":true,"path":path,"version":ver})),
                    Ok(Err(current)) => Self::err(format!(
                        "{path} changed while it was being edited (now version {current}); read it again and redo the edit"
                    )),
                    Err(e) => Self::err(format!("write failed: {e}")),
                }
            }
            "search_files" => {
                let query = v.get("query").and_then(|s| s.as_str()).unwrap_or("");
                if query.is_empty() {
                    return Self::err("missing 'query'");
                }
                if v.get("regex").and_then(Value::as_bool).unwrap_or(false) {
                    return Self::err(
                        "regex content search is unavailable on the constrained runtime; use a literal query (and optionally a 'glob' to restrict files)",
                    );
                }
                let path_glob = v.get("glob").and_then(Value::as_str);
                let mut ws = self.workspace()?;
                let paths: Vec<String> =
                    ws.list(&scope).await?.into_iter().map(|(p, _)| p).collect();
                let mut matches = Vec::new();
                'outer: for p in paths {
                    if let Some(g) = path_glob {
                        if !glob_match(g, &p) {
                            continue;
                        }
                    }
                    if let Some(c) = ws.read(&scope, &p).await? {
                        let text = String::from_utf8_lossy(&c);
                        for (i, line) in text.lines().enumerate() {
                            if line.contains(query) {
                                matches.push(serde_json::json!({"path":p,"line":i+1,"text":line}));
                                if matches.len() >= 100 {
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
                Self::ok(serde_json::json!({ "matches": matches }))
            }
            "find_files" => {
                let glob = v.get("glob").and_then(|s| s.as_str()).unwrap_or("");
                if glob.is_empty() {
                    return Self::err("missing 'glob'");
                }
                let mut ws = self.workspace()?;
                let files: Vec<String> = ws
                    .list(&scope)
                    .await?
                    .into_iter()
                    .map(|(p, _)| p)
                    .filter(|p| glob_match(glob, p))
                    .collect();
                Self::ok(serde_json::json!({ "files": files }))
            }
            "file_history" => {
                let path = v.get("path").and_then(|s| s.as_str()).unwrap_or("");
                if path.is_empty() {
                    return Self::err("missing 'path'");
                }
                let mut ws = self.workspace()?;
                match ws.version_numbers(&scope, path).await {
                    Ok(versions) => Self::ok(
                        serde_json::json!({"path": path, "versions": versions, "count": versions.len()}),
                    ),
                    Err(e) => Self::err(format!("history failed: {e}")),
                }
            }
            "read_file_version" => {
                let path = v.get("path").and_then(|s| s.as_str()).unwrap_or("");
                let version = v.get("version").and_then(Value::as_u64).unwrap_or(0);
                if path.is_empty() {
                    return Self::err("missing 'path'");
                }
                if version == 0 {
                    return Self::err("missing positive integer 'version'");
                }
                let mut ws = self.workspace()?;
                match ws.read_version(&scope, path, version).await {
                    Ok(Some(c)) => Self::ok(serde_json::json!({
                        "path": path,
                        "version": version,
                        "content": String::from_utf8_lossy(&c)
                    })),
                    Ok(None) => Self::err(format!("no such version {version} of {path}")),
                    Err(e) => Self::err(format!("read version failed: {e}")),
                }
            }
            "apply_patch" => {
                let mut ws = self.workspace()?;
                let mut fs = apply_patch::WorkspaceFsRef::new(&mut ws, &scope);
                let out = apply_patch::run_apply_patch(&mut fs, args).await;
                // Bounded like every other result: a raw byte cut could split a
                // UTF-8 character or the envelope, with no notice that it did.
                if out.len() <= MAX_TOOL_OUTPUT {
                    Ok(out)
                } else {
                    match serde_json::from_slice::<Value>(&out) {
                        Ok(value) => Self::ok(value),
                        Err(_) => Self::ok(serde_json::json!({ "report": String::from_utf8_lossy(&out) })),
                    }
                }
            }
            "remember_memory" => {
                let text = v.get("text").and_then(Value::as_str).unwrap_or("").trim();
                if text.is_empty() {
                    return Self::err("missing 'text'");
                }
                let (category, entities) = memory::memory_labels(&v);
                match self.remember_local(text, category, entities).await? {
                    Some(local) => Self::ok(local),
                    None => Self::err("memory store is not bound"),
                }
            }
            "get_memory" => {
                let id = v.get("id").and_then(Value::as_str).unwrap_or("").trim();
                if id.is_empty() {
                    return Self::err("missing 'id'");
                }
                match self.get_local_memory(id).await? {
                    Some(local) => Self::ok(local),
                    None => Self::err("memory store is not bound"),
                }
            }
            "list_memory" => {
                let include_superseded = v
                    .get("include_superseded")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                match self.list_local_memory(include_superseded).await? {
                    Some(local) => Self::ok(local),
                    None => Self::err("memory store is not bound"),
                }
            }
            "correct_memory" => {
                let id = v.get("id").and_then(Value::as_str).unwrap_or("").trim();
                let text = v.get("text").and_then(Value::as_str).unwrap_or("").trim();
                if id.is_empty() {
                    return Self::err("missing 'id'");
                }
                if text.is_empty() {
                    return Self::err("missing 'text'");
                }
                match self.correct_local_memory(id, text).await? {
                    Some(local) => Self::ok(local),
                    None => Self::err("memory store is not bound"),
                }
            }
            "supersede_memory" => {
                let supersedes = v
                    .get("supersedes")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let text = v.get("text").and_then(Value::as_str).unwrap_or("").trim();
                if supersedes.is_empty() {
                    return Self::err("missing 'supersedes'");
                }
                if text.is_empty() {
                    return Self::err("missing 'text'");
                }
                let (category, entities) = memory::memory_labels(&v);
                match self
                    .supersede_local_memory(supersedes, text, category, entities)
                    .await?
                {
                    Some(local) => Self::ok(local),
                    None => Self::err("memory store is not bound"),
                }
            }
            "forget_memory" => {
                let id = v.get("id").and_then(Value::as_str).unwrap_or("").trim();
                if id.is_empty() {
                    return Self::err("missing 'id'");
                }
                match self.forget_local_memory(id).await? {
                    Some(local) => Self::ok(local),
                    None => Self::err("memory store is not bound"),
                }
            }
            "recall_memory" => {
                let query = v.get("query").and_then(Value::as_str).unwrap_or("").trim();
                if query.is_empty() {
                    return Self::err("missing 'query'");
                }
                let limit = v
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(8)
                    .clamp(1, 20);
                match self.recall_local(query, limit).await? {
                    Some(local) => Self::ok(local),
                    None => Self::err("memory store is not bound"),
                }
            }
            // No worker sibling exists in this bundle; the schema is not
            // advertised, and a model that calls it anyway gets the honest
            // refusal rather than a hang.
            "run_code" => Self::err("code execution is not available on this deployment"),
            "load_skill" => self.load_skill(args).await,
            t if t == crate::agents::definition::DELEGATE_TOOL && self.subagents.is_some() => {
                self.invoke_delegate(args).await
            }
            "end_turn" => Self::ok(serde_json::json!({"ok":true})),
            other if is_web_tool(other) => self.invoke_web_tool(other, args).await,
            other if is_buzz_tool(other) => self.invoke_buzz(other, args).await,
            other if is_schedule_tool(other) => self.invoke_schedule(other, args).await,
            other if is_fabric_a2a_tool(other) => self.invoke_fabric_a2a(other, args).await,
            other if is_mcp_tool(other) => self.invoke_mcp(other, args).await,
            other => Self::err(format!("unknown tool: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An invalid path the model chose is a soft tool error the model can fix.
    /// Propagated as an AgentError it failed the recorded call, and a fenced
    /// turn then redelivered the same bad call forever (finding_02f5287e27a382e8).
    #[tokio::test]
    async fn an_invalid_model_path_is_a_soft_error_for_read_and_edit() {
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let ctx = env.context();
        let registry =
            CloudToolRegistry::new(&ctx, "ws-content", "ws-index", "revka:buzz-a".to_string());
        for path in ["/README.md", "../foo", "   "] {
            let args = serde_json::json!({ "path": path }).to_string();
            let out = registry
                .invoke("read_file", args.as_bytes())
                .await
                .unwrap_or_else(|e| panic!("read_file {path:?} must be a soft error, got {e}"));
            let v: Value = serde_json::from_slice(&out).unwrap();
            assert!(v.get("error").is_some(), "read_file {path:?}: {v}");

            let args = serde_json::json!({ "path": path, "old_str": "a", "new_str": "b" }).to_string();
            let out = registry
                .invoke("edit_file", args.as_bytes())
                .await
                .unwrap_or_else(|e| panic!("edit_file {path:?} must be a soft error, got {e}"));
            let v: Value = serde_json::from_slice(&out).unwrap();
            assert!(v.get("error").is_some(), "edit_file {path:?}: {v}");
        }
    }

    /// A crawl job is polled only by the conversation that started it
    /// (finding_8c3def4d388bb240).
    #[tokio::test]
    async fn a_crawl_job_is_owned_by_the_conversation_that_started_it() {
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let ctx = env.context();
        let private =
            CloudToolRegistry::new(&ctx, "ws-content", "ws-index", "revka:buzz-private".to_string());
        let public =
            CloudToolRegistry::new(&ctx, "ws-content", "ws-index", "revka:buzz-public".to_string());
        private.record_crawl_job("job-123").await.unwrap();
        assert!(private.crawl_job_owned("job-123").await.is_ok());
        assert!(public.crawl_job_owned("job-123").await.is_err());
        assert!(private.crawl_job_owned("job-456").await.is_err());
        assert!(private.crawl_job_owned("").await.is_err());
    }

    #[test]
    fn coding_schemas_include_core_file_tools() {
        let names: Vec<String> = coding_tool_schemas().into_iter().map(|t| t.name).collect();
        for want in [
            "read_file",
            "write_file",
            "apply_patch",
            "find_files",
            "end_turn",
            "remember_memory",
            "recall_memory",
        ] {
            assert!(names.iter().any(|n| n == want), "{want} present: {names:?}");
        }
        // The unwired families are NOT advertised: no worker, no workflows,
        // no control plane, no web (web joins only when its egress exists).
        for gone in [
            "run_code",
            "workflow_start",
            "ferrite_provision",
            "web_search",
        ] {
            assert!(
                !names.iter().any(|n| n == gone),
                "{gone} must not be advertised by default: {names:?}"
            );
        }
    }

    /// A model-supplied crawl job id stays one segment under
    /// `browser-rendering/crawl/`: a `..` or a traversal cannot climb out of
    /// the crawl path while the request carries the Cloudflare token.
    #[test]
    fn a_crawl_job_id_cannot_climb_out_of_the_crawl_path() {
        let cfg = WebConfigDoc {
            cloudflare_account_id: Some("acct".into()),
            cloudflare_api_base: "https://api.cloudflare.com/client/v4".into(),
            ..WebConfigDoc::default()
        };
        assert_eq!(
            cloudflare_crawl_status_url(&cfg, "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0").unwrap(),
            "https://api.cloudflare.com/client/v4/accounts/acct/browser-rendering/crawl/0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0"
        );
        for hostile in [
            "..",
            ".",
            "",
            "../../../../user/tokens",
            "abc/def",
            "%2e%2e",
            "abc?limit=1",
            "abc#",
        ] {
            assert!(
                cloudflare_crawl_status_url(&cfg, hostile).is_err(),
                "job_id {hostile:?} must be refused"
            );
        }
        for dots in ["", ".", ".."] {
            assert!(web::encode_path_segment(dots).is_err(), "{dots:?}");
        }
        assert_eq!(web::encode_path_segment("a.b/c").unwrap(), "a.b%2Fc");
    }

    #[test]
    fn fabric_task_state_accepts_both_vocabularies() {
        // Slash methods (tasks/get) emit v030 lowercase…
        let v030 = serde_json::json!({ "status": { "state": "working" } });
        assert_eq!(fabric_task_state(&v030), "working");
        // …the REST face / GetTask alias emits TASK_STATE_* — both normalize
        // to the same lowercase-hyphen form, including the underscore states.
        let rest = serde_json::json!({ "status": { "state": "TASK_STATE_COMPLETED" } });
        assert_eq!(fabric_task_state(&rest), "completed");
        let input = serde_json::json!({ "status": { "state": "TASK_STATE_INPUT_REQUIRED" } });
        assert_eq!(fabric_task_state(&input), "input-required");
        // A shape with no state normalizes to empty (still-working per poll loop).
        assert_eq!(fabric_task_state(&serde_json::json!({})), "");
    }

    #[test]
    fn fabric_final_text_prefers_status_message_then_last_artifact() {
        // status.message wins when present.
        let both = serde_json::json!({
            "status": { "message": { "parts": [{ "kind": "text", "text": "from status" }] } },
            "artifacts": [{ "parts": [{ "kind": "text", "text": "from artifact" }] }],
        });
        assert_eq!(fabric_final_text(&both).as_deref(), Some("from status"));
        // Fallback: the LAST artifact (each new turn resets the list, so the
        // last one is the current turn's answer).
        let artifacts_only = serde_json::json!({
            "artifacts": [
                { "parts": [{ "kind": "text", "text": "stale" }] },
                { "parts": [{ "kind": "text", "text": "current" }] },
            ],
        });
        assert_eq!(
            fabric_final_text(&artifacts_only).as_deref(),
            Some("current")
        );
        // Multiple text parts join; non-text parts are skipped, not errors.
        let multi = serde_json::json!({
            "status": { "message": { "parts": [
                { "kind": "text", "text": "a" },
                { "kind": "file", "uri": "ignored" },
                { "kind": "text", "text": "b" },
            ] } },
        });
        assert_eq!(fabric_final_text(&multi).as_deref(), Some("a\nb"));
        assert_eq!(fabric_final_text(&serde_json::json!({})), None);
    }

    #[test]
    fn fabric_result_text_clamp_is_char_safe_and_marked() {
        // Under the bound: byte-identical.
        assert_eq!(clamp_fabric_result_text("short"), "short");
        // Over it: cut on a char boundary with the visible marker — never the
        // raw byte truncate that leaves invalid UTF-8 inside the JSON.
        let big = "\u{e9}".repeat(FABRIC_RESULT_TEXT_MAX_BYTES);
        let clamped = clamp_fabric_result_text(&big);
        assert!(clamped.ends_with("\n[truncated]"));
        assert!(clamped.len() <= FABRIC_RESULT_TEXT_MAX_BYTES + "\n[truncated]".len());
        // The clamped text plus its envelope stays inside MAX_TOOL_OUTPUT, so
        // `Tools::ok` passes it through untouched.
        let envelope = serde_json::json!({ "ok": true, "task_id": "t", "state": "completed", "text": clamped });
        assert!(envelope.to_string().len() <= MAX_TOOL_OUTPUT);
    }

    #[test]
    fn ok_bounds_an_oversized_mcp_result_as_an_explicit_char_safe_stand_in() {
        // The MCP seam wraps arbitrary connector output in exactly this
        // envelope (`invoke_mcp` → `Self::ok(serde_json::json!({"ok": true, "result": .. }))`),
        // so this is the least-bounded input `ok` ever sees. Multi-byte on
        // purpose: a raw byte cut would land mid-UTF-8.
        let big = format!("{}\u{e9}", "\u{e9} ".repeat(8 * 1024)); // ~24 KiB
        let out = CloudToolRegistry::ok(serde_json::json!({
            "ok": true,
            "result": { "content": [{ "type": "text", "text": big }] },
        }))
        .expect("ok never fails");
        assert!(out.len() <= MAX_TOOL_OUTPUT, "bounded: {}", out.len());
        let text = String::from_utf8(out).expect("valid UTF-8");
        let v: Value = serde_json::from_str(&text).expect("valid JSON");
        // Never a success carrying a shortened value: an explicit error with
        // the original size and a preview of the beginning.
        assert!(v.get("ok").is_none() && v.get("result").is_none(), "{v}");
        assert_eq!(v["truncated"], serde_json::json!(true));
        assert!(v["error"].as_str().is_some_and(|e| e.contains("too large")));
        assert!(v["bytes"].as_u64().is_some_and(|b| b as usize > MAX_TOOL_OUTPUT));
        assert!(v["preview"].as_str().is_some_and(|p| p.starts_with("{\"ok\":true")));
    }

    #[test]
    fn ok_never_cuts_an_oversized_opaque_value_into_a_plausible_fragment() {
        // An encoded screenshot: a head-and-tail of it would decode to garbage
        // inside an envelope that still says ok. It becomes the explicit
        // oversize stand-in instead.
        let base64 = "QUJD".repeat(8 * 1024);
        let out = CloudToolRegistry::ok(serde_json::json!({ "ok": true, "png_base64": base64 }))
            .expect("ok never fails");
        assert!(out.len() <= MAX_TOOL_OUTPUT);
        let v: Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(v["truncated"], serde_json::json!(true));
        assert!(v.get("png_base64").is_none(), "no shortened payload under its own key: {v}");
    }

    /// Review findings: a prose KEY does not make a value prose, and no
    /// heuristic on the value proves it either. Line-wrapped base64, a data
    /// URL, embedded JSON, CSV, YAML, XML and plain prose under
    /// `content`/`body`/`output` all become the explicit stand-in; none is
    /// cut in place under a success envelope.
    #[test]
    fn ok_never_cuts_machine_data_that_sits_under_a_prose_key() {
        let wrapped: String = (0..600).map(|_| "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo0NTY3ODk=\n").collect();
        let data_url = format!("data:image/png;base64,{}", "QUJD".repeat(8 * 1024));
        let json_text = serde_json::to_string(&serde_json::json!({
            "rows": (0..2_000).map(|i| serde_json::json!({ "id": i, "name": format!("row {i}") })).collect::<Vec<_>>(),
        }))
        .unwrap();
        let csv: String = (0..2_000).map(|i| format!("{i}, Ada Lovelace, London office, {i}.50\n")).collect();
        let yaml: String = (0..2_000).map(|i| format!("- id: {i}\n  name: item number {i}\n")).collect();
        let xml: String = (0..2_000).map(|i| format!("<row id=\"{i}\" name=\"item {i}\" />\n")).collect();
        let prose = "the quick brown fox jumps over the lazy dog ".repeat(1_000);
        for (key, value) in [
            ("content", wrapped),
            ("body", data_url),
            ("output", json_text),
            ("content", csv),
            ("body", yaml),
            ("output", xml),
            ("text", prose),
        ] {
            let out = CloudToolRegistry::ok(serde_json::json!({ "ok": true, key: value }))
                .expect("ok never fails");
            assert!(out.len() <= MAX_TOOL_OUTPUT, "{key}: bounded");
            let v: Value = serde_json::from_slice(&out).expect("valid JSON");
            assert_eq!(v["truncated"], serde_json::json!(true), "{key}");
            assert!(v.get(key).is_none(), "{key}: no shortened payload under its own key: {v}");
            assert!(v.get("preview").is_some(), "{key}: the explicit stand-in: {v}");
            assert!(v.get("ok").is_none(), "{key}: never reported as a success: {v}");
        }
    }

    /// Review finding: an envelope slightly over the cap had its largest string
    /// shortened whatever it was, so a path or id came back cut inside a
    /// success. Machine-readable values are never shortened.
    #[test]
    fn ok_never_shortens_a_machine_readable_value() {
        let path = format!("dir/{}.txt", "p".repeat(MAX_TOOL_OUTPUT / 2));
        let id = "i".repeat(MAX_TOOL_OUTPUT / 2 + 64);
        let out = CloudToolRegistry::ok(serde_json::json!({ "ok": true, "path": path, "task_id": id }))
            .expect("ok never fails");
        assert!(out.len() <= MAX_TOOL_OUTPUT);
        let v: Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(v["truncated"], serde_json::json!(true));
        assert!(v.get("path").is_none() && v.get("task_id").is_none(), "no cut value under its own key: {v}");
        assert!(v.get("preview").is_some(), "the explicit stand-in: {v}");
    }

    #[test]
    fn ok_leaves_a_fitting_envelope_untouched() {
        let v = serde_json::json!({ "ok": true, "result": "small" });
        let out = CloudToolRegistry::ok(v.clone()).expect("ok never fails");
        assert_eq!(out, v.to_string().into_bytes());
    }

    #[test]
    fn fabric_context_id_is_stable_per_thread_and_fresh_per_generation() {
        // Generation 1 is the plain durable key — the one the sidecar pins to
        // a single Fabric thread across restarts.
        assert_eq!(
            fabric_context_id("buzz-general", 1),
            "buzz:main:buzz-general"
        );
        assert_eq!(
            fabric_context_id("buzz-general", 0),
            "buzz:main:buzz-general"
        );
        // A bumped generation NEVER collides with the failed context.
        assert_eq!(
            fabric_context_id("buzz-general", 2),
            "buzz:main:buzz-general:g2"
        );
    }

    /// Review finding F058: a model-chosen `thread_key` mapped straight onto
    /// the workspace-wide Fabric context, so `buzz-<other channel>` reached
    /// that channel's conversation.
    #[test]
    fn a_model_thread_key_stays_inside_its_own_conversation() {
        let mine = fabric_context_id(&scoped_fabric_thread_key("revka:buzz-a", "buzz-b"), 1);
        let theirs = fabric_context_id(&scoped_fabric_thread_key("revka:buzz-b", "buzz-b"), 1);
        assert_ne!(mine, fabric_context_id("buzz-b", 1), "cannot reach the pipeline's channel context");
        assert_ne!(mine, theirs, "distinct conversations never share a Fabric thread");
        assert_eq!(
            mine,
            fabric_context_id(&scoped_fabric_thread_key("revka:buzz-a", "buzz-b"), 1),
            "the same key in the same conversation is stable"
        );
    }

    /// Review finding F058: `fabric_ask_result` polled any task id, so one
    /// conversation could read the answer to another's Fabric task.
    #[tokio::test]
    async fn fabric_task_ids_are_pollable_only_from_the_conversation_that_started_them() {
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let ctx = env.context();
        let registry = |scope: &str| {
            CloudToolRegistry::new(&ctx, "ws-content", "ws-index", scope.to_string())
                .with_fabric_a2a("fabric-a2a-egress", "https://peer.invalid/a2a", "token")
        };
        let a = registry("revka:buzz-a");
        let b = registry("revka:buzz-b");
        a.record_fabric_task("task-123").await.expect("recorded");
        assert!(a.fabric_task_owned("task-123").await.is_ok());
        assert!(b.fabric_task_owned("task-123").await.is_err());

        // The refusal is answered before anything reaches the peer.
        for (tool, args) in [
            ("fabric_ask_result", r#"{"task_id":"task-123"}"#),
            ("fabric_ask", r#"{"prompt":"recap","task_id":"task-123"}"#),
        ] {
            let out = String::from_utf8(b.invoke(tool, args.as_bytes()).await.unwrap()).unwrap();
            assert!(out.contains("unknown task_id"), "{tool}: {out}");
        }
        assert!(fabric_task_key("s", "").is_none());
        assert!(fabric_task_key("s", &"x".repeat(MAX_FABRIC_TASK_ID_BYTES + 1)).is_none());
        assert!(fabric_task_key("s", "a\u{1f}b").is_none());
    }

    /// Review finding: an envelope made of MANY short machine values under a
    /// prose key had one of them cut while it still said ok.
    #[test]
    fn many_short_machine_values_under_a_prose_key_are_never_cut() {
        let chunk = "QUJD".repeat(200); // 800 bytes of base64
        let url = format!("data:image/png;base64,{}", "QUJD".repeat(150));
        let json_item = serde_json::to_string(&serde_json::json!({ "id": "x".repeat(700) })).unwrap();
        for item in [chunk, url, json_item] {
            let items: Vec<Value> = (0..80).map(|_| Value::String(item.clone())).collect();
            let out = CloudToolRegistry::ok(serde_json::json!({ "ok": true, "content": items }))
                .expect("ok never fails");
            assert!(out.len() <= MAX_TOOL_OUTPUT);
            let v: Value = serde_json::from_slice(&out).expect("valid JSON");
            assert_eq!(v["truncated"], serde_json::json!(true));
            assert!(v.get("content").is_none(), "no cut item under its own key: {v}");
            assert!(v.get("preview").is_some(), "the explicit stand-in: {v}");
        }
    }

    /// Review finding F228: an envelope oversized by many short strings fell
    /// through to a raw byte cut — invalid JSON, possibly invalid UTF-8 —
    /// recorded as the durable tool result.
    #[test]
    fn many_small_strings_still_produce_valid_bounded_json() {
        for row in ["row-ééé-", "r", "\"q\""] {
            let rows: Vec<Value> = (0..6000)
                .map(|i| Value::String(format!("{row}{i}")))
                .collect();
            let out = CloudToolRegistry::ok(serde_json::json!({ "rows": rows })).unwrap();
            assert!(out.len() <= MAX_TOOL_OUTPUT, "{} bytes", out.len());
            let text = std::str::from_utf8(&out).expect("valid UTF-8");
            let v: Value = serde_json::from_str(text).expect("valid JSON");
            assert_eq!(v["truncated"], serde_json::json!(true));
            assert!(v["preview"].as_str().is_some_and(|p| p.starts_with("{\"rows\"")));
        }
    }

    #[test]
    fn fabric_a2a_schemas_declare_the_two_tools() {
        let names: Vec<String> = fabric_a2a_tool_schemas()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["fabric_ask", "fabric_ask_result"]);
        assert!(is_fabric_a2a_tool("fabric_ask"));
        assert!(is_fabric_a2a_tool("fabric_ask_result"));
        assert!(!is_fabric_a2a_tool("fabric_poll"));
    }

    #[test]
    fn a_post_result_says_landed_not_posted_or_unknown() {
        let record = |v: Value| post_record(&v);
        assert_eq!(record(serde_json::json!({ "ok": true, "id": "e1" })), PostRecord::Landed);
        // A storage/internal/conflict failure can follow a committed write.
        assert_eq!(
            record(serde_json::json!({ "ok": false, "error": "storage", "retryable": true })),
            PostRecord::Unknown
        );
        // Only a definitive refusal says nothing was posted.
        assert_eq!(
            record(serde_json::json!({ "ok": false, "error": "not a member", "retryable": false })),
            PostRecord::NotPosted
        );
        assert_eq!(
            record(serde_json::json!({ "ok": false, "error": "refused" })),
            PostRecord::Unknown,
            "a negative record without retryable:false predates the field and proves nothing"
        );
        assert_eq!(record(serde_json::json!({ "error": "buzz_post requires 'channel'" })), PostRecord::NotPosted);
        assert_eq!(
            record(serde_json::json!({ "approval_required": "buzz_post", "note": "needs approval" })),
            PostRecord::NotPosted,
            "a parked gated call never reached the relay"
        );
        for unknown in [
            serde_json::json!(null),
            serde_json::json!({}),
            serde_json::json!({ "ok": false }),
            serde_json::json!({ "ok": false, "error": null }),
            serde_json::json!({ "status": "queued" }),
            serde_json::json!({ "error": "x", "id": "e1" }),
        ] {
            assert_eq!(record(unknown.clone()), PostRecord::Unknown, "{unknown}");
        }
    }

    #[test]
    fn buzz_tools_are_recognized() {
        assert!(is_buzz_tool("buzz_post"));
        assert!(is_buzz_tool("buzz_react"));
        assert!(is_buzz_tool("buzz_read"));
        assert!(!is_buzz_tool("buzz_delete"));
        let names: Vec<String> = buzz_tool_schemas().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["buzz_post", "buzz_react", "buzz_read"]);
    }

    /// Review finding: a storage/internal/conflict error from a schedule write
    /// was reported as a definite failure, though create publishes its wake
    /// first and cancel commits before recalling receipts.
    #[test]
    fn schedule_write_plane_errors_are_uncertain_and_refusals_definite() {
        use crate::relay::error::BuzzError;
        for e in [
            BuzzError::Storage("put failed".into()),
            BuzzError::Internal("encode".into()),
            BuzzError::Conflict("changed".into()),
        ] {
            for tool in ["buzz_schedule", "buzz_unschedule"] {
                let v = schedule_write_failure(tool, &e);
                assert_eq!(v["outcome_unknown"], true, "{tool} {e:?}: {v}");
                assert_eq!(v["retryable"], true, "{v}");
                assert!(v["note"].as_str().unwrap().contains("buzz_schedules"), "{v}");
            }
        }
        for e in [
            BuzzError::InvalidEvent("interval_ms must be at least 60000".into()),
            BuzzError::Unauthorized("not yours".into()),
            BuzzError::NotFound("no such schedule".into()),
        ] {
            let v = schedule_write_failure("buzz_schedule", &e);
            assert_eq!(v["retryable"], false, "{v}");
            assert!(v.get("outcome_unknown").is_none(), "{v}");
        }
    }

    /// Review finding: the delegation cap reset when an interrupted parent turn
    /// was replayed by a fresh registry. A parent turn delegates up to the cap,
    /// its driver stops before the turn finishes, and a FRESH registry replays
    /// it through the real tool loop: the delegations settled from the log
    /// consume the cap, so the replay's next delegation is refused before any
    /// subagent is resolved or launched.
    #[tokio::test]
    async fn a_replayed_parent_turn_cannot_delegate_past_the_fan_out_cap() {
        use crate::agents::agent_loop::{
            run_tool_loop, ModelClient, ModelStep, StepToolCall, ToolRegistry,
        };
        use crate::agents::definition::{DELEGATE_TOOL, MAX_DELEGATIONS_PER_TURN};
        use crate::agents::error::{AgentError, EventDoc, Scope};
        use crate::agents::session::{testkit::MemApe, SessionEngine};
        use std::future::Future;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Counts model calls: no child may be launched here, so none happen.
        #[derive(Default)]
        struct NoModel {
            calls: AtomicUsize,
        }
        impl crate::agents::turns::LlmTransport for NoModel {
            fn chat<'a>(
                &'a self,
                _ctx: &'a AppContext,
                _request: crate::llm::ChatRequest,
            ) -> std::pin::Pin<
                Box<
                    dyn Future<Output = std::result::Result<crate::llm::ChatReply, String>>
                        + Send
                        + 'a,
                >,
            > {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Err("no model call is expected".to_string()) })
            }
            fn model<'a>(
                &'a self,
                _ctx: &'a AppContext,
                _model: &'a str,
            ) -> std::pin::Pin<
                Box<
                    dyn Future<Output = std::result::Result<crate::llm::ModelReply, String>>
                        + Send
                        + 'a,
                >,
            > {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Err("no model call is expected".to_string()) })
            }
        }

        struct Script {
            steps: Vec<ModelStep>,
            at: AtomicUsize,
        }
        impl ModelClient for Script {
            fn step(
                &self,
                _history: &[EventDoc],
            ) -> impl Future<Output = Result<ModelStep>> + Send {
                let i = self
                    .at
                    .fetch_add(1, Ordering::SeqCst)
                    .min(self.steps.len() - 1);
                let step = self.steps[i].clone();
                async move { Ok(step) }
            }
        }

        /// The real registry, whose tool plane fails the first call of
        /// `fail`: the driver stops there, as a crashed one would.
        struct FailingOnce<'r, 'c> {
            inner: &'r CloudToolRegistry<'c>,
            fail: std::sync::Mutex<Option<&'static str>>,
        }
        impl ToolRegistry for FailingOnce<'_, '_> {
            fn invoke(
                &self,
                tool: &str,
                args: &[u8],
            ) -> impl Future<Output = Result<Vec<u8>>> + Send {
                let fail = {
                    let mut fail = self.fail.lock().unwrap();
                    if *fail == Some(tool) {
                        fail.take();
                        true
                    } else {
                        false
                    }
                };
                let inner = self.inner.invoke(tool, args);
                async move {
                    if fail {
                        Err(AgentError::Plane {
                            message: "tool plane down".into(),
                        })
                    } else {
                        inner.await
                    }
                }
            }
            fn replay_safe(&self, tool: &str) -> bool {
                self.inner.replay_safe(tool)
            }
            fn note_recorded(&self, tool: &str) {
                self.inner.note_recorded(tool)
            }
        }

        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let ctx = env.context();
        let llm = std::sync::Arc::new(NoModel::default());
        let registry = || {
            CloudToolRegistry::new(&ctx, "ws-content", "ws-index", "revka:buzz-a".to_string())
                .with_llm(llm.clone())
                .with_subagents(
                    Vec::new(),
                    "parent",
                    "thread",
                    "msg",
                    crate::agents::definition::InheritedToolPolicy::default(),
                )
        };
        let delegate = |i: usize| StepToolCall {
            id: format!("d{i}"),
            tool: DELEGATE_TOOL.into(),
            args: br#"{"subagent":"ghost","task":"look"}"#.to_vec(),
        };
        let mut eng = SessionEngine::new(MemApe::default());
        let sc = Scope::new("t", "a", "th").unwrap();
        eng.open(&sc).await.unwrap();
        eng.admit_message(&sc, "t1", b"hi").await.unwrap();

        // The first driver delegates up to the cap, then stops.
        let mut first_batch: Vec<StepToolCall> =
            (0..MAX_DELEGATIONS_PER_TURN).map(delegate).collect();
        first_batch.push(StepToolCall {
            id: "boom".into(),
            tool: "read_file".into(),
            args: b"{}".to_vec(),
        });
        let first_registry = registry();
        let first = FailingOnce {
            inner: &first_registry,
            fail: std::sync::Mutex::new(Some("read_file")),
        };
        let model = Script {
            steps: vec![ModelStep::ToolCalls {
                text: String::new(),
                calls: first_batch,
            }],
            at: AtomicUsize::new(0),
        };
        assert!(run_tool_loop(&mut eng, &sc, &model, &first, "t1", 8)
            .await
            .is_err());

        // A fresh registry replays the turn; the model now delegates once more.
        let replay_registry = registry();
        let replay = FailingOnce {
            inner: &replay_registry,
            fail: std::sync::Mutex::new(None),
        };
        let model = Script {
            steps: vec![
                ModelStep::ToolCalls {
                    text: String::new(),
                    calls: vec![delegate(99)],
                },
                ModelStep::Final {
                    text: "done".into(),
                },
            ],
            at: AtomicUsize::new(0),
        };
        assert_eq!(
            run_tool_loop(&mut eng, &sc, &model, &replay, "t1", 8)
                .await
                .unwrap(),
            "done"
        );

        let snap = eng.get(&sc).await.unwrap().unwrap();
        let results: Vec<String> = eng
            .read_events(&sc, 1, snap.seq)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.kind == crate::agents::conv::TOOL_RESULT_EVENT_KIND)
            .map(|e| String::from_utf8_lossy(&e.data).into_owned())
            .collect();
        let delegated: Vec<&String> = results
            .iter()
            .filter(|r| r.contains("unknown subagent: ghost"))
            .collect();
        assert_eq!(
            delegated.len(),
            MAX_DELEGATIONS_PER_TURN,
            "the first driver's delegations ran: {results:?}"
        );
        assert!(
            results
                .last()
                .is_some_and(|r| r.contains("delegation limit reached")),
            "the replayed turn's next delegation is refused: {results:?}"
        );
        assert_eq!(
            llm.calls.load(Ordering::SeqCst),
            0,
            "no child turn was launched"
        );
    }

    #[test]
    fn schedule_tools_are_recognized_and_declared_together() {
        assert!(is_schedule_tool("buzz_schedule"));
        assert!(is_schedule_tool("buzz_schedules"));
        assert!(is_schedule_tool("buzz_unschedule"));
        // NOT a persona op: nothing here is signed or posted.
        assert!(!is_buzz_tool("buzz_schedule"));
        let names: Vec<String> = schedule_tool_schemas()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(
            names,
            vec!["buzz_schedule", "buzz_schedules", "buzz_unschedule"]
        );
        // Declared and routed must agree — a tool in one and not the other is
        // either invisible or a trap.
        for name in &names {
            assert!(is_schedule_tool(name), "{name} is declared but not routed");
        }
        // The bounds a model needs are in the DESCRIPTION, because a refusal it can
        // predict is one it does not spend a step discovering.
        let schedule = schedule_tool_schemas()
            .into_iter()
            .find(|t| t.name == "buzz_schedule")
            .expect("declared");
        assert!(
            schedule.description.contains("60000"),
            "the interval floor must be stated: {}",
            schedule.description
        );
        assert!(
            schedule.description.contains("1000"),
            "the occurrence cap must be stated: {}",
            schedule.description
        );
    }

    #[test]
    fn mcp_tools_are_recognized_and_not_advertised_by_default() {
        assert!(is_mcp_tool("mcp_connectors"));
        assert!(is_mcp_tool("mcp_list_tools"));
        assert!(is_mcp_tool("mcp_call_tool"));
        assert!(
            !is_mcp_tool("mcp_authorize"),
            "connecting is not an agent's to do"
        );
        let names: Vec<String> = mcp_tool_schemas().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec!["mcp_connectors", "mcp_list_tools", "mcp_call_tool"]
        );
        // Every declared tool is dispatchable, and every dispatchable one is
        // declared — a tool in one list and not the other is either invisible or
        // a trap.
        for name in &names {
            assert!(is_mcp_tool(name), "{name} is declared but not routed");
        }
        // NOT in the default surface: the schemas join only where the egress
        // handle exists (the web-tool rule).
        let default_names: Vec<String> =
            coding_tool_schemas().into_iter().map(|t| t.name).collect();
        for name in &names {
            assert!(
                !default_names.contains(name),
                "{name} must not be advertised without its egress handle"
            );
        }
    }
}
