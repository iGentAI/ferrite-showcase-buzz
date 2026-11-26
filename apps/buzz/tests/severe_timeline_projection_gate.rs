//! **The channel timeline must not re-send the channel's roster on every page.**
//!
//! A NIP-29 group projection (39000 metadata / 39001 admins / 39002 members) is
//! *channel state*, but it carries the channel's `h` tag, so ingest files it in
//! the channel's timeline index next to the messages. The roster grows one `p`
//! tag per member, so on a real room it becomes the largest event in the
//! channel and rides along on **every** timeline page — measured live on the dev
//! relay's `general` (580 members): 49,676 bytes, 88% of a 20-row window reply,
//! at ~1 ms of latency per KB of reply.
//!
//! [`query::KindSelect::timeline_rows`] is the read's way to decline it. This
//! gate holds three things together, and the third is the one that matters:
//!
//! 1. the roster really is in the timeline index (or the fix addresses nothing);
//! 2. selecting it out is a large, *measured* reduction in serialized bytes;
//! 3. **no event a reader needs is lost** — every message comes back whole,
//!    signature and tags byte-identical, and the default read still returns the
//!    projections so the NIP-01 subscription path is untouched.
//!
//! Every number here is asserted on real `serde_json` output of the real read
//! path against a real store, never on a claim.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::Visibility;
use buzz::relay::crypto::to_hex;
use buzz::relay::query::{self, KindSelect};
use buzz::relay::{kind, store as buzz_store, DEFAULT_COMMUNITY};
use common::{Actor, T0_SECS, gate_env, error_text, signed_at, submit, tag};
use ferrite_app::serde_json::{self, json};
use common::SsoTestEnv as TestEnv;

/// Members to join the room. Enough that the roster is unmistakably the biggest
/// event in the channel, which is the condition the fix exists for, while still
/// keeping the gate's runtime sane — each join drives a full ingest saga.
const MEMBERS: usize = 24;

/// The config key an operator seeds the relay's signing identity into. WITHOUT
/// it the relay mints no projections at all, and every assertion here would
/// pass vacuously — so each case seeds it and then proves a projection exists
/// before proving anything about removing it.
const RELAY_IDENTITY_CONFIG: &[u8] = b"buzz-relay-identity";

async fn seed_relay_identity(env: &TestEnv) {
    let relay = Actor::new("the-relay-itself");
    let mut config = env.store("config");
    buzz_store::put_raw(
        &mut config,
        RELAY_IDENTITY_CONFIG,
        to_hex(&relay.sk).as_bytes(),
    )
    .await
    .expect("relay identity seed");
}

