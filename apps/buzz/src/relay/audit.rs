//! The tamper-evident audit chain.
//!
//! Every security-relevant action appends an entry whose hash covers the entry's
//! own fields *and* the previous entry's hash, so altering or removing any entry
//! breaks every hash after it. Ported from buzz's `buzz-audit`
//! (`buzz/crates/buzz-audit/src/hash.rs:42-73`), with two deliberate changes:
//!
//! * buzz serialized the timestamp as RFC 3339 truncated to microseconds and
//!   documents a digit-count trap in that encoding. There is no date formatter
//!   here (and no ambient clock), so the preimage uses the injected epoch-ms as
//!   big-endian bytes: same ordering, no formatting ambiguity to get wrong.
//! * buzz serialized `metadata` as canonical JSON with sorted keys. This port
//!   keeps the sorted-key requirement but states it as a type invariant —
//!   [`AuditDetail`] is an ordered map, so a caller cannot accidentally produce
//!   two different preimages for the same logical detail.
//!
//! The chain is per community and appended under a compare-and-swap on its head,
//! which is what replaces buzz's `pg_advisory_lock` single-writer guarantee: two
//! concurrent appends cannot interleave because only one can win the CAS, and the
//! loser re-reads the head and retries.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use ferrite_app::client::ApeClusterClient;
use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json;

use crate::relay::crypto::sha256::Sha256;
use crate::relay::crypto::to_hex;
use crate::relay::event::{EventId, PubKey};
use crate::relay::keys;
use crate::relay::{BuzzError, Result};

/// The genesis predecessor: 32 zero bytes, as in buzz.
pub const GENESIS_PREV: [u8; 32] = [0u8; 32];

/// What happened. The closed set from buzz (`action.rs:8-31`) — a new action is a
/// deliberate protocol addition, not an ad-hoc string, so an auditor can
/// enumerate everything the log can contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", rename_all = "snake_case")]
pub enum AuditAction {
    EventCreated,
    EventDeleted,
    ChannelCreated,
    ChannelUpdated,
    ChannelDeleted,
    MemberAdded,
    MemberRemoved,
    AuthSuccess,
    AuthFailure,
    RateLimitExceeded,
    MediaUploaded,
    /// An operator wrote (or deleted) a config key through the operator route.
    ConfigUpdated,
    /// An operator committed a UI bundle (the assets `finish` step).
    UiBundleCommitted,
}

impl AuditAction {
    /// The stable wire string. These bytes are inside the hash preimage, so they
    /// are frozen: renaming one rewrites history.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EventCreated => "event_created",
            Self::EventDeleted => "event_deleted",
            Self::ChannelCreated => "channel_created",
            Self::ChannelUpdated => "channel_updated",
            Self::ChannelDeleted => "channel_deleted",
            Self::MemberAdded => "member_added",
            Self::MemberRemoved => "member_removed",
            Self::AuthSuccess => "auth_success",
            Self::AuthFailure => "auth_failure",
            Self::RateLimitExceeded => "rate_limit_exceeded",
            Self::MediaUploaded => "media_uploaded",
            Self::ConfigUpdated => "config_updated",
            Self::UiBundleCommitted => "ui_bundle_committed",
        }
    }
}

/// Ordered detail map. `BTreeMap` rather than a free-form JSON object so the
/// canonical serialization inside the preimage is deterministic by construction.
pub type AuditDetail = BTreeMap<String, String>;

/// One entry in a community's chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct AuditEntry {
    /// Dense, starting at 1.
    pub seq: u64,
    pub community: String,
    pub action: AuditAction,
    pub at_ms: u64,
    /// Hex actor pubkey, when there is one. `None` and `Some("")` hash
    /// differently — buzz tags presence explicitly for exactly this reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// What was acted on: an event id, channel id, or blob digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub detail: AuditDetail,
    /// Hex hash of the preceding entry; the genesis entry carries 64 zeros.
    pub prev_hash: String,
    /// Hex hash of this entry.
    pub hash: String,
}

/// The fields that go into a hash, before the hash exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditDraft {
    pub community: String,
    pub action: AuditAction,
    pub at_ms: u64,
    pub actor: Option<PubKey>,
    pub object: Option<String>,
    pub detail: AuditDetail,
}

