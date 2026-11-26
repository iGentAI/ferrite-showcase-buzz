//! **NIP-09 deletion, and the resurrection paths that make it hard.**
//!
//! A deletion request is trivial to *record* and hard to *keep*: the relay is a
//! competing-consumer app whose sagas are re-driven by redelivery, so the same
//! `apply_storage` that first wrote an event's rows will happily write them
//! again after a deletion removed them. This gate pins the whole rule:
//!
//! * an `e`-tag deletion of the author's OWN event stops it being served, and no
//!   re-drive, repair, or re-publish of that id brings it back
//!   (`ingest::tombstone`);
//! * an `a`-tag (addressable) deletion retires a whole `kind:pubkey:d` cell up to
//!   the request's `created_at`, INCLUDING versions the relay is handed
//!   afterwards (`ReplaceableHead::deleted_through`), while a genuinely newer
//!   version still publishes;
//! * a deletion of somebody else's event changes nothing at all;
//! * the deletion request itself stays queryable, which is what lets the fact of
//!   the deletion propagate.
//!
//! Every assertion reads the durable state the app left behind and the query
//! face's answer, never a policy function.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::crypto::to_hex;
use buzz::relay::effects::ReplaceableHead;
use buzz::relay::{ingest, keys, kind};
use common::{
    Actor, COMMUNITY, FEED_GENERATION, T0_MS, T0_SECS, admission, body_json, gate_env, index_keys,
    query, signed_at, stored_event, submit, tag,
};
use ferrite_app::serde_json::{self, json};
use common::SsoTestEnv as TestEnv;

/// The stored watermark for one replaceable cell, if any.
async fn head_of(env: &TestEnv, kind_: u32, author: &Actor, d: &str) -> Option<ReplaceableHead> {
    let mut guards = env.ape("guards");
    let key = keys::dtag_head(COMMUNITY, kind_, &author.pk, d);
    let raw = guards.get(&key).await.expect("head read")?;
    Some(serde_json::from_slice(&raw).expect("head decodes"))
}

/// Every id the author index still resolves to a STORED body for — the reader
/// rule (an index row whose body is gone is invisible).
async fn queryable_ids(env: &TestEnv, author: &Actor, kind_: u32) -> Vec<String> {
    let mut out = Vec::new();
    for key in index_keys(
        env,
        &keys::author_prefix(COMMUNITY, &author.pk, Some(kind_)),
    )
    .await
    {
        let Some((_, id)) = keys::parse_index_tail(&key) else {
            continue;
        };
        if stored_event(env, &id).await.is_some() {
            out.push(to_hex(&id));
        }
    }
    out
}

/// What the QUERY face hands back for one author+kind.
///
/// `POST /buzz/api/query` is a channel TIMELINE read (no `filters` member — that
/// is the WebSocket `REQ` face), so the caller-side narrowing to one author+kind
/// happens here, over the events the route actually served.
async fn served_ids(env: &TestEnv, author: &Actor, kind_: u32) -> Vec<String> {
    let reply = query(env, author, &json!({ "limit": 200 })).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let body = body_json(&reply);
    let author_hex = author.pk_hex();
    body["events"]
        .as_array()
        .expect("an events array")
        .iter()
        .filter(|e| e["kind"] == json!(kind_) && e["pubkey"].as_str() == Some(author_hex.as_str()))
        .filter_map(|e| e["id"].as_str().map(ToOwned::to_owned))
        .collect()
}

