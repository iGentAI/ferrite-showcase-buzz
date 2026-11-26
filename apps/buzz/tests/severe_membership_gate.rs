//! **M1.2 — membership materialization, decided by the WIRING.**
//!
//! `effects::authorize_membership_command` has its own unit tests; they prove the
//! rule table is right about facts handed to it. The property that keeps a
//! private room owned and its roster honest is different: *the route authorizes
//! the command on every submission, and the ingest saga materializes the `mem/`
//! row and the channel owner-set exactly once.* So every case here submits a real
//! signed 9000/9001/9021/9022 through `POST /buzz/api/events` on a live fleet and
//! asserts on the durable state the saga left behind — the `mem/` row and the
//! channel document's `owners` set — not on a log line.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::{Role, Visibility};
use buzz::relay::kind;
use common::{
    Actor, T0_SECS, channel_owners, gate_env, error_text, membership, seed_channel_with_owner,
    seed_membership, signed, signed_at, submit, tag,
};

/// `h` + `p` (+ optional `role`) tag set for a membership command.
fn member_tags(channel: &str, target: &Actor, role: Option<Role>) -> Vec<Vec<String>> {
    let mut tags = vec![tag("h", channel), tag("p", &target.pk_hex())];
    if let Some(role) = role {
        tags.push(tag("role", role.as_str()));
    }
    tags
}

/// An owner adds a member (9000): the `mem/` row materializes live, at the
/// granted role, and re-driving the same event does not change it.
#[test]
 fn severe_owner_adds_a_member() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let newcomer = Actor::new("newcomer");
    let channel = "chan-add";
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;

    // Before: not a member.
    assert!(membership(&env, channel, &newcomer).await.is_none());

    let add = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &newcomer, Some(Role::Member)),
        "",
    );
    let reply = submit(&env, &owner, &add).await;
    assert_eq!(reply.status, 200, "add accepted: {}", error_text(&reply));

    let m = membership(&env, channel, &newcomer)
        .await
        .expect("membership row materialized");
    assert!(m.is_live() && m.role == Role::Member);
    // The newcomer is not an owner.
    assert!(!channel_owners(&env, channel)
        .await
        .contains(&newcomer.pk_hex()));

    // Re-submitting the identical event is absorbed (saga idempotency): still one
    // live member row, unchanged.
    let reply = submit(&env, &owner, &add).await;
    assert_eq!(reply.status, 200);
    let again = membership(&env, channel, &newcomer).await.unwrap();
    assert_eq!(again.role, Role::Member);
    assert!(again.is_live());

    });
}

/// A non-member cannot add anyone to a private room (rejected before storage),
/// and a non-owner cannot grant an elevated role.
#[test]
 fn severe_add_authorization_is_enforced() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let member = Actor::new("member");
    let stranger = Actor::new("stranger");
    let victim = Actor::new("victim");
    let channel = "chan-auth";
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;
    seed_membership(&env, channel, &member, Role::Member).await;

    // A stranger (no membership) adding someone: refused.
    let by_stranger = signed(
        &stranger,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &victim, Some(Role::Member)),
        "",
    );
    let reply = submit(&env, &stranger, &by_stranger).await;
    assert_eq!(reply.status, 403, "stranger add must be refused");
    assert!(membership(&env, channel, &victim).await.is_none());

    // A plain member granting an ELEVATED role: refused.
    let elevate = signed(
        &member,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &victim, Some(Role::Admin)),
        "",
    );
    let reply = submit(&env, &member, &elevate).await;
    assert_eq!(reply.status, 403, "member cannot grant admin");
    assert!(membership(&env, channel, &victim).await.is_none());

    });
}

/// An owner removes a member (9001): soft removal (`removed_at` set, row kept),
/// and the removed member reads as not-live.
#[test]
 fn severe_owner_removes_a_member_softly() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let member = Actor::new("member");
    let channel = "chan-remove";
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;
    seed_membership(&env, channel, &member, Role::Member).await;

    let remove = signed(
        &owner,
        kind::KIND_NIP29_REMOVE_USER,
        member_tags(channel, &member, None),
        "",
    );
    let reply = submit(&env, &owner, &remove).await;
    assert_eq!(reply.status, 200, "remove accepted: {}", error_text(&reply));

    let m = membership(&env, channel, &member)
        .await
        .expect("the row is kept — removal is soft");
    assert!(!m.is_live(), "removed member is not live");
    assert!(m.removed_at.is_some());

    });
}