impl AuditDraft {
    #[must_use]
    pub fn new(community: &str, action: AuditAction, at_ms: u64) -> Self {
        Self {
            community: community.to_string(),
            action,
            at_ms,
            actor: None,
            object: None,
            detail: AuditDetail::new(),
        }
    }

    #[must_use]
    pub fn actor(mut self, pubkey: &PubKey) -> Self {
        self.actor = Some(*pubkey);
        self
    }

    #[must_use]
    pub fn event(mut self, id: &EventId) -> Self {
        self.object = Some(to_hex(id));
        self
    }

    #[must_use]
    pub fn object(mut self, object: impl Into<String>) -> Self {
        self.object = Some(object.into());
        self
    }

    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<String>) -> Self {
        self.detail.insert(key.to_string(), value.into());
        self
    }

    /// Seal the draft at `seq` after `prev_hash`, computing the chain hash.
    pub fn seal(self, seq: u64, prev_hash: &[u8; 32]) -> Result<AuditEntry> {
        let actor_hex = self.actor.as_ref().map(|pk| to_hex(pk));
        let hash = compute_hash(
            seq,
            &self.community,
            self.action,
            self.at_ms,
            actor_hex.as_deref(),
            self.object.as_deref(),
            &self.detail,
            prev_hash,
        )?;
        Ok(AuditEntry {
            seq,
            community: self.community,
            action: self.action,
            at_ms: self.at_ms,
            actor: actor_hex,
            object: self.object,
            detail: self.detail,
            prev_hash: to_hex(prev_hash),
            hash: to_hex(&hash),
        })
    }
}

/// The chain hash over one entry's fields plus its predecessor.
///
/// Field order and the presence tags are part of the format. The community id
/// leads, which binds an entry to its tenant: a row lifted from another
/// community's chain fails verification even at the same sequence number.
#[allow(clippy::too_many_arguments)]
pub fn compute_hash(
    seq: u64,
    community: &str,
    action: AuditAction,
    at_ms: u64,
    actor_hex: Option<&str>,
    object: Option<&str>,
    detail: &AuditDetail,
    prev_hash: &[u8; 32],
) -> Result<[u8; 32]> {
    let mut h = Sha256::new();
    // Length-prefix the variable-length community so that ("ab","c") and
    // ("a","bc") cannot produce the same preimage.
    h.update(&(community.len() as u64).to_be_bytes());
    h.update(community.as_bytes());
    h.update(&seq.to_be_bytes());
    h.update(&at_ms.to_be_bytes());
    h.update(action.as_str().as_bytes());
    h.update(&[0u8]);
    match actor_hex {
        Some(a) => {
            h.update(&[1u8]);
            h.update(&(a.len() as u64).to_be_bytes());
            h.update(a.as_bytes());
        }
        None => h.update(&[0u8]),
    }
    match object {
        Some(o) => {
            h.update(&[1u8]);
            h.update(&(o.len() as u64).to_be_bytes());
            h.update(o.as_bytes());
        }
        None => h.update(&[0u8]),
    }
    let detail_json = serde_json::to_vec(detail)
        .map_err(|e| BuzzError::Internal(format!("audit detail encode: {e}")))?;
    h.update(&(detail_json.len() as u64).to_be_bytes());
    h.update(&detail_json);
    h.update(prev_hash);
    Ok(h.finalize())
}

/// The community's chain head: what the next append compares against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct AuditHead {
    pub seq: u64,
    pub hash: String,
}

impl AuditHead {
    /// The head before any entry exists.
    #[must_use]
    pub fn genesis() -> Self {
        Self {
            seq: 0,
            hash: to_hex(&GENESIS_PREV),
        }
    }

    pub fn hash_bytes(&self) -> Result<[u8; 32]> {
        let raw = crate::relay::crypto::from_hex(&self.hash)
            .ok_or_else(|| BuzzError::Internal("audit head hash is not hex".into()))?;
        raw.try_into()
            .map_err(|_| BuzzError::Internal("audit head hash is not 32 bytes".into()))
    }

