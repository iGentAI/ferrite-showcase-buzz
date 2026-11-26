//! Search gates: what the term index finds, and what it must never surface.
//!
//! The index is keyed by TERM alone — a word said in a private room has a row
//! exactly like a word said in the open — so the access gate is the only thing
//! separating them. That is the property worth a gate: an index is the classic
//! place for an access boundary to be forgotten, because its rows look like
//! metadata rather than like content.

use super::common;

use buzz::relay::channel::Visibility;
use buzz::relay::{keys, kind};
use common::SsoTestEnv as TestEnv;
use common::{
    Actor, COMMUNITY, SUBMIT_ATTEMPTS, admission, body_json, gate_env, error_text, index_keys,
    membership, post, query, returned, returned_ids, seed_channel_with_owner, signed, signed_at,
    stored_event, submit, tag,
};
use ferrite_app::serde_json::{self, json, Value};
use ferrite_app::IngressReply;

const SEARCH_PATH: &str = "/buzz/api/search";

/// Run one search as `who`, re-posting if the reply is lost (the frozen-clock
/// re-send discipline `common::query` documents).
async fn search(env: &TestEnv, who: &Actor, body: Value) -> IngressReply {
    let bytes = serde_json::to_vec(&body).expect("encodes");
    for attempt in 1..=SUBMIT_ATTEMPTS {
        match env.handle(post(who, SEARCH_PATH, bytes.clone())).await {
            Ok(reply) => return reply,
            Err(e) if attempt == SUBMIT_ATTEMPTS => panic!("no reply after {SUBMIT_ATTEMPTS}: {e}"),
            Err(_) => continue,
        }
    }
    unreachable!()
}

/// Post one message into `channel` and return it.
async fn say(env: &TestEnv, who: &Actor, channel: &str, text: &str) -> buzz::relay::Event {
    let event = signed(
        who,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        text,
    );
    let reply = submit(env, who, &event).await;
    assert_eq!(reply.status, 200, "submit: {}", error_text(&reply));
    event
}

/// **A message becomes findable, and only by words it actually contains.**
#[test]
 fn severe_a_stored_message_is_found_by_its_own_words() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("search-ana");
    seed_channel_with_owner(&env, "general", Visibility::Open, &ana).await;

    let posted = say(&env, &ana, "general", "The deployment finished at midnight").await;

    let found = search(&env, &ana, json!({ "q": "deployment" })).await;
    assert_eq!(found.status, 200, "{}", error_text(&found));
    assert!(
        returned(&found, &posted),
        "the message was not found by a word it contains"
    );

    // Case folds, because the index and the query share one tokenizer.
    assert!(returned(
        &search(&env, &ana, json!({ "q": "DEPLOYMENT" })).await,
        &posted
    ));

    // A word it does not contain finds nothing — and the answer is COMPLETE, so
    // a caller knows that is the truth rather than a budget running out.
    let miss = search(&env, &ana, json!({ "q": "kubernetes" })).await;
    assert!(!returned(&miss, &posted));
    assert_eq!(
        body_json(&miss).get("complete").and_then(Value::as_bool),
        Some(true),
        "an exhausted walk must say so, or an empty answer is unreadable"
    );

    });
}

/// **Two words mean AND, not OR.**
///
/// The conjunction is the whole reason the index key is computable from
/// `(term, created_at, id)`: membership in a second term's bucket is a point
/// read rather than a merge. If it silently behaved as OR, a two-word search
/// would return everything containing either word.
#[test]
 fn severe_two_terms_are_a_conjunction() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("search-ana");
    seed_channel_with_owner(&env, "general", Visibility::Open, &ana).await;

    let both = say(&env, &ana, "general", "the staging deployment is green").await;
    // Carries EXACTLY ONE of the two terms. That is the whole point: a control
    // sharing NEITHER term would be excluded by an OR implementation too, so the
    // gate would pass against the very bug it names.
    let one_term = say(&env, &ana, "general", "the deployment rollout is green").await;

    // First prove the control is indexed and reachable on its own — otherwise
    // its absence below could mean "not indexed" rather than "AND refused it".
    let single = search(&env, &ana, json!({ "q": "deployment" })).await;
    assert!(
        returned(&single, &one_term),
        "the control is findable by its own term"
    );
    assert!(
        returned(&single, &both),
        "and so is the message carrying both"
    );

    let hits = search(&env, &ana, json!({ "q": "staging deployment" })).await;
    assert!(returned(&hits, &both), "the message with both terms");
    assert!(
        !returned(&hits, &one_term),
        "a message carrying only ONE of the terms must not match a conjunction \
         — under OR it would be returned here"
    );

    // Order does not change the answer — only which bucket drives the walk.
    let reversed = search(&env, &ana, json!({ "q": "deployment staging" })).await;
    assert_eq!(returned_ids(&hits), returned_ids(&reversed));

    });
}

