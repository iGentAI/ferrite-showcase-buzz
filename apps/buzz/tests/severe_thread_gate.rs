//! **M1.4 — thread metadata materialization, decided by the WIRING.**
//!
//! A threaded reply bumps its parent's `reply_count` (direct children) and its
//! root's `descendant_count` (whole subtree), and derives its depth from its
//! parent. Every case posts real NIP-10-threaded messages through `POST
//! /buzz/api/events` and reads the durable thread metadata back.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::Visibility;
use buzz::relay::effects::ThreadMeta;
use buzz::relay::ingest;
use buzz::relay::{crypto, keys, kind, store, DEFAULT_COMMUNITY as COMMUNITY};
use common::{
    Actor, FEED_GENERATION, T0_MS, T0_SECS, gate_env, error_text, seed_channel_with_owner, signed_at,
    stored_event, submit, tag,
};
use common::SsoTestEnv as TestEnv;

async fn meta(env: &TestEnv, event_hex: &str) -> Option<ThreadMeta> {
    let mut events = env.store("events");
    let key = keys::thread_meta(COMMUNITY, event_hex);
    store::get_doc::<ThreadMeta>(&mut events, &key)
        .await
        .expect("thread meta read")
}

/// `root`/`reply` markers pointing at the given ids.
fn reply_tags(channel: &str, root: &str, parent: &str) -> Vec<Vec<String>> {
    vec![
        tag("h", channel),
        vec!["e".into(), root.into(), String::new(), "root".into()],
        vec!["e".into(), parent.into(), String::new(), "reply".into()],
    ]
}

async fn post(env: &TestEnv, actor: &Actor, kind_: u32, tags: Vec<Vec<String>>, at: u64) -> String {
    let ev = signed_at(actor, kind_, tags, "msg", at);
    let reply = submit(env, actor, &ev).await;
    assert_eq!(reply.status, 200, "post accepted: {}", error_text(&reply));
    crypto::to_hex(&ev.id)
}

/// A reply whose child's effect ran before its own still ends up counted and
/// upgraded from the stub the child minted for it.
#[test]
fn severe_a_reply_whose_child_ran_first_is_counted_and_upgraded() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-stub";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;

    let root = post(&env, &owner, kind::KIND_STREAM_MESSAGE, vec![tag("h", channel)], T0_SECS).await;

    let parent_ev = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        reply_tags(channel, &root, &root),
        "msg",
        T0_SECS + 1,
    );
    let parent = crypto::to_hex(&parent_ev.id);
    let outcome = {
        let mut ape = env.ape("guards");
        let outcome = ingest::admit(&mut ape, COMMUNITY, &parent_ev, Some(channel.into()), T0_MS)
            .await
            .expect("admission");
        let mut events = env.store("events");
        ingest::apply_storage(&mut events, COMMUNITY, &parent_ev, Some(channel.into()))
            .await
            .expect("storage");
        outcome
    };

    post(&env, &owner, kind::KIND_STREAM_MESSAGE, reply_tags(channel, &root, &parent), T0_SECS + 2).await;
    let stub = meta(&env, &parent).await.expect("the child minted a stub row");
    assert_eq!(stub.parent, None, "a stub names no parent yet");

    {
        let mut ape = env.ape("guards");
        let mut events = env.store("events");
        let mut topics = env.topics("feed");
        ingest::drive(
            &mut events,
            &mut ape,
            &mut topics,
            COMMUNITY,
            FEED_GENERATION,
            &parent_ev,
            &outcome,
            T0_MS,
            None,
        )
        .await
        .expect("drive");
    }

    let parent_meta = meta(&env, &parent).await.expect("parent metadata");
    assert_eq!(parent_meta.parent.as_deref(), Some(root.as_str()));
    assert_eq!(parent_meta.depth, 1);
    assert_eq!(parent_meta.reply_count, 1, "the child's count survives the upgrade");
    let root_meta = meta(&env, &root).await.expect("root metadata");
    assert_eq!(root_meta.reply_count, 1, "the parent is a direct child of the root");
    assert_eq!(root_meta.descendant_count, 2, "parent and child are in the subtree");
    });
}

