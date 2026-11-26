//! Media upload gates: what reaches the blob plane, and what must not.
//!
//! The read half of media has been wired since the first deploy — `/buzz/blob/`
//! is a PUBLIC content route over the `media` blob binding — so everything these
//! gates assert about a stored object is also an assertion about what the world
//! can fetch. That is why the refusal cases matter more than the happy path: an
//! object that reaches the blob has already escaped.

use super::common;

use buzz::relay::crypto::sha256::sha256;
use buzz::relay::crypto::to_hex;
use buzz::relay::{b64, keys};
use common::{Actor, SUBMIT_ATTEMPTS, body_json, gate_env, error_text, post, submit};
use ferrite_app::serde_json::{self, json, Value};
use ferrite_app::{IngressReply, IngressRequest};
use common::SsoTestEnv as TestEnv;

const BEGIN: &str = "/buzz/api/media/begin";
const CHUNK: &str = "/buzz/api/media/chunk";

/// A valid PNG of `size` bytes: the real signature, then filler.
fn png(size: usize) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.resize(size.max(bytes.len()), 0x5a);
    bytes
}

fn digest(bytes: &[u8]) -> String {
    to_hex(&sha256(bytes))
}

/// Send one request, re-posting if the reply is lost.
///
/// The same frozen-clock re-send discipline `common::submit` documents: under a
/// manual clock a queue lease never expires, so a lost reply is permanent rather
/// than slow. Each attempt mints a fresh authorization header.
async fn send(env: &TestEnv, make: impl Fn() -> IngressRequest) -> IngressReply {
    for attempt in 1..=SUBMIT_ATTEMPTS {
        match env.handle(make()).await {
            Ok(reply) => return reply,
            Err(e) if attempt == SUBMIT_ATTEMPTS => panic!("no reply after {SUBMIT_ATTEMPTS}: {e}"),
            Err(_) => continue,
        }
    }
    unreachable!()
}

async fn begin(env: &TestEnv, who: &Actor, body: Value) -> IngressReply {
    let bytes = serde_json::to_vec(&body).expect("encodes");
    send(env, || post(who, BEGIN, bytes.clone())).await
}

async fn chunk(env: &TestEnv, who: &Actor, body: Value) -> IngressReply {
    let bytes = serde_json::to_vec(&body).expect("encodes");
    send(env, || post(who, CHUNK, bytes.clone())).await
}

/// Upload one object end to end, answering with the finish reply.
async fn upload(env: &TestEnv, who: &Actor, bytes: &[u8], mime: &str) -> IngressReply {
    let opened = begin(
        env,
        who,
        json!({ "sha256": digest(bytes), "size": bytes.len(), "mime": mime }),
    )
    .await;
    assert_eq!(opened.status, 200, "begin: {}", error_text(&opened));
    let id = body_json(&opened)
        .get("upload")
        .and_then(Value::as_str)
        .expect("an upload id")
        .to_string();
    chunk(
        env,
        who,
        json!({ "upload": id, "offset": 0, "data": b64::encode(bytes), "last": true }),
    )
    .await
}

/// What the blob plane holds under one media key.
async fn stored_object(env: &TestEnv, sha: &str, ext: &str) -> Option<Vec<u8>> {
    let key = keys::media_object(sha, ext).expect("a valid media key");
    let mut media = env.blob("media");
    media.get(key.as_bytes()).await.expect("blob read")
}

/// The capability name a served media URL carries, checked for shape.
fn capability_of(url: &str, ext: &str) -> String {
    let name = url
        .strip_prefix("/buzz/blob/")
        .and_then(|rest| rest.strip_suffix(&format!(".{ext}")))
        .unwrap_or_else(|| panic!("not a media URL: {url}"));
    assert!(name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()), "{url}");
    name.to_string()
}

/// **The happy path, asserted where it matters: on the blob plane.**
///
/// The reply's URL is not the claim — the BYTES being fetchable are, because the
/// content route serves whatever is under that key to anyone. The key must also
/// carry the type's extension: the gateway derives `Content-Type` from it and
/// nothing else, so an extensionless key would serve every image as a download.
#[test]
 fn severe_an_uploaded_image_is_stored_under_a_key_that_names_its_type() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("media-ana");
    let bytes = png(4096);
    let sha = digest(&bytes);

    let done = upload(&env, &ana, &bytes, "image/png").await;
    assert_eq!(done.status, 200, "finish: {}", error_text(&done));
    let reply = body_json(&done);
    assert_eq!(reply.get("complete").and_then(Value::as_bool), Some(true));
    assert_eq!(
        reply.get("sha256").and_then(Value::as_str),
        Some(sha.as_str())
    );

    // The URL the relay answers with is the one a client attaches: the public
    // content route, the object's capability name AND the extension. The name
    // is not the content's digest, which anyone who can guess the content can
    // compute.
    let url = reply.get("url").and_then(Value::as_str).expect("a url");
    let name = capability_of(url, "png");
    assert_ne!(name, sha, "the served name is the guessable content hash");

    // And the bytes really are there, byte for byte, under that name only.
    assert_eq!(
        stored_object(&env, &name, "png").await.as_deref(),
        Some(bytes.as_slice()),
        "the object the content route would serve"
    );
    assert_eq!(stored_object(&env, &sha, "png").await, None, "stored under its content hash");

    });
}

