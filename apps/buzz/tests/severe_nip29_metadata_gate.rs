//! **The NIP-29 relay-signed group projections, decided by the WIRING.**
//!
//! buzz's real client does not build its workspace from this relay's JSON
//! routes. It builds it from kind **39000** (group metadata) and kind **39002**
//! (group members) events that the RELAY signs with its own key — the desktop
//! client's `channel_info_from_event` / `channel_members_from_event` are the
//! only way its channel list and roster are ever populated, and its
//! `pending_owned_channels` overlay exists purely to paper over the moment
//! before "the real kind:39002 is observed". Declaring the kinds proves
//! nothing: the property is that a real create and a real membership command,
//! through the real routes, LEAVE those events behind — signed by the relay,
//! carrying the right tags, superseding the previous version, and gated by the
//! channel they describe.
//!
//! So every case here drives the real app on a real fleet, under the FROZEN
//! clock the house uses. The frozen clock is not incidental: two versions of one
//! projection minted in the same second would tie, and `version_beats` breaks a
//! tie by lowest id — where the STALE roster can win. A gate that let time move
//! would hide exactly that bug.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::time::Duration;

use buzz::relay::channel::{Role, Visibility};
use buzz::relay::crypto::to_hex;
use buzz::relay::effects::ReplaceableHead;
use buzz::relay::event::Event;
use buzz::relay::{keys, kind, store as buzz_store};
use common::{
    Actor, COMMUNITY, FEED_GENERATION, REPLY_BUDGET, T0_MS, T0_SECS, admission, audit_ref,
    channel_query, gate_env, error_text, feed_copies, feed_records, index_keys, membership, post,
    query, returned_ids, seed_channel_with_owner, seed_membership, signed, signed_at,
    stored_event, submit, tag, ws_path_with_ticket,
};
use ferrite_app::serde_json::{self, json, Value};
use common::SsoTestEnv as TestEnv;
use ferrite_app_test::{TestHarness, WsSession};

/// The config key an operator seeds the relay's own signing identity into
/// (`ingress::RELAY_IDENTITY_CONFIG`). A secret of the same name wins when one
/// is injected; the harness injects none, so this is the path under test.
const RELAY_IDENTITY_CONFIG: &[u8] = b"buzz-relay-identity";

/// How long a projection that MUST arrive on a live stream is waited for, and
/// how long one that must NOT arrive is watched for. The negative budget is
/// longer than the pump's idle poll (1s) so a leak has several batches to show
/// up in.
const ARRIVES: Duration = REPLY_BUDGET;
const NEVER: Duration = Duration::from_secs(6);

/// The identity the operator seeds. An [`Actor`] is just a keypair, and using
/// one keeps the gate's expectations about the relay's pubkey derived from the
/// same secret the app is given.
fn relay() -> Actor {
    Actor::new("the-relay-itself")
}

