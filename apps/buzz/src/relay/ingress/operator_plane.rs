//! The operator routes: configuration and web client asset publication.

use ferrite_app::tracing;
use ferrite_app::serde_json;
use ferrite_app::serde_json::Value;
use ferrite_app::rt;
use ferrite_app::AppContext;
use ferrite_app::IngressRequest;
use crate::relay::auth;
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::store;
use crate::relay::BuzzError;
use crate::relay::Result;
use crate::relay::DEFAULT_COMMUNITY;
use super::{FEED_GENERATION, header, storage};
use super::boot::{reconcile_persona_profiles};
use super::ui_assets::ui_asset_versioned_key;
use super::observability::{record_audit_or_alert};

/// The path prefix every per-key config route lives under. The key IS the path
/// suffix, which is what makes NIP-98's exact-path binding a per-key token: a
/// header minted for writing `buzz-relay-url` can never write `buzz-agents`.
pub(super) const OPERATOR_CONFIG_PREFIX: &str = "/buzz/api/operator/config/";

/// Fixed-window bound on operator requests per pubkey. Generous because a UI
/// push is chunk-heavy (~200 requests for the real bundle); fail-open like the
/// invite limiter — the NIP-98 signature and burn, not the limiter, are the
/// gate, and the burn already bounds replay.
pub(super) const OPERATOR_RATE_LIMIT: i64 = 600;
pub(super) const OPERATOR_RATE_WINDOW_MS: u64 = 60_000;

/// Authenticate and authorize one operator request: NIP-98 (binding, freshness,
/// single-use burn — the same markers, in the same config store, as every
/// member route), the body commitment for POSTs, then membership in the
/// operator set.
///
/// WHY A COMMUNITY MEMBER CANNOT ESCALATE INTO THIS SET: membership is sourced
/// from planes no HTTP request can write. The secret arrives only through the
/// owner-gated MCP `set_secret` (platform account bearer token), and when it
/// exists the config fallback is IGNORED entirely — never unioned — so a
/// config-plane writer cannot widen a secret-rooted set. The config fallback
/// key `buzz-operators` is itself refused by the write route's allowlist
/// ([`crate::relay::operator::registry_entry`]), and nothing else on either face
/// writes bare `buzz-*` keys (the only config writes in this codebase are
/// `sess/`, `sesscur/`, `replay/`, `rate/`, `upload/`, and `upchunk/`
/// prefixed). Moderators (`buzz-community-moderators`) are CONSUMERS of config
/// with moderation verbs only — no operator route consults the moderator set,
/// and no moderation verb writes config. Channel owner/admin roles live on the
/// events plane and are never read here. And NIP-98 gives per-request
/// proof-of-possession with burn-on-use, path binding, and body binding, so a
/// captured operator request authorizes nothing beyond itself.
pub(super) async fn operator_caller(
    ctx: &AppContext,
    req: &IngressRequest,
    require_payload: bool,
) -> Result<auth::Caller> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let (caller, _) = auth::authenticate_event(
        &mut config,
        community,
        header(req, "authorization"),
        &req.method,
        &req.path,
        req.verified_host(),
        // Every POST in this family carries an UNSIGNED config document, so
        // the signed header must also commit to the body — one header, one
        // body — checked before the header is burned.
        require_payload.then_some(req.body.as_slice()),
        now_ms,
    )
    .await?;

    // Resolve the operator set: the secret first, else the config fallback
    // (the relay_identity pattern — and the only seam the severe gates can
    // seed, since ferrite-app-test injects an empty secrets map). Absent,
    // unreadable, or malformed all read the same: NO operator exists until
    // the owner seeds one, and every request is refused.
    let refusal = || BuzzError::Unauthorized("not an operator of this deployment".into());
    let set_bytes = match ctx.secret(crate::relay::operator::OPERATOR_SET_SECRET) {
        Some(secret) => Some(secret.into_bytes()),
        None => match config
            .get(crate::relay::operator::OPERATOR_SET_CONFIG)
            .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!("operator set read failed: {e}");
                None
            }
        },
    };
    let Some(bytes) = set_bytes else {
        return Err(refusal());
    };
    let Ok(operators) = crate::relay::operator::parse_operator_set(&bytes) else {
        tracing::warn!(
            "the operator set is malformed; every operator request is refused"
        );
        return Err(refusal());
    };
    if !operators.contains(&caller.pubkey) {
        return Err(refusal());
    }

    // Fail-open fixed window, as the invite limiter: an ambiguous increment
    // must not lock the workspace's own operator out of their config.
    let rate_key = keys::rate(community, &caller.pubkey, "operator");
    match store::bump_expiring_counter(
        &mut config,
        &rate_key,
        1,
        now_ms.saturating_add(OPERATOR_RATE_WINDOW_MS),
    )
    .await
    {
        Ok(count) if count > OPERATOR_RATE_LIMIT => return Err(BuzzError::RateLimited),
        Ok(_) => {}
        Err(e) => tracing::warn!("operator limiter failed open: {e}"),
    }
    Ok(caller)
}

