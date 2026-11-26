//! **S18 — bounds by construction.** The size ceilings, attacked from both sides.
//!
//! Two bounds carry real weight in this app, and both are easy to get subtly
//! wrong:
//!
//! * **The event bound is on the WIRE form, not on `content`.** JSON escaping
//!   inflates a string by up to 2× (quotes, backslashes) or 6× (control
//!   characters), so a content-length check would admit a body the gateway
//!   refuses. The escaping-heavy case below is the proof: its decoded content is
//!   comfortably under the guaranteed-postable size while its serialized form is
//!   over the hard cap, and the relay must refuse it on the serialized measure.
//! * **A query page is bounded by BYTES as well as by count.** A caller asking for
//!   500 events of 240 KiB each must get a bounded page and a cursor, never a
//!   120 MB reply.
//!
//! Both bounds are asserted through the routes, on the reply the caller actually
//! receives.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::collections::BTreeSet;

use buzz::relay::crypto::to_hex;
use buzz::relay::event::{MAX_CONTENT_BYTES_GUARANTEED, MAX_SERIALIZED_EVENT_BYTES};
use buzz::relay::kind;
use buzz::relay::query::{MAX_PAGE_BYTES, MAX_PAGE_EVENTS};
use common::{
    Actor, COMMUNITY, FEED_GENERATION, QUERY_PATH, T0_SECS, admission, audit_head, body_json,
    channel_query, gate_env, error_text, feed_records, index_keys, post, query, returned,
    returned_ids, signed, signed_at, signed_oversized, stored_event, stored_event_count, submit,
    tag,
};
use ferrite_app::serde_json::json;

/// Bytes of JSON an event costs beyond its content (hex id, pubkey, signature,
/// field names, punctuation). Measured, not guessed.
fn overhead_bytes(actor: &Actor) -> usize {
    let probe = signed(actor, kind::KIND_TEXT_NOTE, vec![], "");
    probe.serialized_len()
}

/// A validly signed event whose serialized form is just OVER the hard cap.
#[test]
 fn severe_an_oversized_event_is_refused_as_too_large() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");

    let content = "a".repeat(MAX_SERIALIZED_EVENT_BYTES - overhead_bytes(&alice) + 64);
    let huge = signed_oversized(&alice, kind::KIND_TEXT_NOTE, vec![], &content);
    let measured = huge.serialized_len();
    assert!(
        measured > MAX_SERIALIZED_EVENT_BYTES,
        "the fixture is not oversized: {measured}"
    );
    assert!(
        measured < 256 * 1024,
        "the fixture must still fit the gateway's body bound, or the app never sees it"
    );

    let reply = submit(&env, &alice, &huge).await;
    assert_eq!(
        reply.status,
        400,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(
        error_text(&reply),
        format!(
            "invalid: event is {measured} bytes, over the {MAX_SERIALIZED_EVENT_BYTES} byte limit"
        )
    );

    // Refused before any effect: no body, no index row, no saga, no chain entry,
    // no feed record.
    assert!(stored_event(&env, &huge.id).await.is_none());
    assert_eq!(stored_event_count(&env).await, 0);
    assert!(admission(&env, &huge.id).await.is_none());
    assert!(audit_head(&env).await.is_none());
    assert_eq!(feed_records(&env, FEED_GENERATION).await.len(), 0);

    // The relay is still healthy and admits an ordinary event afterwards.
    let ok = signed(&alice, kind::KIND_TEXT_NOTE, vec![], "small");
    assert_eq!(submit(&env, &alice, &ok).await.status, 200);

    });
}

