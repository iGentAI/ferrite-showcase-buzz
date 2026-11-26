//! Deleting what an absent creator left behind.
//!
//! Rooms and projects are created by identities that can disappear — a departed
//! colleague, or a throwaway identity a journey minted and never kept. Two
//! authorities close that gap, and each is gated here in both directions:
//!
//! * a room (9008) may be deleted by its owner/admin, a community moderator, or
//!   an operator — never by an ordinary member or an outsider;
//! * a project (30621) may be retired through `POST /buzz/api/projects/delete`
//!   by its creator, a community moderator, or an operator — never by anyone
//!   else, and never with a header that does not commit to the body.

use super::common;

use std::sync::atomic::{AtomicU64, Ordering};

use buzz::relay::channel::{Role, Visibility};
use buzz::relay::crypto::sha256::sha256;
use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::{keys, kind, operator, store as buzz_store};
use common::SsoTestEnv as TestEnv;
use common::{
    Actor, COMMUNITY, T0_SECS, auth_header, channel_doc, gate_env, error_text, post, query, request_with_auth,
    seed_channel_with_owner, seed_membership, signed, stored_event, submit, tag,
};
use ferrite_app::serde_json::{self, json};
use ferrite_app::IngressRequest;

const MODERATORS_CONFIG: &[u8] = b"buzz-community-moderators";
const DELETE_PROJECT: &str = "/buzz/api/projects/delete";

static NONCE: AtomicU64 = AtomicU64::new(0);

fn nonce() -> String {
    format!("del{}", NONCE.fetch_add(1, Ordering::Relaxed))
}

/// A POST whose NIP-98 header also commits to the body (`payload` tag).
fn committed_post(actor: &Actor, path: &str, body: Vec<u8>) -> IngressRequest {
    let event = Event::sign_new(
        &actor.sk,
        kind::KIND_HTTP_AUTH,
        vec![
            vec!["u".into(), format!("https://buzz.test{path}")],
            vec!["method".into(), "POST".into()],
            vec!["payload".into(), to_hex(&sha256(&body))],
            vec!["nonce".into(), nonce()],
        ],
        "",
        T0_SECS,
    )
    .expect("authorization events sign");
    request_with_auth("POST", path, Some(&auth_header(&event)), body)
}

async fn handle(env: &TestEnv, request: IngressRequest) -> ferrite_app::IngressReply {
    env.handle(request).await.expect("the app answers")
}

async fn seed_moderators(env: &TestEnv, who: &[&Actor]) {
    let list: Vec<String> = who.iter().map(|a| a.pk_hex()).collect();
    let mut config = env.store("config");
    buzz_store::put_raw(
        &mut config,
        MODERATORS_CONFIG,
        &serde_json::to_vec(&list).expect("encodes"),
    )
    .await
    .expect("moderator seed");
}

async fn seed_operator(env: &TestEnv, who: &Actor) {
    let mut config = env.store("config");
    let set = serde_json::to_vec(&json!([who.pk_hex()])).expect("set encodes");
    buzz_store::put_raw(&mut config, operator::OPERATOR_SET_CONFIG, &set)
        .await
        .expect("operator seed");
}

async fn is_deleted(env: &TestEnv, id: &str) -> bool {
    let raw = channel_doc(env, id).await.expect("channel retained");
    let channel: buzz::relay::channel::Channel =
        serde_json::from_slice(&raw).expect("channel decodes");
    channel.deleted_at.is_some()
}

