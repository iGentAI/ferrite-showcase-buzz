//! **S13 — protocol fidelity.** The signed-event contract, exercised through the
//! real routes on a real fleet.
//!
//! Nostr's guarantees are cryptographic, so a relay's fidelity is decided at the
//! ingest boundary: an event's id must be the hash of its own fields, its
//! signature must verify under its own pubkey, its author must be the caller who
//! submitted it, its `created_at` must be inside the drift window, and a resubmit
//! must be absorbed rather than duplicated. Every one of those is asserted here
//! against observable state — the route's reply, the stored event body, the
//! ordered index keys, the APE saga document, and the audit chain — never against
//! a log line.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::audit::{verify_chain, AuditAction, GENESIS_PREV};
use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::kind;
use buzz::relay::{keys, query};
use common::{
    Actor, COMMUNITY, FEED_GENERATION, T0_SECS, admission, audit_entries, audit_head, audit_ref,
    body_json, changefeed_records, channel_query, gate_env, error_text, feed_copies, feed_records,
    index_keys, query, returned, returned_ids, seed_channel, seed_channel_with_owner, signed,
    signed_at, stored_event, stored_event_count, submit, tag,
};
use ferrite_app::serde_json::{json, Value};

/// A signed kind-9 message is accepted, stored byte-for-byte, indexed, audited,
/// published to the feed, and read back through `POST /buzz/api/query`.
#[test]
 fn severe_a_signed_message_is_accepted_and_readable_through_query() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    // Kind 9 is channel-scoped by rule, so a message needs a channel it may write
    // to: the document plus the author's membership, exactly what a real creation
    // leaves behind.
    let channel = "chan-lobby";
    seed_channel_with_owner(
        &env,
        channel,
        buzz::relay::channel::Visibility::Open,
        &alice,
    )
    .await;
    let event = signed(
        &alice,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "hello workspace",
    );

    let reply = submit(&env, &alice, &event).await;
    assert_eq!(
        reply.status,
        200,
        "submission refused: {}",
        String::from_utf8_lossy(&reply.body)
    );
    let body = body_json(&reply);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["accepted"], json!(true));
    assert_eq!(body["event_id"], json!(to_hex(&event.id)));
    assert_eq!(
        body["message"],
        json!(""),
        "a first submission is not a duplicate"
    );

    // The stored body is the SAME event, signature included: a relay that
    // re-serialized or normalized anything would break every client's
    // verification.
    let stored = stored_event(&env, &event.id)
        .await
        .expect("event was stored");
    assert_eq!(stored, event, "the stored event is not byte-identical");
    assert!(stored.verify().is_ok(), "the stored event still verifies");

    // Its read paths exist: the global timeline bucket, the kind index, the
    // author index.
    assert_eq!(
        index_keys(&env, &keys::ts_prefix(COMMUNITY, Some(channel)))
            .await
            .len(),
        1
    );
    assert_eq!(
        index_keys(
            &env,
            &keys::kind_prefix(COMMUNITY, kind::KIND_STREAM_MESSAGE)
        )
        .await
        .len(),
        1
    );
    assert_eq!(
        index_keys(&env, &keys::author_prefix(COMMUNITY, &alice.pk, None))
            .await
            .len(),
        1
    );

    // The saga finished, and the audit chain holds exactly one verifiable entry
    // naming this event.
    let saga = admission(&env, &event.id).await.expect("a saga document");
    assert!(saga.is_complete(), "the saga did not complete: {saga:?}");
    assert_eq!(saga.audit_seq, Some(1));
    let chain = audit_entries(&env).await;
    assert_eq!(chain.len(), 1, "one entry for one event");
    assert_eq!(chain[0].action, AuditAction::EventCreated);
    assert_eq!(chain[0].object.as_deref(), Some(to_hex(&event.id).as_str()));
    assert_eq!(chain[0].actor.as_deref(), Some(alice.pk_hex().as_str()));
    verify_chain(COMMUNITY, &chain, &GENESIS_PREV).expect("the chain verifies");

    // It reached the live feed and the durable changefeed exactly once.
    assert_eq!(feed_copies(&env, FEED_GENERATION, &event.id).await, 1);
    assert_eq!(changefeed_records(&env).await.len(), 1);

    // And it reads back through the route.
    let page = query(&env, &alice, &channel_query(channel)).await;
    assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));
    let page_body = body_json(&page);
    assert_eq!(page_body["complete"], json!(true));
    assert_eq!(returned_ids(&page), vec![to_hex(&event.id)]);
    let returned: Event =
        ferrite_app::serde_json::from_value(page_body["events"][0].clone()).expect("an event");
    assert_eq!(returned, event, "the read-back event is not the one signed");

    });
}

