//! **A join request must never LOWER a role the actor already holds.**
//!
//! Found live, not by reading: creating a room over the HTTP face and then
//! sending a `9021` for it made the very next `9002` fail
//! `restricted: only owners/admins may change this channel setting`. The join
//! path called the shared membership upsert, which assigns `doc.role`
//! unconditionally, so a channel's own OWNER was silently demoted to member and
//! lost rename, archive and member management on the room they had just made.
//!
//! The distinction this gate defends is narrow and load-bearing: `PUT_USER` MAY
//! change a role — an admin demoting somebody is what that verb is for — while
//! `JOIN_REQUEST` may only ever ADD. Both go through membership state, so a
//! future refactor that unifies them would silently restore the bug; hence a
//! gate rather than a unit test, asserting on what the RELAY does to durable
//! state and on what the write face then permits.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::{Membership, Role};
use buzz::relay::{keys, kind};
use common::{Actor, COMMUNITY, FEED_GENERATION, SsoTestEnv, T0_MS, gate_env, post, signed, submit, tag};
use ferrite_app::serde_json::json;

/// The actor's live membership role for one channel, straight off the events
/// plane — the same row `ingress::live_role` reads to authorize an edit.
async fn role_of(env: &SsoTestEnv, channel: &str, who: &Actor) -> Option<Role> {
    let mut events = env.store("events");
    let key = keys::mem(COMMUNITY, channel, &who.pk).expect("a member key");
    let raw = events.get(&key).await.expect("member read")?;
    let doc: Membership = ferrite_app::serde_json::from_slice(&raw).expect("a membership decodes");
    doc.is_live().then_some(doc.role)
}

/// An owner who joins their own channel stays an owner, and may still edit it.
#[test]
 fn severe_an_owners_own_join_request_does_not_demote_them() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");

    let created = create_channel(&env, &owner, "war-room").await;
    assert_eq!(created.status, 201, "the owner creates the room");
    assert_eq!(
        role_of(&env, "war-room", &owner).await,
        Some(Role::Owner),
        "creating a room makes the creator its owner"
    );

    // The join a real client sends — buzz's own onboarding joins rooms it can
    // already see, including ones this identity created.
    let join = signed(
        &owner,
        kind::KIND_NIP29_JOIN_REQUEST,
        vec![tag("h", "war-room")],
        "",
    );
    assert_eq!(
        submit(&env, &owner, &join).await.status,
        200,
        "the join is accepted"
    );

    assert_eq!(
        role_of(&env, "war-room", &owner).await,
        Some(Role::Owner),
        "a join request must not lower the role the actor already holds"
    );

    // The consequence the bug actually had: the owner could no longer rename it.
    let rename = signed(
        &owner,
        kind::KIND_NIP29_EDIT_METADATA,
        vec![
            tag("h", "war-room"),
            tag("d", "war-room"),
            tag("name", "War Room"),
        ],
        "",
    );
    let reply = submit(&env, &owner, &rename).await;
    assert_eq!(
        reply.status,
        200,
        "the owner must still be able to rename their own room after joining it: {}",
        String::from_utf8_lossy(&reply.body)
    );

    });
}

/// A plain member's join is still a plain membership — the fix must not hand out
/// a role nobody granted.
#[test]
 fn severe_a_strangers_join_grants_only_membership() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let stranger = Actor::new("stranger");

    assert_eq!(create_channel(&env, &owner, "lobby").await.status, 201);

    let join = signed(
        &stranger,
        kind::KIND_NIP29_JOIN_REQUEST,
        vec![tag("h", "lobby")],
        "",
    );
    assert_eq!(submit(&env, &stranger, &join).await.status, 200);
    assert_eq!(
        role_of(&env, "lobby", &stranger).await,
        Some(Role::Member),
        "an open room's join grants membership, nothing more"
    );

    // And it buys no privilege: renaming is still refused.
    let rename = signed(
        &stranger,
        kind::KIND_NIP29_EDIT_METADATA,
        vec![
            tag("h", "lobby"),
            tag("d", "lobby"),
            tag("name", "Mine Now"),
        ],
        "",
    );
    let reply = submit(&env, &stranger, &rename).await;
    assert_ne!(
        reply.status, 200,
        "a member must not be able to rename a room they merely joined"
    );

    });
}

/// `PUT_USER` keeps its power to CHANGE a role, including downward — the join
/// fix must not have frozen roles generally.
#[test]
 fn severe_put_user_can_still_demote_where_join_cannot() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let deputy = Actor::new("deputy");

    assert_eq!(create_channel(&env, &owner, "ops").await.status, 201);

    // The owner promotes the deputy to admin…
    let promote = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        vec![
            tag("h", "ops"),
            tag("p", &deputy.pk_hex()),
            tag("role", "admin"),
        ],
        "",
    );
    assert_eq!(submit(&env, &owner, &promote).await.status, 200);
    assert_eq!(role_of(&env, "ops", &deputy).await, Some(Role::Admin));

    // …and demotes them again. This is the verb that MAY lower a role.
    let demote = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        vec![
            tag("h", "ops"),
            tag("p", &deputy.pk_hex()),
            tag("role", "member"),
        ],
        "",
    );
    assert_eq!(submit(&env, &owner, &demote).await.status, 200);
    assert_eq!(
        role_of(&env, "ops", &deputy).await,
        Some(Role::Member),
        "put_user must still be able to demote — only JOIN is forbidden from doing so"
    );

    // A subsequent self-join by the demoted deputy must not restore admin.
    let rejoin = signed(
        &deputy,
        kind::KIND_NIP29_JOIN_REQUEST,
        vec![tag("h", "ops")],
        "",
    );
    assert_eq!(submit(&env, &deputy, &rejoin).await.status, 200);
    assert_eq!(
        role_of(&env, "ops", &deputy).await,
        Some(Role::Member),
        "a join neither raises nor lowers: the demotion stands"
    );

    });
}

