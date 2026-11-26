//! **W8 — invites and join-policy, decided by the WIRING.**
//!
//! Stateless HMAC invite codes over the deployed routes: minting is a
//! community-authority action (the operator's moderator set, fail-closed),
//! claiming is signed by the JOINING pubkey and materializes durable
//! community membership exactly once; forged/expired/foreign codes are
//! refused coarsely; a configured join policy makes the invite-bound
//! acceptance receipt mandatory; and the opt-in closed-community mode turns
//! that membership into a real write gate. Assertions on durable state and
//! the routes' replies, never logs.
//!
//! Every refusal here is **403**, not 401. Each of these callers authenticates
//! perfectly — a fresh, correctly bound NIP-98 header — and is refused because of
//! what they lack: mint authority, a valid invite, a bound receipt, or claimed
//! membership. None of that can be fixed by authenticating again.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::Visibility;
use buzz::relay::{invites, keys, kind};
use common::{
    Actor, COMMUNITY, T0_SECS, body_json, gate_env, post, seed_channel_with_owner, signed_at,
    stored_event_count, submit, tag,
};
use ferrite_app::serde_json::{json, Value};
use common::SsoTestEnv as TestEnv;

const MINT_PATH: &str = "/buzz/api/invites";
const CLAIM_PATH: &str = "/buzz/api/invites/claim";
const ACCEPT_PATH: &str = "/buzz/api/invites/accept-policy";

async fn seed_config(env: &TestEnv, key: &[u8], value: &[u8]) {
    let mut config = env.store("config");
    config.put(key, value).await.expect("config seed");
}

async fn seed_invite_key(env: &TestEnv) {
    seed_config(env, b"buzz-invite-key", &[0x42u8; 32]).await;
}

async fn seed_moderators(env: &TestEnv, mods: &[&Actor]) {
    let hexes: Vec<String> = mods.iter().map(|m| m.pk_hex()).collect();
    seed_config(
        env,
        b"buzz-community-moderators",
        json!(hexes).to_string().as_bytes(),
    )
    .await;
}

async fn mint(env: &TestEnv, actor: &Actor) -> (u16, Value) {
    let reply = env
        .handle(post(actor, MINT_PATH, b"{}".to_vec()))
        .await
        .expect("mint replies");
    (reply.status, body_json(&reply))
}

async fn claim_with(env: &TestEnv, actor: &Actor, body: Value) -> (u16, Value) {
    let reply = env
        .handle(post(actor, CLAIM_PATH, body.to_string().into_bytes()))
        .await
        .expect("claim replies");
    (reply.status, body_json(&reply))
}

async fn claim(env: &TestEnv, actor: &Actor, code: &str) -> (u16, Value) {
    claim_with(env, actor, json!({ "code": code })).await
}

async fn community_member(env: &TestEnv, actor: &Actor) -> bool {
    let mut events = env.store("events");
    events
        .get(&keys::community_mem(COMMUNITY, &actor.pk))
        .await
        .expect("membership read")
        .is_some()
}

/// Minting is fail-closed on the community authority: nobody configured ⇒
/// nobody mints; a non-moderator is refused; the moderator's code claims a
/// durable membership exactly once — and a re-claim is `already_member`.
#[test]
 fn severe_mint_is_gated_and_claim_is_idempotent() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("moderator");
    let stranger = Actor::new("stranger");
    let joiner = Actor::new("joiner");
    seed_invite_key(&env).await;

    // No moderator set configured: NOBODY mints (fail-closed).
    let (status, body) = mint(&env, &moderator).await;
    assert_eq!(status, 403, "no configured authority may mint: {body}");

    seed_moderators(&env, &[&moderator]).await;
    // A non-moderator still cannot mint.
    let (status, body) = mint(&env, &stranger).await;
    assert_eq!(status, 403, "a non-moderator cannot mint: {body}");

    // The moderator mints; the joiner claims; membership is durable.
    let (status, minted) = mint(&env, &moderator).await;
    assert_eq!(status, 200, "{minted}");
    let code = minted["code"].as_str().expect("a code").to_string();
    assert!(minted["expires_at"].as_u64().unwrap() > T0_SECS);

    assert!(!community_member(&env, &joiner).await);
    let (status, claimed) = claim(&env, &joiner, &code).await;
    assert_eq!(status, 200, "{claimed}");
    assert_eq!(claimed["status"], "joined");
    assert_eq!(claimed["role"], "member");
    assert!(
        community_member(&env, &joiner).await,
        "membership is durable"
    );

    // Codes are multi-use until expiry; a RE-claim by the same pubkey is
    // idempotent, and another pubkey may still use the same code.
    let (status, again) = claim(&env, &joiner, &code).await;
    assert_eq!(status, 200);
    assert_eq!(again["status"], "already_member", "{again}");
    let second = Actor::new("second-joiner");
    let (status, other) = claim(&env, &second, &code).await;
    assert_eq!(status, 200);
    assert_eq!(other["status"], "joined", "{other}");

    });
}

