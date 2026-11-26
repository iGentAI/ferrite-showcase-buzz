//! **NIP-29 group creation (kind 9007), through the wire a standard client uses.**
//!
//! The relay advertises NIP-29 and used to refuse its group-creation kind: a 9007
//! named its group in an `h` tag, the write path resolved that tag, and then failed
//! closed with `not found: no such channel`. So a standard NIP-29 client could not
//! create a room at all, while `kind.rs` documented that 9007 "creates the channel,
//! so it cannot reference one".
//!
//! These gates hold the fix to the same standard as the HTTP creation route: the room
//! really exists afterwards, its creator really owns it, an id already taken is not
//! quietly taken over, and an unusable id writes nothing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::common;

use buzz::relay::channel::{Channel, Role, Visibility};
use buzz::relay::crypto::to_hex;
use buzz::relay::kind;
use common::{
    Actor, channel_doc, gate_env, membership, post, returned, seed_channel, seed_channel_with_owner,
    seed_removed_membership, signed, stored_event, submit, tag,
};
use common::SsoTestEnv as TestEnv;

async fn channel(env: &TestEnv, id: &str) -> Option<Channel> {
    let bytes = channel_doc(env, id).await?;
    Some(ferrite_app::serde_json::from_slice(&bytes).expect("channel decodes"))
}

/// A 9007 naming `id`, with whatever extra tags the case wants.
fn create_group(actor: &Actor, id: &str, extra: Vec<Vec<String>>) -> buzz::relay::event::Event {
    let mut tags = vec![tag("h", id)];
    tags.extend(extra);
    signed(actor, kind::KIND_NIP29_CREATE_GROUP, tags, "")
}

/// **A standard client's 9007 creates a room it can then post into.**
///
/// The whole point: before this, the same two frames a NIP-29 client sends (create,
/// then post) ended in `no such channel` on the first one.
#[test]
 fn severe_a_nip29_create_makes_a_room_its_author_owns() {
    common::run_scenario(|| async move {
    let gate = gate_env().await;
    let founder = Actor::new("founder");
    let id = "nip29-room";

    assert!(channel(&gate, id).await.is_none(), "nothing there yet");
    let create = create_group(&founder, id, vec![tag("name", "The Room")]);
    let reply = submit(&gate, &founder, &create).await;
    assert_eq!(
        reply.status,
        200,
        "a NIP-29 create was refused: {}",
        String::from_utf8_lossy(&reply.body)
    );

    // The room exists, carries the name the event asked for, and is owned by its
    // author — the same state the HTTP creation route leaves behind.
    let doc = channel(&gate, id).await.expect("the room exists");
    assert_eq!(doc.name, "The Room");
    assert_eq!(
        doc.visibility,
        Visibility::Open,
        "NIP-29 groups are public by default"
    );
    assert!(
        doc.owners.contains(&founder.pk_hex()),
        "the creator does not own the room: {:?}",
        doc.owners
    );
    let membership = membership(&gate, id, &founder).await.expect("an owner row");
    assert_eq!(membership.role, Role::Owner);
    assert!(membership.is_live());

    // And the create is usable: the author can post into the room it just made.
    let note = signed(
        &founder,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", id)],
        "first message in a room I just created",
    );
    let posted = submit(&gate, &founder, &note).await;
    assert_eq!(
        posted.status,
        200,
        "the room a 9007 created does not accept its author's writes: {}",
        String::from_utf8_lossy(&posted.body)
    );
    assert!(stored_event(&gate, &note.id).await.is_some());

    // The private marker is honoured, in NIP-29's own VALUELESS spelling: a
    // `["private"]` tag has no element at index 1, so reading it with `first_tag`
    // would miss it and hand back a public room.
    let private_id = "nip29-private";
    let private = create_group(&founder, private_id, vec![vec!["private".to_string()]]);
    assert_eq!(submit(&gate, &founder, &private).await.status, 200);
    assert_eq!(
        channel(&gate, private_id).await.expect("exists").visibility,
        Visibility::Private,
        "the valueless private marker was ignored"
    );

    });
}

