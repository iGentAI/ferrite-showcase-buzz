//! **A member removed from a PRIVATE room must be told** — the revocation must
//! not censor the notice announcing the revocation.
//!
//! The relay mints a `#p`-gated 44101 for a removed member, scoped (`h`) to the
//! room it is about. In a private room the membership read gate refuses a
//! non-member any `h`-scoped event — and by the time the subject reads their
//! 44101 they are exactly a non-member, because `bump_revocation` deliberately
//! commits BEFORE the notice is pushed (§6.3 rule b). Observed live: the same
//! member, same `#p` filter, back to back — the OPEN room returned
//! `[44101, 44100]`, the PRIVATE room returned `[]`. The one case where losing
//! access matters is the only case nobody was told about.
//!
//! This gate pins the fix — a NARROW read-gate exemption — and its narrowness:
//!
//! * the removed member reads the room's 44101, and the 44100 they legitimately
//!   received while a member, through the real query route;
//! * the same removed member still CANNOT read the room's ordinary events;
//! * a stranger reads none of it — not the messages, not the notices.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::{Role, Visibility};
use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::{keys, kind, store as buzz_store};
use common::{
    Actor, COMMUNITY, T0_SECS, gate_env, index_keys, membership, query, returned, returned_ids,
    seed_channel_with_owner, signed_at, stored_event, submit, tag,
};
use ferrite_app::serde_json::json;
use common::SsoTestEnv as TestEnv;

/// The config key an operator seeds the relay's signing identity into
/// (`ingress::RELAY_IDENTITY_CONFIG`).
const RELAY_IDENTITY_CONFIG: &[u8] = b"buzz-relay-identity";

async fn seed_relay_identity(env: &TestEnv) -> Actor {
    let relay = Actor::new("the-relay-itself");
    let mut config = env.store("config");
    buzz_store::put_raw(
        &mut config,
        RELAY_IDENTITY_CONFIG,
        to_hex(&relay.sk).as_bytes(),
    )
    .await
    .expect("relay identity seed");
    relay
}

/// Every stored event of one kind, by the kind index.
async fn stored_of_kind(env: &TestEnv, kind_: u32) -> Vec<Event> {
    let mut out = Vec::new();
    for key in index_keys(env, &keys::kind_prefix(COMMUNITY, kind_)).await {
        let Some((_, id)) = keys::parse_index_tail(&key) else {
            continue;
        };
        if let Some(event) = stored_event(env, &id).await {
            out.push(event);
        }
    }
    out
}

/// A removed member of a private room reads their own membership notices —
/// and ONLY those. A stranger reads nothing at all.
#[test]
 fn severe_a_removed_member_of_a_private_room_reads_their_own_notices() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_relay_identity(&env).await;
    let owner = Actor::new("private-room-owner");
    let member = Actor::new("member-to-be-removed");
    let channel = "chan-private-removal";
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;

    // The owner adds the member (mints the 44100), posts a room message, then
    // removes the member (mints the 44101, revocation committed first).
    let add = signed_at(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        vec![
            tag("h", channel),
            tag("p", &member.pk_hex()),
            tag("role", Role::Member.as_str()),
        ],
        "",
        T0_SECS,
    );
    assert_eq!(submit(&env, &owner, &add).await.status, 200);

    let message = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "members-only content",
        T0_SECS + 1,
    );
    assert_eq!(submit(&env, &owner, &message).await.status, 200);

    let remove = signed_at(
        &owner,
        kind::KIND_NIP29_REMOVE_USER,
        vec![tag("h", channel), tag("p", &member.pk_hex())],
        "",
        T0_SECS + 2,
    );
    assert_eq!(submit(&env, &owner, &remove).await.status, 200);

    // Both notices exist in the store — so any read miss below is CENSORSHIP,
    // not a minting failure.
    let added = stored_of_kind(&env, kind::KIND_MEMBER_ADDED_NOTIFICATION).await;
    let removed = stored_of_kind(&env, kind::KIND_MEMBER_REMOVED_NOTIFICATION).await;
    assert_eq!(added.len(), 1, "the add minted its notice: {added:?}");
    assert_eq!(
        removed.len(),
        1,
        "the removal minted its notice: {removed:?}"
    );

    // The removed member — a NON-member now — reads both of their notices
    // through the real query route.
    let page = query(&env, &member, &json!({ "channel": channel, "limit": 50 })).await;
    assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));
    assert!(
        returned(&page, &removed[0]),
        "the removed member was never told: the revocation censored its own 44101 \
         (returned: {:?})",
        returned_ids(&page)
    );
    assert!(
        returned(&page, &added[0]),
        "the 44100 the member legitimately received while a member vanished \
         retroactively (returned: {:?})",
        returned_ids(&page)
    );

    // NARROWNESS, same reader: the exemption is the two notice kinds, nothing
    // else — the removed member must NOT read the room's ordinary events.
    assert!(
        !returned(&page, &message),
        "the notice exemption leaked a private room's message to a removed member"
    );

    // NARROWNESS, other readers: a stranger is not the notices' `p`-tag
    // subject, so they read neither the notices nor the message.
    let stranger = Actor::new("never-a-member");
    let theirs = query(&env, &stranger, &json!({ "channel": channel, "limit": 50 })).await;
    assert_eq!(
        theirs.status,
        200,
        "{}",
        String::from_utf8_lossy(&theirs.body)
    );
    assert!(
        returned_ids(&theirs).is_empty(),
        "a stranger read a private room's events: {:?}",
        returned_ids(&theirs)
    );

    });
}
