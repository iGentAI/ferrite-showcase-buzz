//! Pause gates: a registered persona that does not answer.
//!
//! Retiring an agent removes it; PAUSING keeps its identity, its definition and
//! its history while stopping it being woken. That distinction is the whole
//! reason the verb exists separately from `delete_persona`, and it is only real
//! if the dispatcher honours it.
//!
//! This gate exists because of the exact shape of the bug it catches. The route
//! wrote its row and the config route read it back correctly — so every test OF
//! THE ROUTE passed — while the mention dispatcher never consulted it and the
//! agent went on answering. Nothing short of asserting on what a mention does
//! could see that.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::time::Duration;

use buzz::agents::turns::{self, TurnRequest};
use buzz::relay::crypto::to_hex;
use buzz::relay::{agents, kind};
use common::SsoTestEnv as TestEnv;
use common::{
    Actor, COMMUNITY, REPLY_BUDGET, SUBMIT_ATTEMPTS, T0_MS, gate_env, error_text, post,
    seed_channel_with_owner, signed, submit, tag,
};
use ferrite_app::serde_json::{self, json};
use ferrite_app_test::TestHarness;

const MODERATORS_CONFIG: &[u8] = b"buzz-community-moderators";
const ACTIVE_PATH: &str = "/buzz/api/agents/active";

/// The relay with the agent bindings from `ferrite.json`.
///
/// The turn LOOP is deliberately not run: this gate asks whether a mention
/// produces a turn REQUEST, which is the dispatcher's decision, and running a
/// consumer would race the assertion for the answer.
async fn dispatch_env() -> TestEnv {
    let env = TestEnv(
        TestHarness::new().with_store("nonces")
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
            .await,
    );
    env.run_app(|ctx| async move { buzz::relay::ingress::serve(ctx).await });
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

/// Register one persona and give it the signing key it would need to answer.
async fn seed_persona(env: &TestEnv, agent: &str, actor: &Actor) {
    let entries = json!([{ "agent": agent, "pubkey": actor.pk_hex() }]);
    seed_config(env, agents::REGISTRY_CONFIG, entries.to_string().as_bytes()).await;
    seed_config(
        env,
        &buzz::relay::keys::keyring(COMMUNITY, agent),
        to_hex(&actor.sk).as_bytes(),
    )
    .await;
}

async fn seed_moderators(env: &TestEnv, who: &[&Actor]) {
    let list: Vec<String> = who.iter().map(|a| a.pk_hex()).collect();
    seed_config(
        env,
        MODERATORS_CONFIG,
        &serde_json::to_vec(&list).expect("encodes"),
    )
    .await;
}

/// Drain and count the turn requests currently queued.
///
/// Leases and does NOT ack, which is fine here: nothing else consumes this queue
/// in this gate, and the count is what is being asserted.
async fn queued_turns(env: &TestEnv) -> Vec<TurnRequest> {
    let mut queue = env.context().queue(turns::TURNS).expect("the turns queue");
    let mut out = Vec::new();
    // Five empty polls in a row is the queue being genuinely empty rather than
    // slow — the same shape the agent gate's own no-turn assertion uses.
    let mut empty = 0;
    while empty < 5 {
        match queue.poll_wait(200).await {
            Ok(Some(lease)) => {
                empty = 0;
                if let Ok(turn) = serde_json::from_slice::<TurnRequest>(&lease.payload) {
                    out.push(turn);
                }
                let _ = queue.ack(lease.token).await;
            }
            _ => empty += 1,
        }
    }
    out
}

/// Set a persona's active flag through the route, as a client does.
async fn set_active(env: &TestEnv, who: &Actor, agent: &str, active: bool) -> u16 {
    let body = serde_json::to_vec(&json!({ "agent": agent, "active": active })).expect("encodes");
    for attempt in 1..=SUBMIT_ATTEMPTS {
        match env.handle(post(who, ACTIVE_PATH, body.clone())).await {
            Ok(reply) => return reply.status,
            Err(e) if attempt == SUBMIT_ATTEMPTS => panic!("no reply: {e}"),
            Err(_) => continue,
        }
    }
    unreachable!()
}

/// Mention `persona` in `channel`, as a person would.
async fn mention(env: &TestEnv, who: &Actor, channel: &str, persona: &Actor, text: &str) {
    let event = signed(
        who,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel), tag("p", &persona.pk_hex())],
        text,
    );
    let reply = submit(env, who, &event).await;
    assert_eq!(reply.status, 200, "mention: {}", error_text(&reply));
}

/// **A paused persona is not woken, and resuming brings it back.**
///
/// The whole loop, because each half is meaningless without the other: a pause
/// that could not be lifted would be retirement under a different name.
#[test]
 fn severe_a_paused_persona_is_not_woken_by_a_mention() {
    common::run_scenario(|| async move {
    let env = dispatch_env().await;
    let ana = Actor::new("pause-ana");
    let revka = Actor::new("pause-revka");
    seed_persona(&env, "revka", &revka).await;
    seed_moderators(&env, &[&ana]).await;
    seed_channel_with_owner(
        &env,
        "general",
        buzz::relay::channel::Visibility::Open,
        &ana,
    )
    .await;

    // Registered and active: a mention wakes it.
    mention(&env, &ana, "general", &revka, "@revka are you there?").await;
    let woken = queued_turns(&env).await;
    assert_eq!(
        woken.len(),
        1,
        "a registered persona must be woken by a mention: {woken:?}"
    );
    assert_eq!(woken[0].agent, "revka");

    // Paused.
    assert_eq!(set_active(&env, &ana, "revka", false).await, 200);

    mention(&env, &ana, "general", &revka, "@revka still there?").await;
    let after_pause = queued_turns(&env).await;
    assert!(
        after_pause.is_empty(),
        "a PAUSED persona was still woken — the flag is written and ignored, which \
         is a button that reports success and changes nothing: {after_pause:?}"
    );

    // Resumed: it answers again. Without this half a "pause" would be a
    // retirement, and the two are different products.
    assert_eq!(set_active(&env, &ana, "revka", true).await, 200);
    mention(&env, &ana, "general", &revka, "@revka welcome back").await;
    let after_resume = queued_turns(&env).await;
    assert_eq!(
        after_resume.len(),
        1,
        "resuming did not bring the persona back: {after_resume:?}"
    );

    });
}

/// **Pausing is not open to anybody**, and a refused attempt leaves the persona
/// answering.
///
/// The second half is the one worth asserting: a refusal that still wrote the row
/// would silence somebody else's agent while telling the caller they had failed.
#[test]
 fn severe_a_stranger_cannot_pause_someone_elses_agent() {
    common::run_scenario(|| async move {
    let env = dispatch_env().await;
    let ana = Actor::new("pause-ana");
    let mallory = Actor::new("pause-mallory");
    let revka = Actor::new("pause-revka");
    seed_persona(&env, "revka", &revka).await;
    seed_moderators(&env, &[&ana]).await;
    seed_channel_with_owner(
        &env,
        "general",
        buzz::relay::channel::Visibility::Open,
        &ana,
    )
    .await;

    // A member who neither created the agent nor moderates the community.
    assert_eq!(
        set_active(&env, &mallory, "revka", false).await,
        403,
        "anybody could silence anybody's agent"
    );

    // And it really is still answering — the refusal did not write the row.
    mention(&env, &ana, "general", &revka, "@revka are you there?").await;
    let woken = queued_turns(&env).await;
    assert_eq!(
        woken.len(),
        1,
        "a REFUSED pause silenced the persona anyway: {woken:?}"
    );

    });
}
