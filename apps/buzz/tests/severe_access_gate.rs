//! **S15 — access-boundary red team.** Who may read what, decided by the WIRING
//! and not by the policy function.
//!
//! `access::may_deliver` has its own unit tests; they prove the policy is right
//! about facts handed to it. That is not the property that keeps a private message
//! private. The property that does is: *the route resolves the facts correctly, on
//! every read, and refuses when it cannot resolve them.* So every case here goes
//! through `POST /buzz/api/query` on a live fleet and asserts on what the reply
//! contains — a leak here is a leak in production.
//!
//! The fixtures seed channel documents and membership rows directly, through the
//! same key builders and planes the app uses (`guard/ch/...` on the guards plane,
//! `mem/...` in the events store), rather than posting them through the creation
//! route: the boundary under test is the READ gate, and seeding keeps a gate from
//! failing for a reason that belongs to a different route. Nothing about the gate
//! is bypassed — the route still resolves visibility and membership from those
//! planes on every single request.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::{Role, Visibility};
use buzz::relay::crypto::to_hex;
use buzz::relay::kind;
use common::{
    Actor, channel_doc, channel_query, corrupt_channel, gate_env, error_text, query, returned,
    returned_ids, seed_channel, seed_membership, seed_removed_membership, signed, stored_event,
    submit, tag,
};
use ferrite_app::serde_json::{self, json};

/// A private channel's message reaches a live member and nobody else — not a
/// stranger, not a removed member, and not even its own author without a
/// membership row.
#[test]
 fn severe_a_private_channel_message_reaches_members_only() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let author = Actor::new("author");
    let member = Actor::new("member");
    let stranger = Actor::new("stranger");
    let ex_member = Actor::new("ex-member");
    let channel = "chan-secret";

    seed_channel(&env, channel, Visibility::Private, &author).await;
    seed_membership(&env, channel, &author, Role::Owner).await;
    seed_membership(&env, channel, &member, Role::Member).await;
    seed_removed_membership(&env, channel, &ex_member).await;

    let secret = signed(
        &author,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "the merger closes friday",
    );
    assert_eq!(
        submit(&env, &author, &secret).await.status,
        200,
        "the write path admitted the event"
    );
    // It IS stored and indexed: the gate is on delivery, not on storage.
    assert!(stored_event(&env, &secret.id).await.is_some());

    // The member sees it.
    let seen = query(&env, &member, &channel_query(channel)).await;
    assert_eq!(seen.status, 200, "{}", String::from_utf8_lossy(&seen.body));
    assert!(
        returned(&seen, &secret),
        "a live member was denied their own channel"
    );

    // The author sees it (owner membership).
    assert!(returned(
        &query(&env, &author, &channel_query(channel)).await,
        &secret
    ));

    // The stranger does not — and gets an ordinary empty page, not an error that
    // would confirm the channel exists.
    let denied = query(&env, &stranger, &channel_query(channel)).await;
    assert_eq!(
        denied.status,
        200,
        "{}",
        String::from_utf8_lossy(&denied.body)
    );
    assert_eq!(
        returned_ids(&denied),
        Vec::<String>::new(),
        "a non-member read a private channel"
    );
    assert!(
        !String::from_utf8_lossy(&denied.body).contains(&secret.content),
        "the content leaked into a non-member's reply"
    );

    // The removed member reads like a stranger.
    let removed = query(&env, &ex_member, &channel_query(channel)).await;
    assert_eq!(
        returned_ids(&removed),
        Vec::<String>::new(),
        "a removed member still reads the channel"
    );

    // The obvious evasion: drop the `channel` from the query and read the
    // community-global timeline instead. A channel event is not indexed there, so
    // there is nothing to gate — and the stranger still gets nothing.
    let stripped = query(&env, &stranger, &json!({"limit": 100})).await;
    assert_eq!(
        stripped.status,
        200,
        "{}",
        String::from_utf8_lossy(&stripped.body)
    );
    assert_eq!(
        returned_ids(&stripped),
        Vec::<String>::new(),
        "a private channel's event was reachable from the global timeline"
    );

    // …and asking for the channel under a different spelling reaches a different
    // (empty, unresolvable) bucket rather than the real one.
    for alias in ["CHAN-SECRET", "chan-secret ", "./chan-secret"] {
        let page = query(&env, &stranger, &channel_query(alias)).await;
        assert_eq!(
            returned_ids(&page),
            Vec::<String>::new(),
            "the alias {alias:?} resolved onto a private channel"
        );
    }

    });
}