/// Seed the relay identity the way an operator does: 64-hex in the config store.
async fn seed_relay_identity(env: &TestEnv) -> Actor {
    let relay = relay();
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

/// Create a room through the REAL route (`POST /buzz/api/channels`) — the only
/// way a channel comes into existence in this port, and therefore where a
/// client's first sight of it has to come from.
async fn create_room(
    env: &TestEnv,
    owner: &Actor,
    id: &str,
    name: &str,
    visibility: Visibility,
) -> ferrite_app::IngressReply {
    env.handle(post(
        owner,
        "/buzz/api/channels",
        json!({ "id": id, "name": name, "visibility": visibility.as_str() })
            .to_string()
            .into_bytes(),
    ))
    .await
    .expect("the app replied")
}

/// The watermark for one projection cell, read with the app's own key builder.
async fn projection_head(
    env: &TestEnv,
    kind_: u32,
    relay: &Actor,
    channel: &str,
) -> Option<ReplaceableHead> {
    let mut guards = env.ape("guards");
    let raw = guards
        .get(&keys::dtag_head(COMMUNITY, kind_, &relay.pk, channel))
        .await
        .expect("ape read")?;
    Some(serde_json::from_slice(&raw).expect("head decodes"))
}

/// The projection this relay currently publishes for `(kind, channel)`: the
/// watermark's winner, read back through its stored body. `None` means nothing
/// is published — which is what a client would see as an empty workspace.
async fn projection(env: &TestEnv, kind_: u32, relay: &Actor, channel: &str) -> Option<Event> {
    let head = projection_head(env, kind_, relay, channel).await?;
    let id = buzz::relay::crypto::from_hex32(&head.id).expect("a 32-byte head id");
    stored_event(env, &id).await
}

/// The same, but insisting: a missing projection is the whole bug this gate
/// exists for, so it fails with the channel named.
async fn expect_projection(env: &TestEnv, kind_: u32, relay: &Actor, channel: &str) -> Event {
    projection(env, kind_, relay, channel)
        .await
        .unwrap_or_else(|| panic!("no kind:{kind_} projection for channel {channel}"))
}

/// A tag's second element, by tag name.
fn tag_value(event: &Event, name: &str) -> Option<String> {
    event.first_tag(name).map(str::to_string)
}

/// Whether the event carries a bare marker tag (`["private"]`, `["open"]`, …).
fn has_marker(event: &Event, name: &str) -> bool {
    event
        .tags
        .iter()
        .any(|t| t.as_slice() == [name.to_string()])
}

/// The `(pubkey, role)` pairs a roster projection carries. 39002 uses NIP-29's
/// `["p", pubkey, relay_url, role]`, 39001 uses `["p", pubkey, role]`; the role
/// is read from the LAST element either way, which is exactly what the desktop
/// client's `channel_members_from_event` does.
fn roster(event: &Event) -> Vec<(String, String)> {
    event
        .tags
        .iter()
        .filter(|t| t.first().map(String::as_str) == Some("p"))
        .filter_map(|t| {
            let pubkey = t.get(1)?.clone();
            let role = t.last().filter(|r| r.as_str() != pubkey).cloned();
            Some((pubkey, role.unwrap_or_default()))
        })
        .collect()
}

/// Every stored event id of one kind in the community, per the kind index —
/// "what could a client possibly find", independent of any watermark.
async fn stored_ids_of_kind(env: &TestEnv, kind_: u32) -> Vec<String> {
    let mut out = Vec::new();
    for key in index_keys(env, &keys::kind_prefix(COMMUNITY, kind_)).await {
        let Some((_, id)) = keys::parse_index_tail(&key) else {
            continue;
        };
        // An index row only counts if its body is still there: replacement
        // deletes indexes first and the body last, and readers skip the gap.
        if stored_event(env, &id).await.is_some() {
            out.push(to_hex(&id));
        }
    }
    out
}

/// `h` + `p` (+ optional `role`) tags for a membership command.
fn member_tags(channel: &str, target: &Actor, role: Option<Role>) -> Vec<Vec<String>> {
    let mut tags = vec![tag("h", channel), tag("p", &target.pk_hex())];
    if let Some(role) = role {
        tags.push(tag("role", role.as_str()));
    }
    tags
}

// ------------------------------------------------------------------ the gates

/// **Creation publishes the events a client reads.** A room created through the
/// real route leaves a relay-SIGNED 39000 behind whose `d` is the channel and
/// whose tags carry the name, plus the roster pair — and a client cannot forge
/// one of its own.
#[test]
 fn severe_creating_a_channel_publishes_relay_signed_group_metadata() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let relay = seed_relay_identity(&env).await;
    let owner = Actor::new("owner");
    let channel = "chan-meta";

    let reply = create_room(&env, &owner, channel, "general", Visibility::Open).await;
    assert_eq!(
        reply.status,
        201,
        "the room is created: {}",
        String::from_utf8_lossy(&reply.body)
    );

    // ---- kind 39000: the channel a client lists -------------------------
    let meta = expect_projection(&env, kind::KIND_NIP29_GROUP_METADATA, &relay, channel).await;
    assert_eq!(
        tag_value(&meta, "d").as_deref(),
        Some(channel),
        "the `d` tag IS the channel id a client keys its list by: {:?}",
        meta.tags
    );
    assert_eq!(
        tag_value(&meta, "name").as_deref(),
        Some("general"),
        "the name a client shows must be on the event: {:?}",
        meta.tags
    );
    assert_eq!(meta.content, "", "NIP-29 group state lives in tags");

    // Signed by the RELAY's own key, and actually valid: a client that verifies
    // (every nostr client does) must accept it.
    assert_eq!(meta.pubkey, relay.pk, "the relay signs its own projections");
    assert_ne!(meta.pubkey, owner.pk, "not signed by the creating client");
    assert_eq!(meta.verify(), Ok(()), "the projection must verify");

    // NIP-29's marker pairs, and the channel scope this port gates on.
    assert!(
        has_marker(&meta, "public"),
        "an open room is public: {:?}",
        meta.tags
    );
    assert!(has_marker(&meta, "open"), "an open room is self-joinable");
    assert!(!has_marker(&meta, "private"));
    assert_eq!(tag_value(&meta, "h").as_deref(), Some(channel));

    // It went through the ordinary saga: admitted-and-completed, audited, and on
    // the live feed under the channel it describes — not written behind the
    // ingest path's back.
    let saga = admission(&env, &meta.id)
        .await
        .expect("the projection was admitted");
    assert!(saga.is_complete(), "the projection's saga must complete");
    assert_eq!(saga.channel.as_deref(), Some(channel));
    assert!(
        audit_ref(&env, &meta.id).await.is_some(),
        "the projection is audited"
    );
    assert_eq!(
        feed_copies(&env, FEED_GENERATION, &meta.id).await,
        1,
        "the projection reached the delivery feed exactly once"
    );

    // ---- kinds 39002 / 39001: the creator is the sole member and owner ----
    let members = expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, channel).await;
    assert_eq!(
        roster(&members),
        vec![(owner.pk_hex(), Role::Owner.as_str().to_string())],
        "a new room's roster is exactly its creator: {:?}",
        members.tags
    );
    assert_eq!(tag_value(&members, "d").as_deref(), Some(channel));
    let admins = expect_projection(&env, kind::KIND_NIP29_GROUP_ADMINS, &relay, channel).await;
    assert_eq!(
        roster(&admins),
        vec![(owner.pk_hex(), Role::Owner.as_str().to_string())],
        "the creator is the room's owner: {:?}",
        admins.tags
    );

    // ---- a client cannot forge any of them --------------------------------
    // The whole authority of a member list is the relay's signature, so the
    // submit path must refuse these kinds outright.
    for kind_ in [
        kind::KIND_NIP29_GROUP_METADATA,
        kind::KIND_NIP29_GROUP_ADMINS,
        kind::KIND_NIP29_GROUP_MEMBERS,
    ] {
        let forged = signed(
            &owner,
            kind_,
            vec![tag("d", channel), tag("h", channel), tag("name", "pwned")],
            "",
        );
        let refused = submit(&env, &owner, &forged).await;
        assert_eq!(
            refused.status,
            403,
            "a client-signed kind:{kind_} must be refused: {}",
            String::from_utf8_lossy(&refused.body)
        );
        assert!(
            error_text(&refused).contains("relay-signed"),
            "the refusal must say why: {}",
            error_text(&refused)
        );
        assert!(
            stored_event(&env, &forged.id).await.is_none(),
            "a forged kind:{kind_} must not be stored"
        );
    }
    // The relay's own projection is untouched by the forgery attempts.
    assert_eq!(
        expect_projection(&env, kind::KIND_NIP29_GROUP_METADATA, &relay, channel)
            .await
            .id,
        meta.id
    );

    });
}

