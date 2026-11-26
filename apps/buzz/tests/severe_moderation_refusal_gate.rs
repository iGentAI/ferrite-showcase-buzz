//! **What a refused member is TOLD, and what the relay refuses to pretend.**
//!
//! Three defects, one theme: a moderation decision the client could not act on.
//!
//! 1. **The refusal sentence was unparseable.** `buzz-web`'s composer timeout
//!    banner is armed by exactly one thing —
//!    `features/moderation/lib/timeout.ts::parseTimeoutRejection`, whose
//!    documented "load-bearing parse contract" is
//!    `restricted: you are timed out until <unix_seconds>` — and the relay
//!    answered `restricted: restricted: you are banned or timed out`. Three
//!    faults in one string: it fails the client's `startsWith` (doubled wire
//!    token), it cannot distinguish a BAN from a TIMEOUT, and it carries no
//!    expiry. So a timed-out member saw a generic error, no chip, no countdown,
//!    and no reason to believe the block would ever end.
//! 2. **A temporary ban said nothing about its end.** `Restriction::banned_until`
//!    exists now, so a ban refusal can carry one; a "banned for 3 days" that
//!    reads identically to a permanent ban is the *"every temporary ban was
//!    permanent"* bug wearing a different hat.
//! 3. **Relay-membership commands were stored and ignored.** Kinds 9030 / 9031 /
//!    9032 (NIP-43 add / remove / change-role) appear in `kind.rs` and NOWHERE
//!    else: no applier, no projection. `addRelayMember` and
//!    `changeRelayMemberRole` therefore returned success while nothing changed,
//!    which is worse than a refusal, because the caller has no way to find out.
//!
//! Plus the read that replaces them: `GET /buzz/api/me`, which answers whether
//! the CALLER holds community-moderation authority. Every moderation affordance
//! in the browser build gates on a kind-13534 membership snapshot this relay does
//! not publish, so the moderator UI was unreachable even for a real moderator.
//!
//! **Every assertion here is on the wire string or on durable state**, never on a
//! helper's return value: the client parses the string, so the string is the
//! contract. Two of these gates would have FAILED before the change (the refusal
//! wording, and the 9030 family being stored with a 200), and the `/me` gate would
//! not have compiled a passing run at all — the route did not exist.

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
    Actor, SUBMIT_ATTEMPTS, T0_SECS, body_json, gate_env, error_text, fresh_auth, request_with_auth,
    seed_channel_with_owner, seed_membership, signed, stored_event, submit, tag,
};
use ferrite_app::serde_json::{json, Value};
use ferrite_app::IngressReply;
use common::SsoTestEnv as TestEnv;

const DAY_SECS: u64 = 86_400;
const ME_PATH: &str = "/buzz/api/me";

/// The exact prefix `buzz-web` matches on. Spelled as a literal so a change to
/// either side breaks this gate rather than the product.
const CLIENT_TIMEOUT_PREFIX: &str = "restricted: you are timed out until";

/// `parseTimeoutRejection`, ported. `None` = "not a timeout refusal, the client
/// leaves it to the generic error path"; `Some(None)` = "timed out, expiry
/// unusable"; `Some(Some(ms))` = the expiry the banner counts down to.
fn parse_timeout_rejection(message: &str) -> Option<Option<u64>> {
    let rest = message.trim().strip_prefix(CLIENT_TIMEOUT_PREFIX)?.trim();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    match digits.parse::<u64>() {
        Ok(secs) if secs > 0 => Some(Some(secs * 1000)),
        _ => Some(None),
    }
}

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

/// A moderation command (9040 ban / 9042 timeout / …) issued by `moderator`.
async fn moderate(
    env: &TestEnv,
    moderator: &Actor,
    command: u32,
    target: &Actor,
    expires_at_secs: Option<u64>,
) -> IngressReply {
    let mut tags = vec![tag("p", &target.pk_hex())];
    if let Some(secs) = expires_at_secs {
        tags.push(tag("expiration", &secs.to_string()));
    }
    let event = signed(moderator, command, tags, "");
    submit(env, moderator, &event).await
}