/// A reply row written before counting became recoverable already had its
/// counters applied; re-driving it adds nothing.
#[test]
fn severe_a_legacy_reply_row_is_not_counted_again_on_redrive() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-legacy";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    let root = post(&env, &owner, kind::KIND_STREAM_MESSAGE, vec![tag("h", channel)], T0_SECS).await;

    let reply_ev = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        reply_tags(channel, &root, &root),
        "msg",
        T0_SECS + 1,
    );
    let reply = crypto::to_hex(&reply_ev.id);
    let outcome = {
        let mut ape = env.ape("guards");
        let outcome = ingest::admit(&mut ape, COMMUNITY, &reply_ev, Some(channel.into()), T0_MS)
            .await
            .expect("admission");
        let mut events = env.store("events");
        ingest::apply_storage(&mut events, COMMUNITY, &reply_ev, Some(channel.into()))
            .await
            .expect("storage");
        let legacy_reply = ThreadMeta {
            root: root.clone(),
            parent: Some(root.clone()),
            depth: 1,
            ..ThreadMeta::default()
        };
        let legacy_root = ThreadMeta {
            root: root.clone(),
            reply_count: 1,
            descendant_count: 1,
            ..ThreadMeta::default()
        };
        store::put_doc(&mut events, &keys::thread_meta(COMMUNITY, &reply), &legacy_reply)
            .await
            .expect("legacy reply row");
        store::put_doc(&mut events, &keys::thread_meta(COMMUNITY, &root), &legacy_root)
            .await
            .expect("legacy root row");
        outcome
    };

    {
        let mut ape = env.ape("guards");
        let mut events = env.store("events");
        let mut topics = env.topics("feed");
        ingest::drive(
            &mut events,
            &mut ape,
            &mut topics,
            COMMUNITY,
            FEED_GENERATION,
            &reply_ev,
            &outcome,
            T0_MS,
            None,
        )
        .await
        .expect("drive");
    }

    let root_meta = meta(&env, &root).await.expect("root metadata");
    assert_eq!((root_meta.reply_count, root_meta.descendant_count), (1, 1));
    });
}

/// A root with two direct replies and one nested reply: reply_count counts direct
/// children, descendant_count counts the whole subtree, depth increments.
#[test]
 fn severe_reply_and_descendant_counters() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-thread";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;

    let root = post(
        &env,
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        T0_SECS,
    )
    .await;

    // Two direct replies to the root.
    let r1 = post(
        &env,
        &owner,
        kind::KIND_STREAM_MESSAGE,
        reply_tags(channel, &root, &root),
        T0_SECS + 1,
    )
    .await;
    let _r2 = post(
        &env,
        &owner,
        kind::KIND_STREAM_MESSAGE,
        reply_tags(channel, &root, &root),
        T0_SECS + 2,
    )
    .await;

    // A nested reply to r1.
    let r3 = post(
        &env,
        &owner,
        kind::KIND_STREAM_MESSAGE,
        reply_tags(channel, &root, &r1),
        T0_SECS + 3,
    )
    .await;

    let root_meta = meta(&env, &root).await.expect("root has metadata");
    assert_eq!(root_meta.reply_count, 2, "two DIRECT children of the root");
    assert_eq!(root_meta.descendant_count, 3, "three events in the subtree");
    assert_eq!(root_meta.depth, 0, "the root sits at depth 0");

    let r1_meta = meta(&env, &r1).await.expect("r1 has metadata");
    assert_eq!(r1_meta.depth, 1, "a direct reply is depth 1");
    assert_eq!(r1_meta.reply_count, 1, "r1 has one direct child (r3)");

    let r3_meta = meta(&env, &r3).await.expect("r3 has metadata");
    assert_eq!(r3_meta.depth, 2, "a reply-to-a-reply is depth 2");
    assert_eq!(r3_meta.root, root, "the nested reply carries the true root");

    // The summary read matches.
    let mut events = env.store("events");
    let (reply_count, descendant_count, _last) =
        buzz::relay::effects::thread_summary(&mut events, COMMUNITY, &root)
            .await
            .unwrap()
            .expect("root summary");
    assert_eq!((reply_count, descendant_count), (2, 3));

    });
}

