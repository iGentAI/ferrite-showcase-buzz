//! The typed Store layer: one place that turns documents into bytes and
//! platform `io::Error`s into [`BuzzError`]s.
//!
//! Nothing above this module hand-rolls a JSON body, decides a scan page size,
//! or decides how many keys fit in one batched read. That matters for three
//! reasons the platform hands us rather than lets us choose:
//!
//! * **Batched calls have hard bounds.** One scan page is clamped to
//!   [`MAX_SCAN_PAGE`] server-side, a batched read of more than
//!   [`MAX_MULTI_GET`] keys is *refused*, and a batched write above
//!   [`MAX_PUT_BATCH`] entries fails without a round trip
//!   (`ferrite-store/src/service.rs:57-66`). [`multi_get_raw`] and [`put_many`]
//!   chunk to those bounds so callers can pass a whole page of keys.
//! * **`put_batch` is a throughput batch, not a transaction.** A window prefix
//!   can commit while the call reports an error (`cluster_node.rs:4225-4235`), so
//!   every batched write here must be value-idempotent; correctness never rides
//!   on "all or nothing".
//! * **Conditional writes have an ambiguous-retry contract.** A transport failure
//!   after the server applied a `put_if_version` makes the retry observe its own
//!   effect and report a mismatch whose `current` is the caller's own new version
//!   (`cluster_node.rs:4459-4466`), so a losing CAS must always re-read rather
//!   than conclude someone else won.
//!
//! Value encoding is JSON throughout ([`encode_doc`]): self-describing, and the
//! same shape the api face already speaks, so a document never needs a second
//! representation.

use ferrite_app::client::StoreClusterClient;
use ferrite_app::serde::de::DeserializeOwned;
use ferrite_app::serde::Serialize;
use ferrite_app::serde_json;

use crate::relay::{BuzzError, Result};

/// Store binding holding events, indexes, memberships, presence, sessions.
pub const EVENTS: &str = "events";
/// APE binding holding admission sagas, the audit chain, and every guard.
pub const GUARDS: &str = "guards";
/// Topics binding holding the delivery feed and the changefeed.
pub const FEED: &str = "feed";
/// Blob binding holding media objects and chunks (public content route).
pub const MEDIA: &str = "media";
/// Store binding holding operator configuration and relay metadata.
pub const CONFIG: &str = "config";
/// Queue binding the api route delivers requests on.
pub const REQUESTS: &str = "requests";
/// Queue binding replies are published to.
pub const REPLIES: &str = "replies";
/// Queue binding the stream route delivers `WS_OPEN`/`STREAM_*` envelopes on.
pub const STREAM_REQUESTS: &str = "stream-requests";
/// Queue binding the **WebSocket** route delivers `WS_OPEN` / `WS_MESSAGE` /
/// `WS_CLOSE` envelopes on. Separate from [`STREAM_REQUESTS`] because the
/// gateway binds one queue per route, and the two faces speak different
/// protocols (NIP-01 frames here, SSE session opens there).
pub const WS_REQUESTS: &str = "ws-requests";
/// Blob binding holding the web UI assets (public content route).
pub const WEB_ASSETS: &str = "web-assets";

/// Store binding holding the agent runtime's memory.
///
/// Declared since the turn pipeline was wired and never read back by the relay,
/// which is why a client could only refuse `get_agent_memory`.
pub const AGENT_MEMORY: &str = "agent-memory";

/// The egress handle link previews are fetched through.
///
/// Its OWN handle rather than the provider one, so the two capabilities stay
/// separable: a deployment can serve previews without granting model access, or
/// the reverse, and an operator reading the spec sees exactly which destinations
/// each is scoped to.
pub const LINK_PREVIEW_EGRESS: &str = "link-preview-egress";

