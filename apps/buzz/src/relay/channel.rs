//! Channels — the rooms of a workspace.
//!
//! A channel is the unit of access control: membership is the only gate, and
//! every read and delivery decision resolves to "is this pubkey a live member of
//! this channel". Transliterated from buzz's `channels` / `channel_members`
//! tables (`buzz/migrations/0001_initial_schema.sql:72-148`), with the
//! relational row replaced by a guarded APE document so concurrent mutations
//! serialize through a compare-and-swap instead of a row lock.

use ferrite_app::serde::{Deserialize, Serialize};

use crate::relay::crypto::sha256::sha256;

use crate::relay::crypto::to_hex;
use crate::relay::event::PubKey;

/// The three lenses buzz puts over one event log, plus the DM and workflow
/// forms. Stream is fast chat, Forum is slow long-form, Dm is a closed set of
/// 2..=9 participants, Workflow is an automation's own room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", rename_all = "lowercase")]
pub enum ChannelType {
    Stream,
    Forum,
    Dm,
    Workflow,
}

impl ChannelType {
    /// The wire spelling used in event tags and API payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::Forum => "forum",
            Self::Dm => "dm",
            Self::Workflow => "workflow",
        }
    }

    /// Parse the wire spelling; unknown values are refused rather than defaulted
    /// so a typo cannot silently create a channel of the wrong kind.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stream" => Some(Self::Stream),
            "forum" => Some(Self::Forum),
            "dm" => Some(Self::Dm),
            "workflow" => Some(Self::Workflow),
            _ => None,
        }
    }
}

/// Open channels are discoverable and self-joinable by any workspace member;
/// private channels are invisible to non-members and require an invite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", rename_all = "lowercase")]
pub enum Visibility {
    Open,
    Private,
}

impl Visibility {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Private => "private",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "private" => Some(Self::Private),
            _ => None,
        }
    }

    #[must_use]
    pub const fn is_private(self) -> bool {
        matches!(self, Self::Private)
    }
}

/// Member roles, ordered by authority. `Bot` is deliberately a peer of `Member`
/// rather than a lesser thing: an agent in a channel has a member's affordances
/// and its own audit trail, which is the point of the product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", rename_all = "lowercase")]
pub enum Role {
    Guest,
    Bot,
    Member,
    Admin,
    Owner,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Guest => "guest",
            Self::Bot => "bot",
            Self::Member => "member",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "guest" => Some(Self::Guest),
            "bot" => Some(Self::Bot),
            "member" => Some(Self::Member),
            "admin" => Some(Self::Admin),
            "owner" => Some(Self::Owner),
            _ => None,
        }
    }

    /// Whether this role may add or remove other members, change metadata, or
    /// delete another member's message.
    #[must_use]
    pub const fn is_moderator(self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }
}

/// Who may add members to a channel — buzz's `channel_add_policy`, which lets a
/// user refuse being added to rooms by agents without refusing agents entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", rename_all = "snake_case")]
pub enum AddPolicy {
    Anyone,
    OwnerOnly,
    Nobody,
}

// A manual impl rather than `#[default]` on the variant: upstream Core's code
// gate does not review the `default` helper attribute and refuses it.
impl Default for AddPolicy {
    fn default() -> Self {
        Self::Anyone
    }
}

/// A channel membership row. Removal is a soft delete (`removed_at`) so that
/// re-adding a member is a state transition with history rather than a new row,
/// and so an audit reader can see that someone was once present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Membership {
    /// Hex-encoded member pubkey.
    pub pubkey: String,
    pub role: Role,
    pub joined_at: u64,
    /// Set when the member was removed; `None` means live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_at: Option<u64>,
    /// Hex pubkey of whoever invited them, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invited_by: Option<String>,
    /// Per-user hide for DM channels (buzz's `hidden_at`): the conversation stays
    /// intact but drops out of the member's list until it is spoken in again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_at: Option<u64>,
}

impl Membership {
    #[must_use]
    pub fn new(pubkey: &PubKey, role: Role, now_ms: u64) -> Self {
        Self {
            pubkey: to_hex(pubkey),
            role,
            joined_at: now_ms,
            removed_at: None,
            invited_by: None,
            hidden_at: None,
        }
    }

    /// A membership grants access only while it is live.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.removed_at.is_none()
    }
}

