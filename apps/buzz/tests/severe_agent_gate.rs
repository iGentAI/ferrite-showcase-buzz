//! **The persona pipeline, single-bundle shape: dispatch, persona
//! commands, and the WHOLE mention→turn→answer loop — decided by the wiring.**
//!
//! Three layers, all driven end to end through the deployed serve loops:
//!
//! * **Dispatch** (relay → turn queue): a stored kind-9 mention of a
//!   registered persona publishes ONE durable [`TurnRequest`] onto the app's
//!   own `agent-turns` queue — and only when the persona could itself READ
//!   the triggering message. Personas never wake each other (the loop rule).
//! * **Persona commands** (agent runtime → relay): a persona's post/react/
//!   read executes through the SAME ingest path as a client write —
//!   membership-gated, signed by the relay-held key, deterministic under
//!   retries (same correlation ⇒ same event id, stored once) — now entered
//!   by a function call ([`buzz::relay::persona::execute`]), not a seam queue.
//! * **The turn pipeline** (queue → model → channel): the turn loop leases
//!   the trigger, runs ONE agent turn over a SCRIPTED model transport, and
//!   the answer lands in the channel as a signed persona event threaded
//!   under the trigger; a redelivered trigger is absorbed by the session
//!   admission and never runs (or posts) twice.
//!
//! Assertions are on durable state: leased queue envelopes, stored events,
//! thread metadata — never logs.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use buzz::agents::turns::{self, LlmTransport, TurnRequest};
use buzz::relay::channel::{Role, Visibility};
use buzz::relay::crypto::to_hex;
use buzz::relay::persona::{self, PersonaOp};
use buzz::relay::{agents, kind};
use common::{
    Actor, COMMUNITY, REPLY_BUDGET, T0_MS, T0_SECS, admission, gate_env, membership, post, query,
    seed_channel_with_owner, seed_membership, signed, signed_at, stored_event,
    stored_event_count, submit, tag,
};
use ferrite_app::serde_json::{self, json, Value};
use ferrite_app::AppContext;
use common::SsoTestEnv as TestEnv;
use ferrite_app_test::{TestHarness};

/// One fleet at a time in this binary (the common fleet permit is private to
/// its own users; a local lock gives the same bound).
static AGENT_GATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A scripted model transport: every chat answers with a fixed final text and
/// counts its calls, so a gate can assert "exactly one turn ran".
struct ScriptedLlm {
    chats: AtomicUsize,
    reply_text: String,
}

impl ScriptedLlm {
    fn new(reply_text: &str) -> Arc<Self> {
        Arc::new(Self {
            chats: AtomicUsize::new(0),
            reply_text: reply_text.to_string(),
        })
    }
}

impl LlmTransport for ScriptedLlm {
    fn chat<'a>(
        &'a self,
        _ctx: &'a AppContext,
        _request: buzz::llm::ChatRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<buzz::llm::ChatReply, String>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.chats.fetch_add(1, Ordering::SeqCst);
            Ok(buzz::llm::ChatReply {
                model: "scripted".into(),
                completion: self.reply_text.clone(),
                usage: buzz::llm::Usage::default(),
                finish_reason: "stop".into(),
                context_window: 1_000_000,
                cached: false,
                tool_calls: Vec::new(),
            })
        })
    }

    fn model<'a>(
        &'a self,
        _ctx: &'a AppContext,
        model: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<buzz::llm::ModelReply, String>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            Ok(buzz::llm::ModelReply {
                model: model.to_string(),
                provider: "scripted".into(),
                max_output_tokens: 4_096,
                context_window: 1_000_000,
                native_tools: true,
            })
        })
    }
}

/// A transport that answers through the buzz tool on its FIRST call and then
/// FAILS — the shape observed live on a sandbox fleet (2026-07-30): a haiku
/// posted via `buzz_post`, then `provider_shape (502)` on the next model step.
struct ToolThenFailLlm {
    calls: AtomicUsize,
    channel: String,
    content: String,
}

impl LlmTransport for ToolThenFailLlm {
    fn chat<'a>(
        &'a self,
        _ctx: &'a AppContext,
        _request: buzz::llm::ChatRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<buzz::llm::ChatReply, String>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(buzz::llm::ChatReply {
                    model: "scripted".into(),
                    completion: String::new(),
                    usage: buzz::llm::Usage::default(),
                    finish_reason: "tool_use".into(),
                    context_window: 1_000_000,
                    cached: false,
                    tool_calls: vec![buzz::llm::ToolCallOut {
                        id: "tc1".into(),
                        name: "buzz_post".into(),
                        arguments: json!({ "channel": self.channel, "content": self.content })
                            .to_string(),
                    }],
                });
            }
            Err("provider_shape (502): Anthropic response had no text or tool content".to_string())
        })
    }

    fn model<'a>(
        &'a self,
        _ctx: &'a AppContext,
        model: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<buzz::llm::ModelReply, String>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            Ok(buzz::llm::ModelReply {
                model: model.to_string(),
                provider: "scripted".into(),
                max_output_tokens: 4_096,
                context_window: 1_000_000,
                native_tools: true,
            })
        })
    }
}