/// **A membership change republishes the roster, and the old roster stops
/// existing.** Add, then remove: each lands a 39002 carrying exactly the live
/// member set, with a strictly newer `created_at` (under a FROZEN clock, so only
/// the watermark floor can produce it), and each supersedes its predecessor —
/// body and index rows gone, W7-style.
#[test]
 fn severe_membership_changes_republish_the_roster_and_retire_the_old_one() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let relay = seed_relay_identity(&env).await;
    let owner = Actor::new("owner");
    let newcomer = Actor::new("newcomer");
    let channel = "chan-roster";
    assert_eq!(
        create_room(&env, &owner, channel, "roster", Visibility::Open)
            .await
            .status,
        201
    );

    let v1 = expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, channel).await;
    assert_eq!(roster(&v1), vec![(owner.pk_hex(), "owner".to_string())]);

    // ---- add ------------------------------------------------------------
    let add = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &newcomer, Some(Role::Member)),
        "",
    );
    let reply = submit(&env, &owner, &add).await;
    assert_eq!(reply.status, 200, "add accepted: {}", error_text(&reply));

    let v2 = expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, channel).await;
    let mut after_add = roster(&v2);
    after_add.sort();
    let mut want = vec![
        (owner.pk_hex(), "owner".to_string()),
        (newcomer.pk_hex(), "member".to_string()),
    ];
    want.sort();
    assert_eq!(
        after_add, want,
        "the roster must be EXACTLY the live member set: {:?}",
        v2.tags
    );
    assert!(
        v2.created_at > v1.created_at,
        "created_at must advance or the STALE roster can win the tie ({} vs {})",
        v2.created_at,
        v1.created_at
    );
    // The old roster is not merely shadowed: it is gone.
    assert!(
        stored_event(&env, &v1.id).await.is_none(),
        "the superseded roster's body must be deleted"
    );
    assert_eq!(
        stored_ids_of_kind(&env, kind::KIND_NIP29_GROUP_MEMBERS).await,
        vec![to_hex(&v2.id)],
        "exactly one queryable 39002 per channel"
    );
    // And it is not queryable through the read face either.
    let page = query(&env, &owner, &channel_query(channel)).await;
    assert_eq!(page.status, 200);
    let ids = returned_ids(&page);
    assert!(
        ids.contains(&to_hex(&v2.id)),
        "the current roster is readable"
    );
    assert!(
        !ids.contains(&to_hex(&v1.id)),
        "the superseded roster must not come back from a query"
    );

    // A trigger that changes NOTHING publishes nothing: re-adding the same
    // member at the same role is a distinct event (distinct content ⇒ distinct
    // id) that really drives a saga, and the projection step must recognize its
    // own output rather than churn a new version.
    let noop = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &newcomer, Some(Role::Member)),
        "re-add, same role",
    );
    assert_eq!(submit(&env, &owner, &noop).await.status, 200);
    assert!(
        stored_event(&env, &noop.id).await.is_some(),
        "the no-op trigger really was ingested"
    );
    assert_eq!(
        expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, channel)
            .await
            .id,
        v2.id,
        "an unchanged roster must not be republished"
    );

    // ---- remove ---------------------------------------------------------
    let remove = signed(
        &owner,
        kind::KIND_NIP29_REMOVE_USER,
        member_tags(channel, &newcomer, None),
        "",
    );
    let reply = submit(&env, &owner, &remove).await;
    assert_eq!(reply.status, 200, "remove accepted: {}", error_text(&reply));

    let v3 = expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, channel).await;
    assert_eq!(
        roster(&v3),
        vec![(owner.pk_hex(), "owner".to_string())],
        "a removed member must leave the roster: {:?}",
        v3.tags
    );
    assert!(
        v3.created_at > v2.created_at,
        "created_at must keep advancing ({} vs {})",
        v3.created_at,
        v2.created_at
    );
    assert!(
        stored_event(&env, &v2.id).await.is_none(),
        "the roster the member was in must not survive their removal"
    );
    assert_eq!(
        stored_ids_of_kind(&env, kind::KIND_NIP29_GROUP_MEMBERS).await,
        vec![to_hex(&v3.id)]
    );
    // The removed member's own membership row agrees (the source of truth the
    // projection is derived from).
    assert!(!membership(&env, channel, &newcomer)
        .await
        .expect("the row is kept, soft-removed")
        .is_live());

    // Supersession left NO index row behind. This is the other half of the
    // channel tag's job: replacement recomputes a superseded event's index keys
    // from that event's own body, so a projection that did not carry its channel
    // would have its timeline row deleted from the wrong bucket — leaving a live
    // row pointing at a body that is gone.
    for key in index_keys(&env, &keys::ts_prefix(COMMUNITY, Some(channel))).await {
        let Some((_, id)) = keys::parse_ts_index(&key) else {
            continue;
        };
        assert!(
            stored_event(&env, &id).await.is_some(),
            "a timeline row outlived its event body: {}",
            String::from_utf8_lossy(&key)
        );
    }

    // The admin list tracked the same changes and still names one owner.
    let admins = expect_projection(&env, kind::KIND_NIP29_GROUP_ADMINS, &relay, channel).await;
    assert_eq!(roster(&admins), vec![(owner.pk_hex(), "owner".to_string())]);
    assert_eq!(
        stored_ids_of_kind(&env, kind::KIND_NIP29_GROUP_ADMINS).await,
        vec![to_hex(&admins.id)],
        "the admin cell converged too"
    );

    });
}

