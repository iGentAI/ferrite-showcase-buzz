//! Moderation: the restriction a member carries and how a moderation event sets it.

use ferrite_app::tracing;
use ferrite_app::client::StoreClusterClient;
use ferrite_app::serde::Deserialize;
use ferrite_app::serde::Serialize;
use crate::relay::error::BuzzError;
use crate::relay::error::Result;
use crate::relay::event::Event;
use crate::relay::event::PubKey;
use crate::relay::keys;
use crate::relay::kind;
use crate::relay::store;
use super::{CAS_RETRIES};
use super::membership::{target_pubkey};

/// A member's community moderation state. Absent = unrestricted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Restriction {
    /// Banned from writing — until [`Self::banned_until`] if set, else
    /// indefinitely.
    #[serde(default)]
    pub banned: bool,
    /// A temporary ban's expiry in epoch-ms (`None` = permanent while
    /// `banned`). Recorded so a moderator's "ban for N days" actually ends —
    /// before this field existed, every ban was silently permanent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banned_until: Option<u64>,
    /// Timed out until this epoch-ms (writes refused while `now < until`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_until: Option<u64>,
}

/// Why a member's write is refused right now, and until when.
///
/// The refusal SENTENCE is derived from the restriction state rather than written
/// by the call site. `buzz-web`'s composer banner
/// (`features/moderation/lib/timeout.ts`) parses the prefix
/// `restricted: you are timed out until ` and reads the epoch-SECONDS that
/// follow, so with one flat string for both sanctions and every expiry, a member
/// cannot be told they are timed out, nor when it ends, nor that they are in
/// fact banned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteBlock {
    /// Banned from the community. `until_ms` is `None` for a permanent ban.
    Banned {
        /// Epoch-ms the ban lapses at, if it is temporary.
        until_ms: Option<u64>,
    },
    /// Timed out until `until_ms` (epoch-ms).
    TimedOut {
        /// Epoch-ms the timeout lapses at.
        until_ms: u64,
    },
}

impl WriteBlock {
    /// The detail half of the refusal — WITHOUT the `restricted: ` token, which
    /// [`BuzzError`]'s `Display` prepends. Passing a pre-prefixed string is what
    /// put `restricted: restricted: …` on the wire.
    ///
    /// The timeout sentence is a CONTRACT with `buzz-web`: the prefix
    /// `you are timed out until ` (after the token) followed by epoch SECONDS is
    /// what `parseTimeoutRejection` matches, and it is the only thing that ever
    /// arms the composer's timeout banner. A ban deliberately reads differently
    /// so it cannot be mistaken for a timeout that will lapse on its own.
    ///
    /// Expiries are rounded UP to the next whole second: the client multiplies
    /// them back by 1000 and stops showing the block when that instant passes, so
    /// truncating would let the client believe the block had lifted while the
    /// relay was still refusing.
    #[must_use]
    pub fn refusal_detail(&self) -> String {
        match *self {
            Self::TimedOut { until_ms } => {
                format!("you are timed out until {}", until_ms.div_ceil(1000))
            }
            Self::Banned {
                until_ms: Some(until_ms),
            } => format!(
                "you are banned from this community until {}",
                until_ms.div_ceil(1000)
            ),
            Self::Banned { until_ms: None } => "you are banned from this community".into(),
        }
    }
}

impl Restriction {
    /// The block that refuses this member's writes at `now_ms`, if any.
    ///
    /// A live ban outranks a live timeout. Telling a banned member "timed out
    /// until X" would promise them access at X that a ban does not return.
    #[must_use]
    pub fn write_block(&self, now_ms: u64) -> Option<WriteBlock> {
        if self.banned && self.banned_until.is_none_or(|until| now_ms < until) {
            return Some(WriteBlock::Banned {
                until_ms: self.banned_until,
            });
        }
        self.timeout_until
            .filter(|until| now_ms < *until)
            .map(|until_ms| WriteBlock::TimedOut { until_ms })
    }

    /// Whether a write from this member must be refused at `now_ms`.
    ///
    /// Defined as "there is a block", so the predicate the write gate consults
    /// and the sentence the member is told can never disagree about whether they
    /// are restricted.
    #[must_use]
    pub fn blocks_write(&self, now_ms: u64) -> bool {
        self.write_block(now_ms).is_some()
    }