/// The egress handles third-party MCP connectors are reached through.
///
/// Their own handles, scoped to the catalog's hosts, so granting an agent access
/// to Linear does not also grant it a model provider. An MCP endpoint absent from
/// the spec cannot be dialled at all — which is the platform's SSRF defence and
/// the reason [`crate::mcp::catalog`] is a curated list rather than a URL field.
///
/// There is more than one because a capability carries at most sixteen key
/// prefixes and an egress declaration's destinations become them verbatim, so the
/// catalog does not fit in a single grant (`catalog::MAX_DESTINATIONS_PER_HANDLE`).
/// Re-exported FROM the catalog rather than spelled again here: two literals for
/// one manifest binding is a typo away from a surface that advertises itself and
/// then cannot dial.
pub const MCP_EGRESS_CLOUDFLARE: &str = crate::mcp::catalog::EGRESS_CLOUDFLARE;

/// The egress handle the catalog's non-Cloudflare connectors ride.
pub const MCP_EGRESS_PARTNERS: &str = crate::mcp::catalog::EGRESS_PARTNERS;

/// Upper bound on one prefix-scan page; the server clamps larger requests
/// (`ferrite-store/src/service.rs:57`).
pub const MAX_SCAN_PAGE: u32 = 1024;

/// Upper bound on keys in one batched read; larger requests are *refused*
/// (`ferrite-store/src/service.rs:60`).
pub const MAX_MULTI_GET: usize = 256;

/// Upper bound on entries in one batched write; larger requests are *refused*
/// (`ferrite-store/src/service.rs:63`).
pub const MAX_PUT_BATCH: usize = 256;

/// One page of a prefix scan: `(key, value, version)` triplets in ascending key
/// order, plus the platform's **over-approximate** more-pages flag — `true` does
/// not promise another row, so a pager must tolerate an empty follow-up page.
pub type ScanPage = (Vec<(Vec<u8>, Vec<u8>, u64)>, bool);

/// Serialize a document to its stored bytes.
pub fn encode_doc<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| BuzzError::Internal(format!("store encode: {e}")))
}

/// Deserialize a stored document, naming the key in the error.
///
/// A decode failure is an internal fault (something wrote a shape we cannot
/// read), never a client error — the key is included because that is the only
/// way to find the offending row again.
pub fn decode_doc<T: DeserializeOwned>(key: &[u8], bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes)
        .map_err(|e| BuzzError::Internal(format!("store decode {}: {e}", label(key))))
}

/// A key rendered for an error message. Keys are ASCII by construction
/// ([`crate::relay::keys`]), and the length bound keeps one bad row from producing an
/// unbounded log line.
fn label(key: &[u8]) -> String {
    const MAX: usize = 160;
    let text = String::from_utf8_lossy(key);
    if text.len() <= MAX {
        return text.into_owned();
    }
    let mut out: String = text.chars().take(MAX).collect();
    out.push('…');
    out
}

fn storage_err(op: &str, key: &[u8], e: &std::io::Error) -> BuzzError {
    BuzzError::Storage(format!("store {op} {}: {e}", label(key)))
}

/// Read raw bytes at `key`.
pub async fn get_raw(store: &mut StoreClusterClient, key: &[u8]) -> Result<Option<Vec<u8>>> {
    store
        .get(key)
        .await
        .map_err(|e| storage_err("get", key, &e))
}

/// Read raw bytes and the row's version, for a later conditional write.
pub async fn get_raw_versioned(
    store: &mut StoreClusterClient,
    key: &[u8],
) -> Result<Option<(Vec<u8>, u64)>> {
    store
        .get_versioned(key)
        .await
        .map_err(|e| storage_err("get_versioned", key, &e))
}

/// Read and decode the document at `key`. Absence is `Ok(None)`, not an error.
pub async fn get_doc<T: DeserializeOwned>(
    store: &mut StoreClusterClient,
    key: &[u8],
) -> Result<Option<T>> {
    match get_raw(store, key).await? {
        Some(bytes) => Ok(Some(decode_doc(key, &bytes)?)),
        None => Ok(None),
    }
}

/// Read and decode the document at `key` together with its version.
///
/// This is the read half of every read-modify-write in the app: the version goes
/// straight back into [`put_doc_if_version`], so a concurrent writer is detected
/// instead of overwritten.
pub async fn get_doc_versioned<T: DeserializeOwned>(
    store: &mut StoreClusterClient,
    key: &[u8],
) -> Result<Option<(T, u64)>> {
    match get_raw_versioned(store, key).await? {
        Some((bytes, version)) => Ok(Some((decode_doc(key, &bytes)?, version))),
        None => Ok(None),
    }
}

