//! **M1.5 — reactions, decided by the WIRING.**
//!
//! A reaction (kind 7) is an upsert with buzz's peculiar "reactivate only if
//! removed" rule: adding one that is already active stores no event and changes
//! no tally; adding one that was removed revives the same row. The reaction
//! inherits its target's channel verbatim, and a reaction to a target the relay
//! does not hold is refused. Every case posts real events and asserts on the
//! `POST /buzz/api/reactions` tally.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::Visibility;
use buzz::relay::effects::{self, ReactionState};
use buzz::relay::{crypto, keys, kind, store, DEFAULT_COMMUNITY as COMMUNITY};
use common::{
    Actor, T0_MS, T0_SECS, body_json, gate_env, error_text, post, seed_channel_with_owner,
    seed_membership, signed, signed_at, stored_event_count, submit, tag,
};
use ferrite_app::serde_json::{json, Value};
use common::SsoTestEnv as TestEnv;

/// Post a message and return its event id (hex).
async fn post_message(env: &TestEnv, actor: &Actor, channel: &str, text: &str) -> String {
    let msg = signed(
        actor,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        text,
    );
    let reply = submit(env, actor, &msg).await;
    assert_eq!(
        reply.status,
        200,
        "message accepted: {}",
        error_text(&reply)
    );
    crypto::to_hex(&msg.id)
}

/// The reaction tallies on `target`, as the read route returns them.
async fn tallies(env: &TestEnv, reader: &Actor, target: &str) -> Vec<Value> {
    let body = json!({ "target": target }).to_string().into_bytes();
    let reply = env
        .handle(post(reader, "/buzz/api/reactions", body))
        .await
        .expect("reactions reply");
    assert_eq!(reply.status, 200, "reactions read: {}", error_text(&reply));
    body_json(&reply)["reactions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn react(actor: &Actor, target: &str, emoji: &str, created_at: u64) -> buzz::relay::event::Event {
    signed_at(
        actor,
        kind::KIND_REACTION,
        vec![tag("e", target)],
        emoji,
        created_at,
    )
}

/// Adding, then re-adding the same active reaction: the second is a duplicate —
/// no second event stored, tally still one.
#[test]
 fn severe_active_duplicate_reaction_is_a_noop() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-react";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    let target = post_message(&env, &owner, channel, "react to me").await;

    let before = stored_event_count(&env).await;
    let first = react(&owner, &target, "👍", T0_SECS);
    assert_eq!(submit(&env, &owner, &first).await.status, 200);
    assert_eq!(
        stored_event_count(&env).await,
        before + 1,
        "reaction stored"
    );

    // Same active reaction again (distinct event id via a later second): duplicate.
    let dup = react(&owner, &target, "👍", T0_SECS + 1);
    let reply = submit(&env, &owner, &dup).await;
    assert_eq!(reply.status, 200);
    assert!(
        error_text(&reply).contains("duplicate") || body_json(&reply)["accepted"] == json!(false),
        "an active duplicate is reported as such: {}",
        error_text(&reply)
    );
    assert_eq!(
        stored_event_count(&env).await,
        before + 1,
        "no second kind-7 event was stored for the active duplicate"
    );

    let t = tallies(&env, &owner, &target).await;
    assert_eq!(t.len(), 1);
    assert_eq!(t[0]["emoji"], json!("👍"));
    assert_eq!(t[0]["count"], json!(1));

    });
}

