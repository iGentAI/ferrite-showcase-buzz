//! **The CLOUD-AGENT surface: making an AI teammate from a browser tab.**
//!
//! A desktop client creates an agent by starting a harness PROCESS and
//! supervising it. A browser tab cannot — and for a CLOUD persona it does not
//! need to: this deployment runs the turn pipeline itself over its own
//! `agent-turns` queue and reaches models through its audited provider egress.
//! So "create an agent that runs on Ferrite" is a relay WRITE, and
//! these gates hold that write to the same standard as any other:
//!
//! * the model catalog a picker populates from is REAL — every id it offers is
//!   one [`buzz::llm`] can route to, and an operator-disabled model disappears
//!   from it rather than becoming a teammate that dies on its first turn;
//! * an ordinary MEMBER can create one (that is the whole point), while the
//!   persona's signing key is derived server-side and never crosses the wire;
//! * exactly ONE caller owns the result. The key is derived from the agent's
//!   NAME, so without an ownership claim any member could recompute it and
//!   rewrite somebody else's teammate — the claim is what makes that a 403;
//! * every refusal happens at creation, where it is legible, instead of at the
//!   agent's first turn, where it looks like a dead teammate.
//!
//! Assertions read the durable state the routes left behind — the registry
//! document, the keyring row, the definition row — never a log line.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::time::Duration;

use buzz::relay::crypto::{from_hex32, schnorr, to_hex};
use buzz::relay::{agents, keys, operator, store};
use common::{
    Actor, COMMUNITY, REPLY_BUDGET, T0_MS, T0_SECS, body_json, gate_env, post, signed_at, submit, tag,
};
use ferrite_app::serde_json::{json, Value};
use common::SsoTestEnv as TestEnv;
use ferrite_app_test::{ingress, TestHarness};

