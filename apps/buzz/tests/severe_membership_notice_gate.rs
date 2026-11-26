//! **The relay tells an added participant, promptly** (the ~2-minute DM).
//!
//! A client's live subscriptions are per KNOWN channel (`{"#h":[id]}`), so a room
//! it has never heard of — the DM you were just added to — matches none of them,
//! and discovery falls back to the channel-list poll. buzz-web has the fast path
//! already: one always-on subscription for kinds 44100/44101 filtered
//! `{"#p":[me]}` whose handler invalidates the channel list. Those kinds were
//! declared here and NOTHING MINTED THEM.
//!
//! This gate pins the relay side of that fix:
//!
//! * opening a DM leaves a relay-SIGNED 44100 for the other participant, scoped
//!   to the DM room, naming the trigger;
//! * the notice is `#p`-gated: the subject reads it, a stranger cannot, and the
//!   subject's own filter (`kinds:[44100], #p:[me]`) is admissible;
//! * no notice is minted for the actor (they already know) and none for a
//!   command the state did not honor;
//! * a re-driven saga mints NO second notice — the id is deterministic in the
//!   trigger, which is the dedup;
//! * a client cannot forge one.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::{Role, Visibility};
use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::{ingest, keys, kind, store as buzz_store};
use common::{
    Actor, COMMUNITY, FEED_GENERATION, T0_MS, T0_SECS, admission, gate_env, error_text, index_keys,
    membership, query, returned, seed_channel_with_owner, signed, signed_at, stored_event,
    submit, tag,
};
use ferrite_app::serde_json::json;
use common::SsoTestEnv as TestEnv;

/// The config key an operator seeds the relay's signing identity into
/// (`ingress::RELAY_IDENTITY_CONFIG`).
const RELAY_IDENTITY_CONFIG: &[u8] = b"buzz-relay-identity";

fn relay() -> Actor {
    Actor::new("the-relay-itself")
}

async fn seed_relay_identity(env: &TestEnv) -> Actor {
    let relay = relay();
    let mut config = env.store("config");
    buzz_store::put_raw(
        &mut config,
        RELAY_IDENTITY_CONFIG,
        to_hex(&relay.sk).as_bytes(),
    )
    .await
    .expect("relay identity seed");
    relay
}

/// Every stored event of one kind, by the kind index (the body must still be
/// there — the reader rule).
async fn stored_of_kind(env: &TestEnv, kind_: u32) -> Vec<Event> {
    let mut out = Vec::new();
    for key in index_keys(env, &keys::kind_prefix(COMMUNITY, kind_)).await {
        let Some((_, id)) = keys::parse_index_tail(&key) else {
            continue;
        };
        if let Some(event) = stored_event(env, &id).await {
            out.push(event);
        }
    }
    out
}

fn first_tag<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event
        .tags
        .iter()
        .find(|t| t.first().map(String::as_str) == Some(name))
        .and_then(|t| t.get(1))
        .map(String::as_str)
}

fn dm_open(opener: &Actor, other: &Actor, at: u64) -> Event {
    signed_at(
        opener,
        kind::KIND_DM_OPEN,
        vec![tag("p", &other.pk_hex())],
        "",
        at,
    )
}

/// Opening a DM notifies the OTHER participant — and only them — with a
/// relay-signed 44100 scoped to the new room.
#[test]
 fn severe_opening_a_dm_notifies_the_other_participant() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let relay = seed_relay_identity(&env).await;
    let alice = Actor::new("alice");
    let bob = Actor::new("bob");
    let room = buzz::relay::channel::dm_participant_hash(&[alice.pk, bob.pk]);

    let open = dm_open(&alice, &bob, T0_SECS);
    assert_eq!(submit(&env, &alice, &open).await.status, 200);

    let notices = stored_of_kind(&env, kind::KIND_MEMBER_ADDED_NOTIFICATION).await;
    assert_eq!(
        notices.len(),
        1,
        "exactly one notice — for bob, not for the actor: {notices:?}"
    );
    let notice = &notices[0];
    assert_eq!(notice.pubkey, relay.pk, "the notice is signed by the RELAY");
    assert_eq!(
        first_tag(notice, "p"),
        Some(bob.pk_hex().as_str()),
        "about bob"
    );
    assert_eq!(
        first_tag(notice, "h"),
        Some(room.as_str()),
        "scoped to the DM room"
    );
    assert_eq!(
        first_tag(notice, "e"),
        Some(to_hex(&open.id).as_str()),
        "names the trigger, which is what makes the id deterministic"
    );
    assert_eq!(
        notice.created_at, T0_SECS,
        "stamped with the trigger's admission instant, not a fresh clock read"
    );

    // The subject can read it through the room's timeline; a stranger cannot.
    let mine = query(&env, &bob, &json!({ "channel": room, "limit": 50 })).await;
    assert_eq!(mine.status, 200, "{}", String::from_utf8_lossy(&mine.body));
    assert!(
        returned(&mine, notice),
        "the subject must be able to read their own notice"
    );

    let stranger = Actor::new("carol-outsider");
    let theirs = query(&env, &stranger, &json!({ "channel": room, "limit": 50 })).await;
    assert!(
        !returned(&theirs, notice),
        "a non-participant read a membership notice: {}",
        String::from_utf8_lossy(&theirs.body)
    );

    // A re-drive of the trigger's saga mints no second notice: the notice id is a
    // function of the trigger, so the second attempt hits its own admission.
    let mut ape = env.ape("guards");
    let mut events = env.store("events");
    let mut topics = env.topics("feed");
    let outcome = ingest::admit(&mut ape, COMMUNITY, &open, None, T0_MS)
        .await
        .expect("re-admit");
    let seckey = relay.sk;
    ingest::drive(
        &mut events,
        &mut ape,
        &mut topics,
        COMMUNITY,
        FEED_GENERATION,
        &open,
        &outcome,
        T0_MS,
        Some(&seckey),
    )
    .await
    .expect("re-drive");
    assert_eq!(
        stored_of_kind(&env, kind::KIND_MEMBER_ADDED_NOTIFICATION)
            .await
            .len(),
        1,
        "a re-driven saga minted a second notice"
    );

    });
}