/// **An ordinary member cannot delete a room; a community moderator and an
/// operator can, without being members of it.**
#[test]
 fn severe_room_delete_admits_owner_moderator_and_operator_only() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("del-owner");
    let member = Actor::new("del-member");
    let moderator = Actor::new("del-moderator");
    let op = Actor::new("del-operator");
    seed_moderators(&env, &[&moderator]).await;
    seed_operator(&env, &op).await;

    for room in ["left-a", "left-b", "left-c"] {
        seed_channel_with_owner(&env, room, Visibility::Open, &owner).await;
        seed_membership(&env, room, &member, Role::Member).await;
    }

    // A plain member: refused, nothing stored, the room stays.
    let refused = signed(&member, kind::KIND_NIP29_DELETE_GROUP, vec![tag("h", "left-a")], "");
    let reply = submit(&env, &member, &refused).await;
    assert_eq!(reply.status, 403, "{}", error_text(&reply));
    assert!(stored_event(&env, &refused.id).await.is_none());
    assert!(!is_deleted(&env, "left-a").await);

    // An outsider who is neither a member nor a moderator: refused too.
    let stranger = Actor::new("del-stranger");
    let refused = signed(&stranger, kind::KIND_NIP29_DELETE_GROUP, vec![tag("h", "left-a")], "");
    let reply = submit(&env, &stranger, &refused).await;
    assert_eq!(reply.status, 403, "{}", error_text(&reply));
    assert!(!is_deleted(&env, "left-a").await);

    // The owner still may.
    let by_owner = signed(&owner, kind::KIND_NIP29_DELETE_GROUP, vec![tag("h", "left-a")], "");
    let reply = submit(&env, &owner, &by_owner).await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    assert!(is_deleted(&env, "left-a").await);

    // A community moderator who never joined the room.
    let by_moderator =
        signed(&moderator, kind::KIND_NIP29_DELETE_GROUP, vec![tag("h", "left-b")], "");
    let reply = submit(&env, &moderator, &by_moderator).await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    assert!(is_deleted(&env, "left-b").await);

    // An operator who never joined the room.
    let by_operator = signed(&op, kind::KIND_NIP29_DELETE_GROUP, vec![tag("h", "left-c")], "");
    let reply = submit(&env, &op, &by_operator).await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    assert!(is_deleted(&env, "left-c").await);

    // A deleted room fails closed for every authority.
    let again = signed(&op, kind::KIND_NIP29_DELETE_GROUP, vec![tag("h", "left-c"), tag("nonce", "2")], "");
    let reply = submit(&env, &op, &again).await;
    assert_eq!(reply.status, 403, "{}", error_text(&reply));

    });
}

/// **Removing a member deletes the community row AND writes the revocation
/// tombstone a live reader's pump watches, and only a moderator may do it.**
#[test]
 fn severe_members_remove_deletes_the_row_and_writes_the_tombstone() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("rm-moderator");
    let ordinary = Actor::new("rm-ordinary");
    let leaver = Actor::new("rm-leaver");
    seed_moderators(&env, &[&moderator]).await;
    let mut events = env.store("events");
    buzz_store::put_raw(&mut events, &keys::community_mem(COMMUNITY, &leaver.pk), b"{}")
        .await
        .expect("membership seed");
    let body = serde_json::to_vec(&json!({ "pubkey": leaver.pk_hex() })).expect("encodes");

    let refused = handle(
        &env,
        committed_post(&ordinary, "/buzz/api/members/remove", body.clone()),
    )
    .await;
    assert_eq!(refused.status, 403, "{}", error_text(&refused));
    assert!(events.get(&keys::community_mem(COMMUNITY, &leaver.pk)).await.expect("read").is_some());

    let reply = handle(
        &env,
        committed_post(&moderator, "/buzz/api/members/remove", body),
    )
    .await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    assert!(events.get(&keys::community_mem(COMMUNITY, &leaver.pk)).await.expect("read").is_none());
    assert!(
        events.get(&keys::revoked(COMMUNITY, &leaver.pk)).await.expect("read").is_some(),
        "the removal must leave the tombstone a live reader's pump sees"
    );

    });
}

fn project(author: &Actor, d: &str) -> Event {
    signed(author, kind::KIND_PROJECT, vec![tag("d", d), tag("name", d)], "")
}

fn coordinate(author: &Actor, d: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "coordinate": format!("{}:{}:{d}", kind::KIND_PROJECT, author.pk_hex()) }))
        .expect("body encodes")
}

