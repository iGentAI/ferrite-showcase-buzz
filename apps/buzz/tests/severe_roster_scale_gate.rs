//! **The 39002 roster at scale: complete past one scan page, and HONEST past
//! the event ceiling.**
//!
//! The relay-signed kind-39002 member list is the ONLY place a client's roster,
//! mention picker, and permission-adjacent UI come from — a pubkey missing from
//! it does not exist, as far as any client can tell. Two ways the projection
//! can lose people, both silent on the wire before this gate existed:
//!
//! * **The scan pages.** `mem/{c}/{chan}/` rows come back at most
//!   [`buzz_store::MAX_SCAN_PAGE`] per page; a projection built from the FIRST
//!   page silently drops whoever sorts past it. The live dev relay's `general`
//!   channel grows monotonically (579 members and counting), so this is a
//!   when, not an if.
//! * **The event has a hard ceiling.** One 39002 must serialize under
//!   [`MAX_SERIALIZED_EVENT_BYTES`] or [`Event::sign_new`] refuses to mint it
//!   at all — so past ~2.8k members SOME bound must exist
//!   ([`MAX_PROJECTION_MEMBERS`]). The property this gate demands is that the
//!   bound is visible ON THE WIRE: a `["truncated","<total live>"]` tag, so a
//!   client can tell "these are all the members" from "these are the first N
//!   of M". A log line on the relay tells the client nothing.
//!
//! Both cases drive the real app on a real fleet and read back the projection
//! a client would: the watermark cell, then the stored body it names.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::{Membership, Role, Visibility};
use buzz::relay::crypto::to_hex;
use buzz::relay::effects::{ReplaceableHead, MAX_PROJECTION_MEMBERS};
use buzz::relay::event::{Event, MAX_SERIALIZED_EVENT_BYTES};
use buzz::relay::{keys, kind, store as buzz_store};
use common::{
    Actor, COMMUNITY, T0_MS, gate_env, error_text, membership, post, query, signed, stored_event,
    submit, tag,
};
use ferrite_app::serde_json::{self, json};
use common::SsoTestEnv as TestEnv;

/// The config key an operator seeds the relay's own signing identity into
/// (`ingress::RELAY_IDENTITY_CONFIG`).
const RELAY_IDENTITY_CONFIG: &[u8] = b"buzz-relay-identity";

