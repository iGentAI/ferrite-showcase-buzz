//! Plan 031 Contract D: process command/result events.
//!
//! A `KIND_PROCESS_RESULT` (47002) asserts "the platform performed this RCS
//! action" — authority that belongs to the operator-registered process cell
//! and to nobody else. These gates hold the two halves of that contract: the
//! REFUSAL (a forged result is refused at ingest and stores nothing, and an
//! unconfigured workspace refuses the kind outright), and the ADMISSION (the
//! registered cell's result lands, and a member's `KIND_PROCESS_COMMAND` is
//! an ordinary room event). The operator-face validator is pinned beside them,
//! because the gate is only as good as the registry shape it compares against.

use super::common;

use buzz::relay::channel::Visibility;
use buzz::relay::crypto::to_hex;
use buzz::relay::query::Page;
use buzz::relay::{kind, operator, store};
use common::{
    Actor, SUBMIT_ATTEMPTS, body_json, gate_env, error_text, membership, post, query,
    seed_channel_with_owner, signed, signed_at, stored_event, submit, tag, T0_SECS,
};
use ferrite_app::serde_json::{self, json};
use common::SsoTestEnv as TestEnv;

const PROCESS_CELLS_CONFIG: &[u8] = b"buzz-process-cells";

/// Register `cell` as THE process cell, the way the operator plane would
/// (canonical lowercase JSON array — the exact bytes its validator stores).
async fn register_cell(env: &TestEnv, cell: &Actor) {
    let mut config = env.store("config");
    let doc = format!("[\"{}\"]", cell.pk_hex());
    store::put_raw(&mut config, PROCESS_CELLS_CONFIG, doc.as_bytes())
        .await
        .expect("seed buzz-process-cells");
}

/// **A forged process result is refused and stores nothing** — in both the
/// unconfigured workspace (fail closed: even the would-be cell is refused
/// before registration) and the configured one (an ordinary member's forgery
/// is refused by name).
#[test]
 fn severe_a_forged_process_result_is_refused_and_stores_nothing() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let cell = Actor::new("proc-cell");
    let ana = Actor::new("proc-ana");
    seed_channel_with_owner(&env, "process-x", Visibility::Open, &cell).await;

    // No cell registered: the kind is refused outright, whoever signs it.
    let unregistered = signed(
        &cell,
        kind::KIND_PROCESS_RESULT,
        vec![tag("h", "process-x")],
        "{}",
    );
    let refused = submit(&env, &cell, &unregistered).await;
    assert_eq!(refused.status, 403, "{}", error_text(&refused));
    assert!(
        error_text(&refused).contains("buzz-process-cells"),
        "the refusal should name the missing registry: {}",
        error_text(&refused)
    );
    assert!(
        stored_event(&env, &unregistered.id).await.is_none(),
        "a refused result must not be stored"
    );

    // With the cell registered, an ordinary member's forgery is still refused.
    // The forger is added to the room FIRST, so what refuses the event is the
    // authorship gate and not a room-membership check — the test has to isolate
    // the rule it claims to prove.
    register_cell(&env, &cell).await;
    let put = signed(
        &cell,
        kind::KIND_NIP29_PUT_USER,
        vec![tag("h", "process-x"), tag("p", &ana.pk_hex())],
        "",
    );
    assert_eq!(submit(&env, &cell, &put).await.status, 200);
    // Proven a member: an ordinary room event from this actor is accepted.
    let ordinary = signed(
        &ana,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", "process-x")],
        "I am in this room",
    );
    let accepted = submit(&env, &ana, &ordinary).await;
    assert_eq!(
        accepted.status,
        200,
        "the forger must be a room member, or the next assertion proves nothing: {}",
        error_text(&accepted)
    );

    let forged = signed(
        &ana,
        kind::KIND_PROCESS_RESULT,
        vec![tag("h", "process-x")],
        "{}",
    );
    let refused = submit(&env, &ana, &forged).await;
    assert_eq!(refused.status, 403, "{}", error_text(&refused));
    assert!(
        error_text(&refused).contains("not a registered process cell"),
        "the refusal should say WHY: {}",
        error_text(&refused)
    );
    assert!(
        stored_event(&env, &forged.id).await.is_none(),
        "a forged result must not be stored, or the silent path stays reachable"
    );

    });
}

