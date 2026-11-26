//! **A kind-39000 read and a kind-39002 read are the same read, and one page is
//! not the workspace.**
//!
//! This gate exists to close a wrong diagnosis, and the wrong diagnosis is worth
//! stating because it was reasonable. Measured live on the dev relay:
//!
//! ```text
//! {kinds:[39000], limit:500}  ->  146 events,  881 tags,  69 KB, 3891 ms
//! {kinds:[39002], limit:500}  ->  146 events, 1544 tags, 162 KB, 1209 ms
//! ```
//!
//! Same event count, *less than half* the bytes, ~3x the wall clock. The
//! conclusion drawn was that the 39000 filter must resolve to a different index,
//! or fall back to a scan. It does not. Both are [`query::by_kind`] over
//! `keys::kind_prefix`, through the same pager, with the same
//! [`query::KindSelect::all`] — there is no per-kind branch anywhere on the path.
//!
//! Re-measured 2026-07-29 against the live relay, back to back on one socket:
//! `39000` and `39002` cost **1055 ms and 1086 ms** for the same 134 events, at
//! four different limits, in both orders. The 3x was a queueing artefact:
//! `channelsFromRelay` issues the two REQs *concurrently* on one socket,
//! WebSocket reads do not overlap, and whichever REQ lands second pays its own
//! cost plus the first's — 1068/2124 ms one way and **2126/1078 ms with the send
//! order reversed**, which is the control that load cannot fake. The "expensive"
//! kind moves with the send order, not with the kind.
//!
//! So there are two gates here:
//!
//! * [`severe_the_two_group_projection_kinds_cost_the_same_read`] — the
//!   refutation, against a real store: same row count, same event count, both
//!   pages complete without a cursor, and the roster page carrying multiples of
//!   the metadata page's bytes for the same count. If a future change makes one
//!   of these kinds resolve differently — a scan fallback, a second index, an
//!   extra page — this fails.
//! * [`severe_a_kind_page_is_bought_in_index_rows_so_one_page_is_not_the_workspace`]
//!   — the defect that IS real on this path, and the one that made the read
//!   expensive: a page is spent on index ROWS, not on rows the caller can use.
//!   Live, `{kinds:[39000],limit:1}` answered with **zero** events while 134
//!   rooms existed, `limit:20` answered with 5, and `limit:500` answered with
//!   133 while paging with `until` found **39 more channels** the client never
//!   saw. The relay is not lying — every short page reports `complete: false`
//!   and hands back a resume point — the caller (`ws_candidates`, which takes
//!   `2 x limit` candidates and stops) never pages. This gate pins the contract
//!   the fix has to rely on.
//!
//! Every number asserted here is measured off the real read path against a real
//! store. Nothing is asserted about wall-clock, deliberately: the live figures
//! were taken on a box whose load average ran from 8 to 31, and the conclusions
//! above rest on ratios, controls and orderings for exactly that reason.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::Visibility;
use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::{keys, kind, query, store as buzz_store, DEFAULT_COMMUNITY};
use common::{Actor, T0_SECS, gate_env, error_text, index_keys, signed_at, submit, tag};
use ferrite_app::serde_json::{self, json};
use common::SsoTestEnv as TestEnv;

/// The config key an operator seeds the relay's signing identity into. WITHOUT
/// it the relay mints no projections at all and every assertion here would pass
/// vacuously, so each case seeds it and then proves projections exist before
/// proving anything about their cost.
const RELAY_IDENTITY_CONFIG: &[u8] = b"buzz-relay-identity";

/// Rooms in the fixture. Enough that a `limit: 1` page is unmistakably a
/// fraction of the workspace, and few enough that every room's creation saga
/// completes inside the gate's budget.
const ROOMS: usize = 6;

