//! **Plane clients are REUSED across requests, not rebuilt per request.**
//!
//! This gate exists because "we pool connections now" is otherwise an assertion
//! in a commit message. The platform pools one live, already-authenticated
//! connection per client INSTANCE, and `AppContext::store` builds a fresh
//! instance on every call — so an app that calls it per handle pays primary
//! discovery, a TCP dial and the capability handshake on every request, forever.
//! One `POST /buzz/api/events` crossed six of those inside `ingest_event` alone.
//!
//! `relay::planes` keeps the clients instead, and counts both halves: clients
//! HANDED OUT and clients BUILT. Reuse is then a measurable property rather than
//! a claim — builds flatten while hand-outs climb — and this gate asserts it
//! against the REAL app over a real in-process fleet.
//!
//! It lives in its own test binary ON PURPOSE: the counters are process-global,
//! so a sibling test running concurrently in the same binary would perturb them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use buzz::relay::channel::{Role, Visibility};
use buzz::relay::planes;
use common::{Actor, channel_query, gate_env, query, seed_channel_with_owner, seed_membership};

/// How many requests the measured burst drives. Enough that a per-request build
/// would be unmistakable (a cold app would build ~4 clients per query), small
/// enough to stay quick.
const BURST: usize = 25;

#[test]
 fn severe_plane_clients_are_reused_across_requests() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ada = Actor::new("ada-the-reader");
    let channel = "pool-probe";
    seed_channel_with_owner(&env, channel, Visibility::Open, &ada).await;
    seed_membership(&env, channel, &ada, Role::Member).await;

    // Warm up: the first requests legitimately build the clients they need, and
    // boot work (asset seeding, topic creation) has its own. Measure AFTER that,
    // so this gate is about steady state rather than start-up.
    for _ in 0..3 {
        let reply = query(&env, &ada, &channel_query(channel)).await;
        assert_eq!(
            reply.status,
            200,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
    }
    let builds_before = planes::builds();
    let checkouts_before = planes::checkouts();

    for _ in 0..BURST {
        let reply = query(&env, &ada, &channel_query(channel)).await;
        assert_eq!(
            reply.status,
            200,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
    }

    let builds = planes::builds() - builds_before;
    let checkouts = planes::checkouts() - checkouts_before;

    // Each query authenticates (config), reads the timeline (events) and
    // resolves channel access (events + guards), so a burst hands out several
    // clients per request. If it does not, this gate is measuring nothing.
    assert!(
        checkouts >= BURST as u64 * 2,
        "expected the burst to hand out at least two clients per request, saw \
         {checkouts} hand-outs for {BURST} requests — is the pool still wired in?"
    );

    // THE ASSERTION: a warm pool builds (almost) nothing. A small allowance
    // covers a binding this workload touches for the first time mid-burst; what
    // it will not tolerate is a build per request, which is what the app did
    // before pooling and what a regression would restore.
    assert!(
        builds <= 4,
        "a warm pool must not build a client per request: {builds} clients built \
         for {checkouts} hand-outs across {BURST} requests"
    );

    // And the clients really are parked for reuse rather than dropped.
    let idle: usize = planes::idle_counts().iter().map(|(_, n)| n).sum();
    assert!(
        idle > 0,
        "after a burst some clients should be idle in the pool, found none: {:?}",
        planes::idle_counts()
    );

    // The app is still healthy after all of that — the closing invariant every
    // gate in this suite shares.
    let health = env
        .handle(common::get("/buzz/api/healthz"))
        .await
        .expect("the app replied");
    assert_eq!(health.status, 200);

    });
}
