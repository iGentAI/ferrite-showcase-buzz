//! Identity-archive gates (NIP-IA): what retires an identity, and who may.
//!
//! Every kind here — 9035/9036 requests, 8002/8003 notices, the 13535 list — was
//! registered and referenced NOWHERE outside the registry, so a request was
//! accepted, stored, audited and delivered while nothing archived anything. These
//! gates assert the two things that separate a working implementation from that:
//! the archive ROW changes, and an unauthorized request changes nothing.

use super::common;

use buzz::relay::channel::{Role, Visibility};
use buzz::relay::crypto::to_hex;
use buzz::relay::{archive, kind, store};
use common::{
    Actor, COMMUNITY, body_json, gate_env, error_text, post, query, seed_channel_with_owner,
    seed_membership, signed, stored_event, submit, tag,
};
use ferrite_app::serde_json;
use common::SsoTestEnv as TestEnv;

const MODERATORS_CONFIG: &[u8] = b"buzz-community-moderators";
const RELAY_IDENTITY_CONFIG: &[u8] = b"buzz-relay-identity";

/// Give the deployment a signing identity, so the relay-signed notices and the
/// 13535 list can be published at all. Seeded through the CONFIG fallback
/// because `ferrite-app-test` injects an empty secrets map.
async fn seed_relay_identity(env: &TestEnv) -> Actor {
    let relay = Actor::new("archive-relay");
    let mut config = env.store("config");
    store::put_raw(
        &mut config,
        RELAY_IDENTITY_CONFIG,
        to_hex(&relay.sk).as_bytes(),
    )
    .await
    .expect("identity seed");
    relay
}

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

/// Whether the relay's own state says this identity is archived — the ROW, which
/// is what every consumer reads, rather than the published list, which is a
/// derived view that can be one publish behind.
async fn is_archived(env: &TestEnv, who: &Actor) -> bool {
    let mut events = env.store("events");
    archive::is_archived(&mut events, COMMUNITY, &who.pk)
        .await
        .expect("archive read")
}

/// Every archived identity, as the relay holds them.
async fn archived_set(env: &TestEnv) -> Vec<String> {
    let mut events = env.store("events");
    archive::archived_identities(&mut events, COMMUNITY)
        .await
        .expect("archive scan")
        .0
        .into_iter()
        .map(|entry| entry.pubkey)
        .collect()
}

/// **An identity may retire itself, and only a moderator brings it back.**
///
/// The self case needs no elevated authority and is the common one — a bare 9035
/// with no `p` tag means "me", which is why `archive_target` falls back to the
/// author. The gate asserts the ROW both ways: an archive that only published a
/// notice would leave every consumer reading the identity as live. Once archived,
/// the identity cannot send even the unarchive (F232: archiving is enforced), so
/// coming back is a moderator's act.
#[test]
 fn severe_an_identity_can_archive_itself_and_a_moderator_brings_it_back() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_relay_identity(&env).await;
    let ana = Actor::new("archive-ana");
    let mod_actor = Actor::new("archive-moderator");
    seed_moderators(&env, &[&mod_actor]).await;

    assert!(!is_archived(&env, &ana).await, "nobody starts archived");

    let request = signed(&ana, kind::KIND_IA_ARCHIVE_REQUEST, vec![], "moving on");
    let accepted = submit(&env, &ana, &request).await;
    assert_eq!(accepted.status, 200, "{}", error_text(&accepted));
    assert!(
        is_archived(&env, &ana).await,
        "the request was accepted and nothing archived — the exact shape this closes"
    );
    assert_eq!(archived_set(&env).await, vec![ana.pk_hex()]);

    // The archived identity cannot undo its own archive.
    let undo = signed(&ana, kind::KIND_IA_UNARCHIVE_REQUEST, vec![], "");
    let refused = submit(&env, &ana, &undo).await;
    assert_eq!(refused.status, 403, "{}", error_text(&refused));
    assert!(is_archived(&env, &ana).await);
    assert!(stored_event(&env, &undo.id).await.is_none());

    // ...and unarchiving by a moderator really removes it, rather than adding a
    // second row that readers would have to reconcile.
    let lift = signed(
        &mod_actor,
        kind::KIND_IA_UNARCHIVE_REQUEST,
        vec![tag("p", &ana.pk_hex())],
        "",
    );
    let lifted = submit(&env, &mod_actor, &lift).await;
    assert_eq!(lifted.status, 200, "{}", error_text(&lifted));
    assert!(!is_archived(&env, &ana).await);
    assert!(archived_set(&env).await.is_empty());

    });
}