/// Every forgery class is refused coarsely, expiry is enforced by the frozen
/// clock, and nothing refused materializes membership. The claim limiter
/// bounds probing per pubkey.
#[test]
 fn severe_forged_and_expired_codes_are_refused_and_probing_is_bounded() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("moderator");
    let prober = Actor::new("prober");
    seed_invite_key(&env).await;
    seed_moderators(&env, &[&moderator]).await;

    // A code minted under a DIFFERENT key: refused (and coarse).
    let foreign_key = invites::derive_invite_key(&[9u8; 32]);
    let (foreign, _) =
        invites::mint_invite(&foreign_key, COMMUNITY, 3600, T0_SECS, b"seed").unwrap();
    let (status, body) = claim(&env, &prober, &foreign).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "restricted: invite_invalid");

    // Garbage.
    let (status, _) = claim(&env, &prober, "not-a-code").await;
    assert_eq!(status, 403);

    // An EXPIRED code under the real key: distinguishable post-MAC (UX), still
    // a refusal, still no membership. Minted in the past relative to the
    // frozen clock.
    let real_key = invites::derive_invite_key(&[0x42u8; 32]);
    let (expired, _) =
        invites::mint_invite(&real_key, COMMUNITY, 60, T0_SECS - 3600, b"seed").unwrap();
    let (status, body) = claim(&env, &prober, &expired).await;
    assert_eq!(status, 403);
    assert_eq!(body["error"], "restricted: invite_expired", "{body}");

    // A hand-crafted "owner" payload under the REAL key (a future buggy mint
    // path): the role cap holds at verify.
    let owner_code = {
        let payload = ferrite_app::serde_json::to_vec(&json!({
            "c": COMMUNITY, "r": "owner", "e": T0_SECS + 3600, "n": "x"
        }))
        .unwrap();
        let mac = buzz::relay::crypto::hmac::hmac_sha256(&real_key, &payload);
        let b64url = |b: &[u8]| {
            buzz::relay::b64::encode(b)
                .replace('+', "-")
                .replace('/', "_")
                .trim_end_matches('=')
                .to_string()
        };
        format!("{}.{}", b64url(&payload), b64url(&mac))
    };
    let (status, body) = claim(&env, &prober, &owner_code).await;
    assert_eq!(
        status, 403,
        "an elevated role never rides a bearer link: {body}"
    );

    assert!(
        !community_member(&env, &prober).await,
        "no refused claim may materialize membership"
    );

    // The fixed-window limiter: the prober has burned 4 attempts; 6 more pass
    // the window's budget of 10, and the 11th is rate-limited BEFORE any
    // verification work.
    for _ in 0..6 {
        let (status, _) = claim(&env, &prober, "probe").await;
        assert_eq!(status, 403);
    }
    let (status, body) = claim(&env, &prober, "probe").await;
    assert_eq!(status, 429, "probing is bounded per pubkey: {body}");

    });
}

