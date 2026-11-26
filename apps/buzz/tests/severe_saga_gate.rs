//! **S17 — saga resumption.** Every crash window in the ingest saga, engineered
//! against a live fleet.
//!
//! Ingest is a guarded saga because the app is a competing consumer over a queue
//! that redelivers: admission elects one writer, each effect is idempotent and
//! self-recognizing, and the triggering envelope stays unacked until the saga
//! completes. The failure mode that matters is therefore not "does the happy path
//! work" but "what does the state look like after a driver dies mid-saga and
//! something re-drives it".
//!
//! A crash is engineered the honest way: the gate performs the saga's first
//! step(s) itself, through the same public entry points the app uses and against
//! the same planes, and then calls the ROUTE for the same event. The route's
//! duplicate path must finish what the dead driver started, and the end state must
//! be exactly one of everything.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::collections::BTreeSet;

use buzz::relay::audit::{verify_chain, GENESIS_PREV};
use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::ingest::{self, SagaState};
use buzz::relay::kind;
use common::{
    Actor, COMMUNITY, FEED_GENERATION, T0_MS, T0_SECS, admission, audit_entries,
    audit_entries_for, audit_head, audit_ref, body_json, gate_env, error_text,
    feed_copies, feed_records, gate_with, index_keys, query, returned, returned_ids,
    seed_channel, signed, signed_at, stored_event, stored_event_count, submit, submit_all,
    submit_request, tag,
};
use ferrite_app::serde_json::json;

/// Admit `event` without applying any effect — the state a driver leaves behind
/// when it dies immediately after winning the admission election.
async fn admit_only(env: &ferrite_app_test::TestEnv, event: &Event, channel: Option<String>) {
    let mut ape = env.ape("guards");
    let outcome = ingest::admit(&mut ape, COMMUNITY, event, channel, T0_MS)
        .await
        .expect("admission");
    assert!(
        !outcome.was_duplicate(),
        "the gate must be the FIRST admitter"
    );
    assert_eq!(outcome.doc().state, SagaState::Admitted);
}

/// The window between admission and any effect: the route must finish the saga.
#[test]
 fn severe_a_crash_right_after_admission_is_completed_by_the_next_call() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let event = signed(&alice, kind::KIND_TEXT_NOTE, vec![], "half-written");

    admit_only(&env, &event, None).await;

    // The half-state: admitted, nothing else. A reader must see NOTHING.
    let saga = admission(&env, &event.id).await.expect("the saga document");
    assert_eq!(saga.state, SagaState::Admitted);
    assert_eq!(saga.audit_seq, None);
    assert!(stored_event(&env, &event.id).await.is_none());
    assert!(audit_head(&env).await.is_none());
    assert_eq!(feed_records(&env, FEED_GENERATION).await.len(), 0);
    let page = query(&env, &alice, &json!({})).await;
    assert_eq!(
        returned_ids(&page),
        Vec::<String>::new(),
        "an admitted-but-unapplied event must not be readable"
    );

    // The resubmit (or, in production, the redelivered envelope) drives it home.
    let reply = submit(&env, &alice, &event).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(
        body_json(&reply)["message"],
        json!("duplicate:"),
        "the second caller found an existing admission"
    );

    // Exactly one of everything.
    let saga = admission(&env, &event.id).await.expect("the saga document");
    assert_eq!(saga.state, SagaState::Complete);
    assert_eq!(saga.audit_seq, Some(1));
    assert_eq!(stored_event(&env, &event.id).await.as_ref(), Some(&event));
    assert_eq!(stored_event_count(&env).await, 1);
    let chain = audit_entries_for(&env, &event.id).await;
    assert_eq!(chain.len(), 1, "one entry for one event: {chain:?}");
    assert_eq!(audit_ref(&env, &event.id).await, Some(1));
    verify_chain(COMMUNITY, &audit_entries(&env).await, &GENESIS_PREV).expect("the chain verifies");
    assert_eq!(feed_copies(&env, FEED_GENERATION, &event.id).await, 1);
    // And it is readable now.
    assert!(returned(&query(&env, &alice, &json!({})).await, &event));

    });
}

