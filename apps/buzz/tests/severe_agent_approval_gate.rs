//! Approval gates: a tool a persona's definition gates behind approval runs
//! only after its owner or an operator approves it, on that thread
//! (review finding F229).
//!
//! The turn pipeline always parked a gated call with `approval_required`, but
//! nothing ever recorded a grant, so a gated tool could never run. Every half of
//! the fix is only real end to end, so this gate drives real turns over a
//! scripted model on both sides of the approval:
//!
//! * before any approval the gated tool does not run;
//! * a stranger and a community moderator are refused, and a tool the
//!   definition does not gate is refused rather than recorded;
//! * an approval on ANOTHER channel's thread does not unlock this one;
//! * the owner's approval on this channel's thread does, and so does an
//!   operator's for a persona they do not own;
//! * the approval lasts across turns until it is revoked; a stranger and a
//!   moderator cannot revoke it; after the owner's (or an operator's) revoke
//!   the tool needs approval again, and a later approval opens it once more
//!   (the owner's decision on F229: an approval lasts for the thread, with a
//!   revoke action).

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

use buzz::agents::turns::{self, LlmTransport};
use buzz::relay::channel::{Role, Visibility};
use buzz::relay::crypto::to_hex;
use buzz::relay::{agents, kind};
use common::SsoTestEnv as TestEnv;
use common::{
    Actor, COMMUNITY, REPLY_BUDGET, SUBMIT_ATTEMPTS, T0_MS, T0_SECS, error_text, post,
    seed_channel_with_owner, seed_membership, signed_at, submit, tag,
};
use ferrite_app::serde_json::{self, json};
use ferrite_app::AppContext;
use ferrite_app_test::TestHarness;

const APPROVE_PATH: &str = "/buzz/api/agents/approve";
const REVOKE_PATH: &str = "/buzz/api/agents/revoke";
const MODERATORS_CONFIG: &[u8] = b"buzz-community-moderators";
const GATED_TEXT: &str = "the gated post ran";
const FINAL_TEXT: &str = "turn finished";

/// A scripted model: on a fresh user message it calls `buzz_post` into its
/// channel; once the call has a result it ends the turn. So a turn whose gated
/// call is parked answers only `FINAL_TEXT`, and a turn whose call ran also
/// leaves a post starting with `GATED_TEXT` in the channel. Each post is
/// numbered, so two turns that both ran the tool leave two distinct posts.
struct GatedPostLlm {
    channel: String,
    calls: AtomicUsize,
}