/// The last-owner guard: neither removing nor demoting the sole owner is allowed
/// — the port's deliberate divergence from buzz (an ownerless room here has no
/// operator to recover it).
#[test]
 fn severe_the_last_owner_cannot_be_removed_or_demoted() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-lastowner";
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;

    // Self-remove of the sole owner (9001 targeting self): refused.
    let self_remove = signed(
        &owner,
        kind::KIND_NIP29_REMOVE_USER,
        member_tags(channel, &owner, None),
        "",
    );
    let reply = submit(&env, &owner, &self_remove).await;
    assert_eq!(reply.status, 403, "cannot remove the last owner");
    assert!(channel_owners(&env, channel)
        .await
        .contains(&owner.pk_hex()));
    assert!(membership(&env, channel, &owner).await.unwrap().is_live());

    // Leave (9022) by the sole owner: refused for the same reason.
    let leave = signed(
        &owner,
        kind::KIND_NIP29_LEAVE_REQUEST,
        vec![tag("h", channel)],
        "",
    );
    let reply = submit(&env, &owner, &leave).await;
    assert_eq!(reply.status, 403, "the last owner cannot leave");

    // Demote (9000 role=member on self): refused — the divergence.
    let demote = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &owner, Some(Role::Member)),
        "",
    );
    let reply = submit(&env, &owner, &demote).await;
    assert_eq!(reply.status, 403, "the last owner cannot be demoted");
    assert!(channel_owners(&env, channel)
        .await
        .contains(&owner.pk_hex()));

    // With a SECOND owner present, the first owner may now leave.
    let owner2 = Actor::new("owner2");
    let promote = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &owner2, Some(Role::Owner)),
        "",
    );
    assert_eq!(submit(&env, &owner, &promote).await.status, 200);
    assert!(channel_owners(&env, channel)
        .await
        .contains(&owner2.pk_hex()));

    let leave_again = signed(
        &owner,
        kind::KIND_NIP29_LEAVE_REQUEST,
        vec![tag("h", channel)],
        "",
    );
    assert_eq!(
        submit(&env, &owner, &leave_again).await.status,
        200,
        "with two owners, one may leave"
    );
    let owners = channel_owners(&env, channel).await;
    assert!(owners.contains(&owner2.pk_hex()) && !owners.contains(&owner.pk_hex()));

    });
}

#[test]
 fn severe_a_torn_owner_removal_does_not_let_the_next_one_empty_the_room() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let owner2 = Actor::new("owner2");
    let channel = "chan-torn-owner";
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;
    let promote = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &owner2, Some(Role::Owner)),
        "",
    );
    assert_eq!(submit(&env, &owner, &promote).await.status, 200);

    // The first owner's removal committed its membership row and failed before
    // the owner-set write: soft-removed, still in the set.
    let mut events = env.store("events");
    let key = buzz::relay::keys::mem(common::COMMUNITY, channel, &owner.pk).expect("safe id");
    let mut row: buzz::relay::channel::Membership =
        buzz::relay::store::get_doc(&mut events, &key)
            .await
            .expect("read")
            .expect("the owner's row");
    row.removed_at = Some(common::T0_MS);
    buzz::relay::store::put_doc(&mut events, &key, &row)
        .await
        .expect("torn removal");
    assert!(channel_owners(&env, channel).await.contains(&owner.pk_hex()));

    let leave = signed(
        &owner2,
        kind::KIND_NIP29_LEAVE_REQUEST,
        vec![tag("h", channel)],
        "",
    );
    let _ = submit(&env, &owner2, &leave).await;
    assert!(
        membership(&env, channel, &owner2).await.unwrap().is_live(),
        "the last live owner must stay a live member"
    );

    });
}

async fn tear_owner_removal(env: &common::SsoTestEnv, channel: &str, owner: &Actor) {
    let mut events = env.store("events");
    let key = buzz::relay::keys::mem(common::COMMUNITY, channel, &owner.pk).expect("safe id");
    let mut row: buzz::relay::channel::Membership =
        buzz::relay::store::get_doc(&mut events, &key)
            .await
            .expect("read")
            .expect("the owner's row");
    row.removed_at = Some(common::T0_MS);
    buzz::relay::store::put_doc(&mut events, &key, &row)
        .await
        .expect("torn removal");
}