/// `actor` tries to post `note` in `channel`; returns `(status, error string)`.
///
/// `note` must be DISTINCT per call by the same actor. `signed` is deterministic
/// over (key, kind, tags, content, created_at) and the clock is frozen, so two
/// identical posts are the same event id — and the second would be absorbed by the
/// admission marker as a duplicate 200, turning a "the block lifted" assertion
/// into a false green that a still-blocked relay would also satisfy.
async fn try_post(env: &TestEnv, actor: &Actor, channel: &str, note: &str) -> (u16, String) {
    let msg = signed(
        actor,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        note,
    );
    let reply = submit(env, actor, &msg).await;
    let text = if reply.status == 200 {
        String::new()
    } else {
        error_text(&reply)
    };
    (reply.status, text)
}

/// `GET /buzz/api/me` as `actor`, with the bounded re-send every authenticated
/// helper in this suite needs.
///
/// The re-send is a HARNESS artifact, documented at `common::query`: under the
/// gates' frozen clock a lease granted to a long-poll whose client timeout fires
/// in the same instant is never redelivered, so a lost reply is permanent rather
/// than slow. Each attempt mints a FRESH authorization header (the replay guard
/// burns an id on use) and the route is a read, so asking again changes nothing.
async fn get_me(env: &TestEnv, actor: &Actor) -> IngressReply {
    for attempt in 1..=SUBMIT_ATTEMPTS {
        let (_, header) = fresh_auth(actor, "GET", ME_PATH);
        let req = request_with_auth("GET", ME_PATH, Some(&header), Vec::new());
        match env.handle(req).await {
            Ok(reply) => return reply,
            Err(e) if attempt == SUBMIT_ATTEMPTS => {
                panic!("the app never answered {SUBMIT_ATTEMPTS} GET {ME_PATH} sends: {e}")
            }
            Err(_) => continue,
        }
    }
    unreachable!("the loop either returns a reply or panics")
}

// ─────────────────────────── 1. the timeout refusal ──────────────────────────

/// A timed-out member's write is refused with the sentence the composer banner
/// parses, carrying the expiry the moderator actually chose.
///
/// Pre-fix this failed on the very first assertion: the relay answered
/// `restricted: restricted: you are banned or timed out`.
#[test]
 fn severe_a_timed_out_member_is_told_when_the_timeout_ends() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("refusal-moderator");
    let member = Actor::new("refusal-timed-out-member");
    let channel = "chan-refusal-timeout";
    seed_moderators(&env, &[&moderator]).await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &moderator).await;
    seed_membership(&env, channel, &member, Role::Member).await;

    // The control: this member can post BEFORE the timeout, so the refusal below
    // is the timeout and not a broken composer path.
    assert_eq!(
        try_post(&env, &member, channel, "before the timeout")
            .await
            .0,
        200,
        "the member could not post even before being timed out"
    );

    let expires_at_secs = T0_SECS + 7 * DAY_SECS;
    assert_eq!(
        moderate(
            &env,
            &moderator,
            kind::KIND_MODERATION_TIMEOUT,
            &member,
            Some(expires_at_secs)
        )
        .await
        .status,
        200,
        "the 9042 timeout was refused"
    );

    let (status, error) = try_post(&env, &member, channel, "during the timeout").await;
    assert_eq!(status, 403, "a timed-out member's write was admitted");
    assert_eq!(
        error,
        format!("restricted: you are timed out until {expires_at_secs}"),
        "this is the string the client parses; it does not match the contract"
    );
    assert_eq!(
        parse_timeout_rejection(&error),
        Some(Some(expires_at_secs * 1000)),
        "the composer's timeout banner cannot arm from {error:?}"
    );

    // Lifted: the same member, the same channel, the same composer path — the only
    // thing that changed is the relay's restriction state.
    assert_eq!(
        moderate(
            &env,
            &moderator,
            kind::KIND_MODERATION_UNTIMEOUT,
            &member,
            None
        )
        .await
        .status,
        200,
        "the 9043 untimeout was refused"
    );
    assert_eq!(
        try_post(&env, &member, channel, "after the untimeout")
            .await
            .0,
        200,
        "the write block outlived the timeout"
    );

    });
}

// ───────────────────────────── 2. the ban refusal ────────────────────────────