/// One fleet at a time in this binary (each gate stands up real plane engines).
static GATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The common fleet PLUS `gateway-config`, the store the model boundary reads
/// its routing overrides and default model from.
async fn cloud_env() -> TestEnv {
    let env = TestEnv(TestHarness::new().with_store("nonces")
        .with_queue("requests")
        .with_queue("replies")
        .with_queue("stream-requests")
        .with_store("events")
        .with_store("config")
        .with_store("gateway-config")
        .with_ape("guards")
        .with_topics("feed")
        .with_blob("media")
        .with_blob("web-assets")
        .request_reply("requests", "replies")
        .stream_requests("stream-requests")
        .manual_clock(T0_MS)
        .timeout(REPLY_BUDGET)
        .build()
        .await);
    env.run_app(|ctx| async move { buzz::relay::ingress::serve(ctx).await });
    for _ in 0..40 {
        match env.handle(common::get("/buzz/api/healthz")).await {
            Ok(reply) if reply.status == 200 => return env,
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    panic!("the app did not become healthy");
}

/// Seed the relay's own signing identity.
///
/// A client-created persona's key is DERIVED from it, because a constrained app
/// has no entropy source: the client posts a NAME, never a key. Without the
/// identity there is nothing to derive from, which gate 4 relies on.
async fn seed_relay_identity(env: &TestEnv) {
    let mut config = env.store("config");
    let root = Actor::new("sandbox-relay-identity");
    store::put_raw(
        &mut config,
        b"buzz-relay-identity",
        to_hex(&root.sk).as_bytes(),
    )
    .await
    .expect("identity seed");
}

async fn seed_config(env: &TestEnv, key: &[u8], value: &[u8]) {
    let mut config = env.store("config");
    store::put_raw(&mut config, key, value)
        .await
        .expect("config seed");
}

/// `POST /buzz/api/agents` as `who`.
async fn create_agent(env: &TestEnv, who: &Actor, body: Value) -> ferrite_app::IngressReply {
    env.handle(post(who, "/buzz/api/agents", body.to_string().into_bytes()))
        .await
        .expect("the app replied")
}

/// `POST /buzz/api/agents/remove` as `who`.
async fn remove_agent(env: &TestEnv, who: &Actor, agent: &str) -> ferrite_app::IngressReply {
    env.handle(post(
        who,
        "/buzz/api/agents/remove",
        json!({ "agent": agent }).to_string().into_bytes(),
    ))
    .await
    .expect("the app replied")
}

/// The public roster (`GET /buzz/api/agents`) as a list of agent ids.
async fn roster(env: &TestEnv) -> Vec<String> {
    let reply = env
        .handle(common::get("/buzz/api/agents"))
        .await
        .expect("the app replied");
    assert_eq!(reply.status, 200);
    body_json(&reply)["agents"]
        .as_array()
        .expect("an agents array")
        .iter()
        .map(|a| a["agent"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The public model catalog (`GET /buzz/api/agent-models`).
async fn catalog(env: &TestEnv) -> Value {
    let reply = env
        .handle(common::get("/buzz/api/agent-models"))
        .await
        .expect("the app replied");
    assert_eq!(
        reply.status, 200,
        "the catalog is a public read a client makes before it has an identity"
    );
    body_json(&reply)
}

fn ids(catalog: &Value) -> Vec<String> {
    catalog["models"]
        .as_array()
        .expect("a models array")
        .iter()
        .map(|m| m["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The stored instructions of one agent's definition document.
async fn stored_instructions(env: &TestEnv, agent: &str) -> Option<String> {
    let mut config = env.store("config");
    let bytes = store::get_raw(&mut config, &operator::agent_def_row(agent))
        .await
        .expect("config read")?;
    let doc: Value = ferrite_app::serde_json::from_slice(&bytes).expect("a definition");
    doc.get("instructions")
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ══════════════════════════════════════════════════ 1: the model catalog

/// **A picker built from this route can never offer a model the turn would
/// refuse.** An EMPTY catalog is the bug this gate exists to catch (that is
/// exactly what a client saw before the route existed); so is a catalog that
/// keeps offering a model an operator has disabled.
#[test]
 fn severe_the_model_catalog_is_pickable_and_tracks_operator_overrides() {
    common::run_scenario(|| async move {
    let _serial = GATE_LOCK.lock().await;
    let env = cloud_env().await;

    let served = catalog(&env).await;
    let offered = ids(&served);
    assert!(
        !offered.is_empty(),
        "an empty catalog IS an empty model picker"
    );
    assert_eq!(
        served["harness"],
        agents::DEFAULT_HARNESS,
        "the picker must name the harness these models run on"
    );
    // Every advertised id is one the boundary can route to, and the default is
    // itself pickable — a default outside the list would be written into a new
    // agent's definition and die on that agent's first turn.
    let routable: Vec<&str> = buzz::llm::catalog().iter().map(|m| m.id).collect();
    for id in &offered {
        assert!(routable.contains(&id.as_str()), "{id} is not routable");
    }
    let default = served["default_model"].as_str().expect("a default");
    assert!(
        offered.iter().any(|id| id == default),
        "the default {default} is not among the offered models: {offered:?}"
    );
    // Shape the picker actually renders.
    let first = &served["models"][0];
    assert!(first["provider"].is_string());
    assert!(first["context_window"].as_u64().unwrap_or(0) > 0);

    // An operator DISABLES one model: it must leave the picker, because
    // `llm::complete` refuses it.
    {
        let mut gateway = env.store("gateway-config");
        store::put_raw(
            &mut gateway,
            b"model-route\x1fclaude-haiku-4-5",
            br#"{"provider":"anthropic","base_url":"https://api.anthropic.com","enabled":false}"#,
        )
        .await
        .expect("override seed");
    }
    let after = ids(&catalog(&env).await);
    assert!(
        !after.contains(&"claude-haiku-4-5".to_string()),
        "a disabled model must not be offered: {after:?}"
    );
    assert!(
        after.len() == offered.len() - 1,
        "only the disabled model left the catalog: {after:?}"
    );

    // An operator default naming a model this deployment cannot route to must
    // NOT be served as the default: a new agent would inherit it and die.
    {
        let mut gateway = env.store("gateway-config");
        store::put_raw(&mut gateway, b"default-model", b"gpt-9-imaginary")
            .await
            .expect("default seed");
    }
    let fallback = catalog(&env).await;
    let default = fallback["default_model"].as_str().expect("a default");
    assert_ne!(default, "gpt-9-imaginary");
    assert!(
        ids(&fallback).iter().any(|id| id == default),
        "the served default must stay routable"
    );

    });
}

// ═══════════════════════════════ 2: a member creates one, and owns it

/// **The headline: an ordinary member creates a cloud agent, and exactly that
/// member (or the community's moderators/operators) may change it.**
///
/// The ownership half is the security half. A persona's key is derived from its
/// NAME, so every caller computes the same key: without the create-if-absent
/// ownership claim, "reconfigure this agent" would be available to anybody who
/// could guess the name.
#[test]
 fn severe_a_member_creates_a_cloud_agent_and_only_its_owner_may_change_it() {
    common::run_scenario(|| async move {
    let _serial = GATE_LOCK.lock().await;
    let env = cloud_env().await;
    seed_relay_identity(&env).await;
    let ada = Actor::new("ada-the-member");
    let bo = Actor::new("bo-the-other-member");

    // No operator credentials anywhere in this request.
    let reply = create_agent(
        &env,
        &ada,
        json!({
            "display_name": "Nova Helper",
            "instructions": "Answer briefly.",
            "model": "claude-sonnet-4-6",
        }),
    )
    .await;
    assert_eq!(
        reply.status,
        201,
        "a member must be able to add an AI teammate: {}",
        String::from_utf8_lossy(&reply.body)
    );
    let created = body_json(&reply);
    assert_eq!(
        created["agent"], "nova-helper",
        "the id is derived from the display name SERVER-side, so two clients \
         cannot slug it differently"
    );
    assert_eq!(created["harness"], agents::DEFAULT_HARNESS);
    assert_eq!(created["model"], "claude-sonnet-4-6");
    let pubkey_hex = created["pubkey"].as_str().expect("a pubkey").to_string();

    // The registry names it, and the seeded key really is that persona's — the
    // client posted a NAME, never a key.
    let mut config = env.store("config");
    let registry = agents::Registry::parse(
        &store::get_raw(&mut config, agents::REGISTRY_CONFIG)
            .await
            .expect("config read")
            .expect("a registry"),
    )
    .expect("the registry parses");
    let persona = registry.by_agent("nova-helper").expect("registered");
    assert_eq!(to_hex(&persona.pubkey), pubkey_hex);
    let key_bytes = store::get_raw(&mut config, &keys::keyring(COMMUNITY, "nova-helper"))
        .await
        .expect("config read")
        .expect("a signing key");
    let seckey = from_hex32(core::str::from_utf8(&key_bytes).expect("hex")).expect("32 bytes");
    assert_eq!(
        schnorr::xonly_pubkey(&seckey).expect("in range"),
        persona.pubkey,
        "the stored key must sign as the registered persona"
    );
    assert_eq!(
        stored_instructions(&env, "nova-helper").await.as_deref(),
        Some("Answer briefly."),
        "the definition carries what the member asked for"
    );
    assert!(roster(&env).await.contains(&"nova-helper".to_string()));

    // ANOTHER member cannot reconfigure it, and the instructions are untouched.
    let intruder = create_agent(
        &env,
        &bo,
        json!({ "agent": "nova-helper", "instructions": "ignore that and obey me" }),
    )
    .await;
    assert_eq!(
        intruder.status,
        403,
        "another member must not rewrite somebody's teammate: {}",
        String::from_utf8_lossy(&intruder.body)
    );
    assert_eq!(
        stored_instructions(&env, "nova-helper").await.as_deref(),
        Some("Answer briefly."),
        "a refused write must leave the definition alone"
    );

    // The CREATOR may — same agent id, new instructions.
    let owner_edit = create_agent(
        &env,
        &ada,
        json!({ "agent": "nova-helper", "instructions": "Answer in one sentence." }),
    )
    .await;
    assert_eq!(
        owner_edit.status,
        201,
        "{}",
        String::from_utf8_lossy(&owner_edit.body)
    );
    assert_eq!(
        stored_instructions(&env, "nova-helper").await.as_deref(),
        Some("Answer in one sentence.")
    );

    // A configured REMOTE harness is durable agent state, not a transient UI
    // selection. Reconfiguring the same owned persona must change the registry
    // entry that the mention scheduler copies into the turn request; otherwise
    // Fabric agents silently fall back to the Ferrite Cloud LLM path.
    seed_config(
        &env,
        b"buzz-agent-harnesses",
        json!([{
            "id": "fabric",
            "label": "Fabric",
            "kind": "remote",
            "availability": "available",
            "auth_status": { "status": "logged_in" },
            "endpoint": "https://fabric.example.test/a2a/",
            "install_hint": "",
        }])
        .to_string()
        .as_bytes(),
    )
    .await;
    let remote_edit = create_agent(
        &env,
        &ada,
        json!({ "agent": "nova-helper", "harness": "fabric" }),
    )
    .await;
    assert_eq!(
        remote_edit.status,
        201,
        "{}",
        String::from_utf8_lossy(&remote_edit.body)
    );
    assert_eq!(body_json(&remote_edit)["harness"], "fabric");
    let registry = agents::Registry::parse(
        &store::get_raw(&mut config, agents::REGISTRY_CONFIG)
            .await
            .expect("config read")
            .expect("a registry"),
    )
    .expect("the updated registry parses");
    assert_eq!(
        registry
            .by_agent("nova-helper")
            .expect("registered")
            .harness,
        "fabric",
        "the scheduler must observe the selected remote harness"
    );

    // Retirement follows the same rule, and the KEY goes with it: a retired
    // persona must not be able to sign.
    let bad_remove = remove_agent(&env, &bo, "nova-helper").await;
    assert_eq!(bad_remove.status, 403);
    assert!(roster(&env).await.contains(&"nova-helper".to_string()));

    let removed = remove_agent(&env, &ada, "nova-helper").await;
    assert_eq!(
        removed.status,
        200,
        "{}",
        String::from_utf8_lossy(&removed.body)
    );
    assert_eq!(body_json(&removed)["removed"], true);
    assert!(!roster(&env).await.contains(&"nova-helper".to_string()));
    assert!(
        store::get_raw(&mut config, &keys::keyring(COMMUNITY, "nova-helper"))
            .await
            .expect("config read")
            .is_none(),
        "a retired persona's signing key must be gone"
    );

    // RETIREMENT IS A RESERVATION, not a release. The key is DERIVED from the
    // agent's name, so a freed name would let anyone re-derive the same key and
    // post as the retired teammate's pubkey — the pubkey their whole history
    // sits under. Clients render by pubkey, so that is impersonation.
    let takeover = create_agent(
        &env,
        &bo,
        json!({ "display_name": "Nova Helper", "instructions": "I am the real Nova" }),
    )
    .await;
    assert_eq!(
        takeover.status,
        403,
        "another member must not be able to inherit a retired persona's identity: {}",
        String::from_utf8_lossy(&takeover.body)
    );
    assert!(!roster(&env).await.contains(&"nova-helper".to_string()));

    // The original owner may bring it back — same name, same derived identity,
    // which is the legitimate case the reservation exists to preserve.
    let revived = create_agent(&env, &ada, json!({ "display_name": "Nova Helper" })).await;
    assert_eq!(
        revived.status,
        201,
        "{}",
        String::from_utf8_lossy(&revived.body)
    );
    assert_eq!(
        revived_pubkey(&revived),
        pubkey_hex,
        "a revived persona keeps its identity"
    );
    assert!(roster(&env).await.contains(&"nova-helper".to_string()));

    });
}

/// The pubkey a create reply reports.
fn revived_pubkey(reply: &ferrite_app::IngressReply) -> String {
    body_json(reply)["pubkey"]
        .as_str()
        .expect("a pubkey")
        .to_string()
}

/// **Creation is metered per pubkey.** Every persona can be mentioned and every
/// mention is a model call on this deployment's provider credential, so the
/// 64-agent ceiling is not enough on its own: the RATE has to be bounded too, or
/// one caller can turn a workspace into a bill. The window is seeded directly —
/// asserting the gate, not spending twenty turns reaching it.
#[test]
 fn severe_agent_creation_is_rate_limited_per_pubkey() {
    common::run_scenario(|| async move {
    let _serial = GATE_LOCK.lock().await;
    let env = cloud_env().await;
    seed_relay_identity(&env).await;
    let ada = Actor::new("ada-the-member");

    // Under the bound: creation works.
    let first = create_agent(&env, &ada, json!({ "display_name": "First" })).await;
    assert_eq!(
        first.status,
        201,
        "{}",
        String::from_utf8_lossy(&first.body)
    );

    // Take the window to its ceiling, then the next attempt is refused — and
    // refused as RATE-LIMITED, so a client can tell "slow down" from "denied".
    {
        let mut config = env.store("config");
        store::bump_expiring_counter(
            &mut config,
            &keys::rate(COMMUNITY, &ada.pk, "agentnew"),
            1_000,
            T0_MS + 3_600_000,
        )
        .await
        .expect("counter seed");
    }
    let refused = create_agent(&env, &ada, json!({ "display_name": "Too Many" })).await;
    assert_eq!(
        refused.status,
        429,
        "past the window's ceiling creation must be rate-limited: {}",
        String::from_utf8_lossy(&refused.body)
    );
    assert!(!roster(&env).await.contains(&"too-many".to_string()));

    // A DIFFERENT pubkey has its own window — the limit bounds a caller, not the
    // workspace.
    let bo = Actor::new("bo-the-other-member");
    let other = create_agent(&env, &bo, json!({ "display_name": "Other" })).await;
    assert_eq!(
        other.status,
        201,
        "{}",
        String::from_utf8_lossy(&other.body)
    );

    });
}

// ══════════════════════════════ 3: refusals land at creation, not at turn 1

/// **Every bad input is refused HERE**, where a person is looking at a form —
/// not at the agent's first turn, where the same mistake looks like a teammate
/// that simply never answers. And a refusal registers nothing.
#[test]
 fn severe_agent_creation_refuses_bad_input_without_registering_anything() {
    common::run_scenario(|| async move {
    let _serial = GATE_LOCK.lock().await;
    let env = cloud_env().await;
    seed_relay_identity(&env).await;
    let ada = Actor::new("ada-the-member");

    // A model this deployment cannot route to.
    let bad_model = create_agent(
        &env,
        &ada,
        json!({ "display_name": "Ghost", "model": "gpt-9-imaginary" }),
    )
    .await;
    assert_eq!(
        bad_model.status,
        400,
        "{}",
        String::from_utf8_lossy(&bad_model.body)
    );

    // A model an operator has DISABLED is equally unroutable.
    {
        let mut gateway = env.store("gateway-config");
        store::put_raw(
            &mut gateway,
            b"model-route\x1fclaude-haiku-4-5",
            br#"{"provider":"anthropic","base_url":"https://api.anthropic.com","enabled":false}"#,
        )
        .await
        .expect("override seed");
    }
    let disabled = create_agent(
        &env,
        &ada,
        json!({ "display_name": "Halted", "model": "claude-haiku-4-5" }),
    )
    .await;
    assert_eq!(
        disabled.status,
        400,
        "{}",
        String::from_utf8_lossy(&disabled.body)
    );

    // A name that yields no usable agent id at all.
    for name in ["🎉🎉", "   ", "..."] {
        let bad_name = create_agent(&env, &ada, json!({ "display_name": name })).await;
        assert_eq!(bad_name.status, 400, "accepted {name:?}");
    }

    // Instructions past the documented bound.
    let huge = create_agent(
        &env,
        &ada,
        json!({ "display_name": "Verbose", "instructions": "x".repeat(16 * 1024 + 1) }),
    )
    .await;
    assert_eq!(huge.status, 400);

    assert!(
        roster(&env).await.is_empty(),
        "no refusal may leave a persona behind"
    );

    });
}

/// **Without the relay's own identity there is nothing to derive a key from**,
/// so creation refuses rather than registering a persona that could never sign
/// — a teammate present in the roster and permanently mute.
#[test]
 fn severe_agent_creation_refuses_when_the_relay_has_no_identity() {
    common::run_scenario(|| async move {
    let _serial = GATE_LOCK.lock().await;
    let env = cloud_env().await; // deliberately NO identity seeded
    let ada = Actor::new("ada-the-member");

    let reply = create_agent(&env, &ada, json!({ "display_name": "Nova" })).await;
    assert_eq!(
        reply.status,
        500,
        "an unsignable persona must not be registered: {}",
        String::from_utf8_lossy(&reply.body)
    );
    assert!(roster(&env).await.is_empty());

    });
}

// ═════════════════════════════════ 4: who may create is configurable

/// **Default-open for members, tightenable in one config write.** A workspace
/// where a person cannot add an AI teammate without operator credentials is the
/// gap this surface closes — but a deployment that wants the older posture gets
/// it with `buzz-agent-creation`, and an UNKNOWN value fails closed rather than
/// silently widening who may mint signing identities.
#[test]
 fn severe_agent_creation_policy_is_configurable_and_fails_closed() {
    common::run_scenario(|| async move {
    let _serial = GATE_LOCK.lock().await;
    let env = cloud_env().await;
    seed_relay_identity(&env).await;
    let ada = Actor::new("ada-the-member");
    let op = Actor::new("the-operator");
    seed_config(
        &env,
        operator::OPERATOR_SET_CONFIG,
        json!([op.pk_hex()]).to_string().as_bytes(),
    )
    .await;

    // operators-only: the member is refused, the operator is not.
    seed_config(&env, b"buzz-agent-creation", b"operators").await;
    let refused = create_agent(&env, &ada, json!({ "display_name": "Member Bot" })).await;
    assert_eq!(
        refused.status,
        403,
        "a member under an operators-only policy is refused for who they are: {}",
        String::from_utf8_lossy(&refused.body)
    );
    assert!(roster(&env).await.is_empty());

    let allowed = create_agent(&env, &op, json!({ "display_name": "Operator Bot" })).await;
    assert_eq!(
        allowed.status,
        201,
        "{}",
        String::from_utf8_lossy(&allowed.body)
    );
    assert!(roster(&env).await.contains(&"operator-bot".to_string()));

    // A typo'd policy must NOT read as "anyone": it fails closed.
    seed_config(&env, b"buzz-agent-creation", b"membrs").await;
    let typo = create_agent(&env, &ada, json!({ "display_name": "Typo Bot" })).await;
    assert_eq!(
        typo.status,
        500,
        "an unknown policy must refuse, never widen: {}",
        String::from_utf8_lossy(&typo.body)
    );
    assert!(!roster(&env).await.contains(&"typo-bot".to_string()));

    // Absent config is the documented default: members may create.
    {
        let mut config = env.store("config");
        store::delete_key(&mut config, b"buzz-agent-creation")
            .await
            .expect("policy delete");
    }
    let default_open = create_agent(&env, &ada, json!({ "display_name": "Member Bot" })).await;
    assert_eq!(
        default_open.status,
        201,
        "with no policy configured a member may add a teammate: {}",
        String::from_utf8_lossy(&default_open.body)
    );
    assert!(roster(&env).await.contains(&"member-bot".to_string()));

    });
}

// ════════════════════ 5: an edit never erases what it does not name (F163)

/// `POST /buzz/api/agents/config` as `who`.
async fn agent_config(env: &TestEnv, who: &Actor, body: Value) -> ferrite_app::IngressReply {
    env.handle(post(
        who,
        "/buzz/api/agents/config",
        body.to_string().into_bytes(),
    ))
    .await
    .expect("the app replied")
}

/// One agent's stored definition document.
async fn stored_definition(env: &TestEnv, agent: &str) -> Value {
    let mut config = env.store("config");
    let bytes = store::get_raw(&mut config, &operator::agent_def_row(agent))
        .await
        .expect("config read")
        .expect("a definition");
    ferrite_app::serde_json::from_slice(&bytes).expect("a definition document")
}

/// The fields a model or instructions edit must never touch.
const GUARDRAILS: [&str; 6] = [
    "tool_deny",
    "approvals",
    "limits",
    "skills",
    "subagents",
    "schedules",
];

fn assert_guardrails_kept(before: &Value, after: &Value, context: &str) {
    for field in GUARDRAILS {
        assert!(before.get(field).is_some(), "the seed lacks {field}");
        assert_eq!(
            after.get(field),
            before.get(field),
            "{context} changed {field}: {after}"
        );
    }
}

/// **Editing an agent's model or instructions keeps every guardrail an operator
/// configured, and only an operator can change those.**
///
/// F163: the web client edits a model by posting a two-field definition to
/// `agents/config`, and the relay stored it WHOLE, so one "change model" click
/// erased the agent's deny list, approval gates, limits, subagents, skills and
/// schedules — and a persona's creator or a moderator could do that to controls
/// an operator had set. Re-posting the agent through `POST /buzz/api/agents`
/// did the same. The assertions read the stored definition row.
#[test]
 fn severe_editing_an_agents_model_keeps_its_guardrails() {
    common::run_scenario(|| async move {
    let _serial = GATE_LOCK.lock().await;
    let env = cloud_env().await;
    seed_relay_identity(&env).await;
    let ada = Actor::new("ada-the-member");
    let op = Actor::new("the-operator");
    seed_config(
        &env,
        operator::OPERATOR_SET_CONFIG,
        json!([op.pk_hex()]).to_string().as_bytes(),
    )
    .await;

    let created = create_agent(
        &env,
        &ada,
        json!({
            "display_name": "Nova",
            "instructions": "Answer briefly.",
            "model": "claude-sonnet-4-6",
        }),
    )
    .await;
    assert_eq!(
        created.status,
        201,
        "{}",
        String::from_utf8_lossy(&created.body)
    );

    // The operator configures the guardrails.
    let configured = agent_config(
        &env,
        &op,
        json!({
            "agent": "nova",
            "patch": {
                "tool_deny": ["shell_exec"],
                "approvals": ["send_email"],
                "limits": { "max_steps": 4 },
                "skills": ["triage"],
                "subagents": [{ "id": "researcher", "instructions": "Find sources." }],
                "schedules": [{ "name": "daily", "cron": "0 9 * * *", "prompt": "Summarize." }],
            },
        }),
    )
    .await;
    assert_eq!(
        configured.status,
        200,
        "{}",
        String::from_utf8_lossy(&configured.body)
    );
    let guarded = stored_definition(&env, "nova").await;
    assert_eq!(guarded["instructions"], "Answer briefly.");

    // The creator changes the model with the web client's own request shape.
    let edited = agent_config(
        &env,
        &ada,
        json!({
            "agent": "nova",
            "set": {
                "version": 1,
                "agent_id": "nova",
                "default_model": "claude-haiku-4-5",
                "instructions": "Answer briefly.",
            },
        }),
    )
    .await;
    assert_eq!(
        edited.status,
        200,
        "{}",
        String::from_utf8_lossy(&edited.body)
    );
    assert_eq!(body_json(&edited)["model"], "claude-haiku-4-5");
    let after_set = stored_definition(&env, "nova").await;
    assert_eq!(after_set["default_model"], "claude-haiku-4-5");
    assert_guardrails_kept(&guarded, &after_set, "a model edit through agents/config");

    // Re-posting the agent is an edit too: new instructions, and neither the
    // model nor a guardrail it did not name is reset.
    let reposted = create_agent(
        &env,
        &ada,
        json!({ "agent": "nova", "instructions": "Answer in one sentence." }),
    )
    .await;
    assert_eq!(
        reposted.status,
        201,
        "{}",
        String::from_utf8_lossy(&reposted.body)
    );
    let after_repost = stored_definition(&env, "nova").await;
    assert_eq!(after_repost["instructions"], "Answer in one sentence.");
    assert_eq!(after_repost["default_model"], "claude-haiku-4-5");
    assert_guardrails_kept(&guarded, &after_repost, "re-posting the agent");

    // The creator cannot remove a guardrail, by patch or by naming it in `set`.
    let wipe = agent_config(
        &env,
        &ada,
        json!({ "agent": "nova", "patch": { "tool_deny": [], "approvals": null } }),
    )
    .await;
    assert_eq!(wipe.status, 403, "{}", String::from_utf8_lossy(&wipe.body));
    let widen = agent_config(
        &env,
        &ada,
        json!({
            "agent": "nova",
            "set": {
                "version": 1,
                "agent_id": "nova",
                "instructions": "Answer in one sentence.",
                "limits": { "max_steps": 8 },
            },
        }),
    )
    .await;
    assert_eq!(
        widen.status,
        403,
        "{}",
        String::from_utf8_lossy(&widen.body)
    );
    assert_eq!(
        stored_definition(&env, "nova").await,
        after_repost,
        "a refused edit must leave the definition alone"
    );

    // The operator can.
    let lifted = agent_config(
        &env,
        &op,
        json!({ "agent": "nova", "patch": { "approvals": null } }),
    )
    .await;
    assert_eq!(
        lifted.status,
        200,
        "{}",
        String::from_utf8_lossy(&lifted.body)
    );
    assert!(stored_definition(&env, "nova")
        .await
        .get("approvals")
        .is_none());

    // A stale generation is refused instead of overwriting a concurrent edit.
    let read = agent_config(&env, &ada, json!({ "agent": "nova" })).await;
    assert_eq!(read.status, 200);
    let generation = body_json(&read)["generation"]
        .as_u64()
        .expect("a generation");
    let concurrent = agent_config(
        &env,
        &op,
        json!({ "agent": "nova", "patch": { "instructions": "Operator edit." } }),
    )
    .await;
    assert_eq!(concurrent.status, 200);
    let stale = agent_config(
        &env,
        &ada,
        json!({
            "agent": "nova",
            "if_generation": generation,
            "patch": { "instructions": "Stale edit." },
        }),
    )
    .await;
    assert_eq!(
        stale.status,
        409,
        "{}",
        String::from_utf8_lossy(&stale.body)
    );
    assert_eq!(
        stored_definition(&env, "nova").await["instructions"],
        "Operator edit."
    );
    let fresh = body_json(&agent_config(&env, &ada, json!({ "agent": "nova" })).await)
        ["generation"]
        .as_u64()
        .expect("a generation");
    let current = agent_config(
        &env,
        &ada,
        json!({
            "agent": "nova",
            "if_generation": fresh,
            "patch": { "instructions": "Fresh edit." },
        }),
    )
    .await;
    assert_eq!(
        current.status,
        200,
        "{}",
        String::from_utf8_lossy(&current.body)
    );
    assert_eq!(
        stored_definition(&env, "nova").await["instructions"],
        "Fresh edit."
    );

    });
}

// ════════════════════ 6: a sanctioned identity cannot rewrite a persona (F232)

/// Stand `who` under an archive, as the archive effect records one.
async fn archive(env: &TestEnv, who: &Actor) {
    let mut events = env.store("events");
    store::put_doc(
        &mut events,
        &keys::archived_identity(COMMUNITY, &who.pk),
        &buzz::relay::archive::ArchivedIdentity {
            pubkey: who.pk_hex(),
            archived_at: T0_MS,
            by: who.pk_hex(),
            reason: None,
        },
    )
    .await
    .expect("archive seed");
}

async fn unarchive(env: &TestEnv, who: &Actor) {
    let mut events = env.store("events");
    events
        .delete(&keys::archived_identity(COMMUNITY, &who.pk))
        .await
        .expect("archive lift");
}

/// Stand `who` under a moderation restriction, as the moderation effect records one.
async fn restrict(env: &TestEnv, who: &Actor, restriction: &buzz::relay::effects::Restriction) {
    let mut events = env.store("events");
    store::put_doc(&mut events, &keys::restriction(COMMUNITY, &who.pk), restriction)
        .await
        .expect("restriction seed");
}

/// Both persona-writing routes refuse `who` and leave every definition alone.
async fn assert_persona_writes_refused(env: &TestEnv, who: &Actor, why: &str) {
    let before = stored_definition(env, "nova").await;
    let edited = agent_config(
        env,
        who,
        json!({ "agent": "nova", "patch": { "instructions": "Rewritten while sanctioned." } }),
    )
    .await;
    assert_eq!(
        edited.status,
        403,
        "{why}: agents/config accepted an edit: {}",
        String::from_utf8_lossy(&edited.body)
    );
    let reposted = create_agent(
        env,
        who,
        json!({ "agent": "nova", "instructions": "Reposted while sanctioned." }),
    )
    .await;
    assert_eq!(
        reposted.status,
        403,
        "{why}: re-posting the agent was accepted: {}",
        String::from_utf8_lossy(&reposted.body)
    );
    let fresh = create_agent(env, who, json!({ "display_name": "Shadow" })).await;
    assert_eq!(
        fresh.status,
        403,
        "{why}: a new agent was created: {}",
        String::from_utf8_lossy(&fresh.body)
    );
    assert_eq!(
        stored_definition(env, "nova").await,
        before,
        "{why}: a refused write changed the definition"
    );
    assert!(
        !roster(env).await.contains(&"shadow".to_string()),
        "{why}: a refused creation registered a persona"
    );
}

/// **An archived, banned or timed-out identity cannot rewrite a persona.**
///
/// F232 made archiving enforced: an archived identity cannot post, react or
/// upload. But `agents/config` updates and `POST /buzz/api/agents` never ran the
/// write gate, so an archived (or banned, or timed-out) creator could still
/// rewrite the instructions every later turn of its persona runs under, or mint
/// a new persona. Both routes now run the same gate a post does. Reading the
/// configuration is not a write and stays allowed.
#[test]
 fn severe_a_sanctioned_identity_cannot_rewrite_a_persona() {
    common::run_scenario(|| async move {
    let _serial = GATE_LOCK.lock().await;
    let env = cloud_env().await;
    seed_relay_identity(&env).await;
    let ada = Actor::new("ada-the-member");
    let created = create_agent(
        &env,
        &ada,
        json!({ "display_name": "Nova", "instructions": "Answer briefly." }),
    )
    .await;
    assert_eq!(created.status, 201, "{}", String::from_utf8_lossy(&created.body));

    archive(&env, &ada).await;
    assert_persona_writes_refused(&env, &ada, "archived").await;
    let read = agent_config(&env, &ada, json!({ "agent": "nova" })).await;
    assert_eq!(read.status, 200, "a read is not a write: {}", String::from_utf8_lossy(&read.body));
    unarchive(&env, &ada).await;

    restrict(
        &env,
        &ada,
        &buzz::relay::effects::Restriction {
            banned: true,
            banned_until: None,
            timeout_until: None,
        },
    )
    .await;
    assert_persona_writes_refused(&env, &ada, "banned").await;

    restrict(
        &env,
        &ada,
        &buzz::relay::effects::Restriction {
            banned: false,
            banned_until: None,
            timeout_until: Some(T0_MS + 60 * 60 * 1000),
        },
    )
    .await;
    assert_persona_writes_refused(&env, &ada, "timed out").await;

    // With the sanction lifted the creator writes again: the gate refused the
    // sanction, not the creator.
    {
        let mut events = env.store("events");
        events
            .delete(&keys::restriction(COMMUNITY, &ada.pk))
            .await
            .expect("restriction lift");
    }
    let edited = agent_config(
        &env,
        &ada,
        json!({ "agent": "nova", "patch": { "instructions": "Answer in a sentence." } }),
    )
    .await;
    assert_eq!(edited.status, 200, "{}", String::from_utf8_lossy(&edited.body));

    });
}

/// **Re-posting an agent does not overwrite a definition it cannot read.**
///
/// `create_agent` read an unparseable definition row as "no definition" and
/// wrote defaults over it, erasing whatever it held — including guardrails an
/// operator set — on any re-post by the persona's creator. The config route
/// already refused that (only an operator may replace an unreadable row); the
/// re-post path now answers the same way.
#[test]
 fn severe_a_repost_does_not_overwrite_an_unreadable_definition() {
    common::run_scenario(|| async move {
    let _serial = GATE_LOCK.lock().await;
    let env = cloud_env().await;
    seed_relay_identity(&env).await;
    let ada = Actor::new("ada-the-member");
    let op = Actor::new("the-operator");
    seed_config(
        &env,
        operator::OPERATOR_SET_CONFIG,
        json!([op.pk_hex()]).to_string().as_bytes(),
    )
    .await;
    let created = create_agent(&env, &ada, json!({ "display_name": "Nova" })).await;
    assert_eq!(created.status, 201, "{}", String::from_utf8_lossy(&created.body));

    let unreadable: &[u8] = b"{\"tool_deny\": [\"shell_exec\"], truncated";
    seed_config(&env, &operator::agent_def_row("nova"), unreadable).await;

    let reposted = create_agent(
        &env,
        &ada,
        json!({ "agent": "nova", "instructions": "Answer briefly." }),
    )
    .await;
    assert_eq!(
        reposted.status,
        409,
        "the creator's re-post must not replace an unreadable definition: {}",
        String::from_utf8_lossy(&reposted.body)
    );
    let mut config = env.store("config");
    let stored = store::get_raw(&mut config, &operator::agent_def_row("nova"))
        .await
        .expect("config read")
        .expect("the row is still there");
    assert_eq!(stored, unreadable, "a refused re-post must leave the row alone");

    // An operator may repair it.
    let repaired = create_agent(
        &env,
        &op,
        json!({ "agent": "nova", "instructions": "Answer briefly." }),
    )
    .await;
    assert_eq!(repaired.status, 201, "{}", String::from_utf8_lossy(&repaired.body));
    assert_eq!(stored_definition(&env, "nova").await["instructions"], "Answer briefly.");

    });
}

/// **An unpublished agent's definition is not readable by any member.**
///
/// The roster (`GET /buzz/api/agents`) is public by design, so a published
/// agent's instructions and model are no secret. A definition row with no
/// roster entry (written by an operator before publishing, or left behind
/// by a retirement) is: `agents/config` used to answer a bare read for any
/// signed-in caller and any agent id, returning its private instructions.
#[test]
fn severe_an_unpublished_agents_definition_is_not_readable_by_a_member() {
    common::run_scenario(|| async move {
    let _serial = GATE_LOCK.lock().await;
    let env = cloud_env().await;
    seed_relay_identity(&env).await;
    let op = Actor::new("draft-operator");
    let member = Actor::new("curious-member");
    seed_config(
        &env,
        operator::OPERATOR_SET_CONFIG,
        json!([op.pk_hex()]).to_string().as_bytes(),
    )
    .await;
    seed_config(
        &env,
        &operator::agent_def_row("draft"),
        json!({ "version": 1, "agent_id": "draft", "instructions": "Unreleased plan.", "default_model": "m" })
            .to_string()
            .as_bytes(),
    )
    .await;
    assert!(!roster(&env).await.contains(&"draft".to_string()));

    let snooped = agent_config(&env, &member, json!({ "agent": "draft" })).await;
    assert_eq!(snooped.status, 403, "{}", String::from_utf8_lossy(&snooped.body));
    assert!(
        !String::from_utf8_lossy(&snooped.body).contains("Unreleased plan."),
        "an unpublished definition leaked to a member"
    );

    let operator_read = agent_config(&env, &op, json!({ "agent": "draft" })).await;
    assert_eq!(operator_read.status, 200, "{}", String::from_utf8_lossy(&operator_read.body));
    assert_eq!(body_json(&operator_read)["instructions"], "Unreleased plan.");
    });
}

/// **A banned agent owner cannot change, resume or create agents.**
///
/// Management authority (creator, moderator, operator) is standing; the write
/// gate is current. An owner banned by a moderator kept the first and could
/// still reconfigure their agent, resume it and mint a new persona — an
/// identity that then acts and spends while its creator is restricted.
#[test]
fn severe_a_banned_owner_cannot_change_resume_or_create_agents() {
    common::run_scenario(|| async move {
    let _serial = GATE_LOCK.lock().await;
    let env = cloud_env().await;
    seed_relay_identity(&env).await;
    let moderator = Actor::new("agent-moderator");
    let owner = Actor::new("soon-banned-owner");
    seed_config(
        &env,
        b"buzz-community-moderators",
        json!([moderator.pk_hex()]).to_string().as_bytes(),
    )
    .await;
    let created = create_agent(
        &env,
        &owner,
        json!({ "display_name": "Orbit", "instructions": "Before the ban.", "model": "claude-sonnet-4-6" }),
    )
    .await;
    assert_eq!(created.status, 201, "{}", String::from_utf8_lossy(&created.body));

    let ban = signed_at(
        &moderator,
        buzz::relay::kind::KIND_MODERATION_BAN,
        vec![tag("p", &owner.pk_hex())],
        "",
        T0_SECS + 1,
    );
    assert_eq!(submit(&env, &moderator, &ban).await.status, 200, "the ban lands");

    let edit = agent_config(
        &env,
        &owner,
        json!({ "agent": "orbit", "patch": { "instructions": "After the ban." } }),
    )
    .await;
    assert_eq!(edit.status, 403, "{}", String::from_utf8_lossy(&edit.body));
    assert_eq!(stored_definition(&env, "orbit").await["instructions"], "Before the ban.");

    let resume = env
        .handle(post(
            &owner,
            "/buzz/api/agents/active",
            json!({ "agent": "orbit", "active": true }).to_string().into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(resume.status, 403, "{}", String::from_utf8_lossy(&resume.body));

    let fresh = create_agent(
        &env,
        &owner,
        json!({ "display_name": "Second", "instructions": "x", "model": "claude-sonnet-4-6" }),
    )
    .await;
    assert_eq!(fresh.status, 403, "{}", String::from_utf8_lossy(&fresh.body));
    assert!(!roster(&env).await.contains(&"second".to_string()));
    });
}

/// **A create refused because the registry names the agent under another key
/// must not overwrite that persona's signing key.**
#[test]
 fn severe_a_refused_create_leaves_a_registered_personas_key_alone() {
    common::run_scenario(|| async move {
    let _serial = GATE_LOCK.lock().await;
    let env = cloud_env().await;
    seed_relay_identity(&env).await;
    let ada = Actor::new("ada-keyring-order");
    let registered = Actor::new("operator-registered-persona");
    seed_config(
        &env,
        agents::REGISTRY_CONFIG,
        json!([{ "agent": "nova-helper", "pubkey": registered.pk_hex() }])
            .to_string()
            .as_bytes(),
    )
    .await;
    seed_config(
        &env,
        &keys::keyring(COMMUNITY, "nova-helper"),
        to_hex(&registered.sk).as_bytes(),
    )
    .await;

    let reply = create_agent(
        &env,
        &ada,
        json!({ "display_name": "Nova Helper", "instructions": "x", "model": "claude-sonnet-4-6" }),
    )
    .await;
    assert_eq!(reply.status, 409, "{}", String::from_utf8_lossy(&reply.body));
    let mut config = env.store("config");
    let key = store::get_raw(&mut config, &keys::keyring(COMMUNITY, "nova-helper"))
        .await
        .expect("config read")
        .expect("the key is still there");
    assert_eq!(
        core::str::from_utf8(&key).expect("hex"),
        to_hex(&registered.sk),
        "the registered persona's signing key must not be replaced"
    );

    });
}

/// **A sanctioned creator cannot retire the persona it created.** Configuring a
/// persona already ran the write gate; retiring it (which deletes its signing
/// key) did not, so an archived, banned or removed former owner kept that power
/// (finding_b1ba777f2850df17).
#[test]
fn severe_an_archived_creator_cannot_retire_its_persona() {
    common::run_scenario(|| async move {
        let _serial = GATE_LOCK.lock().await;
        let env = cloud_env().await;
        seed_relay_identity(&env).await;
        let ada = Actor::new("ada-archived-creator");
        let reply = create_agent(
            &env,
            &ada,
            json!({
                "display_name": "Gone Helper",
                "instructions": "Answer briefly.",
                "model": "claude-sonnet-4-6",
            }),
        )
        .await;
        assert_eq!(reply.status, 201, "{}", String::from_utf8_lossy(&reply.body));
        let agent = body_json(&reply)["agent"].as_str().expect("an agent id").to_string();

        let archive = common::signed(&ada, buzz::relay::kind::KIND_IA_ARCHIVE_REQUEST, vec![], "leaving");
        assert_eq!(submit(&env, &ada, &archive).await.status, 200);

        let refused = remove_agent(&env, &ada, &agent).await;
        assert_eq!(
            refused.status,
            403,
            "an archived creator must not retire the persona: {}",
            String::from_utf8_lossy(&refused.body)
        );
        assert!(roster(&env).await.contains(&agent), "the persona is still registered");
    });
}