#[test]
fn severe_a_removed_admin_rejoins_as_a_plain_member() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let deputy = Actor::new("deputy");

    assert_eq!(create_channel(&env, &owner, "purge").await.status, 201);
    let promote = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        vec![tag("h", "purge"), tag("p", &deputy.pk_hex()), tag("role", "admin")],
        "",
    );
    assert_eq!(submit(&env, &owner, &promote).await.status, 200);
    assert_eq!(role_of(&env, "purge", &deputy).await, Some(Role::Admin));

    let remove = signed(
        &owner,
        kind::KIND_NIP29_REMOVE_USER,
        vec![tag("h", "purge"), tag("p", &deputy.pk_hex())],
        "",
    );
    assert_eq!(submit(&env, &owner, &remove).await.status, 200);
    assert_eq!(role_of(&env, "purge", &deputy).await, None);

    let rejoin = signed(&deputy, kind::KIND_NIP29_JOIN_REQUEST, vec![tag("h", "purge")], "");
    assert_eq!(submit(&env, &deputy, &rejoin).await.status, 200);
    assert_eq!(
        role_of(&env, "purge", &deputy).await,
        Some(Role::Member),
        "a removed admin must not regain moderator rights by joining again"
    );
    });
}

#[test]
fn severe_a_plain_member_cannot_demote_an_admin_on_an_anyone_add_policy_channel() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let deputy = Actor::new("deputy");
    let member = Actor::new("member");

    assert_eq!(create_channel(&env, &owner, "hall").await.status, 201);
    let promote = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        vec![tag("h", "hall"), tag("p", &deputy.pk_hex()), tag("role", "admin")],
        "",
    );
    assert_eq!(submit(&env, &owner, &promote).await.status, 200);
    let join = signed(&member, kind::KIND_NIP29_JOIN_REQUEST, vec![tag("h", "hall")], "");
    assert_eq!(submit(&env, &member, &join).await.status, 200);

    let demote = signed(
        &member,
        kind::KIND_NIP29_PUT_USER,
        vec![tag("h", "hall"), tag("p", &deputy.pk_hex()), tag("role", "member")],
        "",
    );
    assert_ne!(submit(&env, &member, &demote).await.status, 200);
    assert_eq!(
        role_of(&env, "hall", &deputy).await,
        Some(Role::Admin),
        "a member's PUT_USER must leave the admin's role alone"
    );
    });
}

#[test]
fn severe_a_demoted_admins_stored_demotion_is_refused_when_the_saga_is_driven() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let deputy = Actor::new("deputy");
    let early = Actor::new("early");
    let late = Actor::new("late");

    assert_eq!(create_channel(&env, &owner, "stale").await.status, 201);
    for who in [&deputy, &early, &late] {
        let promote = signed(
            &owner,
            kind::KIND_NIP29_PUT_USER,
            vec![tag("h", "stale"), tag("p", &who.pk_hex()), tag("role", "admin")],
            "",
        );
        assert_eq!(submit(&env, &owner, &promote).await.status, 200);
    }

    let demotion = |target: &Actor| {
        signed(
            &deputy,
            kind::KIND_NIP29_PUT_USER,
            vec![tag("h", "stale"), tag("p", &target.pk_hex()), tag("role", "member")],
            "",
        )
    };
    let store_both = [demotion(&early), demotion(&late)];
    let mut outcomes = Vec::new();
    for event in &store_both {
        let mut ape = env.ape("guards");
        outcomes.push(
            buzz::relay::ingest::admit(&mut ape, COMMUNITY, event, Some("stale".into()), T0_MS)
                .await
                .expect("admission"),
        );
        let mut store = env.store("events");
        buzz::relay::ingest::apply_storage(&mut store, COMMUNITY, event, Some("stale".into()))
            .await
            .expect("storage");
    }

    drive_stored(&env, &store_both[0], &outcomes[0]).await.expect("drive");
    assert_eq!(
        role_of(&env, "stale", &early).await,
        Some(Role::Member),
        "while its author is an admin the stored demotion takes effect"
    );

    let demote_deputy = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        vec![tag("h", "stale"), tag("p", &deputy.pk_hex()), tag("role", "member")],
        "",
    );
    assert_eq!(submit(&env, &owner, &demote_deputy).await.status, 200);
    assert_eq!(role_of(&env, "stale", &deputy).await, Some(Role::Member));

    drive_stored(&env, &store_both[1], &outcomes[1]).await.expect("drive");
    assert_eq!(
        role_of(&env, "stale", &late).await,
        Some(Role::Admin),
        "a command stored while its author was an admin must not demote an admin after the author lost the role"
    );
    });
}

