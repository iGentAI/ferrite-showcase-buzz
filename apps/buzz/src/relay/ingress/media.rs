//! Media objects: reads, upload limits and staged chunk uploads.

use ferrite_app::tracing;
use ferrite_app::serde_json;
use ferrite_app::serde_json::Value;
use ferrite_app::AppContext;
use ferrite_app::IngressRequest;
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::store;
use crate::relay::BuzzError;
use crate::relay::Result;
use crate::relay::DEFAULT_COMMUNITY;
use super::{HEADER_CONTENT_TYPE, Reply, authenticate_request, enforce_write_gate, storage};
use super::observability::{record_audit_or_alert};

/// Where the gateway serves stored media from: the public content route mapped
/// onto the `media/` key prefix (`ferrite.json`). A client is told this rather
/// than deriving it, so the route can move without a client release.
pub(super) const MEDIA_URL_PREFIX: &str = "/buzz/blob/";

/// The public URL one stored object is served at, by its capability name.
///
/// The NAME is the capability: this URL is unauthenticated, so anyone holding it
/// can fetch the object. That is what lets a client render an image straight
/// from the gateway without the relay proxying bytes, and it is also the whole of
/// the access control — media is not channel-scoped. The name is
/// [`media_capability`], not the content's SHA-256, so it is unguessable whatever
/// the content is.
pub(super) fn media_url(capability: &str, ext: &str) -> String {
    format!("{MEDIA_URL_PREFIX}{capability}.{ext}")
}

/// The config key holding this deployment's media-capability key: 32 random
/// bytes, created on first use and never returned by any route.
const MEDIA_CAPABILITY_KEY: &[u8] = b"secret/media-capability-key";

/// This deployment's media-capability key, created if absent.
///
/// Create-if-absent, then read back: two instances racing to create it converge
/// on whichever write landed first, so every instance names an object alike.
async fn media_capability_key(config: &mut ferrite_app::client::StoreClusterClient) -> Result<Vec<u8>> {
    if let Some(key) = store::get_raw(config, MEDIA_CAPABILITY_KEY).await?.filter(|k| k.len() == 32) {
        return Ok(key);
    }
    let fresh = crate::crypto_kernel::random_bytes(32)
        .map_err(|e| BuzzError::Internal(format!("media key: {e}")))?;
    config
        .put_if_version(MEDIA_CAPABILITY_KEY, &fresh, None)
        .await
        .map_err(|e| BuzzError::Storage(format!("media key write: {e}")))?
        .ok();
    store::get_raw(config, MEDIA_CAPABILITY_KEY)
        .await?
        .filter(|k| k.len() == 32)
        .ok_or_else(|| BuzzError::Storage("media key unavailable".into()))
}

/// The name an object with content digest `sha256` and extension `ext` is stored
/// and served under: an HMAC of both under the deployment's media key.
///
/// The content's own SHA-256 was the name, so anyone who could guess an
/// attachment's content (a short note, a filled template, a known file) could
/// compute its URL and confirm from 200 or 404 that it had been posted, private
/// rooms and DMs included, and receive the bytes (audit finding
/// `apps/buzz/src/relay/ingress/media.rs:content-hash-capability-confirms-guessable-private-media`).
/// The HMAC keeps deduplication (equal content, equal name) without the name
/// being computable from the content.
pub(super) fn media_capability(key: &[u8], sha256: &str, ext: &str) -> String {
    let message = format!("buzz-media/v1\u{1f}{sha256}\u{1f}{ext}");
    crate::relay::crypto::to_hex(&ferrite_app::crypto::hmac_sha256(key, message.as_bytes()))
}