/// Write raw bytes at `key`, returning the new version.
pub async fn put_raw(store: &mut StoreClusterClient, key: &[u8], value: &[u8]) -> Result<u64> {
    store
        .put(key, value)
        .await
        .map_err(|e| storage_err("put", key, &e))
}

/// Encode and write a document, returning the new version.
pub async fn put_doc<T: Serialize>(
    store: &mut StoreClusterClient,
    key: &[u8],
    value: &T,
) -> Result<u64> {
    let bytes = encode_doc(value)?;
    put_raw(store, key, &bytes).await
}

/// Conditional write: apply only if the row is still at `expected`
/// (`None` means create-if-absent).
///
/// The outer `Result` is transport/encoding failure; the inner one is the
/// *outcome*: `Ok(version)` when applied, `Err(current)` when the row had moved
/// (`None` inside means the row is now absent). Callers must re-read on
/// `Err(_)` rather than assume another writer won — a lost response to an applied
/// write reports the caller's own new version as the mismatch.
pub async fn put_doc_if_version<T: Serialize>(
    store: &mut StoreClusterClient,
    key: &[u8],
    value: &T,
    expected: Option<u64>,
) -> Result<core::result::Result<u64, Option<u64>>> {
    let bytes = encode_doc(value)?;
    store
        .put_if_version(key, &bytes, expected)
        .await
        .map_err(|e| storage_err("put_if_version", key, &e))
}

/// Write a document that self-deletes at `expires_at_ms` (absolute epoch ms;
/// `0` means no expiry).
///
/// This is how presence, rate windows, and replay markers work: expiry is
/// *observable absence* everywhere, across crashes and failovers, so there is no
/// sweeper to run and no TTL to renew. It is also why the app needs no clock of
/// its own for these rows — the deadline is computed from the injected `now_ms`.
pub async fn put_expiring_doc<T: Serialize>(
    store: &mut StoreClusterClient,
    key: &[u8],
    value: &T,
    expires_at_ms: u64,
) -> Result<u64> {
    let bytes = encode_doc(value)?;
    store
        .put_expiring(key, &bytes, expires_at_ms)
        .await
        .map_err(|e| storage_err("put_expiring", key, &e))
}

/// [`put_expiring_doc`] for bytes that are already framed.
///
/// The replay record ([`crate::relay::auth`]) is the caller: it stores a status and a
/// reply body under its own compact framing, not a serde document, because the
/// body is opaque bytes the route already encoded and re-encoding it through
/// serde would copy it a second time for nothing.
pub async fn put_expiring_raw(
    store: &mut StoreClusterClient,
    key: &[u8],
    value: &[u8],
    expires_at_ms: u64,
) -> Result<u64> {
    store
        .put_expiring(key, value, expires_at_ms)
        .await
        .map_err(|e| storage_err("put_expiring", key, &e))
}

/// The lease primitive: a conditional write whose value carries its own expiry.
///
/// `expected = None` with a deadline acquires a lock that self-releases if the
/// holder dies — the next acquirer's create-if-absent simply succeeds once the
/// deadline passes. Same outcome shape as [`put_doc_if_version`].
pub async fn put_expiring_doc_if_version<T: Serialize>(
    store: &mut StoreClusterClient,
    key: &[u8],
    value: &T,
    expected: Option<u64>,
    expires_at_ms: u64,
) -> Result<core::result::Result<u64, Option<u64>>> {
    let bytes = encode_doc(value)?;
    store
        .put_if_version_expiring(key, &bytes, expected, expires_at_ms)
        .await
        .map_err(|e| storage_err("put_if_version_expiring", key, &e))
}

/// Delete `key` (a replicated tombstone; the row reads as absent afterwards).
pub async fn delete_key(store: &mut StoreClusterClient, key: &[u8]) -> Result<u64> {
    store
        .delete(key)
        .await
        .map_err(|e| storage_err("delete", key, &e))
}

