//! Fix gate: the operator plane's HAPPY paths and its first-order refusals.
//!
//! This file proves the implementation works — fail-closed root, config
//! write/read/list, the body commitment, the chunked UI push, and the
//! anti-clobber seeding rule. The adversarial depth (replay, cross-path
//! tokens, escalation from every in-band role, TTL expiry under a moved
//! clock) belongs to `severe_operator_gate.rs` / `severe_ui_push_gate.rs`.
//!
//! The operator root is seeded through the CONFIG-STORE fallback key
//! (`buzz-operators`), because `ferrite-app-test` injects an empty secrets map
//! with no `with_secret` builder — which is precisely why the fallback seam
//! exists. The route family itself refuses to write that key, so the seeding
//! here goes around the HTTP face, exactly as a gate must.

use super::common;

use std::sync::atomic::{AtomicU64, Ordering};

use buzz::relay::crypto::sha256::sha256;
use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::{b64 as buzz_b64, keys, kind, operator, store as buzz_store};
use common::{
    Actor, COMMUNITY, T0_SECS, audit_entries, auth_header, body_json, gate_env, fresh_auth, post,
    request_with_auth, tag,
};
use ferrite_app::serde_json::{self, json};
use ferrite_app::IngressRequest;
use common::SsoTestEnv as TestEnv;

/// Nonce source local to this file (the shared one in `common` is private).
static OP_NONCE: AtomicU64 = AtomicU64::new(0);

fn nonce() -> String {
    format!("op{}", OP_NONCE.fetch_add(1, Ordering::Relaxed))
}

/// A POST whose NIP-98 header ALSO commits to the body via the `payload` tag —
/// what every write in the operator family requires.
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

/// An authenticated GET (reads need no payload tag — they have no body).
fn operator_get(actor: &Actor, path: &str) -> IngressRequest {
    let (_, header) = fresh_auth(actor, "GET", path);
    request_with_auth("GET", path, Some(&header), Vec::new())
}

/// Seed the operator set through the config fallback, from OUTSIDE the face.
async fn seed_operator(env: &TestEnv, operator: &Actor) {
    let mut config = env.store("config");
    let set = serde_json::to_vec(&json!([operator.pk_hex()])).expect("set encodes");
    buzz_store::put_raw(&mut config, operator::OPERATOR_SET_CONFIG, &set)
        .await
        .expect("operator seed");
}

async fn config_value(env: &TestEnv, key: &[u8]) -> Option<Vec<u8>> {
    let mut config = env.store("config");
    buzz_store::get_raw(&mut config, key)
        .await
        .expect("config read")
}

// ------------------------------------------------------------------ fail closed

#[test]
 fn with_no_operator_seeded_every_route_fails_closed() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let anyone = Actor::new("would-be-operator");

    for (request, label) in [
        (operator_get(&anyone, "/buzz/api/operator/config"), "list"),
        (
            operator_get(&anyone, "/buzz/api/operator/config/buzz-relay-url"),
            "read",
        ),
        (
            operator_post(
                &anyone,
                "/buzz/api/operator/config/buzz-relay-url",
                json!({ "value": "wss://x" }).to_string().into_bytes(),
            ),
            "write",
        ),
        (
            operator_post(
                &anyone,
                "/buzz/api/operator/assets/begin",
                json!({ "files": [{ "path": "index.html", "size": 1,
                        "sha256": to_hex(&sha256(b"x")) }] })
                .to_string()
                .into_bytes(),
            ),
            "begin",
        ),
    ] {
        let reply = env.handle(request).await.expect("the app replied");
        // 403, not 401: these callers authenticate perfectly well. With no operator
        // set seeded, nobody is permitted, and telling them to authenticate again
        // would send a standard client round a loop it cannot win.
        assert_eq!(reply.status, 403, "{label} was not refused");
    }
    // The refused write created nothing.
    assert!(config_value(&env, b"buzz-relay-url").await.is_none());
    assert!(config_value(&env, operator::OPERATOR_SET_CONFIG)
        .await
        .is_none());

    });
}

// ------------------------------------------------------------- config lifecycle