async fn room_with_two_owners(env: &common::SsoTestEnv, channel: &str, owner: &Actor, owner2: &Actor) {
    seed_channel_with_owner(env, channel, Visibility::Private, owner).await;
    let promote = signed(
        owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, owner2, Some(Role::Owner)),
        "",
    );
    assert_eq!(submit(env, owner, &promote).await.status, 200);
}

#[test]
fn severe_a_torn_owner_removal_does_not_let_a_demotion_take_the_last_live_owner() {
    common::run_scenario(|| async move {
        let env = gate_env().await;
        let owner = Actor::new("owner");
        let owner2 = Actor::new("owner2");
        let channel = "chan-torn-demote";
        room_with_two_owners(&env, channel, &owner, &owner2).await;
        tear_owner_removal(&env, channel, &owner).await;

        let demote = signed(
            &owner2,
            kind::KIND_NIP29_PUT_USER,
            member_tags(channel, &owner2, Some(Role::Member)),
            "",
        );
        let _ = submit(&env, &owner2, &demote).await;
        let row = membership(&env, channel, &owner2).await.unwrap();
        assert!(row.is_live());
        assert_eq!(row.role, Role::Owner, "the last live owner must keep the Owner role");
        assert!(channel_owners(&env, channel).await.contains(&owner2.pk_hex()));
    });
}

#[test]
fn severe_a_torn_grant_does_not_let_a_demotion_take_the_last_live_owner() {
    common::run_scenario(|| async move {
        let env = gate_env().await;
        let owner = Actor::new("owner");
        let owner2 = Actor::new("owner2");
        let channel = "chan-torn-grant-demote";
        room_with_two_owners(&env, channel, &owner, &owner2).await;
        let mut events = env.store("events");
        let key = buzz::relay::keys::mem(common::COMMUNITY, channel, &owner.pk).expect("safe id");
        let mut row: buzz::relay::channel::Membership =
            buzz::relay::store::get_doc(&mut events, &key)
                .await
                .expect("read")
                .expect("the owner's row");
        row.role = Role::Member;
        buzz::relay::store::put_doc(&mut events, &key, &row)
            .await
            .expect("torn grant");
        assert!(channel_owners(&env, channel).await.contains(&owner.pk_hex()));

        let demote = signed(
            &owner2,
            kind::KIND_NIP29_PUT_USER,
            member_tags(channel, &owner2, Some(Role::Member)),
            "",
        );
        let _ = submit(&env, &owner2, &demote).await;
        let row = membership(&env, channel, &owner2).await.unwrap();
        assert!(row.is_live());
        assert_eq!(row.role, Role::Owner, "the last live owner must keep the Owner role");
        assert!(channel_owners(&env, channel).await.contains(&owner2.pk_hex()));
    });
}

#[test]
fn severe_a_torn_owner_removal_does_not_let_a_removal_take_the_last_live_owner() {
    common::run_scenario(|| async move {
        let env = gate_env().await;
        let owner = Actor::new("owner");
        let owner2 = Actor::new("owner2");
        let channel = "chan-torn-remove";
        room_with_two_owners(&env, channel, &owner, &owner2).await;
        tear_owner_removal(&env, channel, &owner).await;

        let remove = signed(
            &owner2,
            kind::KIND_NIP29_REMOVE_USER,
            member_tags(channel, &owner2, None),
            "",
        );
        let _ = submit(&env, &owner2, &remove).await;
        let row = membership(&env, channel, &owner2).await.unwrap();
        assert!(row.is_live(), "the last live owner must stay a live member");
        assert_eq!(row.role, Role::Owner);
        assert!(channel_owners(&env, channel).await.contains(&owner2.pk_hex()));
    });
}

 #[test]
 fn severe_a_torn_grant_does_not_make_the_next_removal_empty_the_room() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let owner2 = Actor::new("owner2");
    let channel = "chan-torn-grant";
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;
    let promote = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &owner2, Some(Role::Owner)),
        "",
    );
    assert_eq!(submit(&env, &owner, &promote).await.status, 200);

    // The first owner is in the owner set but their row is a live non-owner row,
    // the state a grant leaves when its second write fails.
    let mut events = env.store("events");
    let key = buzz::relay::keys::mem(common::COMMUNITY, channel, &owner.pk).expect("safe id");
    let mut row: buzz::relay::channel::Membership =
        buzz::relay::store::get_doc(&mut events, &key)
            .await
            .expect("read")
            .expect("the owner's row");
    row.role = Role::Member;
    buzz::relay::store::put_doc(&mut events, &key, &row)
        .await
        .expect("torn grant");
    assert!(channel_owners(&env, channel).await.contains(&owner.pk_hex()));

    let leave = signed(
        &owner2,
        kind::KIND_NIP29_LEAVE_REQUEST,
        vec![tag("h", channel)],
        "",
    );
    let _ = submit(&env, &owner2, &leave).await;
    assert!(
        membership(&env, channel, &owner2).await.unwrap().is_live(),
        "a set entry without an Owner row is not a second owner"
    );

    });
}

