//! Verbs this relay refuses because nothing applies them.
//!
//! An event that is accepted, stored, audited and delivered while no code path
//! acts on it reports success to a caller who has no way to learn otherwise.
//! Plan 025 closed that for NIP-43's 9030–9032; an audit of every registered kind
//! against the code that applies one turned up more. Kind 9008 now has a real,
//! authorized tombstone effect and is covered here as a positive lifecycle gate.
//!
//! Refusal gates assert both that the verb is refused and that nothing reached
//! storage. A refusal that still wrote the event would leave the silent path
//! open through any query. The delete-group lifecycle gate instead proves the
//! authorized event is stored and its promised tombstone is materialized.

use super::common;

use buzz::relay::channel::Visibility;
use buzz::relay::kind;
use common::{
    Actor, channel_doc, channel_query, gate_env, error_text, query, returned, returned_ids, seed_channel_with_owner, seed_membership, signed, stored_event, submit, tag,
};

/// **9005 (NIP-29 delete-event) is refused, and NIP-09 kind 5 really does erase.**
///
/// The refusal names kind 5, so the gate proves kind 5 works rather than trusting
/// the sentence: a refusal pointing at a second broken path would be no better
/// than the silence it replaced.
#[test]
 fn severe_nip29_delete_event_is_refused_and_nip09_deletion_works() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("inert-ana");
    seed_channel_with_owner(&env, "general", Visibility::Open, &ana).await;

    let message = signed(
        &ana,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", "general")],
        "this one goes away",
    );
    assert_eq!(submit(&env, &ana, &message).await.status, 200);
    assert!(stored_event(&env, &message.id).await.is_some());

    // The inert verb: refused, and nothing stored for it.
    let inert = signed(
        &ana,
        kind::KIND_NIP29_DELETE_EVENT,
        vec![
            tag("h", "general"),
            tag("e", &buzz::relay::crypto::to_hex(&message.id)),
        ],
        "",
    );
    let refused = submit(&env, &ana, &inert).await;
    assert_eq!(refused.status, 400, "{}", error_text(&refused));
    assert!(
        error_text(&refused).contains("9005"),
        "the refusal should name the kind: {}",
        error_text(&refused)
    );
    assert!(
        stored_event(&env, &inert.id).await.is_none(),
        "a refused verb must not be stored, or the silent path stays reachable"
    );
    assert!(
        stored_event(&env, &message.id).await.is_some(),
        "and the target is of course untouched"
    );

    // The alternative the refusal names actually erases.
    let deletion = signed(
        &ana,
        kind::KIND_DELETION,
        vec![tag("e", &buzz::relay::crypto::to_hex(&message.id))],
        "",
    );
    assert_eq!(submit(&env, &ana, &deletion).await.status, 200);
    assert!(
        stored_event(&env, &message.id).await.is_none(),
        "NIP-09 kind 5 is the working path, and it must actually remove the event"
    );

    });
}

/// **9008 (NIP-29 delete-group) is owner/admin-only and tombstones the room.**
#[test]
 fn severe_nip29_delete_group_tombstones_and_hides_the_room() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("inert-ana");
    let member = Actor::new("inert-member");
    seed_channel_with_owner(&env, "doomed", Visibility::Open, &ana).await;
    seed_membership(
        &env,
        "doomed",
        &member,
        buzz::relay::channel::Role::Member,
    )
    .await;

    let unauthorized = signed(
        &member,
        kind::KIND_NIP29_DELETE_GROUP,
        vec![tag("h", "doomed")],
        "",
    );
    let refused = submit(&env, &member, &unauthorized).await;
    assert_eq!(refused.status, 403, "{}", error_text(&refused));
    assert!(stored_event(&env, &unauthorized.id).await.is_none());

    let message = signed(
        &ana,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", "doomed")],
        "visible before deletion",
    );
    assert_eq!(submit(&env, &ana, &message).await.status, 200);

    // Deletion is intentionally allowed while archived: an owner must not have
    // to reopen a closed room merely to remove it.
    let archive = signed(
        &ana,
        kind::KIND_NIP29_EDIT_METADATA,
        vec![tag("h", "doomed"), tag("archived", "true")],
        "",
    );
    assert_eq!(submit(&env, &ana, &archive).await.status, 200);

    let deletion = signed(
        &ana,
        kind::KIND_NIP29_DELETE_GROUP,
        vec![tag("h", "doomed"), tag("d", "doomed")],
        "",
    );
    assert_eq!(submit(&env, &ana, &deletion).await.status, 200);
    assert!(stored_event(&env, &deletion.id).await.is_some());
    let raw = channel_doc(&env, "doomed").await.expect("channel retained");
    let channel: buzz::relay::channel::Channel =
        ferrite_app::serde_json::from_slice(&raw).expect("channel decodes");
    assert!(channel.deleted_at.is_some(), "the durable tombstone landed");

    let after = signed(
        &ana,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", "doomed")],
        "should not land",
    );
    let closed = submit(&env, &ana, &after).await;
    assert_eq!(
        closed.status,
        403,
        "deletion must refuse subsequent writes: {}",
        error_text(&closed)
    );
    let hidden = query(&env, &ana, &channel_query("doomed")).await;
    assert!(
        !returned(&hidden, &message),
        "a deleted room's retained history must not be readable"
    );

    });
}