/// Create a room through the REAL route, which is what makes the relay mint the
/// room's projections in the first place.
async fn create_room(env: &TestEnv, owner: &Actor, id: &str) {
    let reply = env
        .handle(common::post(
            owner,
            "/buzz/api/channels",
            json!({ "id": id, "name": id, "visibility": Visibility::Open.as_str() })
                .to_string()
                .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(reply.status, 201, "create room: {}", error_text(&reply));
}

/// Join through the real route, which re-emits the roster.
async fn join(env: &TestEnv, who: &Actor, channel: &str, at: u64) {
    let ev = signed_at(
        who,
        kind::KIND_NIP29_JOIN_REQUEST,
        vec![tag("h", channel)],
        "",
        at,
    );
    let reply = submit(env, who, &ev).await;
    assert_eq!(reply.status, 200, "join: {}", error_text(&reply));
}

/// Read the channel timeline straight from the store, bypassing the routes, so
/// the gate measures the read path itself rather than a route's envelope.
async fn read(env: &TestEnv, channel: &str, limit: usize, select: KindSelect) -> query::Page {
    let mut events = env.store("events");
    query::timeline_selected(
        &mut events,
        DEFAULT_COMMUNITY,
        Some(channel),
        None,
        limit,
        select,
    )
    .await
    .expect("timeline read")
}

/// The roster is in the timeline index, it dominates the page, and selecting it
/// out is a big measured byte cut that loses nothing a reader needs.
#[test]
 fn severe_a_timeline_page_need_not_carry_the_channel_roster() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_relay_identity(&env).await;
    let owner = Actor::new("roster-owner");
    let channel = "chan-roster";
    create_room(&env, &owner, channel).await;

    // Real joins, so the roster is built by the relay's own projection emitter
    // rather than hand-shaped by the test.
    for i in 0..MEMBERS {
        let member = Actor::new(&format!("roster-member-{i}"));
        join(&env, &member, channel, T0_SECS + i as u64).await;
    }

    // A handful of ordinary messages: what the reader actually came for. The
    // clock is FROZEN at T0, so every timestamp stays inside the freshness
    // window the relay enforces on a submitted event.
    let mut posted = Vec::new();
    for i in 0..8u64 {
        let ev = signed_at(
            &owner,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", channel)],
            "an ordinary chat message",
            T0_SECS + MEMBERS as u64 + i,
        );
        let reply = submit(&env, &owner, &ev).await;
        assert_eq!(reply.status, 200, "post {i}: {}", error_text(&reply));
        posted.push(ev);
    }

    let limit = 50;
    let before = read(&env, channel, limit, KindSelect::all()).await;
    let after = read(&env, channel, limit, KindSelect::timeline_rows()).await;

    // (1) The roster really is in the timeline index, and really is the biggest
    //     thing in it. If ingest ever stops indexing it, this fails loudly
    //     rather than letting the gate pass vacuously.
    let projections: Vec<_> = before
        .events
        .iter()
        .filter(|e| kind::is_group_projection(e.kind))
        .collect();
    assert!(
        !projections.is_empty(),
        "no group projection in the timeline index — this gate would prove nothing"
    );
    let biggest = before
        .events
        .iter()
        .max_by_key(|e| serde_json::to_vec(e).unwrap().len())
        .unwrap();
    assert!(
        kind::is_group_projection(biggest.kind),
        "the biggest event in the page is kind {}, not a projection",
        biggest.kind
    );

    // (2) The byte cut, measured on real serialized output.
    let before_bytes = serde_json::to_vec(&before).unwrap().len();
    let after_bytes = serde_json::to_vec(&after).unwrap().len();
    let projection_bytes: usize = projections
        .iter()
        .map(|e| serde_json::to_vec(e).unwrap().len())
        .sum();
    assert!(
        after_bytes < before_bytes,
        "filtering did not shrink the page: {before_bytes} B -> {after_bytes} B"
    );
    // The projections are a substantial share of the page even in a 24-member
    // test room...
    assert!(
        projection_bytes * 5 > before_bytes,
        "the projections are only {projection_bytes} of {before_bytes} B — \
         the fixture is not representative of a real room"
    );
    // ...and, the part that actually extrapolates: the roster costs one `p` tag
    // PER MEMBER, so its size grows with the room while a message does not.
    // That growth law is why this matters at 580 members (49,676 B measured
    // live) and not merely at 24 — a share-of-page assertion alone would be a
    // statement about the fixture, not about the property.
    let roster = before
        .events
        .iter()
        .find(|e| e.kind == kind::KIND_NIP29_GROUP_MEMBERS)
        .expect("a member roster");
    let roster_bytes = serde_json::to_vec(roster).unwrap().len();
    let p_tags = roster
        .tags
        .iter()
        .filter(|t| t.first().is_some_and(|n| n == "p"))
        .count();
    assert_eq!(p_tags, MEMBERS + 1, "the roster must carry owner + members");
    assert!(
        roster_bytes > MEMBERS * 60,
        "the roster is {roster_bytes} B for {MEMBERS} members — it is not \
         growing per-member, so the live 49 KB number would not follow"
    );
    // The saving is the projections' own bytes (plus the array commas that
    // separated them), not some incidental re-encoding.
    assert_eq!(
        before_bytes - after_bytes,
        projection_bytes + projections.len(),
        "the cut is not exactly the projections"
    );

    // (3) Nothing a reader needs was lost. Every non-projection event in the
    //     unrestricted page appears in the filtered page BYTE-IDENTICAL — same
    //     signature, same tags, same content. A payload trim that quietly drops
    //     a field is not a win, and this is what rules that out.
    let kept: Vec<_> = before
        .events
        .iter()
        .filter(|e| !kind::is_group_projection(e.kind))
        .collect();
    assert_eq!(
        after.events.len(),
        kept.len(),
        "the filtered page lost a non-projection event"
    );
    for (got, want) in after.events.iter().zip(kept.iter()) {
        assert_eq!(&got, want, "a surviving event was altered");
        assert_eq!(got.sig, want.sig, "a surviving event lost its signature");
        assert_eq!(got.tags, want.tags, "a surviving event lost tags");
    }

    // Every message actually posted is still readable through the filtered path.
    for ev in &posted {
        assert!(
            after.events.iter().any(|e| e.id == ev.id),
            "a posted message vanished from the filtered timeline"
        );
    }

    });
}

