//! Agent routes: state, memory, config, the cloud-agent roster and its creation
//! policy.

use ferrite_app::tracing;
use ferrite_app::serde_json;
use ferrite_app::serde_json::Value;
use ferrite_app::AppContext;
use ferrite_app::IngressRequest;
use crate::relay::auth;
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::store;
use crate::relay::BuzzError;
use crate::relay::Result;
use crate::relay::DEFAULT_COMMUNITY;
use super::{CLAIM_RETRIES, authenticate_request, enforce_write_gate, header, storage};
use super::boot::{admit_persona_to_community};
use super::events::{ingest_event};
use super::channels::{is_community_moderator};
use super::identity::{relay_identity};

/// Whether a persona is PAUSED — registered, but not woken by a mention.
///
/// Kept out of the registry document on purpose: that document is one CAS point
/// for the whole roster, so pausing an agent through it would contend with every
/// concurrent creation. A per-agent row is a point write and a point read.
pub(super) async fn agent_is_paused(
    config: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    agent: &str,
) -> bool {
    matches!(
        store::get_raw(config, &keys::agent_paused(community, agent)).await,
        Ok(Some(_))
    )
}

/// `POST /buzz/api/agents/active` — pause or resume a persona.
///
/// Retiring an agent removes it; pausing keeps its identity, its definition and
/// its history while stopping it answering. Authorized like every other change to
/// an agent: its creator, a community moderator, or an operator.
pub(super) async fn set_agent_active(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("agent body: {e}")))?;
    let agent = body
        .get("agent")
        .and_then(Value::as_str)
        .and_then(clean_agent_id)
        .ok_or_else(|| BuzzError::InvalidEvent("this needs an 'agent' id".into()))?;
    let active = body
        .get("active")
        .and_then(Value::as_bool)
        .ok_or_else(|| BuzzError::InvalidEvent("this needs an 'active' boolean".into()))?;
    if !may_manage_agent(ctx, &mut config, community, &agent, &caller.pubkey).await? {
        return Err(BuzzError::Unauthorized(
            "only the agent's creator, a community moderator, or an operator may pause it".into(),
        ));
    }
    // Management authority is standing, the write gate is current: a sanctioned
    // or removed owner keeps the first and must not use it while restricted.
    enforce_write_gate(ctx, community, &caller.pubkey, now_ms).await?;

    let key = keys::agent_paused(community, &agent);
    if active {
        store::delete_key(&mut config, &key).await?;
    } else {
        store::put_raw(&mut config, &key, b"1").await?;
    }
    serde_json::to_vec(&serde_json::json!({ "ok": true, "agent": agent, "active": active }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/agents/memory` — what an agent remembers.
///
/// The `agent-memory` binding has existed since the turn pipeline was wired and
/// nothing ever read it back, so a client could only refuse the question. Bounded
/// and readable by anyone who may manage the agent — an agent's memory can carry
/// what people said to it, so it is not public.
pub(super) async fn agent_memory(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("memory body: {e}")))?;
    let agent = body
        .get("agent")
        .and_then(Value::as_str)
        .and_then(clean_agent_id)
        .ok_or_else(|| BuzzError::InvalidEvent("this needs an 'agent' id".into()))?;
    if !may_manage_agent(ctx, &mut config, community, &agent, &caller.pubkey).await? {
        return Err(BuzzError::Unauthorized(
            "only the agent's creator, a community moderator, or an operator may read its memory"
                .into(),
        ));
    }

    // The memory plane is optional: a deployment without it has an agent with no
    // memory, which is an empty answer rather than an error.
    let Ok(mut memory) = planes::store(ctx, store::AGENT_MEMORY) else {
        return serde_json::to_vec(&serde_json::json!({ "ok": true, "agent": agent, "entries": [] }))
            .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")));
    };
    let prefix = keys::agent_memory_prefix(community, &agent);
    let (rows, more) = store::scan(&mut memory, &prefix, None, 200).await?;
    let entries: Vec<Value> = rows
        .iter()
        .filter_map(|(key, value, _version)| {
            let id = key.rsplit(|b| *b == b'/').next()?;
            Some(serde_json::json!({
                "id": String::from_utf8_lossy(id),
                "value": serde_json::from_slice::<Value>(value).unwrap_or_else(|_| {
                    Value::String(String::from_utf8_lossy(value).into_owned())
                }),
            }))
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "ok": true, "agent": agent, "entries": entries, "complete": !more
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/agents/config` — read or write a cloud agent's configuration.
///
/// A cloud agent has no local harness process, so there is no "session" to hold
/// per-session config: its DEFINITION on the relay is the configuration, and this
/// reads or updates that. A body with neither `set` nor `patch` is a read.
///
/// An update never replaces the stored definition wholesale. `set` is the web
/// client's document (its `instructions`/`default_model` are taken as given,
/// every other stored field it does not name is kept), and `patch` is a
/// top-level merge patch (see [`crate::relay::operator::DefinitionUpdate`]).
/// The two fields the route reads back are what a persona's creator or a
/// community moderator may change; the agent's guardrails and capabilities
/// (`tool_deny`, `approvals`, `limits`, `subagents`, ...) change only for an
/// operator. A body may carry `if_generation`, the `generation` a read
/// returned, so a concurrent edit is refused with 409 instead of overwritten.
pub(super) async fn agent_config(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("config body: {e}")))?;
    let agent = body
        .get("agent")
        .and_then(Value::as_str)
        .and_then(clean_agent_id)
        .ok_or_else(|| BuzzError::InvalidEvent("this needs an 'agent' id".into()))?;

    let update = match (body.get("set"), body.get("patch")) {
        (Some(_), Some(_)) => {
            return Err(BuzzError::InvalidEvent(
                "a config update names either 'set' or 'patch', not both".into(),
            ))
        }
        (Some(set), None) => Some((set, crate::relay::operator::DefinitionUpdate::Set)),
        (None, Some(patch)) => Some((patch, crate::relay::operator::DefinitionUpdate::Patch)),
        (None, None) => None,
    };
    let is_read = update.is_none();
    if let Some((update, mode)) = update {
        // An edit is a write: an archived, banned or timed-out identity cannot
        // rewrite a persona any more than it can post. Checked before
        // the management check, so the refusal names the sanction.
        enforce_write_gate(ctx, community, &caller.pubkey, now_ms).await?;
        if !may_manage_agent(ctx, &mut config, community, &agent, &caller.pubkey).await? {
            return Err(BuzzError::Unauthorized(
                "only the agent's creator, a community moderator, or an operator may configure it"
                    .into(),
            ));
        }
        let expected_generation = match body.get("if_generation") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_u64().ok_or_else(|| {
                BuzzError::InvalidEvent("if_generation must be a generation a read returned".into())
            })?),
        };
        let is_operator = caller_is_operator(ctx, &mut config, &caller.pubkey).await?;
        update_agent_definition(
            &mut config,
            &agent,
            update,
            mode,
            is_operator,
            expected_generation,
        )
        .await?;
    }

    // Reading a definition needs management authority unless the agent is on
    // the public roster, whose instructions and model `GET /buzz/api/agents`
    // already shows to anyone. A definition row with no roster entry (retired,
    // or written by an operator before it was published) is not public.
    if is_read
        && !registry_or_empty(&mut config)
            .await?
            .personas
            .iter()
            .any(|persona| persona.agent == agent)
        && !may_manage_agent(ctx, &mut config, community, &agent, &caller.pubkey).await?
    {
        return Err(BuzzError::Unauthorized(
            "only the agent's creator, a community moderator, or an operator may read an unpublished agent's configuration"
                .into(),
        ));
    }
    // ONE versioned read builds the reply. Reading the fields and the generation
    // separately lets a concurrent edit land between them, so a client could be
    // handed the old instructions under the new generation and then overwrite
    // that edit with an `if_generation` the relay accepted.
    let (instructions, model, generation) = agent_config_view(&mut *config, &agent).await?;
    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "agent": agent,
        "instructions": instructions,
        "model": model,
        "generation": generation,
        "active": !agent_is_paused(&mut config, community, &agent).await,
        "harness": crate::relay::agents::DEFAULT_HARNESS,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// Where [`agent_config_view`] reads a definition row from.