/// The key one per-key config path names, or not-found for a bare prefix.
pub(super) fn operator_config_key(path: &str) -> Result<&str> {
    match path.strip_prefix(OPERATOR_CONFIG_PREFIX) {
        Some(key) if !key.is_empty() => Ok(key),
        _ => Err(BuzzError::NotFound("no such operator config key".into())),
    }
}

/// The two flat row families the operator face may address (see
/// `operator::keyring_agent` and `operator::agent_def_agent`).
pub(super) enum FlatFamily<'a> {
    Keyring(&'a str),
    AgentDef(&'a str),
}

pub(super) fn flat_family(key: &str) -> Option<FlatFamily<'_>> {
    if let Some(agent) = crate::relay::operator::keyring_agent(key) {
        return Some(FlatFamily::Keyring(agent));
    }
    crate::relay::operator::agent_def_agent(key).map(FlatFamily::AgentDef)
}

/// `POST /buzz/api/operator/feed/retire` — delete the delivery topics this
/// deployment's generation has superseded.
///
/// This is the app's only disposal lever on the log plane. Per-topic retention,
/// a prefix trim and per-record erasure do not exist there (a known
/// platform gap in the log lifecycle), so a feed's history
/// grows without bound until a whole generation is retired — and nothing did
/// that before this route, which made "buzz can retire a generation" a claim
/// with no code behind it.
///
/// The body is `{"confirm": true}` to delete and anything else (including an
/// empty object) to plan. Deleting durable history on a mis-typed request is
/// the failure worth one extra field, and the plan answer is the same list the
/// confirmed call acts on. The operator's NIP-98 header commits to that body,
/// so a captured plan request cannot be replayed as a confirmed one.
///
/// It never touches the live generation, the changefeed, or another community's
/// topics — see [`crate::relay::ingest::superseded_feed_generations`].
pub(super) async fn operator_feed_retire(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let _caller = operator_caller(ctx, req, true).await?;
    let confirm = if req.body.is_empty() {
        false
    } else {
        serde_json::from_slice::<Value>(&req.body)
            .map_err(|e| BuzzError::InvalidEvent(format!("retire body: {e}")))?
            .get("confirm")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    let mut topics = planes::topics(ctx, store::FEED).map_err(storage)?;
    let affected = crate::relay::ingest::retire_superseded_feeds(
        &mut topics,
        DEFAULT_COMMUNITY,
        FEED_GENERATION,
        !confirm,
    )
    .await?;
    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "live_generation": FEED_GENERATION,
        "confirmed": confirm,
        // Named for what happened: a plan lists what WOULD go, a confirmed call
        // lists what DID. One field would have made those indistinguishable.
        "topics": affected,
        "retired": if confirm { affected.len() } else { 0 },
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `GET /buzz/api/operator/config` — every key in the table, uniformly
/// REDACTED: presence, size, version, and a fingerprint, never a value. The
/// uniform redaction keeps the reply trivially inside the 512 KiB budget and
/// means listing can never become the leak the secret-valued flag closes.
pub(super) async fn operator_config_list(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let _caller = operator_caller(ctx, req, false).await?;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let mut rows = Vec::with_capacity(crate::relay::operator::WRITABLE_KEYS.len());
    for entry in crate::relay::operator::WRITABLE_KEYS {
        let row = match store::get_raw_versioned(&mut config, entry.key.as_bytes()).await? {
            Some((bytes, version)) => serde_json::json!({
                "key": entry.key,
                "present": true,
                "bytes": bytes.len(),
                "version": version,
                "sha256_16": crate::relay::operator::sha256_16(&bytes),
                "secret_valued": entry.secret_valued,
                "writable": entry.writable,
            }),
            None => serde_json::json!({
                "key": entry.key,
                "present": false,
                "secret_valued": entry.secret_valued,
                "writable": entry.writable,
            }),
        };
        rows.push(row);
    }
    serde_json::to_vec(&serde_json::json!({ "ok": true, "keys": rows }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `GET /buzz/api/operator/config/{key}` — one key's value and version, for
/// the read-modify-write loop. Secret-valued keys answer with the redacted
/// metadata instead: write-only is the contract, even to the operator who
/// wrote them.
pub(super) async fn operator_config_read(
    ctx: &AppContext,
    req: &IngressRequest,
    path: &str,
) -> Result<Vec<u8>> {
    let _caller = operator_caller(ctx, req, false).await?;
    let key = operator_config_key(path)?;
    let community = DEFAULT_COMMUNITY;
    // A persona key is addressed by its flat `buzz-keyring-{agent}` name and
    // stored on the row the consumer reads; see `operator::keyring_agent`.
    if let Some(agent) = crate::relay::operator::keyring_agent(key) {
        let row = keys::keyring(community, agent);
        let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
        let body = match store::get_raw_versioned(&mut config, &row).await? {
            None => serde_json::json!({ "ok": true, "key": key, "present": false }),
            // Always redacted: a persona key is a credential.
            Some((bytes, version)) => serde_json::json!({
                "ok": true,
                "key": key,
                "present": true,
                "bytes": bytes.len(),
                "sha256_16": crate::relay::operator::sha256_16(&bytes),
                "version": version,
                "secret_valued": true,
            }),
        };
        return serde_json::to_vec(&body)
            .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")));
    }
    // An agent definition is addressed by its flat `buzz-agent-def-{agent}`
    // name and stored on the runtime loader's row. Not a credential: readable.
    if let Some(agent) = crate::relay::operator::agent_def_agent(key) {
        let row = crate::relay::operator::agent_def_row(agent);
        let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
        let body = match store::get_raw_versioned(&mut config, &row).await? {
            None => serde_json::json!({ "ok": true, "key": key, "present": false }),
            Some((bytes, version)) => {
                let value = serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| {
                    Value::String(String::from_utf8_lossy(&bytes).into_owned())
                });
                serde_json::json!({ "ok": true, "key": key, "present": true, "value": value, "version": version })
            }
        };
        return serde_json::to_vec(&body)
            .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")));
    }
    let Some(entry) = crate::relay::operator::registry_entry(key) else {
        return Err(BuzzError::NotFound("no such operator config key".into()));
    };
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let body = match store::get_raw_versioned(&mut config, key.as_bytes()).await? {
        None => serde_json::json!({ "ok": true, "key": key, "present": false }),
        Some((bytes, version)) if entry.secret_valued => serde_json::json!({
            "ok": true,
            "key": key,
            "present": true,
            "bytes": bytes.len(),
            "sha256_16": crate::relay::operator::sha256_16(&bytes),
            "version": version,
            "secret_valued": true,
        }),
        Some((bytes, version)) => {
            // Canonical bytes are JSON for every non-secret key except the two
            // raw-string forms (`buzz-community-closed`, `buzz-relay-url`);
            // a raw form that fails the parse is answered as a string.
            let value = serde_json::from_slice::<Value>(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
            serde_json::json!({ "ok": true, "key": key, "present": true, "value": value, "version": version })
        }
    };
    serde_json::to_vec(&body).map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/operator/config/{key}` — write one key.
///
/// Body: `{ "value": <JSON>, "expected_version"?: n }`. The key's validator
/// decides what is storable and returns the canonical bytes, so nothing
/// unvalidated ever reaches the store; `value: null` deletes the key,
/// restoring its documented absent-default (open community, wired-derived
/// harnesses, no join policy, fail-closed authority sets). With
/// `expected_version` the write is a compare-and-swap — a miss is a 409 and
/// the client re-reads.
pub(super) async fn operator_config_write(
    ctx: &AppContext,
    req: &IngressRequest,
    path: &str,
) -> Result<Vec<u8>> {
    let caller = operator_caller(ctx, req, true).await?;
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let key = operator_config_key(path)?;
    // `stored_key` is the row actually written, and `entry` is the table row
    // that validates it. They differ for exactly TWO flat families — a persona
    // signing key (`buzz-keyring-{agent}` → `keyring/{community}/{agent}`) and
    // an agent definition (`buzz-agent-def-{agent}` → the runtime loader's
    // row) — see `operator::keyring_agent` / `operator::agent_def_agent` for
    // why those are reachable under flat names and no other '/'-keyed row is.
    let entry = match flat_family(key) {
        Some(_) => None,
        None => {
            let Some(entry) = crate::relay::operator::registry_entry(key) else {
                // Not in the table ⇒ this face has no such key. This is the arm
                // that keeps the credentials (buzz-operators,
                // buzz-relay-identity, …) and every '/'-prefixed row family out
                // of reach by construction.
                return Err(BuzzError::NotFound("no such operator config key".into()));
            };
            if !entry.writable {
                return Err(BuzzError::InvalidEvent(
                    "buzz-web-assets-manifest is written only by the assets finish step".into(),
                ));
            }
            Some(entry)
        }
    };
    let stored_key = match flat_family(key) {
        Some(FlatFamily::Keyring(agent)) => keys::keyring(community, agent),
        Some(FlatFamily::AgentDef(agent)) => crate::relay::operator::agent_def_row(agent),
        None => key.as_bytes().to_vec(),
    };
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("write body: {e}")))?;
    let Some(value) = body.get("value") else {
        return Err(BuzzError::InvalidEvent(
            "write body needs a value field".into(),
        ));
    };
    let expected_version = body.get("expected_version").and_then(Value::as_u64);
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;

    let (version, fingerprint) = if value.is_null() {
        if let Some(expected) = expected_version {
            // The platform has no delete-if-version primitive, so the compare
            // half is a read immediately before the tombstone. That honours
            // the documented contract — a STALE decision is refused as a 409
            // instead of silently wiping a concurrent write — leaving only
            // the in-flight race a real CAS delete would also need the
            // platform to close.
            let current = store::get_raw_versioned(&mut config, &stored_key)
                .await?
                .map(|(_, version)| version);
            if current != Some(expected) {
                return Err(BuzzError::Conflict(
                    "expected_version is stale; re-read the key".into(),
                ));
            }
        }
        store::delete_key(&mut config, &stored_key).await?;
        (None, "deleted".to_string())
    } else {
        let canonical = match entry {
            Some(entry) => entry.validate(value)?,
            None => match flat_family(key) {
                Some(FlatFamily::AgentDef(agent)) => {
                    crate::relay::operator::validate_agent_definition(agent, value)?
                }
                _ => crate::relay::operator::validate_persona_key(value)?,
            },
        };
        let fingerprint = crate::relay::operator::sha256_16(&canonical);
        let version = match expected_version {
            Some(expected) => match config
                .put_if_version(&stored_key, &canonical, Some(expected))
                .await
            {
                Ok(Ok(version)) => version,
                Ok(Err(_)) => {
                    return Err(BuzzError::Conflict(
                        "expected_version is stale; re-read the key".into(),
                    ))
                }
                Err(e) => return Err(BuzzError::Storage(format!("config write: {e}"))),
            },
            None => store::put_raw(&mut config, &stored_key, &canonical).await?,
        };
        (Some(version), fingerprint)
    };

    // The audit chain is exactly for this: who turned which knob, when, to
    // what — by fingerprint, because the value may be secret-valued.
    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let audit = crate::relay::audit::AuditDraft::new(
        community,
        crate::relay::audit::AuditAction::ConfigUpdated,
        now_ms,
    )
    .actor(&caller.pubkey)
    .object(key)
    .with("sha256_16", fingerprint.clone());
    // The write is durable. The line is fought for and, failing that, raised as a
    // fault — never a reason to report the operator's change as failed (the
    // create_channel rule).
    record_audit_or_alert(ctx, &mut guards, community, audit, "operator config write").await;

    // A `buzz-agents` write replaces the WHOLE roster, so it can add several
    // personas at once or restore one that had been removed. Re-run the same
    // idempotent reconcile over the result rather than trying to diff what
    // changed: it admits any new persona to the community and gives it a profile,
    // which is what makes it mentionable and — in a closed community — able to
    // write at all. Spawned, because an operator's config write should not wait on
    // a roster walk to be told it succeeded.
    if key.as_bytes() == crate::relay::agents::REGISTRY_CONFIG {
        let reconcile_ctx = ctx.clone();
        rt::spawn(async move {
            reconcile_persona_profiles(&reconcile_ctx).await;
        });
    }

    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "key": key,
        "version": version,
        "sha256_16": fingerprint,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// The committed UI manifest, tolerantly: an unreadable manifest reads as
/// ABSENT here (warned), so a full push can always heal a corrupt row —
/// `begin` treats everything as missing and `finish` overwrites it. (The boot
/// seeding makes the OPPOSITE call on the same fault: it skips, because its
/// failure mode is clobbering a pushed bundle, not refusing an upload.)
pub(super) async fn current_ui_manifest(
    config: &mut ferrite_app::client::StoreClusterClient,
) -> Result<Option<crate::relay::operator::Manifest>> {
    match store::get_raw(config, crate::relay::operator::MANIFEST_CONFIG).await? {
        Some(bytes) => match serde_json::from_slice::<crate::relay::operator::Manifest>(&bytes) {
            Ok(manifest) => Ok(Some(manifest)),
            Err(e) => {
                tracing::warn!(
                    "web-assets manifest is unreadable ({e}); treating it as absent"
                );
                Ok(None)
            }
        },
        None => Ok(None),
    }
}

/// `POST /buzz/api/operator/assets/begin` — start a UI-bundle upload.
///
/// Body: `{ "files": [{path,size,sha256}], "prune"?: bool }`. The reply's
/// `missing` list is the diff against the CURRENT manifest — one store read,
/// never a hash of ~500 blob objects inside one request — so an incremental
/// push re-sends only what changed; unchanged paths are pre-marked done. The
/// session is an EXPIRING doc: an abandoned upload self-erases after
/// [`crate::relay::operator::UPLOAD_TTL_MS`] with no sweeper.
pub(super) async fn operator_assets_begin(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let caller = operator_caller(ctx, req, true).await?;
    let now_ms = ctx.now_ms();
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("begin body: {e}")))?;
    let files: Vec<crate::relay::operator::ManifestFile> = body
        .get("files")
        .cloned()
        .ok_or_else(|| BuzzError::InvalidEvent("begin needs a files array".into()))
        .and_then(|v| {
            serde_json::from_value(v)
                .map_err(|e| BuzzError::InvalidEvent(format!("begin files: {e}")))
        })?;
    let files = crate::relay::operator::validate_bundle(&files)?;
    let prune = body.get("prune").and_then(Value::as_bool).unwrap_or(false);

    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let current = current_ui_manifest(&mut config).await?;
    let mut pre_done = std::collections::BTreeSet::new();
    let mut missing = Vec::new();
    for file in &files {
        let unchanged = current.as_ref().is_some_and(|m| {
            m.files
                .iter()
                .any(|f| f.path == file.path && f.sha256 == file.sha256)
        });
        if unchanged {
            pre_done.insert(file.path.clone());
        } else {
            missing.push(file.path.clone());
        }
    }

    let upload = crate::relay::operator::upload_id(&req.correlation_id, now_ms, &caller.pubkey);
    let session = crate::relay::operator::UploadSession {
        files,
        prune,
        by: crate::relay::crypto::to_hex(&caller.pubkey),
        created_ms: now_ms,
        pre_done,
    };
    let expires_at_ms = session.expires_at_ms();
    store::put_expiring_doc(&mut config, &keys::upload(&upload), &session, expires_at_ms).await?;
    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "upload": upload,
        "expires_at_ms": expires_at_ms,
        "missing": missing,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/operator/assets/chunk` — stage one chunk; on `last`,
/// assemble, verify, and publish the file.
///
/// Body: `{ "upload", "path", "offset", "data": base64, "last": bool }`.
/// Staging is an idempotent expiring rewrite (a retried chunk lands on the
/// same key), and the blob is touched only after the WHOLE file's length and
/// SHA-256 match the manifest — an integrity failure refuses to name the path
/// and leaves the staging rows to expire, so the served `ui/` tree can never
/// hold a half-verified object. Staged keys are never publicly served: the
/// content route maps only the `ui/` key prefix.
pub(super) async fn operator_assets_chunk(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let _caller = operator_caller(ctx, req, true).await?;
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("chunk body: {e}")))?;
    let upload = body
        .get("upload")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("chunk needs an upload id".into()))?;
    let path = body
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("chunk needs a path".into()))?;
    let offset = body
        .get("offset")
        .and_then(Value::as_u64)
        .ok_or_else(|| BuzzError::InvalidEvent("chunk needs an offset".into()))?;
    let last = body.get("last").and_then(Value::as_bool).unwrap_or(false);
    let data = body
        .get("data")
        .and_then(Value::as_str)
        .and_then(crate::relay::b64::decode)
        .ok_or_else(|| BuzzError::InvalidEvent("chunk data is not base64".into()))?;
    if data.len() > crate::relay::operator::CHUNK_BYTES {
        return Err(BuzzError::InvalidEvent(format!(
            "a chunk is at most {} bytes",
            crate::relay::operator::CHUNK_BYTES
        )));
    }

    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let session_key = keys::upload(upload);
    let Some(session) =
        store::get_doc::<crate::relay::operator::UploadSession>(&mut config, &session_key).await?
    else {
        return Err(BuzzError::NotFound("unknown or expired upload".into()));
    };
    let Some(file) = session.files.iter().find(|f| f.path == path) else {
        return Err(BuzzError::InvalidEvent(
            "the upload's manifest does not name this path".into(),
        ));
    };
    let index = crate::relay::operator::chunk_index(file.size, offset)?;
    let index = u32::try_from(index)
        .map_err(|_| BuzzError::InvalidEvent("chunk offset is out of range".into()))?;
    let sha16 = crate::relay::operator::path_sha16(path);
    let expires_at_ms = session.expires_at_ms();
    let chunk_key = keys::upchunk(upload, &sha16, index);
    config
        .put_expiring(&chunk_key, &data, expires_at_ms)
        .await
        .map_err(|e| BuzzError::Storage(format!("chunk stage: {e}")))?;

    if !last {
        return serde_json::to_vec(&serde_json::json!({ "ok": true, "path": path, "complete": false }))
            .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")));
    }

    // Reassemble in index order (≤64 chunks for the 8 MiB cap, well under
    // MAX_MULTI_GET) and hold the result against the manifest's promise.
    let count = crate::relay::operator::chunk_count(file.size);
    let mut chunk_keys = Vec::new();
    for i in 0..count {
        let i =
            u32::try_from(i).map_err(|_| BuzzError::Internal("chunk count out of range".into()))?;
        chunk_keys.push(keys::upchunk(upload, &sha16, i));
    }
    let parts = store::multi_get_raw(&mut config, &chunk_keys).await?;
    let mut assembled: Vec<u8> = Vec::new();
    for part in parts {
        let Some(bytes) = part else {
            return Err(BuzzError::InvalidEvent(
                "a chunk is missing or expired; re-send the file".into(),
            ));
        };
        assembled.extend_from_slice(&bytes);
    }
    if u64::try_from(assembled.len()).unwrap_or(u64::MAX) != file.size
        || crate::relay::crypto::to_hex(&crate::relay::crypto::sha256::sha256(&assembled))
            != file.sha256
    {
        return Err(BuzzError::InvalidEvent(
            "assembled file does not match the manifest's size and sha256".into(),
        ));
    }

    // Write-if-changed onto the immutable content-addressed key first (`ui-sha256/{sha256}`).
    // Unversioned `ui/{path}` keys are updated only when `finish` commits the
    // manifest, so an in-progress upload never overwrites live assets served
    // under the previous manifest.
    let _blob_key = keys::ui_asset(path)
        .ok_or_else(|| BuzzError::Internal("a validated path stopped being a ui key".into()))?;
    let versioned_key = ui_asset_versioned_key(&file.sha256);
    let mut assets = planes::blob(ctx, store::WEB_ASSETS).map_err(storage)?;
    let existing = assets
        .get(versioned_key.as_bytes())
        .await
        .map_err(|e| BuzzError::Storage(format!("web asset read: {e}")))?;
    if existing.as_deref() != Some(assembled.as_slice()) {
        assets
            .put(versioned_key.as_bytes(), &assembled)
            .await
            .map_err(|e| BuzzError::Storage(format!("web asset put: {e}")))?;
    }
    // The completion marker: ONE small unconditional write on a key of this
    // path's own, carrying the session's expiry. It is written AFTER the blob, so
    // a crash in between leaves the path unverified and `finish` still refuses —
    // the marker means "these exact bytes are on the served key", and nothing
    // else may create it.
    config
        .put_expiring(&keys::updone(upload, &sha16), b"1", expires_at_ms)
        .await
        .map_err(|e| BuzzError::Storage(format!("upload completion mark: {e}")))?;
    serde_json::to_vec(&serde_json::json!({ "ok": true, "path": path, "complete": true }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// The manifest paths of `session` that have no verified `updone/` row yet.
///
/// Batched (`multi_get`), so proving a 499-file bundle costs two round trips
/// rather than 499 — and costs nothing at all for the paths `begin` already
/// resolved as byte-identical.
pub(super) async fn unverified_upload_paths(
    config: &mut ferrite_app::client::StoreClusterClient,
    upload: &str,
    session: &crate::relay::operator::UploadSession,
) -> Result<Vec<String>> {
    let awaiting = session.awaiting_verification();
    let keys: Vec<Vec<u8>> = awaiting
        .iter()
        .map(|path| keys::updone(upload, &crate::relay::operator::path_sha16(path)))
        .collect();
    let rows = store::multi_get_raw(config, &keys).await?;
    Ok(awaiting
        .iter()
        .zip(rows.iter())
        .filter(|(_, row)| row.is_none())
        .map(|(path, _)| (*path).to_string())
        .collect())
}

/// `POST /buzz/api/operator/assets/finish` — commit the bundle.
///
/// Refused unless every manifest path is verified-done. The NEW manifest is
/// written BEFORE any pruning — the listed order would delete under a manifest
/// that still names the deleted keys, whereas this order's worst crash leaves
/// only orphaned blobs the next prune sweeps. Prune's enumeration comes from
/// the OLD manifest (or, before any exists, from the embedded asset list),
/// because the blob plane has no scan: manifests are the only enumeration of
/// the `ui/` tree.
pub(super) async fn operator_assets_finish(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let caller = operator_caller(ctx, req, true).await?;
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("finish body: {e}")))?;
    let upload = body
        .get("upload")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("finish needs an upload id".into()))?;

    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let session_key = keys::upload(upload);
    let Some(session) =
        store::get_doc::<crate::relay::operator::UploadSession>(&mut config, &session_key).await?
    else {
        return Err(BuzzError::NotFound("unknown or expired upload".into()));
    };
    // Every path must be verified — either byte-identical at `begin` or carrying
    // its own `updone/` row. The refusal names one of them, because "3 files are
    // missing" without a name is an unactionable error for a 499-file push.
    let unverified = unverified_upload_paths(&mut config, upload, &session).await?;
    if let Some(first) = unverified.first() {
        return Err(BuzzError::InvalidEvent(format!(
            "{} file(s) are not uploaded and verified yet (e.g. {first})",
            unverified.len()
        )));
    }

    let old = current_ui_manifest(&mut config).await?;
    let mut assets = planes::blob(ctx, store::WEB_ASSETS).map_err(storage)?;
    for path in session.awaiting_verification() {
        let Some(file) = session.files.iter().find(|f| f.path == path) else {
            continue;
        };
        let _blob_key = keys::ui_asset(&file.path)
            .ok_or_else(|| BuzzError::Internal("a validated path stopped being a ui key".into()))?;
        let versioned_key = ui_asset_versioned_key(&file.sha256);
        let present = assets
            .get(versioned_key.as_bytes())
            .await
            .map_err(|e| BuzzError::Storage(format!("web asset read: {e}")))?
            .is_some();
        if !present {
            return Err(BuzzError::InvalidEvent(format!(
                "verified asset {} is missing from web-assets blob",
                file.path
            )));
        }
    }

    // Backfill immutable `ui-sha256/{sha256}` objects for every asset in the
    // committed manifest (one at a time) before committing the new
    // manifest or overwriting/pruning unversioned `ui/` keys, so in-flight reads
    // that selected an older unversioned manifest still resolve its exact files.
    if let Some(ref old_manifest) = old {
        for old_file in &old_manifest.files {
            let versioned_key = ui_asset_versioned_key(&old_file.sha256);
            let has_versioned = assets
                .get(versioned_key.as_bytes())
                .await
                .map_err(|e| BuzzError::Storage(format!("web asset read: {e}")))?
                .is_some();
            if !has_versioned {
                if let Some(old_blob_key) = keys::ui_asset(&old_file.path) {
                    if let Some(old_bytes) = assets
                        .get(old_blob_key.as_bytes())
                        .await
                        .map_err(|e| BuzzError::Storage(format!("web asset read: {e}")))?
                    {
                        if u64::try_from(old_bytes.len()).ok() == Some(old_file.size)
                            && crate::relay::crypto::to_hex(
                                &crate::relay::crypto::sha256::sha256(&old_bytes),
                            ) == old_file.sha256
                        {
                            assets
                                .put(versioned_key.as_bytes(), &old_bytes)
                                .await
                                .map_err(|e| {
                                    BuzzError::Storage(format!("web asset backfill put: {e}"))
                                })?;
                        }
                    }
                }
            }
        }
    }

    let etag = crate::relay::operator::manifest_etag(&session.files);
    let manifest = crate::relay::operator::Manifest {
        etag: etag.clone(),
        files: session.files.clone(),
        committed_ms: now_ms,
        by: session.by.clone(),
    };
    let manifest_bytes = store::encode_doc(&manifest)?;
    store::put_raw(
        &mut config,
        crate::relay::operator::MANIFEST_CONFIG,
        &manifest_bytes,
    )
    .await?;

    // Copy newly uploaded assets onto their unversioned `ui/` compatibility keys
    // one at a time, avoiding holding the entire bundle in memory at once.
    for path in session.awaiting_verification() {
        let Some(file) = session.files.iter().find(|f| f.path == path) else {
            continue;
        };
        let blob_key = keys::ui_asset(&file.path)
            .ok_or_else(|| BuzzError::Internal("a validated path stopped being a ui key".into()))?;
        let versioned_key = ui_asset_versioned_key(&file.sha256);
        let bytes = assets
            .get(versioned_key.as_bytes())
            .await
            .map_err(|e| BuzzError::Storage(format!("web asset read: {e}")))?
            .ok_or_else(|| {
                BuzzError::InvalidEvent(format!(
                    "verified asset {} is missing from web-assets blob",
                    file.path
                ))
            })?;
        let existing = assets
            .get(blob_key.as_bytes())
            .await
            .map_err(|e| BuzzError::Storage(format!("web asset read: {e}")))?;
        if existing.as_deref() != Some(bytes.as_slice()) {
            assets
                .put(blob_key.as_bytes(), &bytes)
                .await
                .map_err(|e| BuzzError::Storage(format!("web asset put: {e}")))?;
        }
    }

    let mut pruned = 0usize;
    if session.prune {
        let new_keys: std::collections::BTreeSet<String> = session
            .files
            .iter()
            .filter_map(|f| keys::ui_asset(&f.path))
            .collect();
        let old_keys: std::collections::BTreeSet<String> = match &old {
            Some(m) => m
                .files
                .iter()
                .filter_map(|f| keys::ui_asset(&f.path))
                .collect(),
            // No manifest yet ⇒ the only prior writer was the boot seeding.
            None => crate::relay::assets::ui::UI_ASSETS
                .iter()
                .map(|(key, _)| (*key).to_string())
                .collect(),
        };
        for key in old_keys.difference(&new_keys) {
            match assets.delete(key.as_bytes()).await {
                Ok(true) => pruned += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!("prune of {key} failed: {e}"),
            }
        }
    }

    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let audit = crate::relay::audit::AuditDraft::new(
        community,
        crate::relay::audit::AuditAction::UiBundleCommitted,
        now_ms,
    )
    .actor(&caller.pubkey)
    .object(etag.clone())
    .with("files", session.files.len().to_string());
    record_audit_or_alert(ctx, &mut guards, community, audit, "ui bundle commit").await;
    let _ = store::delete_key(&mut config, &session_key).await;

    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "etag": etag,
        "files": session.files.len(),
        "pruned": pruned,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}