/// The channel document. One guarded APE record per channel; `seq` is the
/// monotone guard every mutation compares against, which is what replaces
/// buzz's `pg_advisory_xact_lock` and transactional role checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Channel {
    /// Channel identifier — a UUID-shaped string minted by the creating client.
    pub id: String,
    pub name: String,
    pub channel_type: ChannelType,
    pub visibility: Visibility,
    /// Monotone mutation counter; the CAS guard. A reader that acts on a channel
    /// document must carry this value into its guarded commit.
    pub seq: u64,
    /// Hex pubkey of the creator.
    pub created_by: String,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// The channel's shared document (buzz's canvas).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<u64>,
    /// Ephemeral channels (huddles) carry a TTL; the reaper archives them once
    /// `ttl_deadline` passes with no new activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_deadline: Option<u64>,
    /// SHA-256 over the sorted participant set — the identity of a DM, so that
    /// opening a DM with the same people twice finds the same room.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participant_hash: Option<String>,
    #[serde(default)]
    pub add_policy: AddPolicy,
    /// Hex pubkeys of the channel's currently-live owners. This is the ONE piece
    /// of membership state that lives on the channel document rather than in a
    /// per-member `mem/` row, and it is here for exactly one reason: the
    /// last-owner guard must be atomic. buzz makes "count the owners, then remove
    /// one" a single Postgres transaction; Ferrite has no cross-key transaction,
    /// and a `mem/` scan on the Store plane cannot be guarded together with the
    /// removal. Keeping the owner set on the channel's own guarded APE document
    /// makes the whole decision one `commit_n_if` compare-and-swap on `seq` — the
    /// role this field's neighbour `seq` was written to play. Per-member reads
    /// (`authorize_channel_write`, `resolve_access`) still use the `mem/` rows and
    /// are unchanged; only owner-count decisions read this.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub owners: std::collections::BTreeSet<String>,
}

impl Channel {
    /// A new channel with its creator as owner. The caller commits this document
    /// and the creator's membership in one guarded envelope, so a channel can
    /// never exist without an owner.
    #[must_use]
    pub fn new(
        id: String,
        name: String,
        channel_type: ChannelType,
        visibility: Visibility,
        created_by: &PubKey,
        now_ms: u64,
    ) -> Self {
        Self {
            id,
            name,
            channel_type,
            visibility,
            seq: 1,
            created_by: to_hex(created_by),
            created_at: now_ms,
            updated_at: now_ms,
            description: None,
            topic: None,
            purpose: None,
            canvas: None,
            archived_at: None,
            deleted_at: None,
            ttl_seconds: None,
            ttl_deadline: None,
            participant_hash: None,
            add_policy: AddPolicy::default(),
            owners: std::iter::once(to_hex(created_by)).collect(),
        }
    }

    /// Writable channels are neither archived nor deleted. Reads of an archived
    /// channel still succeed — history does not vanish — but writes are refused.
    #[must_use]
    pub const fn is_writable(&self) -> bool {
        self.archived_at.is_none() && self.deleted_at.is_none()
    }

    #[must_use]
    pub const fn is_visible(&self) -> bool {
        self.deleted_at.is_none()
    }

    /// Whether a non-member may see this channel exists and read it.
    #[must_use]
    pub const fn is_publicly_readable(&self) -> bool {
        matches!(self.visibility, Visibility::Open) && self.is_visible()
    }
}

/// Minimum / maximum participants in a DM (buzz's `create_dm` bounds).
pub const DM_MIN_PARTICIPANTS: usize = 2;
pub const DM_MAX_PARTICIPANTS: usize = 9;

/// The identity of a DM: SHA-256 over the sorted, de-duplicated raw 32-byte
/// pubkeys, with no separator (buzz's `compute_participant_hash` — safe without a
/// separator only because every element is a fixed 32 bytes). Order- and
/// duplicate-independent; set-sensitive. Returned as 64-hex.
#[must_use]
pub fn dm_participant_hash(pubkeys: &[PubKey]) -> String {
    let mut sorted: Vec<PubKey> = pubkeys.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut concat = Vec::with_capacity(sorted.len() * 32);
    for pk in &sorted {
        concat.extend_from_slice(pk);
    }
    to_hex(&sha256(&concat))
}