/// The live-verified bug: a deleted replaceable kind-10100 agent profile stayed
/// served. It must stop being served — and STAY gone through every path that
/// re-runs the event's saga.
#[test]
 fn severe_a_deleted_agent_profile_stops_being_served_and_never_comes_back() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let profile = signed_at(
        &alice,
        kind::KIND_AGENT_PROFILE,
        vec![],
        r#"{"name":"nova"}"#,
        T0_SECS - 10,
    );
    assert_eq!(submit(&env, &alice, &profile).await.status, 200);
    assert_eq!(
        served_ids(&env, &alice, kind::KIND_AGENT_PROFILE).await,
        vec![to_hex(&profile.id)]
    );

    // The deletion request, by `e` tag, from the profile's own author.
    let del = signed_at(
        &alice,
        kind::KIND_DELETION,
        vec![tag("e", &to_hex(&profile.id))],
        "",
        T0_SECS,
    );
    assert_eq!(submit(&env, &alice, &del).await.status, 200);

    assert!(
        served_ids(&env, &alice, kind::KIND_AGENT_PROFILE)
            .await
            .is_empty(),
        "the deleted profile is still served by the query face"
    );
    assert!(
        stored_event(&env, &profile.id).await.is_none(),
        "the deleted profile's body is still stored"
    );
    assert!(
        queryable_ids(&env, &alice, kind::KIND_AGENT_PROFILE)
            .await
            .is_empty(),
        "an index row still resolves to the deleted profile"
    );
    let doc = admission(&env, &profile.id)
        .await
        .expect("an admission document");
    assert!(doc.is_deleted(), "the deleted id was not tombstoned");

    // The deletion REQUEST stays queryable — that is how the deletion travels.
    assert_eq!(
        served_ids(&env, &alice, kind::KIND_DELETION).await,
        vec![to_hex(&del.id)],
        "the kind-5 request must remain readable"
    );

    // Resurrection path 1: the client re-publishes the very same event.
    assert_eq!(submit(&env, &alice, &profile).await.status, 200);
    assert!(
        served_ids(&env, &alice, kind::KIND_AGENT_PROFILE)
            .await
            .is_empty(),
        "a re-publish of a deleted event resurrected it"
    );
    assert!(stored_event(&env, &profile.id).await.is_none());

    // Resurrection path 2: the queue re-drives the profile's saga end to end
    // (redelivery, or the repair sweep). `apply_storage` would re-create every
    // row it finds absent; the tombstone is what stops it.
    let mut ape = env.ape("guards");
    let mut events = env.store("events");
    let mut topics = env.topics("feed");
    let outcome = ingest::admit(&mut ape, COMMUNITY, &profile, None, T0_MS)
        .await
        .expect("re-admit finds the tombstone");
    assert!(outcome.was_duplicate());
    ingest::drive(
        &mut events,
        &mut ape,
        &mut topics,
        COMMUNITY,
        FEED_GENERATION,
        &profile,
        &outcome,
        T0_MS,
        None,
    )
    .await
    .expect("driving a tombstoned saga erases instead of failing");
    assert!(
        stored_event(&env, &profile.id).await.is_none(),
        "a re-driven saga resurrected a deleted event"
    );
    assert!(queryable_ids(&env, &alice, kind::KIND_AGENT_PROFILE)
        .await
        .is_empty());

    });
}

/// A deletion may only retire its OWN author's events. Bob naming Alice's event
/// changes nothing — not the rows, not the tombstone.
#[test]
 fn severe_a_deletion_cannot_retire_another_authors_event() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice-owner");
    let bob = Actor::new("bob-attacker");
    let profile = signed_at(
        &alice,
        kind::KIND_AGENT_PROFILE,
        vec![],
        r#"{"name":"alice"}"#,
        T0_SECS - 10,
    );
    assert_eq!(submit(&env, &alice, &profile).await.status, 200);

    // By `e` tag, and by the addressable coordinate — both must be ignored.
    let by_e = signed_at(
        &bob,
        kind::KIND_DELETION,
        vec![tag("e", &to_hex(&profile.id))],
        "",
        T0_SECS,
    );
    let by_a = signed_at(
        &bob,
        kind::KIND_DELETION,
        vec![tag(
            "a",
            &format!("{}:{}:", kind::KIND_AGENT_PROFILE, alice.pk_hex()),
        )],
        "",
        T0_SECS,
    );
    assert_eq!(submit(&env, &bob, &by_e).await.status, 200);
    assert_eq!(submit(&env, &bob, &by_a).await.status, 200);

    assert_eq!(
        served_ids(&env, &alice, kind::KIND_AGENT_PROFILE).await,
        vec![to_hex(&profile.id)],
        "a foreign deletion removed an event it does not own"
    );
    assert!(stored_event(&env, &profile.id).await.is_some());
    assert!(
        !admission(&env, &profile.id)
            .await
            .expect("a document")
            .is_deleted(),
        "a foreign deletion tombstoned an event it does not own"
    );
    // Alice's cell carries no watermark from Bob's request.
    let head = head_of(&env, kind::KIND_AGENT_PROFILE, &alice, "")
        .await
        .expect("a head");
    assert_eq!(head.deleted_through, 0);

    });
}