/// Extra members joined to one room, so its 39002 roster is much larger than its
/// 39000 metadata — which is the live shape and the whole point of the byte
/// comparison.
///
/// CALIBRATED against assertion (4), which requires the roster page to carry more
/// than twice the metadata page's bytes. The metadata page is fixed by [`ROOMS`]
/// (~2.6 KB); each member adds one `p` tag (~72 B) to exactly one roster, so this
/// constant alone decides the ratio. At 24 the fixture produced 4794 B against
/// 2583 B — 1.86x, UNDER the floor — and the gate failed on its own
/// meaningfulness check while every claim it makes about the READ still held.
/// Raising the roster restores the margin AND moves the fixture toward the live
/// shape it is modelling (49,676 B at general's 580 members); lowering the floor
/// would instead have given up the asymmetry this gate exists to keep testing.
const BIG_ROOM_MEMBERS: usize = 64;

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
/// room's 39000/39001/39002 projections.
async fn create_room(env: &TestEnv, owner: &Actor, id: &str, visibility: Visibility) {
    let reply = env
        .handle(common::post(
            owner,
            "/buzz/api/channels",
            json!({ "id": id, "name": id, "visibility": visibility.as_str() })
                .to_string()
                .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(reply.status, 201, "create {id}: {}", error_text(&reply));
}

async fn join(env: &TestEnv, who: &Actor, channel: &str, at: u64) {
    let ev = signed_at(
        who,
        kind::KIND_NIP29_JOIN_REQUEST,
        vec![tag("h", channel)],
        "",
        at,
    );
    let reply = submit(env, who, &ev).await;
    assert_eq!(reply.status, 200, "join {channel}: {}", error_text(&reply));
}

/// The room ids the fixture builds, and which of them are private.
fn room_id(i: usize) -> String {
    format!("room-{i}")
}

fn is_private(i: usize) -> bool {
    // Everything but the first, so a non-member reader can see exactly ONE room
    // and the gate's deliverability arithmetic is deterministic.
    i != 0
}

/// Stand up ROOMS rooms, one open and the rest private, with one big roster.
async fn seed_workspace(env: &TestEnv) -> Actor {
    seed_relay_identity(env).await;
    let owner = Actor::new("projection-cost-owner");
    for i in 0..ROOMS {
        let visibility = if is_private(i) {
            Visibility::Private
        } else {
            Visibility::Open
        };
        create_room(env, &owner, &room_id(i), visibility).await;
    }
    // One room gets a real roster, built by the relay's own projection emitter.
    // The open room is the one that grows, so the byte asymmetry survives the
    // access gate as well as the read.
    for m in 0..BIG_ROOM_MEMBERS {
        let member = Actor::new(&format!("projection-cost-member-{m}"));
        join(env, &member, &room_id(0), T0_SECS + m as u64).await;
    }
    owner
}

async fn read_kind(env: &TestEnv, kind_: u32, cursor: Option<&str>, limit: usize) -> query::Page {
    let mut events = env.store("events");
    query::by_kind(&mut events, DEFAULT_COMMUNITY, kind_, cursor, limit)
        .await
        .expect("kind read")
}

fn d_tag(event: &Event) -> String {
    event
        .tags
        .iter()
        .find(|t| t.first().is_some_and(|n| n == "d"))
        .and_then(|t| t.get(1))
        .cloned()
        .unwrap_or_default()
}

fn page_bytes(page: &query::Page) -> usize {
    page.events
        .iter()
        .map(|e| serde_json::to_vec(e).unwrap().len())
        .sum()
}

/// The rooms a page names, sorted and deduplicated — the client's own view
/// (`latestByDTag`), so a surviving superseded version does not read as an extra
/// room.
fn distinct_rooms(page: &query::Page) -> Vec<String> {
    let mut ds: Vec<String> = page.events.iter().map(d_tag).collect();
    ds.sort();
    ds.dedup();
    ds
}

/// The two kinds are one index family read by one pager: same rows, same events,
/// same page shape, wildly different bytes.
#[test]
 fn severe_the_two_group_projection_kinds_cost_the_same_read() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_workspace(&env).await;

    // (0) Non-vacuity. The relay really minted both projections for every room;
    //     without this the equalities below could hold at zero.
    let meta_rows = index_keys(
        &env,
        &keys::kind_prefix(DEFAULT_COMMUNITY, kind::KIND_NIP29_GROUP_METADATA),
    )
    .await;
    let roster_rows = index_keys(
        &env,
        &keys::kind_prefix(DEFAULT_COMMUNITY, kind::KIND_NIP29_GROUP_MEMBERS),
    )
    .await;
    assert!(
        meta_rows.len() >= ROOMS,
        "expected at least one 39000 index row per room, got {}",
        meta_rows.len()
    );
    // The symmetry claim at the INDEX level: whatever churn the fixture produced,
    // the two families hold it identically. Asserted as an equality rather than
    // against ROOMS so that a lingering superseded row (a retirement bug, which
    // this gate is not about) fails the arithmetic below by name instead of here.
    assert_eq!(
        meta_rows.len(),
        roster_rows.len(),
        "the two projection indexes hold different row counts ({} vs {}) — one of \
         them is accumulating rows the other retires, which WOULD make one read \
         dearer than the other",
        meta_rows.len(),
        roster_rows.len()
    );

    // (1) The two reads: same limit, same index family, same pager.
    let meta = read_kind(&env, kind::KIND_NIP29_GROUP_METADATA, None, 500).await;
    let rosters = read_kind(&env, kind::KIND_NIP29_GROUP_MEMBERS, None, 500).await;

    assert_eq!(
        meta.events.len(),
        rosters.events.len(),
        "the two kinds returned different event counts — the fixture is asymmetric \
         and nothing below means what it says"
    );

    // (2) Neither read paged and neither fell back: both walked their prefix to
    //     the end in one page and said so. A scan fallback or a second index
    //     would show up here as an incomplete page or a cursor.
    assert!(
        meta.complete && meta.next_cursor.is_none(),
        "the 39000 read did not finish its index in one page"
    );
    assert!(
        rosters.complete && rosters.next_cursor.is_none(),
        "the 39002 read did not finish its index in one page"
    );

    // (3) Same rooms, both ways. If one kind's index ever stops covering a room
    //     the client's channel list silently loses it.
    let meta_ds = distinct_rooms(&meta);
    let roster_ds = distinct_rooms(&rosters);
    assert_eq!(meta_ds, roster_ds, "the two indexes name different rooms");
    let expected: Vec<String> = {
        let mut v: Vec<String> = (0..ROOMS).map(room_id).collect();
        v.sort();
        v
    };
    assert_eq!(meta_ds, expected, "a room is missing from the channel list");

    // (4) The bytes are wildly different for the identical event count — which is
    //     exactly the observation the "bytes explain the cost" hypothesis rested
    //     on, and it is true. It just does not follow: the read shape above is
    //     identical. This assertion is what makes the refutation non-trivial.
    let meta_bytes = page_bytes(&meta);
    let roster_bytes = page_bytes(&rosters);
    assert!(
        roster_bytes > meta_bytes * 2,
        "the roster page is only {roster_bytes} B against {meta_bytes} B — the \
         fixture no longer reproduces the live byte asymmetry, so this gate would \
         not catch a byte-driven regression"
    );

    // (5) And the roster really grows per member, which is why it is the big one
    //     on a real workspace (49,676 B at general's live 580 members) rather
    //     than merely in this fixture.
    let big = rosters
        .events
        .iter()
        .find(|e| d_tag(e) == room_id(0))
        .expect("the big room's roster");
    let p_tags = big
        .tags
        .iter()
        .filter(|t| t.first().is_some_and(|n| n == "p"))
        .count();
    assert_eq!(
        p_tags,
        BIG_ROOM_MEMBERS + 1,
        "the roster must carry owner + members, or it is not growing per member"
    );

    });
}