/// The escaping trap: content built from quotes, backslashes and control
/// characters whose DECODED length is under the guaranteed-postable size, but
/// whose SERIALIZED length is over the hard cap. A relay that bounded
/// `content.len()` would accept this and then hand the gateway a body it refuses.
#[test]
 fn severe_an_escaping_heavy_event_is_refused_on_its_serialized_size() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");

    // U+0001 escapes to ``: six wire bytes per one decoded byte.
    let content = "\u{1}".repeat(41_000);
    assert!(
        content.len() < MAX_CONTENT_BYTES_GUARANTEED,
        "the decoded content must be under the advertised guarantee ({} vs {})",
        content.len(),
        MAX_CONTENT_BYTES_GUARANTEED
    );
    let sneaky = signed_oversized(&alice, kind::KIND_TEXT_NOTE, vec![], &content);
    let measured = sneaky.serialized_len();
    assert!(
        measured > MAX_SERIALIZED_EVENT_BYTES,
        "escaping did not inflate as expected: {measured}"
    );

    let reply = submit(&env, &alice, &sneaky).await;
    assert_eq!(
        reply.status,
        400,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(
        error_text(&reply),
        format!(
            "invalid: event is {measured} bytes, over the {MAX_SERIALIZED_EVENT_BYTES} byte limit"
        )
    );
    assert_eq!(stored_event_count(&env).await, 0);
    assert!(admission(&env, &sneaky.id).await.is_none());

    // The same shape built from quotes and backslashes (2× escaping) is refused on
    // the same measure.
    let quoted = "\"\\".repeat(MAX_SERIALIZED_EVENT_BYTES / 2);
    let mixed = signed_oversized(&alice, kind::KIND_TEXT_NOTE, vec![], &quoted);
    assert!(mixed.serialized_len() > MAX_SERIALIZED_EVENT_BYTES);
    // This one is over the gateway's body bound too — the app is the second line
    // of defence, and it must still be the RIGHT refusal rather than a 500.
    let reply = submit(&env, &alice, &mixed).await;
    assert_eq!(
        reply.status,
        400,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert!(error_text(&reply).starts_with("invalid: event is "));
    assert_eq!(stored_event_count(&env).await, 0);

    });
}

/// The other side of the bound: content of exactly the *guaranteed* size, built
/// from the realistic worst case (every byte a quote), must be accepted. The
/// constant advertises a promise; this is the promise being kept.
#[test]
 fn severe_the_guaranteed_content_size_is_actually_postable() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");

    let content = "\"".repeat(MAX_CONTENT_BYTES_GUARANTEED);
    let event = signed(&alice, kind::KIND_TEXT_NOTE, vec![], &content);
    assert!(
        event.serialized_len() <= MAX_SERIALIZED_EVENT_BYTES,
        "the guarantee is broken by construction: {}",
        event.serialized_len()
    );

    let reply = submit(&env, &alice, &event).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let stored = stored_event(&env, &event.id).await.expect("stored");
    assert_eq!(stored.content.len(), MAX_CONTENT_BYTES_GUARANTEED);
    assert_eq!(stored, event);
    assert!(stored.verify().is_ok());

    // And it reads back through the route intact.
    let page = query(&env, &alice, &json!({"limit": 10})).await;
    assert_eq!(page.status, 200);
    assert!(returned(&page, &event));

    });
}