/// The common env plus the agent bindings from `ferrite.json`: the turn
/// queue and the agent state planes. `llm` wires the turn loop; `None` runs
/// only the relay (the dispatch-half gates).
async fn agents_env(llm: Option<Arc<dyn LlmTransport>>) -> TestEnv {
    let env = TestEnv(TestHarness::new().with_store("nonces")
        .with_queue("requests")
        .with_queue("replies")
        .with_queue("stream-requests")
        .with_queue("agent-turns")
        .with_store("events")
        .with_store("config")
        .with_store("agent-workspace-index")
        .with_store("agent-memory")
        .with_store("gateway-config")
        .with_ape("guards")
        .with_ape("agent-sessions")
        .with_topics("feed")
        .with_blob("media")
        .with_blob("web-assets")
        .with_blob("agent-workspace-content")
        .request_reply("requests", "replies")
        .stream_requests("stream-requests")
        .manual_clock(T0_MS)
        .timeout(REPLY_BUDGET)
        .build()
        .await);
    env.run_app(|ctx| async move { buzz::relay::ingress::serve(ctx).await });
    if let Some(llm) = llm {
        env.run_app(move |ctx| {
            let llm = Arc::clone(&llm);
            async move { turns::turn_loop_with(ctx, llm).await }
        });
    }
    for _ in 0..40 {
        match env
            .handle(common::get("/buzz/api/healthz"))
            .await
        {
            Ok(reply) if reply.status == 200 => return env,
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    panic!("the app did not become healthy");
}

async fn seed_config(env: &TestEnv, key: &[u8], value: &[u8]) {
    let mut config = env.store("config");
    config.put(key, value).await.expect("config seed");
}

/// Register `personas` (no seam token exists anymore — both halves are one
/// application).
async fn seed_personas(env: &TestEnv, personas: &[(&str, &Actor)]) {
    let entries: Vec<_> = personas
        .iter()
        .map(|(agent, actor)| json!({ "agent": agent, "pubkey": actor.pk_hex() }))
        .collect();
    seed_config(
        env,
        agents::REGISTRY_CONFIG,
        json!(entries).to_string().as_bytes(),
    )
    .await;
}

/// Seed a persona's relay-held signing key.
async fn seed_persona_key(env: &TestEnv, agent: &str, actor: &Actor) {
    seed_config(
        env,
        &buzz::relay::keys::keyring(COMMUNITY, agent),
        to_hex(&actor.sk).as_bytes(),
    )
    .await;
}

/// Lease the next turn request, if one arrives within the budget.
async fn next_turn(env: &TestEnv) -> Option<TurnRequest> {
    let mut queue = env.context().queue(turns::TURNS).expect("queue");
    for _ in 0..20 {
        match queue.poll_wait(200).await {
            Ok(Some(lease)) => {
                let turn = serde_json::from_slice(&lease.payload).expect("a turn request");
                assert!(queue.ack(lease.token).await.expect("acknowledge inspected turn"));
                return Some(turn);
            }
            Ok(None) => continue,
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    None
}

/// Assert the turn queue stays empty for a full second.
async fn assert_no_turn(env: &TestEnv, why: &str) {
    let mut queue = env.context().queue(turns::TURNS).expect("queue");
    for _ in 0..5 {
        if let Ok(Some(lease)) = queue.poll_wait(200).await {
            panic!(
                "{why}: unexpected turn {}",
                String::from_utf8_lossy(&lease.payload)
            );
        }
    }
}

/// Run one persona command through the in-process surface, as the turn
/// pipeline's tools do.
async fn run_persona(env: &TestEnv, agent: &str, correlation: &str, op: PersonaOp) -> Value {
    match persona::execute(&env.context(), agent, correlation, op).await {
        Ok(value) => value,
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

fn post_op(channel: &str, content: &str) -> PersonaOp {
    PersonaOp::Post {
        channel: channel.to_string(),
        content: content.to_string(),
        reply_to: None,
    }
}

/// A mention wakes exactly the personas that can READ the message: in a
/// private channel, the member persona is dispatched, the outsider persona is
/// not — and the request carries the trigger id as the admission key.
#[test]
 fn severe_dispatch_is_read_gated_per_persona() {
    common::run_scenario(|| async move {
    let _serial = AGENT_GATE_LOCK.lock().await;
    let env = agents_env(None).await;
    let human = Actor::new("human");
    let revka = Actor::new("revka");
    let mira = Actor::new("mira");
    let channel = "chan-agents";
    seed_personas(&env, &[("revka", &revka), ("mira", &mira)]).await;
    seed_channel_with_owner(&env, channel, Visibility::Private, &human).await;
    seed_membership(&env, channel, &revka, Role::Member).await;
    // mira is NOT a member of the private channel.

    let msg = signed_at(
        &human,
        kind::KIND_STREAM_MESSAGE,
        vec![
            tag("h", channel),
            tag("p", &revka.pk_hex()),
            tag("p", &mira.pk_hex()),
        ],
        "revka, mira — thoughts?",
        T0_SECS,
    );
    assert_eq!(submit(&env, &human, &msg).await.status, 200);

    let turn = next_turn(&env).await.expect("one turn for revka");
    assert_eq!(turn.agent, "revka");
    assert_eq!(
        turn.msg_id,
        to_hex(&msg.id),
        "the trigger id IS the admission key"
    );
    assert_eq!(turn.thread, format!("buzz-{channel}"));
    assert_eq!(turn.harness, agents::DEFAULT_HARNESS);
    assert!(turn.message.contains("revka, mira — thoughts?"));
    assert!(turn.message.contains(channel));

    // The outsider persona gets NOTHING: a private room's message must not be
    // copied into an agent thread its persona could not read.
    assert_no_turn(&env, "mira is not a member").await;

    });
}

/// Personas never wake each other, and the pipeline is inert without its
/// registry — and a malformed registry disables dispatch WITHOUT taking chat
/// down.
#[test]
 fn severe_dispatch_never_loops_and_fails_inert() {
    common::run_scenario(|| async move {
    let _serial = AGENT_GATE_LOCK.lock().await;
    let env = agents_env(None).await;
    let human = Actor::new("human");
    let revka = Actor::new("revka");
    let mira = Actor::new("mira");
    let channel = "chan-loop";
    seed_channel_with_owner(&env, channel, Visibility::Open, &human).await;
    seed_membership(&env, channel, &revka, Role::Member).await;

    // Before any registry exists: a mention dispatches nothing.
    let pre = signed_at(
        &human,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel), tag("p", &revka.pk_hex())],
        "hello before registry",
        T0_SECS,
    );
    assert_eq!(submit(&env, &human, &pre).await.status, 200);
    assert_no_turn(&env, "no registry").await;

    // Fully configured: a PERSONA's own message mentioning another persona
    // dispatches nothing — the loop-prevention rule.
    let entries = json!([
        { "agent": "revka", "pubkey": revka.pk_hex() },
        { "agent": "mira", "pubkey": mira.pk_hex() }
    ]);
    seed_config(
        &env,
        agents::REGISTRY_CONFIG,
        entries.to_string().as_bytes(),
    )
    .await;
    let from_persona = signed_at(
        &revka,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel), tag("p", &mira.pk_hex())],
        "mira, over to you",
        T0_SECS + 2,
    );
    assert_eq!(submit(&env, &revka, &from_persona).await.status, 200);
    assert_no_turn(&env, "personas must not wake each other").await;

    // A malformed registry disables dispatch but chat keeps working.
    seed_config(&env, agents::REGISTRY_CONFIG, b"{ not json").await;
    let during_breakage = signed_at(
        &human,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel), tag("p", &revka.pk_hex())],
        "still here?",
        T0_SECS + 3,
    );
    assert_eq!(
        submit(&env, &human, &during_breakage).await.status,
        200,
        "a broken persona registry must not block human chat"
    );
    assert_no_turn(&env, "malformed registry").await;

    });
}