/// An `a`-tag deletion is a WATERMARK over the cell, not a row delete: every
/// version at or below the request's `created_at` is retired, including the ones
/// the relay is handed afterwards — while a genuinely newer version still lands.
#[test]
 fn severe_an_address_deletion_retires_the_cell_and_later_stale_versions() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let dana = Actor::new("dana");
    let kind_ = kind::KIND_READ_STATE; // 30078, parameterized-replaceable
    let d = "inbox";

    let v1 = signed_at(&dana, kind_, vec![tag("d", d)], "seq 1", T0_SECS - 100);
    let v2 = signed_at(&dana, kind_, vec![tag("d", d)], "seq 2", T0_SECS - 50);
    let other_cell = signed_at(&dana, kind_, vec![tag("d", "board")], "seq 9", T0_SECS - 50);
    for e in [&v1, &v2, &other_cell] {
        assert_eq!(submit(&env, &dana, e).await.status, 200);
    }

    let del = signed_at(
        &dana,
        kind::KIND_DELETION,
        vec![tag("a", &format!("{kind_}:{}:{d}", dana.pk_hex()))],
        "",
        T0_SECS,
    );
    assert_eq!(submit(&env, &dana, &del).await.status, 200);

    // The whole cell is gone; the sibling cell is untouched.
    assert_eq!(
        queryable_ids(&env, &dana, kind_).await,
        vec![to_hex(&other_cell.id)],
        "the address deletion took the wrong rows"
    );
    let head = head_of(&env, kind_, &dana, d).await.expect("a head");
    assert_eq!(
        head.deleted_through, T0_SECS,
        "the watermark was not raised"
    );
    assert!(admission(&env, &v2.id)
        .await
        .expect("a document")
        .is_deleted());

    // A stale version arriving AFTER the deletion (a client syncing history) is
    // absorbed and stays dead: this is the case a row-delete implementation gets
    // wrong, because there was nothing to delete when the request arrived.
    let stale = signed_at(&dana, kind_, vec![tag("d", d)], "seq 0", T0_SECS - 5);
    assert_eq!(submit(&env, &dana, &stale).await.status, 200);
    assert!(
        stored_event(&env, &stale.id).await.is_none(),
        "a version covered by the deletion watermark was served"
    );
    assert_eq!(
        queryable_ids(&env, &dana, kind_).await,
        vec![to_hex(&other_cell.id)]
    );

    // A version NEWER than the request is a fresh statement of the cell and must
    // publish normally — the watermark rides along, it does not clamp.
    let fresh = signed_at(&dana, kind_, vec![tag("d", d)], "seq 3", T0_SECS + 10);
    assert_eq!(submit(&env, &dana, &fresh).await.status, 200);
    let mut ids = queryable_ids(&env, &dana, kind_).await;
    ids.sort();
    let mut want = vec![to_hex(&other_cell.id), to_hex(&fresh.id)];
    want.sort();
    assert_eq!(
        ids, want,
        "a version newer than the deletion must be served"
    );
    let head = head_of(&env, kind_, &dana, d).await.expect("a head");
    assert_eq!(head.id, to_hex(&fresh.id));
    assert_eq!(
        head.deleted_through, T0_SECS,
        "installing a new winner must not clear the deletion watermark"
    );

    });
}

/// The torn-saga case the watermark exists for: a version is ADMITTED and then
/// its driver dies before storing anything, so the address deletion sweeps
/// nothing and tombstones nothing. When redelivery finally drives that saga, its
/// storage step re-creates the rows — and the replacement step must recognize
/// the watermark and erase them again.
#[test]
 fn severe_a_saga_admitted_before_an_address_deletion_cannot_land_after_it() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let erin = Actor::new("erin");
    let kind_ = kind::KIND_READ_STATE;
    let d = "inbox";
    let pending = signed_at(&erin, kind_, vec![tag("d", d)], "seq 1", T0_SECS - 20);

    // Admitted, nothing stored: the saga tore at its very first step.
    let mut ape = env.ape("guards");
    let mut events = env.store("events");
    let mut topics = env.topics("feed");
    let outcome = ingest::admit(&mut ape, COMMUNITY, &pending, None, T0_MS)
        .await
        .expect("admit the pending version");
    assert!(!outcome.was_duplicate());

    // The address deletion lands while that saga is in limbo.
    let del = signed_at(
        &erin,
        kind::KIND_DELETION,
        vec![tag("a", &format!("{kind_}:{}:{d}", erin.pk_hex()))],
        "",
        T0_SECS,
    );
    assert_eq!(submit(&env, &erin, &del).await.status, 200);
    assert_eq!(
        head_of(&env, kind_, &erin, d)
            .await
            .expect("a head")
            .deleted_through,
        T0_SECS
    );

    // Redelivery drives the torn saga to completion.
    ingest::drive(
        &mut events,
        &mut ape,
        &mut topics,
        COMMUNITY,
        FEED_GENERATION,
        &pending,
        &outcome,
        T0_MS,
        None,
    )
    .await
    .expect("the torn saga completes");

    assert!(
        stored_event(&env, &pending.id).await.is_none(),
        "a saga admitted before the deletion landed its version after it"
    );
    assert!(
        queryable_ids(&env, &erin, kind_).await.is_empty(),
        "the deleted cell came back through a re-driven saga"
    );

    });
}

