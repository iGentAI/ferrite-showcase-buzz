//! **M1.1 — channel-metadata edits (kind 9002), decided by the WIRING.**
//!
//! A 9002 edit renames a room, sets its topic/purpose, flips visibility, or
//! archives it. Authorization is split (privileged fields need owner/admin; the
//! two member fields need only membership) and an archived room accepts only the
//! unarchive. Every case submits a real signed 9002 through `POST
//! /buzz/api/events` and asserts on the durable channel document.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::{Channel, Role, Visibility};
use buzz::relay::kind;
use common::{
    Actor, channel_doc, gate_env, error_text, membership, seed_channel_with_owner, seed_membership,
    signed, submit, tag,
};
use common::SsoTestEnv as TestEnv;

async fn channel(env: &TestEnv, id: &str) -> Channel {
    let bytes = channel_doc(env, id).await.expect("channel present");
    ferrite_app::serde_json::from_slice(&bytes).expect("channel decodes")
}

/// An owner renames, sets topic/purpose, and flips visibility; state lands and
/// `seq`/`updated_at` advance.
#[test]
 fn severe_owner_edits_metadata() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let id = "chan-edit";
    seed_channel_with_owner(&env, id, Visibility::Open, &owner).await;
    let before = channel(&env, id).await;

    let edit = signed(
        &owner,
        kind::KIND_NIP29_EDIT_METADATA,
        vec![
            tag("h", id),
            tag("name", "renamed"),
            tag("topic", "the topic"),
            tag("purpose", "the purpose"),
            tag("visibility", "private"),
        ],
        "",
    );
    let reply = submit(&env, &owner, &edit).await;
    assert_eq!(reply.status, 200, "edit accepted: {}", error_text(&reply));

    let c = channel(&env, id).await;
    assert_eq!(c.name, "renamed");
    assert_eq!(c.topic.as_deref(), Some("the topic"));
    assert_eq!(c.purpose.as_deref(), Some("the purpose"));
    assert_eq!(c.visibility, Visibility::Private);
    assert!(c.seq > before.seq, "the mutation counter advanced");

    });
}

/// A plain member may set `topic`/`purpose` but not a privileged field; a
/// non-member may set neither.
#[test]
 fn severe_edit_authorization_is_split() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let member = Actor::new("member");
    let stranger = Actor::new("stranger");
    let id = "chan-edit-auth";
    seed_channel_with_owner(&env, id, Visibility::Private, &owner).await;
    seed_membership(&env, id, &member, Role::Member).await;

    // Member sets the topic: allowed.
    let topic = signed(
        &member,
        kind::KIND_NIP29_EDIT_METADATA,
        vec![tag("h", id), tag("topic", "member topic")],
        "",
    );
    assert_eq!(submit(&env, &member, &topic).await.status, 200);
    assert_eq!(
        channel(&env, id).await.topic.as_deref(),
        Some("member topic")
    );

    // Member tries to rename (privileged): refused, name unchanged. 403 rather
    // than 401 — they are a member in good standing, just not an owner.
    let rename = signed(
        &member,
        kind::KIND_NIP29_EDIT_METADATA,
        vec![tag("h", id), tag("name", "hijacked")],
        "",
    );
    assert_eq!(submit(&env, &member, &rename).await.status, 403);
    assert_ne!(channel(&env, id).await.name, "hijacked");

    // Stranger tries to set the topic: refused (not a member).
    let by_stranger = signed(
        &stranger,
        kind::KIND_NIP29_EDIT_METADATA,
        vec![tag("h", id), tag("topic", "stranger")],
        "",
    );
    assert_eq!(submit(&env, &stranger, &by_stranger).await.status, 403);

    });
}

/// Archive blocks further edits (except the unarchive), and unarchive re-opens
/// the room and re-arms a TTL.
#[test]
 fn severe_archive_then_only_unarchive_is_accepted() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let id = "chan-archive";
    seed_channel_with_owner(&env, id, Visibility::Open, &owner).await;

    // Archive with a TTL set.
    let archive = signed(
        &owner,
        kind::KIND_NIP29_EDIT_METADATA,
        vec![tag("h", id), tag("archived", "true"), tag("ttl", "3600")],
        "",
    );
    assert_eq!(submit(&env, &owner, &archive).await.status, 200);
    let c = channel(&env, id).await;
    assert!(c.archived_at.is_some());
    assert_eq!(c.ttl_seconds, Some(3600));
    // Archived: the deadline is not armed while archived.
    assert!(c.ttl_deadline.is_none());

    // A rename on an archived channel: refused.
    let rename = signed(
        &owner,
        kind::KIND_NIP29_EDIT_METADATA,
        vec![tag("h", id), tag("name", "nope")],
        "",
    );
    assert_eq!(
        submit(&env, &owner, &rename).await.status,
        403,
        "an archived channel refuses edits"
    );

    // Even an ordinary message is refused on the archived room.
    let msg = signed(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", id)],
        "hello",
    );
    assert_eq!(submit(&env, &owner, &msg).await.status, 403);

    // Unarchive: accepted, and the TTL deadline is re-armed from now.
    let unarchive = signed(
        &owner,
        kind::KIND_NIP29_EDIT_METADATA,
        vec![tag("h", id), tag("archived", "false")],
        "",
    );
    assert_eq!(submit(&env, &owner, &unarchive).await.status, 200);
    let c = channel(&env, id).await;
    assert!(c.archived_at.is_none(), "unarchived");
    assert!(c.ttl_deadline.is_some(), "ttl re-armed on unarchive");

    });
}

/// A malformed edit is rejected before it touches state: an unknown `archived`
/// value, a non-positive `ttl`, and an edit naming no recognized field.
#[test]
 fn severe_malformed_edits_are_refused() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let id = "chan-malformed";
    seed_channel_with_owner(&env, id, Visibility::Open, &owner).await;

    for tags in [
        vec![tag("h", id), tag("archived", "maybe")],
        vec![tag("h", id), tag("ttl", "0")],
        vec![tag("h", id), tag("ttl", "-5")],
        vec![tag("h", id), tag("name", "   ")],
        vec![tag("h", id), tag("unrecognized", "x")],
    ] {
        let bad = signed(&owner, kind::KIND_NIP29_EDIT_METADATA, tags, "");
        let reply = submit(&env, &owner, &bad).await;
        assert_eq!(
            reply.status,
            400,
            "malformed edit refused: {}",
            error_text(&reply)
        );
    }
    // The channel is untouched by any of the refusals.
    assert_eq!(channel(&env, id).await.name, format!("channel {id}"));

    });
}
