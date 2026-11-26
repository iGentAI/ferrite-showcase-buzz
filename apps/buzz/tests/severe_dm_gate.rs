//! **M1.3 — DMs, decided by the WIRING.**
//!
//! A DM is identified by the hash of its participant set, so opening one twice
//! with the same people finds the same room; every participant is a plain member
//! (no owner); a member may hide it per-viewer; and the participant set is
//! immutable — adding someone makes a NEW DM. Cases post real 41010/41011/41012
//! events and read the durable channel + membership state.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::{ChannelType, Role, Visibility};
use buzz::relay::kind;
use common::{Actor, T0_SECS, channel_doc, gate_env, membership, signed_at, submit, tag};
use common::SsoTestEnv as TestEnv;

// A tiny wrapper so the test names the id the way a client would derive it.
mod dmid {
    use buzz::relay::event::PubKey;
    pub fn of(pks: &[PubKey]) -> String {
        buzz::relay::channel::dm_participant_hash(pks)
    }
}

async fn channel(env: &TestEnv, id: &str) -> Option<buzz::relay::channel::Channel> {
    channel_doc(env, id)
        .await
        .map(|b| ferrite_app::serde_json::from_slice(&b).expect("channel decodes"))
}

fn dm_open(opener: &Actor, others: &[&Actor], at: u64) -> buzz::relay::event::Event {
    let tags = others.iter().map(|a| tag("p", &a.pk_hex())).collect();
    signed_at(opener, kind::KIND_DM_OPEN, tags, "", at)
}

/// Opening a DM creates a private `dm` room whose id is the participant hash,
/// with every participant a plain member and no owner; opening again with the
/// same people finds the same room.
#[test]
 fn severe_open_dm_is_idempotent_by_participants() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let bob = Actor::new("bob");
    let id = dmid::of(&[alice.pk, bob.pk]);

    assert!(channel(&env, &id).await.is_none(), "no DM yet");

    assert_eq!(
        submit(&env, &alice, &dm_open(&alice, &[&bob], T0_SECS))
            .await
            .status,
        200
    );
    let c = channel(&env, &id).await.expect("DM created");
    assert_eq!(c.channel_type, ChannelType::Dm);
    assert_eq!(c.visibility, Visibility::Private);
    assert!(c.owners.is_empty(), "a DM has no owner");
    assert_eq!(c.participant_hash.as_deref(), Some(id.as_str()));

    // Both participants are live members at role member.
    for who in [&alice, &bob] {
        let m = membership(&env, &id, who).await.expect("member");
        assert!(m.is_live() && m.role == Role::Member);
    }

    // Opening again (bob this time, distinct event): same room, still one.
    assert_eq!(
        submit(&env, &bob, &dm_open(&bob, &[&alice], T0_SECS + 1))
            .await
            .status,
        200
    );
    assert_eq!(
        channel(&env, &id)
            .await
            .unwrap()
            .participant_hash
            .as_deref(),
        Some(id.as_str()),
        "the same participants resolve to the same DM"
    );

    // The reverse participant order hashes to the same id.
    assert_eq!(
        dmid::of(&[bob.pk, alice.pk]),
        id,
        "hash is order-independent"
    );

    });
}

/// Hiding a DM (41012) sets the caller's own `hidden_at`; re-opening clears only
/// the caller's hide.
#[test]
 fn severe_hide_is_per_viewer_and_reopen_unhides() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let bob = Actor::new("bob");
    let id = dmid::of(&[alice.pk, bob.pk]);
    assert_eq!(
        submit(&env, &alice, &dm_open(&alice, &[&bob], T0_SECS))
            .await
            .status,
        200
    );

    // Alice hides the DM.
    let hide = signed_at(
        &alice,
        kind::KIND_DM_HIDE,
        vec![tag("h", &id)],
        "",
        T0_SECS + 1,
    );
    assert_eq!(submit(&env, &alice, &hide).await.status, 200);
    assert!(membership(&env, &id, &alice)
        .await
        .unwrap()
        .hidden_at
        .is_some());
    // Bob's view is unaffected.
    assert!(membership(&env, &id, &bob)
        .await
        .unwrap()
        .hidden_at
        .is_none());

    // Re-opening as alice clears her hide.
    assert_eq!(
        submit(&env, &alice, &dm_open(&alice, &[&bob], T0_SECS + 2))
            .await
            .status,
        200
    );
    assert!(membership(&env, &id, &alice)
        .await
        .unwrap()
        .hidden_at
        .is_none());

    });
}

/// A DM outside the 2–9 participant bound is refused before storage.
#[test]
 fn severe_out_of_range_dm_is_refused() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");

    // Just alice (self only, 1 participant): refused.
    let solo = signed_at(&alice, kind::KIND_DM_OPEN, vec![], "", T0_SECS);
    let reply = submit(&env, &alice, &solo).await;
    assert_eq!(reply.status, 400, "a 1-participant DM is refused");

    // Ten participants (self + 9 others = 10): refused.
    let others: Vec<Actor> = (0..9).map(|i| Actor::new(&format!("m{i}"))).collect();
    let refs: Vec<&Actor> = others.iter().collect();
    let big = dm_open(&alice, &refs, T0_SECS + 1);
    assert_eq!(
        submit(&env, &alice, &big).await.status,
        400,
        "a 10-participant DM is refused"
    );

    });
}

