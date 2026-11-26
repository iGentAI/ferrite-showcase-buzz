//! Per-agent versioned object workspace over Blob (content) + Store (version
//! counter + path registry): the monotone-head guarded CAS, scan-prefix
//! pagination, and scope isolation.

use ferrite_app::client::{BlobClusterClient, StoreClusterClient};

use crate::agents::error::{validate_field, AgentError, Result};

/// Maximum bytes of a workspace path.
pub const MAX_PATH_BYTES: usize = 1024;
/// Maximum bytes of one workspace object's content.
pub const MAX_WS_CONTENT_BYTES: usize = 8 * 1024 * 1024;
/// Bounded CAS rounds for the monotone head publication.
const HEAD_CAS_ROUNDS: usize = 64;
/// Bounded create attempts when claiming the next change-log sequence.
const SYNC_APPEND_ROUNDS: usize = 64;

fn vcounter_key(scope: &str, path: &str) -> Vec<u8> {
    format!("wsv\u{1f}{scope}\u{1f}{path}").into_bytes()
}

fn content_key(scope: &str, path: &str, version: u64) -> Vec<u8> {
    format!("wsc\u{1f}{scope}\u{1f}{path}\u{1f}{version:020}").into_bytes()
}

/// Index record of one drawn version. Written PENDING (`VERSIONREG_PENDING`)
/// before the version's content goes to the blob plane and marked committed
/// (empty value) after. A committed entry lets `version_numbers` skip the blob
/// read for that version; every other drawn number is confirmed against the
/// blob (blobs cannot be listed), so the index is a shortcut, not the
/// authority.
fn versionreg_key(scope: &str, path: &str, version: u64) -> Vec<u8> {
    format!("wsr\u{1f}{scope}\u{1f}{path}\u{1f}{version:020}").into_bytes()
}

const VERSIONREG_PENDING: &[u8] = b"p";

fn pathreg_key(scope: &str, path: &str) -> Vec<u8> {
    format!("wsp\u{1f}{scope}\u{1f}{path}").into_bytes()
}

fn pathreg_prefix(scope: &str) -> Vec<u8> {
    format!("wsp\u{1f}{scope}\u{1f}").into_bytes()
}

/// The per-scope CHANGE-SEQUENCE key: a monotone counter drawn the same way
/// the per-path version counter is, so a workspace has one ordering every
/// writer agrees on (the `sync` profile).
fn syncseq_key(scope: &str) -> Vec<u8> {
    format!("wss\u{1f}{scope}").into_bytes()
}

/// One recorded change, keyed by its sequence so a catchup is a bounded scan
/// rather than a read-modify-write of a growing document.
fn syncchg_key(scope: &str, sequence: u64) -> Vec<u8> {
    format!("wsl\u{1f}{scope}\u{1f}{sequence:020}").into_bytes()
}

fn syncchg_prefix(scope: &str) -> Vec<u8> {
    format!("wsl\u{1f}{scope}\u{1f}").into_bytes()
}

/// A published head whose change record may not be in the log yet: written
/// BEFORE the head is published and removed after the change is appended, so a
/// failed or interrupted append is never silently lost. `sync_head` and
/// `sync_changes` append the change for every marker they find (the entry
/// reports the path's current head state, so a repeat is harmless) and then
/// remove it. Value: the change record (`{kind}\u{1f}{path}`).
fn unlogged_key(scope: &str, path: &str, version: u64) -> Vec<u8> {
    format!("wsu\u{1f}{scope}\u{1f}{path}\u{1f}{version:020}").into_bytes()
}

fn unlogged_prefix(scope: &str) -> Vec<u8> {
    format!("wsu\u{1f}{scope}\u{1f}").into_bytes()
}

/// How a head publication ended (see [`Workspace::publish_head`]).
enum HeadOutcome {
    /// This version is the head now.
    Published,
    /// A newer version already was: this one is kept by number only.
    Superseded,
    /// The conditional write's expectation failed; the current head.
    Lost(u64),
}

/// One entry of a workspace's change log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncChange {
    pub sequence: u64,
    /// `write` | `delete` | `mkdir` | `metadata` — the SDK's `SyncChangeKind`.
    /// This face records `write` and `delete`; the others exist in the
    /// vocabulary and are carried through rather than invented.
    pub change_kind: String,
    pub path: String,
}

/// The content of a deletion version. The workspace is append-only, so a
/// delete is a new version of the path; [`Workspace`] alone reads it back as
/// absent, so every reader (the file tools, `apply_patch`, history) agrees
/// that a deleted path is gone.
const TOMBSTONE: &[u8] = "\u{0}__ferrite_apply_patch_deleted__\u{0}".as_bytes();

/// Appended to a path's head registry entry (after the eight version bytes)
/// when that head is a deletion, so [`Workspace::list`] leaves the path out
/// without reading its content. The version comparison reads only the first
/// eight bytes, so the marker never affects which head wins.
const DELETED_MARK: u8 = 1;

fn read_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    let n = b.len().min(8);
    a[..n].copy_from_slice(&b[..n]);
    u64::from_le_bytes(a)
}

/// Decode a counter maintained by the store's `add`: signed decimal text.
fn parse_counter(b: &[u8]) -> Result<u64> {
    std::str::from_utf8(b)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .map(|n| n.max(0) as u64)
        .ok_or_else(|| AgentError::RecordInvalid {
            reason: "workspace change counter is not a decimal integer".into(),
        })
}

/// Validate a workspace path with the same rules as `apply_patch`: paths are
/// workspace-relative, bounded, and cannot traverse above the workspace.
fn validate_path(path: &str) -> Result<()> {
    crate::agents::apply_patch::validate_path(path)
        .map_err(|reason| AgentError::RecordInvalid { reason })
}

/// A versioned object workspace over a Blob client (content) and a Store client
/// (the monotone version counter + the path registry).
pub struct Workspace {
    blob: BlobClusterClient,
    store: StoreClusterClient,
    /// Test-only fault injection: the named step of the next write fails.
    #[cfg(test)]
    fail_at: Option<&'static str>,
}