/// **A create that tore between its two writes is FINISHED by a re-drive.**
///
/// Creation is a channel document plus the creator's owner membership, and the
/// request queue is at-least-once, so the window between them is reachable: the
/// document commits, the instance dies, the envelope comes back. If the retry saw a
/// room it owned and simply returned, the membership would never be written — and in
/// a PRIVATE room that is a room its own creator cannot post into, permanently.
#[test]
 fn severe_a_torn_nip29_create_is_finished_by_a_re_drive() {
    common::run_scenario(|| async move {
    let gate = gate_env().await;
    let founder = Actor::new("founder");
    let id = "nip29-torn";

    // Exactly the state a torn create leaves: the document, and no membership.
    // (`seed_channel` writes only the document; `seed_channel_with_owner` is the
    // completed form.)
    seed_channel(&gate, id, Visibility::Private, &founder).await;
    assert!(
        membership(&gate, id, &founder).await.is_none(),
        "the fixture must reproduce the tear, not a finished create"
    );

    // The damage is real and observable: the creator cannot write to their own room.
    let blocked = signed(
        &founder,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", id)],
        "can I speak in my own room?",
    );
    assert_eq!(
        submit(&gate, &founder, &blocked).await.status,
        403,
        "the fixture is not a torn create if the owner can already post"
    );

    // Re-driving the create finishes it.
    let again = submit(&gate, &founder, &create_group(&founder, id, vec![])).await;
    assert_eq!(
        again.status,
        200,
        "a re-drive of an owner's own create was refused: {}",
        String::from_utf8_lossy(&again.body)
    );
    let repaired = membership(&gate, id, &founder)
        .await
        .expect("the owner membership was not repaired");
    assert_eq!(repaired.role, Role::Owner);
    assert!(repaired.is_live());

    // ...and the room works now.
    let speaks = signed(
        &founder,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", id)],
        "now I can speak",
    );
    assert_eq!(
        submit(&gate, &founder, &speaks).await.status,
        200,
        "the repair did not restore the creator's own access"
    );

    // The repair must not resurrect a REMOVED membership: that would make 9007 a
    // way for an ex-owner to reinstate themselves with nobody granting it.
    let ex = Actor::new("ex-owner");
    seed_channel(&gate, "nip29-removed", Visibility::Private, &ex).await;
    seed_removed_membership(&gate, "nip29-removed", &ex).await;
    let retry = submit(&gate, &ex, &create_group(&ex, "nip29-removed", vec![])).await;
    assert_eq!(
        retry.status,
        200,
        "{}",
        String::from_utf8_lossy(&retry.body)
    );
    let row = membership(&gate, "nip29-removed", &ex)
        .await
        .expect("the removed row is still there");
    assert!(
        !row.is_live(),
        "a removed membership was resurrected by re-sending a create"
    );

    });
}

/// **A group id belongs to whoever created it first.**
///
/// The creation has to be idempotent, because the request queue is at-least-once and
/// a re-driven saga runs it again — but idempotent for the OWNER only. If any caller
/// could re-create an existing id, 9007 would be a way to take over a room by naming
/// it.
#[test]
 fn severe_a_nip29_create_cannot_take_over_an_existing_room() {
    common::run_scenario(|| async move {
    let gate = gate_env().await;
    let founder = Actor::new("founder");
    let interloper = Actor::new("interloper");
    let id = "nip29-contested";

    assert_eq!(
        submit(
            &gate,
            &founder,
            &create_group(&founder, id, vec![tag("name", "Mine")])
        )
        .await
        .status,
        200
    );

    // Somebody else naming the same id is refused, and changes nothing.
    let theirs = create_group(&interloper, id, vec![tag("name", "Actually Mine")]);
    let refused = submit(&gate, &interloper, &theirs).await;
    assert_eq!(
        refused.status,
        409,
        "an existing group id was taken over: {}",
        String::from_utf8_lossy(&refused.body)
    );
    let doc = channel(&gate, id).await.expect("still there");
    assert_eq!(doc.name, "Mine", "the room was renamed by a stranger");
    assert!(!doc.owners.contains(&interloper.pk_hex()));
    assert!(
        membership(&gate, id, &interloper).await.is_none(),
        "a refused create left a membership row behind"
    );

    // The OWNER re-sending its own create is accepted — which is what makes a
    // re-driven saga safe — and does not duplicate anything.
    let again = submit(&gate, &founder, &create_group(&founder, id, vec![])).await;
    assert_eq!(
        again.status,
        200,
        "an owner's repeat create was refused, so a re-drive would wedge: {}",
        String::from_utf8_lossy(&again.body)
    );
    let doc = channel(&gate, id).await.expect("still there");
    assert_eq!(
        doc.name, "Mine",
        "a repeat create must not rewrite the room"
    );
    assert_eq!(
        doc.owners.len(),
        1,
        "a repeat create duplicated the owner set: {:?}",
        doc.owners
    );

    });
}

/// **An unusable group id writes nothing.**
///
/// The id becomes an authorization key, so one that cannot be spelled as a key must
/// be refused rather than escaped into some other room's namespace.
#[test]
 fn severe_a_nip29_create_refuses_an_unusable_group_id() {
    common::run_scenario(|| async move {
    let gate = gate_env().await;
    let founder = Actor::new("founder");
    let real = "nip29-real";
    assert_eq!(
        submit(&gate, &founder, &create_group(&founder, real, vec![]))
            .await
            .status,
        200
    );

    for hostile in ["", "a/b", "../nip29-real", "a b", "!escaped"] {
        let event = create_group(&founder, hostile, vec![]);
        let reply = submit(&gate, &founder, &event).await;
        assert!(
            reply.status == 400 || reply.status == 404,
            "the group id {hostile:?} was accepted with {}: {}",
            reply.status,
            String::from_utf8_lossy(&reply.body)
        );
        assert!(
            stored_event(&gate, &event.id).await.is_none(),
            "a refused create stored its event: {hostile:?}"
        );
    }

    // The real room is untouched by any of the attempts.
    let doc = channel(&gate, real).await.expect("still there");
    assert_eq!(doc.owners.len(), 1);
    assert!(doc.owners.contains(&to_hex(&founder.pk)));

    });
}