/// The persona surface fails closed at every gate: unregistered persona,
/// unseeded key, and a seeded key that does not match the registered pubkey.
/// None of them stores anything.
#[test]
 fn severe_persona_commands_fail_closed() {
    common::run_scenario(|| async move {
    let _serial = AGENT_GATE_LOCK.lock().await;
    let env = agents_env(None).await;
    let human = Actor::new("human");
    let revka = Actor::new("revka");
    let channel = "chan-cmd";
    seed_channel_with_owner(&env, channel, Visibility::Open, &human).await;
    seed_membership(&env, channel, &revka, Role::Member).await;
    let before = stored_event_count(&env).await;

    // 1. No persona registry at all: refused.
    let r = run_persona(&env, "revka", "c-noreg", post_op(channel, "hi")).await;
    assert_eq!(r["ok"], false, "unregistered persona: {r}");
    seed_personas(&env, &[("revka", &revka)]).await;

    // 2. Registered, but the signing key is not seeded.
    let r = run_persona(&env, "revka", "c-nokey", post_op(channel, "hi")).await;
    assert_eq!(r["ok"], false, "unseeded key: {r}");

    // 3. A seeded key that derives a DIFFERENT pubkey than the registry names:
    // an integrity failure, refused — never "act as whoever the key implies".
    let imposter = Actor::new("imposter");
    seed_persona_key(&env, "revka", &imposter).await;
    let r = run_persona(&env, "revka", "c-mismatch", post_op(channel, "hi")).await;
    assert_eq!(r["ok"], false, "mismatched key: {r}");

    // 4. An oversized correlation id is refused before any work.
    let r = run_persona(&env, "revka", &"c".repeat(129), post_op(channel, "hi")).await;
    assert_eq!(r["ok"], false, "oversized correlation: {r}");

    assert_eq!(
        stored_event_count(&env).await,
        before,
        "no refused command may store an event"
    );

    });
}

/// The persona write path IS the client write path: a non-member persona's
/// post into a private channel is refused; once added as a member it posts,
/// signed by the relay-held key — and a RETRIED command (same correlation)
/// rebuilds the SAME event, stored once.
#[test]
 fn severe_persona_posts_are_member_gated_and_replay_absorbed() {
    common::run_scenario(|| async move {
    let _serial = AGENT_GATE_LOCK.lock().await;
    let env = agents_env(None).await;
    let human = Actor::new("human");
    let revka = Actor::new("revka");
    let channel = "chan-private";
    seed_personas(&env, &[("revka", &revka)]).await;
    seed_persona_key(&env, "revka", &revka).await;
    seed_channel_with_owner(&env, channel, Visibility::Private, &human).await;

    // Not a member: the write is refused by the same gate a client hits.
    let refused = run_persona(&env, "revka", "c-outsider", post_op(channel, "let me in")).await;
    assert_eq!(
        refused["ok"], false,
        "a non-member persona cannot post: {refused}"
    );

    // Once a member, the same command works.
    seed_membership(&env, channel, &revka, Role::Member).await;
    let posted = run_persona(&env, "revka", "c-post", post_op(channel, "on it")).await;
    assert_eq!(posted["ok"], true, "a member persona posts: {posted}");
    assert_eq!(posted["duplicate"], false);
    let event_id = posted["event_id"]
        .as_str()
        .expect("an event id")
        .to_string();

    // The stored event is signed by the persona and lives in the channel.
    let id: buzz::relay::EventId = buzz::relay::crypto::from_hex32(&event_id).expect("hex id");
    let stored = stored_event(&env, &id)
        .await
        .expect("the persona's event is durable");
    assert_eq!(to_hex(&stored.pubkey), revka.pk_hex());
    assert_eq!(stored.kind, kind::KIND_STREAM_MESSAGE);
    assert!(stored.verify().is_ok(), "the relay-minted event verifies");
    let count_after_first = stored_event_count(&env).await;

    // RETRY: the same correlation id (a redelivered turn, or a model retry)
    // is absorbed — the answer reports the ORIGINAL event id and nothing is
    // stored twice.
    let replayed = run_persona(&env, "revka", "c-post", post_op(channel, "on it")).await;
    assert_eq!(replayed["ok"], true);
    assert_eq!(
        replayed["event_id"].as_str().unwrap(),
        event_id,
        "a replay reports the ORIGINAL event: {replayed}"
    );
    assert_eq!(
        stored_event_count(&env).await,
        count_after_first,
        "a replayed command must not store a second event"
    );

    // A threaded reply resolves NIP-10 ancestry and bumps the parent's meta.
    let threaded = run_persona(
        &env,
        "revka",
        "c-reply",
        PersonaOp::Post {
            channel: channel.to_string(),
            content: "following up in thread".to_string(),
            reply_to: Some(event_id.clone()),
        },
    )
    .await;
    assert_eq!(threaded["ok"], true, "threaded reply: {threaded}");
    let mut events = env.store("events");
    let meta = buzz::relay::effects::thread_meta_of(&mut events, COMMUNITY, &event_id)
        .await
        .expect("meta read")
        .expect("the parent gained a thread row");
    assert_eq!(
        meta.reply_count, 1,
        "the persona's reply threads like any reply"
    );

    });
}