impl LlmTransport for GatedPostLlm {
    fn chat<'a>(
        &'a self,
        _ctx: &'a AppContext,
        request: buzz::llm::ChatRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<buzz::llm::ChatReply, String>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let after_tool = request
                .messages
                .as_ref()
                .and_then(|m| m.last())
                .is_some_and(|m| m.role == "tool");
            let tool_calls = if after_tool {
                Vec::new()
            } else {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                vec![buzz::llm::ToolCallOut {
                    id: "gated1".into(),
                    name: "buzz_post".into(),
                    arguments: json!({
                        "channel": self.channel,
                        "content": format!("{GATED_TEXT} #{n}"),
                    })
                    .to_string(),
                }]
            };
            Ok(buzz::llm::ChatReply {
                model: "scripted".into(),
                completion: if after_tool {
                    FINAL_TEXT.into()
                } else {
                    String::new()
                },
                usage: buzz::llm::Usage::default(),
                finish_reason: if after_tool { "stop" } else { "tool_use" }.into(),
                context_window: 1_000_000,
                cached: false,
                tool_calls,
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

async fn approval_env(channel: &str) -> TestEnv {
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
    let llm: Arc<dyn LlmTransport> = Arc::new(GatedPostLlm {
        channel: channel.to_string(),
        calls: AtomicUsize::new(0),
    });
    env.run_app(move |ctx| {
        let llm = Arc::clone(&llm);
        async move { turns::turn_loop_with(ctx, llm).await }
    });
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

/// Register `revka`, owned by `owner`, with a definition that gates `buzz_post`.
async fn seed_gated_persona(env: &TestEnv, revka: &Actor, owner: &Actor) {
    let registry = json!([{ "agent": "revka", "pubkey": revka.pk_hex() }]);
    seed_config(
        env,
        agents::REGISTRY_CONFIG,
        registry.to_string().as_bytes(),
    )
    .await;
    seed_config(
        env,
        &buzz::relay::keys::keyring(COMMUNITY, "revka"),
        to_hex(&revka.sk).as_bytes(),
    )
    .await;
    seed_config(
        env,
        &buzz::relay::keys::agent_owner(COMMUNITY, "revka"),
        owner.pk_hex().as_bytes(),
    )
    .await;
    let definition = json!({ "version": 1, "agent_id": "revka", "approvals": ["buzz_post"] });
    seed_config(
        env,
        &buzz::agents::definition::agent_def_key("revka"),
        definition.to_string().as_bytes(),
    )
    .await;
}

/// POST `body` to `path` as `who`; the reply's status and JSON body (`null`
/// when the body is not JSON).
async fn call(
    env: &TestEnv,
    who: &Actor,
    path: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let bytes = serde_json::to_vec(&body).expect("encodes");
    for attempt in 1..=SUBMIT_ATTEMPTS {
        match env.handle(post(who, path, bytes.clone())).await {
            Ok(reply) => {
                let json = serde_json::from_slice(&reply.body).unwrap_or(serde_json::Value::Null);
                return (reply.status, json);
            }
            Err(e) if attempt == SUBMIT_ATTEMPTS => panic!("no reply: {e}"),
            Err(_) => continue,
        }
    }
    unreachable!()
}

async fn approve(env: &TestEnv, who: &Actor, body: serde_json::Value) -> u16 {
    call(env, who, APPROVE_PATH, body).await.0
}

async fn revoke(env: &TestEnv, who: &Actor, body: serde_json::Value) -> (u16, serde_json::Value) {
    call(env, who, REVOKE_PATH, body).await
}

/// How many of `voices` are posts the gated tool made.
fn gated_posts(voices: &[String]) -> usize {
    voices.iter().filter(|v| v.starts_with(GATED_TEXT)).count()
}

/// The persona's messages in `channel`, oldest first.
async fn persona_voices(env: &TestEnv, persona: &Actor, channel: &str) -> Vec<String> {
    let mut events = env.store("events");
    let page = buzz::relay::query::timeline(&mut events, COMMUNITY, Some(channel), None, 100)
        .await
        .expect("timeline read");
    let mut voices: Vec<(u64, String)> = page
        .events
        .iter()
        .filter(|e| e.pubkey == persona.pk && e.kind == kind::KIND_STREAM_MESSAGE)
        .map(|e| (e.created_at, e.content.clone()))
        .collect();
    voices.sort();
    voices.into_iter().map(|(_, c)| c).collect()
}

/// Mention the persona and wait until the turn it triggers has answered, which
/// is when its final text lands (after any gated post it was allowed to make).
async fn mention_and_settle(
    env: &TestEnv,
    who: &Actor,
    persona: &Actor,
    channel: &str,
    text: &str,
    at_secs: u64,
) -> Vec<String> {
    let finals_before = persona_voices(env, persona, channel)
        .await
        .iter()
        .filter(|v| v.as_str() == FINAL_TEXT)
        .count();
    let posted_before = persona_voices(env, persona, channel).await.len();
    let mention = signed_at(
        who,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel), tag("p", &persona.pk_hex())],
        text,
        at_secs,
    );
    let reply = submit(env, who, &mention).await;
    assert_eq!(reply.status, 200, "mention: {}", error_text(&reply));
    // A turn whose gated call ran posts GATED_TEXT and then stops (its final text
    // is not posted a second time); a parked turn posts FINAL_TEXT. Either way
    // the persona speaks exactly once per turn.
    for _ in 0..240 {
        let voices = persona_voices(env, persona, channel).await;
        let finals = voices.iter().filter(|v| v.as_str() == FINAL_TEXT).count();
        if voices.len() > posted_before || finals > finals_before {
            // Let the driver ack before the next step reads the queue again.
            let mut queue = env.context().queue(turns::TURNS).expect("queue");
            for _ in 0..240 {
                let (published, acked, dlq, pending) = queue.counts().await.expect("queue counts");
                if acked == published && pending == 0 && dlq == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            return persona_voices(env, persona, channel).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the persona never answered the mention in {channel}");
}

#[test]
 fn severe_a_gated_tool_runs_only_after_its_owner_approves_it_on_that_thread() {
    common::run_scenario(|| async move {
    let channel = "chan-approve";
    let env = approval_env(channel).await;
    let ana = Actor::new("approve-ana"); // revka's owner
    let mod_ = Actor::new("approve-moderator");
    let mallory = Actor::new("approve-mallory");
    let revka = Actor::new("approve-revka");
    seed_gated_persona(&env, &revka, &ana).await;
    seed_config(
        &env,
        MODERATORS_CONFIG,
        &serde_json::to_vec(&[mod_.pk_hex()]).expect("encodes"),
    )
    .await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &ana).await;
    seed_membership(&env, channel, &revka, Role::Member).await;

    // 1. Gated and not approved: the call parks, the persona only relays.
    let voices = mention_and_settle(&env, &ana, &revka, channel, "revka, post it", T0_SECS).await;
    assert_eq!(
        gated_posts(&voices),
        0,
        "a gated tool ran with no approval recorded: {voices:?}"
    );

    // 2. Refusals, none of which may record a grant.
    let here = json!({ "agent": "revka", "tool": "buzz_post", "channel": channel });
    assert_eq!(
        approve(&env, &mallory, here.clone()).await,
        403,
        "a stranger approved"
    );
    assert_eq!(
        approve(&env, &mod_, here.clone()).await,
        403,
        "a community moderator who neither owns the persona nor operates the workspace approved"
    );
    assert_eq!(
        approve(
            &env,
            &ana,
            json!({ "agent": "revka", "tool": "buzz_react", "channel": channel })
        )
        .await,
        400,
        "a tool the definition does not gate must be refused, not recorded"
    );
    assert_eq!(
        approve(
            &env,
            &ana,
            json!({ "agent": "revka", "tool": "buzz_post", "channel": channel, "thread": "x" })
        )
        .await,
        400,
        "naming both a channel and a thread is ambiguous"
    );
    // The owner approves on a DIFFERENT channel's thread: accepted, but it must
    // not reach this one.
    assert_eq!(
        approve(
            &env,
            &ana,
            json!({ "agent": "revka", "tool": "buzz_post", "channel": "elsewhere" })
        )
        .await,
        200
    );
    let voices =
        mention_and_settle(&env, &ana, &revka, channel, "revka, try again", T0_SECS + 1).await;
    assert_eq!(
        gated_posts(&voices),
        0,
        "a refused approval, or one for another thread, unlocked this thread: {voices:?}"
    );

    // 3. The owner approves on this channel's thread: the next turn runs it.
    assert_eq!(approve(&env, &ana, here.clone()).await, 200);
    // Approving again is absorbed, not an error.
    assert_eq!(approve(&env, &ana, here.clone()).await, 200);
    let voices = mention_and_settle(
        &env,
        &ana,
        &revka,
        channel,
        "revka, now post it",
        T0_SECS + 2,
    )
    .await;
    assert_eq!(
        gated_posts(&voices),
        1,
        "the approved tool must run once on the next turn: {voices:?}"
    );

    // 4. The approval lasts for the thread. A stranger and a community
    // moderator cannot revoke it, so the next turn still runs the tool.
    assert_eq!(
        revoke(&env, &mallory, here.clone()).await.0,
        403,
        "a stranger revoked"
    );
    assert_eq!(
        revoke(&env, &mod_, here.clone()).await.0,
        403,
        "a community moderator who neither owns the persona nor operates the workspace revoked"
    );
    let voices = mention_and_settle(
        &env,
        &ana,
        &revka,
        channel,
        "revka, post again",
        T0_SECS + 3,
    )
    .await;
    assert_eq!(
        gated_posts(&voices),
        2,
        "an approval must last across turns until it is revoked: {voices:?}"
    );

    // 5. The owner revokes: the tool needs approval again from the next turn.
    assert_eq!(revoke(&env, &ana, here.clone()).await.0, 200);
    let voices =
        mention_and_settle(&env, &ana, &revka, channel, "revka, once more", T0_SECS + 4).await;
    assert_eq!(
        gated_posts(&voices),
        2,
        "a revoked tool must be back to approval_required: {voices:?}"
    );
    // Revoking again finds nothing live and says so.
    let (status, reply) = revoke(&env, &ana, here.clone()).await;
    assert_eq!(status, 200);
    assert_eq!(reply["revoked"], json!(false), "{reply}");

    // 6. A new approval after a revoke is a new grant, not absorbed by the
    // revoked one.
    assert_eq!(approve(&env, &ana, here).await, 200);
    let voices =
        mention_and_settle(&env, &ana, &revka, channel, "revka, go ahead", T0_SECS + 5).await;
    assert_eq!(
        gated_posts(&voices),
        3,
        "approving after a revoke must unlock the tool again: {voices:?}"
    );

    });
}

#[test]
 fn severe_an_operator_may_approve_a_persona_they_do_not_own() {
    common::run_scenario(|| async move {
    let channel = "chan-operator";
    let env = approval_env(channel).await;
    let ana = Actor::new("approve-ana");
    let olga = Actor::new("approve-operator");
    let revka = Actor::new("approve-revka");
    seed_gated_persona(&env, &revka, &ana).await;
    seed_config(
        &env,
        buzz::relay::operator::OPERATOR_SET_CONFIG,
        &serde_json::to_vec(&[olga.pk_hex()]).expect("encodes"),
    )
    .await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &ana).await;
    seed_membership(&env, channel, &revka, Role::Member).await;

    assert_eq!(
        approve(
            &env,
            &olga,
            json!({ "agent": "revka", "tool": "buzz_post", "thread": format!("buzz-{channel}") })
        )
        .await,
        200,
        "an operator is the backstop for a persona whose owner is gone"
    );
    let voices = mention_and_settle(&env, &ana, &revka, channel, "revka, post it", T0_SECS).await;
    assert_eq!(
        gated_posts(&voices),
        1,
        "an operator's approval must unlock the tool on that thread: {voices:?}"
    );

    // The operator may also revoke it, naming the thread directly.
    let (status, reply) = revoke(
        &env,
        &olga,
        json!({ "agent": "revka", "tool": "buzz_post", "thread": format!("buzz-{channel}") }),
    )
    .await;
    assert_eq!(status, 200, "{reply}");
    let voices = mention_and_settle(
        &env,
        &ana,
        &revka,
        channel,
        "revka, post again",
        T0_SECS + 1,
    )
    .await;
    assert_eq!(
        gated_posts(&voices),
        1,
        "an operator's revoke must put the tool back behind approval: {voices:?}"
    );

    });
}

#[test]
fn severe_sanctioned_or_evicted_owner_cannot_approve_or_revoke_gated_tools() {
    common::run_scenario(|| async move {
        let channel = "chan-sanctioned-owner";
        let env = approval_env(channel).await;
        let ana = Actor::new("sanctioned-owner-ana");
        let olga = Actor::new("sanctioned-operator-olga");
        let revka = Actor::new("sanctioned-revka");
        seed_gated_persona(&env, &revka, &ana).await;
        seed_config(
            &env,
            buzz::relay::operator::OPERATOR_SET_CONFIG,
            &serde_json::to_vec(&[olga.pk_hex()]).expect("encodes"),
        )
        .await;
        seed_channel_with_owner(&env, channel, Visibility::Open, &ana).await;
        seed_membership(&env, channel, &revka, Role::Member).await;

        let here = json!({ "agent": "revka", "tool": "buzz_post", "channel": channel });

        // Operator grants an initial approval so we can verify revoke is also blocked.
        assert_eq!(approve(&env, &olga, here.clone()).await, 200);

        // 1. Banned owner cannot revoke or approve.
        {
            let mut events = env.store("events");
            buzz::relay::store::put_doc(
                &mut events,
                &buzz::relay::keys::restriction(COMMUNITY, &ana.pk),
                &buzz::relay::effects::Restriction {
                    banned: true,
                    banned_until: None,
                    timeout_until: None,
                },
            )
            .await
            .expect("seed ban");
        }
        assert_eq!(
            approve(&env, &ana, here.clone()).await,
            403,
            "a banned persona owner must not approve gated tools"
        );
        assert_eq!(
            revoke(&env, &ana, here.clone()).await.0,
            403,
            "a banned persona owner must not revoke operator-granted approvals"
        );

        // 2. Timed-out owner cannot approve or revoke.
        {
            let mut events = env.store("events");
            buzz::relay::store::put_doc(
                &mut events,
                &buzz::relay::keys::restriction(COMMUNITY, &ana.pk),
                &buzz::relay::effects::Restriction {
                    banned: false,
                    banned_until: None,
                    timeout_until: Some(T0_MS + 60_000),
                },
            )
            .await
            .expect("seed timeout");
        }
        assert_eq!(
            approve(&env, &ana, here.clone()).await,
            403,
            "a timed-out persona owner must not approve gated tools"
        );
        assert_eq!(
            revoke(&env, &ana, here.clone()).await.0,
            403,
            "a timed-out persona owner must not revoke gated tools"
        );

        // Clear restriction, then archive Ana's identity: archived owner cannot approve or revoke.
        {
            let mut events = env.store("events");
            buzz::relay::store::delete_key(
                &mut events,
                &buzz::relay::keys::restriction(COMMUNITY, &ana.pk),
            )
            .await
            .expect("clear restriction");
            buzz::relay::store::put_doc(
                &mut events,
                &buzz::relay::keys::archived_identity(COMMUNITY, &ana.pk),
                &buzz::relay::archive::ArchivedIdentity {
                    pubkey: ana.pk_hex(),
                    archived_at: T0_MS,
                    by: olga.pk_hex(),
                    reason: None,
                },
            )
            .await
            .expect("seed archive");
        }
        assert_eq!(
            approve(&env, &ana, here.clone()).await,
            403,
            "an archived persona owner must not approve gated tools"
        );
        assert_eq!(
            revoke(&env, &ana, here.clone()).await.0,
            403,
            "an archived persona owner must not revoke gated tools"
        );

        // Clear archive, close community without Ana's community membership: evicted owner is refused,
        // while the workspace operator backstop still works without an invite claim.
        {
            let mut events = env.store("events");
            buzz::relay::store::delete_key(
                &mut events,
                &buzz::relay::keys::archived_identity(COMMUNITY, &ana.pk),
            )
            .await
            .expect("clear archive");
        }
        seed_config(&env, b"buzz-community-closed", b"true").await;
        assert_eq!(
            approve(&env, &ana, here.clone()).await,
            403,
            "an evicted owner in a closed community must not approve gated tools"
        );
        assert_eq!(
            revoke(&env, &ana, here.clone()).await.0,
            403,
            "an evicted owner in a closed community must not revoke gated tools"
        );
        assert_eq!(
            approve(&env, &olga, here.clone()).await,
            200,
            "an operator remains the backstop in a closed community"
        );
    });
}

/// **An unusable stored definition never strips a persona's guardrails.** A
/// corrupt or unverifiable definition used to read as "no definition", which
/// runs the unrestricted default surface: no tool filter, no approvals and the
/// platform step cap (finding_7fa8f67b8c423054, finding_26bdd79209a3d722). The
/// loader now tells "nothing stored" apart from "stored but unusable", and a
/// turn against an unusable definition answers with a refusal instead of
/// running the gated tool.
#[test]
fn severe_an_unusable_definition_fails_closed_instead_of_running_ungated() {
    common::run_scenario(|| async move {
        use buzz::agents::bundle::DefinitionLoadError;
        use buzz::agents::definition::{agent_def_key, load_agent_definition};

        let channel = "chan-corrupt-def";
        let env = approval_env(channel).await;
        let ana = Actor::new("corrupt-ana");
        let revka = Actor::new("corrupt-revka");
        seed_gated_persona(&env, &revka, &ana).await;
        seed_channel_with_owner(&env, channel, Visibility::Open, &ana).await;
        seed_membership(&env, channel, &revka, Role::Member).await;
        let ctx = env.context();

        assert!(
            matches!(load_agent_definition(&ctx, "config", "nobody", 32).await, Ok(None)),
            "a persona with nothing stored has no definition"
        );
        assert!(
            matches!(load_agent_definition(&ctx, "config", "revka", 32).await, Ok(Some(_))),
            "the seeded gated definition loads"
        );
        assert!(
            matches!(
                load_agent_definition(&ctx, "no-such-store", "revka", 32).await,
                Err(DefinitionLoadError::Unavailable(_))
            ),
            "an unbound store is an error, not an absent definition"
        );

        seed_config(&env, &agent_def_key("revka"), b"{ not json").await;
        assert!(
            matches!(
                load_agent_definition(&ctx, "config", "revka", 32).await,
                Err(DefinitionLoadError::Invalid(_))
            ),
            "a corrupt stored definition is an error, not an absent definition"
        );

        let voices =
            mention_and_settle(&env, &ana, &revka, channel, "@revka post please", T0_SECS + 1).await;
        assert_eq!(
            gated_posts(&voices),
            0,
            "a persona whose definition is unusable must not run its gated tool: {voices:?}"
        );
    });
}

/// **A workflow `agent` step starts a turn.** The step posts a relay-signed
/// mention through `ingest::drive`, which runs no ingress hooks, so the mention
/// used to sit in the room and no turn was ever enqueued
/// (finding_f47620e628f4b60e, finding_b963d9a036a21ae4).
#[test]
fn severe_a_workflow_agent_step_enqueues_a_persona_turn() {
    common::run_scenario(|| async move {
        let channel = "chan-workflow-agent";
        let env = approval_env(channel).await;
        let ana = Actor::new("wfagent-ana");
        let revka = Actor::new("wfagent-revka");
        let relay = Actor::new("wfagent-relay");
        seed_gated_persona(&env, &revka, &ana).await;
        seed_config(&env, b"buzz-relay-identity", to_hex(&relay.sk).as_bytes()).await;
        seed_channel_with_owner(&env, channel, Visibility::Open, &ana).await;
        seed_membership(&env, channel, &revka, Role::Member).await;

        let definition = json!({
            "name": "ask revka",
            "on": { "contains": "go now" },
            "steps": [{ "type": "agent", "id": "ask", "agent": "revka", "prompt": "summarize the room" }],
        });
        let defined = signed_at(
            &ana,
            kind::KIND_WORKFLOW_DEF,
            vec![tag("d", "ask-revka"), tag("h", channel)],
            &definition.to_string(),
            T0_SECS,
        );
        let reply = submit(&env, &ana, &defined).await;
        assert_eq!(reply.status, 200, "define: {}", error_text(&reply));

        let mut queue = env.context().queue(turns::TURNS).expect("queue");
        let (before, ..) = queue.counts().await.expect("queue counts");
        let trigger = signed_at(&ana, kind::KIND_STREAM_MESSAGE, vec![tag("h", channel)], "go now", T0_SECS + 1);
        let reply = submit(&env, &ana, &trigger).await;
        assert_eq!(reply.status, 200, "trigger: {}", error_text(&reply));
        for _ in 0..100 {
            let (published, ..) = queue.counts().await.expect("queue counts");
            if published > before {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the workflow's agent step never enqueued a turn for revka");
    });
}
