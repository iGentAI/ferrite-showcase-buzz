//! Fix gate: server-assigned channel ids.
//!
//! `POST /buzz/api/channels` used to hard-require a client-supplied id, and
//! buzz-web slugs every user's private "Welcome" to the same `welcome` — which
//! the first user owns forever, so every later login ate a 409 before the
//! client's random-suffix retry papered over it. The fix makes `id` optional:
//! the relay assigns the same slug the client would have, and retries a
//! collision away with a caller-salted suffix. A CLIENT-supplied id keeps the
//! exact 409 semantics, because the starter rooms' create-or-join race reads
//! that conflict as "someone else won, join instead".

use super::common;

use buzz::relay::channel::Role;
use buzz::relay::{keys, store as buzz_store};
use common::{
    Actor, COMMUNITY, body_json, channel_doc, channel_owners, gate_env, error_text, membership, post,
    returned, seed_membership,
};
use ferrite_app::serde_json::{json, Value};

/// **A creator who lost the room's ownership does not get it back by creating
/// the same room again.**
#[test]
 fn recreating_a_room_does_not_restore_a_demoted_creators_ownership() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("ana-demoted-creator");
    let first = env
        .handle(create_request(&ana, &json!({ "name": "Alpha Room" })))
        .await
        .expect("the app replied");
    assert_eq!(first.status, 201, "{}", error_text(&first));
    seed_membership(&env, "alpha-room", &ana, Role::Member).await;

    let again = env
        .handle(create_request(&ana, &json!({ "name": "Alpha Room" })))
        .await
        .expect("the app replied");
    assert_eq!(again.status, 201, "{}", error_text(&again));
    let row = membership(&env, "alpha-room", &ana).await.expect("row");
    assert_eq!(row.role, Role::Member, "a demoted creator stays demoted");

    });
}

/// **A create that committed the room but not the creator's membership is
/// finished by the retry, and a complete room still answers 409.**
#[test]
 fn a_torn_channel_create_is_finished_by_its_retry() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("ana-torn-create");
    let body = json!({ "id": "beta-room", "name": "Beta Room" });
    let first = env.handle(create_request(&ana, &body)).await.expect("replied");
    assert_eq!(first.status, 201, "{}", error_text(&first));

    let mut events = env.store("events");
    buzz_store::delete_key(
        &mut events,
        &keys::mem(COMMUNITY, "beta-room", &ana.pk).expect("safe id"),
    )
    .await
    .expect("membership removed to simulate the torn create");

    let retry = env.handle(create_request(&ana, &body)).await.expect("replied");
    assert_eq!(retry.status, 201, "{}", error_text(&retry));
    let row = membership(&env, "beta-room", &ana).await.expect("row restored");
    assert_eq!(row.role, Role::Owner);

    let complete = env.handle(create_request(&ana, &body)).await.expect("replied");
    assert_eq!(complete.status, 409, "{}", error_text(&complete));

    });
}

/// A create request whose body may omit the id.
fn create_request(actor: &Actor, body: &Value) -> ferrite_app::IngressRequest {
    post(
        actor,
        "/buzz/api/channels",
        ferrite_app::serde_json::to_vec(body).expect("body encodes"),
    )
}

#[test]
 fn a_create_without_an_id_assigns_the_slug_and_returns_it() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("ana-assigned-id");

    let reply = env
        .handle(create_request(&ana, &json!({ "name": "Growth Team" })))
        .await
        .expect("the app replied");
    assert_eq!(
        reply.status,
        201,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let body = body_json(&reply);
    assert_eq!(
        body["channel"]["id"].as_str(),
        Some("growth-team"),
        "the assigned id is the client's own slug rule"
    );

    // The room is real: its guard exists under the returned id and the creator
    // owns it.
    assert!(channel_doc(&env, "growth-team").await.is_some());
    let owners = channel_owners(&env, "growth-team").await;
    assert!(owners.contains(&ana.pk_hex()));

    });
}