/// The read command sees the world through the persona's OWN access: an
/// outsider persona reading a private channel gets the empty, complete
/// window; a member sees the message.
#[test]
 fn severe_persona_reads_are_access_gated() {
    common::run_scenario(|| async move {
    let _serial = AGENT_GATE_LOCK.lock().await;
    let env = agents_env(None).await;
    let human = Actor::new("human");
    let revka = Actor::new("revka");
    let channel = "chan-read";
    seed_personas(&env, &[("revka", &revka)]).await;
    seed_channel_with_owner(&env, channel, Visibility::Private, &human).await;

    let msg = signed_at(
        &human,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "private planning note",
        T0_SECS,
    );
    assert_eq!(submit(&env, &human, &msg).await.status, 200);

    let read = |correlation: &str| {
        (
            correlation.to_string(),
            PersonaOp::Read {
                channel: channel.to_string(),
                cursor: None,
                limit: None,
            },
        )
    };

    // Outsider: empty and complete — not a signal the channel exists.
    let (corr, op) = read("c-read-out");
    let outside = run_persona(&env, "revka", &corr, op).await;
    assert_eq!(outside["ok"], true);
    assert_eq!(outside["window"]["events"].as_array().unwrap().len(), 0);
    assert_eq!(outside["window"]["bounds"]["has_more"], false);

    // Member: the message is visible.
    seed_membership(&env, channel, &revka, Role::Member).await;
    let (corr, op) = read("c-read-in");
    let inside = run_persona(&env, "revka", &corr, op).await;
    assert_eq!(inside["ok"], true, "member read: {inside}");
    let events = inside["window"]["events"].as_array().unwrap();
    assert_eq!(
        events.len(),
        1,
        "the member persona sees the message: {inside}"
    );
    assert_eq!(
        events[0]["event"]["content"].as_str().unwrap(),
        "private planning note"
    );

    });
}

/// A persona read on a member's behalf sees only what that member may see: a
/// member who is not in a private room the persona sits in gets the empty
/// window, the room's own member gets the message, and a persona turn with no
/// operator behind it cannot spend the workspace connector credential.
#[test]
fn severe_persona_reads_are_intersected_with_the_requester() {
    common::run_scenario(|| async move {
    let _serial = AGENT_GATE_LOCK.lock().await;
    let env = agents_env(None).await;
    let owner = Actor::new("owner");
    let outsider = Actor::new("outsider");
    let revka = Actor::new("revka");
    let channel = "chan-deputy";
    seed_personas(&env, &[("revka", &revka)]).await;
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;
    seed_membership(&env, channel, &revka, Role::Member).await;
    let msg = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "private planning note",
        T0_SECS,
    );
    assert_eq!(submit(&env, &owner, &msg).await.status, 200);
    let read = || PersonaOp::Read {
        channel: channel.to_string(),
        cursor: None,
        limit: None,
    };

    // The outsider asks the persona to read the private room: nothing.
    let deputy = persona::execute_for(
        &env.context(),
        "revka",
        Some(&outsider.pk_hex()),
        "c-deputy-out",
        read(),
    )
    .await
    .expect("a soft, empty answer");
    assert_eq!(deputy["window"]["events"].as_array().unwrap().len(), 0, "{deputy}");

    // The room's own member asks: the message.
    let member = persona::execute_for(
        &env.context(),
        "revka",
        Some(&owner.pk_hex()),
        "c-deputy-in",
        read(),
    )
    .await
    .expect("a read");
    assert_eq!(member["window"]["events"].as_array().unwrap().len(), 1, "{member}");

    // A connector call needs an operator behind the turn.
    let ctx = env.context();
    assert!(buzz::relay::mcp_surface::require_operator_requester(&ctx, None).await.is_err());
    assert!(
        buzz::relay::mcp_surface::require_operator_requester(&ctx, Some(&outsider.pk_hex()))
            .await
            .is_err()
    );
    });
}