/// **The registered cell's result is admitted, and a member's command flows**
/// — the positive half, so the gate proves the feature works rather than only
/// that forgeries fail.
#[test]
 fn severe_a_registered_cell_result_is_admitted_and_a_member_command_flows() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let cell = Actor::new("proc-cell");
    let ana = Actor::new("proc-ana");
    seed_channel_with_owner(&env, "process-y", Visibility::Open, &cell).await;
    register_cell(&env, &cell).await;

    // The cell (room owner here) adds the member, as the process cell does
    // when it materializes a room.
    let put = signed(
        &cell,
        kind::KIND_NIP29_PUT_USER,
        vec![tag("h", "process-y"), tag("p", &ana.pk_hex())],
        "",
    );
    assert_eq!(submit(&env, &cell, &put).await.status, 200);

    // A member's COMMAND is an ordinary room event — no authorship gate.
    let command = signed(
        &ana,
        kind::KIND_PROCESS_COMMAND,
        vec![tag("h", "process-y")],
        r#"{"v":1,"verb":"refresh"}"#,
    );
    let accepted = submit(&env, &ana, &command).await;
    assert_eq!(accepted.status, 200, "{}", error_text(&accepted));
    assert!(stored_event(&env, &command.id).await.is_some());

    // The cell's RESULT names the command it answers and is admitted.
    let result = signed(
        &cell,
        kind::KIND_PROCESS_RESULT,
        vec![tag("h", "process-y"), tag("e", &to_hex(&command.id))],
        r#"{"v":1,"outcome":"ok"}"#,
    );
    let accepted = submit(&env, &cell, &result).await;
    assert_eq!(accepted.status, 200, "{}", error_text(&accepted));
    assert!(stored_event(&env, &result.id).await.is_some());

    });
}

/// The ids a `POST /buzz/api/filter-query` answered `actor`, re-posting if the
/// reply is lost (the frozen-clock discipline `common::query` documents).
async fn filter_ids(env: &TestEnv, actor: &Actor, filters: &serde_json::Value) -> Vec<String> {
    let body = serde_json::to_vec(filters).expect("filters encode");
    for attempt in 1..=SUBMIT_ATTEMPTS {
        match env.handle(post(actor, "/buzz/api/filter-query", body.clone())).await {
            Ok(reply) => {
                assert_eq!(reply.status, 200, "{}", error_text(&reply));
                return body_json(&reply)
                    .as_array()
                    .expect("an array of events")
                    .iter()
                    .filter_map(|event| event.get("id").and_then(serde_json::Value::as_str))
                    .map(str::to_string)
                    .collect();
            }
            Err(e) if attempt == SUBMIT_ATTEMPTS => panic!("no reply to a filter query: {e}"),
            Err(_) => continue,
        }
    }
    unreachable!("the loop either returns or panics")
}