#[test]
 fn config_writes_validate_canonicalize_and_cas() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let op = Actor::new("the-operator");
    let member = Actor::new("just-a-member");
    seed_operator(&env, &op).await;

    // A valid write lands the CANONICAL bytes the live consumer reads.
    let reply = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/config/buzz-community-closed",
            json!({ "value": true }).to_string().into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(
        config_value(&env, b"buzz-community-closed")
            .await
            .as_deref(),
        Some(&b"true"[..]),
        "stored byte-identical to what enforce_community_membership compares"
    );

    // Read one key back; list shows presence but NEVER values.
    let read = env
        .handle(operator_get(
            &op,
            "/buzz/api/operator/config/buzz-community-closed",
        ))
        .await
        .expect("the app replied");
    assert_eq!(read.status, 200);
    assert_eq!(body_json(&read)["value"], json!(true));
    let list = env
        .handle(operator_get(&op, "/buzz/api/operator/config"))
        .await
        .expect("the app replied");
    assert_eq!(list.status, 200);
    let listed = body_json(&list);
    let rows = listed["keys"].as_array().expect("rows");
    assert_eq!(rows.len(), operator::WRITABLE_KEYS.len());
    assert!(
        !String::from_utf8_lossy(&list.body).contains("\"value\""),
        "the list is uniformly redacted"
    );

    // A malformed value is refused with the PREVIOUS value intact.
    let bad = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/config/buzz-community-closed",
            json!({ "value": "banana" }).to_string().into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(bad.status, 400);
    assert_eq!(
        config_value(&env, b"buzz-community-closed")
            .await
            .as_deref(),
        Some(&b"true"[..])
    );

    // CAS: a write carrying the live version wins; a stale one is a 409 and
    // the concurrent winner's value survives.
    let write = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/config/buzz-relay-url",
            json!({ "value": "wss://first.example" })
                .to_string()
                .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(write.status, 200);
    let v1 = body_json(&write)["version"].as_u64().expect("a version");
    let cas_ok = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/config/buzz-relay-url",
            json!({ "value": "wss://second.example", "expected_version": v1 })
                .to_string()
                .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(cas_ok.status, 200);
    let stale = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/config/buzz-relay-url",
            json!({ "value": "wss://third.example", "expected_version": v1 })
                .to_string()
                .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(stale.status, 409);
    assert_eq!(
        config_value(&env, b"buzz-relay-url").await.as_deref(),
        Some(&b"wss://second.example"[..])
    );

    // value: null restores the absent default.
    let delete = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/config/buzz-community-closed",
            json!({ "value": null }).to_string().into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(delete.status, 200);
    assert!(config_value(&env, b"buzz-community-closed").await.is_none());

    // The credentials and the root are 404 through this face, untouched rows.
    for credential in [
        "buzz-operators",
        "buzz-relay-identity",
        "buzz-stream-token-key",
        "buzz-invite-key",
        "replay/main/x",
    ] {
        let reply = env
            .handle(operator_post(
                &op,
                &format!("/buzz/api/operator/config/{credential}"),
                json!({ "value": "anything" }).to_string().into_bytes(),
            ))
            .await
            .expect("the app replied");
        assert_eq!(reply.status, 404, "{credential} was served");
    }
    let seeded = config_value(&env, operator::OPERATOR_SET_CONFIG).await;
    assert_eq!(
        seeded,
        Some(serde_json::to_vec(&json!([op.pk_hex()])).expect("encodes")),
        "the root itself is byte-identical after the attempts"
    );

    // The manifest key is readable but refuses writes through this route.
    let manifest_write = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/config/buzz-web-assets-manifest",
            json!({ "value": { "etag": "forged" } })
                .to_string()
                .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(manifest_write.status, 400);

    // Two refusals on the same route, for different reasons, with different codes.
    // The member's request is impeccable — fresh NIP-98, payload tag and all — and is
    // refused for WHO THEY ARE: 403, because authenticating again cannot help. The
    // operator's untagged POST is refused for its CREDENTIAL, which does not commit
    // to the body it arrived with: 401, because a correctly minted header would.
    let member_write = env
        .handle(operator_post(
            &member,
            "/buzz/api/operator/config/buzz-relay-url",
            json!({ "value": "wss://member.example" })
                .to_string()
                .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(member_write.status, 403, "a non-operator is not permitted");
    let untagged = env
        .handle(post(
            &op,
            "/buzz/api/operator/config/buzz-relay-url",
            json!({ "value": "wss://untagged.example" })
                .to_string()
                .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(
        untagged.status, 401,
        "a POST without the payload tag is a credential that does not cover this body"
    );
    assert_eq!(
        config_value(&env, b"buzz-relay-url").await.as_deref(),
        Some(&b"wss://second.example"[..]),
        "neither refused write touched the row"
    );

    // The audit chain recorded the operator's changes.
    let entries = audit_entries(&env).await;
    assert!(
        entries.iter().any(|e| {
            e.action == buzz::relay::audit::AuditAction::ConfigUpdated
                && e.object.as_deref() == Some("buzz-community-closed")
                && e.actor.as_deref() == Some(op.pk_hex().as_str())
        }),
        "config_updated is on the chain"
    );

    // The closing invariant of every gate in this suite.
    let health = env
        .handle(common::get("/buzz/api/healthz"))
        .await
        .expect("the app replied");
    assert_eq!(health.status, 200);

    });
}

// ------------------------------------------------------------------ the UI push

/// Upload one file as chunks. `orders` gives the send order of chunk indexes;
/// the LAST request sent carries `last: true` (a re-send of an already-staged
/// index is the retry the staging absorbs).
async fn push_file(
    env: &TestEnv,
    op: &Actor,
    upload: &str,
    path: &str,
    bytes: &[u8],
    orders: &[usize],
) {
    let chunk = operator::CHUNK_BYTES;
    for (n, &index) in orders.iter().enumerate() {
        let offset = index * chunk;
        let end = (offset + chunk).min(bytes.len());
        let data = bytes.get(offset..end).expect("in range");
        let body = json!({
            "upload": upload,
            "path": path,
            "offset": offset,
            "data": buzz_b64::encode(data),
            "last": n == orders.len() - 1,
        });
        let reply = env
            .handle(operator_post(
                op,
                "/buzz/api/operator/assets/chunk",
                body.to_string().into_bytes(),
            ))
            .await
            .expect("the app replied");
        assert_eq!(
            reply.status,
            200,
            "chunk {index} of {path}: {}",
            String::from_utf8_lossy(&reply.body)
        );
    }
}

#[test]
 fn a_pushed_bundle_serves_survives_reboot_seeding_and_prunes() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let op = Actor::new("ui-operator");
    seed_operator(&env, &op).await;

    // First-boot bootstrap: with no manifest, seeding writes the EMBEDDED bundle.
    // Boot itself no longer runs it (`ingress::SEED_UI_BLOB` is off: on upstream
    // Core the api `/` route answers `/buzz/ui/` from the embedded table and
    // nothing reads the blob copy), so the gate drives the seeding function
    // directly and awaits it, the way it drives the second "boot" further down.
    let mut assets = env.blob("web-assets");
    let embedded_index = buzz::relay::assets::ui::UI_ASSETS
        .iter()
        .find(|(key, _)| *key == "ui/index.html")
        .map(|(_, bytes)| bytes.to_vec())
        .expect("the embedded bundle has an index");

    // What a browser is served, through the app's own route. On a fresh boot,
    // with blob seeding off and nothing pushed, that is the bundle embedded in
    // the build, answered without any copy in the blob.
    let served = |path: &'static str| {
        let env = &env;
        async move {
            env.handle(common::get(path))
                .await
                .expect("the app replied")
        }
    };
    let before = served("/buzz/ui/").await;
    assert_eq!((before.status, &before.body), (200, &embedded_index));
    assert_eq!(
        assets.get(b"ui/index.html").await.expect("blob read"),
        None,
        "boot did not seed the blob, and serving did not need it"
    );

    buzz::relay::ingress::seed_ui_assets(&env.context()).await;
    let seeded = assets.get(b"ui/index.html").await.expect("blob read");
    assert_eq!(
        seeded,
        Some(embedded_index.clone()),
        "the first-boot bootstrap still works"
    );

    // The new bundle: a small index and a file big enough for two chunks.
    let index_bytes = b"<html>pushed bundle</html>".to_vec();
    let mut big = Vec::with_capacity(operator::CHUNK_BYTES + 1_000);
    for i in 0..operator::CHUNK_BYTES + 1_000 {
        big.push(u8::try_from(i % 251).expect("fits"));
    }
    let files = json!([
        { "path": "index.html", "size": index_bytes.len(), "sha256": to_hex(&sha256(&index_bytes)) },
        { "path": "assets/big.js", "size": big.len(), "sha256": to_hex(&sha256(&big)) },
    ]);

    let begin = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/assets/begin",
            json!({ "files": files, "prune": true })
                .to_string()
                .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(
        begin.status,
        200,
        "{}",
        String::from_utf8_lossy(&begin.body)
    );
    let begin_body = body_json(&begin);
    let upload = begin_body["upload"]
        .as_str()
        .expect("an upload id")
        .to_string();
    let missing = begin_body["missing"].as_array().expect("missing");
    assert_eq!(
        missing.len(),
        2,
        "no manifest yet, so everything is missing"
    );

    // finish before anything is uploaded is refused, and writes no manifest.
    let premature = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/assets/finish",
            json!({ "upload": upload }).to_string().into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(premature.status, 400);
    assert!(config_value(&env, operator::MANIFEST_CONFIG)
        .await
        .is_none());

    // Chunks retried and delivered out of order still assemble byte-identically.
    push_file(&env, &op, &upload, "index.html", &index_bytes, &[0]).await;
    push_file(&env, &op, &upload, "assets/big.js", &big, &[1, 0, 1]).await;
    assert_eq!(
        (served("/buzz/ui/").await.status, &served("/buzz/ui/").await.body),
        (200, &embedded_index),
        "staged chunks do not overwrite the served bundle before finish commits the manifest"
    );

    // Integrity: a chunk whose assembled sha mismatches refuses to name the
    // path — a second upload naming different bytes under the same path.
    let begin2 = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/assets/begin",
            json!({ "files": [{ "path": "index.html", "size": index_bytes.len(),
                    "sha256": to_hex(&sha256(b"something else")) }] })
            .to_string()
            .into_bytes(),
        ))
        .await
        .expect("the app replied");
    let upload2 = body_json(&begin2)["upload"]
        .as_str()
        .expect("id")
        .to_string();
    let mismatch = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/assets/chunk",
            json!({ "upload": upload2, "path": "index.html", "offset": 0,
                    "data": buzz_b64::encode(&index_bytes), "last": true })
            .to_string()
            .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(mismatch.status, 400);

    // Commit the real bundle. Prune must delete every embedded key the new
    // manifest no longer names, not index.html. Derive that count from the
    // embedded source of truth so adding a favicon or another hashed asset
    // cannot silently stale this deployment gate.
    let finish = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/assets/finish",
            json!({ "upload": upload }).to_string().into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(
        finish.status,
        200,
        "{}",
        String::from_utf8_lossy(&finish.body)
    );
    let finish_body = body_json(&finish);
    assert_eq!(finish_body["files"], json!(2));
    assert_eq!(
        finish_body["pruned"],
        json!(buzz::relay::assets::ui::UI_ASSETS.len() - 1),
        "every embedded asset other than the replaced index is pruned"
    );

    // The served tree is the pushed bundle, byte for byte: first as a browser
    // gets it from `/buzz/ui/`, which is what "a pushed bundle is authoritative"
    // promises an operator, then as the blob holds it.
    let index = served("/buzz/ui/").await;
    assert_eq!(
        (index.status, &index.body),
        (200, &index_bytes),
        "the origin serves the pushed index, not the embedded one"
    );
    let client_route = served("/buzz/ui/rooms/general").await;
    assert_eq!((client_route.status, &client_route.body), (200, &index_bytes));
    let script = served("/buzz/ui/assets/big.js").await;
    assert_eq!((script.status, &script.body), (200, &big));
    // A file only the replaced embedded bundle had is gone from the origin too:
    // the two bundles are never mixed.
    let embedded_only: &'static str = buzz::relay::assets::ui::UI_ASSETS
        .iter()
        .map(|(key, _)| *key)
        .find(|key| *key != "ui/index.html" && key.rsplit('/').next().is_some_and(|f| f.contains('.')))
        .expect("the embedded bundle has a file besides its index");
    let stale: &'static str = Box::leak(format!("/buzz/{embedded_only}").into_boxed_str());
    assert_eq!(served(stale).await.status, 404, "{stale}");

    assert_eq!(
        assets.get(b"ui/index.html").await.expect("blob read"),
        Some(index_bytes.clone())
    );
    assert_eq!(
        assets.get(b"ui/assets/big.js").await.expect("blob read"),
        Some(big.clone())
    );
    for (path, _) in buzz::relay::assets::ui::UI_ASSETS {
        if *path == "ui/index.html" {
            continue;
        }
        assert_eq!(
            assets
                .get(path.as_bytes())
                .await
                .expect("blob read"),
            None,
            "the pruned embedded asset {path} is gone"
        );
    }

    // The manifest is durable, carries the right etag, and the session is gone.
    let manifest_bytes = config_value(&env, operator::MANIFEST_CONFIG)
        .await
        .expect("a manifest");
    let manifest: operator::Manifest =
        serde_json::from_slice(&manifest_bytes).expect("manifest decodes");
    let expected: Vec<operator::ManifestFile> =
        serde_json::from_value(files).expect("files decode");
    assert_eq!(manifest.etag, operator::manifest_etag(&expected));
    assert!(config_value(&env, &keys::upload(&upload)).await.is_none());

    // THE ANTI-CLOBBER, the load-bearing assertion: run the boot seeding again
    // — what a reboot does — and the uploaded bundle must remain authoritative.
    buzz::relay::ingress::seed_ui_assets(&env.context()).await;
    assert_eq!(
        assets.get(b"ui/index.html").await.expect("blob read"),
        Some(index_bytes.clone()),
        "the embedded bundle was NOT re-seeded over the pushed one"
    );
    for (path, _) in buzz::relay::assets::ui::UI_ASSETS {
        if *path == "ui/index.html" {
            continue;
        }
        assert_eq!(
            assets
                .get(path.as_bytes())
                .await
                .expect("blob read"),
            None,
            "seeding did not resurrect the pruned embedded asset {path}"
        );
    }

    // Legacy unversioned manifest + bundle replacement: remove the versioned
    // `ui-sha256/*` entries of the first pushed bundle so it looks like a bundle
    // pushed before content-addressed keys existed, then replace it with a
    // second bundle (`index_v2` only, pruning `assets/big.js`). Verify that:
    // 1. while the second bundle's chunks are staged before `finish`, `/buzz/ui/`
    //    and `/buzz/ui/assets/big.js` still serve the first bundle from its
    //    digest-verified `ui/` fallback without mixing versions; and
    // 2. `finish` backfills `ui-sha256/{sha256}` for the first bundle's files
    //    before overwriting `ui/index.html` and pruning `ui/assets/big.js`, so
    //    an in-flight reader that already selected the first manifest can still
    //    read both files by their versioned keys while new requests get `index_v2`
    //    and 404 on `/buzz/ui/assets/big.js`.
    let old_index_vkey = format!("ui-sha256/{}", to_hex(&sha256(&index_bytes)));
    let old_big_vkey = format!("ui-sha256/{}", to_hex(&sha256(&big)));
    let _ = assets.delete(old_index_vkey.as_bytes()).await.expect("delete vkey");
    let _ = assets.delete(old_big_vkey.as_bytes()).await.expect("delete vkey");
    assert_eq!(
        (served("/buzz/ui/").await.status, &served("/buzz/ui/").await.body),
        (200, &index_bytes),
        "legacy unversioned manifest still serves matching ui/ bytes"
    );

    let index_v2 = b"<html>pushed bundle v2</html>".to_vec();
    let files_v2 = json!([
        { "path": "index.html", "size": index_v2.len(), "sha256": to_hex(&sha256(&index_v2)) },
    ]);
    let begin_v2 = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/assets/begin",
            json!({ "files": files_v2, "prune": true })
                .to_string()
                .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(begin_v2.status, 200);
    let upload_v2 = body_json(&begin_v2)["upload"]
        .as_str()
        .expect("upload v2 id")
        .to_string();
    push_file(&env, &op, &upload_v2, "index.html", &index_v2, &[0]).await;

    // Before `finish` of v2, the first bundle is still served intact.
    assert_eq!(
        (served("/buzz/ui/").await.status, &served("/buzz/ui/").await.body),
        (200, &index_bytes),
        "staging v2 does not overwrite v1 before finish"
    );
    assert_eq!(
        (
            served("/buzz/ui/assets/big.js").await.status,
            &served("/buzz/ui/assets/big.js").await.body
        ),
        (200, &big),
        "staging v2 does not prune v1 assets before finish"
    );

    // Deterministic interleaved reader test: a reader that already selected the
    // legacy v1 manifest misses the initial `ui-sha256/*` lookup (because v1 is
    // unversioned before finish), then pauses while `finish_v2` runs to
    // completion (backfilling v1's `ui-sha256/*` keys, committing v2,
    // overwriting `ui/index.html` with `index_v2`, and pruning `ui/assets/big.js`).
    // When the paused reader resumes, its unversioned `ui/assets/big.js` lookup
    // misses and its retry against the backfilled `ui-sha256/*` key serves `big`.
    let (interleaved_status, interleaved_body) = buzz::relay::ingress::resolve_ui_manifest_file(
        &env.context(),
        &manifest,
        "assets/big.js",
        false,
        || async {
            let finish_v2 = env
                .handle(operator_post(
                    &op,
                    "/buzz/api/operator/assets/finish",
                    json!({ "upload": upload_v2 }).to_string().into_bytes(),
                ))
                .await
                .expect("the app replied");
            assert_eq!(finish_v2.status, 200);
            assert_eq!(body_json(&finish_v2)["pruned"], json!(1));
        },
    )
    .await;
    assert_eq!(
        (interleaved_status, &interleaved_body),
        (200, &big),
        "in-flight legacy reader paused across finish still resolves pruned v1 asset"
    );

    // After `finish` of v2, new requests serve v2 and 404 on pruned `assets/big.js`,
    // while the backfilled versioned keys for v1 remain available for in-flight reads.
    assert_eq!(
        (served("/buzz/ui/").await.status, &served("/buzz/ui/").await.body),
        (200, &index_v2)
    );
    assert_eq!(served("/buzz/ui/assets/big.js").await.status, 404);
    assert_eq!(
        assets.get(old_index_vkey.as_bytes()).await.expect("blob read"),
        Some(index_bytes),
        "finish backfilled and retained v1 index under its content-addressed key"
    );
    assert_eq!(
        assets.get(old_big_vkey.as_bytes()).await.expect("blob read"),
        Some(big),
        "finish backfilled and retained pruned v1 asset under its content-addressed key"
    );

    // Traversal at begin never stages anything.
    let traversal = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/assets/begin",
            json!({ "files": [{ "path": "../escape.js", "size": 1,
                    "sha256": to_hex(&sha256(b"x")) }] })
            .to_string()
            .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(traversal.status, 400);

    // The audit chain carries the committed etag.
    let entries = audit_entries(&env).await;
    assert!(entries.iter().any(|e| {
        e.action == buzz::relay::audit::AuditAction::UiBundleCommitted
            && e.object.as_deref() == Some(manifest.etag.as_str())
    }));

    });
}