/// **9009 (NIP-29 create-invite) is refused**, because invites here are HMAC
/// codes over the HTTP face and a 9009 was a parallel path to nothing.
#[test]
 fn severe_nip29_create_invite_is_refused() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("inert-ana");
    seed_channel_with_owner(&env, "general", Visibility::Open, &ana).await;

    let inert = signed(
        &ana,
        kind::KIND_NIP29_CREATE_INVITE,
        vec![tag("h", "general")],
        "",
    );
    let refused = submit(&env, &ana, &inert).await;
    assert_eq!(refused.status, 400, "{}", error_text(&refused));
    assert!(
        error_text(&refused).contains("/buzz/api/invites"),
        "the refusal must name the route that works: {}",
        error_text(&refused)
    );
    assert!(stored_event(&env, &inert.id).await.is_none());

    });
}

/// **48104 (huddle liveness) is refused from a client and stores nothing.**
///
/// Sync 002: upstream synthesizes this kind on REQ from its audio-room state and
/// signs it as the relay — its whole authority is the relay's signature. This
/// port has no audio rooms to attest and mints none, so the only thing a
/// client-submitted 48104 could ever be is a forged "this call is live" that
/// every synced client's huddle presence runtime would act on. Before the kind
/// was registered it was accepted as an ordinary channel event.
#[test]
 fn severe_huddle_liveness_cannot_be_forged_by_a_client() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("inert-ana");
    seed_channel_with_owner(&env, "standup", Visibility::Open, &ana).await;

    // The participant-signed half of the family still lands: a huddle START is
    // a member's own act, and refusing it would make huddles unusable rather
    // than safe.
    let started = signed(
        &ana,
        kind::KIND_HUDDLE_STARTED,
        vec![
            tag("h", "standup"),
            tag("d", "11111111-2222-4333-8444-555555555555"),
        ],
        "",
    );
    let ok = submit(&env, &ana, &started).await;
    assert_eq!(ok.status, 200, "{}", error_text(&ok));

    let forged = signed(
        &ana,
        kind::KIND_HUDDLE_LIVENESS,
        vec![
            tag("d", "11111111-2222-4333-8444-555555555555"),
            tag("h", "standup"),
        ],
        r#"{"ephemeral_channel_id":"11111111-2222-4333-8444-555555555555","generation":"1"}"#,
    );
    let refused = submit(&env, &ana, &forged).await;
    assert_eq!(refused.status, 403, "{}", error_text(&refused));
    assert!(
        error_text(&refused).contains("48104"),
        "the refusal should name the kind: {}",
        error_text(&refused)
    );
    assert!(
        stored_event(&env, &forged.id).await.is_none(),
        "a refused liveness record must not be stored, or a query would still serve it"
    );

    // And a REQ for liveness answers nothing rather than something invented:
    // nothing here mints 48104, so the truthful answer is the empty one.
    let page = query(
        &env,
        &ana,
        &ferrite_app::serde_json::json!({
            "kinds": [kind::KIND_HUDDLE_LIVENESS],
            "#h": ["standup"]
        }),
    )
    .await;
    assert_eq!(page.status, 200, "{}", error_text(&page));
    assert_eq!(returned_ids(&page), Vec::<String>::new());

    });
}