/// A plain message with no reply markers gets no thread metadata (it is not a
/// reply), and re-driving a reply does not double-count.
#[test]
 fn severe_non_reply_has_no_metadata_and_counts_are_idempotent() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-thread-idem";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;

    let root = post(
        &env,
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        T0_SECS,
    )
    .await;
    assert!(
        meta(&env, &root).await.is_none(),
        "an unreplied message has no thread row"
    );

    let reply = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        reply_tags(channel, &root, &root),
        "msg",
        T0_SECS + 1,
    );
    assert_eq!(submit(&env, &owner, &reply).await.status, 200);
    // Re-submit the identical reply (same id): absorbed, no double-count.
    assert_eq!(submit(&env, &owner, &reply).await.status, 200);

    let root_meta = meta(&env, &root).await.expect("root now has a row");
    assert_eq!(
        root_meta.reply_count, 1,
        "the re-driven reply did not double-count"
    );
    assert_eq!(root_meta.descendant_count, 1);

    });
}

/// **An over-deep reply is refused before storage, never after.**
///
/// The depth limit used to be enforced only inside the saga's effect step, which
/// runs after the event body and its indexes are written. The author was told
/// `invalid:`, yet the event was stored and served by reads while its audit
/// entry, feed publish and saga completion never happened. The refusal now
/// comes before admission, so a refused reply leaves nothing behind.
#[test]
 fn severe_an_over_deep_reply_is_refused_before_it_is_stored() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-thread-depth";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;

    let root = post(
        &env,
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        T0_SECS,
    )
    .await;
    let deep = post(
        &env,
        &owner,
        kind::KIND_STREAM_MESSAGE,
        reply_tags(channel, &root, &root),
        T0_SECS + 1,
    )
    .await;
    // Stand `deep` at the limit rather than posting a hundred replies, which the
    // per-author rate limit would stretch over minutes.
    let mut at_limit = meta(&env, &deep).await.expect("the reply has metadata");
    at_limit.depth = buzz::relay::effects::MAX_THREAD_DEPTH;
    let mut events = env.store("events");
    store::put_doc(&mut events, &keys::thread_meta(COMMUNITY, &deep), &at_limit)
        .await
        .expect("seed the depth");

    let too_deep = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        reply_tags(channel, &root, &deep),
        "one level too far",
        T0_SECS + 2,
    );
    let reply = submit(&env, &owner, &too_deep).await;
    assert_ne!(reply.status, 200, "the reply is refused");
    assert!(
        error_text(&reply).contains("thread depth limit exceeded"),
        "{}",
        error_text(&reply)
    );
    assert!(
        stored_event(&env, &too_deep.id).await.is_none(),
        "a refused reply must not be stored and served"
    );
    assert!(
        meta(&env, &crypto::to_hex(&too_deep.id)).await.is_none(),
        "and gets no thread row"
    );
    let parent = meta(&env, &deep).await.expect("the parent row");
    assert_eq!(parent.reply_count, 0, "the parent's counters are untouched");

    // A reply AT the limit is still accepted: the refusal is the limit, not a
    // side effect of the seeded row.
    let mut below = at_limit.clone();
    below.depth = buzz::relay::effects::MAX_THREAD_DEPTH - 1;
    store::put_doc(&mut events, &keys::thread_meta(COMMUNITY, &deep), &below)
        .await
        .expect("seed the depth");
    let at_the_limit = post(
        &env,
        &owner,
        kind::KIND_STREAM_MESSAGE,
        reply_tags(channel, &root, &deep),
        T0_SECS + 3,
    )
    .await;
    assert_eq!(
        meta(&env, &at_the_limit).await.expect("a thread row").depth,
        buzz::relay::effects::MAX_THREAD_DEPTH
    );

    });
}