#[test]
fn severe_a_membership_change_cannot_run_while_another_is_in_flight_on_the_channel() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let deputy = Actor::new("deputy");
    let peer = Actor::new("peer");

    assert_eq!(create_channel(&env, &owner, "ordered").await.status, 201);
    for who in [&deputy, &peer] {
        let promote = signed(
            &owner,
            kind::KIND_NIP29_PUT_USER,
            vec![tag("h", "ordered"), tag("p", &who.pk_hex()), tag("role", "admin")],
            "",
        );
        assert_eq!(submit(&env, &owner, &promote).await.status, 200);
    }

    let demote_peer = signed(
        &deputy,
        kind::KIND_NIP29_PUT_USER,
        vec![tag("h", "ordered"), tag("p", &peer.pk_hex()), tag("role", "member")],
        "",
    );
    let outcome = {
        let mut ape = env.ape("guards");
        let outcome = buzz::relay::ingest::admit(
            &mut ape,
            COMMUNITY,
            &demote_peer,
            Some("ordered".into()),
            T0_MS,
        )
        .await
        .expect("admission");
        let mut store = env.store("events");
        buzz::relay::ingest::apply_storage(
            &mut store,
            COMMUNITY,
            &demote_peer,
            Some("ordered".into()),
        )
        .await
        .expect("storage");
        outcome
    };

    hold_lease(&env, "ordered", &"ab".repeat(32)).await;

    assert!(
        drive_stored(&env, &demote_peer, &outcome).await.is_err(),
        "a membership effect must not run while another holds the channel"
    );
    assert_eq!(
        role_of(&env, "ordered", &peer).await,
        Some(Role::Admin),
        "the refused effect must have written nothing"
    );

    hold_lease(&env, "ordered", &buzz::relay::crypto::to_hex(&demote_peer.id)).await;
    assert!(
        drive_stored(&env, &demote_peer, &outcome).await.is_err(),
        "another delivery of the event that holds the lease must wait for it"
    );

    release_lease(&env, "ordered").await;
    let demote_deputy = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        vec![tag("h", "ordered"), tag("p", &deputy.pk_hex()), tag("role", "member")],
        "",
    );
    assert_eq!(submit(&env, &owner, &demote_deputy).await.status, 200);

    drive_stored(&env, &demote_peer, &outcome).await.expect("drive");
    assert_eq!(
        role_of(&env, "ordered", &peer).await,
        Some(Role::Admin),
        "once serialized behind the deputy's demotion, the stored command is refused"
    );
    });
}

/// Overwrite the channel's membership lease so `holder` holds it, unexpired.
async fn hold_lease(env: &SsoTestEnv, channel: &str, holder: &str) {
    let key = keys::membership_lock(COMMUNITY, channel).expect("lock key");
    let value = format!(r#"{{"holder":"{holder}","expires_ms":{}}}"#, T0_MS + 30_000);
    let mut ape = env.ape("guards");
    let before = ape.get(&key).await.expect("lock read");
    ape.commit_n_if(&key, before.as_deref(), &[(key.as_slice(), value.as_bytes())])
        .await
        .expect("lock write")
        .expect("lock replaced");
}

async fn release_lease(env: &SsoTestEnv, channel: &str) {
    let key = keys::membership_lock(COMMUNITY, channel).expect("lock key");
    let freed = br#"{"holder":"","expires_ms":0}"#;
    let mut ape = env.ape("guards");
    let before = ape.get(&key).await.expect("lock read");
    ape.commit_n_if(&key, before.as_deref(), &[(key.as_slice(), freed.as_slice())])
        .await
        .expect("lock release")
        .expect("lock replaced");
}

/// Drive an event's saga directly, bypassing admission authorization.
async fn drive_stored(
    env: &SsoTestEnv,
    event: &buzz::relay::event::Event,
    outcome: &buzz::relay::ingest::AdmitOutcome,
) -> Result<(), String> {
    let mut ape = env.ape("guards");
    let mut events = env.store("events");
    let mut topics = env.topics("feed");
    buzz::relay::ingest::drive(
        &mut events,
        &mut ape,
        &mut topics,
        COMMUNITY,
        FEED_GENERATION,
        event,
        outcome,
        T0_MS,
        None,
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Create a room through the real HTTP route, exactly as a client does — the
/// same shape `relay_fix_channel_ids` uses.
async fn create_channel(
    env: &SsoTestEnv,
    caller: &Actor,
    id: &str,
) -> ferrite_app::IngressReply {
    let body = json!({ "id": id, "name": id, "type": "stream", "visibility": "open" });
    env.handle(post(
        caller,
        "/buzz/api/channels",
        ferrite_app::serde_json::to_vec(&body).expect("body encodes"),
    ))
    .await
    .expect("a create reply")
}