impl Workspace {
    pub fn new(blob: BlobClusterClient, store: StoreClusterClient) -> Self {
        Self {
            blob,
            store,
            #[cfg(test)]
            fail_at: None,
        }
    }

    /// A step of a write that a test can make fail (see `fail_at`).
    fn inject(&self, point: &'static str) -> Result<()> {
        #[cfg(test)]
        if self.fail_at == Some(point) {
            return Err(AgentError::Plane {
                message: format!("injected fault at {point}"),
            });
        }
        let _ = point;
        Ok(())
    }

    /// Write a new version of `path` under `scope`; returns the new version
    /// (monotone from 1). Prior versions stay readable. The version is drawn
    /// atomically from a Store counter; the head registry advances with a
    /// guarded monotone CAS (only ever to a larger version).
    pub async fn write(&mut self, scope: &str, path: &str, content: &[u8]) -> Result<u64> {
        self.put_version(scope, path, content, false).await
    }

    /// Write a new version of `path` ONLY while its head is still `expected`
    /// (`0` = the path has no versions): the conditional half of a
    /// read-modify-write such as `edit_file`. `Ok(Err(current))` when another
    /// write moved the head first; nothing becomes the head then, so two
    /// concurrent edits can never silently erase each other.
    pub async fn write_if_head(
        &mut self,
        scope: &str,
        path: &str,
        content: &[u8],
        expected: u64,
    ) -> Result<std::result::Result<u64, u64>> {
        validate_field("scope", scope)?;
        validate_path(path)?;
        // Checked before a version number is drawn, so the ordinary conflict
        // leaves nothing behind.
        let head = self.head_version(scope, path).await?.unwrap_or(0);
        if head != expected {
            return Ok(Err(head));
        }
        self.put_version_if(scope, path, content, false, Some(expected)).await
    }

    /// Delete `path` under `scope`: a new version that every reader of this
    /// workspace treats as absent (`read`, `read_version`, `list`), recorded
    /// in the change log as a `delete`. Earlier versions stay readable by
    /// number. Returns the deletion's version.
    pub async fn delete(&mut self, scope: &str, path: &str) -> Result<u64> {
        self.put_version(scope, path, TOMBSTONE, true).await
    }

    async fn put_version(
        &mut self,
        scope: &str,
        path: &str,
        content: &[u8],
        deletion: bool,
    ) -> Result<u64> {
        match self.put_version_if(scope, path, content, deletion, None).await? {
            Ok(version) => Ok(version),
            // Unreachable without an expectation; reported as a conflict.
            Err(_) => Err(AgentError::Conflict),
        }
    }

    /// [`put_version`](Self::put_version), optionally conditional on the head
    /// still being `expected` when the head advances (`Ok(Err(current))` if
    /// not). A version drawn by a write that then loses its condition stays
    /// readable by number, like any superseded version, but never becomes the
    /// head.
    async fn put_version_if(
        &mut self,
        scope: &str,
        path: &str,
        content: &[u8],
        deletion: bool,
        expected: Option<u64>,
    ) -> Result<std::result::Result<u64, u64>> {
        validate_field("scope", scope)?;
        validate_path(path)?;
        if content.len() > MAX_WS_CONTENT_BYTES {
            return Err(AgentError::RecordInvalid {
                reason: "content too large".into(),
            });
        }
        // The deletion marker is reserved: an ordinary write of exactly those
        // bytes would read back as absent while still listed as live.
        if !deletion && content == TOMBSTONE {
            return Err(AgentError::RecordInvalid {
                reason: "that exact content is reserved by the workspace".into(),
            });
        }
        // Indexed BEFORE the content is written: a version whose content lands
        // always has an entry, whatever fails after. Failing here writes
        // nothing readable.
        let (v, _) = self.store.add(&vcounter_key(scope, path), 1).await?;
        let version = v as u64;
        let reg = versionreg_key(scope, path, version);
        self.inject("index_pending")?;
        self.store.put(&reg, VERSIONREG_PENDING).await?;
        self.inject("blob")?;
        self.blob
            .put(&content_key(scope, path, version), content)
            .await?;
        // Best effort: an entry left pending is confirmed against the blob by
        // `version_numbers`, so a failure here loses nothing.
        if self.inject("index_commit").is_ok() {
            let _ = self.store.put(&reg, b"").await;
        }
        // The change is marked unlogged BEFORE the head can publish it, so a
        // head the log never heard of always leaves a marker to reconcile.
        let record = format!("{}\u{1f}{path}", if deletion { "delete" } else { "write" });
        let unlogged = unlogged_key(scope, path, version);
        self.inject("unlogged_mark")?;
        self.store.put(&unlogged, record.as_bytes()).await?;
        // Whether THIS version became the head. A version that a later one
        // already superseded is kept by number but is never reported as a
        // change: a delayed delete must not tell a syncing client that a live
        // path was deleted.
        match self.publish_head(scope, path, version, deletion, expected).await {
            Ok(HeadOutcome::Published) => {}
            Ok(other) => {
                // Definitely not published (superseded, or lost its
                // condition): nothing for the log. Best effort — a marker
                // left behind is skipped while its version is above the head
                // and, once a later head passes it, logs that head's state.
                let _ = self.store.delete(&unlogged).await;
                return Ok(match other {
                    HeadOutcome::Lost(cur) => Err(cur),
                    _ => Ok(version),
                });
            }
            // The outcome is unknown: a storage error can hide a CAS that
            // committed. The marker stays, so a head that did publish is
            // still logged by the next sync read.
            Err(e) => return Err(e),
        }
        // Record the change in the workspace's own log, so the `sync` profile
        // reports what actually happened rather than only what one face did.
        //
        // The append lives HERE, not in the protocol handler, because both
        // writers reach the workspace through this method: the ws face's
        // `workspace/write` and the agent's own `write_file`/`apply_patch`
        // tools. Logging at the handler would report a workspace the agent had
        // silently moved on from — the asymmetry a replicating client would
        // eventually notice as lost data.
        //
        // Best effort for the CALLER: a failed append never fails the write.
        // The content is already durable and the head published, and the
        // marker written above makes the next `sync_head`/`sync_changes`
        // append the change, so it is delayed, never lost.
        if self.append_change(scope, &record).await.is_ok() && self.inject("unlogged_clear").is_ok() {
            let _ = self.store.delete(&unlogged).await;
        }
        Ok(Ok(version))
    }