/// **A project is retired by its creator, a moderator or an operator — and by
/// nobody else.**
#[test]
 fn severe_project_delete_admits_creator_moderator_and_operator_only() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let creator = Actor::new("proj-creator");
    let other = Actor::new("proj-other");
    let moderator = Actor::new("proj-moderator");
    let op = Actor::new("proj-operator");
    seed_channel_with_owner(&env, "general", Visibility::Open, &creator).await;
    seed_moderators(&env, &[&moderator]).await;
    seed_operator(&env, &op).await;

    let mine = project(&creator, "mine");
    let modded = project(&creator, "modded");
    let operated = project(&creator, "operated");
    for event in [&mine, &modded, &operated] {
        let reply = submit(&env, &creator, event).await;
        assert_eq!(reply.status, 200, "{}", error_text(&reply));
        assert!(stored_event(&env, &event.id).await.is_some());
    }

    // Someone else: refused, and the project stays.
    let reply = handle(&env, committed_post(&other, DELETE_PROJECT, coordinate(&creator, "mine"))).await;
    assert_eq!(reply.status, 403, "{}", error_text(&reply));
    assert!(stored_event(&env, &mine.id).await.is_some());

    // A header that does not commit to the body: refused even for the creator.
    let reply = handle(&env, post(&creator, DELETE_PROJECT, coordinate(&creator, "mine"))).await;
    assert_ne!(reply.status, 200, "an uncommitted body must be refused");
    assert!(stored_event(&env, &mine.id).await.is_some());

    // Not a project coordinate: refused by name.
    let wrong_kind = serde_json::to_vec(&json!({ "coordinate": format!("30023:{}:mine", creator.pk_hex()) }))
        .expect("encodes");
    let reply = handle(&env, committed_post(&op, DELETE_PROJECT, wrong_kind)).await;
    assert_eq!(reply.status, 400, "{}", error_text(&reply));

    for (actor, d, event) in [(&creator, "mine", &mine), (&moderator, "modded", &modded), (&op, "operated", &operated)] {
        let reply = handle(&env, committed_post(actor, DELETE_PROJECT, coordinate(&creator, d))).await;
        assert_eq!(reply.status, 200, "{d}: {}", error_text(&reply));
        assert!(
            stored_event(&env, &event.id).await.is_none(),
            "{d}: the retired project must be erased"
        );
    }

    });
}

const RELAY_IDENTITY_CONFIG: &[u8] = b"buzz-relay-identity";

/// The rooms a member can list, as `POST /buzz/api/filter-query {kinds:[39000]}`
/// answers them: the `d` of every metadata projection served.
async fn listed_rooms(env: &TestEnv, reader: &Actor) -> Vec<String> {
    let body = serde_json::to_vec(&json!([{ "kinds": [kind::KIND_NIP29_GROUP_METADATA], "limit": 500 }]))
        .expect("encodes");
    let reply = handle(env, committed_post(reader, "/buzz/api/filter-query", body)).await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let events: Vec<Event> = serde_json::from_slice(&reply.body).expect("an event array");
    events
        .iter()
        .filter_map(|event| event.first_tag("d").map(str::to_string))
        .collect()
}

/// **Deleting rooms leaves every other room listed.** Measured on staging
/// 2026-09-12 12:03Z: after an operator deleted 54 rooms, a 39000 read served
/// no room at all, `general` included.
#[test]
 fn severe_deleting_rooms_keeps_the_other_rooms_listed() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let relay = Actor::new("cleanup-relay");
    let mut config = env.store("config");
    buzz_store::put_raw(&mut config, RELAY_IDENTITY_CONFIG, to_hex(&relay.sk).as_bytes())
        .await
        .expect("relay identity seed");
    let owner = Actor::new("cleanup-owner");
    let op = Actor::new("cleanup-operator");
    seed_operator(&env, &op).await;

    for (id, name) in [("general", "general"), ("doomed-1", "doomed 1"), ("doomed-2", "doomed 2")] {
        let reply = handle(
            &env,
            post(
                &owner,
                "/buzz/api/channels",
                json!({ "id": id, "name": name, "visibility": "open" }).to_string().into_bytes(),
            ),
        )
        .await;
        assert_eq!(reply.status, 201, "{id}: {}", error_text(&reply));
    }
    let before = listed_rooms(&env, &owner).await;
    assert!(before.contains(&"general".to_string()), "general listed before: {before:?}");

    for id in ["doomed-1", "doomed-2"] {
        let deletion = signed(&op, kind::KIND_NIP29_DELETE_GROUP, vec![tag("h", id)], "");
        let reply = submit(&env, &op, &deletion).await;
        assert_eq!(reply.status, 200, "{id}: {}", error_text(&reply));
    }
    let after = listed_rooms(&env, &owner).await;
    assert!(after.contains(&"general".to_string()), "general must stay listed: {after:?}");
    assert!(!after.iter().any(|room| room.starts_with("doomed")), "deleted rooms are hidden: {after:?}");

    });
}