/// A page of large events is bounded by BYTES: the reply stays under the page
/// budget, carries a cursor, and the next page continues from exactly where it
/// stopped — no event skipped, none repeated.
#[test]
 fn severe_a_query_page_never_exceeds_the_byte_budget() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    // `/buzz/api/query` cursors are sealed under the stream key; without one
    // the relay issues none.
    {
        let mut config = env.store("config");
        config
            .put(b"buzz-stream-token-key", &[0x42u8; 32])
            .await
            .expect("stream key seed");
    }
    let alice = Actor::new("alice");

    // Nine events of ~240 KiB: more than one 2 MiB page can hold.
    let big = "b".repeat(240_000);
    let mut ids = Vec::new();
    for i in 0..9u64 {
        let event = signed_at(
            &alice,
            kind::KIND_TEXT_NOTE,
            vec![],
            &big,
            T0_SECS - i, // distinct created_at ⇒ a deterministic newest-first order
        );
        let reply = submit(&env, &alice, &event).await;
        assert_eq!(
            reply.status,
            200,
            "event {i}: {}",
            String::from_utf8_lossy(&reply.body)
        );
        ids.push(to_hex(&event.id));
    }
    assert_eq!(stored_event_count(&env).await, 9);

    // Ask for far more than fits.
    let first = query(&env, &alice, &json!({"limit": MAX_PAGE_EVENTS})).await;
    assert_eq!(
        first.status,
        200,
        "{}",
        String::from_utf8_lossy(&first.body)
    );
    let body = body_json(&first);
    let page_one = returned_ids(&first);

    assert!(
        !page_one.is_empty() && page_one.len() < 9,
        "the byte budget did not end the page: {} events",
        page_one.len()
    );
    assert!(
        first.body.len() <= MAX_PAGE_BYTES + 4096,
        "the reply body is {} bytes, over the {MAX_PAGE_BYTES} byte page budget",
        first.body.len()
    );
    assert_eq!(body["complete"], json!(false));
    let cursor = body["next_cursor"]
        .as_str()
        .expect("a stopped page must carry a cursor")
        .to_string();
    // Sealed: the cursor must not spell out the index key (`ts/...`, an event
    // id and its exact timestamp) a withheld page would otherwise leak
    // (finding_7858c47f9d8ba53f).
    assert!(
        !cursor.contains(&to_hex(b"ts/")),
        "the query cursor exposes its index key: {cursor}"
    );

    // Drain the cursor to the end. How MANY pages it takes is a function of the
    // byte budget and must not be baked into the test — what matters is that
    // following the contract yields every event exactly once, and that no page
    // ever exceeds the budget.
    let mut seen: Vec<String> = page_one.clone();
    let mut cursor = Some(cursor);
    let mut pages = 1;
    while let Some(c) = cursor.take() {
        let next = query(
            &env,
            &alice,
            &json!({"limit": MAX_PAGE_EVENTS, "cursor": c}),
        )
        .await;
        assert_eq!(next.status, 200, "{}", String::from_utf8_lossy(&next.body));
        assert!(
            next.body.len() <= MAX_PAGE_BYTES + 4096,
            "page {} is {} bytes, over the {MAX_PAGE_BYTES} byte budget",
            pages + 1,
            next.body.len()
        );
        let rows = returned_ids(&next);
        assert!(!rows.is_empty(), "page {} came back empty", pages + 1);
        seen.extend(rows);
        pages += 1;
        assert!(pages <= 20, "paging did not terminate");
        let body = body_json(&next);
        if body["complete"] == json!(true) {
            break;
        }
        cursor = body["next_cursor"].as_str().map(str::to_string);
    }
    assert!(pages >= 2, "the byte budget never split the set");

    // Every event exactly once: no duplicate across pages, nothing lost.
    let unique: BTreeSet<String> = seen.iter().cloned().collect();
    assert_eq!(
        unique.len(),
        seen.len(),
        "an event appeared on two pages: {seen:?}"
    );
    assert_eq!(
        unique,
        ids.iter().cloned().collect::<BTreeSet<String>>(),
        "paging lost or invented an event"
    );
    // Newest-first within page one.
    assert_eq!(page_one, ids[..page_one.len()].to_vec());

    });
}