/// **An archived identity cannot post** (F232). The row used to be consulted by
/// nothing, so an archived pubkey kept writing as if nothing had happened. Now
/// the write gate refuses it with a sentence a client can show, a refused write
/// stores nothing, and the identity can write again once a moderator lifts it.
#[test]
 fn severe_an_archived_identity_cannot_post_until_a_moderator_lifts_it() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_relay_identity(&env).await;
    let ana = Actor::new("archive-ana");
    let owner = Actor::new("archive-room-owner");
    let mod_actor = Actor::new("archive-moderator");
    seed_moderators(&env, &[&mod_actor]).await;
    let channel = "archive-room";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    seed_membership(&env, channel, &ana, Role::Member).await;

    let before = signed(
        &ana,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "hello",
    );
    let posted = submit(&env, &ana, &before).await;
    assert_eq!(posted.status, 200, "{}", error_text(&posted));

    let imposed = signed(
        &mod_actor,
        kind::KIND_IA_ARCHIVE_REQUEST,
        vec![tag("p", &ana.pk_hex())],
        "left the company",
    );
    assert_eq!(submit(&env, &mod_actor, &imposed).await.status, 200);

    let message = signed(
        &ana,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "still here",
    );
    let refused = submit(&env, &ana, &message).await;
    assert_eq!(refused.status, 403, "{}", error_text(&refused));
    assert!(
        error_text(&refused).contains("archived"),
        "the refusal must say why: {}",
        error_text(&refused)
    );
    assert!(
        stored_event(&env, &message.id).await.is_none(),
        "an archived identity's post was stored"
    );
    let reaction = signed(
        &ana,
        kind::KIND_REACTION,
        vec![tag("e", &to_hex(&before.id)), tag("h", channel)],
        "+",
    );
    assert_eq!(submit(&env, &ana, &reaction).await.status, 403);
    assert!(stored_event(&env, &reaction.id).await.is_none());

    // Everybody else is unaffected.
    let other = signed(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "hi",
    );
    assert_eq!(submit(&env, &owner, &other).await.status, 200);

    let lift = signed(
        &mod_actor,
        kind::KIND_IA_UNARCHIVE_REQUEST,
        vec![tag("p", &ana.pk_hex())],
        "",
    );
    assert_eq!(submit(&env, &mod_actor, &lift).await.status, 200);
    let after = signed(
        &ana,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "back",
    );
    let accepted = submit(&env, &ana, &after).await;
    assert_eq!(accepted.status, 200, "{}", error_text(&accepted));

    });
}

/// **Nobody but a moderator may lift an archive on the identity's behalf.** The
/// archived identity is stopped by the write gate; this is the rule that stops
/// an ordinary member from doing it for them, even for a self-archive.
#[test]
 fn severe_a_member_cannot_unarchive_somebody_else() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_relay_identity(&env).await;
    let ana = Actor::new("archive-ana");
    let friend = Actor::new("archive-friend");

    let request = signed(&ana, kind::KIND_IA_ARCHIVE_REQUEST, vec![], "");
    assert_eq!(submit(&env, &ana, &request).await.status, 200);

    let lift = signed(
        &friend,
        kind::KIND_IA_UNARCHIVE_REQUEST,
        vec![tag("p", &ana.pk_hex())],
        "",
    );
    let refused = submit(&env, &friend, &lift).await;
    assert_eq!(refused.status, 403, "{}", error_text(&refused));
    assert!(is_archived(&env, &ana).await);
    assert!(stored_event(&env, &lift.id).await.is_none());

    });
}

/// **Archiving somebody ELSE needs moderator authority, and a refusal stores
/// nothing.**
///
/// The second half is the one worth asserting: authorization runs pre-storage, so
/// a refused request must leave no row. A refusal that still archived would
/// change the state and only the reply would say otherwise — which is
/// indistinguishable, from the target's side, from having been archived.
#[test]
 fn severe_only_a_moderator_may_archive_someone_else() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_relay_identity(&env).await;
    let ana = Actor::new("archive-ana");
    let mallory = Actor::new("archive-mallory");
    let mod_actor = Actor::new("archive-moderator");
    seed_moderators(&env, &[&mod_actor]).await;

    // A member archiving another member: refused, and nothing changes.
    let hostile = signed(
        &mallory,
        kind::KIND_IA_ARCHIVE_REQUEST,
        vec![tag("p", &ana.pk_hex())],
        "",
    );
    let refused = submit(&env, &mallory, &hostile).await;
    assert_eq!(refused.status, 403, "{}", error_text(&refused));
    assert!(
        !is_archived(&env, &ana).await,
        "a refused archive still retired the identity"
    );
    assert!(
        stored_event(&env, &hostile.id).await.is_none(),
        "a refused request must not be stored, or the silent path stays reachable"
    );

    // The moderator can.
    let allowed = signed(
        &mod_actor,
        kind::KIND_IA_ARCHIVE_REQUEST,
        vec![tag("p", &ana.pk_hex())],
        "left the company",
    );
    let accepted = submit(&env, &mod_actor, &allowed).await;
    assert_eq!(accepted.status, 200, "{}", error_text(&accepted));
    assert!(is_archived(&env, &ana).await);

    });
}