/// **A private room's roster is member-only; an open room's is not.** Driven
/// over the SOCKET, because that is the read path buzz's real client uses and
/// the only one that resolves each event's channel from the event itself: a
/// `REQ {kinds:[39002]}` is exactly how a client asks "which rooms are there,
/// and who is in them", and it must answer differently for a member and a
/// stranger — historically AND live.
///
/// The open room is half the point. A gate that only checked the refusal would
/// pass on a relay that refused everyone, and a relay whose channel list is
/// empty for everybody is the bug this whole slice exists to fix.
#[test]
 fn severe_a_private_rooms_roster_is_member_only_on_the_client_socket() {
    common::run_scenario(|| async move {
    let _serial = WS_LOCK.lock().await;
    let env = ws_env().await;
    let relay = relay(); // seeded by `ws_env`, before the app served a request
    let owner = Actor::new("owner");
    let member = Actor::new("member");
    let stranger = Actor::new("stranger");
    let private = "chan-secret";
    let open = "chan-town-square";

    // Both rooms exist with an owner — the state `POST /buzz/api/channels`
    // leaves behind. Seeded rather than posted because this fixture's request
    // queue belongs to the SOCKET face (the gateway binds one queue per route),
    // and the read gate is what is under test here, not the create route.
    seed_channel_with_owner(&env, private, Visibility::Private, &owner).await;
    seed_membership(&env, private, &member, Role::Member).await;
    seed_channel_with_owner(&env, open, Visibility::Open, &owner).await;

    // Two real client commands per room, each publishing one half of what a
    // client reads: a 9002 metadata edit (⇒ 39000) and a 9000 add (⇒ 39002 +
    // 39001). The edit trigger is exercised nowhere else.
    let mut owner_sock = connect_as(&env, &owner).await;
    for (channel, name) in [(private, "secret room"), (open, "town square")] {
        let rename = signed(
            &owner,
            kind::KIND_NIP29_EDIT_METADATA,
            vec![tag("h", channel), tag("name", name)],
            "",
        );
        publish_ok(&mut owner_sock, &rename).await;
        let add = signed(
            &owner,
            kind::KIND_NIP29_PUT_USER,
            member_tags(channel, &member, Some(Role::Member)),
            "",
        );
        publish_ok(&mut owner_sock, &add).await;
    }

    let private_roster =
        expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, private).await;
    let private_meta =
        expect_projection(&env, kind::KIND_NIP29_GROUP_METADATA, &relay, private).await;
    let open_roster = expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, open).await;
    let open_meta = expect_projection(&env, kind::KIND_NIP29_GROUP_METADATA, &relay, open).await;
    // The private room says so on the wire — the tag a client uses to decide the
    // room is not for public listing — and the edit's new name is on it.
    assert!(
        has_marker(&private_meta, "private"),
        "{:?}",
        private_meta.tags
    );
    assert!(
        has_marker(&private_meta, "closed"),
        "{:?}",
        private_meta.tags
    );
    assert!(has_marker(&open_meta, "public"), "{:?}", open_meta.tags);
    assert_eq!(
        tag_value(&private_meta, "name").as_deref(),
        Some("secret room"),
        "a 9002 edit must reach the projection a client reads: {:?}",
        private_meta.tags
    );
    assert_eq!(
        tag_value(&open_meta, "name").as_deref(),
        Some("town square")
    );

    // ---- the stranger's own view of the workspace -------------------------
    let mut stranger_sock = connect_as(&env, &stranger).await;
    let stranger_sub = "sub-stranger";
    let seen = stored_page(
        &mut stranger_sock,
        &json!(["REQ", stranger_sub, { "kinds": [
            kind::KIND_NIP29_GROUP_METADATA,
            kind::KIND_NIP29_GROUP_MEMBERS
        ], "limit": 50 }]),
        stranger_sub,
    )
    .await;
    assert!(
        seen.contains(&to_hex(&open_roster.id)) && seen.contains(&to_hex(&open_meta.id)),
        "an open room's metadata and roster are community-readable: {seen:?}"
    );
    assert!(
        !seen.contains(&to_hex(&private_roster.id)),
        "a non-member READ a private channel's member list: {seen:?}"
    );
    assert!(
        !seen.contains(&to_hex(&private_meta.id)),
        "a non-member READ a private channel's metadata: {seen:?}"
    );

    // ---- the member's view ------------------------------------------------
    let mut member_sock = connect_as(&env, &member).await;
    let member_sub = "sub-member";
    let seen = stored_page(
        &mut member_sock,
        &json!(["REQ", member_sub, { "kinds": [
            kind::KIND_NIP29_GROUP_METADATA,
            kind::KIND_NIP29_GROUP_MEMBERS
        ], "limit": 50 }]),
        member_sub,
    )
    .await;
    assert!(
        seen.contains(&to_hex(&private_roster.id)) && seen.contains(&to_hex(&private_meta.id)),
        "a member must see their own room: {seen:?}"
    );

    // ---- and live, with both subscriptions already open -------------------
    let third = Actor::new("third");
    let add_third = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(private, &third, Some(Role::Member)),
        "",
    );
    publish_ok(&mut owner_sock, &add_third).await;
    let republished =
        expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, private).await;
    assert_ne!(
        republished.id, private_roster.id,
        "the roster was republished"
    );
    assert_eq!(roster(&republished).len(), 3, "owner + member + third");

    assert!(
        receives_on(
            &mut member_sock,
            member_sub,
            &to_hex(&republished.id),
            ARRIVES
        )
        .await,
        "a member must receive their room's new roster live"
    );
    assert!(
        !receives_on(
            &mut stranger_sock,
            stranger_sub,
            &to_hex(&republished.id),
            NEVER
        )
        .await,
        "a non-member RECEIVED a private channel's member list"
    );

    });
}

