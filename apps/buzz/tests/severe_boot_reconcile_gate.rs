//! **The boot reconcile (`ingress::reconcile_group_projections`), decided by a
//! real second boot.**
//!
//! A client's channel list IS the relay-signed kind-39000/39001/39002
//! projections, and nothing on the write path revisits a room that already
//! exists: a room created before the relay had a signing identity carries no
//! projection, is invisible to every client FOREVER, and `create_channel`
//! answers 409 to any attempt to re-create it — the client cannot self-heal.
//! The boot reconcile exists for exactly that room. It enumerates rooms from
//! the `mem/` rows (the channel document lives on the APE plane, which has no
//! scan; every room has at least its creator's row) and republishes every
//! room's three cells.
//!
//! Two claims, each on durable state a real face can read back:
//!
//! 1. a room whose projections are ABSENT becomes visible after a boot;
//! 2. a room whose projections are CURRENT costs a boot **no writes** — the
//!    watermark, the stored version, and the delivery feed are all untouched,
//!    so a restart of an N-instance deployment is not N republications of
//!    every room.
//!
//! The reconcile is driven the only way it ever runs: by BOOTING an app
//! instance (`env.run_app` runs the real `serve`, which spawns it), never by
//! calling the function.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::time::Duration;

use buzz::relay::channel::{Role, Visibility};
use buzz::relay::crypto::to_hex;
use buzz::relay::effects::ReplaceableHead;
use buzz::relay::event::Event;
use buzz::relay::{keys, kind, store as buzz_store};
use common::{
    Actor, COMMUNITY, FEED_GENERATION, REPLY_BUDGET, admission, channel_query, gate_env, feed_copies,
    feed_records, membership, post, query, returned_ids, seed_channel_with_owner, signed,
    stored_event, tag,
};
use ferrite_app::serde_json::{self, json};
use common::SsoTestEnv as TestEnv;

/// The config key an operator seeds the relay's own signing identity into
/// (`ingress::RELAY_IDENTITY_CONFIG`). A secret of the same name wins when one
/// is injected; the harness injects none, so this is the path under test.
const RELAY_IDENTITY_CONFIG: &[u8] = b"buzz-relay-identity";

/// The three cells every room must carry for a client to see it.
const ALL_CELLS: [u32; 3] = [
    kind::KIND_NIP29_GROUP_METADATA,
    kind::KIND_NIP29_GROUP_ADMINS,
    kind::KIND_NIP29_GROUP_MEMBERS,
];

/// The identity the operator seeds — an [`Actor`] so the pubkey the gate
/// expects is derived from the same secret the app is given.
fn relay() -> Actor {
    Actor::new("the-relay-itself")
}

/// Seed the relay identity the way an operator does: 64-hex in the config store.
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

/// Create a room through the REAL route — the only way a channel comes into
/// existence in this port.
async fn create_room(
    env: &TestEnv,
    owner: &Actor,
    id: &str,
    name: &str,
) -> ferrite_app::IngressReply {
    env.handle(post(
        owner,
        "/buzz/api/channels",
        json!({ "id": id, "name": name, "visibility": Visibility::Open.as_str() })
            .to_string()
            .into_bytes(),
    ))
    .await
    .expect("the app replied")
}

/// The watermark for one projection cell, read with the app's own key builder.
async fn projection_head(
    env: &TestEnv,
    kind_: u32,
    relay: &Actor,
    channel: &str,
) -> Option<ReplaceableHead> {
    let mut guards = env.ape("guards");
    let raw = guards
        .get(&keys::dtag_head(COMMUNITY, kind_, &relay.pk, channel))
        .await
        .expect("ape read")?;
    Some(serde_json::from_slice(&raw).expect("head decodes"))
}

/// The projection this relay currently publishes for `(kind, channel)`: the
/// watermark's winner, read back through its stored body. `None` means nothing
/// is published — which a client sees as "this room does not exist".
async fn projection(env: &TestEnv, kind_: u32, relay: &Actor, channel: &str) -> Option<Event> {
    let head = projection_head(env, kind_, relay, channel).await?;
    let id = buzz::relay::crypto::from_hex32(&head.id).expect("a 32-byte head id");
    stored_event(env, &id).await
}

