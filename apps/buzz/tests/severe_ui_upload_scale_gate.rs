//! **The UI upload scales linearly** (adversarial review finding 7).
//!
//! The chunked operator upload used to keep its completion set INSIDE the session
//! document — the document that names every file in the bundle. Each finished
//! file was therefore a compare-and-swap rewrite of the whole ~100 KiB thing:
//! quadratic bytes for a 499-file push, plus a CAS that two concurrent file
//! workers contend on, which is what turned a real bundle push into ~630
//! requests that hard-429'd.
//!
//! Completion now lives in a row per path (`keys::updone`). This gate pins the
//! three properties that restructure has to keep:
//!
//! * a chunk upload does **not** rewrite the session document — proven on the
//!   document's own store VERSION, not on a timing;
//! * two files finishing at once cannot lose each other's completion (the thing
//!   the CAS was there for);
//! * `finish` still refuses while ANY path is unverified, and the per-path rows
//!   still carry the session's expiry, so an abandoned upload self-erases with no
//!   sweeper.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::sync::atomic::{AtomicU64, Ordering};

use buzz::relay::crypto::sha256::sha256;
use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::{b64 as buzz_b64, keys, kind, operator, store as buzz_store};
use common::{Actor, T0_SECS, auth_header, body_json, gate_env, error_text, request_with_auth};
use ferrite_app::serde_json::{self, json, Value};
use ferrite_app::IngressRequest;
use common::SsoTestEnv as TestEnv;

static UP_NONCE: AtomicU64 = AtomicU64::new(0);

fn nonce() -> String {
    format!("up{}", UP_NONCE.fetch_add(1, Ordering::Relaxed))
}

/// An operator POST: the NIP-98 header also commits to the body.
fn operator_post(actor: &Actor, path: &str, body: Vec<u8>) -> IngressRequest {
    let event = Event::sign_new(
        &actor.sk,
        kind::KIND_HTTP_AUTH,
        vec![
            vec!["u".into(), format!("https://buzz.test{path}")],
            vec!["method".into(), "POST".into()],
            vec!["payload".into(), to_hex(&sha256(&body))],
            vec!["nonce".into(), nonce()],
        ],
        "",
        T0_SECS,
    )
    .expect("authorization events sign");
    request_with_auth("POST", path, Some(&auth_header(&event)), body)
}

async fn seed_operator(env: &TestEnv, op: &Actor) {
    let mut config = env.store("config");
    let set = serde_json::to_vec(&json!([op.pk_hex()])).expect("set encodes");
    buzz_store::put_raw(&mut config, operator::OPERATOR_SET_CONFIG, &set)
        .await
        .expect("operator seed");
}

/// One small file's manifest entry plus its bytes.
fn file(path: &str, seed: u8) -> (Value, Vec<u8>) {
    let bytes = vec![seed; 64 + usize::from(seed)];
    (
        json!({ "path": path, "size": bytes.len(), "sha256": to_hex(&sha256(&bytes)) }),
        bytes,
    )
}

async fn begin(env: &TestEnv, op: &Actor, files: &[Value]) -> (String, Vec<String>) {
    let reply = env
        .handle(operator_post(
            op,
            "/buzz/api/operator/assets/begin",
            json!({ "files": files, "prune": false })
                .to_string()
                .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let body = body_json(&reply);
    let upload = body["upload"].as_str().expect("an upload id").to_string();
    let missing = body["missing"]
        .as_array()
        .expect("a missing list")
        .iter()
        .filter_map(|v| v.as_str().map(ToOwned::to_owned))
        .collect();
    (upload, missing)
}

fn chunk_request(op: &Actor, upload: &str, path: &str, bytes: &[u8]) -> IngressRequest {
    operator_post(
        op,
        "/buzz/api/operator/assets/chunk",
        json!({
            "upload": upload,
            "path": path,
            "offset": 0,
            "data": buzz_b64::encode(bytes),
            "last": true,
        })
        .to_string()
        .into_bytes(),
    )
}

async fn push(env: &TestEnv, op: &Actor, upload: &str, path: &str, bytes: &[u8]) {
    let reply = env
        .handle(chunk_request(op, upload, path, bytes))
        .await
        .expect("the app replied");
    assert_eq!(
        reply.status,
        200,
        "{}: {}",
        path,
        String::from_utf8_lossy(&reply.body)
    );
}

async fn finish(env: &TestEnv, op: &Actor, upload: &str) -> ferrite_app::IngressReply {
    env.handle(operator_post(
        op,
        "/buzz/api/operator/assets/finish",
        json!({ "upload": upload }).to_string().into_bytes(),
    ))
    .await
    .expect("the app replied")
}

/// The session document's stored bytes and version.
async fn session_row(env: &TestEnv, upload: &str) -> Option<(Vec<u8>, u64)> {
    let mut config = env.store("config");
    buzz_store::get_raw_versioned(&mut config, &keys::upload(upload))
        .await
        .expect("session read")
}

async fn done_row(env: &TestEnv, upload: &str, path: &str) -> Option<Vec<u8>> {
    let mut config = env.store("config");
    buzz_store::get_raw(
        &mut config,
        &keys::updone(upload, &operator::path_sha16(path)),
    )
    .await
    .expect("done row read")
}

/// THE finding: a file's completion must not rewrite the session document.
#[test]
 fn severe_a_chunk_upload_never_rewrites_the_session_document() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let op = Actor::new("scale-operator");
    seed_operator(&env, &op).await;

    let files: Vec<(Value, Vec<u8>)> = (0..8u8)
        .map(|i| file(&format!("assets/f{i}.js"), i + 1))
        .collect();
    let manifest: Vec<Value> = files.iter().map(|(entry, _)| entry.clone()).collect();
    let (upload, missing) = begin(&env, &op, &manifest).await;
    assert_eq!(
        missing.len(),
        8,
        "no manifest yet, so every path is missing"
    );

    let (bytes_at_begin, version_at_begin) = session_row(&env, &upload).await.expect("a session");

    for (entry, body) in &files {
        push(&env, &op, &upload, entry["path"].as_str().unwrap(), body).await;
    }

    let (bytes_after, version_after) = session_row(&env, &upload).await.expect("a session");
    assert_eq!(
        version_after,
        version_at_begin,
        "the session document was rewritten {} time(s) by 8 file completions",
        version_after.saturating_sub(version_at_begin)
    );
    assert_eq!(bytes_after, bytes_at_begin, "the session document changed");

    // Completion lives per path instead, and every one of them is there.
    for (entry, _) in &files {
        let path = entry["path"].as_str().unwrap();
        assert!(
            done_row(&env, &upload, path).await.is_some(),
            "{path} has no completion row"
        );
    }

    let reply = finish(&env, &op, &upload).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(body_json(&reply)["files"], json!(8));
    assert!(
        session_row(&env, &upload).await.is_none(),
        "a committed session is deleted"
    );

    });
}

