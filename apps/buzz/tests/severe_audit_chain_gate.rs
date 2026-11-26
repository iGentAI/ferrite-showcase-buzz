//! The audit chain under CONCURRENCY.
//!
//! The chain is per community and its head is ONE key on the guards plane, bumped
//! by every admitted event and by every privileged route. That makes concurrency
//! the interesting axis, and nothing covered it: `audit.rs`'s own tests build
//! chains in a loop, which can never contend.
//!
//! Two properties here, both of which an auditor relies on:
//!
//! * **A privileged action that SUCCEEDED leaves a line.** Dropping it is worse
//!   than a slow write, because the tamper-evident chain's whole claim is that
//!   nothing is missing from it.
//! * **A burst of concurrent writes is not refused.** The chain must not turn
//!   ordinary write concurrency into client-visible failures.
//!
//! Both are driven through the real app on a real fleet, and both read the chain
//! back with the app's own verifier.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use buzz::relay::audit::{verify_chain, AuditAction, GENESIS_PREV};
use buzz::relay::channel::Visibility;
use buzz::relay::crypto::sha256::sha256;
use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::{kind, operator, store as buzz_store};
use common::{
    Actor, COMMUNITY, T0_SECS, audit_entries, audit_head, auth_header, gate_env, error_text,
    gate_with, request_with_auth, seed_channel_with_owner, signed, stored_event, submit_all,
    tag,
};
use ferrite_app::serde_json::{self, json};
use ferrite_app::IngressRequest;
use common::SsoTestEnv as TestEnv;

/// Concurrent privileged writes. Each one appends a line, so a correct relay
/// ends with exactly this many `ConfigUpdated` entries.
const PRIVILEGED_WRITES: usize = 16;

/// Concurrent event submissions, set to the api loop's own `MAX_IN_FLIGHT`.
///
/// That is the point of the number: 32 is the most contention a single instance
/// can generate on the chain head, so it is the honest worst case for one
/// instance rather than an arbitrary load figure. It also sits ABOVE the event
/// path's fixed 16-round retry budget (`ingest::GUARD_RETRIES`), and each round
/// admits exactly one winner — so if the budget is what bounds write concurrency,
/// this is where it shows, as a 409 on an event that is in no way invalid.
const CONCURRENT_EVENTS: usize = 32;

static OP_NONCE: AtomicU64 = AtomicU64::new(0);

fn nonce() -> String {
    format!("audit-op{}", OP_NONCE.fetch_add(1, Ordering::Relaxed))
}

/// A POST whose NIP-98 header also commits to the body via the `payload` tag,
/// which every write in the operator family requires.
fn operator_post(actor: &Actor, path: &str, body: Vec<u8>) -> IngressRequest {
    let event = Event::sign_new(
        &actor.sk,
        kind::KIND_HTTP_AUTH,
        vec![
            vec!["u".into(), format!("https://buzz.test{path}")],
            vec!["method".into(), "POST".into()],
            vec!["payload".into(), to_hex(&sha256(&body))],
            vec!["nonce".into(), nonce()],
        ],
        "",
        T0_SECS,
    )
    .expect("authorization events sign");
    request_with_auth("POST", path, Some(&auth_header(&event)), body)
}

async fn seed_operator(env: &TestEnv, operator_actor: &Actor) {
    let mut config = env.store("config");
    let set = serde_json::to_vec(&json!([operator_actor.pk_hex()])).expect("set encodes");
    buzz_store::put_raw(&mut config, operator::OPERATOR_SET_CONFIG, &set)
        .await
        .expect("operator seed");
}

/// The chain, verified with the app's OWN verifier, plus its length.
///
/// Verification is not decoration here: a gate that only counted rows could not
/// tell a correctly-appended entry from one written with a broken link.
async fn verified_chain(env: &TestEnv) -> Vec<buzz::relay::audit::AuditEntry> {
    let entries = audit_entries(env).await;
    let head = audit_head(env)
        .await
        .expect("a head once anything is audited");
    assert_eq!(
        entries.len() as u64,
        head.seq,
        "the head claims {} entries but only {} exist — the chain has a HOLE, which \
         is the one thing a hash chain is supposed to make impossible",
        head.seq,
        entries.len()
    );
    verify_chain(COMMUNITY, &entries, &GENESIS_PREV).expect("the chain must verify");
    entries
}

/// How many entries record `action` against `object`.
fn count_of(
    entries: &[buzz::relay::audit::AuditEntry],
    action: AuditAction,
    object: &str,
) -> usize {
    entries
        .iter()
        .filter(|e| e.action == action && e.object.as_deref() == Some(object))
        .count()
}