/// An event whose id is not the hash of its fields is refused — and the refusal
/// leaves NO trace: no saga, no body, no index row.
#[test]
 fn severe_an_event_whose_id_does_not_match_its_fields_is_refused() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let honest = signed(&alice, kind::KIND_STREAM_MESSAGE, vec![], "pay bob 10");

    // The classic swap: keep the signed id and signature, change the body.
    let mut tampered = honest.clone();
    tampered.content = "pay eve 10000".into();

    let reply = submit(&env, &alice, &tampered).await;
    assert_eq!(
        reply.status,
        400,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert!(
        error_text(&reply).starts_with("invalid: event id does not match"),
        "unexpected refusal: {}",
        error_text(&reply)
    );

    // Nothing under the claimed id, nothing anywhere.
    assert!(stored_event(&env, &tampered.id).await.is_none());
    assert!(admission(&env, &tampered.id).await.is_none());
    assert_eq!(stored_event_count(&env).await, 0);
    assert!(
        audit_head(&env).await.is_none(),
        "no chain entry was appended"
    );
    assert_eq!(feed_records(&env, FEED_GENERATION).await.len(), 0);

    // A tampered tag list is the same refusal (the id covers tags too).
    let mut retagged = honest.clone();
    retagged.tags = vec![tag("h", "elsewhere")];
    let reply = submit(&env, &alice, &retagged).await;
    assert_eq!(reply.status, 400);
    assert!(error_text(&reply).starts_with("invalid: event id does not match"));
    assert_eq!(stored_event_count(&env).await, 0);

    });
}

/// An event validly signed by key A, submitted under key B's authorization, is
/// refused: the authorization proves who is CALLING, and ingest requires the
/// caller to be the author.
#[test]
 fn severe_an_event_signed_by_another_key_is_refused_under_a_foreign_authorization() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let bob = Actor::new("bob");
    let channel = "chan-lobby";
    seed_channel_with_owner(
        &env,
        channel,
        buzz::relay::channel::Visibility::Open,
        &alice,
    )
    .await;

    // Alice's event, perfectly valid on its own.
    let alices = signed(
        &alice,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "alice speaking",
    );
    assert!(alices.verify().is_ok());

    // Bob relays it under his own NIP-98 header.
    let reply = submit(&env, &bob, &alices).await;
    assert_eq!(
        reply.status,
        403,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert!(
        error_text(&reply).contains("does not match the authenticated caller"),
        "unexpected refusal: {}",
        error_text(&reply)
    );
    assert!(stored_event(&env, &alices.id).await.is_none());
    assert!(admission(&env, &alices.id).await.is_none());
    assert_eq!(stored_event_count(&env).await, 0);

    // The same bytes from Alice herself are accepted, which proves the refusal
    // was about the CALLER and not about the event.
    let reply = submit(&env, &alice, &alices).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert!(stored_event(&env, &alices.id).await.is_some());

    });
}

/// A flipped signature byte is refused even though the id still hashes correctly:
/// the id check and the signature check are independent gates.
#[test]
 fn severe_a_flipped_signature_byte_is_refused() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let mut forged = signed(&alice, kind::KIND_STREAM_MESSAGE, vec![], "not mine");
    assert_eq!(
        forged.compute_id().unwrap(),
        forged.id,
        "the id still matches"
    );
    forged.sig[31] ^= 0x01;

    let reply = submit(&env, &alice, &forged).await;
    assert_eq!(
        reply.status,
        400,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert!(
        error_text(&reply).starts_with("invalid: signature verification failed"),
        "unexpected refusal: {}",
        error_text(&reply)
    );
    assert_eq!(stored_event_count(&env).await, 0);
    assert!(admission(&env, &forged.id).await.is_none());

    });
}