/// **No identity configured ⇒ nothing published, everything else unchanged.**
/// The projections are an operator-enabled feature: without the key the
/// workspace must behave exactly as it did before it existed — creates,
/// membership commands and metadata edits all succeed — and not one relay-signed
/// event may appear.
#[test]
 fn severe_without_a_relay_identity_the_workspace_still_works() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    // Deliberately NO seed_relay_identity here.
    let owner = Actor::new("owner");
    let newcomer = Actor::new("newcomer");
    let channel = "chan-no-identity";

    let created = create_room(&env, &owner, channel, "quiet", Visibility::Open).await;
    assert_eq!(
        created.status,
        201,
        "creation must not depend on the relay identity: {}",
        String::from_utf8_lossy(&created.body)
    );

    let add = signed(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        member_tags(channel, &newcomer, Some(Role::Member)),
        "",
    );
    let reply = submit(&env, &owner, &add).await;
    assert_eq!(reply.status, 200, "add accepted: {}", error_text(&reply));
    assert!(
        membership(&env, channel, &newcomer)
            .await
            .expect("row")
            .is_live(),
        "the membership itself still materializes"
    );

    let rename = signed(
        &owner,
        kind::KIND_NIP29_EDIT_METADATA,
        vec![tag("h", channel), tag("name", "renamed")],
        "",
    );
    let reply = submit(&env, &owner, &rename).await;
    assert_eq!(reply.status, 200, "edit accepted: {}", error_text(&reply));

    // An ordinary message still flows, which is the real test of "nothing else
    // changed": the write path resolves no identity for it at all.
    let note = signed(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "hello",
    );
    assert_eq!(submit(&env, &owner, &note).await.status, 200);
    assert!(stored_event(&env, &note.id).await.is_some());

    // And not one projection exists — under ANY author, so this cannot be
    // fooled by looking for the wrong pubkey.
    for kind_ in [
        kind::KIND_NIP29_GROUP_METADATA,
        kind::KIND_NIP29_GROUP_ADMINS,
        kind::KIND_NIP29_GROUP_MEMBERS,
    ] {
        assert_eq!(
            stored_ids_of_kind(&env, kind_).await,
            Vec::<String>::new(),
            "kind:{kind_} must not be published without a relay identity"
        );
        assert!(
            projection_head(&env, kind_, &relay(), channel)
                .await
                .is_none(),
            "no kind:{kind_} watermark may exist either"
        );
    }
    assert!(
        !feed_records(&env, FEED_GENERATION)
            .await
            .iter()
            .any(|r| kind::is_group_projection(r.event.kind)),
        "no projection may reach the delivery feed"
    );

    });
}