#[test]
 fn secret_valued_keys_never_leak_their_bytes() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let op = Actor::new("token-operator");
    seed_operator(&env, &op).await;

    // The surviving secret-valued flat family: a persona's relay-held signing
    // key (`buzz-keyring-{agent}` → the keyring row its consumer reads).
    let persona = Actor::new("revka");
    let key_hex = to_hex(&persona.sk);
    let write = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/config/buzz-keyring-revka",
            json!({ "value": key_hex }).to_string().into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(
        write.status,
        200,
        "{}",
        String::from_utf8_lossy(&write.body)
    );
    // Stored raw on the row the persona surface reads.
    assert_eq!(
        config_value(&env, &keys::keyring(COMMUNITY, "revka"))
            .await
            .as_deref(),
        Some(key_hex.as_bytes())
    );
    // Neither the read nor the list reply contains the key bytes.
    let read = env
        .handle(operator_get(
            &op,
            "/buzz/api/operator/config/buzz-keyring-revka",
        ))
        .await
        .expect("the app replied");
    assert_eq!(read.status, 200);
    assert!(!String::from_utf8_lossy(&read.body).contains(&key_hex));
    assert_eq!(body_json(&read)["secret_valued"], json!(true));
    let list = env
        .handle(operator_get(&op, "/buzz/api/operator/config"))
        .await
        .expect("the app replied");
    assert!(!String::from_utf8_lossy(&list.body).contains(&key_hex));

    // An invalid persona key never reaches the row.
    let bad = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/config/buzz-keyring-mira",
            json!({ "value": "not-a-key" }).to_string().into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(bad.status, 400);
    assert!(config_value(&env, &keys::keyring(COMMUNITY, "mira"))
        .await
        .is_none());

    // The SECOND flat family, new in the single bundle: an agent DEFINITION
    // (`buzz-agent-def-{agent}`), validated by the runtime's own parser,
    // stored on the loader's row, and READABLE (not a credential).
    let def = json!({ "version": 1, "agent_id": "revka", "default_model": "claude-sonnet-5" });
    let def_write = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/config/buzz-agent-def-revka",
            json!({ "value": def }).to_string().into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(
        def_write.status,
        200,
        "{}",
        String::from_utf8_lossy(&def_write.body)
    );
    assert!(
        config_value(&env, &operator::agent_def_row("revka"))
            .await
            .is_some(),
        "the definition landed on the loader's row"
    );
    let def_read = env
        .handle(operator_get(
            &op,
            "/buzz/api/operator/config/buzz-agent-def-revka",
        ))
        .await
        .expect("the app replied");
    assert_eq!(def_read.status, 200);
    assert_eq!(
        body_json(&def_read)["value"]["default_model"],
        json!("claude-sonnet-5"),
        "a definition is readable — it is configuration, not a credential"
    );
    // A malformed definition is the client's 400, and stores nothing.
    let def_bad = env
        .handle(operator_post(
            &op,
            "/buzz/api/operator/config/buzz-agent-def-mira",
            json!({ "value": { "version": 99 } })
                .to_string()
                .into_bytes(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(def_bad.status, 400);
    assert!(config_value(&env, &operator::agent_def_row("mira"))
        .await
        .is_none());

    });
}