/// **Identical bytes are stored once, and `begin` confirms nothing.**
///
/// The same bytes still converge on the same key, so re-sharing stores nothing
/// new and an interrupted upload is safe to retry. But `begin` no longer answers
/// "already held" with the object's URL for a declared digest: that told any
/// member whether content had been posted anywhere, private rooms included, and
/// handed them its URL without their holding the bytes (audit finding
/// `apps/buzz/src/relay/ingress/media.rs:content-hash-capability-confirms-guessable-private-media`).
#[test]
 fn severe_a_second_upload_of_identical_bytes_is_stored_once_and_confirms_nothing() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("media-ana");
    let bo = Actor::new("media-bo");
    let bytes = png(2048);
    let sha = digest(&bytes);

    let first = upload(&env, &ana, &bytes, "image/png").await;
    assert_eq!(first.status, 200);
    let first_url = body_json(&first)["url"].as_str().expect("a url").to_string();

    // A DIFFERENT member declaring the SAME digest learns nothing about it.
    let again = begin(
        &env,
        &bo,
        json!({ "sha256": sha, "size": bytes.len(), "mime": "image/png" }),
    )
    .await;
    assert_eq!(again.status, 200, "{}", error_text(&again));
    let reply = body_json(&again);
    assert_eq!(reply.get("have").and_then(Value::as_bool), Some(false), "begin confirmed the object exists");
    assert!(reply.get("url").is_none(), "begin handed out the URL without the bytes");
    assert!(reply.get("upload").is_some(), "a session is opened for the bytes");

    // Sending the bytes yields the same object, stored once.
    let second = upload(&env, &bo, &bytes, "image/png").await;
    assert_eq!(second.status, 200, "{}", error_text(&second));
    assert_eq!(body_json(&second)["url"].as_str(), Some(first_url.as_str()), "identical bytes converge on one object");

    });
}

/// **A lying digest is refused, and leaves nothing behind.**
///
/// The digest IS the object's name and its capability, so bytes that do not hash
/// to their declaration must never reach the key they claimed — otherwise one
/// upload could overwrite the content of another object's URL.
#[test]
 fn severe_bytes_that_do_not_match_their_digest_reach_no_blob() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("media-ana");
    let honest = png(1024);
    let claimed = digest(&png(2048)); // a DIFFERENT image's digest

    let opened = begin(
        &env,
        &ana,
        json!({ "sha256": claimed, "size": honest.len(), "mime": "image/png" }),
    )
    .await;
    assert_eq!(opened.status, 200);
    let id = body_json(&opened)
        .get("upload")
        .and_then(Value::as_str)
        .expect("an upload id")
        .to_string();

    let refused = chunk(
        &env,
        &ana,
        json!({ "upload": id, "offset": 0, "data": b64::encode(&honest), "last": true }),
    )
    .await;
    assert_eq!(refused.status, 400, "{}", error_text(&refused));
    assert!(
        error_text(&refused).contains("sha256"),
        "the refusal names the digest: {}",
        error_text(&refused)
    );
    assert_eq!(
        stored_object(&env, &claimed, "png").await,
        None,
        "nothing may be published under a digest the bytes do not have"
    );

    });
}

/// **A lying TYPE is refused, and leaves nothing behind.**
///
/// This is the stored-XSS case. The declared type picks the extension, the
/// extension picks the `Content-Type`, and the route is public and served from
/// the app's own origin — so HTML admitted as `image/png` is a script the
/// workspace hosts for anyone who fetches it.
#[test]
 fn severe_html_declared_as_an_image_reaches_no_blob() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("media-ana");
    let hostile = b"<html><script>alert(document.cookie)</script></html>".to_vec();
    let sha = digest(&hostile);

    let opened = begin(
        &env,
        &ana,
        json!({ "sha256": sha, "size": hostile.len(), "mime": "image/png" }),
    )
    .await;
    assert_eq!(opened.status, 200, "the declaration alone is plausible");
    let id = body_json(&opened)
        .get("upload")
        .and_then(Value::as_str)
        .expect("an upload id")
        .to_string();

    let refused = chunk(
        &env,
        &ana,
        json!({ "upload": id, "offset": 0, "data": b64::encode(&hostile), "last": true }),
    )
    .await;
    assert_eq!(refused.status, 400, "{}", error_text(&refused));
    assert_eq!(
        stored_object(&env, &sha, "png").await,
        None,
        "a scriptable payload must never reach a public same-origin key"
    );

    // And the type it really is cannot be declared either: the allowlist has no
    // entry for HTML at all, so there is no spelling that would store it.
    let named = begin(
        &env,
        &ana,
        json!({ "sha256": sha, "size": hostile.len(), "mime": "text/html" }),
    )
    .await;
    assert_eq!(named.status, 400, "{}", error_text(&named));

    });
}

