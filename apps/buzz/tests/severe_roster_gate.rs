//! Community roster gates: who may read it, and where `admin` comes from.
//!
//! This route feeds the client's moderation UI, so both of its failure modes are
//! misleading rather than merely wrong: a roster that under-reports authority
//! hides moderation from a moderator, and one that over-reports it draws controls
//! for a caller the relay will refuse.

use super::common;

use buzz::relay::{keys, store};
use common::{
    Actor, COMMUNITY, SUBMIT_ATTEMPTS, T0_MS, body_json, gate_env, error_text, fresh_auth,
    membership, post, query, request_with_auth,
};
use ferrite_app::serde_json::{self, json, Value};
use ferrite_app::IngressReply;
use common::SsoTestEnv as TestEnv;

const MEMBERS_PATH: &str = "/buzz/api/members";
const PRESENCE_PATH: &str = "/buzz/api/presence";
const MODERATORS_CONFIG: &[u8] = b"buzz-community-moderators";
const CLOSED_CONFIG: &[u8] = b"buzz-community-closed";

/// Read the roster as `who`, re-posting if the reply is lost (the frozen-clock
/// re-send discipline `common::query` documents).
async fn roster(env: &TestEnv, who: &Actor) -> IngressReply {
    for attempt in 1..=SUBMIT_ATTEMPTS {
        let (_, header) = fresh_auth(who, "GET", MEMBERS_PATH);
        let request = request_with_auth("GET", MEMBERS_PATH, Some(&header), Vec::new());
        match env.handle(request).await {
            Ok(reply) => return reply,
            Err(e) if attempt == SUBMIT_ATTEMPTS => panic!("no reply after {SUBMIT_ATTEMPTS}: {e}"),
            Err(_) => continue,
        }
    }
    unreachable!()
}

/// Seed a community membership row, with whatever role it claims.
async fn seed_member(env: &TestEnv, who: &Actor, role: &str) {
    let mut events = env.store("events");
    let doc = json!({ "role": role, "claimed_at_ms": T0_MS, "source": "gate" });
    store::put_doc(&mut events, &keys::community_mem(COMMUNITY, &who.pk), &doc)
        .await
        .expect("membership seed");
}

/// Seed the operator-provisioned moderator set.
async fn seed_moderators(env: &TestEnv, who: &[&Actor]) {
    let list: Vec<String> = who.iter().map(|a| a.pk_hex()).collect();
    let mut config = env.store("config");
    store::put_raw(
        &mut config,
        MODERATORS_CONFIG,
        &serde_json::to_vec(&list).expect("encodes"),
    )
    .await
    .expect("moderator seed");
}

/// The `(pubkey, role)` pairs a roster reply carries.
fn rows(reply: &IngressReply) -> Vec<(String, String)> {
    body_json(reply)
        .get("members")
        .and_then(Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(|row| {
                    Some((
                        row.get("pubkey")?.as_str()?.to_string(),
                        row.get("role")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// **`admin` comes from the moderator set and nowhere else.**
///
/// A membership row's role is whatever an invite payload said; the
/// operator-provisioned set is what authorizes the moderation kinds. Honouring
/// the row would draw moderation controls for a caller the relay then refuses,
/// and would contradict `/buzz/api/me`, which reads the set directly.
#[test]
 fn severe_only_the_moderator_set_grants_admin_in_the_roster() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let mod_actor = Actor::new("roster-moderator");
    let pretender = Actor::new("roster-pretender");
    seed_moderators(&env, &[&mod_actor]).await;
    // A row that CLAIMS admin. Reachable: the role is copied from the invite
    // payload, which a minting moderator chooses.
    seed_member(&env, &pretender, "admin").await;

    let reply = roster(&env, &pretender).await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let seen = rows(&reply);

    let claimed = seen
        .iter()
        .find(|(pubkey, _)| pubkey == &pretender.pk_hex())
        .map(|(_, role)| role.as_str());
    assert_eq!(
        claimed,
        Some("member"),
        "a membership row must not be able to grant itself admin: {seen:?}"
    );

    // ...while the configured moderator IS an admin, and is present at all
    // despite holding no membership row of their own.
    let granted = seen
        .iter()
        .find(|(pubkey, _)| pubkey == &mod_actor.pk_hex())
        .map(|(_, role)| role.as_str());
    assert_eq!(
        granted,
        Some("admin"),
        "a provisioned moderator must appear in the roster as admin: {seen:?}"
    );

    // And each pubkey appears exactly once.
    let mut pubkeys: Vec<&String> = seen.iter().map(|(pubkey, _)| pubkey).collect();
    pubkeys.sort();
    let before = pubkeys.len();
    pubkeys.dedup();
    assert_eq!(
        pubkeys.len(),
        before,
        "the roster repeated a pubkey: {seen:?}"
    );

    });
}

/// **A moderator who ALSO holds a membership row still appears once.**
///
/// The union is deduplicated, and the row must not downgrade them: the moderator
/// entry is the authoritative one.
#[test]
 fn severe_a_moderator_with_a_membership_row_appears_once_as_admin() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let both = Actor::new("roster-both");
    seed_moderators(&env, &[&both]).await;
    seed_member(&env, &both, "member").await;

    let seen = rows(&roster(&env, &both).await);
    let mine: Vec<&(String, String)> = seen
        .iter()
        .filter(|(pubkey, _)| pubkey == &both.pk_hex())
        .collect();
    assert_eq!(mine.len(), 1, "reported twice: {seen:?}");
    assert_eq!(
        mine[0].1, "admin",
        "the membership row downgraded a moderator"
    );

    });
}