/// The author of a private-channel message is refused too when no membership row
/// backs the claim: the gate authorizes on resolved facts, not on authorship.
#[test]
 fn severe_a_private_channel_denies_an_author_without_membership() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let author = Actor::new("author");
    let channel = "chan-no-rows";
    seed_channel(&env, channel, Visibility::Private, &author).await;

    let event = signed(
        &author,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "wrote it, cannot read it",
    );

    // The WRITE is refused first: membership is the gate on both paths, so an
    // author with no membership row cannot put anything into the room to begin
    // with. (Before the write gate existed this returned 200 and the event was
    // only hidden on the read side — a private room anyone could post into.)
    //
    // 403 and not 401: this caller authenticated perfectly. They are refused for
    // WHO THEY ARE, and the reply body has always said so — "restricted: not a
    // member of this channel" — while the status used to tell a standard client to
    // go and authenticate again, which could never help.
    let refused = submit(&env, &author, &event).await;
    assert_eq!(
        refused.status,
        403,
        "a non-member's write into a private channel was admitted: {}",
        String::from_utf8_lossy(&refused.body)
    );
    assert!(
        stored_event(&env, &event.id).await.is_none(),
        "a refused write still stored its event"
    );

    // Adding the membership row flips it — proving the refusal was the membership
    // fact and not something incidental — and the author can then read it back.
    seed_membership(&env, channel, &author, Role::Owner).await;
    assert_eq!(submit(&env, &author, &event).await.status, 200);
    let page = query(&env, &author, &channel_query(channel)).await;
    assert!(returned(&page, &event));

    });
}

/// With NO channel document, the read fails closed for everyone — including a
/// member whose membership row is right there. An unknown channel is not an open
/// channel.
#[test]
 fn severe_a_missing_channel_document_denies_everyone() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let author = Actor::new("author");
    let member = Actor::new("member");
    let channel = "chan-undocumented";

    // Memberships exist; the channel document does not.
    seed_membership(&env, channel, &member, Role::Member).await;
    seed_membership(&env, channel, &author, Role::Owner).await;
    assert!(
        channel_doc(&env, channel).await.is_none(),
        "no document yet"
    );

    let event = signed(
        &author,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "written into an undocumented channel",
    );

    // An undocumented channel is refused on the WRITE path — a membership row
    // alone is not a room.
    assert_eq!(
        submit(&env, &author, &event).await.status,
        404,
        "a write into an undocumented channel was admitted"
    );
    assert!(stored_event(&env, &event.id).await.is_none());

    // To exercise the READ gate against a document that cannot be resolved, give
    // the room a document, write through it, then corrupt the document — a bad
    // migration or a truncated restore. (APE records never self-delete, so
    // "present but unreadable" is the reachable shape of this failure, and it is
    // the one the gate must treat exactly like "absent".)
    seed_channel(&env, channel, Visibility::Private, &author).await;
    assert_eq!(submit(&env, &author, &event).await.status, 200);
    assert!(stored_event(&env, &event.id).await.is_some());
    corrupt_channel(&env, channel).await;
    assert!(
        channel_doc(&env, channel).await.is_some(),
        "document present"
    );

    for (who, reader) in [("member", &member), ("owner", &author)] {
        let page = query(&env, reader, &channel_query(channel)).await;
        assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));
        assert_eq!(
            returned_ids(&page),
            Vec::<String>::new(),
            "{who} read a channel whose document could not be resolved"
        );
    }

    // Writing a REAL document flips the same query to allowed — so the two
    // refusals above were the resolver failing closed, not the query being broken.
    seed_channel(&env, channel, Visibility::Private, &author).await;
    assert!(returned(
        &query(&env, &member, &channel_query(channel)).await,
        &event
    ));
    // …and still denies a stranger.
    let stranger = Actor::new("stranger");
    assert!(!returned(
        &query(&env, &stranger, &channel_query(channel)).await,
        &event
    ));

    });
}