/// **A privileged action that succeeded must leave an audit line, even when the
/// chain's head is contended.**
///
/// The three routes that audit outside the ingest saga — channel creation, an
/// operator config write, and a UI bundle commit — are the most privileged actions
/// the app has. If a contended head can drop their lines, then an operator's
/// config change can be made unauditable by anything that generates chain traffic,
/// which every ordinary member can do just by posting.
#[test]
 fn severe_a_privileged_action_that_succeeds_is_always_audited() {
    common::run_scenario(|| async move {
    let gate = gate_env().await;
    let op = Actor::new("audit-chain-operator");
    seed_operator(&gate, &op).await;

    // Unconditional writes (no `expected_version`), so nothing here can fail for
    // a reason of its own and every 200 owes the chain exactly one line.
    let writes: Vec<IngressRequest> = (0..PRIVILEGED_WRITES)
        .map(|i| {
            operator_post(
                &op,
                "/buzz/api/operator/config/buzz-relay-url",
                json!({ "value": format!("wss://relay-{i}.example/buzz/ws/") })
                    .to_string()
                    .into_bytes(),
            )
        })
        .collect();

    // Raw `handle_all`, deliberately, where `burst_holds` uses `submit_all`. A lost
    // reply here cannot be answered by re-sending: each write carries a DIFFERENT
    // value, so a re-send after a merely-lost reply would append a SECOND audit line
    // and break the exact-count assertion this test exists for. An event submission
    // is idempotent under its own id; a config write is not.
    let replies = gate.handle_all(writes).await;
    let mut accepted = 0usize;
    for (i, reply) in replies.iter().enumerate() {
        let reply = reply.as_ref().unwrap_or_else(|e| panic!("write {i}: {e}"));
        assert_eq!(
            reply.status,
            200,
            "write {i} was refused: {}",
            String::from_utf8_lossy(&reply.body)
        );
        accepted += 1;
    }
    assert_eq!(accepted, PRIVILEGED_WRITES);

    let entries = verified_chain(&gate).await;
    let audited = count_of(&entries, AuditAction::ConfigUpdated, "buzz-relay-url");
    assert_eq!(
        audited, PRIVILEGED_WRITES,
        "{accepted} privileged writes SUCCEEDED but only {audited} were audited — \
         a contended chain head is dropping the record of privileged actions"
    );

    });
}

/// Drive `writers` concurrent submissions into `room` and require that the chain
/// refused none of them and recorded each exactly once.
///
/// Shared by the single-instance and scaled-out cases so that the only difference
/// between them is the thing under test: how many instances are competing for the
/// head.
async fn burst_holds(gate: &TestEnv, room: &str, label: &str, writers: usize) {
    let owner = Actor::new(&format!("{label}-owner"));
    seed_channel_with_owner(gate, room, Visibility::Open, &owner).await;

    // A distinct author each, so nothing contends on a replaceable-event head and
    // the ONLY shared write target is the audit chain itself.
    //
    // Driven through `submit_all` rather than `handle_all`, which is the same
    // concurrent drive PLUS the re-send a real client performs for a lost reply. That
    // is load-bearing, not tidiness: under the gates' FROZEN clock a lease granted to
    // a long-poll whose client-side timeout fires in the same instant never expires,
    // so the one reply it holds is never redelivered — a missed reply is permanent
    // rather than slow. This gate panicked on exactly that under full-suite load
    // (`submission 2: no reply for test-3`), and because cargo stops at the first
    // failing binary it took 34 later tests with it.
    //
    // The concurrency under test is unchanged: every writer still submits at once and
    // only the ANSWER is recovered. The re-send is absorbed by the admission, because
    // an event's identity is its id.
    let work: Vec<(Actor, Event)> = (0..writers)
        .map(|i| {
            let author = Actor::new(&format!("{label}-author-{i}"));
            let event = signed(
                &author,
                kind::KIND_STREAM_MESSAGE,
                vec![tag("h", room)],
                &format!("{label} message {i}"),
            );
            (author, event)
        })
        .collect();
    let events: Vec<Event> = work.iter().map(|(_, event)| event.clone()).collect();

    let replies = submit_all(gate, &work).await;
    let mut refused: Vec<(usize, u16, String)> = Vec::new();
    for (i, reply) in replies.iter().enumerate() {
        if reply.status != 200 {
            refused.push((i, reply.status, error_text(reply)));
        }
    }
    assert!(
        refused.is_empty(),
        "{} of {writers} concurrent submissions were REFUSED, and none of them is \
         invalid: {refused:?}",
        refused.len()
    );

    // Every accepted event is stored, and audited EXACTLY once.
    let entries = verified_chain(gate).await;
    let mut audited: BTreeMap<String, usize> = BTreeMap::new();
    for entry in &entries {
        if entry.action == AuditAction::EventCreated {
            if let Some(object) = entry.object.clone() {
                *audited.entry(object).or_default() += 1;
            }
        }
    }
    for (i, event) in events.iter().enumerate() {
        let id = to_hex(&event.id);
        assert!(
            stored_event(gate, &event.id).await.is_some(),
            "event {i} was accepted but is not stored"
        );
        assert_eq!(
            audited.get(&id).copied().unwrap_or(0),
            1,
            "event {i} ({id}) is audited {:?} times, not once",
            audited.get(&id)
        );
    }
}

/// **A burst of concurrent writes in one community must not be refused.**
///
/// Every admitted event appends to the same head, and the event path retries a
/// fixed number of rounds while each round admits exactly one winner. This drives
/// more writers than that budget: if the chain is what bounds write concurrency,
/// a perfectly valid event comes back refused.
#[test]
 fn severe_a_burst_of_concurrent_writes_is_not_refused_by_the_chain() {
    common::run_scenario(|| async move {
    let gate = gate_env().await;
    burst_holds(&gate, "chan-audit-burst", "burst", CONCURRENT_EVENTS).await;

    });
}

/// **The chain must not start refusing writes because the deployment SCALED OUT.**
///
/// This is what the "the audit chain serialises every accepted event" concern is
/// really about. One instance can only ever have `MAX_IN_FLIGHT` writers in the
/// air; two instances have twice that, competing for the same single head with the
/// same fixed retry budget. A relay whose writes fail when an operator adds an
/// instance has a scaling bug, not a tuning question — so the number here is
/// deliberately two instances' worth of ceiling.
#[test]
 fn severe_the_chain_holds_when_the_deployment_scales_out() {
    common::run_scenario(|| async move {
    let gate = gate_with(2).await;
    burst_holds(
        &gate,
        "chan-audit-scaleout",
        "scaleout",
        CONCURRENT_EVENTS * 2,
    )
    .await;

    });
}