/// The drift window is exactly ±900 seconds: 901 seconds in the past is refused,
/// 899 is accepted, and the boundary second itself is accepted.
#[test]
 fn severe_drift_refuses_901_seconds_and_admits_899() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let channel = "chan-lobby";
    seed_channel_with_owner(
        &env,
        channel,
        buzz::relay::channel::Visibility::Open,
        &alice,
    )
    .await;

    let stale = signed_at(
        &alice,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "901 seconds old",
        T0_SECS - 901,
    );
    let reply = submit(&env, &alice, &stale).await;
    assert_eq!(
        reply.status,
        400,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(
        error_text(&reply),
        "invalid: created_at is 901s from relay time"
    );
    assert!(stored_event(&env, &stale.id).await.is_none());
    assert!(admission(&env, &stale.id).await.is_none());

    // The same shape 899 seconds old is admitted.
    let fresh = signed_at(
        &alice,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "899 seconds old",
        T0_SECS - 899,
    );
    let reply = submit(&env, &alice, &fresh).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert!(stored_event(&env, &fresh.id).await.is_some());

    // The boundary is inclusive at 900 and exclusive at 901, in both directions.
    let edge_past = signed_at(
        &alice,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "exactly 900 old",
        T0_SECS - 900,
    );
    assert_eq!(submit(&env, &alice, &edge_past).await.status, 200);
    let edge_future = signed_at(
        &alice,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "exactly 900 ahead",
        T0_SECS + 900,
    );
    assert_eq!(submit(&env, &alice, &edge_future).await.status, 200);
    let too_far_future = signed_at(
        &alice,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "901 ahead",
        T0_SECS + 901,
    );
    let reply = submit(&env, &alice, &too_far_future).await;
    assert_eq!(reply.status, 400);
    assert_eq!(
        error_text(&reply),
        "invalid: created_at is 901s from relay time"
    );

    // Three admitted events, three chain entries, one refused pair leaving none.
    assert_eq!(stored_event_count(&env).await, 3);
    let chain = audit_entries(&env).await;
    assert_eq!(chain.len(), 3);
    verify_chain(COMMUNITY, &chain, &GENESIS_PREV).expect("the chain verifies");

    });
}

/// A resubmit answers with the duplicate marker and creates NO second copy: one
/// stored body, one audit entry, one saga document, one feed record.
#[test]
 fn severe_a_resubmit_is_a_duplicate_with_one_stored_copy_and_one_audit_entry() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let channel = "chan-lobby";
    seed_channel_with_owner(
        &env,
        channel,
        buzz::relay::channel::Visibility::Open,
        &alice,
    )
    .await;
    let event = signed(
        &alice,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "say it once",
    );

    let first = submit(&env, &alice, &event).await;
    assert_eq!(
        first.status,
        200,
        "{}",
        String::from_utf8_lossy(&first.body)
    );
    assert_eq!(body_json(&first)["message"], json!(""));
    let head_after_first = audit_head(&env).await.expect("a head");

    // Resubmitted three times, under fresh authorizations each time (the header
    // is single-use, so this is genuinely three requests).
    for _ in 0..3 {
        let again = submit(&env, &alice, &event).await;
        assert_eq!(
            again.status,
            200,
            "{}",
            String::from_utf8_lossy(&again.body)
        );
        let body = body_json(&again);
        assert_eq!(body["ok"], json!(true));
        assert_eq!(body["accepted"], json!(true));
        assert_eq!(
            body["message"],
            json!("duplicate:"),
            "a resubmit must be reported as a duplicate"
        );
        assert_eq!(body["event_id"], json!(to_hex(&event.id)));
    }

    // Exactly one of everything.
    assert_eq!(stored_event_count(&env).await, 1);
    assert_eq!(
        index_keys(&env, &keys::ts_prefix(COMMUNITY, Some(channel)))
            .await
            .len(),
        1,
        "the timeline index must not gain a row per resubmit"
    );
    let chain = audit_entries(&env).await;
    assert_eq!(chain.len(), 1, "the chain grew on a duplicate: {chain:?}");
    assert_eq!(audit_head(&env).await.as_ref(), Some(&head_after_first));
    assert_eq!(audit_ref(&env, &event.id).await, Some(1));
    verify_chain(COMMUNITY, &chain, &GENESIS_PREV).expect("the chain verifies");
    assert_eq!(feed_copies(&env, FEED_GENERATION, &event.id).await, 1);
    assert_eq!(
        changefeed_records(&env).await.len(),
        1,
        "a completed saga re-published to the changefeed"
    );

    // And the read path still returns one event, not four.
    let page = query(&env, &alice, &channel_query(channel)).await;
    assert_eq!(returned_ids(&page), vec![to_hex(&event.id)]);

    });
}