/// **The thread effect counts a reply once however often it is applied.**
///
/// The saga now runs its effect on every drive of an incomplete saga (a torn
/// saga's re-drive, a concurrent duplicate), so the counters cannot rely on the
/// saga to apply them once. The driver that creates the reply's own thread row
/// is the one that bumps; every later application finds the row and counts
/// nothing.
#[test]
 fn severe_the_thread_effect_counts_a_reply_once_when_reapplied() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-thread-reapply";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    let root = post(
        &env,
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        T0_SECS,
    )
    .await;
    let reply = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        reply_tags(channel, &root, &root),
        "counted once",
        T0_SECS + 1,
    );
    let first = submit(&env, &owner, &reply).await;
    assert_eq!(first.status, 200, "{}", error_text(&first));

    let mut events = env.store("events");
    let mut ape = env.ape("guards");
    for _ in 0..2 {
        buzz::relay::effects::apply_effects(
            &mut events,
            &mut ape,
            COMMUNITY,
            &reply,
            Some(channel),
            T0_MS + 5_000,
        )
        .await
        .expect("re-applied");
    }
    let root_meta = meta(&env, &root).await.expect("root metadata");
    assert_eq!(root_meta.reply_count, 1, "one direct reply, counted once");
    assert_eq!(root_meta.descendant_count, 1);

    });
}

/// **A reply cannot move another room's thread counters, or invent threads.**
///
/// The root and parent ids are the author's own `e` tags. A member of an open
/// room used to be able to bump the counters on a private room's message and
/// mint thread rows for ids that name nothing. Only ancestry that is a stored
/// event in the reply's own channel is threaded now; anything else is stored as
/// a plain message and touches no thread row.
#[test]
 fn severe_a_reply_threads_only_under_events_in_its_own_channel() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let insider = Actor::new("thread-insider");
    let outsider = Actor::new("thread-outsider");
    seed_channel_with_owner(&env, "chan-private-thread", Visibility::Private, &insider).await;
    seed_channel_with_owner(&env, "chan-open-thread", Visibility::Open, &outsider).await;

    let secret = post(
        &env,
        &insider,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", "chan-private-thread")],
        T0_SECS,
    )
    .await;
    // Give the private message a thread row, so a bump would be visible.
    let inner = post(
        &env,
        &insider,
        kind::KIND_STREAM_MESSAGE,
        reply_tags("chan-private-thread", &secret, &secret),
        T0_SECS + 1,
    )
    .await;
    let before = meta(&env, &secret).await.expect("the private root's row");
    assert_eq!(before.reply_count, 1);

    // A reply written into the OPEN room that names the private message.
    let foreign = post(
        &env,
        &outsider,
        kind::KIND_STREAM_MESSAGE,
        reply_tags("chan-open-thread", &secret, &inner),
        T0_SECS + 2,
    )
    .await;
    assert_eq!(
        meta(&env, &secret).await.expect("row"),
        before,
        "a reply in another room moved a private thread's counters"
    );
    assert_eq!(meta(&env, &inner).await.expect("row").reply_count, 0);
    assert!(
        meta(&env, &foreign).await.is_none(),
        "a cross-room reply must join no thread"
    );

    // Ids that name no event at all mint no rows.
    let ghost = crypto::to_hex(&[0x42u8; 32]);
    let orphan = post(
        &env,
        &outsider,
        kind::KIND_STREAM_MESSAGE,
        reply_tags("chan-open-thread", &ghost, &ghost),
        T0_SECS + 3,
    )
    .await;
    assert!(meta(&env, &ghost).await.is_none(), "a stub row was minted for a ghost id");
    assert!(meta(&env, &orphan).await.is_none());

    });
}