///
/// A seam rather than a direct store call so the one-read property can be
/// tested against a store whose row changes between any two reads, which is
/// the interleaving a concurrent edit produces and which a live fleet cannot
/// schedule deterministically.
pub(crate) trait DefinitionRows {
    async fn versioned(&mut self, key: &[u8]) -> Result<Option<(Vec<u8>, u64)>>;
}

impl DefinitionRows for ferrite_app::client::StoreClusterClient {
    async fn versioned(&mut self, key: &[u8]) -> Result<Option<(Vec<u8>, u64)>> {
        store::get_raw_versioned(self, key).await
    }
}

/// The `(instructions, model, generation)` the config route answers with, all
/// from ONE versioned read of the definition row, so the fields a client sees
/// always belong to the generation it is told to send back as `if_generation`.
pub(super) async fn agent_config_view<R: DefinitionRows>(
    rows: &mut R,
    agent: &str,
) -> Result<(Option<String>, Option<String>, u64)> {
    let current = rows
        .versioned(&crate::relay::operator::agent_def_row(agent))
        .await?;
    let (instructions, model) =
        definition_fields(current.as_ref().map(|(bytes, _)| bytes.as_slice()));
    Ok((
        instructions,
        model,
        current.map_or(0, |(_, version)| version),
    ))
}

/// Merge one client update into an agent's stored definition, under a
/// compare-and-swap on the definition row.
///
/// The merge ([`crate::relay::operator::merge_agent_definition`]) keeps every
/// field the update does not name, so editing a model cannot erase the
/// agent's deny list, approval gates, limits, subagents, skills or schedules.
/// Changing any of those takes an operator, whatever the caller's management
/// authority. The result goes through the runtime's OWN parser, so this
/// boundary and the loader can never drift about what a definition is.
///
/// `expected_generation` is the row version the caller last read (0 for no
/// row); when given, a definition that has moved since is a 409. Without it a
/// lost race re-reads and re-merges, because the merge is computed from the
/// definition as it is now.
pub(super) async fn update_agent_definition(
    config: &mut ferrite_app::client::StoreClusterClient,
    agent: &str,
    update: &Value,
    mode: crate::relay::operator::DefinitionUpdate,
    is_operator: bool,
    expected_generation: Option<u64>,
) -> Result<()> {
    let key = crate::relay::operator::agent_def_row(agent);
    let mut attempted: Option<Vec<u8>> = None;
    for _ in 0..CLAIM_RETRIES {
        let current = store::get_raw_versioned(config, &key).await?;
        // A lost reply to an applied write reports a mismatch: recognize our
        // own bytes rather than refusing (or re-merging) a write that landed.
        if let (Some((bytes, _)), Some(mine)) = (&current, &attempted) {
            if bytes == mine {
                return Ok(());
            }
        }
        let version = current.as_ref().map(|(_, version)| *version);
        if let Some(expected) = expected_generation {
            let now = version.unwrap_or(0);
            if now != expected {
                return Err(BuzzError::Conflict(format!(
                    "agent {agent:?} was changed since it was read (generation {now}, not \
                     {expected}); read it again and retry"
                )));
            }
        }
        let stored = match &current {
            None => None,
            Some((bytes, _)) => match serde_json::from_slice::<Value>(bytes) {
                Ok(doc) if doc.is_object() => Some(doc),
                // An unreadable row is not an empty one: merging onto nothing
                // would silently drop whatever it held. An operator may
                // replace it; nobody else may guess at it.
                _ if is_operator => None,
                _ => {
                    return Err(BuzzError::Conflict(format!(
                        "agent {agent:?} has an unreadable stored definition; an operator must \
                         repair it"
                    )))
                }
            },
        };
        let merged =
            crate::relay::operator::merge_agent_definition(agent, stored.as_ref(), update, mode)?;
        let guarded =
            crate::relay::operator::changed_guarded_definition_fields(stored.as_ref(), &merged);
        if !guarded.is_empty() && !is_operator {
            return Err(BuzzError::Unauthorized(format!(
                "only an operator may change an agent's {}",
                guarded.join(", ")
            )));
        }
        let bytes = crate::relay::operator::validate_agent_definition(agent, &merged)?;
        match config
            .put_if_version(&key, &bytes, version)
            .await
            .map_err(storage)?
        {
            Ok(_) => return Ok(()),
            Err(_) => attempted = Some(bytes),
        }
    }
    Err(BuzzError::Conflict(format!(
        "agent {agent:?}'s definition is contended; retry"
    )))
}