/// A ban reads as a ban — the banner must NOT arm on it — and a temporary ban
/// says when it lapses.
#[test]
 fn severe_a_ban_is_never_mistaken_for_a_timeout() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("ban-refusal-moderator");
    let permanent = Actor::new("ban-refusal-permanent");
    let temporary = Actor::new("ban-refusal-temporary");
    let channel = "chan-refusal-ban";
    seed_moderators(&env, &[&moderator]).await;
    seed_channel_with_owner(&env, channel, Visibility::Open, &moderator).await;
    seed_membership(&env, channel, &permanent, Role::Member).await;
    seed_membership(&env, channel, &temporary, Role::Member).await;

    assert_eq!(
        moderate(
            &env,
            &moderator,
            kind::KIND_MODERATION_BAN,
            &permanent,
            None
        )
        .await
        .status,
        200,
        "the 9040 ban was refused"
    );
    let (status, error) = try_post(&env, &permanent, channel, "while banned").await;
    assert_eq!(status, 403);
    assert_eq!(error, "restricted: you are banned from this community");
    assert_eq!(
        parse_timeout_rejection(&error),
        None,
        "a BAN armed the timeout banner, which would promise the member their \
         access returns on its own: {error}"
    );

    let lapses_at_secs = T0_SECS + 3 * DAY_SECS;
    assert_eq!(
        moderate(
            &env,
            &moderator,
            kind::KIND_MODERATION_BAN,
            &temporary,
            Some(lapses_at_secs)
        )
        .await
        .status,
        200,
        "the temporary 9040 ban was refused"
    );
    let (status, error) = try_post(&env, &temporary, channel, "while temp-banned").await;
    assert_eq!(status, 403);
    assert_eq!(
        error,
        format!("restricted: you are banned from this community until {lapses_at_secs}"),
        "a temporary ban that says nothing about its end is indistinguishable \
         from a permanent one"
    );
    assert_eq!(parse_timeout_rejection(&error), None);

    });
}

// ───────────────── 3. commands the relay does not implement ──────────────────

/// The NIP-43 relay-membership family is REFUSED and leaves nothing behind.
///
/// Pre-fix every one of these returned 200 and stored a signed event that no code
/// path in the crate ever read — so `addRelayMember` reported success and the
/// caller's role never moved. The stored-event assertion is what makes this a
/// gate rather than a status check: a refusal that still writes the event has not
/// fixed anything.
#[test]
 fn severe_relay_membership_commands_are_refused_not_silently_stored() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    // A community MODERATOR issues them, so the refusal cannot be mistaken for an
    // authorization failure: the most privileged identity the relay has still
    // cannot make these commands do anything.
    let moderator = Actor::new("nip43-moderator");
    let target = Actor::new("nip43-target");
    seed_moderators(&env, &[&moderator]).await;

    for (command, role) in [
        (kind::RELAY_ADMIN_ADD_MEMBER, Some("admin")),
        (kind::RELAY_ADMIN_REMOVE_MEMBER, None),
        (kind::RELAY_ADMIN_CHANGE_ROLE, Some("member")),
    ] {
        let mut tags = vec![tag("p", &target.pk_hex())];
        if let Some(role) = role {
            tags.push(tag("role", role));
        }
        let event = signed(&moderator, command, tags, "");
        let reply = submit(&env, &moderator, &event).await;

        assert_eq!(
            reply.status,
            400,
            "kind {command} was accepted; the relay has no applier for it, so the \
             caller was told a change happened that did not: {}",
            String::from_utf8_lossy(&reply.body)
        );
        let error = error_text(&reply);
        assert!(
            error.starts_with("invalid: "),
            "an unimplemented command must speak a token the client branches on, \
             got {error:?}"
        );
        assert!(
            error.contains(&format!("kind {command}")) && error.contains("not implemented"),
            "the refusal does not say WHAT was refused or WHY: {error:?}"
        );
        assert!(
            stored_event(&env, &event.id).await.is_none(),
            "kind {command} was refused and stored anyway"
        );
    }

    // The control: the moderation family that IS implemented still works, so the
    // refusal above is specific to the unimplemented kinds rather than a blanket
    // break of command ingest.
    assert_eq!(
        moderate(&env, &moderator, kind::KIND_MODERATION_BAN, &target, None)
            .await
            .status,
        200,
        "the 9040 ban stopped working"
    );

    });
}

// ──────────────────── 4. the read that replaces the 13534 ────────────────────