/// Wait for a projection the reconcile MUST publish. The reconcile runs in a
/// spawned task on a freshly booted instance, so the gate polls durable state
/// under the same budget every reply gets.
async fn wait_for_projection(env: &TestEnv, kind_: u32, relay: &Actor, channel: &str) -> Event {
    let deadline = tokio::time::Instant::now() + REPLY_BUDGET;
    loop {
        if let Some(event) = projection(env, kind_, relay, channel).await {
            return event;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the boot reconcile never published kind:{kind_} for channel {channel}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The `(pubkey, role)` pairs a roster projection carries (39002 is
/// `["p", pk, relay_url, role]`, 39001 is `["p", pk, role]`; the role is the
/// LAST element either way — exactly what the client reads).
fn roster(event: &Event) -> Vec<(String, String)> {
    event
        .tags
        .iter()
        .filter(|t| t.first().map(String::as_str) == Some("p"))
        .filter_map(|t| {
            let pubkey = t.get(1)?.clone();
            let role = t.last().filter(|r| r.as_str() != pubkey).cloned();
            Some((pubkey, role.unwrap_or_default()))
        })
        .collect()
}

/// Every feed record that is a group projection for `channel` — what a boot
/// actually PUSHED at connected clients.
async fn projection_feed_count(env: &TestEnv, channel: &str) -> usize {
    feed_records(env, FEED_GENERATION)
        .await
        .iter()
        .filter(|r| {
            kind::is_group_projection(r.event.kind) && r.event.first_tag("h") == Some(channel)
        })
        .count()
}

/// Boot one more REAL app instance on the same fleet — the deployment restart
/// the reconcile exists for.
fn boot_instance(env: &TestEnv) {
    env.run_app(|ctx| async move { buzz::relay::ingress::serve(ctx).await });
}

// ------------------------------------------------------------------ the gates

/// **Claim 1: an invisible room becomes visible after a boot.** The room is
/// created through the real route while NO identity is configured (the exact
/// history the reconcile exists for), the operator seeds the identity, an
/// instance boots — and the room's three relay-signed cells exist, correct,
/// and readable through the query face.
#[test]
 fn severe_a_room_created_before_the_identity_becomes_visible_after_a_boot() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-orphaned";

    // Created for real, before any identity exists.
    let created = create_room(&env, &owner, channel, "orphaned").await;
    assert_eq!(
        created.status,
        201,
        "creation must not depend on the relay identity: {}",
        String::from_utf8_lossy(&created.body)
    );
    assert!(
        membership(&env, channel, &owner)
            .await
            .is_some_and(|m| m.is_live()),
        "the room really exists — its owner's membership row is durable"
    );

    // Invisible: not one cell is published, and a query answers a client with
    // nothing it could build a channel list from.
    let relay = relay();
    for kind_ in ALL_CELLS {
        assert!(
            projection_head(&env, kind_, &relay, channel)
                .await
                .is_none(),
            "no kind:{kind_} may exist before an identity is configured"
        );
    }
    let page = query(&env, &owner, &channel_query(channel)).await;
    assert_eq!(page.status, 200);
    assert!(
        returned_ids(&page).is_empty(),
        "the orphaned room must start out with nothing a client can see"
    );

    // And the client CANNOT self-heal: re-creating the room is refused with the
    // 409 that made this bug permanent before the reconcile existed.
    let retried = create_room(&env, &owner, channel, "orphaned").await;
    assert_eq!(
        retried.status,
        409,
        "re-creating an existing room must answer 409, which is why only the \
         relay can heal it: {}",
        String::from_utf8_lossy(&retried.body)
    );

    // The operator seeds the identity, and an instance boots.
    let relay = seed_relay_identity(&env).await;
    boot_instance(&env);

    // The room is now visible: all three cells, relay-signed and correct.
    let meta = wait_for_projection(&env, kind::KIND_NIP29_GROUP_METADATA, &relay, channel).await;
    assert_eq!(meta.pubkey, relay.pk, "the relay signs its own projections");
    assert_eq!(meta.verify(), Ok(()), "the projection must verify");
    assert_eq!(
        meta.first_tag("d"),
        Some(channel),
        "the `d` tag IS the id a client keys its channel list by: {:?}",
        meta.tags
    );
    assert_eq!(meta.first_tag("h"), Some(channel), "gated by its own room");
    assert_eq!(meta.first_tag("name"), Some("orphaned"), "{:?}", meta.tags);

    let members = wait_for_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, channel).await;
    assert_eq!(
        roster(&members),
        vec![(owner.pk_hex(), Role::Owner.as_str().to_string())],
        "the reconciled roster is derived from the live mem/ rows: {:?}",
        members.tags
    );
    let admins = wait_for_projection(&env, kind::KIND_NIP29_GROUP_ADMINS, &relay, channel).await;
    assert_eq!(
        roster(&admins),
        vec![(owner.pk_hex(), Role::Owner.as_str().to_string())]
    );

    // Through the ordinary saga — not written behind the ingest path's back —
    // and pushed at connected clients exactly once. Completion is the saga's
    // LAST step and the watermark becomes visible a few writes before it (that
    // ordering is load-bearing: a re-driven saga can only finish a torn one if
    // completion commits after every effect), so a freshly reconciled cell is
    // legitimately visible for a few milliseconds while its saga is still
    // closing. Completion is therefore POLLED under the same budget visibility
    // gets — the claim asserted is unchanged (the projection completes the
    // ordinary saga), without asserting an instantaneity the saga never
    // promised. The feed check runs after the poll, when the count is final.
    for event in [&meta, &members, &admins] {
        let deadline = tokio::time::Instant::now() + REPLY_BUDGET;
        loop {
            let saga = admission(&env, &event.id)
                .await
                .unwrap_or_else(|| panic!("kind:{} was not admitted through the saga", event.kind));
            if saga.is_complete() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "kind:{} saga must complete",
                event.kind
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            feed_copies(&env, FEED_GENERATION, &event.id).await,
            1,
            "kind:{} must reach the delivery feed exactly once",
            event.kind
        );
    }

    // And the READ FACE agrees: a client's query of the room now returns the
    // cells it builds its workspace from.
    let page = query(&env, &owner, &channel_query(channel)).await;
    assert_eq!(page.status, 200);
    let ids = returned_ids(&page);
    for event in [&meta, &members, &admins] {
        assert!(
            ids.contains(&to_hex(&event.id)),
            "kind:{} must be queryable after the reconcile: {ids:?}",
            event.kind
        );
    }

    });
}