/// An admin adding and then removing a member notifies that member both times —
/// and a client can never forge either notice.
#[test]
 fn severe_add_and_remove_notify_the_subject_and_cannot_be_forged() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let relay = seed_relay_identity(&env).await;
    let owner = Actor::new("room-owner");
    let member = Actor::new("added-member");
    let channel = "chan-notices";
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;

    let add = signed_at(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        vec![
            tag("h", channel),
            tag("p", &member.pk_hex()),
            tag("role", Role::Member.as_str()),
        ],
        "",
        T0_SECS,
    );
    assert_eq!(submit(&env, &owner, &add).await.status, 200);
    let added = stored_of_kind(&env, kind::KIND_MEMBER_ADDED_NOTIFICATION).await;
    assert_eq!(added.len(), 1, "the added member is notified: {added:?}");
    assert_eq!(first_tag(&added[0], "p"), Some(member.pk_hex().as_str()));
    assert_eq!(first_tag(&added[0], "h"), Some(channel));
    assert_eq!(added[0].pubkey, relay.pk);

    let remove = signed_at(
        &owner,
        kind::KIND_NIP29_REMOVE_USER,
        vec![tag("h", channel), tag("p", &member.pk_hex())],
        "",
        T0_SECS + 1,
    );
    assert_eq!(submit(&env, &owner, &remove).await.status, 200);
    let removed = stored_of_kind(&env, kind::KIND_MEMBER_REMOVED_NOTIFICATION).await;
    assert_eq!(
        removed.len(),
        1,
        "the removed member is notified: {removed:?}"
    );
    assert_eq!(first_tag(&removed[0], "p"), Some(member.pk_hex().as_str()));
    // Still exactly one ADD notice: the removal minted no second one.
    assert_eq!(
        stored_of_kind(&env, kind::KIND_MEMBER_ADDED_NOTIFICATION)
            .await
            .len(),
        1
    );

    // A client-submitted notice is refused outright — the authority of a notice
    // is the relay's signature.
    for kind_ in [
        kind::KIND_MEMBER_ADDED_NOTIFICATION,
        kind::KIND_MEMBER_REMOVED_NOTIFICATION,
    ] {
        let forged = signed_at(
            &owner,
            kind_,
            vec![tag("h", channel), tag("p", &owner.pk_hex())],
            "",
            T0_SECS + 2,
        );
        let reply = submit(&env, &owner, &forged).await;
        assert_eq!(reply.status, 403, "kind {kind_} was accepted from a client");
        assert!(
            error_text(&reply).contains("relay-signed"),
            "{}",
            error_text(&reply)
        );
    }

    });
}

/// A command the state did not honor notifies nobody: an outsider's attempt to
/// add somebody to a room they do not administer is refused, and a refused
/// command must not leave a "you were added" notice behind.
#[test]
 fn severe_a_refused_membership_command_notifies_nobody() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_relay_identity(&env).await;
    let owner = Actor::new("real-owner");
    let outsider = Actor::new("outsider");
    let victim = Actor::new("victim");
    let channel = "chan-refused";
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;

    let add = signed_at(
        &outsider,
        kind::KIND_NIP29_PUT_USER,
        vec![tag("h", channel), tag("p", &victim.pk_hex())],
        "",
        T0_SECS,
    );
    let reply = submit(&env, &outsider, &add).await;
    assert_ne!(reply.status, 200, "an outsider must not add members");
    assert!(
        stored_of_kind(&env, kind::KIND_MEMBER_ADDED_NOTIFICATION)
            .await
            .is_empty(),
        "a refused command left a notice behind"
    );

    });
}

/// With no relay identity configured, the notices are simply absent — a missing
/// operator key must never fail a member's write (the projection rule).
#[test]
 fn severe_without_a_relay_identity_a_dm_still_opens() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice-no-identity");
    let bob = Actor::new("bob-no-identity");
    assert_eq!(
        submit(&env, &alice, &dm_open(&alice, &bob, T0_SECS))
            .await
            .status,
        200
    );
    let room = buzz::relay::channel::dm_participant_hash(&[alice.pk, bob.pk]);
    assert!(
        membership(&env, &room, &bob)
            .await
            .is_some_and(|m| m.is_live()),
        "the DM itself must still work"
    );
    assert!(stored_of_kind(&env, kind::KIND_MEMBER_ADDED_NOTIFICATION)
        .await
        .is_empty());

    });
}
