//! **Scheduled agent turns: the mechanism, the authority, and the one thing a
//! queued message cannot do.**
//!
//! The row arithmetic — identity, recurrence bounds, the catch-up walk, the caps —
//! is covered by unit tests in `relay::schedules`. What those cannot show is
//! whether the PLATFORM's delayed-delivery gate, the turn consumer, the session
//! admission and the relay's authority checks compose into a persona that wakes up
//! and says something. So the properties below drive a real fleet.
//!
//! # One fleet, three phases
//!
//! Every `run_app` task outlives the `TestEnv` that started it, so a binary whose
//! tests each start a turn loop ends up with several alive in ONE process — and a
//! wake can then be taken by a loop whose context points at a different config
//! store. That loop finds no row, decides Stop, and acks it away, leaving a
//! schedule that looks like it silently did nothing. Which is indistinguishable
//! from the defect these tests exist to catch (observed: the third pipeline test
//! to run failed on exactly that, while each passed alone).
//!
//! So the pipeline properties are PHASES of one test against one fleet, and each
//! asserts the model-call count against where the previous phase left it. That is
//! stronger than a fresh zero as well as more reliable: "nothing ran" cannot pass
//! while a turn ran and merely failed to post.
//!
//! # What this harness cannot show, and why
//!
//! The queue plane evaluates its delayed-delivery gate against the CLOCK, and
//! these gates freeze it at `T0_MS`. A wake due at `T0_MS` delivers; the next
//! occurrence, due an interval later, never does. So a recurrence advances exactly
//! once here, `pending` never returns to zero, and the ack COUNT is the only
//! usable completion signal. The recurrence arithmetic is therefore tested where
//! it can be tested honestly: the unit tests over `relay::schedules::advance`.

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

use buzz::agents::turns::{self, LlmTransport, ScheduledWake, TurnRequest};
use buzz::relay::channel::{Role, Visibility};
use buzz::relay::crypto::to_hex;
use buzz::relay::schedules::{self, Schedule, ScheduleRequest};
use buzz::relay::{agents, kind, store as buzz_store};
use common::SsoTestEnv as TestEnv;
use common::{
    Actor, COMMUNITY, REPLY_BUDGET, T0_MS, admission, body_json, gate_env, error_text, post, query,
    seed_channel_with_owner, seed_membership,
};
use ferrite_app::serde_json::{self, json, Value};
use ferrite_app::AppContext;
use ferrite_app_test::TestHarness;

/// One fleet at a time in this binary.
static SCHEDULE_GATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A scripted transport that answers with fixed text and COUNTS its calls.
///
/// The count is what makes every negative assertion here mean something: a channel
/// with no new message is equally consistent with "the turn was skipped" and "the
/// turn ran, spent, and failed to post".
struct CountingLlm {
    chats: AtomicUsize,
    reply: String,
}

impl CountingLlm {
    fn new(reply: &str) -> Arc<Self> {
        Arc::new(Self {
            chats: AtomicUsize::new(0),
            reply: reply.to_string(),
        })
    }
}