// ------------------------------------------------------- the client's socket

/// One ws fleet at a time in this binary: it stands up its own fleet beside the
/// ones [`env`] hands out.
static WS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The relay URL the operator configured, and therefore the one a NIP-42
/// `relay` tag must name.
const RELAY_URL: &str = "wss://buzz.test/buzz/ws/";
const WS_PATH: &str = "/buzz/ws/";

/// The fleet with the ws route's own queue as the driver's request queue — the
/// severe_ws_gate fixture, plus the relay identity.
///
/// Both keys are seeded BEFORE the app runs, so nothing here depends on when a
/// config read happens to land.
async fn ws_env() -> TestEnv {
    let env = TestEnv(TestHarness::new().with_store("nonces")
        .with_queue("requests")
        .with_queue("replies")
        .with_queue("stream-requests")
        .with_queue("ws-requests")
        .with_queue("frames")
        .with_queue("work")
        .with_store("events")
        .with_store("config")
        .with_ape("guards")
        .with_topics("feed")
        .with_blob("media")
        .with_blob("web-assets")
        .request_reply("ws-requests", "replies")
        .stream_requests("stream-requests")
        .frames("frames")
        .manual_clock(T0_MS)
        .timeout(REPLY_BUDGET)
        .build()
        .await);
    {
        let mut config = env.store("config");
        buzz_store::put_raw(&mut config, b"buzz-stream-token-key", &[0x5au8; 32])
            .await
            .expect("stream key seed");
        buzz_store::put_raw(&mut config, b"buzz-relay-url", RELAY_URL.as_bytes())
            .await
            .expect("relay url seed");
    }
    seed_relay_identity(&env).await;
    env.run_app(|ctx| async move { buzz::relay::ingress::serve(ctx).await });
    env
}