/// **Adversarial-review regression (CRITICAL): no self-granted ownership.** A
/// non-member of an open channel who posts a 9000 naming their own pubkey with
/// role=owner must be refused — otherwise it is a full channel takeover (they
/// become an owner and can then remove the real owner). The benign self-join at
/// role=member still works.
#[test]
 fn severe_no_self_granted_ownership() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let attacker = Actor::new("attacker");
    let channel = "chan-takeover";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;

    // Attacker (not even a member) tries to make themselves owner.
    let grab = signed(
        &attacker,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &attacker, Some(Role::Owner)),
        "",
    );
    let reply = submit(&env, &attacker, &grab).await;
    assert_eq!(reply.status, 403, "self-grant of owner must be refused");
    // The attacker is neither an owner nor a member.
    assert!(!channel_owners(&env, channel)
        .await
        .contains(&attacker.pk_hex()));
    assert!(membership(&env, channel, &attacker).await.is_none());
    // The real owner is untouched.
    assert!(channel_owners(&env, channel)
        .await
        .contains(&owner.pk_hex()));

    // The same attacker self-joining at role=member IS allowed (benign join).
    let join = signed(
        &attacker,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &attacker, Some(Role::Member)),
        "",
    );
    assert_eq!(submit(&env, &attacker, &join).await.status, 200);
    let m = membership(&env, channel, &attacker)
        .await
        .expect("joined as member");
    assert!(m.is_live() && m.role == Role::Member);
    assert!(!channel_owners(&env, channel)
        .await
        .contains(&attacker.pk_hex()));

    });
}

/// Join (9021) works only on open channels; a private room refuses the
/// self-request, and an open room admits the requester as a member.
#[test]
 fn severe_join_is_open_channels_only() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let joiner = Actor::new("joiner");

    // Private: refused, no membership created.
    let private = "chan-private";
    seed_channel_with_owner(&env, private, Visibility::Private, &owner).await;
    let join_private = signed(
        &joiner,
        kind::KIND_NIP29_JOIN_REQUEST,
        vec![tag("h", private)],
        "",
    );
    let reply = submit(&env, &joiner, &join_private).await;
    assert_eq!(reply.status, 403, "cannot self-join a private channel");
    assert!(membership(&env, private, &joiner).await.is_none());

    // Open: admitted as a plain member.
    let open = "chan-open";
    seed_channel_with_owner(&env, open, Visibility::Open, &owner).await;
    let join_open = signed(
        &joiner,
        kind::KIND_NIP29_JOIN_REQUEST,
        vec![tag("h", open)],
        "",
    );
    let reply = submit(&env, &joiner, &join_open).await;
    assert_eq!(
        reply.status,
        200,
        "open join accepted: {}",
        error_text(&reply)
    );
    let m = membership(&env, open, &joiner).await.expect("joined");
    assert!(m.is_live() && m.role == Role::Member);
    assert!(!channel_owners(&env, open).await.contains(&joiner.pk_hex()));

    });
}