/// Seed the relay identity the way an operator does: 64-hex in the config store.
async fn seed_relay_identity(env: &TestEnv) -> Actor {
    let relay = Actor::new("the-relay-itself");
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

/// Create a room through the REAL route (`POST /buzz/api/channels`).
async fn create_room(env: &TestEnv, owner: &Actor, id: &str) -> ferrite_app::IngressReply {
    env.handle(post(
        owner,
        "/buzz/api/channels",
        json!({ "id": id, "name": id, "visibility": Visibility::Open.as_str() })
            .to_string()
            .into_bytes(),
    ))
    .await
    .expect("the app replied")
}

/// The projection this relay currently publishes for `(kind, channel)`: the
/// watermark's winner, read back through its stored body — exactly what a
/// client's query would surface.
async fn expect_projection(env: &TestEnv, kind_: u32, relay: &Actor, channel: &str) -> Event {
    let mut guards = env.ape("guards");
    let raw = guards
        .get(&keys::dtag_head(COMMUNITY, kind_, &relay.pk, channel))
        .await
        .expect("ape read")
        .unwrap_or_else(|| panic!("no kind:{kind_} watermark for channel {channel}"));
    let head: ReplaceableHead = serde_json::from_slice(&raw).expect("head decodes");
    let id = buzz::relay::crypto::from_hex32(&head.id).expect("a 32-byte head id");
    stored_event(env, &id)
        .await
        .unwrap_or_else(|| panic!("no kind:{kind_} projection body for channel {channel}"))
}

/// The pubkeys of a roster projection's `p` tags, in tag order.
fn roster_pubkeys(event: &Event) -> Vec<String> {
    event.tag_values("p").map(str::to_string).collect()
}

/// `h` + `p` + `role` tags for a membership command.
fn member_tags(channel: &str, target: &Actor, role: Role) -> Vec<Vec<String>> {
    vec![
        tag("h", channel),
        tag("p", &target.pk_hex()),
        tag("role", role.as_str()),
    ]
}

/// Seed `n` live member rows in one channel, batched to the platform bound —
/// row-at-a-time seeding at this scale is most of a test's wall clock.
///
/// The synthetic pubkeys are zero-prefixed with the index in the LAST bytes, so
/// their hex sorts FIRST in the `mem/{c}/{chan}/{pubkey}` key order — ahead of
/// any real actor's key. That makes "who falls past the cap" deterministic:
/// always the real actors, never the seeded block. Returns the hexes in that
/// same (already sorted) order.
async fn seed_many_members(env: &TestEnv, channel: &str, n: usize) -> Vec<String> {
    let mut entries = Vec::with_capacity(n);
    let mut hexes = Vec::with_capacity(n);
    for i in 0..n {
        let mut pk = [0u8; 32];
        pk[24..32].copy_from_slice(&(i as u64).to_be_bytes());
        let doc = Membership::new(&pk, Role::Member, T0_MS);
        let key = keys::mem(COMMUNITY, channel, &pk).expect("a safe channel id");
        entries.push((key, serde_json::to_vec(&doc).expect("membership encodes")));
        hexes.push(to_hex(&pk));
    }
    let mut events = env.store("events");
    buzz_store::put_many(&mut events, &entries)
        .await
        .expect("bulk membership seed");
    hexes
}

/// Seed one live ADMIN whose pubkey sorts LAST — past every scan page. The
/// admins projection (39001) must still find them.
async fn seed_last_sorting_admin(env: &TestEnv, channel: &str) -> String {
    let pk = [0xffu8; 32];
    let doc = Membership::new(&pk, Role::Admin, T0_MS);
    let key = keys::mem(COMMUNITY, channel, &pk).expect("a safe channel id");
    let mut events = env.store("events");
    buzz_store::put_doc(&mut events, &key, &doc)
        .await
        .expect("admin seed");
    to_hex(&pk)
}

// ------------------------------------------------------------------ the gates

/// **A roster larger than one scan page is published COMPLETELY.** 1,200 live
/// members is more than one store scan page: a projection that stops at the
/// first page silently loses the 170+ members who sort past it, with nothing
/// on the wire saying so.
#[test]
 fn severe_a_roster_larger_than_one_scan_page_is_published_completely() {
    common::run_scenario(|| async move {
    const SEEDED: usize = 1_200;

    let env = gate_env().await;
    let relay = seed_relay_identity(&env).await;
    let owner = Actor::new("owner");
    let newcomer = Actor::new("newcomer");
    let channel = "chan-page-spill";
    assert_eq!(create_room(&env, &owner, channel).await.status, 201);

    let seeded = seed_many_members(&env, channel, SEEDED).await;

    // Trigger a roster republish through the REAL route: the owner adds one
    // more member, which invalidates the 39002 cell.
    let add = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &newcomer, Role::Member),
        "",
    );
    let reply = submit(&env, &owner, &add).await;
    assert_eq!(reply.status, 200, "add accepted: {}", error_text(&reply));

    let projection = expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, channel).await;
    let published = roster_pubkeys(&projection);
    assert_eq!(
        published.len(),
        SEEDED + 2,
        "the roster must carry EVERY live member (seeded {SEEDED} + owner + newcomer), \
         not the first scan page"
    );
    for pk in &seeded {
        assert!(
            published.contains(pk),
            "seeded member {pk} fell off the published roster"
        );
    }
    assert!(published.contains(&owner.pk_hex()));
    assert!(published.contains(&newcomer.pk_hex()));
    // Complete ⇒ nothing on the wire may claim otherwise.
    assert_eq!(
        projection.first_tag("truncated"),
        None,
        "a complete roster must not carry a truncation marker"
    );
    // And it is still a well-formed relay-signed event.
    assert_eq!(projection.verify(), Ok(()), "the projection must verify");
    assert!(projection.serialized_len() <= MAX_SERIALIZED_EVENT_BYTES);

    });
}