/// **A private room's words do not surface to a non-member.**
///
/// The index cannot make this distinction — its rows carry a term, a time and an
/// id, and nothing about who may read them. Only the per-hit access gate does,
/// which is why it is re-run for every candidate rather than trusted from the
/// index.
#[test]
 fn severe_a_private_rooms_words_are_not_searchable_by_an_outsider() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("search-ana");
    let outsider = Actor::new("search-outsider");
    seed_channel_with_owner(&env, "secret", Visibility::Private, &ana).await;

    let secret = say(&env, &ana, "secret", "the acquisition closes on tuesday").await;

    // The member finds it.
    let mine = search(&env, &ana, json!({ "q": "acquisition" })).await;
    assert!(
        returned(&mine, &secret),
        "a member must find their own room"
    );

    // The outsider does not — though the index row exists and their query is
    // identical.
    let theirs = search(&env, &outsider, json!({ "q": "acquisition" })).await;
    assert_eq!(theirs.status, 200, "the search itself is allowed");
    assert!(
        !returned(&theirs, &secret),
        "a private room's content leaked through the search index"
    );

    // The index row IS there — proving the gate, not an absent index, is what
    // withheld it.
    let rows = index_keys(&env, &keys::search_prefix(COMMUNITY, "acquisition")).await;
    assert_eq!(
        rows.len(),
        1,
        "the term was indexed regardless of visibility"
    );

    });
}

/// **A search cursor says nothing about the rows it points at.**
///
/// The walk picks the resume position before the access gate runs, and it used
/// to be the plain hex of an index key, `sidx/{c}/{term}/{inv_ts}/{id}`. So a
/// page whose hits were ALL private handed a non-member the event id and exact
/// timestamp of messages they cannot read. The cursor is now sealed: a member
/// still pages with it, an outsider learns nothing from it, and a forged one is
/// refused.
#[test]
 fn severe_a_search_cursor_does_not_reveal_private_positions() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    {
        let mut config = env.store("config");
        buzz::relay::store::put_raw(&mut config, b"buzz-stream-token-key", &[0x3c; 32])
            .await
            .expect("seed the sealed key");
    }
    let ana = Actor::new("search-ana");
    let outsider = Actor::new("search-outsider");
    seed_channel_with_owner(&env, "vault", Visibility::Private, &ana).await;
    let mut secrets = Vec::new();
    for text in ["zebra one", "zebra two", "zebra three"] {
        secrets.push(say(&env, &ana, "vault", text).await);
    }

    let theirs = search(&env, &outsider, json!({ "q": "zebra", "limit": 1 })).await;
    assert_eq!(theirs.status, 200, "{}", error_text(&theirs));
    let body = body_json(&theirs);
    assert_eq!(body["events"].as_array().map(Vec::len), Some(0));
    if let Some(cursor) = body["next_cursor"].as_str() {
        let decoded = buzz::relay::crypto::from_hex(cursor).unwrap_or_default();
        let decoded = String::from_utf8_lossy(&decoded);
        for secret in &secrets {
            let id = buzz::relay::crypto::to_hex(&secret.id);
            assert!(
                !cursor.contains(&id) && !decoded.contains(&id),
                "the cursor carries a private message's id: {cursor}"
            );
        }
        assert!(
            !decoded.contains("sidx/"),
            "the cursor is a readable index key"
        );
    }

    // The member pages through all three with the sealed cursor.
    let mut seen = std::collections::BTreeSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..6 {
        let mut ask = json!({ "q": "zebra", "limit": 1 });
        if let Some(c) = &cursor {
            ask["cursor"] = json!(c);
        }
        let page = search(&env, &ana, ask).await;
        assert_eq!(page.status, 200, "{}", error_text(&page));
        seen.extend(returned_ids(&page));
        cursor = body_json(&page)["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        seen.len(),
        3,
        "paging with the sealed cursor must reach every hit"
    );

    // A raw index-key cursor is not accepted in its place.
    let rows = index_keys(&env, &keys::search_prefix(COMMUNITY, "zebra")).await;
    let forged = buzz::relay::crypto::to_hex(&rows[0]);
    let refused = search(&env, &outsider, json!({ "q": "zebra", "cursor": forged })).await;
    assert_eq!(refused.status, 400, "{}", error_text(&refused));

    });
}