/// **An upload id is not a bearer token.**
///
/// Learning someone's upload id — from a log, a shared screen — must not let you
/// write into the object they are about to publish. Without the owner check the
/// rightful uploader's digest check would then fail, which is a denial of service
/// at best and a content swap at worst.
#[test]
 fn severe_another_member_cannot_stage_into_someone_elses_upload() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("media-ana");
    let mallory = Actor::new("media-mallory");
    let bytes = png(1024);

    let opened = begin(
        &env,
        &ana,
        json!({ "sha256": digest(&bytes), "size": bytes.len(), "mime": "image/png" }),
    )
    .await;
    let id = body_json(&opened)
        .get("upload")
        .and_then(Value::as_str)
        .expect("an upload id")
        .to_string();

    let refused = chunk(
        &env,
        &mallory,
        json!({ "upload": id, "offset": 0, "data": b64::encode(&png(1024)), "last": false }),
    )
    .await;
    assert_eq!(refused.status, 403, "{}", error_text(&refused));

    // Ana's own upload still completes: the refusal cost her nothing.
    let done = chunk(
        &env,
        &ana,
        json!({ "upload": id, "offset": 0, "data": b64::encode(&bytes), "last": true }),
    )
    .await;
    assert_eq!(done.status, 200, "{}", error_text(&done));

    });
}

/// **The limits a client filters its picker by are the limits the route
/// enforces**, and they are public so onboarding can read them before anyone has
/// authenticated.
#[test]
 fn severe_the_published_limits_match_what_the_route_accepts() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("media-ana");

    let limits = env
        .handle(common::get("/buzz/api/media/limits"))
        .await
        .expect("a reply");
    assert_eq!(limits.status, 200, "the limits are public");
    let doc = body_json(&limits);
    let accepted: Vec<String> = doc
        .get("accepted")
        .and_then(Value::as_array)
        .expect("an accepted list")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert!(accepted.iter().any(|m| m == "image/png"));
    assert!(
        !accepted
            .iter()
            .any(|m| m == "image/svg+xml" || m == "text/html"),
        "a scriptable type must never be advertised as acceptable"
    );

    // The published cap is the enforced cap: one byte over is refused, and the
    // refusal costs the caller nothing but a round trip.
    let max = doc.get("max_bytes").and_then(Value::as_u64).expect("a cap");
    let over = begin(
        &env,
        &ana,
        json!({ "sha256": digest(b"x"), "size": max + 1, "mime": "image/png" }),
    )
    .await;
    assert_eq!(over.status, 400, "{}", error_text(&over));

    // A type absent from the published list is refused at the declaration.
    let scriptable = begin(
        &env,
        &ana,
        json!({ "sha256": digest(b"<svg/>"), "size": 6, "mime": "image/svg+xml" }),
    )
    .await;
    assert_eq!(scriptable.status, 400, "{}", error_text(&scriptable));

    });
}

/// **An uploaded object is fetchable at the URL the relay handed out.**
///
/// The manifest's `/buzz/blob/` content route is shadowed by the `/` api
/// catch-all (content routes are consulted only when no api route matched), so
/// the app serves the read half itself. Before it did, every attachment URL
/// answered "no route".
#[test]
 fn severe_an_uploaded_object_is_served_at_its_url() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("media-ana");
    let bytes = png(3000);
    let done = upload(&env, &ana, &bytes, "image/png").await;
    assert_eq!(done.status, 200, "finish: {}", error_text(&done));
    let url = body_json(&done)["url"].as_str().expect("a url").to_string();

    let served = env
        .handle(common::get(&url))
        .await
        .expect("a reply");
    assert_eq!(served.status, 200, "{url}: {}", error_text(&served));
    assert_eq!(served.body, bytes, "the object's bytes, verbatim");
    let header = |name: &str| {
        served
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(header("content-type").as_deref(), Some("image/png"));
    assert_eq!(header("x-content-type-options").as_deref(), Some("nosniff"));

    // Absent objects, malformed digests and types outside the allowlist are 404s.
    let sha = digest(&bytes);
    for missing in [
        // The content's own digest addresses nothing: guessing the content no
        // longer yields the object.
        format!("/buzz/blob/{sha}.png"),
        format!("/buzz/blob/{}.png", digest(b"never uploaded")),
        format!("/buzz/blob/{sha}.svg"),
        "/buzz/blob/not-a-digest.png".to_string(),
        format!("/buzz/blob/{sha}"),
    ] {
        let reply = env
            .handle(common::get(&missing))
            .await
            .expect("a reply");
        assert_eq!(reply.status, 404, "{missing}: {}", error_text(&reply));
    }

    });
}