/// A page is bought in index rows. One page is therefore not the workspace, and
/// the page says so — which is the only reason a correct caller is possible.
#[test]
 fn severe_a_kind_page_is_bought_in_index_rows_so_one_page_is_not_the_workspace() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_workspace(&env).await;

    // (1) `limit: 1` buys ONE index row out of six rooms, and does not pretend
    //     to be finished. Live this is the read that answered with zero events
    //     across 134 rooms, once its single row was refused by the access gate.
    let first = read_kind(&env, kind::KIND_NIP29_GROUP_METADATA, None, 1).await;
    assert_eq!(first.events.len(), 1, "the budget bought one index row");
    assert!(
        !first.complete && first.next_cursor.is_some(),
        "a page that stopped on its budget must report that it is not the end \
         and hand back a resume point — without this no caller can be correct"
    );

    // (2) Following the cursor reaches every room, exactly once. This is the
    //     contract the caller-side fix has to rely on, so it is asserted rather
    //     than assumed.
    let mut ids: Vec<String> = Vec::new();
    let mut rooms: Vec<String> = Vec::new();
    let mut cursor = first.next_cursor.clone();
    ids.extend(first.events.iter().map(|e| to_hex(&e.id)));
    rooms.extend(first.events.iter().map(d_tag));
    let mut pages = 1;
    while let Some(c) = cursor {
        let page = read_kind(&env, kind::KIND_NIP29_GROUP_METADATA, Some(&c), 1).await;
        pages += 1;
        ids.extend(page.events.iter().map(|e| to_hex(&e.id)));
        rooms.extend(page.events.iter().map(d_tag));
        assert!(pages <= ROOMS * 4, "the pager is not making progress");
        if page.complete {
            break;
        }
        cursor = page.next_cursor;
    }
    // No EVENT is served twice — that is the paging-progress guarantee, and it is
    // what a caller relying on the cursor needs.
    let mut unique_ids = ids.clone();
    unique_ids.sort();
    unique_ids.dedup();
    assert_eq!(
        unique_ids.len(),
        ids.len(),
        "paging served the same event twice: {ids:?}"
    );
    rooms.sort();
    rooms.dedup();
    assert_eq!(
        rooms,
        {
            let mut v: Vec<String> = (0..ROOMS).map(room_id).collect();
            v.sort();
            v
        },
        "paging to completion must reach every room"
    );
    assert!(
        pages > 1,
        "{ROOMS} rooms were somehow answered in one `limit: 1` page"
    );

    });
}