impl LlmTransport for CountingLlm {
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
                completion: self.reply.clone(),
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

/// The relay, and the turn pipeline only when a transport is given.
///
/// `None` runs the relay alone — which a validation test wants, and which keeps it
/// from starting the very consumer that interferes with everything else (see the
/// module docs).
async fn schedule_env(llm: Option<Arc<dyn LlmTransport>>) -> TestEnv {
    // The turns queue needs a named resource, with an identity a wake receipt can
    // be bound to, only when recall is compiled in (`platform-queue-cancel`). The
    // default build binds every receipt to one fixed coordinate, so an ordinary
    // queue serves it, and that is the only kind the in-platform test harness
    // offers: asking for a named one unconditionally meant this suite did not
    // compile under the platform's test build.
    #[cfg(feature = "platform-queue-cancel")]
    let harness = TestHarness::new().with_store("nonces").with_named_queue("agent-turns", "scheduled-turns");
    #[cfg(not(feature = "platform-queue-cancel"))]
    let harness = TestHarness::new().with_store("nonces").with_queue("agent-turns");
    let env = TestEnv(
        harness
            .with_queue("requests")
            .with_queue("replies")
            .with_queue("stream-requests")
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
    if let Some(llm) = llm {
        env.run_app(move |ctx| {
            let llm = Arc::clone(&llm);
            async move { turns::turn_loop_with(ctx, llm).await }
        });
    }
    let mut healthy = false;
    for _ in 0..40 {
        match env
            .handle(common::get("/buzz/api/healthz"))
            .await
        {
            Ok(reply) if reply.status == 200 => {
                healthy = true;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    assert!(healthy, "the app did not become healthy");
    spawn_phantom_lease_healer(&env);
    env
}

/// Heal the one thing a frozen clock cannot: an ABANDONED lease.
///
/// The queue client's `poll_wait` races its own local timer against the
/// server's park deadline with zero margin (`tokio::time::timeout(remaining,
/// request_primary)`, `Err(_) => return Ok(None)` — ferrite-queue
/// `cluster_queue_node.rs`). When the server grants a lease in the last
/// instant of the window, the client has already given up: the message is
/// Leased with nobody driving it. On the real clock the visibility timeout
/// reclaims it and redelivery repairs everything — the absorption these gates
/// prove elsewhere. Under `manual_clock` the reclaim instant NEVER arrives,
/// so one lost race hangs the gate for its whole budget (measured: ~1 run in
/// 5, on any queue — a turn wake, or the ingress request/reply pair).
///
/// So this watcher does exactly what the real clock would do, compressed:
/// when any monitored queue holds pending work and NO counter has moved for
/// two seconds of wall time, it advances the virtual clock past the
/// visibility window (2 000 ms in this harness) so the reclaim fires. It
/// advances at most 20 times (40 s virtual), staying well below
/// `MIN_INTERVAL_MS` (60 s) — the next scheduled occurrence must stay
/// undeliverable or phase 3's row assertions would be about a different
/// schedule. Redelivery after a heal is absorbed by the same admissions the
/// unfrozen system relies on, so a heal can never create a second turn —
/// severe_agent_gate proves that absorption directly.
///
/// Remove when the upstream client keeps a response-transit margin.
fn spawn_phantom_lease_healer(env: &TestEnv) {
    const HARNESS_VISIBILITY_MS: u64 = 2_000;
    const MAX_HEALS: u32 = 20;
    let clock = env.clock();
    let ctx = env.context();
    tokio::spawn(async move {
        let queues = ["requests", "replies", turns::TURNS];
        let mut clients: Vec<_> = queues
            .iter()
            .filter_map(|name| ctx.queue(name).ok())
            .collect();
        let mut last: Option<Vec<(u64, u64, u64, u64)>> = None;
        let mut stable = 0u32;
        let mut heals = 0u32;
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let mut counts = Vec::with_capacity(clients.len());
            for c in clients.iter_mut() {
                match c.counts().await {
                    Ok(t) => counts.push(t),
                    Err(_) => return, // fleet is tearing down
                }
            }
            let pending_somewhere = counts.iter().any(|(p, a, d, _)| p > &(a + d));
            if pending_somewhere && last.as_ref() == Some(&counts) {
                stable += 1;
            } else {
                stable = 0;
            }
            last = Some(counts);
            if stable >= 4 && heals < MAX_HEALS {
                heals += 1;
                stable = 0;
                let now = clock.advance_ms(HARNESS_VISIBILITY_MS + 1);
                eprintln!(
                    "schedule gate: healed a stuck lease (advance #{heals} to {now}) — \
                     see spawn_phantom_lease_healer"
                );
            }
        }
    });
}

async fn seed_config(env: &TestEnv, key: &[u8], value: &[u8]) {
    let mut config = env.store("config");
    config.put(key, value).await.expect("config seed");
}

/// Register `personas` and seed each one's relay-held signing key.
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
    for (agent, actor) in personas {
        seed_config(
            env,
            &buzz::relay::keys::keyring(COMMUNITY, agent),
            to_hex(&actor.sk).as_bytes(),
        )
        .await;
    }
}

/// An open room `human` owns and `persona` is a member of.
async fn room(env: &TestEnv, channel: &str, human: &Actor, persona: &Actor) {
    seed_channel_with_owner(env, channel, Visibility::Open, human).await;
    seed_membership(env, channel, persona, Role::Member).await;
}

/// Ask for a schedule, as `who`.
fn ask(who: &Actor, body: Value) -> ferrite_app::IngressRequest {
    post(who, "/buzz/api/schedules", body.to_string().into_bytes())
}

/// The stored schedule row, if it exists.
async fn row(env: &TestEnv, id: &str) -> Option<Schedule> {
    let mut config = env.store("config");
    schedules::get(&mut config, COMMUNITY, id)
        .await
        .expect("schedule read")
}

/// Seed a live schedule row as a real create leaves it: the row AND its slot under
/// the agent's cap record. A wake runs only while its schedule holds a slot, so a
/// bare row (what only a build before the cap record could write, once a record
/// exists) would be stopped by the cap fence rather than reach the phase's check.
async fn seed_live_row(env: &TestEnv, schedule: &Schedule) {
    let mut config = env.store("config");
    buzz_store::put_doc(
        &mut config,
        schedules::schedule_key(COMMUNITY, &schedule.id).as_bytes(),
        schedule,
    )
    .await
    .expect("seed the schedule row");
    let key = schedules::agent_slots_key(COMMUNITY, &schedule.agent);
    let mut slots: schedules::AgentSlots = buzz_store::get_doc(&mut config, key.as_bytes())
        .await
        .expect("read slots")
        .unwrap_or_default();
    slots.slots.push(schedules::Slot { id: schedule.id.clone(), at_ms: T0_MS, revision: schedule.revision });
    buzz_store::put_doc(&mut config, key.as_bytes(), &slots).await.expect("seed the slot");
}

/// Publish a wake for `schedule`'s current occurrence, by hand and undelayed.
///
/// For the phase that needs a wake to ARRIVE so the row's verdict on it can be
/// observed — a cancelled schedule's queued message cannot be recalled, and that
/// is the whole point of checking the row.
async fn publish_wake(env: &TestEnv, schedule: &Schedule) {
    let mut queue = env.context().queue(turns::TURNS).expect("turn queue");
    let request = TurnRequest {
        agent: schedule.agent.clone(),
        harness: String::new(),
        msg_id: schedules::occurrence_admission(&schedule.id, schedule.revision, schedule.occurrence),
        thread: turns::thread_for_channel(&schedule.channel),
        message: schedule.prompt.clone(),
        model: None,
        schedule: Some(ScheduledWake {
            id: schedule.id.clone(),
            occurrence: schedule.occurrence,
            revision: schedule.revision,
        }),
        requester: None,
    };
    queue
        .publish(serde_json::to_vec(&request).expect("encodes").as_slice())
        .await
        .expect("wake publish");
}

/// Wait until at least `n` turn envelopes have been acked.
///
/// The only completion signal available here. Quiescence is unreachable: settling a
/// recurrence publishes a wake due an interval later, the clock is frozen, so
/// `pending` never returns to zero. Acks climb as each DELIVERABLE wake is retired,
/// which is exactly the boundary each phase needs.
///
/// The budget is generous because its units do not match the work's. The harness
/// clock is FROZEN, so a due wake is deliverable the instant it is published — but
/// this loop counts in WALL CLOCK, and on a box running the whole suite (fifty-nine
/// binaries, sometimes beside an attest build) sixty seconds of wall clock is not
/// sixty seconds of progress. Measured: this gate passes in 4.5 s alone and timed out
/// at 60 s twice under a full run.
///
/// Patience cannot weaken the assertion — an ack either arrives or it does not — and a
/// gate that fails under load is one people learn to re-run until it passes.
async fn wait_for_acks(env: &TestEnv, n: u64, why: &str) {
    let mut queue = env.context().queue(turns::TURNS).expect("turn queue");
    let mut last = (0, 0, 0, 0);
    for _ in 0..1_800 {
        last = queue.counts().await.expect("queue counts");
        let (_published, acked, dlq, _pending) = last;
        assert_eq!(dlq, 0, "{why}: a turn was dead-lettered");
        if acked >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (published, acked, dlq, pending) = last;
    let probe = queue.poll_wait(1_000).await;
    let probe_desc = match &probe {
        Ok(Some(l)) => format!(
            "a FRESH client CAN lease it (id {}) — consumer-side loss",
            l.id
        ),
        Ok(None) => "a fresh client CANNOT lease it — the engine holds it invisible \
                     (gated in the future, or phantom-leased with an expiry the \
                     frozen clock never reaches)"
            .to_string(),
        Err(e) => format!("probe lease errored: {e}"),
    };
    panic!(
        "{why}: fewer than {n} turn(s) were acked \
         (published {published}, acked {acked}, dlq {dlq}, pending {pending}; \
         probe: {probe_desc})"
    );
}

/// Wait until `id`'s row has `remaining` occurrences left (bounded).
///
/// An absolute ack count cannot say THIS phase's wake was handled: a settled
/// occurrence re-publishes its next wake after the commit, and those absorbed
/// duplicates are acked too, so an earlier phase can already account for the
/// count a later phase waits on. The row itself is the observable.
async fn wait_for_remaining(env: &TestEnv, id: &str, remaining: u32, why: &str) {
    let mut last = None;
    for _ in 0..1_800 {
        last = row(env, id).await;
        if last.as_ref().is_some_and(|r| r.remaining == remaining) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{why}: the row never reached remaining = {remaining}: {last:?}");
}

/// The persona's messages in `channel`.
async fn persona_voices(env: &TestEnv, persona: &Actor, channel: &str) -> Vec<String> {
    let mut events = env.store("events");
    let page = buzz::relay::query::timeline(&mut events, COMMUNITY, Some(channel), None, 50)
        .await
        .expect("timeline read");
    page.events
        .iter()
        .filter(|e| e.pubkey == persona.pk && e.kind == kind::KIND_STREAM_MESSAGE)
        .map(|e| e.content.clone())
        .collect()
}

/// **Scheduled wakes: they answer, a cancelled row stops one already queued, and a
/// paused persona skips its occurrence without losing the schedule.**
///
/// Three phases against ONE fleet, for the reason the module docs give. Each phase
/// asserts the model count against where the last one left it, so a turn that ran
/// and posted nothing cannot hide behind a silent channel.
#[test]
 fn severe_scheduled_wakes_answer_cancel_and_respect_a_pause() {
    common::run_scenario(|| async move {
    let _serial = SCHEDULE_GATE_LOCK.lock().await;
    let scripted = CountingLlm::new("Standup in ten minutes.");
    let env = schedule_env(Some(Arc::clone(&scripted) as Arc<dyn LlmTransport>)).await;
    let human = Actor::new("human");
    let revka = Actor::new("revka");
    let mira = Actor::new("mira");
    let zed = Actor::new("zed");
    seed_personas(&env, &[("revka", &revka), ("mira", &mira), ("zed", &zed)]).await;

    // ---- PHASE 1: a due schedule wakes its persona and answers in the channel.
    //
    // The design's whole claim in one assertion: a delayed publish of the SAME
    // `TurnRequest` a mention makes, with no timer anywhere in the app.
    let answered = "chan-sched";
    room(&env, answered, &human, &revka).await;
    let created = env
        .handle(ask(
            &human,
            json!({
                "agent": "revka",
                "channel": answered,
                "prompt": "post the standup reminder",
            }),
        ))
        .await
        .expect("the schedule route answers");
    assert_eq!(created.status, 201, "{}", error_text(&created));
    let one_shot = body_json(&created)["schedule"]["id"]
        .as_str()
        .expect("a schedule id")
        .to_string();

    wait_for_acks(&env, 1, "the due schedule's wake").await;
    assert_eq!(
        persona_voices(&env, &revka, answered).await,
        vec!["Standup in ten minutes.".to_string()],
        "a due schedule must produce exactly one answer"
    );
    assert_eq!(
        scripted.chats.load(Ordering::SeqCst),
        1,
        "one occurrence = one model turn"
    );
    // A ONE-SHOT is finished, not deleted: `remaining = 0` is what a redelivery of
    // this same wake has to find in order to stop.
    let settled = row(&env, &one_shot)
        .await
        .expect("the row survives its last run");
    assert_eq!(settled.remaining, 0, "a one-shot is exhausted, not deleted");

    // ---- PHASE 2: a cancelled row refuses a wake that is ALREADY in the queue.
    //
    // Why cancellation is a row check rather than an unpublish, tested against the
    // hard case deliberately: the wake is published BY HAND after the row says
    // cancelled. A delayed message cannot be recalled, so without the row check the
    // turn would run and the provider would be billed by something the operator
    // believes they stopped.
    let cancelled_room = "chan-cancelled";
    room(&env, cancelled_room, &human, &revka).await;
    let request = ScheduleRequest {
        agent: "revka".into(),
        channel: cancelled_room.into(),
        prompt: "say something".into(),
        first_due_ms: T0_MS,
        interval_ms: None,
        occurrences: 1,
        created_by: human.pk_hex(),
    };
    let mut cancelled = schedules::build(&request, COMMUNITY, T0_MS).expect("builds");
    cancelled.cancelled = true;
    // Seeded WITH its slot, so the cap fence admits it and cancellation is the only
    // thing that can stop this wake.
    seed_live_row(&env, &cancelled).await;
    publish_wake(&env, &cancelled).await;
    wait_for_acks(&env, 2, "the cancelled wake").await;

    assert_eq!(
        persona_voices(&env, &revka, cancelled_room).await,
        Vec::<String>::new(),
        "a cancelled schedule must not answer"
    );
    // A DELTA against phase 1, which is stronger than a fresh zero: it rules out a
    // turn that ran and merely failed to post.
    assert_eq!(
        scripted.chats.load(Ordering::SeqCst),
        1,
        "a cancelled schedule must not reach the model at all"
    );

    // ---- PHASE 3: a paused persona skips its occurrence, and the schedule LIVES.
    //
    // Two things at once. The pause route promises the persona stops answering and
    // a mention already honours it, so a scheduled wake must too. But a pause is
    // meant to be LIFTED, so skipping must not destroy the schedule — asserted on
    // the ROW, because "the agent did not answer" is equally true of a schedule
    // that was accidentally killed.
    let paused_room = "chan-paused";
    room(&env, paused_room, &human, &mira).await;
    {
        let mut config = env.store("config");
        config
            .put(&buzz::relay::keys::agent_paused(COMMUNITY, "mira"), b"1")
            .await
            .expect("pause the persona");
    }
    let created = env
        .handle(ask(
            &human,
            json!({
                "agent": "mira",
                "channel": paused_room,
                "prompt": "hourly check",
                "interval_ms": schedules::MIN_INTERVAL_MS,
                "occurrences": 3,
            }),
        ))
        .await
        .expect("the schedule route answers");
    assert_eq!(created.status, 201, "{}", error_text(&created));
    let recurring = body_json(&created)["schedule"]["id"]
        .as_str()
        .expect("a schedule id")
        .to_string();

    wait_for_acks(&env, 3, "the paused persona's wake").await;
    let advanced = row(&env, &recurring)
        .await
        .expect("the schedule still exists");
    assert!(
        advanced.occurrence > 0,
        "a paused agent's occurrence must still advance: {advanced:?}"
    );
    assert!(
        !advanced.cancelled,
        "a PAUSE must not cancel the schedule — it is meant to be lifted"
    );
    assert_eq!(
        advanced.remaining, 2,
        "exactly one occurrence was consumed: {advanced:?}"
    );
    assert_eq!(
        persona_voices(&env, &mira, paused_room).await,
        Vec::<String>::new(),
        "a paused persona must not answer a scheduled wake"
    );
    assert_eq!(
        scripted.chats.load(Ordering::SeqCst),
        1,
        "a paused persona must not reach the model"
    );

    // ---- PHASES 3a/3b: an ARCHIVED creator, and an ARCHIVED persona, get no turn.
    //
    // F232: an archived identity cannot write, so a schedule it made — or one
    // that wakes an archived persona — must not spend a model call on an answer
    // ingest would refuse. An archive can be lifted, so the occurrence is skipped
    // and the schedule lives, exactly like a pause. Rows are seeded directly
    // because the schedule route itself refuses an archived creator.
    for (phase, creator, agent, archived, acks) in [
        ("archived creator", Actor::new("archived-creator"), "revka", None, 4u64),
        ("archived persona", Actor::new("human"), "zed", Some(&zed), 5u64),
    ] {
        let channel = format!("chan-{}", phase.replace(' ', "-"));
        let persona = if agent == "zed" { &zed } else { &revka };
        room(&env, &channel, &creator, persona).await;
        let request = ScheduleRequest {
            agent: agent.into(),
            channel: channel.clone(),
            prompt: "say something".into(),
            first_due_ms: T0_MS,
            interval_ms: Some(schedules::MIN_INTERVAL_MS),
            occurrences: 3,
            created_by: creator.pk_hex(),
        };
        let schedule = schedules::build(&request, COMMUNITY, T0_MS).expect("builds");
        seed_live_row(&env, &schedule).await;
        let target = archived.unwrap_or(&creator);
        {
            let mut events = env.store("events");
            buzz_store::put_doc(
                &mut events,
                &buzz::relay::keys::archived_identity(COMMUNITY, &target.pk),
                &buzz::relay::archive::ArchivedIdentity {
                    pubkey: target.pk_hex(),
                    archived_at: T0_MS,
                    by: target.pk_hex(),
                    reason: None,
                },
            )
            .await
            .expect("archive seed");
        }
        publish_wake(&env, &schedule).await;
        wait_for_acks(&env, acks, phase).await;
        wait_for_remaining(&env, &schedule.id, 2, phase).await;
        assert_eq!(
            scripted.chats.load(Ordering::SeqCst),
            1,
            "{phase}: the wake must not reach the model"
        );
        assert_eq!(
            persona_voices(&env, persona, &channel).await,
            Vec::<String>::new(),
            "{phase}: the persona must not answer"
        );
        let after = row(&env, &schedule.id).await.expect("the schedule still exists");
        assert!(
            !after.cancelled,
            "{phase}: an archive can be lifted, so the schedule must live: {after:?}"
        );
        assert_eq!(after.remaining, 2, "{phase}: one occurrence skipped: {after:?}");
    }

    // ---- PHASE 4: a wake whose row does not exist is NOT acked away.
    //
    // The race this closes was found on a deployment: `create` publishes the wake
    // before writing the row, so a wake due NOW can be claimed first. Treated as
    // definitive, it was acked and destroyed — and the row appeared a moment later,
    // live and due, with nothing left to drive it. One schedule in five died so.
    //
    // Asserted from the queue rather than the row, because there IS no row: under
    // this harness's frozen clock a lease never expires, so an unacked message simply
    // stays leased and pending. That is precisely the observable the fix produces and
    // the exact opposite of the old behaviour.
    let mut queue = env.context().queue(turns::TURNS).expect("turn queue");
    // Let earlier phases' stragglers settle first: a settled occurrence
    // re-publishes its next wake after the commit, and that absorbed duplicate
    // is acked whenever it is delivered, so a baseline taken mid-flight would
    // credit a straggler's ack to the orphan below. Quiet = the ack count
    // unchanged for 2 s (bounded at 60 s).
    let mut acked_before = queue.counts().await.expect("queue counts").1;
    let mut quiet_ticks = 0;
    for _ in 0..600 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = queue.counts().await.expect("queue counts").1;
        if now == acked_before {
            quiet_ticks += 1;
            if quiet_ticks >= 20 {
                break;
            }
        } else {
            acked_before = now;
            quiet_ticks = 0;
        }
    }
    let orphan = Schedule {
        id: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".into(),
        agent: "revka".into(),
        channel: answered.into(),
        prompt: "a wake whose row was never written".into(),
        next_due_ms: T0_MS,
        interval_ms: None,
        remaining: 1,
        occurrence: 0,
        created_by: human.pk_hex(),
        created_ms: T0_MS,
        cancelled: false,
        revision: 0,
    };
    // Deliberately NO row for it.
    assert!(
        row(&env, &orphan.id).await.is_none(),
        "the point of this phase is that the row is absent"
    );
    publish_wake(&env, &orphan).await;

    // Give the consumer ample opportunity to take it and decide.
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (_published, acked_after, dlq, _pending) = queue.counts().await.expect("queue counts");
    assert_eq!(dlq, 0, "nothing should be dead-lettered here");
    assert_eq!(
        acked_after, acked_before,
        "a wake whose row is not there yet must be left for redelivery, not acked \
         away — acking it is what destroyed the wake and stranded a live schedule"
    );
    assert_eq!(
        scripted.chats.load(Ordering::SeqCst),
        1,
        "and waiting must cost nothing: no turn runs while it waits"
    );

    });
}

/// **A refused schedule stores nothing and says why.**
///
/// No pipeline: this exercises validation, and a turn loop it does not need is the
/// very thing that interferes with the tests that do.
///
/// Every refusal names its bound, because the caller is often a MODEL — "invalid
/// schedule" gives it nothing to correct, while a number gives it a next action.
#[test]
 fn severe_a_refused_schedule_stores_nothing_and_names_its_bound() {
    common::run_scenario(|| async move {
    let _serial = SCHEDULE_GATE_LOCK.lock().await;
    let env = schedule_env(None).await;
    let human = Actor::new("human");
    let revka = Actor::new("revka");
    let channel = "chan-refused";
    seed_personas(&env, &[("revka", &revka)]).await;
    room(&env, channel, &human, &revka).await;

    // A recurrence faster than the floor: a spend loop wearing a schedule's
    // clothes. The refusal must carry the number.
    let fast = env
        .handle(ask(
            &human,
            json!({
                "agent": "revka", "channel": channel, "prompt": "spin",
                "interval_ms": 1_000, "occurrences": 100,
            }),
        ))
        .await
        .expect("answers");
    assert_eq!(fast.status, 400, "{}", error_text(&fast));
    assert!(
        error_text(&fast).contains(&schedules::MIN_INTERVAL_MS.to_string()),
        "the refusal must name the bound: {}",
        error_text(&fast)
    );

    // An empty instruction would buy a model call and produce silence.
    let blank = env
        .handle(ask(
            &human,
            json!({ "agent": "revka", "channel": channel, "prompt": "   " }),
        ))
        .await
        .expect("answers");
    assert_eq!(blank.status, 400, "{}", error_text(&blank));

    // An agent nobody registered: 404, naming it.
    let ghost = env
        .handle(ask(
            &human,
            json!({ "agent": "nobody", "channel": channel, "prompt": "hello" }),
        ))
        .await
        .expect("answers");
    assert_eq!(ghost.status, 404, "{}", error_text(&ghost));

    // A room the caller cannot write to is refused before any row is written.
    let elsewhere = env
        .handle(ask(
            &human,
            json!({ "agent": "revka", "channel": "no-such-room", "prompt": "hello" }),
        ))
        .await
        .expect("answers");
    assert!(
        elsewhere.status == 404 || elsewhere.status == 403,
        "expected a refusal for an unknown room, got {}: {}",
        elsewhere.status,
        error_text(&elsewhere)
    );

    // NOTHING was scheduled. The whole point of refusing at the boundary.
    let mut config = env.store("config");
    let (rows, _truncated) = schedules::list(&mut config, COMMUNITY)
        .await
        .expect("list schedules");
    assert!(
        rows.is_empty(),
        "a refused schedule must store no row: {rows:?}"
    );

    });
}

/// The schedule ids a `POST /buzz/api/schedules/list` answered `who` with.
async fn listed(env: &TestEnv, who: &Actor) -> Vec<String> {
    let reply = env
        .handle(post(who, "/buzz/api/schedules/list", b"{}".to_vec()))
        .await
        .expect("answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    body_json(&reply)["schedules"]
        .as_array()
        .expect("a schedules array")
        .iter()
        .filter_map(|s| s["id"].as_str().map(str::to_string))
        .collect()
}

/// **A private room's schedules are not listed to people outside it.**
///
/// A schedule carries its prompt verbatim and the channel it posts into, and
/// creating one takes write access to that channel. The list used to return every
/// schedule in the community to any member, so someone outside a private room read
/// the prompts written for it and learned the room's id.
#[test]
 fn severe_the_schedule_list_hides_schedules_for_rooms_the_reader_cannot_read() {
    common::run_scenario(|| async move {
    let _serial = SCHEDULE_GATE_LOCK.lock().await;
    let env = schedule_env(None).await;
    let insider = Actor::new("sched-insider");
    let outsider = Actor::new("sched-outsider");
    let revka = Actor::new("revka");
    seed_personas(&env, &[("revka", &revka)]).await;
    seed_channel_with_owner(&env, "chan-secret", Visibility::Private, &insider).await;
    seed_membership(&env, "chan-secret", &revka, Role::Member).await;
    room(&env, "chan-open", &insider, &revka).await;

    let mut ids = Vec::new();
    for channel in ["chan-secret", "chan-open"] {
        let reply = env
            .handle(ask(
                &insider,
                json!({ "agent": "revka", "channel": channel, "prompt": "the private plan",
                        "first_due_ms": T0_MS + 3_600_000 }),
            ))
            .await
            .expect("answers");
        assert_eq!(reply.status, 201, "{}", error_text(&reply));
        ids.push(body_json(&reply)["schedule"]["id"].as_str().unwrap().to_string());
    }
    let (secret, open) = (&ids[0], &ids[1]);

    let theirs = listed(&env, &outsider).await;
    assert!(
        !theirs.contains(secret),
        "a private room's schedule was listed to a non-member: {theirs:?}"
    );
    assert!(theirs.contains(open), "an open room's schedule is still listed: {theirs:?}");

    let mine = listed(&env, &insider).await;
    assert!(mine.contains(secret) && mine.contains(open), "{mine:?}");

    });
}

/// **Concurrent creates cannot exceed the per-agent cap, and identical concurrent
/// creates leave one row that a later cancel is not undone on.**
///
/// The cap was a scan followed by an independent write, so creates racing for one
/// agent each saw room and all committed. And the first create's row write was
/// unconditional, so a slower identical create could overwrite a row the faster
/// one's caller had already cancelled. Driven directly against the fleet's planes,
/// with no turn consumer (the wakes are far in the future).
#[test]
 fn severe_concurrent_schedule_creates_respect_the_cap_and_never_resurrect() {
    common::run_scenario(|| async move {
    let _serial = SCHEDULE_GATE_LOCK.lock().await;
    let env = schedule_env(None).await;
    let ctx = env.context();
    let creator = Actor::new("sched-racer").pk_hex();
    let request = |n: usize| ScheduleRequest {
        agent: "revka".into(),
        channel: format!("chan-race-{n}"),
        prompt: "race".into(),
        first_due_ms: T0_MS + 3_600_000,
        interval_ms: None,
        occurrences: 1,
        created_by: creator.clone(),
    };

    let attempts = schedules::MAX_PER_AGENT + 8;
    let mut tasks = Vec::new();
    for n in 0..attempts {
        let ctx = ctx.clone();
        let request = request(n);
        let mut config = env.store("config");
        tasks.push(tokio::spawn(async move {
            schedules::create(&ctx, &mut config, COMMUNITY, &request, "", T0_MS).await
        }));
    }
    let mut created = 0usize;
    for task in tasks {
        if task.await.expect("joins").is_ok() {
            created += 1;
        }
    }
    let mut config = env.store("config");
    let (rows, _) = schedules::list(&mut config, COMMUNITY).await.expect("lists");
    let live = rows.iter().filter(|r| r.agent == "revka" && !r.cancelled && r.remaining > 0).count();
    assert!(
        live <= schedules::MAX_PER_AGENT,
        "{live} live schedules for one agent after {attempts} concurrent creates ({created} answered Ok)"
    );
    assert!(created >= 1, "the cap must still admit creates");

    // Identical concurrent creates: one row, and it survives a cancel that lands
    // after them unchanged.
    let same = ScheduleRequest { agent: "mira".into(), ..request(0) };
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let ctx = ctx.clone();
        let same = same.clone();
        let mut config = env.store("config");
        tasks.push(tokio::spawn(async move {
            schedules::create(&ctx, &mut config, COMMUNITY, &same, "", T0_MS).await
        }));
    }
    let mut ids = std::collections::BTreeSet::new();
    for task in tasks {
        if let Ok(row) = task.await.expect("joins") {
            ids.insert(row.id);
        }
    }
    assert_eq!(ids.len(), 1, "identical requests are one schedule: {ids:?}");
    let id = ids.into_iter().next().unwrap();
    assert!(schedules::cancel(&ctx, &mut config, COMMUNITY, &id, &creator, false)
        .await
        .expect("cancels"));
    let slots: schedules::AgentSlots = buzz_store::get_doc(
        &mut config,
        schedules::agent_slots_key(COMMUNITY, "mira").as_bytes(),
    )
    .await
    .expect("read slots")
    .unwrap_or_default();
    assert!(
        !slots.slots.iter().any(|slot| slot.id == id),
        "a cancel releases its capacity at once"
    );
    // A re-create of a cancelled schedule revives it as a NEW revision rather than
    // silently rewriting the old one.
    let stored = schedules::get(&mut config, COMMUNITY, &id).await.expect("reads").expect("row");
    assert!(stored.cancelled, "the cancel stands: {stored:?}");
    let revived = schedules::create(&ctx, &mut config, COMMUNITY, &same, "", T0_MS)
        .await
        .expect("re-creates");
    assert_eq!(revived.revision, stored.revision + 1, "a revival is a new revision");
    let slots: schedules::AgentSlots = buzz_store::get_doc(
        &mut config,
        schedules::agent_slots_key(COMMUNITY, "mira").as_bytes(),
    )
    .await
    .expect("read slots")
    .unwrap_or_default();
    assert!(
        slots.slots.iter().any(|slot| slot.id == id && slot.revision == revived.revision),
        "the re-create holds the slot for ITS revision, so a late release of the old one cannot touch it"
    );
    assert!(
        !schedules::slot_admits(Some(&slots), &id, stored.revision),
        "and the cancelled revision is no longer admitted"
    );
    assert_ne!(
        schedules::occurrence_admission(&id, revived.revision, 0),
        schedules::occurrence_admission(&id, stored.revision, 0),
        "its runs are not absorbed as the cancelled revision's"
    );
    });
}

/// **Schedules made before the per-agent slot record existed still count.**
///
/// The slot record is seeded from the agent's live rows when first written, so an
/// agent already holding `MAX_PER_AGENT - 1` schedules cannot be pushed over the cap
/// by two creates racing to write that first record.
#[test]
 fn severe_pre_existing_schedules_count_against_the_cap() {
    common::run_scenario(|| async move {
    let _serial = SCHEDULE_GATE_LOCK.lock().await;
    let env = schedule_env(None).await;
    let ctx = env.context();
    let creator = Actor::new("sched-legacy").pk_hex();
    let request = |n: usize| ScheduleRequest {
        agent: "legacy".into(),
        channel: format!("chan-legacy-{n}"),
        prompt: "legacy".into(),
        first_due_ms: T0_MS + 3_600_000,
        interval_ms: None,
        occurrences: 1,
        created_by: creator.clone(),
    };
    // Rows written directly, as an earlier build without slot records left them.
    {
        let mut config = env.store("config");
        for n in 0..schedules::MAX_PER_AGENT - 1 {
            let row = schedules::build(&request(n), COMMUNITY, T0_MS).expect("builds");
            buzz_store::put_doc(&mut config, schedules::schedule_key(COMMUNITY, &row.id).as_bytes(), &row)
                .await
                .expect("seed a legacy row");
        }
    }
    let mut tasks = Vec::new();
    for n in 100..104 {
        let ctx = ctx.clone();
        let request = request(n);
        let mut config = env.store("config");
        tasks.push(tokio::spawn(async move {
            schedules::create(&ctx, &mut config, COMMUNITY, &request, "", T0_MS).await
        }));
    }
    let mut admitted = 0;
    for task in tasks {
        if task.await.expect("joins").is_ok() {
            admitted += 1;
        }
    }
    let mut config = env.store("config");
    let (rows, _) = schedules::list(&mut config, COMMUNITY).await.expect("lists");
    let live = rows.iter().filter(|r| r.agent == "legacy" && !r.cancelled && r.remaining > 0).count();
    assert_eq!(admitted, 1, "exactly one slot was left");
    assert_eq!(live, schedules::MAX_PER_AGENT, "{live} live schedules");
    });
}

/// **An agent over the cap from before the slot record is capped at its next wake,
/// even if it never creates another schedule.**
///
/// Without a slot record every legacy schedule used to run. The claim path now
/// seeds the record itself, admitting the oldest `MAX_PER_AGENT`; the excess is
/// stopped and cancelled when it wakes.
#[test]
 fn severe_a_legacy_agent_over_the_cap_is_capped_at_its_next_wake() {
    common::run_scenario(|| async move {
    let _serial = SCHEDULE_GATE_LOCK.lock().await;
    let env = schedule_env(None).await;
    let creator = Actor::new("sched-over").pk_hex();
    let mut rows = Vec::new();
    {
        let mut config = env.store("config");
        for n in 0..schedules::MAX_PER_AGENT + 1 {
            let request = ScheduleRequest {
                agent: "overcap".into(),
                channel: format!("chan-over-{n}"),
                prompt: "legacy".into(),
                first_due_ms: T0_MS,
                interval_ms: None,
                occurrences: 1,
                created_by: creator.clone(),
            };
            // Distinct creation times: oldest first is the seeding order.
            let row = schedules::build(&request, COMMUNITY, T0_MS - 1_000 + n as u64).expect("builds");
            buzz_store::put_doc(&mut config, schedules::schedule_key(COMMUNITY, &row.id).as_bytes(), &row)
                .await
                .expect("seed a legacy row");
            rows.push(row);
        }
    }
    let wake = |row: &Schedule| ScheduledWake { id: row.id.clone(), occurrence: 0, revision: 0 };
    let mut config = env.store("config");
    let oldest = schedules::claim(&mut config, COMMUNITY, &wake(&rows[0]), T0_MS).await.expect("claims");
    assert!(matches!(oldest, schedules::Claim::Run(_)), "the oldest legacy schedule keeps its slot: {oldest:?}");
    let newest = rows.last().unwrap();
    let over = schedules::claim(&mut config, COMMUNITY, &wake(newest), T0_MS).await.expect("claims");
    assert_eq!(over, schedules::Claim::Stop, "the schedule beyond the cap must not spend");
    let stored = schedules::get(&mut config, COMMUNITY, &newest.id).await.expect("reads").expect("row");
    assert!(stored.cancelled, "and it is marked cancelled rather than left looking live");
    let slots: schedules::AgentSlots = buzz_store::get_doc(
        &mut config,
        schedules::agent_slots_key(COMMUNITY, "overcap").as_bytes(),
    )
    .await
    .expect("read slots")
    .expect("the claim seeded the record");
    assert_eq!(slots.slots.len(), schedules::MAX_PER_AGENT);
    });
}

/// **Principal namespace isolation (`finding_be34caaff4d24c9d`).**
///
/// 1. 64-hex member pubkeys cannot be registered as persona `agent` IDs via
///    `POST /buzz/api/agents` or `Registry::parse`.
/// 2. Persona-created schedules and member-created schedules occupy distinct
///    creator namespaces: a persona cannot cancel a member's schedule even if
///    given a colliding string, and a member cannot read or cancel a persona's
///    private-room self-schedule.
#[test]
fn severe_schedule_creator_principal_namespace_isolates_members_and_personas() {
    common::run_scenario(|| async move {
        let _serial = SCHEDULE_GATE_LOCK.lock().await;
        let env = schedule_env(None).await;
        let moderator = Actor::new("sched-ns-mod");
        let attacker = Actor::new("sched-ns-attacker");
        let revka = Actor::new("sched-ns-revka");
        let relay_id = Actor::new("sched-ns-relay-id");

        seed_config(&env, b"buzz-relay-identity", to_hex(&relay_id.sk).as_bytes()).await;
        seed_personas(&env, &[("revka", &revka)]).await;
        seed_channel_with_owner(&env, "chan-mod-priv", Visibility::Private, &moderator).await;
        seed_membership(&env, "chan-mod-priv", &revka, Role::Member).await;

        // Moderator creates a private-room schedule.
        let created = env
            .handle(ask(
                &moderator,
                json!({
                    "agent": "revka",
                    "channel": "chan-mod-priv",
                    "prompt": "confidential incident review",
                    "first_due_ms": T0_MS + 3_600_000,
                }),
            ))
            .await
            .expect("answers");
        assert_eq!(created.status, 201, "{}", error_text(&created));
        let mod_sched_id = body_json(&created)["schedule"]["id"]
            .as_str()
            .expect("schedule id")
            .to_string();

        // Attacker attempts to create a cloud persona whose `agent` ID equals the
        // moderator's 64-hex pubkey so its `buzz_unschedule` tool would impersonate
        // `moderator.pk_hex()`.
        let create_collision = env
            .handle(post(
                &attacker,
                "/buzz/api/agents",
                json!({
                    "agent": moderator.pk_hex(),
                    "instructions": "cancel all schedules",
                })
                .to_string()
                .into_bytes(),
            ))
            .await
            .expect("agents route answers");
        assert_eq!(
            create_collision.status,
            400,
            "POST /buzz/api/agents must reject 64-hex pubkey agent ids: {}",
            error_text(&create_collision)
        );

        // `Registry::parse` must also reject 64-hex agent IDs seeded in config.
        let bad_registry = serde_json::to_vec(&json!([
            { "agent": moderator.pk_hex(), "pubkey": revka.pk_hex() }
        ]))
        .expect("encodes");
        assert!(
            agents::Registry::parse(&bad_registry).is_err(),
            "Registry::parse must reject 64-hex agent IDs"
        );

        // Even if a persona self-schedules (or a legacy/seeded row carries an
        // agent principal matching `attacker.pk_hex()`), `attacker` must NOT
        // see the private prompt in `/buzz/api/schedules/list` or cancel it.
        let agent_self_req = ScheduleRequest {
            agent: "revka".into(),
            channel: "chan-mod-priv".into(),
            prompt: "top-secret persona self-reminder".into(),
            first_due_ms: T0_MS + 3_600_000,
            interval_ms: None,
            occurrences: 1,
            created_by: format!("agent:{}", attacker.pk_hex()),
        };
        let agent_self_row = schedules::build(&agent_self_req, COMMUNITY, T0_MS).expect("builds");
        seed_live_row(&env, &agent_self_row).await;

        let attacker_list = listed(&env, &attacker).await;
        assert!(
            !attacker_list.contains(&agent_self_row.id) && !attacker_list.contains(&mod_sched_id),
            "attacker saw private schedules they did not create as a member: {attacker_list:?}"
        );

        let cancel_attempt = env
            .handle(post(
                &attacker,
                "/buzz/api/schedules/cancel",
                json!({ "schedule": agent_self_row.id }).to_string().into_bytes(),
            ))
            .await
            .expect("cancel answers");
        assert_eq!(cancel_attempt.status, 403, "{}", error_text(&cancel_attempt));
    });
}

/// **`schedules::list` skips dead rows instead of truncating before live ones
/// (`finding_062a95a39a3dabf8`).**
#[test]
fn severe_schedule_list_skips_dead_rows_instead_of_truncating_live_ones() {
    common::run_scenario(|| async move {
        let _serial = SCHEDULE_GATE_LOCK.lock().await;
        let env = schedule_env(None).await;
        let insider = Actor::new("sched-list-insider");
        let revka = Actor::new("sched-list-revka");
        seed_personas(&env, &[("revka", &revka)]).await;
        room(&env, "chan-live-sched", &insider, &revka).await;

        // Pick a live schedule whose SHA-256 ID sorts near the end of the hex space.
        let live_row = (0..32)
            .map(|i| {
                schedules::build(
                    &ScheduleRequest {
                        agent: "revka".into(),
                        channel: "chan-live-sched".into(),
                        prompt: format!("live reminder candidate {i}"),
                        first_due_ms: T0_MS + 3_600_000,
                        interval_ms: None,
                        occurrences: 1,
                        created_by: insider.pk_hex(),
                    },
                    COMMUNITY,
                    T0_MS + 500,
                )
                .expect("builds")
            })
            .max_by(|a, b| a.id.cmp(&b.id))
            .expect("at least one candidate");

        // Seed > MAX_LISTED dead (cancelled / exhausted) rows that sort BEFORE `live_row.id`.
        let mut dead_before_live = 0usize;
        {
            let mut config = env.store("config");
            let mut n = 0usize;
            while dead_before_live < schedules::MAX_LISTED + 16 {
                let mut dead = schedules::build(
                    &ScheduleRequest {
                        agent: "revka".into(),
                        channel: "chan-live-sched".into(),
                        prompt: format!("historical dead schedule {n}"),
                        first_due_ms: T0_MS + 60_000,
                        interval_ms: None,
                        occurrences: 1,
                        created_by: insider.pk_hex(),
                    },
                    COMMUNITY,
                    T0_MS,
                )
                .expect("builds");
                if n % 2 == 0 {
                    dead.cancelled = true;
                } else {
                    dead.remaining = 0;
                }
                if dead.id < live_row.id {
                    dead_before_live += 1;
                }
                buzz_store::put_doc(
                    &mut config,
                    schedules::schedule_key(COMMUNITY, &dead.id).as_bytes(),
                    &dead,
                )
                .await
                .expect("seed dead schedule");
                n += 1;
            }
        }
        seed_live_row(&env, &live_row).await;

        let visible = listed(&env, &insider).await;
        assert!(
            visible.contains(&live_row.id),
            "dead historical schedules hid a live schedule ({dead_before_live} dead rows before {}): {visible:?}",
            live_row.id
        );
    });
}