/// **A filter read walks past newest candidates the reader may not see.**
///
/// Candidates come from an index before the access gate runs. With more
/// unreadable rooms newer than a readable one than one candidate page holds, the
/// read answered `[]` and a client walking by `until` stopped: measured on
/// staging 2026-09-12, `{kinds:[39000]}` served no room at all.
#[test]
 fn severe_a_filter_read_serves_readable_events_below_unreadable_ones() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let relay = Actor::new("cleanup-relay-2");
    let mut config = env.store("config");
    buzz_store::put_raw(&mut config, RELAY_IDENTITY_CONFIG, to_hex(&relay.sk).as_bytes())
        .await
        .expect("relay identity seed");
    let owner = Actor::new("walk-owner");
    let reader = Actor::new("walk-reader");

    let reply = handle(
        &env,
        post(&owner, "/buzz/api/channels", json!({ "id": "general", "name": "general", "visibility": "open" }).to_string().into_bytes()),
    )
    .await;
    assert_eq!(reply.status, 201, "{}", error_text(&reply));
    env.advance(1_000);
    // Twelve private rooms, all newer than `general`: more than one candidate
    // page (2·limit = 10) for the read below, and none readable by `reader`.
    for index in 0..12 {
        let id = format!("private-{index:02}");
        let reply = handle(
            &env,
            post(&owner, "/buzz/api/channels", json!({ "id": id, "name": id, "visibility": "private" }).to_string().into_bytes()),
        )
        .await;
        assert_eq!(reply.status, 201, "{id}: {}", error_text(&reply));
        env.advance(1_000);
    }

    let body = serde_json::to_vec(&json!([{ "kinds": [kind::KIND_NIP29_GROUP_METADATA], "limit": 5 }])).expect("encodes");
    let reply = handle(&env, committed_post(&reader, "/buzz/api/filter-query", body)).await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let events: Vec<Event> = serde_json::from_slice(&reply.body).expect("an event array");
    let rooms: Vec<&str> = events.iter().filter_map(|event| event.first_tag("d")).collect();
    assert_eq!(rooms, vec!["general"], "the readable room below the unreadable page must be served");

    // And by address: a `#d` read of an old room is a point read, whatever
    // fills the newest candidate pages.
    let body = serde_json::to_vec(&json!([{ "kinds": [kind::KIND_NIP29_GROUP_METADATA], "#d": ["general"], "limit": 1 }]))
        .expect("encodes");
    let reply = handle(&env, committed_post(&reader, "/buzz/api/filter-query", body)).await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let events: Vec<Event> = serde_json::from_slice(&reply.body).expect("an event array");
    assert_eq!(events.len(), 1, "general by address: {events:?}");
    // A private room by address stays unreadable to a non-member.
    let body = serde_json::to_vec(&json!([{ "kinds": [kind::KIND_NIP29_GROUP_METADATA], "#d": ["private-03"], "limit": 1 }]))
        .expect("encodes");
    let reply = handle(&env, committed_post(&reader, "/buzz/api/filter-query", body)).await;
    let events: Vec<Event> = serde_json::from_slice(&reply.body).expect("an event array");
    assert!(events.is_empty(), "a private room's projection must not reach a non-member");

    });
}