/// **A moderator's archive cannot be reverted by the archived identity.**
///
/// A bare 9036 names its author, so the self-unarchive rule alone let the
/// subject of a moderation decision delete the moderator's row. Only a moderator
/// may lift an archive somebody else imposed, and a refusal stores nothing.
#[test]
 fn severe_the_subject_of_a_moderators_archive_cannot_lift_it() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_relay_identity(&env).await;
    let ana = Actor::new("archive-ana");
    let mod_actor = Actor::new("archive-moderator");
    seed_moderators(&env, &[&mod_actor]).await;

    let imposed = signed(
        &mod_actor,
        kind::KIND_IA_ARCHIVE_REQUEST,
        vec![tag("p", &ana.pk_hex())],
        "",
    );
    assert_eq!(submit(&env, &mod_actor, &imposed).await.status, 200);
    assert!(is_archived(&env, &ana).await);

    let undo = signed(&ana, kind::KIND_IA_UNARCHIVE_REQUEST, vec![], "");
    let refused = submit(&env, &ana, &undo).await;
    assert_eq!(refused.status, 403, "{}", error_text(&refused));
    assert!(
        is_archived(&env, &ana).await,
        "the archived identity reverted a moderator's archive"
    );
    assert!(stored_event(&env, &undo.id).await.is_none());

    // The moderator can lift it.
    let lift = signed(
        &mod_actor,
        kind::KIND_IA_UNARCHIVE_REQUEST,
        vec![tag("p", &ana.pk_hex())],
        "",
    );
    assert_eq!(submit(&env, &mod_actor, &lift).await.status, 200);
    assert!(!is_archived(&env, &ana).await);

    });
}

/// **The relay publishes what it archived**, so a client that never calls the
/// route still learns about it.
///
/// 13535 is replaceable, so this also exercises the converge-and-recognize path:
/// a second archive must REPLACE the list rather than append a second one.
#[test]
 fn severe_the_published_list_names_every_archived_identity() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let relay = seed_relay_identity(&env).await;
    let ana = Actor::new("archive-ana");
    let bo = Actor::new("archive-bo");

    for who in [&ana, &bo] {
        let request = signed(who, kind::KIND_IA_ARCHIVE_REQUEST, vec![], "");
        assert_eq!(submit(&env, who, &request).await.status, 200);
    }

    // The relay's own list event, read back through the ordinary query path.
    let page = query(
        &env,
        &ana,
        &ferrite_app::serde_json::json!({ "limit": 100 }),
    )
    .await;
    let body = body_json(&page);
    let lists: Vec<&ferrite_app::serde_json::Value> = body
        .get("events")
        .and_then(|e| e.as_array())
        .map(|events| {
            events
                .iter()
                .filter(|e| {
                    e.get("kind")
                        .and_then(ferrite_app::serde_json::Value::as_u64)
                        == Some(u64::from(kind::KIND_IA_ARCHIVED_LIST))
                })
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        lists.len(),
        1,
        "13535 is replaceable, so exactly ONE list may be readable: {lists:?}"
    );
    let list = lists[0];
    assert_eq!(
        list.get("pubkey")
            .and_then(ferrite_app::serde_json::Value::as_str),
        Some(relay.pk_hex().as_str()),
        "the list must be signed by the RELAY, not by a member"
    );
    let named: Vec<String> = list
        .get("tags")
        .and_then(ferrite_app::serde_json::Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(|t| {
                    let t = t.as_array()?;
                    (t.first()?.as_str()? == "p").then(|| t.get(1)?.as_str().map(str::to_string))?
                })
                .collect()
        })
        .unwrap_or_default();
    let mut expected = vec![ana.pk_hex(), bo.pk_hex()];
    expected.sort();
    let mut named_sorted = named.clone();
    named_sorted.sort();
    assert_eq!(named_sorted, expected, "the list must name both");

    });
}