/// A deletion whose target this relay does not hold cannot be authorized, so it
/// must NOT tombstone the id — otherwise a stranger could pre-emptively kill an
/// event id it does not own. The request is still accepted and stored.
#[test]
 fn severe_a_deletion_of_an_unknown_id_does_not_pre_kill_it() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice-victim");
    let mallory = Actor::new("mallory");

    // Alice's event exists only as bytes on her machine so far.
    let future = signed_at(
        &alice,
        kind::KIND_AGENT_PROFILE,
        vec![],
        r#"{"name":"later"}"#,
        T0_SECS - 1,
    );
    let del = signed_at(
        &mallory,
        kind::KIND_DELETION,
        vec![tag("e", &to_hex(&future.id))],
        "",
        T0_SECS,
    );
    assert_eq!(submit(&env, &mallory, &del).await.status, 200);
    assert!(
        admission(&env, &future.id).await.is_none(),
        "a deletion of an id the relay does not hold created a tombstone"
    );

    // Alice publishes it afterwards: it is served normally.
    assert_eq!(submit(&env, &alice, &future).await.status, 200);
    assert_eq!(
        served_ids(&env, &alice, kind::KIND_AGENT_PROFILE).await,
        vec![to_hex(&future.id)]
    );

    });
}

/// A deletion's write gate is checked against the channel of its FIRST `e`
/// target. A second target in another room must not be erased on the strength
/// of that one check, or an author expelled from a private room (or writing
/// into an archived one) could erase there by naming a writable room's message
/// first (finding_d582ddd7cd1e2af1).
#[test]
fn severe_a_deletion_only_erases_targets_in_the_channel_it_was_authorized_for() {
    common::run_scenario(|| async move {
        use buzz::relay::channel::{Role, Visibility};
        let env = gate_env().await;
        let alice = Actor::new("alice-two-rooms");
        common::seed_channel_with_owner(&env, "room-a", Visibility::Open, &alice).await;
        common::seed_channel_with_owner(&env, "room-b", Visibility::Open, &alice).await;
        common::seed_membership(&env, "room-a", &alice, Role::Owner).await;
        common::seed_membership(&env, "room-b", &alice, Role::Owner).await;
        let in_a = signed_at(&alice, kind::KIND_STREAM_MESSAGE, vec![tag("h", "room-a")], "a", T0_SECS - 20);
        let in_b = signed_at(&alice, kind::KIND_STREAM_MESSAGE, vec![tag("h", "room-b")], "b", T0_SECS - 19);
        assert_eq!(submit(&env, &alice, &in_a).await.status, 200);
        assert_eq!(submit(&env, &alice, &in_b).await.status, 200);

        let both = signed_at(
            &alice,
            kind::KIND_DELETION,
            vec![tag("e", &to_hex(&in_a.id)), tag("e", &to_hex(&in_b.id))],
            "",
            T0_SECS,
        );
        assert_eq!(submit(&env, &alice, &both).await.status, 200);

        assert!(
            admission(&env, &in_a.id).await.expect("a document").is_deleted(),
            "the target in the authorized channel is deleted"
        );
        assert!(
            !admission(&env, &in_b.id).await.expect("a document").is_deleted(),
            "a target in another channel must not ride on the first target's gate"
        );
        assert!(stored_event(&env, &in_b.id).await.is_some());
    });
}