/// **A process binding survives room volume as a point read, not as a tag
/// query.** `buzz-process-app` decides a room's lifecycle from the binding
/// event it recorded, and asks this relay for that event by id. The tag query
/// it used before (`#h` plus the binding's tags and the cell as author, limit
/// 8) is answered from the room's timeline index, at most eight rounds of
/// twice the limit, so 130 ordinary messages after an archived binding hid it
/// and the archived room read as never bound. This pins both halves on the
/// real read path: the tag query loses the binding under that volume (if it
/// ever stops losing it, the window changed and this note should too), and
/// the `ids` read the cell now makes still finds it.
#[test]
fn severe_a_binding_point_read_survives_room_volume() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let cell = Actor::new("proc-cell");
    // Three members, because the relay admits sixty messages a minute from
    // one author and the gate's clock is frozen.
    let members = [Actor::new("proc-ana"), Actor::new("proc-bo"), Actor::new("proc-cy")];
    let room = "process-volume";
    seed_channel_with_owner(&env, room, Visibility::Open, &cell).await;
    for member in &members {
        let put = signed(
            &cell,
            kind::KIND_NIP29_PUT_USER,
            vec![tag("h", room), tag("p", &member.pk_hex())],
            "",
        );
        assert_eq!(submit(&env, &cell, &put).await.status, 200);
    }

    let binding_id = format!("sha256:{}", "ab".repeat(32));
    let schema = "buzz.process.binding.v1";
    let binding = signed_at(
        &cell,
        kind::KIND_STREAM_MESSAGE,
        vec![
            tag("process-binding", &binding_id),
            tag("process-binding-schema", schema),
            tag("h", room),
        ],
        r#"{"lifecycle":"archived"}"#,
        T0_SECS - 300,
    );
    let accepted = submit(&env, &cell, &binding).await;
    assert_eq!(accepted.status, 200, "{}", error_text(&accepted));
    for (n, member) in (0..130u64).zip(members.iter().cycle()) {
        let chat = signed_at(member, kind::KIND_STREAM_MESSAGE, vec![tag("h", room)], &format!("message {n}"), T0_SECS - 200 + n);
        let accepted = submit(&env, member, &chat).await;
        assert_eq!(accepted.status, 200, "{}", error_text(&accepted));
    }

    let binding_hex = to_hex(&binding.id);
    let by_tags = json!([{
        "kinds": [kind::KIND_STREAM_MESSAGE],
        "authors": [cell.pk_hex()],
        "#h": [room],
        "#process-binding": [binding_id],
        "#process-binding-schema": [schema],
        "limit": 8
    }]);
    assert!(
        !filter_ids(&env, &cell, &by_tags).await.contains(&binding_hex),
        "the tag query found the binding under 130 messages: the relay's window changed"
    );
    let by_id = json!([{
        "ids": [binding_hex],
        "kinds": [kind::KIND_STREAM_MESSAGE],
        "authors": [cell.pk_hex()],
        "#h": [room],
        "limit": 1
    }]);
    assert_eq!(
        filter_ids(&env, &cell, &by_id).await,
        vec![binding_hex],
        "the point read lost the binding under room volume"
    );
    });
}

/// **A binding published before the cell kept a record is found by walking
/// the room's timeline.** A room with no `binding-order` record is read, and
/// the cell's first reservation there ordered, from the newest binding the
/// cell signed on the room's timeline, which `buzz-process-app` walks page by
/// page with the relay's cursor (`{channel, limit: 100, cursor}`). A binding
/// is an ordinary kind-9 room message, not a process command or result, so
/// this pins what that walk depends on: the timeline serves room messages of
/// every author, the cursor reaches a binding buried under 130 later
/// messages on a later page, and the event comes back with an id and
/// signature that still verify under the cell's key, which the cell checks
/// before it trusts the author.
#[test]
fn severe_a_binding_published_before_the_record_is_on_the_timeline_walk() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let cell = Actor::new("proc-cell");
    // Three members, because the relay admits sixty messages a minute from
    // one author and the gate's clock is frozen.
    let members = [Actor::new("proc-ana"), Actor::new("proc-bo"), Actor::new("proc-cy")];
    let room = "process-legacy";
    seed_channel_with_owner(&env, room, Visibility::Open, &cell).await;
    for member in &members {
        let put = signed(
            &cell,
            kind::KIND_NIP29_PUT_USER,
            vec![tag("h", room), tag("p", &member.pk_hex())],
            "",
        );
        assert_eq!(submit(&env, &cell, &put).await.status, 200);
    }
    let binding = signed_at(
        &cell,
        kind::KIND_STREAM_MESSAGE,
        vec![
            tag("process-binding", &format!("sha256:{}", "cd".repeat(32))),
            tag("process-binding-schema", "buzz.process.binding.v1"),
            tag("h", room),
        ],
        r#"{"lifecycle":"active"}"#,
        T0_SECS - 300,
    );
    let accepted = submit(&env, &cell, &binding).await;
    assert_eq!(accepted.status, 200, "{}", error_text(&accepted));
    for (n, member) in (0..130u64).zip(members.iter().cycle()) {
        let chat = signed_at(member, kind::KIND_STREAM_MESSAGE, vec![tag("h", room)], &format!("message {n}"), T0_SECS - 200 + n);
        let accepted = submit(&env, member, &chat).await;
        assert_eq!(accepted.status, 200, "{}", error_text(&accepted));
    }

    let binding_hex = to_hex(&binding.id);
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    let mut found = Vec::new();
    loop {
        let mut ask = json!({"channel": room, "limit": 100});
        if let Some(c) = &cursor {
            ask["cursor"] = json!(c);
        }
        let reply = query(&env, &cell, &ask).await;
        assert_eq!(reply.status, 200, "{}", error_text(&reply));
        let page: Page = serde_json::from_slice(&reply.body).expect("a timeline page");
        assert_eq!(page.complete, page.next_cursor.is_none(), "inconsistent timeline completion");
        pages += 1;
        found.extend(page.events.into_iter().filter(|event| to_hex(&event.id) == binding_hex));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert!(pages > 1, "the room fits one page, so the cursor was never exercised");
    assert_eq!(found.len(), 1, "the timeline walk did not return the binding exactly once");
    let event = &found[0];
    assert_eq!(event.kind, kind::KIND_STREAM_MESSAGE);
    assert_eq!(to_hex(&event.pubkey), cell.pk_hex(), "the binding came back under another author");
    event.verify().expect("the binding's id and signature verify as the relay served them");
    });
}

