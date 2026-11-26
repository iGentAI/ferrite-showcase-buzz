//! **An unreadable schedule row does not block anyone's cap.**
//!
//! A legacy agent's first wake seeds its slot record from a community-wide scan.
//! One undecodable row elsewhere in the community used to make that scan
//! "unprovable", so every legacy wake of every unseeded agent waited forever. Such
//! a row can never be claimed or run, so it holds no slot: the scan skips it, the
//! record is seeded, the wake runs under the cap, and new creates are admitted.
//! Its own store-only fleet: no relay and no turn consumer run, so nothing else
//! claims.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::common;

use buzz::agents::turns::ScheduledWake;
use buzz::relay::schedules::{self, ScheduleRequest};
use buzz::relay::store as buzz_store;
use common::{Actor, COMMUNITY, T0_MS};
use ferrite_app_test::TestHarness;

#[test]
fn severe_an_unreadable_row_does_not_block_seeding_the_cap() {
    common::run_scenario(|| async move {
    #[cfg(feature = "platform-queue-cancel")]
    let harness = TestHarness::new().with_store("nonces").with_named_queue("agent-turns", "scheduled-turns");
    #[cfg(not(feature = "platform-queue-cancel"))]
    let harness = TestHarness::new().with_store("nonces").with_queue("agent-turns");
    let env = harness.with_store("config").manual_clock(T0_MS).build().await;
    let request = ScheduleRequest {
        agent: "unprovable".into(),
        channel: "chan-unprovable".into(),
        prompt: "legacy".into(),
        first_due_ms: T0_MS,
        interval_ms: None,
        occurrences: 1,
        created_by: Actor::new("sched-unprovable").pk_hex(),
    };
    // A schedule from before the slot record existed: a bare row, no slots.
    let row = schedules::build(&request, COMMUNITY, T0_MS - 1_000).expect("builds");
    let mut config = env.store("config");
    buzz_store::put_doc(&mut config, schedules::schedule_key(COMMUNITY, &row.id).as_bytes(), &row)
        .await
        .expect("seed a legacy row");
    config
        .put(schedules::schedule_key(COMMUNITY, "0000corrupt").as_bytes(), b"not a schedule")
        .await
        .expect("seed an unreadable row");
    let wake = ScheduledWake { id: row.id.clone(), occurrence: 0, revision: 0 };
    let claim = schedules::claim(&mut config, COMMUNITY, &wake, T0_MS).await.expect("claims");
    assert!(matches!(claim, schedules::Claim::Run(_)), "the legacy schedule runs under the cap: {claim:?}");
    let slots: schedules::AgentSlots = buzz_store::get_doc(
        &mut config,
        schedules::agent_slots_key(COMMUNITY, "unprovable").as_bytes(),
    )
    .await
    .expect("read slots")
    .expect("the claim seeded the record");
    assert!(slots.slots.iter().any(|s| s.id == row.id), "the legacy schedule holds a slot");
    // And new schedules are admitted against that record.
    let mut fresh = request.clone();
    fresh.channel = "chan-unprovable-new".into();
    let ctx = env.context();
    schedules::create(&ctx, &mut config, COMMUNITY, &fresh, "", T0_MS)
        .await
        .expect("a new create is admitted");
    });
}