/// Paging is not broken by filtering: a rejected row is *consumed*, so a page
/// whose rows are all rejected still advances rather than handing back a cursor
/// that re-reads the same rows forever.
#[test]
 fn severe_filtered_paging_terminates_and_loses_no_message() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_relay_identity(&env).await;
    let owner = Actor::new("page-owner");
    let channel = "chan-page";
    create_room(&env, &owner, channel).await;

    // Interleave joins (each re-emitting the roster) with messages, so the
    // timeline is a mix of wanted and rejected rows rather than one clean block.
    let mut posted = Vec::new();
    for i in 0..8u64 {
        join(
            &env,
            &Actor::new(&format!("page-member-{i}")),
            channel,
            T0_SECS + i * 2,
        )
        .await;

        let ev = signed_at(
            &owner,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", channel)],
            "message between joins",
            T0_SECS + i * 2 + 1,
        );
        assert_eq!(submit(&env, &owner, &ev).await.status, 200);
        posted.push(ev.id);
    }

    // The default read keeps the projections, so the NIP-01 subscription face
    // still answers `{"#h": [channel]}` from the complete index. That is also
    // what makes the "no projection surfaced" assertion below mean something.
    {
        let mut events = env.store("events");
        let all = query::timeline(&mut events, DEFAULT_COMMUNITY, Some(channel), None, 200)
            .await
            .expect("unrestricted read");
        assert!(
            all.events.iter().any(|e| kind::is_group_projection(e.kind)),
            "the DEFAULT timeline read dropped group projections — the REQ path narrowed"
        );
    }

    // Page through in small bites with the filter on, following the cursor.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for round in 0..64 {
        let mut events = env.store("events");
        let page = query::timeline_selected(
            &mut events,
            DEFAULT_COMMUNITY,
            Some(channel),
            cursor.as_deref(),
            3,
            KindSelect::timeline_rows(),
        )
        .await
        .unwrap_or_else(|e| panic!("round {round}: {e}"));

        for ev in &page.events {
            assert!(
                !kind::is_group_projection(ev.kind),
                "a filtered page surfaced a projection"
            );
            seen.push(ev.id);
        }
        if page.complete {
            assert!(page.next_cursor.is_none(), "complete page carried a cursor");
            break;
        }
        let next = page
            .next_cursor
            .expect("incomplete page must carry a cursor");
        assert_ne!(
            Some(&next),
            cursor.as_ref(),
            "the cursor did not advance — paging would spin forever"
        );
        cursor = Some(next);
        assert!(round < 63, "paging did not terminate");
    }

    // Every message survived the walk, exactly once.
    for id in &posted {
        assert_eq!(
            seen.iter().filter(|s| *s == id).count(),
            1,
            "a message was lost or duplicated by filtered paging"
        );
    }

    });
}