/// A configured join policy makes the invite-bound receipt mandatory: no
/// receipt ⇒ refused; wrong version or unconfirmed age at acceptance ⇒ no
/// receipt; a receipt for a DIFFERENT code ⇒ refused; the right receipt ⇒
/// joined, with the accepted version recorded on the membership.
#[test]
 fn severe_join_policy_requires_the_bound_receipt() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("moderator");
    let joiner = Actor::new("joiner");
    seed_invite_key(&env).await;
    seed_moderators(&env, &[&moderator]).await;
    seed_config(
        &env,
        b"buzz-join-policy",
        json!({
            "version": "v3",
            "terms_markdown": "# Terms\nBe kind.",
            "age_attestation_required": true
        })
        .to_string()
        .as_bytes(),
    )
    .await;

    let (_, minted) = mint(&env, &moderator).await;
    let code = minted["code"].as_str().expect("a code").to_string();

    // Claim without a receipt: refused.
    let (status, body) = claim(&env, &joiner, &code).await;
    assert_eq!(status, 403);
    assert_eq!(body["error"], "restricted: join_policy_required", "{body}");

    // Acceptance with the wrong version, or without the required age
    // confirmation: no receipt.
    let accept = |body: Value| {
        let env = &env;
        let joiner = &joiner;
        async move {
            let reply = env
                .handle(post(joiner, ACCEPT_PATH, body.to_string().into_bytes()))
                .await
                .expect("accept replies");
            (reply.status, body_json(&reply))
        }
    };
    let (status, _) =
        accept(json!({ "code": code, "policy_version": "v2", "age_confirmed": true })).await;
    assert_eq!(status, 400, "wrong version is not acceptance");
    let (status, _) =
        accept(json!({ "code": code, "policy_version": "v3", "age_confirmed": false })).await;
    assert_eq!(status, 400, "required age attestation is not optional");

    // Real acceptance mints a receipt — but it binds to THE code: presenting
    // it with a different code fails.
    let (status, accepted) =
        accept(json!({ "code": code, "policy_version": "v3", "age_confirmed": true })).await;
    assert_eq!(status, 200, "{accepted}");
    let receipt = accepted["receipt"].as_str().expect("a receipt").to_string();

    let (_, other_minted) = mint(&env, &moderator).await;
    let other_code = other_minted["code"].as_str().expect("a code");
    assert_ne!(other_code, code, "distinct mints differ");
    let (status, body) = claim_with(
        &env,
        &joiner,
        json!({ "code": other_code, "policy_receipt": receipt }),
    )
    .await;
    assert_eq!(status, 403, "a receipt binds to one code: {body}");

    // The right pairing joins, and the membership records the version.
    let (status, joined) = claim_with(
        &env,
        &joiner,
        json!({ "code": code, "policy_receipt": receipt }),
    )
    .await;
    assert_eq!(status, 200, "{joined}");
    assert_eq!(joined["status"], "joined");
    let mut events = env.store("events");
    let member: Value = ferrite_app::serde_json::from_slice(
        &events
            .get(&keys::community_mem(COMMUNITY, &joiner.pk))
            .await
            .unwrap()
            .expect("membership doc"),
    )
    .unwrap();
    assert_eq!(member["policy_version"], "v3", "{member}");

    });
}

/// The closed-community mode: writes require claimed membership; claiming
/// through the REAL route opens them; the default (config absent) stays
/// byte-identically open.
#[test]
 fn severe_closed_community_gates_writes_until_a_claim() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("moderator");
    let outsider = Actor::new("outsider");
    let channel = "chan-open";
    seed_invite_key(&env).await;
    seed_moderators(&env, &[&moderator]).await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &moderator).await;

    let msg = |actor: &Actor, at: u64, text: &str| {
        signed_at(
            actor,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", channel)],
            text,
            at,
        )
    };

    // Default: open — an outsider writes into an open channel (pre-W8 shape).
    let before = submit(&env, &outsider, &msg(&outsider, T0_SECS, "open by default")).await;
    assert_eq!(
        before.status,
        200,
        "{}",
        String::from_utf8_lossy(&before.body)
    );

    // The operator closes the community: the SAME outsider is refused, and
    // nothing is stored.
    seed_config(&env, b"buzz-community-closed", b"true").await;
    let stored_before = stored_event_count(&env).await;
    let refused = submit(&env, &outsider, &msg(&outsider, T0_SECS + 1, "still here?")).await;
    assert_eq!(
        refused.status,
        403,
        "a closed community refuses unclaimed writers: {}",
        String::from_utf8_lossy(&refused.body)
    );
    assert_eq!(stored_event_count(&env).await, stored_before);

    // Membership seeding: the moderator claims too (moderator authority is
    // not membership), then the outsider claims through the REAL route.
    let (_, minted) = mint(&env, &moderator).await;
    let code = minted["code"].as_str().expect("a code");
    let (status, _) = claim(&env, &outsider, code).await;
    assert_eq!(status, 200);

    // The claim is what opens the write path.
    let after = submit(
        &env,
        &outsider,
        &msg(&outsider, T0_SECS + 2, "claimed and back"),
    )
    .await;
    assert_eq!(
        after.status,
        200,
        "a claimed member writes: {}",
        String::from_utf8_lossy(&after.body)
    );

    });
}