    /// Whether a LIVE ban stands against this member at `now_ms`.
    ///
    /// The read-side sanction, and deliberately narrower than
    /// [`Self::blocks_write`]: a ban revokes community access, so the live pumps
    /// evict on it, while a timeout only blocks WRITES and must never cost a
    /// member their socket.
    ///
    /// Time-aware, which the raw `banned` flag is not. The stored document
    /// outlives a temporary ban's expiry until some later command normalizes it
    /// (see `apply_moderation`), so reading the flag directly evicted a member
    /// whose ban had lapsed and who the write path was happily admitting — on any
    /// unrelated tombstone bump, a channel removal included.
    #[must_use]
    pub fn ban_blocks_access(&self, now_ms: u64) -> bool {
        matches!(self.write_block(now_ms), Some(WriteBlock::Banned { .. }))
    }
}

/// Apply a moderation command (9040–9043) to a target's restriction state.
/// Authorization (the actor is a community moderator) is enforced pre-storage;
/// this materializes the decision. The target is the first `p` tag; a ban or a
/// timeout reads its requested expiry from [`moderation_expiry_ms`], a timeout
/// defaulting to one hour when none was asked for.
pub(super) async fn apply_moderation(
    store: &mut StoreClusterClient,
    community: &str,
    event: &Event,
    now_ms: u64,
) -> Result<()> {
    let Some(target) = target_pubkey(event) else {
        return Ok(());
    };
    let key = keys::restriction(community, &target);
    for _ in 0..CAS_RETRIES {
        let current = store::get_doc_versioned::<Restriction>(store, &key).await?;
        let (mut doc, expected) = match current {
            Some((doc, version)) => (doc, Some(version)),
            None => (Restriction::default(), None),
        };
        // An expired temporary ban is normalized away before the new command
        // applies, so a later command does not inherit a stale `banned = true`
        // and the live-session ban check does not evict a member whose ban
        // already lapsed. (`blocks_write` is time-aware regardless; this keeps
        // the DOCUMENT honest for the readers that are not.)
        if doc.banned && doc.banned_until.is_some_and(|until| now_ms >= until) {
            doc.banned = false;
            doc.banned_until = None;
        }
        match event.kind {
            kind::KIND_MODERATION_BAN => {
                doc.banned = true;
                // A requested expiry makes the ban temporary; none = permanent.
                doc.banned_until = moderation_expiry_ms(event);
            }
            kind::KIND_MODERATION_UNBAN => {
                doc.banned = false;
                doc.banned_until = None;
            }
            kind::KIND_MODERATION_TIMEOUT => {
                let until =
                    moderation_expiry_ms(event).unwrap_or_else(|| now_ms.saturating_add(3_600_000));
                doc.timeout_until = Some(until);
            }
            kind::KIND_MODERATION_UNTIMEOUT => doc.timeout_until = None,
            _ => {}
        }
        match store::put_doc_if_version(store, &key, &doc, expected).await? {
            Ok(_) => {
                // §6.3 rule (b): the revocation tombstone commits with the
                // state change, BEFORE the saga's feed append, so a pump
                // catches the change on its next batch even when the feed
                // record never lands (the crash window).
                bump_revocation(store, community, &target, now_ms).await;
                return Ok(());
            }
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict("moderation state contended".into()))
}

/// The expiry a moderation command asked for, in epoch-ms.
///
/// The wire contract is NIP-40: `["expiration", "<epoch-SECONDS>"]` — what the
/// kind declarations promise and what `buzz-web` sends for both a timeout and
/// a temporary ban. `["until", "<epoch-ms>"]` is not a tag any client sends, but
/// it stays accepted as a fallback so an older stored command re-drives to the
/// same state it originally produced. When both are present, the wire contract
/// wins.
pub(super) fn moderation_expiry_ms(event: &Event) -> Option<u64> {
    if let Some(secs) = event
        .first_tag("expiration")
        .and_then(|v| v.parse::<u64>().ok())
    {
        return Some(secs.saturating_mul(1000));
    }
    event.first_tag("until").and_then(|v| v.parse::<u64>().ok())
}

/// Bump `target`'s revocation tombstone (design §6.3 rule b). Best-effort by
/// design: the tombstone is the crash-window BACKSTOP behind the feed-driven
/// invalidation and the cache TTL, so a failed bump degrades to the existing
/// posture instead of wedging the saga that already committed its state.
pub(super) async fn bump_revocation(
    store: &mut StoreClusterClient,
    community: &str,
    target: &PubKey,
    now_ms: u64,
) {
    let key = keys::revoked(community, target);
    if let Err(e) = store::put_raw(store, &key, now_ms.to_string().as_bytes()).await {
        tracing::warn!("revocation tombstone bump failed (TTL backstop holds): {e}");
    }
}

/// Read a member's restriction state (`None` = unrestricted).
pub async fn restriction_of(
    store: &mut StoreClusterClient,
    community: &str,
    who: &PubKey,
) -> Result<Option<Restriction>> {
    let key = keys::restriction(community, who);
    store::get_doc::<Restriction>(store, &key).await
}