/// The next decoded NIP-01 message, or `None` on a timeout or a socket close.
async fn next_message(session: &mut WsSession, budget: Duration) -> Option<Vec<Value>> {
    let frame = tokio::time::timeout(budget, session.recv()).await.ok()??;
    serde_json::from_slice::<Vec<Value>>(&frame.data).ok()
}

/// Wait for the next message whose verb is `verb`, ignoring others.
async fn expect_verb(session: &mut WsSession, verb: &str, budget: Duration) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut seen: Vec<String> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            panic!("no {verb} frame arrived within {budget:?}; frames seen: {seen:?}");
        }
        match next_message(session, left).await {
            Some(message) => {
                if message.first() == Some(&json!(verb)) {
                    return message;
                }
                seen.push(
                    message
                        .first()
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string(),
                );
            }
            None => seen.push("<no frame within the remaining budget>".into()),
        }
    }
}

/// Send one client frame.
async fn send(session: &mut WsSession, message: &Value) {
    session
        .send(message.to_string())
        .await
        .expect("the frame is published");
}

/// Open the real socket, answer its NIP-42 challenge as `actor`, and return the
/// authenticated session.
async fn connect_as(env: &TestEnv, actor: &Actor) -> WsSession {
    let mut session = env
        .ws_open(&ws_path_with_ticket(WS_PATH, env.now_ms()))
        .await
        .expect("the ws face opens");
    let challenge = expect_verb(&mut session, "AUTH", ARRIVES).await;
    let challenge = challenge[1].as_str().expect("a challenge").to_string();
    let auth = signed_at(
        actor,
        kind::KIND_AUTH,
        vec![tag("relay", RELAY_URL), tag("challenge", &challenge)],
        "",
        T0_SECS,
    );
    send(&mut session, &json!(["AUTH", auth])).await;
    let ok = expect_verb(&mut session, "OK", ARRIVES).await;
    assert_eq!(ok[2], json!(true), "NIP-42 must be accepted: {ok:?}");
    session
}