/// The window between the storage writes and the audit append: the re-drive
/// rewrites identical bytes and appends exactly one entry.
#[test]
 fn severe_a_crash_between_storage_and_audit_audits_exactly_once() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let channel = "chan-resume";
    seed_channel(
        &env,
        channel,
        buzz::relay::channel::Visibility::Open,
        &alice,
    )
    .await;
    let event = signed(
        &alice,
        kind::KIND_TEXT_NOTE,
        vec![tag("h", channel)],
        "stored, not audited",
    );

    admit_only(&env, &event, Some(channel.to_string())).await;
    {
        let mut store = env.store("events");
        ingest::apply_storage(&mut store, COMMUNITY, &event, Some(channel))
            .await
            .expect("storage");
    }

    // The body and its index rows exist, but the event was never audited and never
    // delivered.
    assert_eq!(stored_event(&env, &event.id).await.as_ref(), Some(&event));
    assert_eq!(
        index_keys(
            &env,
            &buzz::relay::keys::ts_prefix(COMMUNITY, Some(channel))
        )
        .await
        .len(),
        1
    );
    assert!(audit_head(&env).await.is_none());
    assert_eq!(feed_records(&env, FEED_GENERATION).await.len(), 0);

    let reply = submit(&env, &alice, &event).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(body_json(&reply)["message"], json!("duplicate:"));

    // The rewrite was byte-identical (no second copy, no changed row) and the
    // audit happened once.
    assert_eq!(stored_event_count(&env).await, 1);
    assert_eq!(stored_event(&env, &event.id).await.as_ref(), Some(&event));
    assert_eq!(
        index_keys(
            &env,
            &buzz::relay::keys::ts_prefix(COMMUNITY, Some(channel))
        )
        .await
        .len(),
        1,
        "the re-drive duplicated an index row"
    );
    assert_eq!(audit_entries_for(&env, &event.id).await.len(), 1);
    assert_eq!(feed_copies(&env, FEED_GENERATION, &event.id).await, 1);
    assert!(admission(&env, &event.id)
        .await
        .expect("saga")
        .is_complete());

    });
}

/// The append's own race, driven directly: many drivers of ONE event, each on its
/// own client, all start with no marker and fight for the head. Exactly one line
/// may be written and every driver must be told that line's sequence. Until
/// 2026-09-19 the first round read the marker BEFORE the head, so a driver that
/// read "no marker", then read the winner's new head, committed a second line
/// under the same marker; `severe_two_concurrent_submissions_produce_one_audit_entry`
/// caught it on main about one run in several. Each event here is sixteen
/// chances at that window, and other events' appends keep the head moving.
#[test]
 fn severe_many_drivers_of_one_event_append_exactly_one_audit_entry() {
    common::run_scenario(|| async move {
    const EVENTS: usize = 12;
    const DRIVERS: usize = 16;
    let env = gate_env().await;
    let alice = Actor::new("alice");

    for round in 0..EVENTS {
        let event = signed(&alice, kind::KIND_TEXT_NOTE, vec![], &format!("raced {round}"));
        let mut drivers = Vec::with_capacity(DRIVERS);
        for _ in 0..DRIVERS {
            let mut ape = env.ape("guards");
            let event = event.clone();
            drivers.push(tokio::spawn(async move {
                ingest::append_audit(&mut ape, COMMUNITY, &event, T0_MS).await
            }));
        }
        let mut seqs = BTreeSet::new();
        for driver in drivers {
            seqs.insert(driver.await.expect("the driver ran").expect("the append answers"));
        }
        assert_eq!(
            seqs,
            BTreeSet::from([round as u64 + 1]),
            "every driver of event {round} is told the one sequence it was recorded at"
        );
        let chain = audit_entries(&env).await;
        assert_eq!(chain.len(), round + 1, "event {round} was recorded more than once: {chain:?}");
    }
    let chain = audit_entries(&env).await;
    verify_chain(COMMUNITY, &chain, &GENESIS_PREV).expect("the chain verifies");

    });
}