/// Remove (kind-5 self-delete) then re-add: the row revives — reactivate only if
/// removed.
#[test]
 fn severe_removed_reaction_revives_on_readd() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-revive-rxn";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    let target = post_message(&env, &owner, channel, "react to me").await;

    let rx = react(&owner, &target, "🎉", T0_SECS);
    assert_eq!(submit(&env, &owner, &rx).await.status, 200);
    assert_eq!(tallies(&env, &owner, &target).await.len(), 1);

    // Delete the reaction (kind 5 targeting the reaction event id).
    let del = signed_at(
        &owner,
        kind::KIND_DELETION,
        vec![tag("e", &crypto::to_hex(&rx.id))],
        "",
        T0_SECS + 1,
    );
    assert_eq!(submit(&env, &owner, &del).await.status, 200);
    assert!(
        tallies(&env, &owner, &target).await.is_empty(),
        "the removed reaction drops out of the tally"
    );

    // Re-add (distinct event): the row revives.
    let readd = react(&owner, &target, "🎉", T0_SECS + 2);
    assert_eq!(submit(&env, &owner, &readd).await.status, 200);
    let t = tallies(&env, &owner, &target).await;
    assert_eq!(t.len(), 1);
    assert_eq!(t[0]["emoji"], json!("🎉"));

    });
}

/// A reaction row follows the reaction event that owns it: a late retraction of
/// an earlier reaction must not remove a later reaction that revived the row.
#[test]
fn severe_a_late_retraction_does_not_remove_a_later_reaction() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-rx-late-retract";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    let target = post_message(&env, &owner, channel, "react to me").await;

    let first = react(&owner, &target, "🎉", T0_SECS);
    assert_eq!(submit(&env, &owner, &first).await.status, 200);

    let mut events = env.store("events");
    let key = keys::reaction(COMMUNITY, &target, &owner.pk_hex(), "🎉");
    let later = ReactionState {
        reaction_event_id: "bb".repeat(32),
        emoji: "🎉".into(),
        created_at: T0_MS + 2,
        removed_at: None,
    };
    store::put_doc(&mut events, &key, &later).await.expect("seed the later reaction");

    let retraction = signed_at(
        &owner,
        kind::KIND_DELETION,
        vec![tag("e", &crypto::to_hex(&first.id))],
        "",
        T0_SECS + 1,
    );
    let mut ape = env.ape("guards");
    effects::apply_effects(&mut events, &mut ape, COMMUNITY, &retraction, Some(channel), T0_MS + 3)
        .await
        .expect("the retraction effect");

    assert_eq!(
        tallies(&env, &owner, &target).await.len(),
        1,
        "retracting the first reaction must leave the later one live"
    );
    });
}

/// Re-applying a reaction that was already retracted must not revive it.
#[test]
fn severe_a_retracted_reaction_is_not_revived_by_its_own_redrive() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-rx-no-revive";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    let target = post_message(&env, &owner, channel, "react to me").await;

    let first = react(&owner, &target, "🎉", T0_SECS);
    assert_eq!(submit(&env, &owner, &first).await.status, 200);

    let mut events = env.store("events");
    let key = keys::reaction(COMMUNITY, &target, &owner.pk_hex(), "🎉");
    let retracted = ReactionState {
        reaction_event_id: crypto::to_hex(&first.id),
        emoji: "🎉".into(),
        created_at: T0_MS,
        removed_at: Some(T0_MS + 1),
    };
    store::put_doc(&mut events, &key, &retracted).await.expect("seed the retracted row");

    let mut ape = env.ape("guards");
    effects::apply_effects(&mut events, &mut ape, COMMUNITY, &first, Some(channel), T0_MS + 2)
        .await
        .expect("the reaction effect");

    assert!(
        tallies(&env, &owner, &target).await.is_empty(),
        "re-applying a retracted reaction must not revive it"
    );
    });
}

/// A reaction to a target the relay does not hold is refused.
#[test]
 fn severe_reaction_to_a_missing_target_is_refused() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let missing = "f".repeat(64);
    let rx = react(&owner, &missing, "👍", T0_SECS);
    let reply = submit(&env, &owner, &rx).await;
    assert_eq!(reply.status, 400, "missing target refused");
    assert!(error_text(&reply).contains("target"));

    });
}