/// `POST /buzz/api/agents/owner` — who owns an agent.
///
/// This was refused as unanswerable: resolving an owner means VERIFYING a NIP-OA
/// claim, and this build has no verifier, so reporting a claimed owner would
/// present an assertion as a fact.
///
/// It turns out not to need one. The relay RECORDED the owner when the agent was
/// created, behind the create-if-absent ownership claim that decides who may
/// configure it — so the answer is already known, and already verified by the
/// relay at the moment it was established rather than by re-checking something a
/// profile asserts. An agent with no recorded owner (one an operator seeded
/// directly into the registry) answers `null`, which is the truth about it.
pub(super) async fn agent_owner_route(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let _caller = authenticate_request(&mut config, community, req, now_ms).await?;
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("owner body: {e}")))?;

    // Either the agent id, or the pubkey a client saw on a message.
    let agent = match body
        .get("agent")
        .and_then(Value::as_str)
        .and_then(clean_agent_id)
    {
        Some(agent) => Some(agent),
        None => {
            let wanted = body
                .get("pubkey")
                .and_then(Value::as_str)
                .and_then(crate::relay::crypto::from_hex32)
                .ok_or_else(|| {
                    BuzzError::InvalidEvent("this needs an 'agent' id or a 'pubkey'".into())
                })?;
            registry_or_empty(&mut config)
                .await?
                .personas
                .iter()
                .find(|p| p.pubkey == wanted)
                .map(|p| p.agent.clone())
        }
    };
    let owner = match &agent {
        Some(agent) => store::get_raw(&mut config, &keys::agent_owner(community, agent))
            .await?
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
            .filter(|hex| crate::relay::crypto::from_hex32(hex).is_some()),
        None => None,
    };
    serde_json::to_vec(&serde_json::json!({ "ok": true, "agent": agent, "owner_pubkey": owner }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `buzz-agent-harnesses` — the operator's declaration of which agent backends
/// this deployment can reach, as a JSON array of harness objects.
///
/// Absent, the answer is derived from what is actually WIRED rather than
/// guessed: see [`agent_harnesses`].
pub(super) const AGENT_HARNESSES_CONFIG: &[u8] = b"buzz-agent-harnesses";

//
// A browser tab cannot launch or supervise a local harness process — but a
// CLOUD persona needs none: this app runs the turn pipeline itself over its
// own `agent-turns` queue, and reaches models through its audited
// `provider-egress` handle. Registering one is therefore a relay
// write, and these routes are what let a client make it without an operator
// shell: the model catalog to pick from, the roster to show, create, remove.

/// The config key naming WHO may create a cloud persona through this route:
/// `members` (any authenticated caller — the DEFAULT), `moderators`, or
/// `operators`. Default-open for members deliberately: a workspace where a
/// person cannot add an AI teammate without operator credentials is the gap
/// this route exists to close. An operator tightens it with one config write.
pub(super) const AGENT_CREATION_CONFIG: &[u8] = b"buzz-agent-creation";

/// The most personas this route will register. A cap, not a licence: each
/// persona is a relay-held signing key plus a turn-pipeline consumer, so
/// unbounded creation is a resource vector.
pub(super) const MAX_CLIENT_AGENTS: usize = 64;

/// How many agents ONE pubkey may create per [`AGENT_CREATE_RATE_WINDOW_MS`].
///
/// Creation is a path to PROVIDER SPEND, not just to a registry row: every
/// persona can be mentioned, and every mention is a model call on this
/// deployment's own credential. [`MAX_CLIENT_AGENTS`] bounds how many personas
/// can exist; this bounds how fast one caller can get there, which is what
/// makes bulk creation and name probing uninteresting. Generous enough that a
/// person adding a teammate — and making a few mistakes doing it — never meets
/// it.
pub(super) const AGENT_CREATE_RATE_LIMIT: i64 = 20;
pub(super) const AGENT_CREATE_RATE_WINDOW_MS: u64 = 60 * 60 * 1000;

/// `GET /buzz/api/agent-models` — the models this deployment can route to,
/// plus the configured default. PUBLIC, like `agent-harnesses`: a client must
/// be able to populate a model picker during onboarding, before it has an
/// identity to authenticate with.
pub(super) async fn agent_models(ctx: &AppContext) -> Result<Vec<u8>> {
    let models: Vec<Value> = crate::llm::routable_catalog(ctx)
        .await
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "provider": m.provider,
                "context_window": m.context_window,
                "max_output_tokens": m.max_output_tokens,
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "default_model": crate::llm::configured_default_model(ctx).await,
        "harness": crate::relay::agents::DEFAULT_HARNESS,
        "models": models,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// Read the persona registry, tolerating a malformed document: a client's
/// roster must degrade to empty rather than 500 the whole agents surface.
pub(super) async fn registry_or_empty(
    config: &mut ferrite_app::client::StoreClusterClient,
) -> Result<crate::relay::agents::Registry> {
    let Some(bytes) = store::get_raw(config, crate::relay::agents::REGISTRY_CONFIG).await? else {
        return Ok(crate::relay::agents::Registry::default());
    };
    Ok(
        crate::relay::agents::Registry::parse(&bytes).unwrap_or_else(|e| {
            tracing::warn!(
                "the buzz-agents registry is malformed ({e}); serving none"
            );
            crate::relay::agents::Registry::default()
        }),
    )
}

/// The persona registry, parsed STRICTLY.
///
/// [`registry_or_empty`] is tolerant by design — a broken document must not 500 a
/// client's roster — and that tolerance is wrong for any decision with teeth: an
/// unparseable document and "there are no personas" become indistinguishable, so
/// one malformed row reads as every persona having been retired. On the wake path
/// that answer CANCELS schedules, so a single bad write would irreversibly cancel
/// every scheduled wake in the workspace while each cancellation looked correct.
///
/// The same distinction `create_agent` and `remove_agent` already make: the list
/// path stays tolerant, anything that mutates parses strictly.
///
/// An ABSENT registry is still an empty one, because absence is a fact. A parse
/// failure is not.
pub(super) async fn registry_strict(
    config: &mut ferrite_app::client::StoreClusterClient,
) -> Result<crate::relay::agents::Registry> {
    let Some(bytes) = store::get_raw(config, crate::relay::agents::REGISTRY_CONFIG).await? else {
        return Ok(crate::relay::agents::Registry::default());
    };
    crate::relay::agents::Registry::parse(&bytes)
}

/// One persona's stored definition fields a client wants to show or edit.
pub(super) async fn agent_definition_fields(
    config: &mut ferrite_app::client::StoreClusterClient,
    agent: &str,
) -> (Option<String>, Option<String>) {
    let key = crate::relay::operator::agent_def_row(agent);
    let Ok(Some(bytes)) = store::get_raw(config, &key).await else {
        return (None, None);
    };
    definition_fields(Some(&bytes))
}

/// The `instructions` and `default_model` one stored definition names.
/// Tolerant: an absent or unreadable row names neither.
pub(super) fn definition_fields(bytes: Option<&[u8]>) -> (Option<String>, Option<String>) {
    let Some(Ok(doc)) = bytes.map(serde_json::from_slice::<Value>) else {
        return (None, None);
    };
    (
        doc.get("instructions")
            .and_then(Value::as_str)
            .map(str::to_string),
        doc.get("default_model")
            .and_then(Value::as_str)
            .map(str::to_string),
    )
}

/// `GET /buzz/api/agents` — the personas this deployment runs, with the
/// instructions and model each was created with. Public: a persona's pubkey
/// is already on every message it posts, so the roster reveals nothing a
/// reader cannot see, and a client needs it to render mentions and avatars.
pub(super) async fn list_agents(ctx: &AppContext) -> Result<Vec<u8>> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let registry = registry_or_empty(&mut config).await?;
    let mut agents = Vec::with_capacity(registry.personas.len());
    for persona in &registry.personas {
        let (instructions, model) = agent_definition_fields(&mut config, &persona.agent).await;
        agents.push(serde_json::json!({
            "agent": persona.agent,
            "pubkey": crate::relay::crypto::to_hex(&persona.pubkey),
            "harness": persona.harness,
            "instructions": instructions,
            "model": model,
        }));
    }
    serde_json::to_vec(&serde_json::json!({ "agents": agents }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// A client-supplied agent id, or one derived from a display name: the agent
/// runtime's OWN charset (lowercase alphanumeric plus `-`/`_`), so a created
/// persona can always carry a definition row and be named in a registry.
pub(super) fn clean_agent_id(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || lower == '-' || lower == '_' {
            out.push(lower);
        } else if !out.ends_with('-') {
            // Any other rune (a space, an emoji, punctuation) becomes ONE
            // separator, so "Ada the Helper" and "Ada  the  Helper" agree.
            out.push('-');
        }
    }
    let id = out.trim_matches('-').to_string();
    if id.is_empty() || id.len() > 64 || crate::relay::crypto::from_hex32(&id).is_some() {
        return None;
    }
    Some(id)
}

/// Derive a persona's signing key.
///
/// A constrained app has NO entropy source (see [`crate::relay::keys`]: the
/// relay cannot mint keys), so a client-created persona's key is HMAC-derived
/// from the relay's own root secret under a domain-separated label. Two
/// properties make this the right shape rather than a workaround: the secret
/// never crosses the wire (a client posts a NAME, not a key), and recreating
/// the same agent id is idempotent instead of orphaning the identity it has
/// already published under. The counter walks past a digest that is not a
/// valid secp256k1 scalar — astronomically unlikely, and fail-closed.
pub(super) fn derive_persona_seckey(root: &[u8; 32], community: &str, agent: &str) -> Option<[u8; 32]> {
    for counter in 0u8..16 {
        let seckey = crate::relay::crypto::hmac::hmac_sha256_concat(
            root,
            &[
                b"buzz-persona/v1/",
                community.as_bytes(),
                b"/",
                agent.as_bytes(),
                b"/",
                &[counter],
            ],
        );
        if crate::relay::crypto::schnorr::xonly_pubkey(&seckey).is_some() {
            return Some(seckey);
        }
    }
    None
}

/// Is `who` in the operator set? Secret-first, config-fallback — the same
/// resolution order the operator plane itself uses.
pub(super) async fn caller_is_operator(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
    who: &crate::relay::event::PubKey,
) -> Result<bool> {
    let bytes = match ctx.secret(crate::relay::operator::OPERATOR_SET_SECRET) {
        Some(injected) => injected.into_bytes(),
        None => match store::get_raw(config, crate::relay::operator::OPERATOR_SET_CONFIG).await? {
            Some(bytes) => bytes,
            None => return Ok(false),
        },
    };
    Ok(crate::relay::operator::parse_operator_set(&bytes)?
        .iter()
        .any(|pk| pk == who))
}

/// Enforce the deployment's agent-creation policy. An UNKNOWN policy value
/// fails closed: a typo in the config must not silently widen who may mint
/// signing identities.
pub(super) async fn enforce_agent_creation_policy(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    who: &crate::relay::event::PubKey,
) -> Result<()> {
    let policy = store::get_raw(config, AGENT_CREATION_CONFIG)
        .await?
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_ascii_lowercase())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "members".to_string());
    match policy.as_str() {
        "members" => Ok(()),
        "moderators" => {
            if is_community_moderator(config, community, who).await? {
                Ok(())
            } else {
                Err(BuzzError::Unauthorized(
                    "only a community moderator may create an agent here".into(),
                ))
            }
        }
        "operators" => {
            if caller_is_operator(ctx, config, who).await? {
                Ok(())
            } else {
                Err(BuzzError::Unauthorized(
                    "only an operator may create an agent here".into(),
                ))
            }
        }
        other => Err(BuzzError::Internal(format!(
            "buzz-agent-creation is {other:?}; expected members|moderators|operators"
        ))),
    }
}

/// May `who` MANAGE `agent` — change what it is, or retire it?
///
/// Deliberately NOT the creation policy: with the default `members` policy,
/// reusing it would let any authenticated member rewrite or delete anybody's
/// teammate. The rule is narrower and needs no configuration — the persona's
/// own creator, a community moderator, or an operator.
pub(super) async fn may_manage_agent(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    agent: &str,
    who: &crate::relay::event::PubKey,
) -> Result<bool> {
    if let Some(owner) = store::get_raw(config, &keys::agent_owner(community, agent)).await? {
        if String::from_utf8_lossy(&owner).trim() == crate::relay::crypto::to_hex(who) {
            return Ok(true);
        }
    }
    if is_community_moderator(config, community, who).await? {
        return Ok(true);
    }
    caller_is_operator(ctx, config, who).await
}

/// Serialize a registry back to its canonical stored form (the exact shape
/// [`crate::relay::agents::Registry::parse`] reads and the operator route
/// writes, so the two paths can never disagree about the document).
pub(super) fn registry_bytes(personas: &[crate::relay::agents::Persona]) -> Result<Vec<u8>> {
    let entries: Vec<Value> = personas
        .iter()
        .map(|p| {
            serde_json::json!({
                "agent": p.agent,
                "pubkey": crate::relay::crypto::to_hex(&p.pubkey),
                "harness": p.harness,
            })
        })
        .collect();
    serde_json::to_vec(&entries).map_err(|e| BuzzError::Internal(format!("registry encode: {e}")))
}

/// `POST /buzz/api/agents` — create a CLOUD persona: one that runs on this
/// deployment's own turn pipeline with no local process anywhere.
///
/// Body: `{ agent?, display_name?, instructions?, harness?, model? }`. Either an
/// explicit `agent` id or a `display_name` to derive one from is required.
/// The reply carries the persona's pubkey, so the caller can mention it
/// immediately.
///
/// What this writes, in the order that survives both concurrency and a crash:
/// the OWNERSHIP CLAIM first (create-if-absent on the agent name — exactly one
/// caller earns the right to configure this agent), then the derived signing
/// KEY (identical bytes for the same id, so it can never be a takeover), then
/// the registry entry, and only then the caller-controlled definition and
/// profile. A crash anywhere leaves a state the SAME caller can retry into,
/// and no state another member can seize.
pub(super) async fn create_agent(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    // Creating or re-posting an agent is a write: an archived, banned or
    // timed-out identity is refused here exactly as a post would be.
    enforce_write_gate(ctx, community, &caller.pubkey, now_ms).await?;
    enforce_agent_creation_policy(ctx, &mut config, community, &caller.pubkey).await?;

    enforce_agent_creation_rate(&mut config, community, &caller.pubkey, now_ms).await?;

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("agent body: {e}")))?;
    let display_name = body
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let requested = body
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let source = if requested.trim().is_empty() {
        display_name.as_str()
    } else {
        requested
    };
    let agent = clean_agent_id(source).ok_or_else(|| {
        BuzzError::InvalidEvent(
            "an agent needs a name that yields 1..=64 characters of [a-z0-9_-] (and is not a 64-hex pubkey)".into(),
        )
    })?;
    let instructions = body
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if instructions.len() > 16 * 1024 {
        return Err(BuzzError::InvalidEvent(
            "agent instructions are over the 16 KiB bound".into(),
        ));
    }
    let harness = resolve_agent_harness(&mut config, &body).await?;
    let model = resolve_agent_model(ctx, &body).await?;

    // The relay's root secret is what a persona key is derived from; without
    // it the relay could not sign for the persona anyway.
    let root = relay_identity(ctx, &mut config).await.ok_or_else(|| {
        BuzzError::Internal(
            "this deployment has no relay identity, so it cannot hold a persona key".into(),
        )
    })?;
    let seckey = derive_persona_seckey(&root, community, &agent)
        .ok_or_else(|| BuzzError::Internal("persona key derivation failed".into()))?;
    let pubkey = crate::relay::crypto::schnorr::xonly_pubkey(&seckey)
        .ok_or_else(|| BuzzError::Internal("derived persona key is out of range".into()))?;

    claim_agent_ownership(ctx, &mut config, community, &agent, &caller.pubkey).await?;
    write_persona_key(&mut config, community, &agent, &pubkey, &seckey).await?;
    register_persona(&mut config, &agent, &pubkey, &harness).await?;

    // The persona is REGISTERED now, so admit it to the community before anything
    // it signs is submitted. In a closed community this is what lets its own
    // profile below — and every answer it ever posts — past
    // `enforce_community_membership`; without it a teammate created through this
    // route would be dispatchable and mute until the next boot reconcile.
    //
    // PROPAGATED, not best-effort: answering 201 for a persona that cannot write
    // is the failure this whole path exists to prevent. The registry entry above
    // is already committed and every write here is idempotent, so the caller's
    // retry heals it rather than duplicating anything.
    {
        let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
        if admit_persona_to_community(&mut events, community, &pubkey, now_ms).await? {
            tracing::info!("agent {agent} was admitted to the community");
        }
    }

    let definition = write_agent_definition(
        ctx,
        &mut config,
        &caller.pubkey,
        &agent,
        &AgentDefinitionInput {
            display_name: &display_name,
            instructions: &instructions,
            model: &model,
            body: &body,
        },
    )
    .await?;

    // The persona's profile, so it renders with a name and not a raw pubkey.
    // Best-effort: a teammate that exists but has no profile yet is a cosmetic
    // gap, and must not fail a creation that already committed.
    let profile_name = if display_name.is_empty() {
        agent.clone()
    } else {
        display_name.clone()
    };
    post_persona_profile(ctx, &seckey, &pubkey, &agent, &profile_name, now_ms).await;

    tracing::info!(
        agent = %agent,
        by = %crate::relay::crypto::to_hex(&caller.pubkey),
        "a cloud agent was created from a client"
    );
    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "agent": agent,
        "display_name": profile_name,
        "pubkey": crate::relay::crypto::to_hex(&pubkey),
        "harness": harness,
        "model": definition.get("default_model"),
        "instructions": definition.get("instructions"),
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// Bound the RATE, not just the total (see the constants): fixed-window per
/// pubkey, counted before validation so refused probing costs the prober.
/// Fails OPEN on an ambiguous increment, like every other limiter here: the
/// policy check, not this counter, is the authority.
async fn enforce_agent_creation_rate(
    config: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    who: &crate::relay::event::PubKey,
    now_ms: u64,
) -> Result<()> {
    match store::bump_expiring_counter(
        config,
        &keys::rate(community, who, "agentnew"),
        1,
        now_ms.saturating_add(AGENT_CREATE_RATE_WINDOW_MS),
    )
    .await
    {
        Ok(count) if count > AGENT_CREATE_RATE_LIMIT => return Err(BuzzError::RateLimited),
        Ok(_) => {}
        Err(e) => tracing::warn!("agent creation limiter failed open: {e}"),
    }
    Ok(())
}

async fn resolve_agent_harness(config: &mut ferrite_app::client::StoreClusterClient, body: &Value) -> Result<String> {
    Ok(match body.get("harness").and_then(Value::as_str).map(str::trim) {
        None | Some("") | Some(crate::relay::agents::DEFAULT_HARNESS) => {
            crate::relay::agents::DEFAULT_HARNESS.to_string()
        }
        Some(requested @ ("fabric" | "a2a")) => {
            let declared = config
                .get(AGENT_HARNESSES_CONFIG)
                .await
                .map_err(storage)?
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .and_then(|value| value.as_array().cloned())
                .unwrap_or_default();
            let available = declared.iter().any(|entry| {
                entry.get("id").and_then(Value::as_str) == Some(requested)
                    && entry.get("availability").and_then(Value::as_str) == Some("available")
            });
            if !available {
                return Err(BuzzError::InvalidEvent(format!(
                    "agent harness {requested:?} is not configured as available on this deployment"
                )));
            }
            requested.to_string()
        }
        Some(requested) => {
            return Err(BuzzError::InvalidEvent(format!(
                "agent harness {requested:?} is not one this deployment can run"
            )));
        }
    })
}

/// A model the picker offered, or this deployment's configured default. An
/// id outside the catalog is refused HERE rather than at the first turn,
/// where it would surface as a dead teammate instead of a bad request.
async fn resolve_agent_model(ctx: &AppContext, body: &Value) -> Result<String> {
    Ok(match body.get("model").and_then(Value::as_str).map(str::trim) {
        None | Some("") => {
            let default = crate::llm::configured_default_model(ctx).await;
            // When an override has disabled EVERY model there is no routable
            // default to fall back on: refuse rather than register a teammate
            // whose first turn would die with `unsupported_model`.
            if !crate::llm::routable_catalog(ctx)
                .await
                .iter()
                .any(|m| m.id == default)
            {
                return Err(BuzzError::Internal(
                    "this deployment has no routable model, so an agent cannot be created".into(),
                ));
            }
            default
        }
        Some(requested) => {
            if !crate::llm::routable_catalog(ctx)
                .await
                .iter()
                .any(|m| m.id == requested)
            {
                return Err(BuzzError::InvalidEvent(format!(
                    "model {requested:?} is not one this deployment can route to"
                )));
            }
            requested.to_string()
        }
    })
}

/// THE OWNERSHIP CLAIM: the first write of the route, and the thing every
/// later write derives its authority from.
///
/// Create-if-absent on the agent NAME. The persona key is DERIVED from that
/// name, so every caller computes the same key and a registry "same key" test
/// alone would let any member rewrite another member's teammate. Claiming the
/// name atomically gives exactly ONE caller the right to configure this agent.
/// It is deliberately FIRST rather than last: a crash after it leaves a state
/// the same caller retries into (the claim is theirs, so the gate passes and
/// creation completes), while a registry entry written before an owner would
/// strand an agent nobody but a moderator could repair. Everything fallible
/// before this call (body parsing, the model check, key derivation) has
/// already run, so a bad request cannot squat a name it never earns.
async fn claim_agent_ownership(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    agent: &str,
    who: &crate::relay::event::PubKey,
) -> Result<()> {
    let caller_hex = crate::relay::crypto::to_hex(who);
    let claimed = config
        .put_if_version(
            &keys::agent_owner(community, agent),
            caller_hex.as_bytes(),
            None,
        )
        .await
        .map_err(storage)?
        .is_ok();
    if !claimed && !may_manage_agent(ctx, config, community, agent, who).await? {
        return Err(BuzzError::Unauthorized(format!(
            "an agent named {agent:?} is already claimed; only its creator, a community \
             moderator, or an operator may change it"
        )));
    }
    Ok(())
}

/// The KEY. Its bytes are DERIVED from the agent id, so this write is
/// idempotent under any race, and a persona that appears in the registry can
/// always sign. It is written only after the registry is known not to name
/// this agent under a different identity, so a refused request never replaces
/// the signing material of a registered persona.
async fn write_persona_key(
    config: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    agent: &str,
    pubkey: &crate::relay::event::PubKey,
    seckey: &[u8; 32],
) -> Result<()> {
    if let Some((bytes, _)) =
        store::get_raw_versioned(config, crate::relay::agents::REGISTRY_CONFIG).await?
    {
        let registry = crate::relay::agents::Registry::parse(&bytes)?;
        if registry
            .personas
            .iter()
            .any(|p| p.agent == agent && p.pubkey != *pubkey)
        {
            return Err(BuzzError::Conflict(format!(
                "an agent named {agent:?} already exists under a different key"
            )));
        }
    }
    store::put_raw(
        config,
        &keys::keyring(community, agent),
        crate::relay::crypto::to_hex(seckey).as_bytes(),
    )
    .await?;
    Ok(())
}

/// The registry entry, under a compare-and-swap: the registry is ONE document,
/// so concurrent creators must not lose one another's persona.
async fn register_persona(
    config: &mut ferrite_app::client::StoreClusterClient,
    agent: &str,
    pubkey: &crate::relay::event::PubKey,
    harness: &str,
) -> Result<()> {
    let mut inserted = false;
    let mut already_registered = false;
    for _ in 0..CLAIM_RETRIES {
        let current =
            store::get_raw_versioned(config, crate::relay::agents::REGISTRY_CONFIG).await?;
        // A MALFORMED registry is an error on the mutation path, never an
        // empty one: `unwrap_or_default` here would rewrite the document with
        // this one persona and silently delete every existing teammate. The
        // LIST path stays tolerant (a broken registry shows no agents); a
        // WRITE refuses until an operator repairs it.
        let (expected, mut personas) = match &current {
            Some((bytes, version)) => (
                Some(*version),
                crate::relay::agents::Registry::parse(bytes)?.personas,
            ),
            None => (None, Vec::new()),
        };
        if let Some(existing) = personas.iter_mut().find(|p| p.agent == agent) {
            // Already registered, and this caller holds the ownership claim,
            // so the definition write below IS the authorized update.
            if existing.pubkey == *pubkey {
                if existing.harness == harness {
                    already_registered = true;
                    break;
                }
                existing.harness = harness.to_string();
                let bytes = registry_bytes(&personas)?;
                match config
                    .put_if_version(crate::relay::agents::REGISTRY_CONFIG, &bytes, expected)
                    .await
                    .map_err(storage)?
                {
                    Ok(_) => {
                        inserted = true;
                        break;
                    }
                    Err(_) => continue,
                }
            }
            // Registered under a DIFFERENT key than this name derives: a
            // registry an operator hand-edited. Refuse rather than fight it.
            return Err(BuzzError::Conflict(format!(
                "an agent named {agent:?} already exists under a different key"
            )));
        }
        if personas.len() >= MAX_CLIENT_AGENTS {
            return Err(BuzzError::InvalidEvent(format!(
                "this deployment already has {MAX_CLIENT_AGENTS} agents"
            )));
        }
        personas.push(crate::relay::agents::Persona {
            agent: agent.to_string(),
            pubkey: *pubkey,
            harness: harness.to_string(),
        });
        let bytes = registry_bytes(&personas)?;
        match config
            .put_if_version(crate::relay::agents::REGISTRY_CONFIG, &bytes, expected)
            .await
            .map_err(storage)?
        {
            Ok(_) => {
                inserted = true;
                break;
            }
            // Somebody else wrote the registry: re-read and re-decide rather
            // than assume we lost (a lost response reports our own version).
            Err(_) => continue,
        }
    }
    if !inserted && !already_registered {
        return Err(BuzzError::Conflict(
            "the agent registry is contended; retry".into(),
        ));
    }
    Ok(())
}

struct AgentDefinitionInput<'a> {
    display_name: &'a str,
    instructions: &'a str,
    model: &'a str,
    body: &'a Value,
}