/// Atomically add `delta` to the counter at `key`, creating it with
/// `expires_at_ms` when absent — the whole `INCR`+`EXPIRE` pair in one op.
///
/// Returns the counter's new value. This is the one primitive here that is **not
/// idempotent under retry**, so it may only be used where an over-count is
/// acceptable: rate-limit windows, never correctness state.
pub async fn bump_expiring_counter(
    store: &mut StoreClusterClient,
    key: &[u8],
    delta: i64,
    expires_at_ms: u64,
) -> Result<i64> {
    store
        .add_expiring(key, delta, expires_at_ms)
        .await
        .map(|(value, _version)| value)
        .map_err(|e| storage_err("add_expiring", key, &e))
}

/// One ordered page of keys under `prefix`, starting strictly after
/// `start_after`.
///
/// `limit` is clamped into `1..=MAX_SCAN_PAGE`: the server clamps the upper end
/// anyway, and the lower clamp keeps a caller from asking for zero rows in a
/// paging loop that would then never terminate.
pub async fn scan(
    store: &mut StoreClusterClient,
    prefix: &[u8],
    start_after: Option<&[u8]>,
    limit: u32,
) -> Result<ScanPage> {
    let limit = limit.clamp(1, MAX_SCAN_PAGE);
    store
        .scan_prefix(prefix, start_after, limit)
        .await
        .map_err(|e| storage_err("scan_prefix", prefix, &e))
}

/// The batches [`multi_get_raw`] will issue: one per [`MAX_MULTI_GET`] keys.
pub fn read_batches(keys: &[Vec<u8>]) -> core::slice::Chunks<'_, Vec<u8>> {
    keys.chunks(MAX_MULTI_GET)
}

/// The batches [`put_many`] will issue: one per [`MAX_PUT_BATCH`] entries.
pub fn write_batches(
    entries: &[(Vec<u8>, Vec<u8>)],
) -> core::slice::Chunks<'_, (Vec<u8>, Vec<u8>)> {
    entries.chunks(MAX_PUT_BATCH)
}

/// Batched point reads, positional with the input and chunked to the platform
/// bound. A missing key is `None` in its slot; versions are dropped because no
/// read path here conditions on them.
pub async fn multi_get_raw(
    store: &mut StoreClusterClient,
    keys: &[Vec<u8>],
) -> Result<Vec<Option<Vec<u8>>>> {
    let mut out = Vec::with_capacity(keys.len());
    for batch in read_batches(keys) {
        let got = store.multi_get(batch).await.map_err(|e| {
            storage_err(
                "multi_get",
                batch.first().map_or(&[][..], |k| k.as_slice()),
                &e,
            )
        })?;
        // Positional contract: a short reply would silently re-align every later
        // slot with the wrong key, so refuse it instead of guessing.
        if got.len() != batch.len() {
            return Err(BuzzError::Storage(format!(
                "multi_get returned {} results for {} keys",
                got.len(),
                batch.len()
            )));
        }
        out.extend(got.into_iter().map(|(value, _version)| value));
    }
    Ok(out)
}