/// **A closed community does not hand its roster to an outsider.**
///
/// The list of everyone in a private workspace is exactly what an attacker
/// wants, and it is the one disclosure this route could make.
#[test]
 fn severe_a_closed_community_refuses_a_non_members_roster_read() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let member = Actor::new("roster-member");
    let outsider = Actor::new("roster-outsider");
    seed_member(&env, &member, "member").await;
    {
        let mut config = env.store("config");
        store::put_raw(&mut config, CLOSED_CONFIG, b"true")
            .await
            .expect("closed seed");
    }

    let refused = roster(&env, &outsider).await;
    assert_eq!(refused.status, 403, "{}", error_text(&refused));

    // The member still reads it, and sees themself.
    let allowed = roster(&env, &member).await;
    assert_eq!(allowed.status, 200, "{}", error_text(&allowed));
    assert!(
        rows(&allowed)
            .iter()
            .any(|(pubkey, _)| pubkey == &member.pk_hex()),
        "a member cannot see their own membership"
    );
    assert_eq!(
        body_json(&allowed)
            .get("membership_required")
            .and_then(Value::as_bool),
        Some(true),
    );

    });
}

/// **A closed community does not hand its online roster to an outsider.**
///
/// `presence` returns the same class of fact as `members` (which workspace
/// identities are live right now), so it takes the same membership gate.
#[test]
 fn severe_a_closed_community_refuses_a_non_members_presence_read() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let member = Actor::new("presence-member");
    let outsider = Actor::new("presence-outsider");
    seed_member(&env, &member, "member").await;
    {
        let mut config = env.store("config");
        store::put_raw(&mut config, CLOSED_CONFIG, b"true")
            .await
            .expect("closed seed");
    }

    let refused = env
        .handle(post(&outsider, PRESENCE_PATH, b"{}".to_vec()))
        .await
        .expect("reply");
    assert_eq!(refused.status, 403, "{}", error_text(&refused));
    assert!(
        body_json(&refused).get("online").is_none(),
        "a refused presence read must carry no roster"
    );

    let allowed = env
        .handle(post(&member, PRESENCE_PATH, b"{}".to_vec()))
        .await
        .expect("reply");
    assert_eq!(allowed.status, 200, "{}", error_text(&allowed));

    });
}

/// **An open community says so, rather than looking empty.**
///
/// With no gate configured there are no claim rows to scan, so `members` is
/// empty — and that is indistinguishable from "nobody is here" without the flag.
#[test]
 fn severe_an_open_community_reports_that_it_tracks_no_membership() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let anyone = Actor::new("roster-anyone");

    let reply = roster(&env, &anyone).await;
    assert_eq!(reply.status, 200, "an open community answers any member");
    let body = body_json(&reply);
    assert_eq!(
        body.get("membership_required").and_then(Value::as_bool),
        Some(false),
        "an empty roster is only readable alongside this flag"
    );
    assert_eq!(body.get("complete").and_then(Value::as_bool), Some(true));
    assert!(
        rows(&reply).is_empty(),
        "no rows are written until a gate exists"
    );

    });
}

/// **An unauthenticated caller gets nothing**, because a roster is for the
/// people in the community.
#[test]
 fn severe_the_roster_needs_a_credential() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    // The same bounded re-send every other read here uses: under the frozen
    // clock a lost reply is permanent rather than slow, and a REFUSAL is just as
    // subject to that as an answer.
    let mut reply = None;
    for _ in 0..SUBMIT_ATTEMPTS {
        let anonymous = request_with_auth("GET", MEMBERS_PATH, None, Vec::new());
        if let Ok(got) = env.handle(anonymous).await {
            reply = Some(got);
            break;
        }
    }
    let reply = reply.expect("the app never answered an anonymous roster read");
    // 401, not 403: authenticating IS the fix for an absent credential.
    assert_eq!(reply.status, 401, "{}", error_text(&reply));

    });
}