/// The window between the audit append and the saga completion — the nastiest
/// one, because the resumed driver must recognize its OWN marker rather than
/// appending a second entry or wedging on a guard it moved itself.
#[test]
 fn severe_a_crash_between_audit_and_completion_appends_no_second_entry() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let event = signed(&alice, kind::KIND_TEXT_NOTE, vec![], "audited, unfinished");

    admit_only(&env, &event, None).await;
    {
        let mut store = env.store("events");
        ingest::apply_storage(&mut store, COMMUNITY, &event, None)
            .await
            .expect("storage");
    }
    let seq = {
        let mut ape = env.ape("guards");
        ingest::append_audit(&mut ape, COMMUNITY, &event, T0_MS)
            .await
            .expect("audit append")
    };
    assert_eq!(seq, 1);
    let head_before = audit_head(&env).await.expect("a head");
    assert_eq!(head_before.seq, 1);
    // The saga is still incomplete, and the event was never delivered.
    assert_eq!(
        admission(&env, &event.id).await.expect("saga").state,
        SagaState::Admitted
    );
    assert_eq!(feed_records(&env, FEED_GENERATION).await.len(), 0);

    let reply = submit(&env, &alice, &event).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(body_json(&reply)["message"], json!("duplicate:"));

    // The chain did not move, the saga records the SAME sequence, and the event
    // finally reached the feed.
    assert_eq!(audit_head(&env).await.as_ref(), Some(&head_before));
    let chain = audit_entries(&env).await;
    assert_eq!(chain.len(), 1, "a second entry was appended: {chain:?}");
    verify_chain(COMMUNITY, &chain, &GENESIS_PREV).expect("the chain verifies");
    let saga = admission(&env, &event.id).await.expect("saga");
    assert_eq!(saga.state, SagaState::Complete);
    assert_eq!(saga.audit_seq, Some(1));
    assert_eq!(feed_copies(&env, FEED_GENERATION, &event.id).await, 1);

    });
}