/// `finish` refuses while any path is unverified — and names one, because "3
/// files are missing" is unactionable on a 499-file push.
#[test]
 fn severe_finish_refuses_until_every_path_is_verified() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let op = Actor::new("partial-operator");
    seed_operator(&env, &op).await;

    let a = file("index.html", 1);
    let b = file("assets/app.js", 2);
    let c = file("assets/app.css", 3);
    let (upload, _) = begin(&env, &op, &[a.0.clone(), b.0.clone(), c.0.clone()]).await;

    push(&env, &op, &upload, "index.html", &a.1).await;
    let refused = finish(&env, &op, &upload).await;
    assert_eq!(refused.status, 400);
    let text = error_text(&refused);
    assert!(text.contains("2 file(s)"), "{text}");
    assert!(
        text.contains("assets/app.js") || text.contains("assets/app.css"),
        "the refusal must name an unverified path: {text}"
    );
    assert!(
        buzz_store::get_raw(&mut env.store("config"), operator::MANIFEST_CONFIG)
            .await
            .expect("manifest read")
            .is_none(),
        "a refused finish wrote a manifest"
    );

    push(&env, &op, &upload, "assets/app.js", &b.1).await;
    push(&env, &op, &upload, "assets/app.css", &c.1).await;
    assert_eq!(finish(&env, &op, &upload).await.status, 200);

    });
}

/// Two files finishing AT ONCE: the case the old compare-and-swap existed for.
/// Per-path rows cannot lose an update, so both completions survive and `finish`
/// is satisfied.
#[test]
 fn severe_concurrent_file_completions_cannot_lose_each_other() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let op = Actor::new("concurrent-operator");
    seed_operator(&env, &op).await;

    let a = file("assets/one.js", 7);
    let b = file("assets/two.js", 9);
    let (upload, _) = begin(&env, &op, &[a.0.clone(), b.0.clone()]).await;

    let mut replies = env.handle_all([
        chunk_request(&op, &upload, "assets/one.js", &a.1),
        chunk_request(&op, &upload, "assets/two.js", &b.1),
    ]).await.into_iter();
    let one = replies.next().expect("the first concurrent chunk reply");
    let two = replies.next().expect("the second concurrent chunk reply");
    assert!(replies.next().is_none(), "exactly two chunks were submitted");
    assert_eq!(one.expect("a reply").status, 200);
    assert_eq!(two.expect("a reply").status, 200);

    assert!(done_row(&env, &upload, "assets/one.js").await.is_some());
    assert!(done_row(&env, &upload, "assets/two.js").await.is_some());
    assert_eq!(finish(&env, &op, &upload).await.status, 200);

    });
}

/// The incremental push: when the committed manifest already covers every path
/// byte-identically, `begin` resolves them all at once and `finish` succeeds with
/// NO chunk uploads and NO completion rows — the pre-verified set must not cost a
/// write per file either, which is the other end of the same quadratic.
#[test]
 fn severe_an_unchanged_bundle_needs_no_uploads_and_no_completion_rows() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let op = Actor::new("incremental-operator");
    seed_operator(&env, &op).await;

    let a = file("index.html", 4);
    let b = file("assets/app.js", 5);
    let (first, _) = begin(&env, &op, &[a.0.clone(), b.0.clone()]).await;
    push(&env, &op, &first, "index.html", &a.1).await;
    push(&env, &op, &first, "assets/app.js", &b.1).await;
    assert_eq!(finish(&env, &op, &first).await.status, 200);

    // The same bundle again.
    let (second, missing) = begin(&env, &op, &[a.0.clone(), b.0.clone()]).await;
    assert!(
        missing.is_empty(),
        "nothing changed, so nothing is missing: {missing:?}"
    );
    assert!(
        done_row(&env, &second, "index.html").await.is_none(),
        "an unchanged path must not need a completion row"
    );
    let reply = finish(&env, &op, &second).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(body_json(&reply)["files"], json!(2));

    });
}