/// A channel id that cannot even be spelled as an authorization key resolves to
/// "unresolved" and therefore denies, rather than resolving to some other key.
#[test]
 fn severe_an_unspellable_channel_id_denies_rather_than_resolving() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let author = Actor::new("author");
    // A `/` cannot appear in a key component, so `guard_channel` refuses it.
    let hostile = "chan/../../guard/ch/main/chan-open";
    let open = "chan-open";
    seed_channel(&env, open, Visibility::Open, &author).await;

    let event = signed(
        &author,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", hostile)],
        "traversal attempt",
    );

    // The id cannot be spelled as an authorization key, so the write cannot be
    // authorized: refused before storage, rather than admitted into a bucket whose
    // access nobody can resolve.
    let refused = submit(&env, &author, &event).await;
    assert!(
        refused.status == 400 || refused.status == 404,
        "a traversal-shaped channel id was admitted with status {}: {}",
        refused.status,
        String::from_utf8_lossy(&refused.body)
    );
    assert!(stored_event(&env, &event.id).await.is_none());

    // Asking for the hostile channel: the id escapes into its own bucket, and the
    // guard key cannot be built, so the answer is empty rather than the open
    // channel's contents.
    let page = query(&env, &author, &channel_query(hostile)).await;
    assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));
    assert_eq!(
        returned_ids(&page),
        Vec::<String>::new(),
        "a traversal-shaped channel id resolved to an open channel"
    );

    // …and the open channel it tried to impersonate is unaffected.
    let page = query(&env, &author, &channel_query(open)).await;
    assert_eq!(returned_ids(&page), Vec::<String>::new());

    });
}

/// An author-only kind (30300, a reminder) never reaches a non-author, even
/// though it is stored globally and the global timeline is otherwise readable by
/// anyone.
#[test]
 fn severe_an_author_only_kind_never_reaches_a_non_author() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let snoop = Actor::new("snoop");

    let reminder = signed(
        &owner,
        kind::KIND_EVENT_REMINDER,
        vec![tag("d", "standup"), tag("p", &snoop.pk_hex())],
        "call the lawyer at 4",
    );
    assert!(kind::is_author_only(reminder.kind));
    assert_eq!(submit(&env, &owner, &reminder).await.status, 200);

    // A readable event beside it, so an empty reply cannot be mistaken for "the
    // query returns nothing at all". It must be a CHANNEL-LESS kind: a global
    // query returns only channel-less events, because buzz's scoping invariant is
    // symmetric — global subscriptions never receive channel events and channel
    // subscriptions never receive global ones.
    let public = signed(&owner, kind::KIND_TEXT_NOTE, vec![], "good morning");
    assert!(kind::is_global_only_kind(public.kind));
    assert_eq!(submit(&env, &owner, &public).await.status, 200);

    let mine = query(&env, &owner, &json!({})).await;
    assert!(
        returned(&mine, &reminder),
        "the author cannot read their own reminder"
    );
    assert!(returned(&mine, &public));

    let theirs = query(&env, &snoop, &json!({})).await;
    assert_eq!(
        returned_ids(&theirs),
        vec![to_hex(&public.id)],
        "an author-only kind reached a non-author (a p tag is not a grant here)"
    );
    assert!(!String::from_utf8_lossy(&theirs.body).contains("call the lawyer"));

    });
}