/// Two callers submit the SAME event at the same time, against two competing app
/// instances. Exactly one wins admission; the audit chain gains exactly one entry.
#[test]
 fn severe_two_concurrent_submissions_produce_one_audit_entry() {
    common::run_scenario(|| async move {
    let env = gate_with(2).await;
    let alice = Actor::new("alice");
    let event = signed(&alice, kind::KIND_TEXT_NOTE, vec![], "raced");

    let mut replies = env.handle_all([
        submit_request(&alice, &event),
        submit_request(&alice, &event),
    ]).await.into_iter();
    let left = replies.next().expect("the first concurrent reply");
    let right = replies.next().expect("the second concurrent reply");
    assert!(replies.next().is_none(), "exactly two requests were submitted");
    let left = left.expect("a reply");
    let right = right.expect("a reply");
    assert_eq!(left.status, 200, "{}", String::from_utf8_lossy(&left.body));
    assert_eq!(
        right.status,
        200,
        "{}",
        String::from_utf8_lossy(&right.body)
    );

    let markers = [
        body_json(&left)["message"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        body_json(&right)["message"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    ];
    assert_eq!(
        markers.iter().filter(|m| m.is_empty()).count(),
        1,
        "exactly one caller may win the admission election: {markers:?}"
    );
    assert_eq!(
        markers.iter().filter(|m| *m == "duplicate:").count(),
        1,
        "the loser must report a duplicate: {markers:?}"
    );

    // One stored copy, one saga, one audit entry. (The feed is at-least-once by
    // design — both drivers may append — so the invariant there is one DISTINCT
    // event id, which is exactly the dedup contract a consumer implements.)
    assert_eq!(stored_event_count(&env).await, 1);
    assert!(admission(&env, &event.id)
        .await
        .expect("saga")
        .is_complete());
    let chain = audit_entries(&env).await;
    assert_eq!(chain.len(), 1, "the race appended twice: {chain:?}");
    verify_chain(COMMUNITY, &chain, &GENESIS_PREV).expect("the chain verifies");
    let ids: BTreeSet<String> = feed_records(&env, FEED_GENERATION)
        .await
        .iter()
        .map(|r| to_hex(&r.event.id))
        .collect();
    assert_eq!(ids, BTreeSet::from([to_hex(&event.id)]));
    assert!(feed_copies(&env, FEED_GENERATION, &event.id).await >= 1);

    // The read path returns one event, whichever driver got there first.
    assert_eq!(
        returned_ids(&query(&env, &alice, &json!({})).await),
        vec![to_hex(&event.id)]
    );

    });
}

/// The same race, many times over. One pass of the test above hits the window
/// between a driver's marker read and its head read only occasionally (it failed
/// about one CI run in six on an unchanged tree, 19 September 2026: "the race
/// appended twice", two `EventCreated` entries for one event id). Forty events,
/// four simultaneous submissions each, against three competing instances, make
/// a regression of the ordering in `audit::append` fail reliably rather than
/// now and then.
#[test]
 fn severe_repeated_same_event_races_never_audit_an_event_twice() {
    common::run_scenario(|| async move {
    const EVENTS: usize = 40;
    let env = gate_with(3).await;
    for round in 0..EVENTS {
        // One author per round: the relay rate-limits an author, and the race
        // under test is between drivers of one event, not between authors.
        let alice = Actor::new(&format!("racer-{round}"));
        let event = signed(&alice, kind::KIND_TEXT_NOTE, vec![], &format!("raced {round}"));
        let replies = env.handle_all([
            submit_request(&alice, &event),
            submit_request(&alice, &event),
            submit_request(&alice, &event),
            submit_request(&alice, &event),
        ]).await;
        assert_eq!(replies.len(), 4, "all four concurrent submissions answer");
        for reply in replies {
            let reply = reply.expect("a reply");
            assert_eq!(reply.status, 200, "{}", String::from_utf8_lossy(&reply.body));
        }
    }

    let chain = audit_entries(&env).await;
    let mut per_event: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for entry in &chain {
        *per_event.entry(entry.object.clone().unwrap_or_default()).or_default() += 1;
    }
    let doubled: Vec<_> = per_event
        .iter()
        .filter(|(_, count)| **count != 1)
        .map(|(id, count)| {
            let seqs: Vec<u64> = chain
                .iter()
                .filter(|entry| entry.object.as_deref() == Some(id.as_str()))
                .map(|entry| entry.seq)
                .collect();
            (id.clone(), *count, seqs)
        })
        .collect();
    assert!(doubled.is_empty(), "events audited more than once: {doubled:?}");
    assert_eq!(chain.len(), EVENTS, "one audit entry per admitted event");
    verify_chain(COMMUNITY, &chain, &GENESIS_PREV).expect("the chain verifies");
    assert_eq!(stored_event_count(&env).await, EVENTS);

    });
}

/// Many distinct events, submitted concurrently against competing instances, must
/// leave a DENSE, verifiable audit chain: the head CAS is the only serialization
/// point, and losing it must retry rather than skip or collide on a sequence.
#[test]
 fn severe_concurrent_distinct_events_keep_the_audit_chain_dense() {
    common::run_scenario(|| async move {
    let env = gate_with(3).await;
    let authors: Vec<Actor> = (0..3).map(|i| Actor::new(&format!("author-{i}"))).collect();

    let mut events = Vec::new();
    let mut work = Vec::new();
    for i in 0..12u64 {
        let author = &authors[usize::try_from(i).unwrap_or(0) % authors.len()];
        let event = signed_at(
            author,
            kind::KIND_TEXT_NOTE,
            vec![],
            &format!("message {i}"),
            T0_SECS - i,
        );
        work.push((*author, event.clone()));
        events.push(event);
    }

    // Concurrent, with the frozen-clock re-send discipline: the race happens on
    // the first drive, and a re-send only recovers an answer the manual clock
    // would otherwise strand forever (see `common::submit_all`).
    let replies = submit_all(&env, &work).await;
    for (i, reply) in replies.iter().enumerate() {
        assert_eq!(
            reply.status,
            200,
            "event {i}: {}",
            String::from_utf8_lossy(&reply.body)
        );
    }

    // Dense sequences 1..=12, one per event, verifiable end to end.
    let chain = audit_entries(&env).await;
    assert_eq!(chain.len(), 12, "the chain is not dense: {:?}", chain.len());
    let seqs: Vec<u64> = chain.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=12).collect::<Vec<u64>>());
    verify_chain(COMMUNITY, &chain, &GENESIS_PREV).expect("the chain verifies");
    let objects: BTreeSet<Option<String>> = chain.iter().map(|e| e.object.clone()).collect();
    assert_eq!(objects.len(), 12, "two entries name the same event");
    for event in &events {
        assert!(audit_ref(&env, &event.id).await.is_some());
        assert!(stored_event(&env, &event.id).await.is_some());
        assert!(
            admission(&env, &event.id)
                .await
                .expect("saga")
                .is_complete(),
            "a saga did not complete"
        );
    }
    assert_eq!(stored_event_count(&env).await, 12);

    // Every event is on the feed, and every one is readable.
    let ids: BTreeSet<String> = feed_records(&env, FEED_GENERATION)
        .await
        .iter()
        .map(|r| to_hex(&r.event.id))
        .collect();
    assert_eq!(ids.len(), 12);
    let alice = &authors[0];
    let page = query(&env, alice, &json!({"limit": 100})).await;
    assert_eq!(returned_ids(&page).len(), 12);

    });
}