/// **The single-bundle capstone**: a human mention becomes ONE durable turn,
/// the turn runs over the scripted model, and the answer lands in the channel
/// as a signed persona event THREADED under the trigger — with a redelivered
/// trigger absorbed by the session admission (no second model call, no second
/// post: no double voice).
#[test]
 fn severe_mention_runs_one_turn_and_answers_in_channel() {
    common::run_scenario(|| async move {
    let _serial = AGENT_GATE_LOCK.lock().await;
    let scripted = ScriptedLlm::new("Here is my considered answer.");
    let env = agents_env(Some(Arc::clone(&scripted) as Arc<dyn LlmTransport>)).await;
    let human = Actor::new("human");
    let revka = Actor::new("revka");
    let channel = "chan-e2e";
    seed_personas(&env, &[("revka", &revka)]).await;
    seed_persona_key(&env, "revka", &revka).await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &human).await;
    seed_membership(&env, channel, &revka, Role::Member).await;

    let mention = signed_at(
        &human,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel), tag("p", &revka.pk_hex())],
        "revka, what do you think?",
        T0_SECS,
    );
    assert_eq!(submit(&env, &human, &mention).await.status, 200);
    let trigger_hex = to_hex(&mention.id);

    // The pipeline consumes the trigger and posts the persona's answer.
    let answer = wait_for_persona_answer(&env, &revka, channel).await;
    assert_eq!(answer.kind, kind::KIND_STREAM_MESSAGE);
    assert_eq!(answer.content, "Here is my considered answer.");
    assert!(
        answer.verify().is_ok(),
        "the answer is a validly signed event"
    );
    assert!(
        answer
            .tags
            .iter()
            .any(|t| t.first().map(String::as_str) == Some("e")
                && t.get(1).map(String::as_str) == Some(trigger_hex.as_str())),
        "the answer is threaded under the trigger: {:?}",
        answer.tags
    );
    assert_eq!(
        scripted.chats.load(Ordering::SeqCst),
        1,
        "one mention = one model turn"
    );
    let count_after_turn = stored_event_count(&env).await;

    // REDELIVERY: publish the SAME trigger again (a lease that outlived its
    // visibility window). The admission absorbs it: no new model call, no
    // second post. The queue's own counters are the positive evidence — but
    // an ACK for the ORIGINAL turn may still be in flight (the answer event
    // proves the post, not the ack), so first wait for QUIESCENCE (every
    // published message acked), then require the replay's own ack on top.
    let mut queue = env.context().queue(turns::TURNS).expect("queue");
    let mut quiesced = false;
    for _ in 0..240 {
        let (published, acked, dlq, pending) = queue.counts().await.expect("queue counts");
        if acked == published && pending == 0 && dlq == 0 {
            quiesced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        quiesced,
        "the original trigger was never fully acknowledged"
    );
    let replay = TurnRequest {
        agent: "revka".to_string(),
        harness: String::new(),
        msg_id: trigger_hex.clone(),
        thread: format!("buzz-{channel}"),
        message: "revka, what do you think?".to_string(),
        model: None,
        // A mention's replay, not a scheduled wake.
        schedule: None,
        requester: None,
    };
    queue
        .publish(serde_json::to_vec(&replay).expect("encodes").as_slice())
        .await
        .expect("replay publish");
    let mut absorbed = false;
    for _ in 0..240 {
        let (published, acked, dlq, pending) = queue.counts().await.expect("queue counts");
        if acked == published && pending == 0 && dlq == 0 {
            absorbed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        absorbed,
        "the replayed trigger was never consumed and acked"
    );
    assert_eq!(
        scripted.chats.load(Ordering::SeqCst),
        1,
        "a redelivered trigger must not run a second model turn"
    );
    assert_eq!(
        stored_event_count(&env).await,
        count_after_turn,
        "a redelivered trigger must not post a second answer"
    );

    });
}

/// **The one-voice rule under a FAILED turn** (regression, found live on a
/// sandbox fleet 2026-07-30): the persona answers through `buzz_post`, and the
/// NEXT model step fails. The turn's error must NOT be posted on top of the
/// answer the persona already gave — the channel saw a haiku followed by a
/// `turn error: ... provider_shape` message, because the error path reported
/// zero posts and the answer-or-silence fallback fired anyway.
#[test]
 fn severe_a_turn_that_fails_after_posting_speaks_only_once() {
    common::run_scenario(|| async move {
    let _serial = AGENT_GATE_LOCK.lock().await;
    let channel = "chan-onevoice";
    let answer_text = "Queues wait patiently; nothing is lost.";
    let scripted = Arc::new(ToolThenFailLlm {
        calls: AtomicUsize::new(0),
        channel: channel.to_string(),
        content: answer_text.to_string(),
    });
    let env = agents_env(Some(Arc::clone(&scripted) as Arc<dyn LlmTransport>)).await;
    let human = Actor::new("human");
    let revka = Actor::new("revka");
    seed_personas(&env, &[("revka", &revka)]).await;
    seed_persona_key(&env, "revka", &revka).await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &human).await;
    seed_membership(&env, channel, &revka, Role::Member).await;

    let mention = signed_at(
        &human,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel), tag("p", &revka.pk_hex())],
        "revka, one line please",
        T0_SECS,
    );
    assert_eq!(submit(&env, &human, &mention).await.status, 200);

    // The tool's post lands — that IS the persona's answer.
    let posted = wait_for_persona_answer(&env, &revka, channel).await;
    assert_eq!(posted.content, answer_text);

    // Then the turn FAILS. Wait for the queue to quiesce (the driver acked the
    // turn, so every post it was ever going to make has been made), and count.
    let mut queue = env.context().queue(turns::TURNS).expect("queue");
    let mut quiesced = false;
    for _ in 0..240 {
        let (published, acked, dlq, pending) = queue.counts().await.expect("queue counts");
        if published > 0 && acked == published && pending == 0 && dlq == 0 {
            quiesced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(quiesced, "the failing turn was never acknowledged");

    let mut events = env.store("events");
    let page = buzz::relay::query::timeline(&mut events, COMMUNITY, Some(channel), None, 50)
        .await
        .expect("timeline read");
    let voices: Vec<&str> = page
        .events
        .iter()
        .filter(|e| e.pubkey == revka.pk && e.kind == kind::KIND_STREAM_MESSAGE)
        .map(|e| e.content.as_str())
        .collect();
    assert_eq!(
        voices.len(),
        1,
        "a turn that failed AFTER posting must not speak twice: {voices:?}"
    );
    assert!(
        !voices[0].contains("turn error"),
        "the persona's one voice must be its answer, not the error: {voices:?}"
    );
    assert!(
        scripted.calls.load(Ordering::SeqCst) >= 2,
        "the fixture must actually have reached the failing model step"
    );

    });
}