/// Re-adding a removed member revives the same row (buzz's un-remove), and a
/// promotion followed by a demotion moves the owner set both ways.
#[test]
 fn severe_readd_revives_and_role_changes_move_the_owner_set() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let member = Actor::new("member");
    let channel = "chan-revive";
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;
    seed_membership(&env, channel, &member, Role::Member).await;

    // Distinct `created_at` per mutation so each is a distinct event id — two
    // byte-identical commands would collide and the second would be deduped by
    // the saga (which is correct, but not what this ordering test wants to see).
    // Remove, then re-add: the row is revived live.
    let remove = signed_at(
        &owner,
        kind::KIND_NIP29_REMOVE_USER,
        member_tags(channel, &member, None),
        "",
        T0_SECS,
    );
    assert_eq!(submit(&env, &owner, &remove).await.status, 200);
    assert!(!membership(&env, channel, &member).await.unwrap().is_live());

    let readd = signed_at(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &member, Some(Role::Admin)),
        "",
        T0_SECS + 1,
    );
    assert_eq!(submit(&env, &owner, &readd).await.status, 200);
    let m = membership(&env, channel, &member).await.unwrap();
    assert!(
        m.is_live() && m.role == Role::Admin,
        "revived at the new role"
    );

    // Promote to owner: the owner set gains them. Then demote back to admin: the
    // owner set drops them (safe now that `owner` is still an owner).
    let promote = signed_at(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &member, Some(Role::Owner)),
        "",
        T0_SECS + 2,
    );
    assert_eq!(submit(&env, &owner, &promote).await.status, 200);
    assert!(channel_owners(&env, channel)
        .await
        .contains(&member.pk_hex()));

    let demote = signed_at(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &member, Some(Role::Admin)),
        "",
        T0_SECS + 3,
    );
    assert_eq!(submit(&env, &owner, &demote).await.status, 200);
    let owners = channel_owners(&env, channel).await;
    assert!(!owners.contains(&member.pk_hex()) && owners.contains(&owner.pk_hex()));
    assert_eq!(
        membership(&env, channel, &member).await.unwrap().role,
        Role::Admin
    );

    });
}