/// **A ban whose saga tore after the body write still takes effect.**
///
/// The effect step used to run only when `apply_storage` reported the body newly
/// written. A driver that died after the body landed left the re-drive with
/// `was_new = false`, so it skipped the effect and went on to audit, deliver and
/// complete: the ban was announced to every subscriber and recorded in the audit
/// chain, and the banned member kept writing. Effects now run on every drive of
/// an incomplete saga, each idempotent on its own.
#[test]
 fn severe_a_ban_torn_after_storage_is_applied_by_the_re_drive() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("moderator");
    let member = Actor::new("member");
    {
        let mut config = env.store("config");
        config
            .put(
                b"buzz-community-moderators",
                json!([moderator.pk_hex()]).to_string().as_bytes(),
            )
            .await
            .expect("seed moderators");
    }
    let ban = signed(
        &moderator,
        kind::KIND_MODERATION_BAN,
        vec![tag("p", &member.pk_hex())],
        "",
    );

    // The dead driver got as far as the body and its index rows.
    admit_only(&env, &ban, None).await;
    {
        let mut store = env.store("events");
        ingest::apply_storage(&mut store, COMMUNITY, &ban, None)
            .await
            .expect("storage");
    }
    let mut events = env.store("events");
    assert!(
        buzz::relay::effects::restriction_of(&mut events, COMMUNITY, &member.pk)
            .await
            .expect("restriction read")
            .is_none(),
        "the torn saga applied nothing"
    );

    // The moderator's client resubmits; the duplicate path re-drives the saga.
    let reply = submit(&env, &moderator, &ban).await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    assert!(admission(&env, &ban.id)
        .await
        .expect("saga")
        .is_complete());
    assert!(
        buzz::relay::effects::restriction_of(&mut events, COMMUNITY, &member.pk)
            .await
            .expect("restriction read")
            .is_some_and(|r| r.banned),
        "a completed, audited, delivered ban must be in force"
    );
    assert_eq!(audit_entries_for(&env, &ban.id).await.len(), 1);

    });
}