#[test]
 fn create_persists_description_and_temporary_channel_deadline() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("ana-channel-metadata");

    let invalid = env
        .handle(create_request(
            &ana,
            &json!({ "name": "Invalid temporary", "ttl_seconds": 0 }),
        ))
        .await
        .expect("the app replied");
    assert_eq!(invalid.status, 400);
    assert!(channel_doc(&env, "invalid-temporary").await.is_none());

    let reply = env
        .handle(create_request(
            &ana,
            &json!({
                "name": "Browser acceptance",
                "description": "Persist this description",
                "ttl_seconds": "1800"
            }),
        ))
        .await
        .expect("the app replied");
    assert_eq!(reply.status, 201);

    let body = body_json(&reply);
    assert_eq!(
        body["channel"]["description"].as_str(),
        Some("Persist this description")
    );
    assert_eq!(body["channel"]["ttl_seconds"].as_u64(), Some(1800));

    let stored_bytes = channel_doc(&env, "browser-acceptance")
        .await
        .expect("the channel exists");
    let stored: buzz::relay::channel::Channel =
        ferrite_app::serde_json::from_slice(&stored_bytes).expect("channel decodes");
    assert_eq!(
        stored.description.as_deref(),
        Some("Persist this description")
    );
    assert_eq!(stored.ttl_seconds, Some(1800));
    assert_eq!(
        stored.ttl_deadline,
        Some(stored.created_at.saturating_add(1_800_000))
    );

    });
}

#[test]
 fn two_same_name_creates_by_different_callers_yield_two_rooms() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("ana-welcome");
    let bo = Actor::new("bo-welcome");

    // The exact live failure: both users' private rooms are named "Welcome".
    let first = env
        .handle(create_request(
            &ana,
            &json!({ "name": "Welcome", "visibility": "private" }),
        ))
        .await
        .expect("the app replied");
    assert_eq!(first.status, 201);
    let first_id = body_json(&first)["channel"]["id"]
        .as_str()
        .expect("an id")
        .to_string();
    assert_eq!(first_id, "welcome", "the first caller gets the plain slug");

    let second = env
        .handle(create_request(
            &bo,
            &json!({ "name": "Welcome", "visibility": "private" }),
        ))
        .await
        .expect("the app replied");
    assert_eq!(
        second.status,
        201,
        "the collision is retried away, never a 409: {}",
        String::from_utf8_lossy(&second.body)
    );
    let second_id = body_json(&second)["channel"]["id"]
        .as_str()
        .expect("an id")
        .to_string();
    assert_ne!(second_id, first_id);
    assert!(
        second_id.starts_with("welcome-"),
        "the salted id keeps the recognizable stem: {second_id}"
    );

    // Two rooms, each owned by its own creator.
    assert!(channel_owners(&env, &first_id)
        .await
        .contains(&ana.pk_hex()));
    assert!(channel_owners(&env, &second_id)
        .await
        .contains(&bo.pk_hex()));

    });
}

#[test]
 fn a_client_supplied_id_keeps_the_conflict_semantics() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("ana-starter");
    let bo = Actor::new("bo-starter");

    let first = env
        .handle(create_request(
            &ana,
            &json!({ "id": "general", "name": "General" }),
        ))
        .await
        .expect("the app replied");
    assert_eq!(first.status, 201);

    // The starter-room race depends on the loser SEEING this conflict, so it
    // joins instead of creating — a silent retry here would break that.
    let second = env
        .handle(create_request(
            &bo,
            &json!({ "id": "general", "name": "General" }),
        ))
        .await
        .expect("the app replied");
    assert_eq!(second.status, 409);
    assert!(error_text(&second).contains("already exists"));

    // An unusable explicit id is still a clean 400, nothing half-created.
    let bad = env
        .handle(create_request(&ana, &json!({ "id": "a/b", "name": "Bad" })))
        .await
        .expect("the app replied");
    assert_eq!(bad.status, 400);
    assert!(channel_doc(&env, "general").await.is_some());

    // And a name is still required when the id is omitted.
    let nameless = env
        .handle(create_request(&ana, &json!({ "visibility": "open" })))
        .await
        .expect("the app replied");
    assert_eq!(nameless.status, 400);

    });
}

#[test]
 fn an_unsluggable_name_falls_back_to_a_room_id() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("ana-emoji");

    let reply = env
        .handle(create_request(&ana, &json!({ "name": "🎉🎉🎉" })))
        .await
        .expect("the app replied");
    assert_eq!(
        reply.status,
        201,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let id = body_json(&reply)["channel"]["id"]
        .as_str()
        .expect("an id")
        .to_string();
    assert!(id.starts_with("room-"), "{id}");
    assert!(channel_doc(&env, &id).await.is_some());

    });
}