/// **A malformed cell registry fails CLOSED**: refusing is the only answer
/// that keeps "the document broke" distinguishable from "there are no cells",
/// and the tolerant-reader mistake here would admit nothing but also refuse
/// nothing loudly. Note the operator plane cannot WRITE a malformed document
/// (its validator refuses); this seeds the store directly to model corruption.
///
/// The MIXED case is the sharp one: a document holding this very cell's valid
/// pubkey next to junk must refuse the kind, not authorize the valid entry —
/// otherwise a partially corrupt authority list still grants authority.
#[test]
 fn severe_a_malformed_cell_registry_fails_closed() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let cell = Actor::new("proc-cell");
    seed_channel_with_owner(&env, "process-z", Visibility::Open, &cell).await;

    for (label, doc) in [
        ("not JSON at all", b"not json".to_vec()),
        ("a JSON scalar", b"\"aa\"".to_vec()),
        (
            "this cell's own key beside junk",
            format!("[\"{}\",\"junk\"]", cell.pk_hex()).into_bytes(),
        ),
        (
            "this cell's own key beside a non-string",
            format!("[\"{}\",7]", cell.pk_hex()).into_bytes(),
        ),
    ] {
        {
            let mut config = env.store("config");
            store::put_raw(&mut config, PROCESS_CELLS_CONFIG, &doc)
                .await
                .expect("seed corrupt registry");
        }
        let result = signed(
            &cell,
            kind::KIND_PROCESS_RESULT,
            vec![tag("h", "process-z")],
            "{}",
        );
        let refused = submit(&env, &cell, &result).await;
        assert_eq!(
            refused.status,
            403,
            "{label} must refuse the kind: {}",
            error_text(&refused)
        );
        assert!(
            error_text(&refused).contains("buzz-process-cells"),
            "{label}: the refusal should name the registry: {}",
            error_text(&refused)
        );
        assert!(
            stored_event(&env, &result.id).await.is_none(),
            "{label}: a refused result must not be stored"
        );
    }

    });
}

