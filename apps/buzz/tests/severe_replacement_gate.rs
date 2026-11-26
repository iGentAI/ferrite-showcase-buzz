//! **W7 / M1.6 — NIP-16/33 replacement with watermarks, decided by the WIRING.**
//!
//! A replaceable cell — `(kind, author)` for NIP-01 kinds, `(kind, author, d)`
//! for NIP-33 — converges on ONE winning version: newest `created_at`, equal
//! timestamps breaking to the LOWEST id. The loser's queryable rows (event
//! body + every index) are deleted; the head watermark on the APE plane is
//! the advisory lock's replacement. Cases assert on durable state — stored
//! bodies, index rows, query results, the watermark — in BOTH arrival orders,
//! and re-drive a torn saga to prove reconvergence.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::crypto::to_hex;
use buzz::relay::effects::ReplaceableHead;
use buzz::relay::{ingest, keys, kind};
use common::{
    Actor, COMMUNITY, FEED_GENERATION, T0_MS, T0_SECS, gate_env, index_keys, query, returned_ids,
    signed_at, stored_event, submit, tag,
};
use ferrite_app::serde_json::{self, json};
use common::SsoTestEnv as TestEnv;

/// The stored watermark for one cell, if any.
async fn head_of(env: &TestEnv, kind_: u32, author: &Actor, d: &str) -> Option<ReplaceableHead> {
    let mut guards = env.ape("guards");
    let key = keys::dtag_head(COMMUNITY, kind_, &author.pk, d);
    let raw = guards.get(&key).await.expect("head read")?;
    Some(serde_json::from_slice(&raw).expect("head decodes"))
}

/// Every stored body id for `(author, kind)` per the author index.
async fn queryable_ids(env: &TestEnv, author: &Actor, kind_: u32) -> Vec<String> {
    let mut out = Vec::new();
    for key in index_keys(
        env,
        &keys::author_prefix(COMMUNITY, &author.pk, Some(kind_)),
    )
    .await
    {
        let Some((_, id)) = keys::parse_index_tail(&key) else {
            continue;
        };
        // An index row only counts if its body is still present (the reader
        // rule) — replacement deletes indexes first, body last.
        if stored_event(env, &id).await.is_some() {
            out.push(to_hex(&id));
        }
    }
    out
}

/// A newer version replaces: the old body and its index rows are GONE, the
/// watermark names the winner — and a stale version resubmitted later (a
/// client syncing history) is absorbed without resurrecting anything.
#[test]
 fn severe_a_newer_version_supersedes_and_a_stale_resubmit_stays_dead() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");

    let v1 = signed_at(
        &alice,
        kind::KIND_PROFILE,
        vec![],
        r#"{"name":"v1"}"#,
        T0_SECS - 100,
    );
    let v2 = signed_at(
        &alice,
        kind::KIND_PROFILE,
        vec![],
        r#"{"name":"v2"}"#,
        T0_SECS,
    );

    assert_eq!(submit(&env, &alice, &v1).await.status, 200);
    assert_eq!(
        queryable_ids(&env, &alice, kind::KIND_PROFILE).await,
        vec![to_hex(&v1.id)],
        "v1 is the profile until replaced"
    );

    assert_eq!(submit(&env, &alice, &v2).await.status, 200);
    assert_eq!(
        queryable_ids(&env, &alice, kind::KIND_PROFILE).await,
        vec![to_hex(&v2.id)],
        "v2 replaced v1"
    );
    assert!(
        stored_event(&env, &v1.id).await.is_none(),
        "the superseded body is deleted"
    );
    let head = head_of(&env, kind::KIND_PROFILE, &alice, "")
        .await
        .expect("a watermark");
    assert_eq!(head.id, to_hex(&v2.id));
    assert_eq!(head.created_at, v2.created_at);

    // The stale version arrives again (history sync): accepted, absorbed,
    // and v2 still stands alone.
    assert_eq!(submit(&env, &alice, &v1).await.status, 200);
    assert_eq!(
        queryable_ids(&env, &alice, kind::KIND_PROFILE).await,
        vec![to_hex(&v2.id)],
        "a stale resubmit must not resurrect the old version"
    );
    assert!(stored_event(&env, &v1.id).await.is_none());

    // The QUERY face agrees with the store: only v2 comes back.
    let reply = query(
        &env,
        &alice,
        &json!({ "filters": [{ "kinds": [kind::KIND_PROFILE], "authors": [alice.pk_hex()] }] }),
    )
    .await;
    assert_eq!(reply.status, 200);
    let ids = returned_ids(&reply);
    assert_eq!(
        ids,
        vec![to_hex(&v2.id)],
        "the query face returns only the winner"
    );

    });
}