/// **Claim 2: a converged room costs a boot nothing.** One room is fully
/// published (the create route did it); one room is missing its cells (the
/// sentinel). A boot must publish the sentinel's cells — proving the reconcile
/// RAN — while the converged room's watermark, stored version, and feed
/// records stay byte-for-byte what they were.
///
/// The sentinel is named to scan AFTER the converged room (`mem/` rows are
/// enumerated in key order), so once its cells exist the reconcile has already
/// visited — and must have skipped — the converged one.
#[test]
 fn severe_a_boot_republishes_nothing_for_a_room_whose_projections_are_current() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let relay = seed_relay_identity(&env).await;
    let owner = Actor::new("owner");
    let converged = "chan-a-converged"; // scans first
    let sentinel = "chan-z-sentinel"; // scans last: the "reconcile ran" marker

    assert_eq!(
        create_room(&env, &owner, converged, "settled").await.status,
        201
    );
    let mut before: Vec<(u32, Event, ReplaceableHead)> = Vec::new();
    for kind_ in ALL_CELLS {
        let head = projection_head(&env, kind_, &relay, converged)
            .await
            .unwrap_or_else(|| panic!("creation published kind:{kind_}"));
        let event = projection(&env, kind_, &relay, converged)
            .await
            .unwrap_or_else(|| panic!("kind:{kind_} body present"));
        before.push((kind_, event, head));
    }
    let feed_before = projection_feed_count(&env, converged).await;
    assert_eq!(feed_before, 3, "creation fed exactly the three cells");

    // The sentinel: a room with the state a pre-identity creation leaves
    // behind (channel document + owner membership, NO projections).
    seed_channel_with_owner(&env, sentinel, Visibility::Open, &owner).await;
    for kind_ in ALL_CELLS {
        assert!(projection_head(&env, kind_, &relay, sentinel)
            .await
            .is_none());
    }

    boot_instance(&env);

    // The reconcile ran to (and past) the sentinel: its cells now exist.
    for kind_ in ALL_CELLS {
        let event = wait_for_projection(&env, kind_, &relay, sentinel).await;
        assert_eq!(event.pubkey, relay.pk);
        assert_eq!(event.first_tag("d"), Some(sentinel));
    }

    // And the converged room was left ALONE: same winning version (not a
    // republication that merely carried the same tags), same stored body, no
    // new feed push, no superseded-and-swept churn.
    for (kind_, event, head) in &before {
        let now = projection_head(&env, *kind_, &relay, converged)
            .await
            .unwrap_or_else(|| panic!("kind:{kind_} must still be published"));
        assert_eq!(
            now.id, head.id,
            "a current kind:{kind_} cell was republished by the boot"
        );
        assert_eq!(
            now.created_at, head.created_at,
            "a no-op reconcile must not advance kind:{kind_}'s watermark"
        );
        assert_eq!(
            stored_event(&env, &event.id).await.as_ref(),
            Some(event),
            "the pre-boot kind:{kind_} version must survive the boot untouched"
        );
        assert_eq!(
            feed_copies(&env, FEED_GENERATION, &event.id).await,
            1,
            "kind:{kind_} must not be re-pushed at connected clients"
        );
    }
    assert_eq!(
        projection_feed_count(&env, converged).await,
        feed_before,
        "a converged room must cost the boot zero feed writes"
    );

    });
}