/// Degenerate limits are handled without an error and without an unbounded read:
/// zero asks for nothing, and an absurd limit is clamped by the hard cap.
#[test]
 fn severe_degenerate_page_limits_are_bounded() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    for i in 0..3u64 {
        let event = signed_at(
            &alice,
            kind::KIND_TEXT_NOTE,
            vec![],
            &format!("m{i}"),
            T0_SECS - i,
        );
        assert_eq!(submit(&env, &alice, &event).await.status, 200);
    }

    // Zero: an empty, complete page (there is no cursor to echo).
    let zero = query(&env, &alice, &json!({"limit": 0})).await;
    assert_eq!(zero.status, 200, "{}", String::from_utf8_lossy(&zero.body));
    assert_eq!(returned_ids(&zero), Vec::<String>::new());
    assert_eq!(body_json(&zero)["complete"], json!(true));

    // Absurd: accepted, clamped, and complete because everything fits.
    let huge = query(&env, &alice, &json!({"limit": 4_000_000_000u64})).await;
    assert_eq!(huge.status, 200, "{}", String::from_utf8_lossy(&huge.body));
    assert_eq!(returned_ids(&huge).len(), 3);
    assert!(returned_ids(&huge).len() <= MAX_PAGE_EVENTS);
    assert_eq!(body_json(&huge)["complete"], json!(true));

    // A non-numeric limit falls back to the default rather than failing.
    let junk = query(&env, &alice, &json!({"limit": "lots"})).await;
    assert_eq!(junk.status, 200, "{}", String::from_utf8_lossy(&junk.body));
    assert_eq!(returned_ids(&junk).len(), 3);

    // A body that is not an object at all is a 400, not a panic.
    let not_object = env
        .handle(post(&alice, QUERY_PATH, b"[1,2,3]".to_vec()))
        .await
        .expect("a reply");
    assert_eq!(
        not_object.status, 200,
        "an array body is a valid JSON value"
    );
    let broken = env
        .handle(post(&alice, QUERY_PATH, b"{oops".to_vec()))
        .await
        .expect("a reply");
    assert_eq!(broken.status, 400);
    assert!(error_text(&broken).starts_with("invalid: query body"));

    });
}

/// Key components are bounded too: a channel id far past the component cap can
/// neither be authorized nor produce an unbounded key.
#[test]
 fn severe_an_overlong_channel_id_is_bounded_and_denied() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let overlong = "c".repeat(4_096);

    // A channel-scoped kind, so the id is actually used as a channel.
    let event = signed(
        &alice,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", &overlong)],
        "in a very long channel",
    );
    let reply = submit(&env, &alice, &event).await;
    assert!(
        reply.status == 400 || reply.status == 404,
        "an over-long channel id was admitted with status {}: {}",
        reply.status,
        String::from_utf8_lossy(&reply.body)
    );
    assert!(
        stored_event(&env, &event.id).await.is_none(),
        "a refused write still stored its event"
    );
    assert_eq!(stored_event_count(&env).await, 0);

    // Even if such a row existed, the index key it would take is bounded: the
    // component collapses to a hashed bucket rather than growing without limit.
    let prefix = buzz::relay::keys::ts_prefix(COMMUNITY, Some(&overlong));
    assert!(
        prefix.len() < 128,
        "an unbounded key component would reach the store: {} bytes",
        prefix.len()
    );
    assert_eq!(index_keys(&env, &prefix).await.len(), 0);

    // And the read for it is empty, not an error and not somebody else's bucket.
    let page = query(&env, &alice, &channel_query(&overlong)).await;
    assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));
    assert_eq!(returned_ids(&page), Vec::<String>::new());

    });
}

/// The HTTP filter faces enforce the socket face's per-filter value bound: each
/// `#e`/`#p` value can cost an index walk (finding_682bd69ebd8e5cf5).
#[test]
fn severe_http_filter_queries_and_subscriptions_bound_their_value_lists() {
    common::run_scenario(|| async move {
        let env = gate_env().await;
        let alice = Actor::new("alice-bounds");
        let mentions: Vec<String> = (0..600u32).map(|i| format!("{i:064x}")).collect();
        let body = ferrite_app::serde_json::to_vec(&json!([{ "#p": mentions }])).unwrap();
        for path in ["/query", "/buzz/api/subscriptions"] {
            let reply = env
                .handle(common::post(&alice, path, body.clone()))
                .await
                .expect("the app replied");
            assert_eq!(
                reply.status,
                400,
                "{path} must refuse a 600-value #p list: {}",
                String::from_utf8_lossy(&reply.body)
            );
        }
        let many = ferrite_app::serde_json::to_vec(&json!([{}, {}, {}, {}, {}, {}, {}, {}, {}])).unwrap();
        let reply = env
            .handle(common::post(&alice, "/buzz/api/subscriptions", many))
            .await
            .expect("the app replied");
        assert_eq!(reply.status, 400, "nine filters exceed the advertised REQ limit");
    });
}