/// Two different members reacting with different emoji: two groups, and an empty
/// content defaults to `+`.
#[test]
 fn severe_distinct_reactions_group_by_emoji() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let member = Actor::new("member");
    let channel = "chan-multi-rxn";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    seed_membership(&env, channel, &member, buzz::relay::channel::Role::Member).await;
    let target = post_message(&env, &owner, channel, "reactions please").await;

    assert_eq!(
        submit(&env, &owner, &react(&owner, &target, "👍", T0_SECS))
            .await
            .status,
        200
    );
    assert_eq!(
        submit(&env, &member, &react(&member, &target, "👍", T0_SECS))
            .await
            .status,
        200
    );
    // Empty content ⇒ "+".
    let plus = signed_at(
        &member,
        kind::KIND_REACTION,
        vec![tag("e", &target)],
        "",
        T0_SECS + 1,
    );
    assert_eq!(submit(&env, &member, &plus).await.status, 200);

    let t = tallies(&env, &owner, &target).await;
    // Groups: "+" and "👍" (ordered by emoji).
    assert_eq!(t.len(), 2, "two emoji groups: {t:?}");
    let thumbs = t.iter().find(|g| g["emoji"] == json!("👍")).unwrap();
    assert_eq!(thumbs["count"], json!(2), "both members thumbed");

    });
}

#[test]
fn severe_reactions_route_denies_deleted_private_messages_gated_kinds_and_chained_reactions() {
    common::run_scenario(|| async move {
        let env = gate_env().await;
        let owner = Actor::new("rxn-sec-owner");
        let stranger = Actor::new("rxn-sec-stranger");
        let channel = "chan-rxn-private";
        seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;

        // 1. Reaction on a private message, then message deleted via NIP-09:
        // materialized rxn/ rows must NOT become world-readable when target_event is None.
        let private_msg = post_message(&env, &owner, channel, "top secret").await;
        let r1 = react(&owner, &private_msg, "🤫", T0_SECS);
        assert_eq!(submit(&env, &owner, &r1).await.status, 200);
        assert!(
            tallies(&env, &stranger, &private_msg).await.is_empty(),
            "stranger denied while live"
        );

        // Also react to the reaction r1 itself (chained reaction has no `h` tag).
        let r1_hex = crypto::to_hex(&r1.id);
        let r2 = react(&owner, &r1_hex, "👀", T0_SECS + 1);
        assert_eq!(submit(&env, &owner, &r2).await.status, 200);
        assert!(
            tallies(&env, &stranger, &r1_hex).await.is_empty(),
            "chained reaction on private-room reaction must not leak to non-member"
        );
        assert_eq!(
            tallies(&env, &owner, &r1_hex).await.len(),
            1,
            "member of private room can read reaction on reaction"
        );

        // Delete the private message via NIP-09.
        let del = signed_at(
            &owner,
            kind::KIND_DELETION,
            vec![tag("e", &private_msg)],
            "",
            T0_SECS + 2,
        );
        assert_eq!(submit(&env, &owner, &del).await.status, 200);
        assert!(
            tallies(&env, &stranger, &private_msg).await.is_empty(),
            "deleted private message reactions must not leak when target_event is None"
        );
        assert!(
            tallies(&env, &stranger, &r1_hex).await.is_empty(),
            "chained reaction whose root private message was deleted must not leak"
        );

        // 2. Channel-less protected event (unshared persona 30175) has channel_id() == None
        // and must still run through access::may_deliver.
        let draft_persona = signed_at(
            &owner,
            kind::KIND_PERSONA,
            vec![tag("d", "secret-persona")],
            "private prompt",
            T0_SECS + 3,
        );
        assert_eq!(submit(&env, &owner, &draft_persona).await.status, 200);
        let persona_hex = crypto::to_hex(&draft_persona.id);
        let rp = react(&owner, &persona_hex, "🔒", T0_SECS + 4);
        assert_eq!(submit(&env, &owner, &rp).await.status, 200);
        assert_eq!(tallies(&env, &owner, &persona_hex).await.len(), 1);
        assert!(
            tallies(&env, &stranger, &persona_hex).await.is_empty(),
            "non-author must not read reactions on an unshared persona"
        );
    });
}