    /// Advance `path`'s head to `version` by a guarded monotone CAS.
    async fn publish_head(
        &mut self,
        scope: &str,
        path: &str,
        version: u64,
        deletion: bool,
        expected: Option<u64>,
    ) -> Result<HeadOutcome> {
        let head_key = pathreg_key(scope, path);
        for _ in 0..HEAD_CAS_ROUNDS {
            let (cur, cur_ver) = match self.store.get_versioned(&head_key).await? {
                Some((b, sv)) => (read_u64(&b), Some(sv)),
                None => (0, None),
            };
            if let Some(expected) = expected {
                if cur != expected {
                    return Ok(HeadOutcome::Lost(cur));
                }
            }
            if version <= cur {
                return Ok(HeadOutcome::Superseded);
            }
            self.inject("head")?;
            let mut head = version.to_le_bytes().to_vec();
            if deletion {
                head.push(DELETED_MARK);
            }
            if self
                .store
                .put_if_version(&head_key, &head, cur_ver)
                .await?
                .is_ok()
            {
                return Ok(HeadOutcome::Published);
            }
        }
        Err(AgentError::Conflict)
    }

    /// Append the change for every head the log has not recorded (see
    /// [`unlogged_key`]), then drop its marker. A marker whose append fails is
    /// kept for the next read. A marker whose version is above the path's
    /// head is left alone: its writer has not published (it may still be
    /// running, and will log its own change), so there is nothing to log yet.
    ///
    /// The kind a log record carries is a HINT, never what a client is told:
    /// [`Self::sync_changes`], the log's only reader, reports every `write` or
    /// `delete` entry with the path's current head state. Appending a record
    /// and clearing a marker cannot be one atomic step, so writers and
    /// reconcilers can append in any order; what the log must guarantee is
    /// only that an entry for the path follows every head publication, and
    /// with the kind derived at read time, the last entry a client sees for a
    /// path always matches the workspace whatever order they landed in.
    async fn reconcile_unlogged(&mut self, scope: &str) -> Result<()> {
        let prefix = unlogged_prefix(scope);
        let mut cursor: Option<Vec<u8>> = None;
        let mut found = Vec::new();
        loop {
            let (page, more) = self.store.scan_prefix(&prefix, cursor.as_deref(), 1024).await?;
            for (k, val, _ver) in &page {
                if k.starts_with(&prefix) {
                    found.push((k.clone(), String::from_utf8_lossy(val).into_owned()));
                }
            }
            let last = page.last().map(|(k, _, _)| k.clone());
            if !more {
                break;
            }
            match last {
                Some(k) => cursor = Some(k),
                None => break,
            }
        }
        for (key, record) in found {
            let tail = &key[prefix.len()..];
            let version = tail
                .len()
                .checked_sub(20)
                .and_then(|at| std::str::from_utf8(&tail[at..]).ok())
                .and_then(|s| s.parse::<u64>().ok());
            let path = record.split_once('\u{1f}').map(|(_, p)| p).unwrap_or("").to_string();
            if let (Some(version), Ok(())) = (version, validate_path(&path)) {
                let head = match self.head_version(scope, &path).await? {
                    Some(head) => head,
                    // No head at all: nothing was published to log.
                    None => continue,
                };
                if version > head {
                    continue;
                }
            }
            self.append_change(scope, &record).await?;
            self.store.delete(&key).await?;
        }
        Ok(())
    }

    /// Append one change record (`{kind}\u{1f}{path}`) to this scope's log.
    ///
    /// The sequence is claimed BY creating its record (a create-only write of
    /// the first absent key at the log's end), so a sequence never exists
    /// without its record: a reader can never see N+1 while N is still
    /// unwritten and skip N for good. Each writer takes the first free key,
    /// so the log stays contiguous.
    ///
    /// The search starts at the PROBED end of the log, not at the raw hint, so
    /// a hint that lags any distance (a writer that stalled before advancing
    /// it) costs a longer probe, never a give-up. The bounded rounds below only
    /// ever absorb genuine contention: another writer creating the very key
    /// this one tried. The hint then advances by CAS and only forwards
    /// ([`Self::advance_sync_hint`]).
    async fn append_change(&mut self, scope: &str, record: &str) -> Result<()> {
        self.inject("log_append")?;
        let mut seq = self.probe_sync_head(scope).await?;
        for _ in 0..SYNC_APPEND_ROUNDS {
            seq += 1;
            match self
                .store
                .put_if_version(&syncchg_key(scope, seq), record.as_bytes(), None)
                .await?
            {
                Ok(_) => {
                    // Best effort: a lagging hint only means a longer probe.
                    self.advance_sync_hint(scope, seq).await;
                    return Ok(());
                }
                // Taken by another writer: try the next sequence.
                Err(_) => continue,
            }
        }
        Err(AgentError::Conflict)
    }

    /// Append one change to this scope's log, best effort (tests use it to
    /// replay a delayed writer's append).
    #[cfg(test)]
    async fn record_change(&mut self, scope: &str, path: &str, change_kind: &str) {
        let _ = self.append_change(scope, &format!("{change_kind}\u{1f}{path}")).await;
    }