/// Equal timestamps break to the LOWEST id — in BOTH arrival orders, two
/// racing relays (or instances) converge on the same winner.
#[test]
 fn severe_equal_timestamps_break_to_the_lowest_id_in_both_orders() {
    common::run_scenario(|| async move {
    let env = gate_env().await;

    for (label, low_first) in [("bob", true), ("carol", false)] {
        let author = Actor::new(label);
        let a = signed_at(&author, kind::KIND_MUTE_LIST, vec![], "list-a", T0_SECS);
        let b = signed_at(&author, kind::KIND_MUTE_LIST, vec![], "list-b", T0_SECS);
        let (winner, loser) = if to_hex(&a.id) < to_hex(&b.id) {
            (&a, &b)
        } else {
            (&b, &a)
        };

        let (first, second) = if low_first {
            (winner, loser)
        } else {
            (loser, winner)
        };
        assert_eq!(submit(&env, &author, first).await.status, 200);
        assert_eq!(submit(&env, &author, second).await.status, 200);

        assert_eq!(
            queryable_ids(&env, &author, kind::KIND_MUTE_LIST).await,
            vec![to_hex(&winner.id)],
            "arrival order {label}: the LOWEST id wins the tie"
        );
        assert!(stored_event(&env, &loser.id).await.is_none());
        let head = head_of(&env, kind::KIND_MUTE_LIST, &author, "")
            .await
            .expect("watermark");
        assert_eq!(head.id, to_hex(&winner.id));
    }

    });
}

/// NIP-33 cells are per-d-tag: two d values coexist, and a newer version
/// replaces ONLY its own cell.
#[test]
 fn severe_nip33_replaces_per_d_tag_cell() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let dana = Actor::new("dana");
    let kind_ = kind::KIND_READ_STATE;

    let inbox_v1 = signed_at(
        &dana,
        kind_,
        vec![tag("d", "inbox")],
        "seq 10",
        T0_SECS - 50,
    );
    let board = signed_at(&dana, kind_, vec![tag("d", "board")], "seq 3", T0_SECS - 40);
    let inbox_v2 = signed_at(&dana, kind_, vec![tag("d", "inbox")], "seq 22", T0_SECS);

    for e in [&inbox_v1, &board, &inbox_v2] {
        assert_eq!(submit(&env, &dana, e).await.status, 200);
    }

    let mut ids = queryable_ids(&env, &dana, kind_).await;
    ids.sort();
    let mut want = vec![to_hex(&inbox_v2.id), to_hex(&board.id)];
    want.sort();
    assert_eq!(ids, want, "inbox replaced within its cell; board untouched");
    assert!(stored_event(&env, &inbox_v1.id).await.is_none());

    let inbox_head = head_of(&env, kind_, &dana, "inbox")
        .await
        .expect("inbox watermark");
    assert_eq!(inbox_head.id, to_hex(&inbox_v2.id));
    let board_head = head_of(&env, kind_, &dana, "board")
        .await
        .expect("board watermark");
    assert_eq!(board_head.id, to_hex(&board.id));

    });
}

/// The torn-saga case: a loser whose saga never completed is re-driven AFTER
/// the winner landed. `apply_storage` re-creates the very rows replacement
/// deleted — and the un-gated replacement step re-converges the cell.
#[test]
 fn severe_a_redriven_loser_saga_reconverges_instead_of_resurrecting() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let erin = Actor::new("erin");
    let old = signed_at(
        &erin,
        kind::KIND_PROFILE,
        vec![],
        r#"{"name":"old"}"#,
        T0_SECS - 100,
    );
    let new = signed_at(
        &erin,
        kind::KIND_PROFILE,
        vec![],
        r#"{"name":"new"}"#,
        T0_SECS,
    );

    // The old version's saga: admitted and stored, then the driver DIES
    // before replacement/completion (we simply stop, as a crash would).
    let mut ape = env.ape("guards");
    let mut events = env.store("events");
    let outcome = ingest::admit(&mut ape, COMMUNITY, &old, None, T0_MS)
        .await
        .expect("admit old");
    ingest::apply_storage(&mut events, COMMUNITY, &old, None)
        .await
        .expect("store old");

    // The new version lands whole through the real route and wins the cell.
    assert_eq!(submit(&env, &erin, &new).await.status, 200);
    assert_eq!(
        queryable_ids(&env, &erin, kind::KIND_PROFILE).await,
        vec![to_hex(&new.id)],
        "the whole saga's version won"
    );

    // Redelivery re-drives the torn saga end to end. Its storage step
    // re-creates the old rows; its replacement step must re-converge.
    let mut topics = env.topics("feed");
    ingest::drive(
        &mut events,
        &mut ape,
        &mut topics,
        COMMUNITY,
        FEED_GENERATION,
        &old,
        &outcome,
        T0_MS,
        // No relay identity: this gate is about replacement, and a profile
        // event invalidates no group projection anyway.
        None,
    )
    .await
    .expect("re-drive the torn saga");

    assert_eq!(
        queryable_ids(&env, &erin, kind::KIND_PROFILE).await,
        vec![to_hex(&new.id)],
        "a re-driven loser saga must reconverge, not resurrect"
    );
    assert!(stored_event(&env, &old.id).await.is_none());
    let head = head_of(&env, kind::KIND_PROFILE, &erin, "")
        .await
        .expect("watermark");
    assert_eq!(head.id, to_hex(&new.id));

    });
}