#[test]
fn severe_effect_time_authorization_blocks_toctou_readmission_and_privilege_escalation() {
    common::run_scenario(|| async move {
        let env = gate_env().await;
        let owner = Actor::new("toctou-owner");
        let admin_a = Actor::new("toctou-admin-a");
        let admin_b = Actor::new("toctou-admin-b");
        let member = Actor::new("toctou-member");
        let accomplice = Actor::new("toctou-accomplice");
        let channel = "chan-toctou";

        seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;
        seed_membership(&env, channel, &admin_a, Role::Admin).await;
        seed_membership(&env, channel, &member, Role::Member).await;

        // 1. Member admitted PUT_USER(self, "member") and PUT_USER(accomplice, "member")
        // before being removed by the owner; when effects run after removal, neither takes effect.
        let self_readmit = signed_at(
            &member,
            kind::KIND_NIP29_PUT_USER,
            member_tags(channel, &member, Some(Role::Member)),
            "",
            T0_SECS + 1,
        );
        let add_accomplice = signed_at(
            &member,
            kind::KIND_NIP29_PUT_USER,
            member_tags(channel, &accomplice, Some(Role::Member)),
            "",
            T0_SECS + 2,
        );
        let kick_member = signed_at(
            &owner,
            kind::KIND_NIP29_REMOVE_USER,
            member_tags(channel, &member, None),
            "",
            T0_SECS + 3,
        );
        assert_eq!(submit(&env, &owner, &kick_member).await.status, 200);
        assert!(!membership(&env, channel, &member).await.unwrap().is_live());

        {
            let mut events = env.store("events");
            let mut ape = env.ape("guards");
            for ev in [&self_readmit, &add_accomplice] {
                buzz::relay::effects::apply_effects(
                    &mut events,
                    &mut ape,
                    common::COMMUNITY,
                    ev,
                    Some(channel),
                    common::T0_MS + 100,
                )
                .await
                .expect("raced effect completes as no-op");
            }
        }
        assert!(
            !membership(&env, channel, &member).await.unwrap().is_live(),
            "expelled member must not self-readmit via raced PUT_USER"
        );
        assert!(
            membership(&env, channel, &accomplice).await.is_none(),
            "expelled member must not add an accomplice via raced PUT_USER"
        );

        // 2. Demoted/removed moderator's in-flight REMOVE_USER must not kick live members.
        seed_membership(&env, channel, &member, Role::Member).await;
        let raced_kick = signed_at(
            &admin_a,
            kind::KIND_NIP29_REMOVE_USER,
            member_tags(channel, &member, None),
            "",
            T0_SECS + 4,
        );
        // Owner demotes admin_a to Member before raced_kick's effect executes.
        seed_membership(&env, channel, &admin_a, Role::Member).await;
        {
            let mut events = env.store("events");
            let mut ape = env.ape("guards");
            buzz::relay::effects::apply_effects(
                &mut events,
                &mut ape,
                common::COMMUNITY,
                &raced_kick,
                Some(channel),
                common::T0_MS + 200,
            )
            .await
            .expect("demoted moderator kick completes as no-op");
        }
        assert!(
            membership(&env, channel, &member).await.unwrap().is_live(),
            "demoted moderator's raced REMOVE_USER must not remove a member"
        );

        // 3. Admin's raced PUT_USER(target, "member") or REMOVE_USER(target) after Owner
        // promoted target to Owner must not demote or remove the new Owner.
        seed_membership(&env, channel, &admin_a, Role::Admin).await;
        let promote_member = signed_at(
            &owner,
            kind::KIND_NIP29_PUT_USER,
            member_tags(channel, &member, Some(Role::Owner)),
            "",
            T0_SECS + 5,
        );
        assert_eq!(submit(&env, &owner, &promote_member).await.status, 200);
        let admin_demotes_owner = signed_at(
            &admin_a,
            kind::KIND_NIP29_PUT_USER,
            member_tags(channel, &member, Some(Role::Member)),
            "",
            T0_SECS + 6,
        );
        let admin_removes_owner = signed_at(
            &admin_a,
            kind::KIND_NIP29_REMOVE_USER,
            member_tags(channel, &member, None),
            "",
            T0_SECS + 7,
        );
        {
            let mut events = env.store("events");
            let mut ape = env.ape("guards");
            for ev in [&admin_demotes_owner, &admin_removes_owner] {
                buzz::relay::effects::apply_effects(
                    &mut events,
                    &mut ape,
                    common::COMMUNITY,
                    ev,
                    Some(channel),
                    common::T0_MS + 300,
                )
                .await
                .expect("admin vs owner race completes as no-op");
            }
        }
        assert!(channel_owners(&env, channel).await.contains(&member.pk_hex()));
        let member_row = membership(&env, channel, &member).await.unwrap();
        assert!(
            member_row.is_live() && member_row.role == Role::Owner,
            "admin must not demote or remove a concurrently promoted owner"
        );

        // 4. Two admins racing PUT_USER(self, "owner") on an ownerless channel: only the first wins.
        let ownerless = "chan-ownerless";
        {
            let mut doc = buzz::relay::channel::Channel::new(
                ownerless.into(),
                ownerless.into(),
                buzz::relay::channel::ChannelType::Stream,
                Visibility::Private,
                &owner.pk,
                common::T0_MS,
            );
            doc.owners.clear();
            let key = buzz::relay::keys::guard_channel(common::COMMUNITY, ownerless).unwrap();
            let bytes = ferrite_app::serde_json::to_vec(&doc).unwrap();
            let mut ape = env.ape("guards");
            ape.commit_n_if(&key, None, &[(key.as_slice(), bytes.as_slice())])
                .await
                .expect("seed ownerless")
                .expect("created");
        }
        seed_membership(&env, ownerless, &admin_a, Role::Admin).await;
        seed_membership(&env, ownerless, &admin_b, Role::Admin).await;
        let claim_a = signed_at(
            &admin_a,
            kind::KIND_NIP29_PUT_USER,
            member_tags(ownerless, &admin_a, Some(Role::Owner)),
            "",
            T0_SECS + 8,
        );
        let claim_b = signed_at(
            &admin_b,
            kind::KIND_NIP29_PUT_USER,
            member_tags(ownerless, &admin_b, Some(Role::Owner)),
            "",
            T0_SECS + 9,
        );
        {
            let mut events = env.store("events");
            let mut ape = env.ape("guards");
            buzz::relay::effects::apply_effects(
                &mut events,
                &mut ape,
                common::COMMUNITY,
                &claim_a,
                Some(ownerless),
                common::T0_MS + 400,
            )
            .await
            .expect("first admin claims owner");
            buzz::relay::effects::apply_effects(
                &mut events,
                &mut ape,
                common::COMMUNITY,
                &claim_b,
                Some(ownerless),
                common::T0_MS + 401,
            )
            .await
            .expect("second admin loses race");
        }
        let final_owners = channel_owners(&env, ownerless).await;
        assert_eq!(final_owners.len(), 1, "only one admin may claim an ownerless room");
        assert!(final_owners.contains(&admin_a.pk_hex()));
        assert_eq!(
            membership(&env, ownerless, &admin_b).await.unwrap().role,
            Role::Admin
        );
    });
}