    /// Move the hint forward to `seq`, never backwards: a CAS loop that stops
    /// as soon as the stored hint is already at or past `seq`. An unconditional
    /// write let a writer that stalled after creating record 1 overwrite a hint
    /// other writers had moved to 66.
    async fn advance_sync_hint(&mut self, scope: &str, seq: u64) {
        let key = syncseq_key(scope);
        for _ in 0..HEAD_CAS_ROUNDS {
            let (cur, version) = match self.store.get_versioned(&key).await {
                Ok(Some((bytes, version))) => match parse_counter(&bytes) {
                    Ok(cur) => (cur, Some(version)),
                    // An unreadable hint is replaced: it is only a hint.
                    Err(_) => (0, Some(version)),
                },
                Ok(None) => (0, None),
                Err(_) => return,
            };
            if cur >= seq {
                return;
            }
            match self
                .store
                .put_if_version(&key, seq.to_string().as_bytes(), version)
                .await
            {
                Ok(Ok(_)) | Err(_) => return,
                Ok(Err(_)) => continue,
            }
        }
    }

    /// Where the search for the log's end starts: the stored hint, never past
    /// a published record.
    async fn sync_hint(&mut self, scope: &str) -> Result<u64> {
        match self.store.get(&syncseq_key(scope)).await? {
            // Decimal text — the store's `add` wrote it that way before, and
            // `record_change` keeps the same encoding.
            Some(b) => parse_counter(&b),
            None => Ok(0),
        }
    }

    /// The scope's current change sequence (`0` before any write): the last
    /// published record, found by probing forward from the hint, after any
    /// change a writer failed to append has been appended.
    pub async fn sync_head(&mut self, scope: &str) -> Result<u64> {
        validate_field("scope", scope)?;
        self.reconcile_unlogged(scope).await?;
        self.probe_sync_head(scope).await
    }

    async fn probe_sync_head(&mut self, scope: &str) -> Result<u64> {
        let mut head = self.sync_hint(scope).await?;
        while self.store.get(&syncchg_key(scope, head + 1)).await?.is_some() {
            head += 1;
        }
        Ok(head)
    }

    /// Changes with `sequence > since`, in order, at most `limit`.
    ///
    /// A `write` or `delete` entry reports the path's CURRENT state, read from
    /// its head registry, not the kind its writer logged. Publishing a head
    /// and appending its record are two steps, so a writer that pauses
    /// between them can append after a newer version's record: a delete that
    /// published, paused, and logged after a newer write would otherwise tell
    /// a syncing client a live path was deleted. Reconciled this way, the last
    /// entry a client sees for a path always matches the workspace.
    ///
    /// A change that has aged out of the log is simply absent — the log is
    /// bounded by the store's own retention, not trimmed here, and a client
    /// that trails badly is expected to fall back to `workspace/list`.
    pub async fn sync_changes(
        &mut self,
        scope: &str,
        since: u64,
        limit: usize,
    ) -> Result<Vec<SyncChange>> {
        validate_field("scope", scope)?;
        // A head whose change a writer failed to append is appended first, so
        // no published change is ever missing from what a client reads.
        self.reconcile_unlogged(scope).await?;
        let prefix = syncchg_prefix(scope);
        let mut out = Vec::new();
        let mut cursor: Option<Vec<u8>> = None;
        loop {
            let (page, more) = self
                .store
                .scan_prefix(&prefix, cursor.as_deref(), 1024)
                .await?;
            for (k, val, _ver) in &page {
                if !k.starts_with(&prefix) {
                    continue;
                }
                // The key's tail is the zero-padded sequence, so the scan
                // returns changes already in order.
                let Ok(sequence) = String::from_utf8_lossy(&k[prefix.len()..]).parse::<u64>()
                else {
                    continue;
                };
                if sequence <= since {
                    continue;
                }
                let raw = String::from_utf8_lossy(val);
                let (change_kind, path) = raw.split_once('\u{1f}').unwrap_or(("write", ""));
                out.push(SyncChange {
                    sequence,
                    change_kind: change_kind.to_string(),
                    path: path.to_string(),
                });
            }
            let last = page.last().map(|(k, _, _)| k.clone());
            if out.len() >= limit || !more {
                break;
            }
            match last {
                Some(k) => cursor = Some(k),
                None => break,
            }
        }
        out.truncate(limit);
        for change in &mut out {
            if change.change_kind != "write" && change.change_kind != "delete" {
                continue;
            }
            if validate_path(&change.path).is_err() {
                continue;
            }
            if let Some(head) = self.store.get(&pathreg_key(scope, &change.path)).await? {
                let deleted = head.get(8) == Some(&DELETED_MARK);
                change.change_kind = if deleted { "delete" } else { "write" }.to_string();
            }
        }
        Ok(out)
    }

    /// Read a specific version's content (`None` if absent, or if that
    /// version is a deletion).
    pub async fn read_version(
        &mut self,
        scope: &str,
        path: &str,
        version: u64,
    ) -> Result<Option<Vec<u8>>> {
        validate_field("scope", scope)?;
        validate_path(path)?;
        Ok(self
            .blob
            .get(&content_key(scope, path, version))
            .await?
            .filter(|content| content.as_slice() != TOMBSTONE))
    }

    /// The current (head) version of `path`, or `None` if it has no versions.
    pub async fn head_version(&mut self, scope: &str, path: &str) -> Result<Option<u64>> {
        validate_field("scope", scope)?;
        validate_path(path)?;
        match self.store.get(&pathreg_key(scope, path)).await? {
            Some(b) => Ok(Some(read_u64(&b))),
            None => Ok(None),
        }
    }

    /// Read the latest content of `path` (`None` if it has no versions or
    /// its latest version is a deletion).
    pub async fn read(&mut self, scope: &str, path: &str) -> Result<Option<Vec<u8>>> {
        match self.head_version(scope, path).await? {
            Some(v) => self.read_version(scope, path, v).await,
            None => Ok(None),
        }
    }