/// `GET /buzz/blob/{name}.{ext}` — one stored media object.
///
/// The name is the capability ([`media_capability`]), so no NIP-98 is
/// asked, and since the demonstration opened no session is either. The path is
/// validated by the same [`keys::media_object`] that named the object at upload,
/// so a malformed digest or extension addresses nothing, and the `Content-Type`
/// comes from the media allowlist for that extension, never from the stored
/// bytes. `nosniff` keeps a browser from second-guessing it. Objects are
/// content-addressed and therefore immutable, so they may be cached for a year.
pub(super) async fn media_object(ctx: &AppContext, path: &str, head: bool) -> Result<Reply> {
    let name = path.strip_prefix(MEDIA_URL_PREFIX).unwrap_or_default();
    let not_found = || BuzzError::NotFound("no such media object".into());
    let (sha, ext) = name.rsplit_once('.').ok_or_else(not_found)?;
    let key = keys::media_object(sha, ext).ok_or_else(not_found)?;
    let mime = crate::relay::media::mime_for_extension(ext).ok_or_else(not_found)?;
    let mut media = planes::blob(ctx, store::MEDIA).map_err(storage)?;
    let bytes = media
        .get(key.as_bytes())
        .await
        .map_err(|e| BuzzError::Storage(format!("media read: {e}")))?
        .ok_or_else(not_found)?;
    Ok(Reply {
        status: 200,
        headers: vec![(HEADER_CONTENT_TYPE.to_string(), mime)],
        body: if head { Vec::new() } else { bytes },
    }
    .with_header("cache-control", "public, max-age=31536000, immutable")
    .with_header("x-content-type-options", "nosniff"))
}