/// **Claim 3: a published projection whose index rows are gone is republished
/// at boot.** A body with no kind index row is invisible to every filter read
/// while its watermark still names it, so "the stored version is current" must
/// also mean "the stored version is readable". Measured on staging 2026-09-12:
/// `general` served no 39000 and a redeploy did not restore it.
#[test]
 fn severe_a_boot_republishes_a_projection_that_lost_its_index_rows() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let relay = seed_relay_identity(&env).await;
    let owner = Actor::new("owner");
    let channel = "general";
    let created = create_room(&env, &owner, channel, "general").await;
    assert_eq!(created.status, 201, "{}", String::from_utf8_lossy(&created.body));
    let before = wait_for_projection(&env, kind::KIND_NIP29_GROUP_METADATA, &relay, channel).await;

    // Remove the published version's index rows, leaving body and watermark.
    let mut events = env.store("events");
    for key in buzz::relay::ingest::index_keys_for(COMMUNITY, &before, Some(channel)) {
        buzz_store::delete_key(&mut events, &key).await.expect("index row removed");
    }

    boot_instance(&env);
    let deadline = tokio::time::Instant::now() + REPLY_BUDGET;
    loop {
        let now = projection(&env, kind::KIND_NIP29_GROUP_METADATA, &relay, channel)
            .await
            .expect("a projection is published");
        if now.id != before.id {
            assert!(now.created_at > before.created_at, "the new version supersedes the unreadable one");
            let mut events = env.store("events");
            let indexed = buzz_store::get_raw(
                &mut events,
                &keys::kind_index(COMMUNITY, now.kind, now.created_at, &now.id),
            )
            .await
            .expect("index read");
            assert!(indexed.is_some(), "the republished version is readable");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the boot never republished the unreadable projection");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    });
}