    /// List the paths in `scope` with their head versions (the scope prefix is
    /// stripped), leaving out a path whose head is a deletion. Pages through
    /// the Store scan until drained.
    pub async fn list(&mut self, scope: &str) -> Result<Vec<(String, u64)>> {
        validate_field("scope", scope)?;
        let prefix = pathreg_prefix(scope);
        let mut out = Vec::new();
        let mut cursor: Option<Vec<u8>> = None;
        loop {
            let (page, more) = self
                .store
                .scan_prefix(&prefix, cursor.as_deref(), 1024)
                .await?;
            for (k, val, _ver) in &page {
                if k.starts_with(&prefix) && val.get(8) != Some(&DELETED_MARK) {
                    let name = String::from_utf8_lossy(&k[prefix.len()..]).into_owned();
                    out.push((name, read_u64(val)));
                }
            }
            let last = page.last().map(|(k, _, _)| k.clone());
            if !more {
                break;
            }
            match last {
                Some(k) => cursor = Some(k),
                None => break,
            }
        }
        Ok(out)
    }

    /// The version numbers that exist for `path`, oldest first (empty if none):
    /// the versions whose content was persisted. Not `1..=head`: a failed
    /// content write leaves a hole below the head, and a conditional write
    /// that lost its race leaves a readable version above it (`put_version_if`
    /// promises it stays readable by number).
    pub async fn version_numbers(&mut self, scope: &str, path: &str) -> Result<Vec<u64>> {
        validate_field("scope", scope)?;
        validate_path(path)?;
        let mut prefix = versionreg_key(scope, path, 0);
        prefix.truncate(prefix.len() - 20);
        let mut committed = Vec::new();
        let mut cursor: Option<Vec<u8>> = None;
        loop {
            let (page, more) = self
                .store
                .scan_prefix(&prefix, cursor.as_deref(), 1024)
                .await?;
            for (k, val, _ver) in &page {
                if let Some(rest) = k.strip_prefix(prefix.as_slice()) {
                    if rest.len() == 20 && val.as_slice() != VERSIONREG_PENDING {
                        if let Some(n) = std::str::from_utf8(rest).ok().and_then(|s| s.parse::<u64>().ok()) {
                            committed.push(n);
                        }
                    }
                }
            }
            let last = page.last().map(|(k, _, _)| k.clone());
            if !more {
                break;
            }
            match last {
                Some(k) => cursor = Some(k),
                None => break,
            }
        }
        committed.sort_unstable();
        // Every number the counter handed out is a candidate. A committed
        // index entry proves its content landed; any other number (a pending
        // entry, a failed write, or a version written by a writer that
        // predates the index or is still running an older build) is confirmed
        // against the blob. The index is only a shortcut, never the authority,
        // so no writer can persist content that the listing then omits.
        let drawn = u64::try_from(self.store.add(&vcounter_key(scope, path), 0).await?.0).unwrap_or(0);
        let mut all = Vec::new();
        for n in 1..=drawn {
            if committed.binary_search(&n).is_ok()
                || self.blob.get(&content_key(scope, path, n)).await?.is_some()
            {
                all.push(n);
            }
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review finding (M5-5): `apply_patch` deleted a file by writing a
    /// sentinel version that only its own adapter understood, so the file
    /// read back to `read_file` as the sentinel, stayed in `list_files` and
    /// `search_files`, and reached the change log as a `write`. A delete is
    /// now the workspace's own operation, seen the same way by every reader.
    #[tokio::test]
    async fn a_file_deleted_by_apply_patch_is_gone_for_every_reader() {
        use crate::agents::apply_patch::{run_apply_patch, WorkspaceFsRef};
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let mut ws = Workspace::new(env.blob("ws-content"), env.store("ws-index"));
        ws.write("agent", "keep.txt", b"kept\n").await.unwrap();
        ws.write("agent", "gone.txt", b"doomed\n").await.unwrap();
        let summary = run_apply_patch(
            &mut WorkspaceFsRef::new(&mut ws, "agent"),
            b"*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch",
        )
        .await;
        assert!(String::from_utf8_lossy(&summary).contains("D gone.txt"), "{}", String::from_utf8_lossy(&summary));

        assert_eq!(ws.read("agent", "gone.txt").await.unwrap(), None);
        let listed: Vec<String> = ws.list("agent").await.unwrap().into_iter().map(|(p, _)| p).collect();
        assert_eq!(listed, ["keep.txt"]);
        // History keeps the content that was deleted; the deletion itself
        // reads as absent rather than as a sentinel string.
        assert_eq!(ws.version_numbers("agent", "gone.txt").await.unwrap(), [1, 2]);
        assert_eq!(ws.read_version("agent", "gone.txt", 1).await.unwrap().as_deref(), Some(&b"doomed\n"[..]));
        assert_eq!(ws.read_version("agent", "gone.txt", 2).await.unwrap(), None);
        let changes = ws.sync_changes("agent", 0, 16).await.unwrap();
        let last = changes.last().unwrap();
        assert_eq!((last.change_kind.as_str(), last.path.as_str()), ("delete", "gone.txt"));

        // Writing the path again brings it back.
        ws.write("agent", "gone.txt", b"back\n").await.unwrap();
        assert_eq!(ws.read("agent", "gone.txt").await.unwrap().as_deref(), Some(&b"back\n"[..]));
        let mut listed: Vec<String> = ws.list("agent").await.unwrap().into_iter().map(|(p, _)| p).collect();
        listed.sort();
        assert_eq!(listed, ["gone.txt", "keep.txt"]);
    }

    /// apply_patch refuses a file that is not UTF-8 instead of rewriting every
    /// invalid byte as U+FFFD, and equivalent spellings of a path name one file.
    #[tokio::test]
    async fn apply_patch_refuses_non_utf8_and_normalizes_paths() {
        use crate::agents::apply_patch::{run_apply_patch, WorkspaceFsRef};
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let mut ws = Workspace::new(env.blob("ws-content"), env.store("ws-index"));
        ws.write("agent", "latin1.txt", b"caf\xe9\nline\n").await.unwrap();
        let out = run_apply_patch(
            &mut WorkspaceFsRef::new(&mut ws, "agent"),
            b"*** Begin Patch\n*** Update File: latin1.txt\n@@\n-line\n+LINE\n*** End Patch",
        )
        .await;
        assert!(String::from_utf8_lossy(&out).contains("not UTF-8"), "{}", String::from_utf8_lossy(&out));
        assert_eq!(ws.read("agent", "latin1.txt").await.unwrap().as_deref(), Some(&b"caf\xe9\nline\n"[..]));

        ws.write("agent", "dir/b.txt", b"one\n").await.unwrap();
        let out = run_apply_patch(
            &mut WorkspaceFsRef::new(&mut ws, "agent"),
            b"*** Begin Patch\n*** Update File: ./dir/./b.txt\n@@\n-one\n+two\n*** End Patch",
        )
        .await;
        assert!(String::from_utf8_lossy(&out).starts_with("Success"), "{}", String::from_utf8_lossy(&out));
        assert_eq!(ws.read("agent", "dir/b.txt").await.unwrap().as_deref(), Some(&b"two\n"[..]));
    }

    /// Review finding: version listing reported the counter range, so drawn
    /// numbers whose content never landed were listed as versions.
    #[tokio::test]
    async fn version_numbers_lists_persisted_versions_not_the_counter_range() {
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let mut ws = Workspace::new(env.blob("ws-content"), env.store("ws-index"));
        ws.write("agent", "f.txt", b"one").await.unwrap();
        // A conditional write that loses its condition still persisted its
        // version (readable by number) above the head.
        let lost = ws.put_version_if("agent", "f.txt", b"lost", false, Some(99)).await.unwrap();
        assert_eq!(lost, Err(1));
        assert_eq!(ws.head_version("agent", "f.txt").await.unwrap(), Some(1));
        assert_eq!(ws.read_version("agent", "f.txt", 2).await.unwrap().as_deref(), Some(&b"lost"[..]));
        // A number drawn whose content never landed is a hole.
        ws.store.add(&vcounter_key("agent", "f.txt"), 1).await.unwrap();
        ws.write("agent", "f.txt", b"four").await.unwrap();
        assert_eq!(ws.version_numbers("agent", "f.txt").await.unwrap(), [1, 2, 4]);
        // Review finding: the content landed but its index commit did not
        // (the entry is left pending): still listed, between indexed versions.
        // A pending entry whose content never landed is not.
        ws.write("agent", "f.txt", b"five").await.unwrap();
        ws.store.put(&versionreg_key("agent", "f.txt", 5), VERSIONREG_PENDING).await.unwrap();
        ws.store.put(&versionreg_key("agent", "f.txt", 6), VERSIONREG_PENDING).await.unwrap();
        ws.store.add(&vcounter_key("agent", "f.txt"), 1).await.unwrap();
        ws.write("agent", "f.txt", b"seven").await.unwrap();
        assert_eq!(ws.version_numbers("agent", "f.txt").await.unwrap(), [1, 2, 4, 5, 7]);

        // Review finding: a path's FIRST drawn version whose content never
        // landed is a hole.
        ws.store.add(&vcounter_key("agent", "g.txt"), 1).await.unwrap();
        ws.write("agent", "g.txt", b"two").await.unwrap();
        assert_eq!(ws.version_numbers("agent", "g.txt").await.unwrap(), [2]);
        // Review finding: a writer on an older build (no index entry) that
        // persists a version after indexed writes is still listed.
        let (old, _) = ws.store.add(&vcounter_key("agent", "g.txt"), 1).await.unwrap();
        ws.blob.put(&content_key("agent", "g.txt", old as u64), b"old build").await.unwrap();
        ws.write("agent", "g.txt", b"four").await.unwrap();
        assert_eq!(ws.version_numbers("agent", "g.txt").await.unwrap(), [2, 3, 4]);

        // A path written entirely before the index existed keeps its history;
        // a legacy number whose content never landed (3) is a hole, and a
        // legacy conditional write that lost its condition (4, above the
        // head) is still listed.
        ws.store.add(&vcounter_key("agent", "h.txt"), 4).await.unwrap();
        ws.blob.put(&content_key("agent", "h.txt", 1), b"one").await.unwrap();
        ws.blob.put(&content_key("agent", "h.txt", 2), b"two").await.unwrap();
        ws.blob.put(&content_key("agent", "h.txt", 4), b"lost").await.unwrap();
        ws.store.put(&pathreg_key("agent", "h.txt"), &2u64.to_le_bytes()).await.unwrap();
        assert_eq!(ws.version_numbers("agent", "h.txt").await.unwrap(), [1, 2, 4]);
        // Later indexed writes extend that history.
        ws.write("agent", "h.txt", b"five").await.unwrap();
        assert_eq!(ws.version_numbers("agent", "h.txt").await.unwrap(), [1, 2, 4, 5]);
        assert!(ws.version_numbers("agent", "../escape").await.is_err());
    }

    /// Review finding: a delete whose append failed was reconciled after a
    /// newer write, so the log's last record for the live path said
    /// `delete`; and a head error dropped the marker although the CAS may
    /// have committed. What a client reads is derived from the head, and a
    /// head error keeps the marker.
    #[tokio::test]
    async fn reconciliation_logs_the_current_head_and_a_head_error_keeps_the_marker() {
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let mut ws = Workspace::new(env.blob("ws-content"), env.store("ws-index"));
        ws.write("agent", "f.txt", b"one").await.unwrap();
        ws.fail_at = Some("log_append");
        ws.delete("agent", "f.txt").await.unwrap();
        ws.fail_at = None;
        ws.write("agent", "f.txt", b"three").await.unwrap();
        let changes = ws.sync_changes("agent", 0, 16).await.unwrap();
        // The reconciled delete record landed AFTER the newer write's: the
        // ordering a stale kind would expose. The client still reads `write`.
        let last = changes.last().unwrap();
        assert_eq!((last.path.as_str(), last.change_kind.as_str()), ("f.txt", "write"), "{changes:?}");
        let (markers, _) = ws.store.scan_prefix(&unlogged_prefix("agent"), None, 16).await.unwrap();
        assert!(markers.is_empty());

        // A head error: the marker survives, and once the head is at or past
        // its version it is logged.
        ws.fail_at = Some("head");
        assert!(ws.write("agent", "g.txt", b"x").await.is_err());
        ws.fail_at = None;
        let (markers, _) = ws.store.scan_prefix(&unlogged_prefix("agent"), None, 16).await.unwrap();
        assert_eq!(markers.len(), 1);
        // Simulate the CAS having committed despite the error.
        ws.store.put(&pathreg_key("agent", "g.txt"), &1u64.to_le_bytes()).await.unwrap();
        let changes = ws.sync_changes("agent", 0, 16).await.unwrap();
        assert_eq!(changes.last().map(|c| c.path.as_str()), Some("g.txt"));
        let (markers, _) = ws.store.scan_prefix(&unlogged_prefix("agent"), None, 16).await.unwrap();
        assert!(markers.is_empty());
    }

    /// Coordinator's sweep: fail each step of a write at each position of a
    /// short history, and check the invariants that must survive any single
    /// failure: the version listing is exactly the drawn versions whose
    /// content exists, the head reads as the last successful write, no
    /// unlogged marker survives a sync read, and the log's last word on the
    /// path matches the head.
    #[tokio::test]
    async fn every_single_write_failure_keeps_the_index_and_the_log_consistent() {
        const POINTS: [&str; 7] = [
            "index_pending",
            "blob",
            "index_commit",
            "unlogged_mark",
            "head",
            "log_append",
            "unlogged_clear",
        ];
        // (content, deletion)
        let history: [(&[u8], bool); 4] = [(b"a", false), (b"b", false), (b"", true), (b"d", false)];
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let mut ws = Workspace::new(env.blob("ws-content"), env.store("ws-index"));
        for point in POINTS {
            for k in 0..history.len() {
                let scope = format!("sweep-{point}-{k}");
                let path = "f.txt";
                let mut expected: Option<Vec<u8>> = None;
                let mut published = 0usize;
                for (i, (content, deletion)) in history.iter().enumerate() {
                    ws.fail_at = (i == k).then_some(point);
                    let result = if *deletion {
                        ws.delete(&scope, path).await
                    } else {
                        ws.write(&scope, path, content).await
                    };
                    ws.fail_at = None;
                    if result.is_ok() {
                        published += 1;
                        expected = (!deletion).then(|| content.to_vec());
                    }
                }
                let ctx = format!("fault {point} at write {k}");
                assert_eq!(ws.read(&scope, path).await.unwrap(), expected, "{ctx}");

                let (drawn, _) = ws.store.add(&vcounter_key(&scope, path), 0).await.unwrap();
                let mut existing = Vec::new();
                for v in 1..=drawn as u64 {
                    if ws.blob.get(&content_key(&scope, path, v)).await.unwrap().is_some() {
                        existing.push(v);
                    }
                }
                assert_eq!(ws.version_numbers(&scope, path).await.unwrap(), existing, "{ctx}");

                let changes = ws.sync_changes(&scope, 0, 64).await.unwrap();
                // A marker may outlive a head error (the CAS outcome is
                // unknown) only while its version is above the head.
                let head_now = ws.head_version(&scope, path).await.unwrap().unwrap_or(0);
                let (markers, _) = ws.store.scan_prefix(&unlogged_prefix(&scope), None, 64).await.unwrap();
                for (key, _, _) in &markers {
                    let v: u64 = String::from_utf8_lossy(&key[key.len() - 20..]).parse().unwrap();
                    assert!(v > head_now, "{ctx}: marker {v} at or below head {head_now} remains");
                }
                let ours: Vec<&SyncChange> = changes.iter().filter(|c| c.path == path).collect();
                assert!(ours.len() >= published, "{ctx}: {published} published, {ours:?}");
                match ws.store.get(&pathreg_key(&scope, path)).await.unwrap() {
                    Some(head) => {
                        let kind = if head.get(8) == Some(&DELETED_MARK) { "delete" } else { "write" };
                        assert_eq!(ours.last().map(|c| c.change_kind.as_str()), Some(kind), "{ctx}");
                    }
                    None => assert!(ours.is_empty(), "{ctx}: a path with no head was logged"),
                }
            }
        }
    }

    /// Review finding: a version that lost to a newer head still appended its
    /// change, so a delayed delete reported a live path as deleted.
    #[tokio::test]
    async fn a_superseded_version_is_not_reported_as_a_change() {
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let mut ws = Workspace::new(env.blob("ws-content"), env.store("ws-index"));
        ws.write("agent", "f.txt", b"one").await.unwrap();
        // Simulate a delayed deletion: its version is drawn first, but a
        // newer write publishes the head before the deletion's CAS runs.
        let (late, _) = ws.store.add(&vcounter_key("agent", "f.txt"), 1).await.unwrap();
        ws.write("agent", "f.txt", b"two").await.unwrap();
        let head = ws.head_version("agent", "f.txt").await.unwrap().unwrap();
        assert!(head > late as u64);
        ws.store.add(&vcounter_key("agent", "f.txt"), -2).await.unwrap();
        let before = ws.sync_head("agent").await.unwrap();
        // The delayed deletion lands with its old (smaller) version.
        let _ = ws.put_version("agent", "f.txt", TOMBSTONE, true).await;
        assert_eq!(ws.read("agent", "f.txt").await.unwrap().as_deref(), Some(&b"two"[..]));
        assert_eq!(ws.sync_head("agent").await.unwrap(), before, "no change logged");
        let changes = ws.sync_changes("agent", 0, 16).await.unwrap();
        assert!(changes.iter().all(|c| c.change_kind == "write"), "{changes:?}");
    }

    /// Review finding: the sequence was drawn before its record was written,
    /// so a reader could pass N while N was still missing. A sequence now
    /// exists only with its record, and a stale hint is probed past.
    #[tokio::test]
    async fn every_reported_sequence_has_its_record_and_a_stale_hint_is_probed_past() {
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let mut ws = Workspace::new(env.blob("ws-content"), env.store("ws-index"));
        for i in 0..5 {
            ws.write("agent", &format!("f{i}.txt"), b"x").await.unwrap();
        }
        // A hint left behind (a writer that crashed after its record).
        ws.store.put(&syncseq_key("agent"), b"2").await.unwrap();
        assert_eq!(ws.sync_head("agent").await.unwrap(), 5);
        ws.write("agent", "f5.txt", b"x").await.unwrap();
        let all = ws.sync_changes("agent", 0, 16).await.unwrap();
        let seqs: Vec<u64> = all.iter().map(|c| c.sequence).collect();
        assert_eq!(seqs, (1..=6).collect::<Vec<_>>(), "contiguous, nothing overwritten");
        assert_eq!(all[5].path, "f5.txt");
    }

    /// Review finding: a stalled writer moved the hint BACKWARDS, and with
    /// more than the append budget of records past it every later append gave
    /// up silently. Appends now start from the probed end and the hint only
    /// moves forward.
    #[tokio::test]
    async fn a_regressed_hint_far_behind_the_log_never_stops_logging() {
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let mut ws = Workspace::new(env.blob("ws-content"), env.store("ws-index"));
        let n = SYNC_APPEND_ROUNDS as u64 + 10;
        for i in 0..n {
            ws.write("agent", &format!("f{i}.txt"), b"x").await.unwrap();
        }
        // The stalled writer of record 1 overwrites the hint with 1.
        ws.store.put(&syncseq_key("agent"), b"1").await.unwrap();
        ws.write("agent", "late.txt", b"x").await.unwrap();
        assert_eq!(ws.sync_head("agent").await.unwrap(), n + 1, "the append was logged");
        let tail = ws.sync_changes("agent", n, 4).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].path, "late.txt");
        // The hint moved forward again, and a stale advance cannot pull it back.
        assert_eq!(ws.sync_hint("agent").await.unwrap(), n + 1);
        ws.advance_sync_hint("agent", 1).await;
        assert_eq!(ws.sync_hint("agent").await.unwrap(), n + 1);
    }