/// The DEFINITION: caller-controlled, so written only now that this request
/// either holds a fresh claim or passed the management check. Validated by the
/// agent runtime's OWN parser, so the boundary and the consumer can never drift
/// about what a definition is.
///
/// A failure here leaves a registered persona with no definition, which the
/// runtime already treats as "default surface": a teammate with generic
/// instructions, not a broken one.
///
/// Re-posting an EXISTING agent is an edit, so it merges onto the stored
/// definition: only a model or instructions the request actually names replace
/// the stored ones, and everything else an operator configured (`tool_deny`,
/// `approvals`, `limits`, `subagents`, `skills`, `schedules`, ...) survives. A
/// new agent gets the defaults.
async fn write_agent_definition(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
    caller: &crate::relay::event::PubKey,
    agent: &str,
    input: &AgentDefinitionInput<'_>,
) -> Result<Value> {
    let AgentDefinitionInput {
        display_name,
        instructions,
        model,
        body,
    } = *input;
    let model_named = body
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|m| !m.trim().is_empty());
    let default_instructions = format!(
        "You are {}, a helpful teammate in this buzz workspace. Answer concisely.",
        if display_name.is_empty() {
            agent.to_string()
        } else {
            display_name.to_string()
        }
    );
    let definition_key = crate::relay::operator::agent_def_row(agent);
    let mut written: Option<Value> = None;
    for _ in 0..CLAIM_RETRIES {
        let current = store::get_raw_versioned(config, &definition_key).await?;
        // MISSING and UNREADABLE are different answers. A missing row gets the
        // defaults; treating an unreadable one the same would let re-posting
        // an agent replace whatever the row holds (its operator-set `tool_deny`,
        // `approvals`, limits, ...) with a fresh default definition. Only an
        // operator may replace it, exactly as on the config route
        // ([`update_agent_definition`]).
        let stored = match &current {
            None => None,
            Some((bytes, _)) => match serde_json::from_slice::<Value>(bytes) {
                Ok(doc) if doc.is_object() => Some(doc),
                _ if caller_is_operator(ctx, config, caller).await? => None,
                _ => {
                    return Err(BuzzError::Conflict(format!(
                        "agent {agent:?} has an unreadable stored definition; an operator must \
                         repair it"
                    )))
                }
            },
        };
        let mut patch = serde_json::Map::new();
        if stored.is_none() || model_named {
            patch.insert("default_model".into(), Value::from(model));
        }
        if !instructions.is_empty() {
            patch.insert("instructions".into(), Value::from(instructions));
        } else if stored.is_none() {
            patch.insert(
                "instructions".into(),
                Value::from(default_instructions.clone()),
            );
        }
        let merged = crate::relay::operator::merge_agent_definition(
            agent,
            stored.as_ref(),
            &Value::Object(patch),
            crate::relay::operator::DefinitionUpdate::Patch,
        )?;
        let definition_bytes = crate::relay::operator::validate_agent_definition(agent, &merged)?;
        if current
            .as_ref()
            .is_some_and(|(bytes, _)| *bytes == definition_bytes)
        {
            written = Some(merged);
            break;
        }
        match config
            .put_if_version(
                &definition_key,
                &definition_bytes,
                current.as_ref().map(|(_, version)| *version),
            )
            .await
            .map_err(storage)?
        {
            Ok(_) => {
                written = Some(merged);
                break;
            }
            // Re-read and re-merge: the next pass recognizes our own bytes if
            // this was a lost reply to a write that landed.
            Err(_) => continue,
        }
    }
    written.ok_or_else(|| {
        BuzzError::Conflict(format!("agent {agent:?}'s definition is contended; retry"))
    })
}