/// **Sync 002 port.** A NIP-PMA private managed-agent aggregate (30179) is
/// author-only exactly as upstream classifies it: its content is the owner's
/// ciphertext, but its outer tags name the agent coordinate and lifecycle state,
/// and before this port an unregistered 30179 was accepted here as an ordinary
/// world-readable event. The non-author is refused on the global scan AND on a
/// by-id read, because knowing an id must not be a grant either.
#[test]
 fn severe_a_private_managed_agent_aggregate_stays_with_its_owner() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("pma-owner");
    let snoop = Actor::new("pma-snoop");
    let agent = Actor::new("pma-agent");

    let aggregate = signed(
        &owner,
        kind::KIND_PRIVATE_MANAGED_AGENT,
        vec![
            tag("d", &agent.pk_hex()),
            tag("state", "active"),
            tag("generation", "1"),
        ],
        "AqQ-nip44-v2-ciphertext-from-owner-to-owner",
    );
    assert!(kind::is_author_only(aggregate.kind));
    assert!(kind::is_global_only_kind(aggregate.kind));
    assert!(kind::is_parameterized_replaceable(aggregate.kind));
    let accepted = submit(&env, &owner, &aggregate).await;
    assert_eq!(accepted.status, 200, "{}", error_text(&accepted));

    // A readable channel-less event beside it, so an empty reply is
    // distinguishable from "the query returns nothing".
    let public = signed(&owner, kind::KIND_TEXT_NOTE, vec![], "hello team");
    assert_eq!(submit(&env, &owner, &public).await.status, 200);

    let mine = query(&env, &owner, &json!({})).await;
    assert!(
        returned(&mine, &aggregate),
        "the owner cannot read back their own private managed-agent aggregate"
    );
    assert!(returned(&mine, &public));

    let theirs = query(&env, &snoop, &json!({})).await;
    assert_eq!(
        returned_ids(&theirs),
        vec![to_hex(&public.id)],
        "a private managed-agent aggregate reached a non-owner on the global scan"
    );
    assert!(!String::from_utf8_lossy(&theirs.body).contains("nip44-v2-ciphertext"));

    // Nor by id: an id is not a capability for an author-only kind.
    let by_id = query(
        &env,
        &snoop,
        &json!({ "ids": [to_hex(&aggregate.id)] }),
    )
    .await;
    assert!(
        !returned(&by_id, &aggregate),
        "a private managed-agent aggregate reached a non-owner by id: {}",
        String::from_utf8_lossy(&by_id.body)
    );

    });
}

/// A `#p`-gated kind (1059, a gift wrap) reaches the author and the addressed
/// pubkey only.
#[test]
 fn severe_a_p_gated_kind_reaches_only_the_addressed_pubkey() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let sender = Actor::new("sender");
    let recipient = Actor::new("recipient");
    let bystander = Actor::new("bystander");

    let wrap = signed(
        &sender,
        kind::KIND_GIFT_WRAP,
        vec![tag("p", &recipient.pk_hex())],
        "AAAA-ciphertext-AAAA",
    );
    assert!(kind::is_p_gated(wrap.kind));
    assert_eq!(submit(&env, &sender, &wrap).await.status, 200);

    // Channel-less, for the same reason as above: this query is a global one.
    let public = signed(&sender, kind::KIND_TEXT_NOTE, vec![], "unclassified");
    assert_eq!(submit(&env, &sender, &public).await.status, 200);

    // The addressee reads it.
    let to_recipient = query(&env, &recipient, &json!({})).await;
    assert!(
        returned(&to_recipient, &wrap),
        "the addressed pubkey was denied its own gift wrap"
    );
    // The author reads it.
    assert!(returned(&query(&env, &sender, &json!({})).await, &wrap));
    // Nobody else does.
    let to_bystander = query(&env, &bystander, &json!({})).await;
    assert_eq!(
        returned_ids(&to_bystander),
        vec![to_hex(&public.id)],
        "a gift wrap reached a third party"
    );
    assert!(!String::from_utf8_lossy(&to_bystander.body).contains("ciphertext"));

    });
}

/// A gift wrap submitted by someone other than its signer is admitted (NIP-59
/// wraps are signed by a throwaway key, so the caller cannot be the author) and
/// still reaches only the addressee.
#[test]
 fn severe_a_gift_wrap_from_an_ephemeral_key_is_admitted_but_still_gated() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ephemeral = Actor::new("ephemeral-wrap-key");
    let submitter = Actor::new("submitter");
    let recipient = Actor::new("recipient");
    let bystander = Actor::new("bystander");

    let wrap = signed(
        &ephemeral,
        kind::KIND_GIFT_WRAP,
        vec![tag("p", &recipient.pk_hex())],
        "BBBB-ciphertext-BBBB",
    );
    // Submitted under a DIFFERENT caller's authorization: allowed only because the
    // kind is `#p`-gated by construction.
    let reply = submit(&env, &submitter, &wrap).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );

    assert!(returned(&query(&env, &recipient, &json!({})).await, &wrap));
    assert!(
        !returned(&query(&env, &bystander, &json!({})).await, &wrap),
        "a relayed gift wrap leaked to a third party"
    );
    assert!(
        !returned(&query(&env, &submitter, &json!({})).await, &wrap),
        "submitting a gift wrap must not grant read access to it"
    );

    });
}