/// Batched writes, chunked to the platform bound.
///
/// **Not atomic**, in two senses: chunks commit independently, and one chunk can
/// partially commit while reporting an error. Only value-idempotent writes may
/// use this (index keys, projections) — a re-drive must be able to rewrite the
/// identical bytes.
pub async fn put_many(
    store: &mut StoreClusterClient,
    entries: &[(Vec<u8>, Vec<u8>)],
) -> Result<Vec<u64>> {
    let mut versions = Vec::with_capacity(entries.len());
    for batch in write_batches(entries) {
        let refs: Vec<(&[u8], &[u8])> = batch
            .iter()
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
            .collect();
        let got = store.put_batch(&refs).await.map_err(|e| {
            storage_err(
                "put_batch",
                batch.first().map_or(&[][..], |(k, _)| k.as_slice()),
                &e,
            )
        })?;
        versions.extend(got);
    }
    Ok(versions)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn binding_names_match_the_manifest_handles() {
        use ferrite_app::serde_json::{self, Value};

        fn collect_handles(value: &Value, handles: &mut Vec<String>) {
            match value {
                Value::Object(fields) => {
                    if let Some(handle) = fields.get("handle").and_then(Value::as_str) {
                        handles.push(handle.to_string());
                    }
                    fields.values().for_each(|child| collect_handles(child, handles));
                }
                Value::Array(items) => {
                    items.iter().for_each(|child| collect_handles(child, handles));
                }
                _ => {}
            }
        }

        let manifest: Value =
            serde_json::from_str(include_str!("../../ferrite.json")).expect("the manifest parses");
        let mut handles = Vec::new();
        collect_handles(&manifest, &mut handles);
        for binding in [
            EVENTS,
            GUARDS,
            FEED,
            MEDIA,
            CONFIG,
            REQUESTS,
            REPLIES,
            STREAM_REQUESTS,
            WS_REQUESTS,
            WEB_ASSETS,
            AGENT_MEMORY,
            LINK_PREVIEW_EGRESS,
            MCP_EGRESS_CLOUDFLARE,
            MCP_EGRESS_PARTNERS,
        ] {
            assert!(
                handles.iter().any(|handle| handle == binding),
                "{binding} resolves to nothing: ferrite.json declares no such handle"
            );
        }
    }

    #[test]
    fn documents_round_trip_as_json() {
        let doc = vec!["a".to_string(), "b".to_string()];
        let bytes = encode_doc(&doc).unwrap();
        assert_eq!(String::from_utf8(bytes.clone()).unwrap(), r#"["a","b"]"#);
        let back: Vec<String> = decode_doc(b"evt/main/x", &bytes).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn a_decode_failure_names_the_key_and_is_internal() {
        let err = decode_doc::<Vec<String>>(b"evt/main/deadbeef", b"not json").unwrap_err();
        match err {
            // Internal, not a client error: nothing a caller sent can cause it.
            BuzzError::Internal(msg) => assert!(msg.contains("evt/main/deadbeef"), "{msg}"),
            _ => panic!("a decode failure must be an internal fault"),
        }
    }

    #[test]
    fn error_labels_are_bounded_and_never_panic_on_odd_bytes() {
        assert_eq!(label(b"evt/main/x"), "evt/main/x");
        let long = vec![b'k'; 1_000];
        let rendered = label(&long);
        assert!(rendered.chars().count() <= 161, "{}", rendered.len());
        assert!(rendered.ends_with('…'));
        // Invalid UTF-8 is replaced, not rejected: this is an error path and must
        // not become a second failure.
        assert!(!label(&[0xff, 0xfe]).is_empty());
    }

    #[test]
    fn read_batches_respect_the_platform_refusal_bound() {
        let keys: Vec<Vec<u8>> = (0..MAX_MULTI_GET + 1).map(|i| vec![i as u8]).collect();
        let sizes: Vec<usize> = read_batches(&keys).map(<[Vec<u8>]>::len).collect();
        assert_eq!(sizes, vec![MAX_MULTI_GET, 1]);
        assert!(read_batches(&[]).next().is_none());
        let exact: Vec<Vec<u8>> = (0..MAX_MULTI_GET).map(|i| vec![i as u8]).collect();
        assert_eq!(read_batches(&exact).count(), 1);
    }

    #[test]
    fn write_batches_respect_the_platform_refusal_bound() {
        let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..MAX_PUT_BATCH * 2 + 3)
            .map(|i| (vec![i as u8], Vec::new()))
            .collect();
        let sizes: Vec<usize> = write_batches(&entries)
            .map(<[(Vec<u8>, Vec<u8>)]>::len)
            .collect();
        assert_eq!(sizes, vec![MAX_PUT_BATCH, MAX_PUT_BATCH, 3]);
    }

    #[test]
    fn platform_caps_match_the_service_bounds() {
        assert_eq!(MAX_SCAN_PAGE, 1_024);
        assert_eq!(MAX_MULTI_GET, 256);
        assert_eq!(MAX_PUT_BATCH, 256);
    }
}
