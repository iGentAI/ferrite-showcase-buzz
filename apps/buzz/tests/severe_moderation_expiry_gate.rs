//! **A requested moderation expiry must be honored, not silently discarded.**
//!
//! The wire contract is NIP-40: the client sends `["expiration", <epoch-SECS>]`
//! on both a timeout (9042) and a temporary ban (9040) — `buzz-web`'s
//! `timeoutMember`/`banMember` and the kind declarations here agree. The relay
//! read `["until", <epoch-MS>]` instead, a tag no client ever sent, so a
//! moderator picking "7 days" silently got the one-hour default — and a
//! temporary BAN recorded no expiry at all, making it permanent.
//!
//! These gates drive multi-day expiries through the real event route and assert
//! the DURABLE restriction state matches what was requested. The assertions
//! read the stored document as raw JSON deliberately: the gate must compile and
//! run (and fail) against the pre-fix state, where the ban expiry field did not
//! exist.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::{Role, Visibility};
use buzz::relay::{keys, kind, store as buzz_store};
use common::{
    Actor, COMMUNITY, T0_SECS, gate_env, seed_channel_with_owner, seed_membership, signed_at, submit,
    tag,
};
use ferrite_app::serde_json::{self, json, Value};
use common::SsoTestEnv as TestEnv;

const DAY_SECS: u64 = 86_400;

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

/// The target's durable restriction document, as raw JSON.
async fn restriction_json(env: &TestEnv, target: &Actor) -> Value {
    let mut events = env.store("events");
    let raw = buzz_store::get_raw(&mut events, &keys::restriction(COMMUNITY, &target.pk))
        .await
        .expect("store read")
        .expect("a restriction document was stored");
    serde_json::from_slice(&raw).expect("restriction decodes")
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

/// A 7-day timeout requested via the wire contract (`expiration`, epoch-secs)
/// is stored as exactly that instant — not the one-hour default.
#[test]
 fn severe_a_timeout_expiration_is_honored_not_defaulted() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("expiry-moderator");
    let member = Actor::new("expiry-member");
    let channel = "chan-timeout-expiry";
    seed_moderators(&env, &[&moderator]).await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &moderator).await;
    seed_membership(&env, channel, &member, Role::Member).await;

    let expires_at_secs = T0_SECS + 7 * DAY_SECS;
    let timeout = signed_at(
        &moderator,
        kind::KIND_MODERATION_TIMEOUT,
        vec![
            tag("p", &member.pk_hex()),
            tag("expiration", &expires_at_secs.to_string()),
        ],
        "",
        T0_SECS,
    );
    assert_eq!(submit(&env, &moderator, &timeout).await.status, 200);
    assert_eq!(
        post_message(&env, &member, channel, T0_SECS + 1).await,
        403,
        "a timed-out member cannot write"
    );

    let doc = restriction_json(&env, &member).await;
    assert_eq!(
        doc.get("timeout_until").and_then(Value::as_u64),
        Some(expires_at_secs * 1000),
        "the moderator asked for 7 days and the relay stored something else: {doc}"
    );

    });
}

/// A temporary ban records its requested expiry; a ban with no expiry stays
/// permanent. Pre-fix the expiry was silently dropped, making every ban
/// permanent until a manual unban.
#[test]
 fn severe_a_temporary_ban_records_its_expiry() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("ban-expiry-moderator");
    let temp_target = Actor::new("temporarily-banned");
    let perm_target = Actor::new("permanently-banned");
    let channel = "chan-ban-expiry";
    seed_moderators(&env, &[&moderator]).await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &moderator).await;
    seed_membership(&env, channel, &temp_target, Role::Member).await;
    seed_membership(&env, channel, &perm_target, Role::Member).await;

    let expires_at_secs = T0_SECS + 3 * DAY_SECS;
    let temp_ban = signed_at(
        &moderator,
        kind::KIND_MODERATION_BAN,
        vec![
            tag("p", &temp_target.pk_hex()),
            tag("expiration", &expires_at_secs.to_string()),
        ],
        "",
        T0_SECS,
    );
    assert_eq!(submit(&env, &moderator, &temp_ban).await.status, 200);
    assert_eq!(
        post_message(&env, &temp_target, channel, T0_SECS + 1).await,
        403,
        "the ban blocks writes while it is live"
    );
    let doc = restriction_json(&env, &temp_target).await;
    assert_eq!(
        doc.get("banned").and_then(Value::as_bool),
        Some(true),
        "{doc}"
    );
    assert_eq!(
        doc.get("banned_until").and_then(Value::as_u64),
        Some(expires_at_secs * 1000),
        "a TEMPORARY ban stored no expiry — it is permanent: {doc}"
    );

    // No `expiration` tag = a permanent ban, exactly as before.
    let perm_ban = signed_at(
        &moderator,
        kind::KIND_MODERATION_BAN,
        vec![tag("p", &perm_target.pk_hex())],
        "",
        T0_SECS,
    );
    assert_eq!(submit(&env, &moderator, &perm_ban).await.status, 200);
    let doc = restriction_json(&env, &perm_target).await;
    assert_eq!(
        doc.get("banned").and_then(Value::as_bool),
        Some(true),
        "{doc}"
    );
    assert_eq!(
        doc.get("banned_until"),
        None,
        "a permanent ban must not carry an expiry: {doc}"
    );

    });
}