    /// Review finding: a writer that published its head and paused before
    /// logging could append after a newer version's record, so a delete logged
    /// last told a syncing client a live path was gone. Every entry now
    /// reports the path's current state.
    #[tokio::test]
    async fn a_change_logged_after_a_newer_one_never_reports_a_stale_state() {
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let mut ws = Workspace::new(env.blob("ws-content"), env.store("ws-index"));
        ws.write("agent", "f.txt", b"one").await.unwrap();
        ws.delete("agent", "f.txt").await.unwrap();
        ws.write("agent", "f.txt", b"two").await.unwrap();
        // The delete's record, delayed past the newer write's record.
        ws.record_change("agent", "f.txt", "delete").await;
        let changes = ws.sync_changes("agent", 0, 16).await.unwrap();
        let last = changes.iter().rev().find(|c| c.path == "f.txt").unwrap();
        assert_eq!(last.change_kind, "write", "{changes:?}");
        assert_eq!(ws.read("agent", "f.txt").await.unwrap().as_deref(), Some(&b"two"[..]));
        // And the converse: a write logged after the deletion that
        // superseded it reports the path as deleted.
        ws.delete("agent", "f.txt").await.unwrap();
        ws.record_change("agent", "f.txt", "write").await;
        let changes = ws.sync_changes("agent", 0, 16).await.unwrap();
        assert_eq!(changes.last().unwrap().change_kind, "delete", "{changes:?}");
    }