async fn post_persona_profile(
    ctx: &AppContext,
    seckey: &[u8; 32],
    pubkey: &crate::relay::event::PubKey,
    agent: &str,
    profile_name: &str,
    now_ms: u64,
) {
    match crate::relay::event::Event::sign_new(
        seckey,
        crate::relay::kind::KIND_PROFILE,
        Vec::new(),
        &serde_json::json!({ "name": agent, "display_name": profile_name, "bot": true }).to_string(),
        now_ms / 1000,
    ) {
        Ok(profile) => {
            if let Err(e) = ingest_event(ctx, pubkey, &profile, now_ms).await {
                tracing::warn!("agent {agent} has no profile yet: {e}");
            }
        }
        Err(e) => tracing::warn!("agent {agent} profile could not be signed: {e}"),
    }
}

/// `POST /buzz/api/projects/delete` — retire one project, `{"coordinate":
/// "30621:<author hex>:<d>"}`.
///
/// Authority: the project's author, a configured community moderator, or an
/// operator of the deployment — the same three that may retire a teammate
/// ([`remove_agent`]). The NIP-98 header must commit to the body (`payload`
/// tag), so a captured header cannot be replayed against another project. The
/// retirement is the NIP-09 address deletion itself ([`crate::relay::effects::retire_address`]):
/// every stored version at or below the current second is tombstoned and erased.
pub(super) async fn delete_project(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let (caller, _) = auth::authenticate_event(
        &mut config,
        community,
        header(req, "authorization"),
        &req.method,
        &req.path,
        req.verified_host(),
        Some(&req.body),
        now_ms,
    )
    .await?;

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("project body: {e}")))?;
    let coordinate = body
        .get("coordinate")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("delete needs a 'coordinate'".into()))?;
    let Some((kind_, author, d_tag)) = crate::relay::effects::parse_address(coordinate) else {
        return Err(BuzzError::InvalidEvent(
            "a coordinate is kind:pubkey:d-tag".into(),
        ));
    };
    if kind_ != crate::relay::kind::KIND_PROJECT || d_tag.is_empty() {
        return Err(BuzzError::InvalidEvent(format!(
            "only a project coordinate ({}:<pubkey>:<d>) may be deleted here",
            crate::relay::kind::KIND_PROJECT
        )));
    }
    let operator = caller_is_operator(ctx, &mut config, &caller.pubkey).await?;
    let authorized = operator
        || author == caller.pubkey
        || is_community_moderator(&mut config, community, &caller.pubkey).await?;
    if !authorized {
        return Err(BuzzError::Unauthorized(
            "only the project's creator, a community moderator, or an operator may delete it"
                .into(),
        ));
    }
    // Deleting is a write: a banned, timed-out, archived or removed creator or
    // moderator keeps the standing that names them and must not use it while
    // sanctioned. An operator acts for the deployment and is not gated.
    if !operator {
        enforce_write_gate(ctx, community, &caller.pubkey, now_ms).await?;
    }

    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    crate::relay::effects::retire_address(
        &mut events,
        &mut guards,
        community,
        kind_,
        &author,
        &d_tag,
        now_ms / 1000,
        now_ms,
    )
    .await?;
    serde_json::to_vec(&serde_json::json!({ "ok": true, "coordinate": coordinate }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/agents/remove` — retire a cloud persona: drop it from the
/// registry so it is no longer dispatchable, and delete its key so it cannot
/// sign. Its past messages stay, exactly like a departed human's.
pub(super) async fn remove_agent(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("agent body: {e}")))?;
    let agent = body
        .get("agent")
        .and_then(Value::as_str)
        .and_then(clean_agent_id)
        .ok_or_else(|| BuzzError::InvalidEvent("remove needs an 'agent' id".into()))?;
    if !may_manage_agent(ctx, &mut config, community, &agent, &caller.pubkey).await? {
        return Err(BuzzError::Unauthorized(
            "only the agent's creator, a community moderator, or an operator may retire it".into(),
        ));
    }
    // Retiring a persona deletes its key: a write, gated like configuring it.
    if !caller_is_operator(ctx, &mut config, &caller.pubkey).await? {
        enforce_write_gate(ctx, community, &caller.pubkey, now_ms).await?;
    }

    let mut removed = false;
    for _ in 0..CLAIM_RETRIES {
        let Some((bytes, version)) =
            store::get_raw_versioned(&mut config, crate::relay::agents::REGISTRY_CONFIG).await?
        else {
            break;
        };
        // Strict on the mutation path, as in `create_agent`: never rewrite a
        // registry we could not read.
        let mut personas = crate::relay::agents::Registry::parse(&bytes)?.personas;
        let before = personas.len();
        personas.retain(|p| p.agent != agent);
        if personas.len() == before {
            break; // not registered: removal is already true
        }
        let bytes = registry_bytes(&personas)?;
        match config
            .put_if_version(crate::relay::agents::REGISTRY_CONFIG, &bytes, Some(version))
            .await
            .map_err(storage)?
        {
            Ok(_) => {
                removed = true;
                break;
            }
            Err(_) => continue,
        }
    }
    // The key goes AFTER the registry entry: a persona that is still
    // dispatchable must still be able to sign.
    //
    // The OWNERSHIP row deliberately stays. A persona's key is derived from
    // `(relay identity, agent name)`, so deleting the claim would let ANY
    // member re-create this name, re-derive this same key, and post as the
    // retired teammate's pubkey — under which that teammate's entire message
    // history already sits. Clients render by pubkey, so that is impersonation,
    // not name reuse. Keeping the row makes retirement a reservation: the
    // original owner (or a moderator/operator) can recreate it, which is the
    // legitimate case, and nobody else can.
    if removed {
        // A failed keyring delete must not be discarded: the registry would say
        // "retired" while the persona's signing key silently lives on — revoked
        // key material with no expiry and no operator signal. Retirement still
        // succeeds (the registry row is already gone, so the persona cannot
        // dispatch), but the leftover key is a real security condition someone
        // has to see and clean up.
        if let Err(e) = store::delete_key(&mut config, &keys::keyring(community, &agent)).await {
            tracing::warn!(
                "agent '{agent}' retired but its keyring row was NOT deleted \
                 (revoked signing key persists in the store until removed): {e}"
            );
        }
    }
    serde_json::to_vec(&serde_json::json!({ "ok": true, "agent": agent, "removed": removed }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `GET /buzz/api/agent-harnesses` — which agent backends this workspace has.
///
/// buzz's desktop client answers this by probing `PATH` for CLI harnesses,
/// which is the right question for a laptop and the wrong one for a hosted
/// workspace: nothing a browser tab can see tells it whether this deployment
/// has an agent runtime behind it. The relay knows, because the runtime is
/// reached through the relay's own bindings, so the relay is what says so.
///
/// Two sources, in order:
///
/// 1. The operator's `buzz-agent-harnesses` document, when set. That is how a
///    deployment declares a backend the relay cannot detect on its own — a
///    Fabric ingress, or an A2A agent card — including its endpoint.
/// 2. Otherwise the wiring: Ferrite Cloud is `available` exactly when the
///    turn pipeline's queue is bound (the same condition the composition
///    root uses to spawn it), and the backends that need an endpoint are
///    reported as `not_installed` with the reason, never as
///    available-but-broken.
///
/// The reply is deliberately open: a client renders whatever rows it gets, so
/// adding a backend is an operator write, not a client release.
pub(super) async fn agent_harnesses(ctx: &AppContext) -> Result<Vec<u8>> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    if let Ok(Some(bytes)) = config.get(AGENT_HARNESSES_CONFIG).await {
        // Shape is enforced on the way OUT as well as on the way in: the
        // operator route only ever stores a validated array, but this store
        // key predates the operator plane, so a document seeded by any other
        // path (a gate, a torn write) must not be echoed verbatim — a scalar
        // here renders as zero harnesses in every client (`harnessCatalog`'s
        // Array.isArray guard) with nothing logged anywhere.
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(declared @ Value::Array(_)) => {
                return serde_json::to_vec(&serde_json::json!({ "harnesses": declared }))
                    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")));
            }
            _ => tracing::warn!(
                "the buzz-agent-harnesses document is not a JSON array; serving the wired default"
            ),
        }
    }

    let cloud_wired = ctx.queue(crate::agents::turns::TURNS).is_ok();
    let body = serde_json::json!({
        "harnesses": [
            {
                "id": "ferrite-cloud",
                "label": "Ferrite Cloud",
                "kind": "cloud",
                "availability": if cloud_wired { "available" } else { "not_installed" },
                "auth_status": { "status": "not_applicable" },
                "endpoint": null,
                "install_hint": if cloud_wired {
                    ""
                } else {
                    "This deployment has no agent-turns queue bound."
                },
            },
            {
                "id": "fabric",
                "label": "Fabric",
                "kind": "remote",
                "availability": "not_installed",
                "auth_status": { "status": "logged_out" },
                "endpoint": null,
                "install_hint": "Declare a Fabric endpoint in buzz-agent-harnesses to use it.",
            },
            {
                "id": "a2a",
                "label": "A2A agent",
                "kind": "a2a",
                "availability": "not_installed",
                "auth_status": { "status": "logged_out" },
                "endpoint": null,
                "install_hint": "Add an A2A agent-card URL in buzz-agent-harnesses to use it.",
            },
        ]
    });
    serde_json::to_vec(&body).map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}