/// A persona (30175) is a private draft until it carries `["shared","true"]`.
#[test]
 fn severe_an_unshared_persona_stays_with_its_author() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let author = Actor::new("author");
    let reader = Actor::new("reader");

    let private = signed(
        &author,
        kind::KIND_PERSONA,
        vec![tag("d", "draft")],
        "internal-only persona",
    );
    let shared = signed(
        &author,
        kind::KIND_PERSONA,
        vec![tag("d", "catalog"), tag("shared", "true")],
        "published persona",
    );
    assert_eq!(submit(&env, &author, &private).await.status, 200);
    assert_eq!(submit(&env, &author, &shared).await.status, 200);

    let mine = returned_ids(&query(&env, &author, &json!({})).await);
    assert!(mine.contains(&to_hex(&private.id)));
    assert!(mine.contains(&to_hex(&shared.id)));

    let theirs = returned_ids(&query(&env, &reader, &json!({})).await);
    assert!(
        theirs.contains(&to_hex(&shared.id)),
        "a shared persona must reach the catalog"
    );
    assert!(
        !theirs.contains(&to_hex(&private.id)),
        "an unshared persona leaked to another reader"
    );
    // A `shared` tag that is not exactly `true` never becomes an event at all:
    // ingest refuses the spelling, because a tag the read gate would silently
    // ignore is a request its author would be right to believe had been understood
    // (`severe_persona_contract_gate`). The read gate's own refusal to treat such a
    // tag as a share is unit-tested in `access::is_unshared_persona`, so the
    // property holds on BOTH sides; this asserts the half an author can observe.
    let almost = signed(
        &author,
        kind::KIND_PERSONA,
        vec![tag("d", "almost"), tag("shared", "yes")],
        "not actually shared",
    );
    let refused = submit(&env, &author, &almost).await;
    assert_eq!(
        refused.status,
        400,
        "a shared tag with no meaning was accepted: {}",
        String::from_utf8_lossy(&refused.body)
    );
    let theirs = returned_ids(&query(&env, &reader, &json!({})).await);
    assert!(!theirs.contains(&to_hex(&almost.id)));

    });
}

/// An open channel is readable by a non-member — the deny cases above are the
/// gate working, not the gate refusing everything.
#[test]
 fn severe_an_open_channel_reaches_a_non_member() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let author = Actor::new("author");
    let outsider = Actor::new("outsider");
    let channel = "chan-town-square";
    seed_channel(&env, channel, Visibility::Open, &author).await;

    let event = signed(
        &author,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "anyone may read this",
    );
    assert_eq!(submit(&env, &author, &event).await.status, 200);

    let page = query(&env, &outsider, &channel_query(channel)).await;
    assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));
    assert!(returned(&page, &event));

    });
}

