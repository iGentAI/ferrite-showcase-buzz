//! **M1.8 — moderation write-block, decided by the WIRING.**
//!
//! A community moderator bans or times out a member; the write path then refuses
//! that member's events (fail-closed). Only a configured moderator may issue the
//! command. Cases post real 9040–9043 events and check the durable effect: the
//! target's next write is refused, and lifting the restriction restores it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::Visibility;
use buzz::relay::kind;
use common::{
    Actor, COMMUNITY, T0_MS, T0_SECS, gate_env, seed_channel_with_owner, seed_membership, signed_at,
    submit, tag,
};
use ferrite_app::serde_json::json;
use common::SsoTestEnv as TestEnv;

/// Seed the community-moderator set (the operator-provisioned config key).
async fn seed_moderators(env: &TestEnv, mods: &[&Actor]) {
    let hexes: Vec<String> = mods.iter().map(|a| a.pk_hex()).collect();
    let mut config = env.store("config");
    config
        .put(
            b"buzz-community-moderators",
            json!(hexes).to_string().as_bytes(),
        )
        .await
        .expect("seed moderators");
}

fn ban(mod_actor: &Actor, target: &Actor, kind_: u32, at: u64) -> buzz::relay::event::Event {
    signed_at(mod_actor, kind_, vec![tag("p", &target.pk_hex())], "", at)
}

async fn post_message(env: &TestEnv, actor: &Actor, channel: &str, at: u64) -> u16 {
    let msg = signed_at(
        actor,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "hi",
        at,
    );
    submit(env, actor, &msg).await.status
}

/// A banned member cannot create a channel; the write gate covers the HTTP
/// create route as it covers events and media.
#[test]
fn severe_a_banned_member_cannot_create_a_channel() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("moderator");
    let member = Actor::new("member");
    seed_moderators(&env, &[&moderator]).await;
    assert_eq!(
        submit(
            &env,
            &moderator,
            &ban(&moderator, &member, kind::KIND_MODERATION_BAN, T0_SECS + 1)
        )
        .await
        .status,
        200
    );

    let body = json!({ "id": "banned-room", "name": "banned-room", "type": "stream", "visibility": "open" });
    let reply = env
        .handle(common::post(
            &member,
            "/buzz/api/channels",
            ferrite_app::serde_json::to_vec(&body).expect("body encodes"),
        ))
        .await
        .expect("a create reply");
    assert_eq!(reply.status, 403, "{}", String::from_utf8_lossy(&reply.body));
    });
}

/// A moderator bans a member; the member's next write is refused; unban restores.
#[test]
 fn severe_ban_blocks_writes_and_unban_restores() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("moderator");
    let member = Actor::new("member");
    let channel = "chan-mod";
    seed_moderators(&env, &[&moderator]).await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &moderator).await;
    seed_membership(&env, channel, &member, buzz::relay::channel::Role::Member).await;

    // The member can write before any ban.
    assert_eq!(post_message(&env, &member, channel, T0_SECS).await, 200);

    // The moderator bans the member.
    assert_eq!(
        submit(
            &env,
            &moderator,
            &ban(&moderator, &member, kind::KIND_MODERATION_BAN, T0_SECS + 1)
        )
        .await
        .status,
        200
    );

    // The member's next write is refused, fail-closed.
    assert_eq!(
        post_message(&env, &member, channel, T0_SECS + 2).await,
        403,
        "a banned member cannot write"
    );

    // Unban restores the member's ability to write.
    assert_eq!(
        submit(
            &env,
            &moderator,
            &ban(
                &moderator,
                &member,
                kind::KIND_MODERATION_UNBAN,
                T0_SECS + 3
            )
        )
        .await
        .status,
        200
    );
    assert_eq!(
        post_message(&env, &member, channel, T0_SECS + 4).await,
        200,
        "unban restores writing"
    );

    });
}

/// A non-moderator cannot ban anyone (fail-closed on the moderator set).
#[test]
 fn severe_only_a_moderator_may_ban() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("moderator");
    let pretender = Actor::new("pretender");
    let victim = Actor::new("victim");
    let channel = "chan-mod-auth";
    seed_moderators(&env, &[&moderator]).await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &moderator).await;
    seed_membership(&env, channel, &victim, buzz::relay::channel::Role::Member).await;

    // A non-moderator's ban is refused, and the victim can still write.
    let attempt = ban(&pretender, &victim, kind::KIND_MODERATION_BAN, T0_SECS);
    assert_eq!(
        submit(&env, &pretender, &attempt).await.status,
        403,
        "a non-moderator cannot ban"
    );
    assert_eq!(
        post_message(&env, &victim, channel, T0_SECS + 1).await,
        200,
        "the victim is unaffected"
    );

    });
}

/// A timeout that names no expiry lasts one hour from now, and untimeout lifts it early.
#[test]
 fn severe_a_timeout_with_no_expiry_lasts_one_hour_and_untimeout_lifts_it() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("moderator");
    let member = Actor::new("member");
    let channel = "chan-timeout";
    seed_moderators(&env, &[&moderator]).await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &moderator).await;
    seed_membership(&env, channel, &member, buzz::relay::channel::Role::Member).await;

    let timeout = ban(&moderator, &member, kind::KIND_MODERATION_TIMEOUT, T0_SECS);
    assert_eq!(submit(&env, &moderator, &timeout).await.status, 200);
    assert_eq!(
        post_message(&env, &member, channel, T0_SECS + 1).await,
        403,
        "a timed-out member cannot write"
    );
    let mut events = env.store("events");
    let stored = buzz::relay::effects::restriction_of(&mut events, COMMUNITY, &member.pk)
        .await
        .expect("restriction read")
        .expect("the timeout is recorded");
    assert_eq!(stored.timeout_until, Some(T0_MS + 3_600_000));

    // Untimeout lifts it.
    assert_eq!(
        submit(
            &env,
            &moderator,
            &ban(
                &moderator,
                &member,
                kind::KIND_MODERATION_UNTIMEOUT,
                T0_SECS + 2
            )
        )
        .await
        .status,
        200
    );
    assert_eq!(
        post_message(&env, &member, channel, T0_SECS + 3).await,
        200,
        "untimeout restores writing"
    );

    });
}