    /// Advance to the entry just sealed.
    #[must_use]
    pub fn advanced_to(entry: &AuditEntry) -> Self {
        Self {
            seq: entry.seq,
            hash: entry.hash.clone(),
        }
    }
}

/// Why a chain failed verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainFault {
    /// Sequence numbers are not dense and ascending from the expected start.
    Sequence { expected: u64, found: u64 },
    /// An entry's `prev_hash` does not match its predecessor's `hash`.
    Link { seq: u64 },
    /// An entry's recomputed hash does not match its stored `hash` — the entry
    /// was altered after it was written.
    Tampered { seq: u64 },
    /// An entry belongs to a different community than the range being verified.
    ForeignCommunity { seq: u64 },
}

/// Verify a contiguous range of entries.
///
/// `start_prev` is the hash the first entry must chain from — 32 zeros when
/// verifying from the genesis entry, otherwise the hash of the entry before the
/// range. Returns `Ok(())` only if every link and every hash checks out.
pub fn verify_chain(
    community: &str,
    entries: &[AuditEntry],
    start_prev: &[u8; 32],
) -> core::result::Result<(), ChainFault> {
    // A chain rooted at genesis starts at 1: a prefix cut off and re-rooted at
    // genesis must not verify. A slice resumed from a known hash starts where
    // it says, but never at 0. `None` means the previous entry was u64::MAX,
    // so no further entry can follow.
    let mut next_seq: Option<u64> = Some(if start_prev == &GENESIS_PREV {
        1
    } else {
        entries.first().map_or(1, |e| e.seq.max(1))
    });
    let mut prev = *start_prev;
    for entry in entries {
        if entry.community != community {
            return Err(ChainFault::ForeignCommunity { seq: entry.seq });
        }
        let expected_seq = next_seq.ok_or(ChainFault::Sequence {
            expected: u64::MAX,
            found: entry.seq,
        })?;
        if entry.seq != expected_seq {
            return Err(ChainFault::Sequence {
                expected: expected_seq,
                found: entry.seq,
            });
        }
        if entry.prev_hash != to_hex(&prev) {
            return Err(ChainFault::Link { seq: entry.seq });
        }
        let recomputed = compute_hash(
            entry.seq,
            &entry.community,
            entry.action,
            entry.at_ms,
            entry.actor.as_deref(),
            entry.object.as_deref(),
            &entry.detail,
            &prev,
        )
        .map_err(|_| ChainFault::Tampered { seq: entry.seq })?;
        if to_hex(&recomputed) != entry.hash {
            return Err(ChainFault::Tampered { seq: entry.seq });
        }
        prev = recomputed;
        next_seq = expected_seq.checked_add(1);
    }
    Ok(())
}

/// How hard the chain is being fought over, since this instance started.
///
/// The head is ONE key per community and every accepted event bumps it, so
/// contention is inherent to the design rather than a fault — which is exactly why
/// it needs a number instead of an argument. `contended / attempts` says how hard
/// writers are competing; `exhausted` is the one that must stay at zero, because
/// an exhausted append is an action that happened with nothing in the chain
/// saying so.
static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static CONTENDED: AtomicU64 = AtomicU64::new(0);
static EXHAUSTED: AtomicU64 = AtomicU64::new(0);

/// `(attempts, contended, exhausted)` — for the operator metrics tier.
#[must_use]
pub fn contention() -> (u64, u64, u64) {
    (
        ATTEMPTS.load(Ordering::Relaxed),
        CONTENDED.load(Ordering::Relaxed),
        EXHAUSTED.load(Ordering::Relaxed),
    )
}

/// Rounds a writer will lose WITHOUT the head moving before it gives up.
///
/// The bound that matters is **not** how many writers there are. A loser that sees
/// the head advance has learned two things — the chain is making progress, and
/// another writer won — so trying again is exactly right, however many writers are
/// in the air. A budget tied to the writer count would instead have to be re-tuned
/// every time a deployment adds an instance, and this was measured: at two
/// instances' full in-flight ceiling, a fixed 16-round budget refused 4 of 64
/// valid events.
///
/// What deserves a bound is a writer that keeps losing while the head does NOT
/// move. That is pathological rather than busy.
const STALLED_ROUNDS: u32 = 16;