/// **A roster past the event ceiling says so ON THE WIRE.** More live members
/// than [`MAX_PROJECTION_MEMBERS`] cannot fit one signed event — so the
/// projection must carry the first N in key order PLUS a
/// `["truncated","<total live>"]` tag a client can act on. A short-but-
/// complete-looking p-list is the bug this gate exists for.
#[test]
 fn severe_a_roster_past_the_event_ceiling_advertises_its_truncation() {
    common::run_scenario(|| async move {
    // Comfortably past the cap, whatever the cap is.
    let seeded_n = MAX_PROJECTION_MEMBERS + 86;

    let env = gate_env().await;
    let relay = seed_relay_identity(&env).await;
    let owner = Actor::new("owner");
    let newcomer = Actor::new("newcomer");
    let channel = "chan-overflow";
    assert_eq!(create_room(&env, &owner, channel).await.status, 201);

    let seeded = seed_many_members(&env, channel, seeded_n).await;
    let far_admin = seed_last_sorting_admin(&env, channel).await;
    // Every live member: the seeded block, the far-sorting admin, the owner,
    // and the newcomer the trigger adds.
    let total_live = seeded_n + 3;

    let add = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &newcomer, Role::Member),
        "",
    );
    let reply = submit(&env, &owner, &add).await;
    assert_eq!(reply.status, 200, "add accepted: {}", error_text(&reply));

    let projection = expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, channel).await;
    let published = roster_pubkeys(&projection);

    // The p-list is the first MAX_PROJECTION_MEMBERS live members in key
    // order — deterministic, so two instances mint identical bytes. The
    // zero-prefixed seeded block sorts first, so it IS the expected prefix.
    assert_eq!(
        published.len(),
        MAX_PROJECTION_MEMBERS,
        "an over-ceiling roster lists exactly the cap"
    );
    assert_eq!(
        published,
        seeded[..MAX_PROJECTION_MEMBERS].to_vec(),
        "the listed members must be the FIRST cap-many in key order"
    );

    // The cut must be visible on the wire, with the true live total: this is
    // the one signal that lets a client tell \"all the members\" from \"the
    // first N of M\".
    assert_eq!(
        projection.first_tag("truncated"),
        Some(total_live.to_string().as_str()),
        "a truncated roster must advertise itself and the true live total \
         (tags carried: d/h + {} p-tags)",
        published.len()
    );

    // The event stays mintable and valid at the cap.
    assert_eq!(projection.verify(), Ok(()), "the projection must verify");
    assert!(
        projection.serialized_len() <= MAX_SERIALIZED_EVENT_BYTES,
        "a cap-sized roster must fit the serialized ceiling ({} > {})",
        projection.serialized_len(),
        MAX_SERIALIZED_EVENT_BYTES
    );

    // The ADMINS projection is built from the same membership walk, and the
    // admin who sorts past every page boundary must still be in it — an admin
    // missing from 39001 is invisible moderation.
    let admins = expect_projection(&env, kind::KIND_NIP29_GROUP_ADMINS, &relay, channel).await;
    let admin_pks = roster_pubkeys(&admins);
    assert!(
        admin_pks.contains(&far_admin),
        "an admin sorting past the first scan page must still appear in 39001: {:?}",
        admins.tags
    );
    assert!(
        admin_pks.contains(&owner.pk_hex()),
        "the owner must appear in 39001"
    );

    });
}

/// MEASUREMENT, not a gate — run explicitly with
/// `cargo test --test severe_roster_scale_gate -- --ignored --nocapture`.
///
/// Times a join (a kind-9000 add through the real route) against rosters of
/// increasing size. The join's reply waits for the whole ingest saga, which
/// includes republishing the 39002 AND retiring the superseded one —
/// `delete_event_rows_scoped` issues one point delete per index row, so a
/// roster of N members retires in ~N+5 SEQUENTIAL store round trips.
///
/// The first join at each size retires only the tiny creation-time roster;
/// the second retires the (N+2)-tag roster the first one published. So
/// `join₂ − join₁` isolates the retirement loop for an N-member channel.
/// In-process per-op latency is far below a deployed store's; the ROUND-TRIP
/// COUNT is the transferable number, the wall clock here is its floor.
#[ignore = "measurement, not a gate; run with --ignored --nocapture"]
#[test]
 fn measure_join_cost_by_roster_size() {
    common::run_scenario(|| async move {
    use tokio::time::Instant;

    let env = gate_env().await;
    let relay = seed_relay_identity(&env).await;
    let owner = Actor::new("owner");

    // 593 seeded + owner + 2 joins = a 595-member roster retired by the third
    // event — the live `general` channel's measured size.
    for seeded_n in [8usize, 593, 1198, 2813] {
        let channel = format!("meas-{seeded_n}");
        assert_eq!(create_room(&env, &owner, &channel).await.status, 201);
        seed_many_members(&env, &channel, seeded_n).await;

        let mut timings = Vec::new();
        for join in 0..2u8 {
            let member = Actor::new(&format!("joiner-{seeded_n}-{join}"));
            let add = signed(
                &owner,
                kind::KIND_NIP29_PUT_USER,
                member_tags(&channel, &member, Role::Member),
                "",
            );
            let started = Instant::now();
            let reply = submit(&env, &owner, &add).await;
            let elapsed = started.elapsed();
            assert_eq!(reply.status, 200, "add accepted: {}", error_text(&reply));
            timings.push(elapsed);
        }

        let roster =
            expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, &channel).await;
        println!(
            "roster {:>4} members ({:>6.1} KiB event): join1 {:>8.1?} (retires {:>4}-tag roster), \
             join2 {:>8.1?} (retires {:>4}-tag roster), retirement delta {:>8.1?}",
            seeded_n + 3,
            roster.serialized_len() as f64 / 1024.0,
            timings[0],
            1,
            timings[1],
            seeded_n + 2,
            timings[1].saturating_sub(timings[0]),
        );
    }

    });
}