/// An `h`-tagged event lands in its channel's timeline bucket and NOT in the
/// community-global one: an index is a routing structure, and a channel query
/// must not be answerable from the global bucket.
#[test]
 fn severe_a_channel_event_is_indexed_only_under_its_channel() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let channel = "chan-open-1";
    seed_channel(
        &env,
        channel,
        buzz::relay::channel::Visibility::Open,
        &alice,
    )
    .await;

    let event = signed(
        &alice,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "in a channel",
    );
    assert_eq!(submit(&env, &alice, &event).await.status, 200);

    assert_eq!(
        index_keys(&env, &keys::ts_prefix(COMMUNITY, Some(channel)))
            .await
            .len(),
        1
    );
    assert_eq!(
        index_keys(&env, &keys::ts_prefix(COMMUNITY, None))
            .await
            .len(),
        0,
        "a channel event leaked into the global timeline bucket"
    );

    // The global timeline query therefore returns nothing…
    let global = query(&env, &alice, &json!({})).await;
    assert_eq!(returned_ids(&global), Vec::<String>::new());
    // …while the channel query returns it (the channel is open).
    let scoped = query(&env, &alice, &channel_query(channel)).await;
    assert!(returned(&scoped, &event));

    });
}

/// A cursor minted for one channel's timeline cannot be replayed against
/// another's: the pager binds a cursor to the prefix it was issued for.
#[test]
 fn severe_a_cursor_from_another_channel_is_refused() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let a = "chan-a";
    let b = "chan-b";
    seed_channel(&env, a, buzz::relay::channel::Visibility::Open, &alice).await;
    seed_channel(&env, b, buzz::relay::channel::Visibility::Open, &alice).await;

    for i in 0..3u32 {
        let e = signed_at(
            &alice,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", a)],
            &format!("a{i}"),
            T0_SECS - u64::from(i),
        );
        assert_eq!(submit(&env, &alice, &e).await.status, 200);
    }

    // Page channel A with limit 1 to obtain a real cursor.
    let page = query(&env, &alice, &json!({"channel": a, "limit": 1})).await;
    let cursor = body_json(&page)["next_cursor"]
        .as_str()
        .expect("a cursor")
        .to_string();
    assert_eq!(body_json(&page)["complete"], json!(false));

    // The same cursor against channel B is refused, not silently re-based.
    let crossed = query(
        &env,
        &alice,
        &json!({"channel": b, "limit": 10, "cursor": cursor}),
    )
    .await;
    assert_eq!(
        crossed.status,
        400,
        "{}",
        String::from_utf8_lossy(&crossed.body)
    );
    assert!(
        error_text(&crossed).contains("does not belong to this query"),
        "unexpected refusal: {}",
        error_text(&crossed)
    );

    // Garbage cursors are refused the same way, and never panic the loop.
    for bad in [
        Value::String("zzzz".into()),
        Value::String("a".repeat(query::MAX_CURSOR_HEX + 2)),
        Value::String(to_hex(b"ts/main/chan-a/short")),
    ] {
        let reply = query(&env, &alice, &json!({"channel": a, "cursor": bad})).await;
        assert_eq!(
            reply.status,
            400,
            "accepted cursor {bad:?}: {}",
            String::from_utf8_lossy(&reply.body)
        );
    }

    // The app is still alive and answering after all of that.
    let page = query(&env, &alice, &json!({"channel": a, "limit": 10})).await;
    assert_eq!(page.status, 200);
    assert_eq!(returned_ids(&page).len(), 3);

    });
}