/// Absolute ceiling on rounds for one append, so contention can never become an
/// unbounded spin.
///
/// Sized well above any realistic writer count — one instance admits
/// `MAX_IN_FLIGHT` (32) requests at once, so this covers many instances competing
/// — and reaching it means something is badly wrong rather than merely busy.
/// `exhausted` on the operator metrics tier is how that becomes visible.
const MAX_ROUNDS: u32 = 512;

/// Append `draft` to `community`'s chain, fighting for the head.
///
/// **The one place the chain is extended.** Both the ingest saga and the
/// privileged routes come through here, because two implementations of how a
/// tamper-evident chain grows would be two chances to disagree about it.
///
/// `marker` is an optional idempotency key, committed in the SAME envelope as the
/// entry and the head bump. The event path passes `auditref/{c}/{id}`, which is
/// what makes saga re-entry safe: a resumed driver sees its own marker and returns
/// the sequence it already got, instead of appending a second line for one event.
/// A caller with no natural key for the action passes `None` and accepts that a
/// re-drive would append again — which for a config write is honest, since two
/// writes ARE two actions.
///
/// **A definitive CAS loss is retried; an ambiguous failure is not.** That
/// distinction is the whole safety argument. When `commit_n_if` reports it
/// compared and refused, nothing was written, so re-reading the head and trying
/// again cannot leave two lines for one action. A TRANSPORT failure says nothing
/// about whether the commit applied, so it propagates instead of being retried,
/// because a retry there is exactly how one action gets recorded twice.
///
/// **The marker is read AFTER the head, every round.** The commit is guarded by
/// the head alone, so the marker check has to be ordered against that guard or
/// it proves nothing. Reading the marker first left this window, which two
/// drivers of one event hit on a loaded machine: B reads the marker (absent), A
/// commits entry, head and marker, B then reads the head A just wrote, and B's
/// guard passes, appending a second line for the same event. With the head read
/// first, either the other driver committed before our head read, and the marker
/// (written in its envelope) is there to see, or it commits after, and the head
/// we are guarding on has moved, so our commit is refused and we look again.
///
/// Each round re-seals the entry: the hash covers the sequence AND the
/// predecessor, so an entry sealed against a head that has since moved is simply
/// not the entry that belongs at the new position.
pub async fn append(
    ape: &mut ApeClusterClient,
    community: &str,
    draft: AuditDraft,
    marker: Option<&[u8]>,
) -> Result<u64> {
    let head_key = keys::audit_head(community);
    // What we believe the head currently holds. `None` means "ask": the first
    // round always reads, and a later round only reads again if a loss did not
    // tell us what the head became.
    let mut believed: Option<Vec<u8>> = None;
    let mut stalled = 0u32;

    for _ in 0..MAX_ROUNDS {
        if stalled >= STALLED_ROUNDS {
            break;
        }
        ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        let head_raw = match believed.take() {
            Some(bytes) => Some(bytes),
            None => ape
                .get(&head_key)
                .await
                .map_err(|e| BuzzError::Storage(format!("audit head read: {e}")))?,
        };
        // After the head, never before it (see above). If this action is
        // already on the chain, by an earlier attempt of ours or by another
        // driver of the same event, we are done.
        if let Some(key) = marker {
            if let Some(existing) = ape
                .get(key)
                .await
                .map_err(|e| BuzzError::Storage(format!("audit marker read: {e}")))?
            {
                return decode_seq(&existing);
            }
        }
        let head = match head_raw.as_deref() {
            Some(bytes) => serde_json::from_slice::<AuditHead>(bytes)
                .map_err(|e| BuzzError::Internal(format!("audit head decode: {e}")))?,
            None => AuditHead::genesis(),
        };
        let seq = head.seq.saturating_add(1);
        let entry = draft.clone().seal(seq, &head.hash_bytes()?)?;
        let entry_bytes = serde_json::to_vec(&entry)
            .map_err(|e| BuzzError::Internal(format!("audit entry encode: {e}")))?;
        let next_head = serde_json::to_vec(&AuditHead::advanced_to(&entry))
            .map_err(|e| BuzzError::Internal(format!("audit head encode: {e}")))?;
        let entry_key = keys::audit_entry(community, seq);
        let seq_bytes = seq.to_string().into_bytes();

        let mut writes: Vec<(&[u8], &[u8])> = Vec::with_capacity(3);
        writes.push((entry_key.as_slice(), entry_bytes.as_slice()));
        writes.push((head_key.as_slice(), next_head.as_slice()));
        if let Some(key) = marker {
            writes.push((key, seq_bytes.as_slice()));
        }

        match ape
            .commit_n_if(&head_key, head_raw.as_deref(), &writes)
            .await
            .map_err(|e| BuzzError::Storage(format!("audit commit: {e}")))?
        {
            Ok(()) => return Ok(seq),
            // The commit's transport retries at least once: a commit that landed but
            // whose reply was lost is retried, and refused because the head is now the
            // one THIS commit wrote. That head seals this exact entry (its hash and
            // sequence), so it is our own write: report it rather than append it again.
            Err(Some(current)) if current == next_head => return Ok(seq),
            Err(current) => {
                // Compared and refused: nothing landed.
                CONTENDED.fetch_add(1, Ordering::Relaxed);
                // Did the chain MOVE? If it did, another writer won and the
                // system is live, so this is a busy chain rather than a stuck
                // one and the loss says nothing about giving up.
                if current.as_deref() == head_raw.as_deref() {
                    stalled = stalled.saturating_add(1);
                } else {
                    stalled = 0;
                }
                // The server just told us what the head is; retry against that
                // instead of paying for another read. (`None` — the guard was
                // reported absent — means read again next round.) The next
                // round's marker read comes after this report, so it is ordered
                // against the head it will guard on.
                believed = current;
            }
        }
    }
    EXHAUSTED.fetch_add(1, Ordering::Relaxed);
    Err(BuzzError::Conflict(
        "audit head contended beyond retry budget".into(),
    ))
}