/// The operator face validates the registry to the moderator-set shape:
/// JSON array of 64-hex pubkeys, deduped, lowercased — the exact bytes the
/// ingest gate compares against, produced by the SAME reader the gate uses.
#[test]
fn the_operator_face_validates_the_cell_registry() {
    let entry = operator::registry_entry("buzz-process-cells").expect("registered key");

    let good = json!(["aa".repeat(32)]);
    assert!(entry.validate(&good).is_ok());

    for bad in [
        json!("not-an-array"),
        json!(["nothex"]),
        json!([1, 2]),
        json!({"cell": true}),
        // The mixed case the ingest gate must also refuse.
        json!(["aa".repeat(32), "junk"]),
    ] {
        assert!(entry.validate(&bad).is_err(), "{bad} must be refused");
    }

    // Dedup + lowercase canonicalization, exactly like the moderator set.
    let dup = json!(["AA".repeat(32), "aa".repeat(32)]);
    let bytes = entry.validate(&dup).expect("canonicalized");
    let parsed: Vec<String> = serde_json::from_slice(&bytes).expect("canonical JSON");
    assert_eq!(parsed, vec!["aa".repeat(32)]);
}

/// The shared strict reader the ingest gate depends on, pinned directly: any
/// fault is an error rather than a partial authority set, and the cap holds
/// against a document seeded outside the operator route.
#[test]
fn the_shared_process_cell_parser_is_strict() {
    // An empty registry is valid and means "no cells" — which the gate turns
    // into a refusal, rather than this parser inventing one.
    assert_eq!(
        operator::parse_process_cells(b"[]")
            .expect("empty is valid")
            .len(),
        0
    );
    let one = operator::parse_process_cells(format!("[\"{}\"]", "aa".repeat(32)).as_bytes())
        .expect("one cell");
    assert_eq!(one, vec![[0xaa; 32]]);
    // Uppercase canonicalizes onto the same key, and duplicates collapse.
    let dup = operator::parse_process_cells(
        format!("[\"{}\",\"{}\"]", "AA".repeat(32), "aa".repeat(32)).as_bytes(),
    )
    .expect("dedup");
    assert_eq!(dup.len(), 1);

    for bad in [
        "not json".to_string(),
        "{}".to_string(),
        "\"aa\"".to_string(),
        "[\"nothex\"]".to_string(),
        "[7]".to_string(),
        "[null]".to_string(),
        // A valid key beside junk must NOT yield a one-element set.
        format!("[\"{}\",\"junk\"]", "aa".repeat(32)),
        format!("[\"{}\",7]", "aa".repeat(32)),
    ] {
        assert!(
            operator::parse_process_cells(bad.as_bytes()).is_err(),
            "{bad} must be refused"
        );
    }

    // The cap is enforced on the INPUT, before dedup, so a padded document
    // cannot slip past by repeating one key.
    let over: Vec<String> = (0..=operator::MAX_PROCESS_CELLS)
        .map(|i| format!("{:02x}", i).repeat(32))
        .collect();
    let doc = serde_json::to_vec(&over).expect("encode");
    assert!(
        operator::parse_process_cells(&doc).is_err(),
        "the cap must hold"
    );
}

/// **A process room's id is reserved for registered process cells.** The id
/// is derived from a public repository and ref, so a member who created it
/// first would own the room the cell adopts on its 409, or lock the cell out.
#[test]
fn severe_a_member_cannot_take_a_process_room_id_first() {
    common::run_scenario(|| async move {
        let env = gate_env().await;
        let cell = Actor::new("proc-cell-room");
        let squatter = Actor::new("proc-squatter");
        register_cell(&env, &cell).await;
        let body = |id: &str| {
            serde_json::to_vec(&json!({"id": id, "name": "predicted", "type": "stream", "visibility": "open"})).unwrap()
        };

        let taken = env
            .handle(post(&squatter, "/buzz/api/channels", body("process-predicted")))
            .await
            .expect("reply");
        assert_eq!(taken.status, 403, "a member created a process room: {}", error_text(&taken));
        let ordinary = env
            .handle(post(&squatter, "/buzz/api/channels", body("chan-ordinary")))
            .await
            .expect("reply");
        assert_eq!(ordinary.status, 201, "{}", error_text(&ordinary));

        let created = env
            .handle(post(&cell, "/buzz/api/channels", body("process-predicted")))
            .await
            .expect("reply");
        assert_eq!(created.status, 201, "the registered cell could not create its room: {}", error_text(&created));
    });
}