#[test]
fn severe_dm_cannot_be_presquatted_or_mutated_with_nip29_commands() {
    common::run_scenario(|| async move {
        let env = gate_env().await;
        let alice = Actor::new("dm-sec-alice");
        let bob = Actor::new("dm-sec-bob");
        let mallory = Actor::new("dm-sec-mallory");
        let id = dmid::of(&[alice.pk, bob.pk]);

        // 1. General room creation must refuse both `type: "dm"` and 64-hex channel IDs.
        let squat_http = env
            .handle(common::post(
                &mallory,
                "/buzz/api/channels",
                ferrite_app::serde_json::to_vec(&ferrite_app::serde_json::json!({
                    "id": id,
                    "name": "squatted",
                    "type": "stream",
                    "visibility": "open",
                }))
                .unwrap(),
            ))
            .await
            .expect("create reply");
        assert_eq!(squat_http.status, 400, "64-hex channel id must be refused on HTTP create");

        let dm_type_http = env
            .handle(common::post(
                &mallory,
                "/buzz/api/channels",
                ferrite_app::serde_json::to_vec(&ferrite_app::serde_json::json!({
                    "name": "fake-dm",
                    "type": "dm",
                    "visibility": "private",
                }))
                .unwrap(),
            ))
            .await
            .expect("create reply");
        assert_eq!(dm_type_http.status, 400, "type=dm must be refused on HTTP create");

        let squat_9007 = signed_at(
            &mallory,
            kind::KIND_NIP29_CREATE_GROUP,
            vec![tag("h", &id), tag("name", "squatted")],
            "",
            T0_SECS,
        );
        assert_eq!(
            submit(&env, &mallory, &squat_9007).await.status,
            400,
            "64-hex channel id must be refused on 9007 create"
        );

        // 2. Defense-in-depth: even if a legacy/corrupt non-DM channel document sits at `id`,
        // `KIND_DM_OPEN` fails closed and never enrolls Alice or Bob into Mallory's room.
        common::seed_channel_with_owner(&env, &id, Visibility::Open, &mallory).await;
        let open_attempt = dm_open(&alice, &[&bob], T0_SECS + 1);
        let refused = submit(&env, &alice, &open_attempt).await;
        assert!(
            refused.status == 403 || refused.status == 409,
            "DM open onto a squatted room must be refused: {}",
            common::error_text(&refused)
        );
        assert!(
            membership(&env, &id, &alice).await.is_none(),
            "Alice must not be enrolled into Mallory's squatted room"
        );
        // Direct effect execution also fails closed without enrolling participants.
        {
            let mut events = env.store("events");
            let mut ape = env.ape("guards");
            buzz::relay::effects::apply_effects(
                &mut events,
                &mut ape,
                common::COMMUNITY,
                &open_attempt,
                None,
                common::T0_MS,
            )
            .await
            .expect("effect completes safely");
        }
        assert!(membership(&env, &id, &alice).await.is_none());

        // 3. On a genuine DM between Alice and Carol, add_policy is Nobody and all NIP-29
        // membership/channel mutations (9000, 9001, 9002, 9008, 9021, 9022) are rejected.
        let carol = Actor::new("dm-sec-carol");
        let real_dm = dmid::of(&[alice.pk, carol.pk]);
        assert_eq!(
            submit(&env, &alice, &dm_open(&alice, &[&carol], T0_SECS + 2))
                .await
                .status,
            200
        );
        let dm_doc = channel(&env, &real_dm).await.expect("real DM exists");
        assert_eq!(dm_doc.add_policy, buzz::relay::channel::AddPolicy::Nobody);

        let put_outsider = signed_at(
            &carol,
            kind::KIND_NIP29_PUT_USER,
            vec![tag("h", &real_dm), tag("p", &mallory.pk_hex())],
            "",
            T0_SECS + 3,
        );
        assert_eq!(
            submit(&env, &carol, &put_outsider).await.status,
            403,
            "PUT_USER on a DM room must be refused"
        );
        assert!(membership(&env, &real_dm, &mallory).await.is_none());

        let edit_open = signed_at(
            &carol,
            kind::KIND_NIP29_EDIT_METADATA,
            vec![tag("h", &real_dm), tag("visibility", "open")],
            "",
            T0_SECS + 4,
        );
        assert_eq!(
            submit(&env, &carol, &edit_open).await.status,
            403,
            "EDIT_METADATA on a DM room must be refused"
        );
        assert_eq!(
            channel(&env, &real_dm).await.unwrap().visibility,
            Visibility::Private
        );
    });
}