/// A sequence marker, as stored: ASCII decimal, in the ONE spelling this module
/// writes.
///
/// Deliberately stricter than `str::parse`, which accepts a leading `+` — so
/// `"+17"` would decode as 17 and a marker would have two spellings. This value
/// is what tells a resumed saga "already recorded at N", so it is read exactly as
/// narrowly as it is written: digits only, no sign, no leading zero.
fn decode_seq(bytes: &[u8]) -> Result<u64> {
    let text = core::str::from_utf8(bytes)
        .map_err(|_| BuzzError::Internal("audit seq marker is not utf-8".into()))?;
    let canonical = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && !(text.len() > 1 && text.starts_with('0'));
    if !canonical {
        return Err(BuzzError::Internal(
            "audit seq marker is not canonical decimal".into(),
        ));
    }
    text.parse::<u64>()
        .map_err(|_| BuzzError::Internal("audit seq marker does not fit u64".into()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn pk(b: u8) -> PubKey {
        [b; 32]
    }

    fn chain(community: &str, n: u64) -> Vec<AuditEntry> {
        let mut out = Vec::new();
        let mut prev = GENESIS_PREV;
        for seq in 1..=n {
            let entry = AuditDraft::new(community, AuditAction::EventCreated, 1_000 + seq)
                .actor(&pk(1))
                .event(&[seq as u8; 32])
                .with("kind", "9")
                .seal(seq, &prev)
                .unwrap();
            prev = crate::relay::crypto::from_hex(&entry.hash)
                .unwrap()
                .try_into()
                .unwrap();
            out.push(entry);
        }
        out
    }

    #[test]
    fn wire_strings_are_frozen_forever() {
        // These bytes are inside the hash preimage: changing any of them
        // rewrites history, so this test is a freeze, not documentation. Every
        // variant appears exactly once — adding an action means adding a row.
        let all = [
            (AuditAction::EventCreated, "event_created"),
            (AuditAction::EventDeleted, "event_deleted"),
            (AuditAction::ChannelCreated, "channel_created"),
            (AuditAction::ChannelUpdated, "channel_updated"),
            (AuditAction::ChannelDeleted, "channel_deleted"),
            (AuditAction::MemberAdded, "member_added"),
            (AuditAction::MemberRemoved, "member_removed"),
            (AuditAction::AuthSuccess, "auth_success"),
            (AuditAction::AuthFailure, "auth_failure"),
            (AuditAction::RateLimitExceeded, "rate_limit_exceeded"),
            (AuditAction::MediaUploaded, "media_uploaded"),
            (AuditAction::ConfigUpdated, "config_updated"),
            (AuditAction::UiBundleCommitted, "ui_bundle_committed"),
        ];
        for (action, wire) in all {
            assert_eq!(action.as_str(), wire);
            // The serde form must agree with the preimage form: an entry that
            // round-trips through JSON has to re-hash identically.
            assert_eq!(
                serde_json::to_string(&action).unwrap(),
                format!("\"{wire}\"")
            );
        }
    }

    #[test]
    fn a_well_formed_chain_verifies() {
        let entries = chain("main", 5);
        assert_eq!(verify_chain("main", &entries, &GENESIS_PREV), Ok(()));
    }

    #[test]
    fn genesis_entry_chains_from_zeros() {
        let entries = chain("main", 1);
        assert_eq!(entries[0].prev_hash, to_hex(&GENESIS_PREV));
        assert_eq!(entries[0].seq, 1);
    }

    #[test]
    fn altering_any_field_breaks_the_hash() {
        for mutate in [
            (|e: &mut AuditEntry| e.at_ms += 1) as fn(&mut AuditEntry),
            |e: &mut AuditEntry| e.action = AuditAction::EventDeleted,
            |e: &mut AuditEntry| e.actor = Some(to_hex(&[9u8; 32])),
            |e: &mut AuditEntry| e.object = Some("tampered".into()),
            |e: &mut AuditEntry| {
                e.detail.insert("kind".into(), "1".into());
            },
            |e: &mut AuditEntry| e.actor = None,
        ] {
            let mut entries = chain("main", 3);
            mutate(&mut entries[1]);
            assert_eq!(
                verify_chain("main", &entries, &GENESIS_PREV),
                Err(ChainFault::Tampered { seq: 2 }),
            );
        }
    }

    #[test]
    fn removing_an_entry_breaks_the_sequence() {
        let mut entries = chain("main", 4);
        entries.remove(1);
        match verify_chain("main", &entries, &GENESIS_PREV) {
            Err(ChainFault::Sequence { expected, found }) => {
                assert_eq!((expected, found), (2, 3));
            }
            other => panic!("expected a sequence fault, got {other:?}"),
        }
    }

    #[test]
    fn relinking_an_entry_to_the_wrong_predecessor_is_caught() {
        let mut entries = chain("main", 3);
        entries[2].prev_hash = entries[0].prev_hash.clone();
        assert_eq!(
            verify_chain("main", &entries, &GENESIS_PREV),
            Err(ChainFault::Link { seq: 3 }),
        );
    }

    #[test]
    fn an_entry_from_another_community_cannot_be_spliced_in() {
        // The whole point of leading the preimage with the community: a valid
        // entry from community B is not a valid entry in community A's chain,
        // even at the same sequence number.
        let other = chain("other", 1);
        let mut entries = chain("main", 2);
        entries[0] = other[0].clone();
        assert_eq!(
            verify_chain("main", &entries, &GENESIS_PREV),
            Err(ChainFault::ForeignCommunity { seq: 1 }),
        );
    }

    #[test]
    fn same_logical_entry_in_two_communities_hashes_differently() {
        let a = AuditDraft::new("main", AuditAction::AuthSuccess, 5)
            .actor(&pk(2))
            .seal(1, &GENESIS_PREV)
            .unwrap();
        let b = AuditDraft::new("other", AuditAction::AuthSuccess, 5)
            .actor(&pk(2))
            .seal(1, &GENESIS_PREV)
            .unwrap();
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn absent_and_empty_actor_hash_differently() {
        let absent = AuditDraft::new("main", AuditAction::AuthFailure, 5)
            .seal(1, &GENESIS_PREV)
            .unwrap();
        let empty = {
            let mut d = AuditDraft::new("main", AuditAction::AuthFailure, 5);
            d.object = Some(String::new());
            d.seal(1, &GENESIS_PREV).unwrap()
        };
        assert_ne!(absent.hash, empty.hash);
    }

    #[test]
    fn community_is_length_prefixed_against_ambiguity() {
        let a = compute_hash(
            1,
            "ab",
            AuditAction::EventCreated,
            0,
            None,
            Some("c"),
            &AuditDetail::new(),
            &GENESIS_PREV,
        )
        .unwrap();
        let b = compute_hash(
            1,
            "a",
            AuditAction::EventCreated,
            0,
            None,
            Some("bc"),
            &AuditDetail::new(),
            &GENESIS_PREV,
        )
        .unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn detail_key_order_does_not_change_the_hash() {
        let mut d1 = AuditDetail::new();
        d1.insert("b".into(), "2".into());
        d1.insert("a".into(), "1".into());
        let mut d2 = AuditDetail::new();
        d2.insert("a".into(), "1".into());
        d2.insert("b".into(), "2".into());
        let h1 = compute_hash(
            1,
            "main",
            AuditAction::EventCreated,
            0,
            None,
            None,
            &d1,
            &GENESIS_PREV,
        )
        .unwrap();
        let h2 = compute_hash(
            1,
            "main",
            AuditAction::EventCreated,
            0,
            None,
            None,
            &d2,
            &GENESIS_PREV,
        )
        .unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn head_advances_and_round_trips() {
        let entries = chain("main", 2);
        let head = AuditHead::advanced_to(&entries[1]);
        assert_eq!(head.seq, 2);
        assert_eq!(head.hash, entries[1].hash);
        assert_eq!(to_hex(&head.hash_bytes().unwrap()), entries[1].hash);
        assert_eq!(AuditHead::genesis().seq, 0);
        assert_eq!(AuditHead::genesis().hash_bytes().unwrap(), GENESIS_PREV);
    }

    #[test]
    fn a_verified_suffix_can_start_from_a_known_predecessor() {
        let entries = chain("main", 5);
        let prev: [u8; 32] = crate::relay::crypto::from_hex(&entries[1].hash)
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(verify_chain("main", &entries[2..], &prev), Ok(()));
        // ...and fails from the wrong predecessor.
        assert!(verify_chain("main", &entries[2..], &GENESIS_PREV).is_err());
    }

    #[test]
    fn an_empty_range_is_vacuously_valid() {
        assert_eq!(verify_chain("main", &[], &GENESIS_PREV), Ok(()));
    }

    #[test]
    fn a_suffix_re_rooted_at_genesis_does_not_verify() {
        let mut forged = chain("main", 3)[2].clone();
        forged.prev_hash = to_hex(&GENESIS_PREV);
        forged.hash = to_hex(
            &compute_hash(
                forged.seq,
                &forged.community,
                forged.action,
                forged.at_ms,
                forged.actor.as_deref(),
                forged.object.as_deref(),
                &forged.detail,
                &GENESIS_PREV,
            )
            .unwrap(),
        );
        assert_eq!(
            verify_chain("main", &[forged], &GENESIS_PREV),
            Err(ChainFault::Sequence { expected: 1, found: 3 })
        );
    }

    #[test]
    fn audit_seq_markers_decode_and_reject_garbage() {
        assert_eq!(decode_seq(b"17").unwrap(), 17);
        assert_eq!(decode_seq(b"0").unwrap(), 0);
        assert_eq!(
            decode_seq(u64::MAX.to_string().as_bytes()).unwrap(),
            u64::MAX
        );
        assert!(decode_seq(b"").is_err());
        assert!(decode_seq(b"seventeen").is_err());
        assert!(decode_seq(&[0xff, 0xfe]).is_err());
        // One spelling only: a sign, padding, or a leading zero is a second way
        // to write the same number, and `str::parse` would have taken the first
        // two of these.
        assert!(decode_seq(b"+17").is_err());
        assert!(decode_seq(b"-17").is_err());
        assert!(decode_seq(b" 17").is_err());
        assert!(decode_seq(b"17 ").is_err());
        assert!(decode_seq(b"017").is_err());
        // Past u64: refused rather than wrapped.
        assert!(decode_seq(b"18446744073709551616").is_err());
        // Every sequence this module writes round-trips.
        for seq in [1_u64, 9, 10, 99, 100, 1_000_000, u64::MAX - 1] {
            assert_eq!(decode_seq(seq.to_string().as_bytes()).unwrap(), seq);
        }
    }
}