/// `GET /buzz/api/me` tells the caller which authority it holds — the read the
/// moderator UI needs and never had.
#[test]
 fn severe_me_reports_the_callers_own_authority_and_sanctions() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let moderator = Actor::new("me-moderator");
    let member = Actor::new("me-member");
    seed_moderators(&env, &[&moderator]).await;

    let mine = body_json(&get_me(&env, &moderator).await);
    assert_eq!(
        mine.get("pubkey").and_then(Value::as_str),
        Some(moderator.pk_hex().as_str()),
        "the route answered about someone else: {mine}"
    );
    assert_eq!(
        mine.get("moderator").and_then(Value::as_bool),
        Some(true),
        "a seeded community moderator is not reported as one: {mine}"
    );
    assert_eq!(
        mine.get("role").and_then(Value::as_str),
        Some("admin"),
        "`canManageCommunityMembers` needs owner|admin here: {mine}"
    );
    assert_eq!(
        mine.get("membership_required").and_then(Value::as_bool),
        Some(false),
        "this community is open: {mine}"
    );
    assert_eq!(mine.get("member").and_then(Value::as_bool), Some(true));
    assert_eq!(
        mine.pointer("/restriction/banned").and_then(Value::as_bool),
        Some(false),
        "{mine}"
    );
    assert!(
        mine.pointer("/restriction/timed_out_until")
            .is_some_and(Value::is_null),
        "an unrestricted caller must report no timeout: {mine}"
    );

    // A plain member is reported as one, and NEVER as an owner: this relay has no
    // owner concept, and claiming one is a lie a client would act on.
    let theirs = body_json(&get_me(&env, &member).await);
    assert_eq!(
        theirs.get("moderator").and_then(Value::as_bool),
        Some(false),
        "a non-moderator was handed moderation authority: {theirs}"
    );
    assert_eq!(theirs.get("role").and_then(Value::as_str), Some("member"));

    // A live timeout is visible BEFORE the member spends a doomed write on it,
    // in the same epoch-SECONDS the refusal sentence uses.
    let expires_at_secs = T0_SECS + DAY_SECS;
    assert_eq!(
        moderate(
            &env,
            &moderator,
            kind::KIND_MODERATION_TIMEOUT,
            &member,
            Some(expires_at_secs)
        )
        .await
        .status,
        200
    );
    let theirs = body_json(&get_me(&env, &member).await);
    assert_eq!(
        theirs
            .pointer("/restriction/timed_out_until")
            .and_then(Value::as_u64),
        Some(expires_at_secs),
        "the member cannot see their own live timeout: {theirs}"
    );
    assert_eq!(
        theirs
            .pointer("/restriction/banned")
            .and_then(Value::as_bool),
        Some(false),
        "a timeout was reported as a ban: {theirs}"
    );

    // The moderator's own answer is unchanged — the route reports on the CALLER
    // only, so one member's sanction is never another's disclosure.
    let mine = body_json(&get_me(&env, &moderator).await);
    assert!(
        mine.pointer("/restriction/timed_out_until")
            .is_some_and(Value::is_null),
        "the route leaked a third party's restriction: {mine}"
    );

    });
}

/// The route is about the AUTHENTICATED caller, so an unauthenticated request
/// cannot be answered at all — it must never fall back to a default identity.
#[test]
 fn severe_me_refuses_an_unauthenticated_caller() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    // Same bounded re-send as `get_me` and for the same frozen-clock reason; an
    // unauthenticated GET has nothing to burn, so re-asking is free.
    let mut last = None;
    for _ in 0..SUBMIT_ATTEMPTS {
        if let Ok(reply) = env
            .handle(request_with_auth("GET", ME_PATH, None, Vec::new()))
            .await
        {
            last = Some(reply);
            break;
        }
    }
    let reply = last.expect("the app never answered an unauthenticated GET /buzz/api/me");
    // 401 here, where every other refusal in this file is 403. The sanction cases
    // above refuse a caller the relay has identified; this request carries no
    // credential at all, so the answer has to be "authenticate", which is the one
    // thing a 403 would tell them not to bother doing.
    assert_eq!(
        reply.status,
        401,
        "an anonymous caller was told who they are: {}",
        String::from_utf8_lossy(&reply.body)
    );

    });
}