/// Publish `event` over the socket and require acceptance.
async fn publish_ok(session: &mut WsSession, event: &Event) {
    send(session, &json!(["EVENT", event])).await;
    let ok = expect_verb(session, "OK", ARRIVES).await;
    assert_eq!(ok[1], json!(to_hex(&event.id)), "the OK names the event");
    assert_eq!(ok[2], json!(true), "the event must be accepted: {ok:?}");
}

/// The event ids one REQ serves before its EOSE. A `CLOSED` is a failure, not an
/// empty page: a refused subscription and an authorized-but-empty one are very
/// different answers.
async fn stored_page(
    session: &mut WsSession,
    req: &Value,
    sub_id: &str,
) -> std::collections::BTreeSet<String> {
    send(session, req).await;
    let deadline = tokio::time::Instant::now() + ARRIVES;
    let mut ids = std::collections::BTreeSet::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            panic!("no EOSE for {sub_id} within {ARRIVES:?}");
        }
        let Some(message) = next_message(session, left).await else {
            continue;
        };
        match message.first().and_then(Value::as_str) {
            Some("EVENT") if message[1] == json!(sub_id) => {
                ids.insert(message[2]["id"].as_str().unwrap_or_default().to_string());
            }
            Some("EOSE") if message[1] == json!(sub_id) => return ids,
            Some("CLOSED") if message[1] == json!(sub_id) => {
                panic!("the subscription was closed instead of served: {message:?}")
            }
            _ => continue,
        }
    }
}

/// Whether `event_id` arrives on `sub_id` within `budget`.
async fn receives_on(
    session: &mut WsSession,
    sub_id: &str,
    event_id: &str,
    budget: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        let Some(message) = next_message(session, left).await else {
            continue;
        };
        if message.first() == Some(&json!("EVENT"))
            && message[1] == json!(sub_id)
            && message[2]["id"] == json!(event_id)
        {
            return true;
        }
    }
}