/// **A reclaimed turn posts the answer its crashed driver staged, without running
/// the model again.**
///
/// Admission used to count as completion: a driver that crashed after admitting
/// the message, or after generating its answer but before the post confirmed,
/// left a trigger every redelivery skipped as a duplicate, and the answer was
/// lost. The turn now holds a lease marker and stages its final answer under it;
/// a redelivery after the lease lapses reclaims the turn and posts exactly that
/// staged answer. Asserted on the model-call count (zero) and the channel.
#[test]
 fn severe_a_reclaimed_turn_posts_its_staged_answer_without_rerunning() {
    common::run_scenario(|| async move {
    let _serial = AGENT_GATE_LOCK.lock().await;
    let scripted = ScriptedLlm::new("A DIFFERENT, newly generated answer.");
    let env = agents_env(Some(Arc::clone(&scripted) as Arc<dyn LlmTransport>)).await;
    let human = Actor::new("human");
    let revka = Actor::new("revka");
    let channel = "chan-reclaim";
    seed_personas(&env, &[("revka", &revka)]).await;
    seed_persona_key(&env, "revka", &revka).await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &human).await;
    seed_membership(&env, channel, &revka, Role::Member).await;

    // A plain message (no mention), so nothing dispatches a turn on its own.
    let parent = signed_at(&human, kind::KIND_STREAM_MESSAGE, vec![tag("h", channel)], "context", T0_SECS);
    assert_eq!(submit(&env, &human, &parent).await.status, 200);
    let msg_id = to_hex(&parent.id);
    let thread = format!("buzz-{channel}");

    // What a crashed first driver leaves: its lease lapsed, its answer staged.
    let marker_key = format!("local/turn/revka/{thread}/{msg_id}");
    let staged = json!({
        "state": "claimed",
        "attempt": 1,
        "lease_until_ms": 0,
        "pending": { "text": "The answer the first driver generated.", "top_level": false },
    });
    env.store("agent-memory")
        .put(marker_key.as_bytes(), staged.to_string().as_bytes())
        .await
        .expect("seed the crashed driver's marker");

    let redelivery = TurnRequest {
        agent: "revka".to_string(),
        harness: String::new(),
        msg_id: msg_id.clone(),
        thread,
        message: "revka, what do you think?".to_string(),
        model: None,
        schedule: None,
        requester: None,
    };
    env.context()
        .queue(turns::TURNS)
        .expect("queue")
        .publish(serde_json::to_vec(&redelivery).expect("encodes").as_slice())
        .await
        .expect("redelivery publish");

    let answer = wait_for_persona_answer(&env, &revka, channel).await;
    assert_eq!(
        answer.content, "The answer the first driver generated.",
        "the reclaim must post the STAGED answer, not a newly generated one"
    );
    assert!(
        answer.tags.iter().any(|t| t.first().map(String::as_str) == Some("e")
            && t.get(1).map(String::as_str) == Some(msg_id.as_str())),
        "threaded under the trigger, the staged form: {:?}",
        answer.tags
    );
    assert_eq!(scripted.chats.load(Ordering::SeqCst), 0, "the model must not run again");
    // And the marker ends answered, so a later redelivery is absorbed.
    let mut done = false;
    for _ in 0..240 {
        let raw = env.store("agent-memory").get(marker_key.as_bytes()).await.expect("marker read");
        if raw.as_deref().is_some_and(|r| String::from_utf8_lossy(r).contains("\"answered\"")) {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(done, "the reclaimed turn's marker must end answered");
    });
}

/// **A driver that crashed after the model answered but BEFORE staging does not
/// cost a second model call.**
///
/// The crash window between the tool loop returning and the answer being staged
/// under the marker: the marker holds a lapsed claim with nothing staged, but the
/// session log already holds the turn's `{msg_id}:final` step, which the loop
/// commits atomically with the model's final text. The reclaiming attempt runs
/// the loop with `RunAnyway`, the loop self-recognizes the finalized step, and
/// the RECORDED answer is posted: zero model calls, no different answer.
#[test]
fn severe_a_reclaim_after_a_finalized_turn_posts_the_recorded_answer() {
    common::run_scenario(|| async move {
    let _serial = AGENT_GATE_LOCK.lock().await;
    let scripted = ScriptedLlm::new("A DIFFERENT, newly generated answer.");
    let env = agents_env(Some(Arc::clone(&scripted) as Arc<dyn LlmTransport>)).await;
    let human = Actor::new("human");
    let revka = Actor::new("revka");
    let channel = "chan-finalized";
    seed_personas(&env, &[("revka", &revka)]).await;
    seed_persona_key(&env, "revka", &revka).await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &human).await;
    seed_membership(&env, channel, &revka, Role::Member).await;

    let parent = signed_at(&human, kind::KIND_STREAM_MESSAGE, vec![tag("h", channel)], "context", T0_SECS);
    assert_eq!(submit(&env, &human, &parent).await.status, 200);
    let msg_id = to_hex(&parent.id);
    let thread = format!("buzz-{channel}");
    let message = "revka, what do you think?";

    // What the crashed first driver left in the session log: the message
    // admitted and the model's final answer committed under `{msg_id}:final`,
    // exactly as the tool loop records it. ("cloud" is the turn pipeline's
    // session tenant.)
    let recorded = "The answer the model gave before the driver crashed.";
    let scope = buzz::agents::error::Scope::new("cloud", "revka", thread.as_str()).expect("scope");
    let mut engine = buzz::agents::session::SessionEngine::new(env.ape("agent-sessions"));
    engine.open(&scope).await.expect("open the session");
    engine
        .admit_message(&scope, &msg_id, message.as_bytes())
        .await
        .expect("admit the trigger");
    engine
        .finalize_turn(&scope, &format!("{msg_id}:final"), recorded.as_bytes(), recorded.as_bytes())
        .await
        .expect("finalize the turn");

    // And in the marker: a lapsed claim on the LAST generation attempt (the
    // budget is spent), NOTHING staged. The finalized answer must still be
    // delivered, not abandoned for the budget.
    let marker_key = format!("local/turn/revka/{thread}/{msg_id}");
    let lapsed = json!({ "state": "claimed", "attempt": 3, "lease_until_ms": 0 });
    env.store("agent-memory")
        .put(marker_key.as_bytes(), lapsed.to_string().as_bytes())
        .await
        .expect("seed the crashed driver's marker");

    let redelivery = TurnRequest {
        agent: "revka".to_string(),
        harness: String::new(),
        msg_id: msg_id.clone(),
        thread,
        message: message.to_string(),
        model: None,
        schedule: None,
        requester: None,
    };
    env.context()
        .queue(turns::TURNS)
        .expect("queue")
        .publish(serde_json::to_vec(&redelivery).expect("encodes").as_slice())
        .await
        .expect("redelivery publish");

    let answer = wait_for_persona_answer(&env, &revka, channel).await;
    assert_eq!(
        answer.content, recorded,
        "the reclaim must post the answer the session log recorded, not a new one"
    );
    assert_eq!(scripted.chats.load(Ordering::SeqCst), 0, "the model must not run again");
    let mut done = false;
    for _ in 0..240 {
        let raw = env.store("agent-memory").get(marker_key.as_bytes()).await.expect("marker read");
        if raw.as_deref().is_some_and(|r| String::from_utf8_lossy(r).contains("\"answered\"")) {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(done, "the reclaimed turn's marker must end answered");
    });
}

/// **A replayed turn whose own tools already posted does not post again.**
///
/// A turn that answered through `buzz_post` and then crashed before its marker
/// was finalized: the reclaim replays the session log, which does NOT re-run the
/// recorded tool call, so the replay's own post counter is zero. The pipeline
/// must count the accepted post the log records and stay quiet, rather than
/// post the finalized text as a second, fallback answer.
#[test]
fn severe_a_replayed_turn_whose_tools_posted_does_not_post_a_fallback() {
    common::run_scenario(|| async move {
    use buzz::agents::conv::{
        encode_tool_result, ToolCallBatch, ToolResultTag, ToolUse, TOOL_CALL_EVENT_KIND,
        TOOL_RESULT_EVENT_KIND,
    };
    let _serial = AGENT_GATE_LOCK.lock().await;
    let scripted = ScriptedLlm::new("A DIFFERENT, newly generated answer.");
    let env = agents_env(Some(Arc::clone(&scripted) as Arc<dyn LlmTransport>)).await;
    let human = Actor::new("human");
    let revka = Actor::new("revka");
    let channel = "chan-toolposted";
    seed_personas(&env, &[("revka", &revka)]).await;
    seed_persona_key(&env, "revka", &revka).await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &human).await;
    seed_membership(&env, channel, &revka, Role::Member).await;

    let parent = signed_at(&human, kind::KIND_STREAM_MESSAGE, vec![tag("h", channel)], "context", T0_SECS);
    assert_eq!(submit(&env, &human, &parent).await.status, 200);
    let msg_id = to_hex(&parent.id);
    let thread = format!("buzz-{channel}");
    let message = "revka, what do you think?";

    // The crashed driver's log: one buzz_post call, ACCEPTED, then the final
    // text — exactly the step ids the tool loop writes.
    let scope = buzz::agents::error::Scope::new("cloud", "revka", thread.as_str()).expect("scope");
    let mut engine = buzz::agents::session::SessionEngine::new(env.ape("agent-sessions"));
    engine.open(&scope).await.expect("open");
    engine.admit_message(&scope, &msg_id, message.as_bytes()).await.expect("admit");
    let batch_step = format!("{msg_id}:batch:0");
    let args = json!({ "channel": channel, "content": "posted by the tool", "reply_to": msg_id }).to_string();
    let batch = ToolCallBatch {
        text: String::new(),
        calls: vec![ToolUse { id: "call_0".into(), name: "buzz_post".into(), args }],
        step: batch_step.clone(),
    };
    engine
        .record_event(&scope, &batch_step, TOOL_CALL_EVENT_KIND, &batch.to_bytes())
        .await
        .expect("record the batch");
    let tag_ = ToolResultTag { batch: batch_step.clone(), index: 0, id: "call_0".into() };
    engine
        .record_event(
            &scope,
            &format!("{batch_step}:call:0"),
            TOOL_RESULT_EVENT_KIND,
            &encode_tool_result(&tag_, br#"{"ok":true,"id":"e1"}"#),
        )
        .await
        .expect("record the accepted post");
    let final_text = "Done, I posted my answer above.";
    engine
        .finalize_turn(&scope, &format!("{msg_id}:final"), final_text.as_bytes(), final_text.as_bytes())
        .await
        .expect("finalize");

    let marker_key = format!("local/turn/revka/{thread}/{msg_id}");
    let lapsed = json!({ "state": "claimed", "attempt": 1, "lease_until_ms": 0 });
    env.store("agent-memory")
        .put(marker_key.as_bytes(), lapsed.to_string().as_bytes())
        .await
        .expect("seed the crashed driver's marker");
    let redelivery = TurnRequest {
        agent: "revka".to_string(),
        harness: String::new(),
        msg_id: msg_id.clone(),
        thread,
        message: message.to_string(),
        model: None,
        schedule: None,
        requester: None,
    };
    env.context()
        .queue(turns::TURNS)
        .expect("queue")
        .publish(serde_json::to_vec(&redelivery).expect("encodes").as_slice())
        .await
        .expect("redelivery publish");

    let mut done = false;
    for _ in 0..240 {
        let raw = env.store("agent-memory").get(marker_key.as_bytes()).await.expect("marker read");
        if raw.as_deref().is_some_and(|r| String::from_utf8_lossy(r).contains("\"answered\"")) {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(done, "the reclaimed turn's marker must end answered");
    let mut events = env.store("events");
    let page = buzz::relay::query::timeline(&mut events, COMMUNITY, Some(channel), None, 50)
        .await
        .expect("timeline read");
    let voices: Vec<_> = page
        .events
        .iter()
        .filter(|e| e.pubkey == revka.pk && e.kind == kind::KIND_STREAM_MESSAGE)
        .map(|e| e.content.clone())
        .collect();
    assert!(
        voices.is_empty(),
        "the replay must not add a fallback post to the tool's answer: {voices:?}"
    );
    assert_eq!(scripted.chats.load(Ordering::SeqCst), 0, "the model must not run again");
    });
}

/// Wait for the persona's answer event to land in `channel`, returning it.
async fn wait_for_persona_answer(
    env: &TestEnv,
    persona: &Actor,
    channel: &str,
) -> buzz::relay::Event {
    let mut events = env.store("events");
    for _ in 0..240 {
        let page = buzz::relay::query::timeline(&mut events, COMMUNITY, Some(channel), None, 50)
            .await
            .expect("timeline read");
        if let Some(event) = page
            .events
            .into_iter()
            .find(|e| e.pubkey == persona.pk && e.kind == kind::KIND_STREAM_MESSAGE)
        {
            return event;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the persona never answered in the channel");
}

/// **A malformed file-tool call is refused instead of destroying content, and
/// tools that do not use the workspace do not need it.**
///
/// `write_file` without a string `content` used to write an empty file, and
/// `edit_file` without a string `new_str` used to delete the matched text; and
/// every tool opened the workspace first, so a deployment missing a workspace
/// binding could not even `end_turn`. Exercised on a context with NO workspace
/// bindings, then on one with them: the explicit empty string still works.
#[test]
fn severe_a_file_tool_refuses_missing_arguments_and_others_need_no_workspace() {
    common::run_scenario(|| async move {
    use buzz::agents::agent_loop::ToolRegistry;
    use buzz::agents::tools::CloudToolRegistry;
    fn reply(bytes: Vec<u8>) -> Value {
        serde_json::from_slice(&bytes).expect("tool results are JSON")
    }

    // No workspace bindings at all.
    let bare = TestHarness::new().with_store("nonces")
        .with_store("agent-memory")
        .manual_clock(T0_MS)
        .build()
        .await;
    let bare_ctx = bare.context();
    let tools = CloudToolRegistry::new(&bare_ctx, "agent-workspace-content", "agent-workspace-index", "revka:buzz-x");
    let done = reply(tools.invoke("end_turn", b"{}").await.expect("end_turn needs no workspace"));
    assert_eq!(done["ok"], Value::Bool(true), "{done}");
    for (tool, args) in [
        ("write_file", json!({ "path": "notes.md" })),
        ("write_file", json!({ "path": "notes.md", "content": 7 })),
        ("edit_file", json!({ "path": "notes.md", "old_str": "a" })),
        ("edit_file", json!({ "path": "notes.md", "old_str": "a", "new_str": null })),
    ] {
        let out = tools
            .invoke(tool, args.to_string().as_bytes())
            .await
            .unwrap_or_else(|e| panic!("{tool} {args} must be a soft refusal, not a plane error: {e}"));
        let out = reply(out);
        assert!(
            out["error"].as_str().is_some_and(|e| e.contains("requires a string")),
            "{tool} {args} must be refused for its argument: {out}"
        );
    }
    // A well-formed file call on this context DOES need the workspace, and says so.
    assert!(
        tools.invoke("write_file", br#"{"path":"a","content":"x"}"#).await.is_err(),
        "a real write without a workspace binding must fail, not pretend"
    );

    // With the workspace: the refusals leave the file intact, and "" is honoured.
    let env = agents_env(None).await;
    let ctx = env.context();
    let tools = CloudToolRegistry::new(&ctx, "agent-workspace-content", "agent-workspace-index", "revka:buzz-y");
    let wrote = reply(tools.invoke("write_file", br#"{"path":"keep.md","content":"keep me"}"#).await.expect("write"));
    assert_eq!(wrote["ok"], Value::Bool(true), "{wrote}");
    let refused = reply(tools.invoke("write_file", br#"{"path":"keep.md"}"#).await.expect("refusal"));
    assert!(refused["error"].is_string(), "{refused}");
    let refused = reply(tools.invoke("edit_file", br#"{"path":"keep.md","old_str":"keep "}"#).await.expect("refusal"));
    assert!(refused["error"].is_string(), "{refused}");
    let read = reply(tools.invoke("read_file", br#"{"path":"keep.md"}"#).await.expect("read"));
    assert_eq!(read["content"], "keep me", "malformed calls must not change the file");
    let emptied = reply(tools.invoke("write_file", br#"{"path":"keep.md","content":""}"#).await.expect("write"));
    assert_eq!(emptied["ok"], Value::Bool(true), "{emptied}");
    let read = reply(tools.invoke("read_file", br#"{"path":"keep.md"}"#).await.expect("read"));
    assert_eq!(read["content"], "", "an explicit empty content empties the file");
    });
}