/// **Only the kinds a person searches are indexed.**
///
/// The write amplification is the cost of this feature, so it must not be paid
/// for reactions, profiles or relay projections — and their content must not
/// turn up in results either.
#[test]
 fn severe_a_reaction_contributes_no_search_rows() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("search-ana");
    seed_channel_with_owner(&env, "general", Visibility::Open, &ana).await;

    let target = say(&env, &ana, "general", "ship it").await;

    // A reaction whose content is a word. It is stored, but never indexed.
    let reaction = signed(
        &ana,
        kind::KIND_REACTION,
        vec![tag("e", &buzz::relay::crypto::to_hex(&target.id))],
        "applause",
    );
    let accepted = submit(&env, &ana, &reaction).await;
    assert_eq!(accepted.status, 200, "{}", error_text(&accepted));
    assert!(
        stored_event(&env, &reaction.id).await.is_some(),
        "the reaction is stored"
    );

    assert!(
        index_keys(&env, &keys::search_prefix(COMMUNITY, "applause"))
            .await
            .is_empty(),
        "a reaction's content must not enter the term index"
    );
    let hits = search(&env, &ana, json!({ "q": "applause" })).await;
    assert!(returned_ids(&hits).is_empty(), "and must not be searchable");

    // The message it reacted to still is.
    assert!(returned(
        &search(&env, &ana, json!({ "q": "ship" })).await,
        &target
    ));

    });
}

/// **A search can be scoped to one channel**, and the scope is applied after the
/// access gate rather than instead of it.
#[test]
 fn severe_a_search_can_be_scoped_to_one_channel() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("search-ana");
    seed_channel_with_owner(&env, "general", Visibility::Open, &ana).await;
    seed_channel_with_owner(&env, "random", Visibility::Open, &ana).await;

    let here = say(&env, &ana, "general", "the roadmap review is friday").await;
    let elsewhere = say(&env, &ana, "random", "the roadmap is a lie").await;

    let scoped = search(&env, &ana, json!({ "q": "roadmap", "channel": "general" })).await;
    assert!(returned(&scoped, &here));
    assert!(
        !returned(&scoped, &elsewhere),
        "a scoped search must not reach another room"
    );

    // Unscoped, both are found.
    let all = search(&env, &ana, json!({ "q": "roadmap" })).await;
    assert!(returned(&all, &here) && returned(&all, &elsewhere));

    });
}