/// A header refused because its `payload` tag does not cover the body must not
/// be burned: a 401 never settles the replay claim, so burning first let a bad
/// body pre-burn a captured token and leave an in-flight claim behind
/// (finding_466d5368ac545168). The same header with the body it signed still
/// works afterwards.
#[test]
fn a_body_mismatch_does_not_burn_the_operator_header() {
    common::run_scenario(|| async move {
        let env = gate_env().await;
        let op = Actor::new("burn-op");
        seed_operator(&env, &op).await;
        let path = "/buzz/api/operator/config/buzz-relay-url";
        let signed_body = json!({ "value": "wss://signed.example" }).to_string().into_bytes();
        let event = Event::sign_new(
            &op.sk,
            kind::KIND_HTTP_AUTH,
            vec![
                vec!["u".into(), format!("https://buzz.test{path}")],
                vec!["method".into(), "POST".into()],
                vec!["payload".into(), to_hex(&sha256(&signed_body))],
                vec!["nonce".into(), nonce()],
            ],
            "",
            T0_SECS,
        )
        .expect("authorization events sign");
        let header = auth_header(&event);
        let tampered = json!({ "value": "wss://tampered.example" }).to_string().into_bytes();
        let refused = env
            .handle(request_with_auth("POST", path, Some(&header), tampered))
            .await
            .expect("the app replied");
        assert_eq!(refused.status, 401, "a body the header does not cover is refused");
        let accepted = env
            .handle(request_with_auth("POST", path, Some(&header), signed_body))
            .await
            .expect("the app replied");
        assert_eq!(
            accepted.status,
            200,
            "the refused attempt must not have burned the header: {}",
            String::from_utf8_lossy(&accepted.body)
        );
        assert_eq!(
            config_value(&env, b"buzz-relay-url").await.as_deref(),
            Some(&b"wss://signed.example"[..]),
        );
    });
}