#[test]
fn severe_reports_and_product_feedback_are_confidential_to_their_author() {
    common::run_scenario(|| async move {
        let env = gate_env().await;
        let reporter = Actor::new("report-author");
        let reported = Actor::new("reported-subject");
        let bystander = Actor::new("report-bystander");

        let report = signed(
            &reporter,
            kind::KIND_REPORT,
            vec![tag("p", &reported.pk_hex())],
            "harassment details in private report",
        );
        let feedback = signed(
            &reporter,
            kind::KIND_PRODUCT_FEEDBACK,
            vec![],
            "private product feedback with /buzz/blob/secret-diag.txt",
        );
        assert_eq!(submit(&env, &reporter, &report).await.status, 200);
        assert_eq!(submit(&env, &reporter, &feedback).await.status, 200);

        let public_note = signed(&reporter, kind::KIND_TEXT_NOTE, vec![], "public note");
        assert_eq!(submit(&env, &reporter, &public_note).await.status, 200);

        // Reporter can read their own submissions.
        let own = query(&env, &reporter, &json!({})).await;
        assert!(returned(&own, &report), "author reads own report");
        assert!(returned(&own, &feedback), "author reads own feedback");

        // Neither the reported subject nor a bystander may read 1984 or 42000 on global timeline.
        for reader in [&reported, &bystander] {
            let page = query(&env, reader, &json!({})).await;
            assert_eq!(
                returned_ids(&page),
                vec![to_hex(&public_note.id)],
                "confidential report or feedback leaked on global query"
            );
            let body = String::from_utf8_lossy(&page.body);
            assert!(!body.contains("harassment details"));
            assert!(!body.contains("secret-diag.txt"));

            // Filter query asking for 1984 or 42000 without constraining authors to self is refused.
            for k in [kind::KIND_REPORT, kind::KIND_PRODUCT_FEEDBACK] {
                let f = env
                    .handle(common::post(
                        reader,
                        "/query",
                        serde_json::to_vec(&json!([{ "kinds": [k] }])).unwrap(),
                    ))
                    .await
                    .expect("filter query reply");
                assert_eq!(
                    f.status, 403,
                    "kind {k} filter query must be refused to non-author: {}",
                    String::from_utf8_lossy(&f.body)
                );
            }

            // Direct ID query via /query also withholds both events.
            let by_ids = env
                .handle(common::post(
                    reader,
                    "/query",
                    serde_json::to_vec(&json!([{
                        "ids": [to_hex(&report.id), to_hex(&feedback.id)]
                    }]))
                    .unwrap(),
                ))
                .await
                .expect("id query reply");
            assert_eq!(by_ids.status, 200);
            let arr: Vec<serde_json::Value> = serde_json::from_slice(&by_ids.body).unwrap();
            assert!(arr.is_empty(), "report/feedback leaked by id: {arr:?}");
        }

        // Reported user's mention index must not contain the abuse report against them.
        let mut events = env.store("events");
        let mentions = buzz::relay::query::by_mention(
            &mut events,
            common::COMMUNITY,
            &reported.pk_hex(),
            None,
            50,
        )
        .await
        .expect("by_mention");
        assert!(
            mentions.events.is_empty(),
            "KIND_REPORT must not be indexed under the reported subject's mention bucket"
        );

        // Stray `h` tags on 1984/42000 are ignored as global-only kinds rather than channel-routed.
        let stray_h = common::signed_at(
            &reporter,
            kind::KIND_REPORT,
            vec![tag("p", &reported.pk_hex()), tag("h", "nonexistent-channel")],
            "report with stray h tag",
            common::T0_SECS + 5,
        );
        assert_eq!(submit(&env, &reporter, &stray_h).await.status, 200);
    });
}

/// Filters the planner could not route to one channel, kind or author
/// index used to read only the channel-less timeline, so every channel event
/// was silently dropped; and an author filter without kinds paged on a
/// kind-major index, so an `until` cursor sorted past every row.
#[test]
fn severe_wide_and_author_filters_still_see_channel_events() {
    common::run_scenario(|| async move {
        let env = gate_env().await;
        let author = Actor::new("wide-author");
        let reader = Actor::new("wide-reader");
        seed_channel(&env, "chan-wide-a", Visibility::Open, &author).await;
        seed_channel(&env, "chan-wide-b", Visibility::Open, &author).await;
        let a = signed(&author, kind::KIND_STREAM_MESSAGE, vec![tag("h", "chan-wide-a")], "in a");
        let b = signed(&author, kind::KIND_STREAM_MESSAGE, vec![tag("h", "chan-wide-b")], "in b");
        assert_eq!(submit(&env, &author, &a).await.status, 200);
        assert_eq!(submit(&env, &author, &b).await.status, 200);

        let ask = |filter: serde_json::Value| {
            let reader = &reader;
            let env = &env;
            async move {
                let reply = env
                    .handle(common::post(reader, "/query", serde_json::to_vec(&json!([filter])).unwrap()))
                    .await
                    .expect("query reply");
                assert_eq!(reply.status, 200, "{}", String::from_utf8_lossy(&reply.body));
                let rows: Vec<serde_json::Value> = serde_json::from_slice(&reply.body).unwrap();
                rows.iter().filter_map(|r| r["id"].as_str().map(str::to_string)).collect::<Vec<_>>()
            }
        };
        let both = |ids: &[String]| ids.contains(&to_hex(&a.id)) && ids.contains(&to_hex(&b.id));

        let ids = ask(json!({ "#h": ["chan-wide-a", "chan-wide-b"] })).await;
        assert!(both(&ids), "a two-channel filter dropped channel events: {ids:?}");
        let ids = ask(json!({ "limit": 50 })).await;
        assert!(both(&ids), "an unnarrowed filter dropped channel events: {ids:?}");
        let ids = ask(json!({ "authors": [author.pk_hex()], "until": a.created_at.max(b.created_at) + 60 })).await;
        assert!(both(&ids), "an author filter with until came back without the author's events: {ids:?}");
    });
}