/// **The search operators narrow the answer — honoured, never eaten.**
///
/// Upstream's client decomposes `from:` / `after:` / `before:` into
/// authors/since/until and sends them with every search (v0.5.x, PR #2871).
/// A relay that drops one returns messages the caller explicitly excluded,
/// and the answer LOOKS right — so the gate proves each bound bites, that the
/// time bounds are inclusive exactly as NIP-01 defines them, and that a
/// malformed field is refused by name rather than read as "no bound".
#[test]
 fn severe_search_operators_narrow_the_answer() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("search-ana");
    let bo = Actor::new("search-bo");
    seed_channel_with_owner(&env, "general", Visibility::Open, &ana).await;

    let hers = say(&env, &ana, "general", "the migration plan is ready").await;
    let his = say(&env, &bo, "general", "the migration looked risky to me").await;

    // Unscoped, both match the term — the control that proves the narrowing
    // below is the operator's doing.
    let all = search(&env, &ana, json!({ "q": "migration" })).await;
    assert!(returned(&all, &hers) && returned(&all, &his));

    // `from:` — only the named author's messages.
    let from_ana = search(
        &env,
        &ana,
        json!({ "q": "migration", "authors": [buzz::relay::crypto::to_hex(&ana.pk)] }),
    )
    .await;
    assert!(returned(&from_ana, &hers), "the named author's message");
    assert!(
        !returned(&from_ana, &his),
        "another author's message must not survive a from: filter"
    );

    // `after:`/`before:` are INCLUSIVE bounds (NIP-01 since/until), proven
    // against the event's own timestamp so no clock assumption is needed.
    let at = hers.created_at;
    assert!(
        returned(
            &search(&env, &ana, json!({ "q": "migration", "since": at })).await,
            &hers
        ),
        "since is inclusive of its own instant"
    );
    let too_new = search(&env, &ana, json!({ "q": "migration", "since": at + 1 })).await;
    assert!(returned_ids(&too_new).is_empty(), "nothing is that new");
    assert_eq!(
        body_json(&too_new).get("complete").and_then(Value::as_bool),
        Some(true),
        "a since-bound that ends the walk IS completeness within the range — \
         an empty answer a caller cannot classify would be unreadable"
    );
    assert!(
        returned(
            &search(&env, &ana, json!({ "q": "migration", "until": at })).await,
            &hers
        ),
        "until is inclusive of its own instant"
    );
    assert!(
        returned_ids(&search(&env, &ana, json!({ "q": "migration", "until": at - 1 })).await)
            .is_empty(),
        "nothing is that old"
    );

    // A present-but-malformed operator field is refused by NAME — never
    // coerced into an unbounded search that looks right. Naming matters: an
    // anonymous 400 leaves the caller diffing their own request against the
    // docs to find which field was wrong.
    for (bad, field) in [
        (
            json!({ "q": "migration", "authors": "not-an-array" }),
            "authors",
        ),
        (json!({ "q": "migration", "authors": ["zz"] }), "author"),
        (json!({ "q": "migration", "since": -5 }), "since"),
        (json!({ "q": "migration", "until": "yesterday" }), "until"),
    ] {
        let refused = search(&env, &ana, bad.clone()).await;
        assert_eq!(
            refused.status,
            400,
            "{bad} was not refused: {}",
            error_text(&refused)
        );
        let text = error_text(&refused);
        assert!(
            text.contains(field),
            "the refusal must name the offending field {field:?}: {bad} -> {text}"
        );
    }

    });
}

/// An edit changes the words people can find without rewriting signed history.
/// Older wording and a different member's forged edit must not replace it.
#[test]
 fn severe_search_follows_the_latest_owned_message_edit() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("search-editor");
    let bo = Actor::new("search-other-editor");
    seed_channel_with_owner(&env, "general", Visibility::Open, &ana).await;
    let original = say(&env, &ana, "general", "originalword release").await;
    let reference = buzz::relay::crypto::to_hex(&original.id);
    let edit = signed_at(
        &ana,
        kind::KIND_STREAM_MESSAGE_EDIT,
        vec![tag("h", "general"), tag("e", &reference)],
        "updatedword release",
        original.created_at + 1,
    );
    assert_eq!(submit(&env, &ana, &edit).await.status, 200);
    let stale = search(&env, &ana, json!({"q":"originalword"})).await;
    assert!(
        returned_ids(&stale).is_empty(),
        "superseded words remain searchable"
    );
    let current = search(&env, &ana, json!({"q":"updatedword release"})).await;
    assert_eq!(current.status, 200, "{}", error_text(&current));
    assert_eq!(
        returned_ids(&current),
        vec![buzz::relay::crypto::to_hex(&edit.id)]
    );
    assert_eq!(
        body_json(&current)["events"][0]["content"],
        "updatedword release"
    );
    let forged = signed_at(
        &bo,
        kind::KIND_STREAM_MESSAGE_EDIT,
        vec![tag("h", "general"), tag("e", &reference)],
        "forgedword release",
        original.created_at + 2,
    );
    // A future admission policy may refuse the forged edit outright; either
    // outcome must leave the legitimate author's current text searchable.
    let _ = submit(&env, &bo, &forged).await;
    assert!(returned_ids(&search(&env, &ana, json!({"q":"forgedword"})).await).is_empty());
    assert_eq!(
        returned_ids(&search(&env, &ana, json!({"q":"release"})).await),
        vec![buzz::relay::crypto::to_hex(&edit.id)]
    );
    let newer = signed_at(
        &ana,
        kind::KIND_STREAM_MESSAGE_EDIT,
        vec![tag("h", "general"), tag("e", &reference)],
        "finalword release",
        original.created_at + 3,
    );
    assert_eq!(submit(&env, &ana, &newer).await.status, 200);
    assert!(returned_ids(&search(&env, &ana, json!({"q":"updatedword"})).await).is_empty());
    assert_eq!(
        returned_ids(&search(&env, &ana, json!({"q":"finalword"})).await),
        vec![buzz::relay::crypto::to_hex(&newer.id)]
    );

    });
}