    /// Two edits of one version: the second must be refused, never applied over
    /// the first.
    #[tokio::test]
    async fn a_conditional_write_never_erases_a_concurrent_edit() {
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let mut ws = Workspace::new(env.blob("ws-content"), env.store("ws-index"));
        let base = ws.write("agent", "f.txt", b"one\n").await.unwrap();
        // Two edits both read version `base`; the first to commit wins.
        let first = ws.write_if_head("agent", "f.txt", b"one\ntwo\n", base).await.unwrap();
        let won = first.expect("the first edit commits");
        let second = ws.write_if_head("agent", "f.txt", b"one\nthree\n", base).await.unwrap();
        assert_eq!(second, Err(won), "the stale edit is refused with the current head");
        assert_eq!(ws.read("agent", "f.txt").await.unwrap().as_deref(), Some(&b"one\ntwo\n"[..]));
        // A path with no versions has head 0.
        assert!(ws.write_if_head("agent", "new.txt", b"x", 0).await.unwrap().is_ok());
    }

    /// Review finding F230: `sync_head` decoded the store's decimal counter
    /// as little-endian bytes ("1" read as 49), so it disagreed with the
    /// sequences `sync_changes` reports.
    #[tokio::test]
    async fn sync_head_counts_changes_in_the_sequence_space_sync_changes_uses() {
        let env = ferrite_app_test::TestHarness::new()
            .with_blob("ws-content")
            .with_store("ws-index")
            .build()
            .await;
        let mut ws = Workspace::new(env.blob("ws-content"), env.store("ws-index"));
        assert_eq!(ws.sync_head("agent").await.unwrap(), 0);
        for i in 0..12 {
            ws.write("agent", &format!("f{i}.txt"), b"x").await.unwrap();
        }
        let head = ws.sync_head("agent").await.unwrap();
        assert_eq!(head, 12);
        let tail = ws.sync_changes("agent", head - 1, 8).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].sequence, head);
        assert_eq!(tail[0].path, "f11.txt");
        assert!(ws.sync_changes("agent", head, 8).await.unwrap().is_empty());
    }
}