/// The number of distinct participants (what the DM bounds are checked against).
#[must_use]
pub fn distinct_participant_count(pubkeys: &[PubKey]) -> usize {
    let mut sorted: Vec<PubKey> = pubkeys.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted.len()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn pk(byte: u8) -> PubKey {
        [byte; 32]
    }

    #[test]
    fn wire_spellings_round_trip() {
        for t in [
            ChannelType::Stream,
            ChannelType::Forum,
            ChannelType::Dm,
            ChannelType::Workflow,
        ] {
            assert_eq!(ChannelType::parse(t.as_str()), Some(t));
        }
        for v in [Visibility::Open, Visibility::Private] {
            assert_eq!(Visibility::parse(v.as_str()), Some(v));
        }
        for r in [
            Role::Guest,
            Role::Bot,
            Role::Member,
            Role::Admin,
            Role::Owner,
        ] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
    }

    #[test]
    fn unknown_wire_values_are_refused_not_defaulted() {
        assert_eq!(ChannelType::parse("Stream"), None);
        assert_eq!(ChannelType::parse(""), None);
        assert_eq!(Visibility::parse("secret"), None);
        assert_eq!(Role::parse("root"), None);
    }

    #[test]
    fn dm_participant_hash_is_order_and_dup_independent_but_set_sensitive() {
        let a = pk(1);
        let b = pk(2);
        let c = pk(3);
        // Order-independent.
        assert_eq!(dm_participant_hash(&[a, b]), dm_participant_hash(&[b, a]));
        // Duplicate-independent.
        assert_eq!(
            dm_participant_hash(&[a, b]),
            dm_participant_hash(&[a, b, a])
        );
        // Set-sensitive.
        assert_ne!(dm_participant_hash(&[a, b]), dm_participant_hash(&[a, c]));
        // 64-hex.
        assert_eq!(dm_participant_hash(&[a, b]).len(), 64);
        // Distinct count ignores dups.
        assert_eq!(distinct_participant_count(&[a, b, a, b]), 2);
    }

    #[test]
    fn only_owner_and_admin_moderate() {
        assert!(Role::Owner.is_moderator());
        assert!(Role::Admin.is_moderator());
        assert!(!Role::Member.is_moderator());
        assert!(!Role::Bot.is_moderator());
        assert!(!Role::Guest.is_moderator());
    }

    #[test]
    fn role_ordering_places_owner_highest() {
        assert!(Role::Owner > Role::Admin);
        assert!(Role::Admin > Role::Member);
        assert!(Role::Member > Role::Bot);
        assert!(Role::Bot > Role::Guest);
    }

    #[test]
    fn removed_membership_is_not_live() {
        let mut m = Membership::new(&pk(1), Role::Member, 1_000);
        assert!(m.is_live());
        m.removed_at = Some(2_000);
        assert!(!m.is_live());
    }

    #[test]
    fn archived_channel_is_readable_but_not_writable() {
        let mut c = Channel::new(
            "chan-1".into(),
            "general".into(),
            ChannelType::Stream,
            Visibility::Open,
            &pk(7),
            1_000,
        );
        assert!(c.is_writable());
        c.archived_at = Some(2_000);
        assert!(!c.is_writable());
        assert!(c.is_visible());
        assert!(c.is_publicly_readable());
    }

    #[test]
    fn deleted_channel_is_invisible() {
        let mut c = Channel::new(
            "chan-2".into(),
            "gone".into(),
            ChannelType::Stream,
            Visibility::Open,
            &pk(8),
            1_000,
        );
        c.deleted_at = Some(5);
        assert!(!c.is_visible());
        assert!(!c.is_publicly_readable());
        assert!(!c.is_writable());
    }

    #[test]
    fn private_channel_is_never_publicly_readable() {
        let c = Channel::new(
            "chan-3".into(),
            "secret".into(),
            ChannelType::Stream,
            Visibility::Private,
            &pk(9),
            1_000,
        );
        assert!(c.visibility.is_private());
        assert!(!c.is_publicly_readable());
    }

    #[test]
    fn new_channel_starts_at_guard_seq_one() {
        let c = Channel::new(
            "chan-4".into(),
            "x".into(),
            ChannelType::Forum,
            Visibility::Open,
            &pk(3),
            42,
        );
        assert_eq!(c.seq, 1);
        assert_eq!(c.created_at, 42);
        assert_eq!(c.updated_at, 42);
        assert_eq!(c.created_by, to_hex(&pk(3)));
    }
}