/// `GET /buzz/api/media/limits` — what this deployment accepts.
///
/// Public, like `relay-info` and the model catalog, and for the same reason: a
/// client's file picker must filter on what the relay will actually accept, and a
/// hardcoded client-side list drifts from the server's allowlist the moment
/// either changes. A caller learns the size bound and the chunk size here instead
/// of discovering them by being refused.
pub(super) fn media_limits() -> Result<Vec<u8>> {
    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "max_bytes": crate::relay::media::MAX_MEDIA_BYTES,
        "chunk_bytes": crate::relay::media::CHUNK_BYTES,
        "accepted": crate::relay::media::accepted_mimes(),
        "url_prefix": MEDIA_URL_PREFIX,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/media/begin` — declare an object and learn whether it needs
/// uploading at all.
///
/// Body: `{ sha256, size, mime }`. The reply is an upload id and the chunk
/// geometry to send the bytes with (`have` is always `false`: an object already
/// held is deduplicated when its last chunk arrives, never confirmed here).
///
/// The gates are a WRITE's gates, deliberately: handing a deployment durable
/// storage is at least as consequential as posting a message, so an outsider in a
/// closed community is refused, a banned or timed-out member is refused with
/// their own sanction's sentence, and the rate limiter counts BEGINS rather than
/// completions so that abandoning sessions is bounded too.
pub(super) async fn media_begin(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;

    enforce_write_gate(ctx, community, &caller.pubkey, now_ms).await?;
    // Fails OPEN on an ambiguous increment, like every other limiter here: the
    // membership and sanction gates above are the authority, not this counter.
    match store::bump_expiring_counter(
        &mut config,
        &keys::rate(community, &caller.pubkey, "media"),
        1,
        now_ms.saturating_add(crate::relay::media::UPLOAD_RATE_WINDOW_MS),
    )
    .await
    {
        Ok(count) if count > crate::relay::media::UPLOAD_RATE_LIMIT => {
            return Err(BuzzError::RateLimited)
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("media upload limiter failed open: {e}"),
    }

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("media begin body: {e}")))?;
    let declared = crate::relay::media::validate_declaration(
        body.get("sha256")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        body.get("size").and_then(Value::as_u64).unwrap_or_default(),
        body.get("mime").and_then(Value::as_str).unwrap_or_default(),
    )?;
    // No "already held" shortcut. Answering `have: true` with the object's URL
    // for a declared digest told any member whether that content had been posted
    // anywhere, and handed them its capability URL, without their holding the
    // bytes. The bytes are always sent; the last chunk deduplicates, so the same
    // image posted to ten rooms is still stored once.

    let upload = crate::relay::media::upload_id(&req.correlation_id, now_ms, &caller.pubkey);
    let session = crate::relay::media::MediaUpload {
        sha256: declared.sha256.clone(),
        size: declared.size,
        mime: declared.mime.clone(),
        ext: declared.ext.to_string(),
        by: crate::relay::crypto::to_hex(&caller.pubkey),
        created_ms: now_ms,
    };
    let expires_at_ms = session.expires_at_ms();
    let chunks = session.chunks();
    store::put_expiring_doc(
        &mut config,
        &keys::media_upload(&upload),
        &session,
        expires_at_ms,
    )
    .await?;
    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "have": false,
        "upload": upload,
        "chunk_bytes": crate::relay::media::CHUNK_BYTES,
        "chunks": chunks,
        "expires_at_ms": expires_at_ms,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/media/chunk` — stage one chunk; on `last`, assemble, verify,
/// and publish.
///
/// Body: `{ upload, offset, data: base64, last }`. Staging is an idempotent
/// expiring write, so a retried chunk lands on the same key and costs nothing.
///
/// Four checks stand between these bytes and a publicly-served URL, and each
/// closes something specific:
///
/// * **the staging belongs to this caller** — an upload id is not a bearer token
///   for writing into somebody else's object;
/// * **the assembled length is the declared length** — a short or padded assembly
///   is a torn upload, not a smaller file;
/// * **the digest is the declared digest** — which is what makes the URL
///   content-addressed rather than merely unguessable;
/// * **the bytes are the declared type** — because the type picks the extension,
///   the extension picks the `Content-Type`, and the route is public and
///   same-origin (see [`crate::relay::media`]).
///
/// Only then is ONE blob object written. Assembly happens here rather than on the
/// blob plane precisely so there is no window in which half an object is readable
/// under its final key.
pub(super) async fn media_stage_chunk(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;

    // The SAME gate as `begin`, re-run on every chunk. Authorization is not a
    // capability a caller keeps for the hour their session lives: a member banned,
    // timed out, or removed from a closed community between beginning an upload
    // and finishing it must not be able to stage another byte — and, because the
    // commit happens on the last chunk, must not be able to publish at all.
    enforce_write_gate(ctx, community, &caller.pubkey, now_ms).await?;

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("media chunk body: {e}")))?;
    let upload = body
        .get("upload")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("a chunk needs an upload id".into()))?;
    let offset = body
        .get("offset")
        .and_then(Value::as_u64)
        .ok_or_else(|| BuzzError::InvalidEvent("a chunk needs an offset".into()))?;
    let last = body.get("last").and_then(Value::as_bool).unwrap_or(false);
    let data = body
        .get("data")
        .and_then(Value::as_str)
        .and_then(crate::relay::b64::decode)
        .ok_or_else(|| BuzzError::InvalidEvent("chunk data is not base64".into()))?;
    if data.len() > crate::relay::media::CHUNK_BYTES {
        return Err(BuzzError::InvalidEvent(format!(
            "a chunk is at most {} bytes",
            crate::relay::media::CHUNK_BYTES
        )));
    }

    let session_key = keys::media_upload(upload);
    let Some(session) =
        store::get_doc::<crate::relay::media::MediaUpload>(&mut config, &session_key).await?
    else {
        return Err(BuzzError::NotFound("unknown or expired upload".into()));
    };
    // An upload id is not a capability to write into another member's object.
    // Without this, learning an id (a log, a shared screen) would let anyone
    // replace the staged content of an upload that is about to be published — the
    // digest check would then fail for the RIGHTFUL uploader, which is a denial of
    // service at best and confusing at worst.
    if session.by != crate::relay::crypto::to_hex(&caller.pubkey) {
        return Err(BuzzError::Unauthorized(
            "this upload was begun by another member".into(),
        ));
    }

    let index = crate::relay::media::chunk_index(session.size, offset)?;
    let index = u32::try_from(index)
        .map_err(|_| BuzzError::InvalidEvent("chunk offset is out of range".into()))?;
    let expires_at_ms = session.expires_at_ms();
    config
        .put_expiring(&keys::media_stage(upload, index), &data, expires_at_ms)
        .await
        .map_err(|e| BuzzError::Storage(format!("media chunk stage: {e}")))?;

    if !last {
        return serde_json::to_vec(&serde_json::json!({ "ok": true, "complete": false }))
            .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")));
    }

    // Reassemble in index order. At 128 KiB a chunk the 8 MiB cap is 64 rows,
    // comfortably inside one batched read.
    let count = session.chunks();
    let mut chunk_keys = Vec::new();
    for i in 0..count {
        let i =
            u32::try_from(i).map_err(|_| BuzzError::Internal("chunk count out of range".into()))?;
        chunk_keys.push(keys::media_stage(upload, i));
    }
    let parts = store::multi_get_raw(&mut config, &chunk_keys).await?;
    let mut assembled: Vec<u8> = Vec::with_capacity(usize::try_from(session.size).unwrap_or(0));
    for part in parts {
        let Some(bytes) = part else {
            return Err(BuzzError::InvalidEvent(
                "a chunk is missing or expired; re-send the object".into(),
            ));
        };
        assembled.extend_from_slice(&bytes);
    }
    if u64::try_from(assembled.len()).unwrap_or(u64::MAX) != session.size {
        return Err(BuzzError::InvalidEvent(
            "the assembled object is not the size the upload declared".into(),
        ));
    }
    let digest = crate::relay::crypto::to_hex(&crate::relay::crypto::sha256::sha256(&assembled));
    if digest != session.sha256 {
        return Err(BuzzError::InvalidEvent(
            "the assembled object does not match the sha256 the upload declared".into(),
        ));
    }
    // The bytes must BE the declared type: it decides the extension, which decides
    // the Content-Type this object is served with from the app's own origin.
    crate::relay::media::verify_bytes(&session.mime, &assembled)?;

    let capability = media_capability(&media_capability_key(&mut config).await?, &session.sha256, &session.ext);
    let blob_key = keys::media_object(&capability, &session.ext)
        .ok_or_else(|| BuzzError::Internal("a verified object produced no media key".into()))?;
    let mut media = planes::blob(ctx, store::MEDIA).map_err(storage)?;
    // Content-addressed under the capability name: identical bytes converge on
    // the identical key, so an object already held is not written again.
    let held = media
        .get(blob_key.as_bytes())
        .await
        .map_err(|e| BuzzError::Storage(format!("media read: {e}")))?
        .is_some();
    if !held {
        media
            .put(blob_key.as_bytes(), &assembled)
            .await
            .map_err(|e| BuzzError::Storage(format!("media put: {e}")))?;
    }

    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let audit = crate::relay::audit::AuditDraft::new(
        community,
        crate::relay::audit::AuditAction::MediaUploaded,
        now_ms,
    )
    .actor(&caller.pubkey)
    .object(&session.sha256)
    .with("mime", session.mime.clone())
    .with("bytes", session.size.to_string());
    // The object is durable. The line is fought for and, failing that, raised as a
    // fault — never a reason to tell the caller their upload failed.
    record_audit_or_alert(ctx, &mut guards, community, audit, "media upload").await;

    // Deleted only on SUCCESS. A failed finish leaves the session so a re-drive
    // re-assembles instead of reporting a commit that never happened; the staged
    // chunks carry the session's own expiry either way, so an abandoned upload
    // still erases itself with no sweeper.
    let _ = store::delete_key(&mut config, &session_key).await;

    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "complete": true,
        "url": media_url(&capability, &session.ext),
        "sha256": session.sha256,
        "size": session.size,
        "mime": session.mime,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}
