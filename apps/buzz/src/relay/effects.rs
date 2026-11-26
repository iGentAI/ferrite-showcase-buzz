//! Materializing workspace state from admitted events.
//!
//! Writes all funnel through `/buzz/api/events`. Some kinds are not just records
//! to store and deliver — they *change workspace state*: a membership command
//! adds or removes a member, a channel-metadata edit renames a room, a reaction
//! toggles a tally. buzz applies those as SQL side effects inside the same
//! transaction that stores the event. Ferrite has no cross-key transaction, so
//! this module applies them as a step of the ingest saga
//! ([`crate::relay::ingest::drive`]), after the event body is stored and before the saga
//! completes.
//!
//! ## Two independent safety properties
//!
//! 1. **Idempotency under saga re-entry.** `drive` can run more than once for one
//!    event (a crash before the completion commit, a redelivery, a concurrent
//!    duplicate), and the effect step runs on every drive of an incomplete saga.
//!    Each effect is therefore idempotent on its own: state effects set state,
//!    and the thread effect bumps its counters only when it creates the event's
//!    own thread row — buzz's "bump only if the row was new". The effect used to
//!    be gated on the event BODY being new instead, which silently dropped the
//!    state change of any saga that tore after the body write.
//!
//! 2. **Correctness under concurrent *distinct* events.** Two different events for
//!    one channel can drive at once, and nothing serializes them. Each
//!    effect that reads-then-writes a document does so as a compare-and-swap on
//!    that document (the APE channel guard's bytes, a `mem/` row's version), and
//!    retries on contention. The last-owner guard in particular is decided from
//!    the channel document's own `owners` set under that CAS, so two racing
//!    owner-removals cannot both believe an owner remains.
//!
//! Authorization is *not* here — it is a pre-storage check
//! ([`crate::relay::ingress`]'s validation) that rejects an unauthorized command before
//! it is ever admitted, exactly as buzz rejects at its validator. By the time an
//! effect runs, the command is authorized; the effect's own CAS re-checks only
//! the invariants that a concurrent event could have changed (the owner count),
//! and a lost race is a logged no-op, never an error that would wedge the saga.

use ferrite_app::client::{ApeClusterClient, StoreClusterClient};

use crate::relay::error::Result;
use crate::relay::event::Event;
use crate::relay::kind::Kind;
use crate::relay::kind;

mod moderation;
mod threads;
mod deletion;
mod channels;
mod reactions;
mod dm;
mod membership;
mod replacement;
mod projections;
mod notices;

pub use moderation::{Restriction, WriteBlock, restriction_of};
use moderation::{apply_moderation};
#[cfg(test)]
use moderation::{moderation_expiry_ms};
pub use threads::{MAX_THREAD_DEPTH, ThreadMeta, refuse_over_deep_reply, thread_ancestry, thread_meta_of, thread_summary};
use threads::{apply_thread};
pub use deletion::{parse_address, retire_address};
use deletion::{apply_deletion};
pub use channels::{authorize_channel_delete, authorize_channel_edit, authorize_membership_command, canonical_channel_name};
use channels::{apply_channel_delete, apply_channel_edit};
pub use reactions::{MAX_REACTION_EMOJI_CHARS, ReactionState, ReactionTally, reaction_is_active_duplicate, reaction_target_and_emoji, read_reactions};
use reactions::{apply_reaction};
pub use dm::{dm_channel_id, dm_participants};
pub(crate) use dm::is_compatible_dm_guard;
use dm::{apply_dm_hide, apply_dm_open};
pub use membership::{authorize_target_role_change, requested_role, target_pubkey};
use membership::{acquire_membership_lock, apply_membership, release_membership_lock};
#[cfg(test)]
use membership::{join_membership_doc, put_user_doc};
pub use replacement::{ReplaceableHead, apply_replacement, purge_event_rows, replacement_d_tag, version_beats};
pub use projections::{ALL_GROUP_PROJECTIONS, MAX_PROJECTION_MEMBERS, ROSTER_TRUNCATED_TAG, emit_group_projections, projections_for, publish_group_projections};
#[cfg(test)]
use projections::{push_metadata_tags, push_truncation_tag};
pub use notices::{MAX_MEMBERSHIP_NOTICES, emit_membership_notices, membership_notice_for, needs_relay_identity};
#[cfg(test)]
use crate::relay::channel::AddPolicy;
#[cfg(test)]
use crate::relay::channel::Channel;
#[cfg(test)]
use crate::relay::channel::Membership;
#[cfg(test)]
use crate::relay::channel::Role;
#[cfg(test)]
use crate::relay::crypto::to_hex;
#[cfg(test)]
use crate::relay::error::BuzzError;
#[cfg(test)]
use crate::relay::event::PubKey;
#[cfg(test)]
use crate::relay::keys;

/// The most times a channel-document CAS is retried under contention before the
/// effect gives up and lets redelivery try again. Contention here is between
/// distinct membership events for one channel, which is rare; a small budget is
/// plenty and bounds the work a single admission can do.
const CAS_RETRIES: usize = 8;

/// Apply the state change an admitted event carries, if any.
///
/// A no-op for the vast majority of kinds (a chat message stores and delivers but
/// changes no materialized state). Only the kinds this module recognizes mutate a
/// channel/membership/thread/reaction document.
pub async fn apply_effects(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    event: &Event,
    channel: Option<&str>,
    now_ms: u64,
) -> Result<()> {
    match event.kind {
        kind::KIND_NIP29_PUT_USER
        | kind::KIND_NIP29_REMOVE_USER
        | kind::KIND_NIP29_JOIN_REQUEST
        | kind::KIND_NIP29_LEAVE_REQUEST => {
            let Some(channel) = channel else {
                // A membership command with no resolved channel is a validation
                // gap, not something to guess at; leave state untouched.
                return Ok(());
            };
            let held = acquire_membership_lock(ape, community, channel, event, now_ms).await?;
            let applied =
                apply_membership(store, ape, community, channel, event, now_ms, &held).await;
            release_membership_lock(ape, community, channel, &held).await;
            applied
        }
        kind::KIND_NIP29_EDIT_METADATA => {
            let Some(channel) = channel else {
                return Ok(());
            };
            apply_channel_edit(ape, community, channel, event, now_ms).await
        }
        kind::KIND_NIP29_DELETE_GROUP => {
            let Some(channel) = channel else {
                return Ok(());
            };
            apply_channel_delete(ape, community, channel, now_ms).await
        }
        kind::KIND_REACTION => apply_reaction(store, community, event, channel, now_ms).await,
        kind::KIND_DELETION => apply_deletion(store, ape, community, event, channel, now_ms).await,
        kind::KIND_DM_OPEN | kind::KIND_DM_ADD_MEMBER => {
            apply_dm_open(store, ape, community, event, now_ms).await
        }
        kind::KIND_DM_HIDE => apply_dm_hide(store, community, event, now_ms).await,
        kind::KIND_MODERATION_BAN
        | kind::KIND_MODERATION_UNBAN
        | kind::KIND_MODERATION_TIMEOUT
        | kind::KIND_MODERATION_UNTIMEOUT => {
            apply_moderation(store, community, event, now_ms).await
        }
        kind::KIND_WORKFLOW_DEF => {
            crate::relay::workflows::on_definition(store, ape, community, event, channel).await
        }
        k if materializes_thread(k) => {
            // A channel-scoped message may be a threaded reply; materialize the
            // thread metadata + counters. A non-reply is a cheap no-op.
            apply_thread(store, community, event, channel, now_ms).await
        }
        _ => Ok(()),
    }
}

/// Whether [`apply_effects`] materializes thread metadata for `k`: a
/// channel-scoped kind that none of its earlier arms claims. Shared with the
/// pre-storage depth check so the two cannot disagree about which kinds are
/// threaded.
fn materializes_thread(k: Kind) -> bool {
    !matches!(
        k,
        kind::KIND_NIP29_PUT_USER
            | kind::KIND_NIP29_REMOVE_USER
            | kind::KIND_NIP29_JOIN_REQUEST
            | kind::KIND_NIP29_LEAVE_REQUEST
            | kind::KIND_NIP29_EDIT_METADATA
            | kind::KIND_NIP29_DELETE_GROUP
            | kind::KIND_REACTION
            | kind::KIND_DELETION
            | kind::KIND_DM_OPEN
            | kind::KIND_DM_ADD_MEMBER
            | kind::KIND_DM_HIDE
            | kind::KIND_MODERATION_BAN
            | kind::KIND_MODERATION_UNBAN
            | kind::KIND_MODERATION_TIMEOUT
            | kind::KIND_MODERATION_UNTIMEOUT
            | kind::KIND_WORKFLOW_DEF
    ) && kind::requires_h_channel_scope(k)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::relay::channel::{ChannelType, Visibility};

    fn pk(byte: u8) -> PubKey {
        [byte; 32]
    }

    fn channel(visibility: Visibility, owners: &[PubKey], add_policy: AddPolicy) -> Channel {
        let mut c = Channel::new(
            "c1".into(),
            "c".into(),
            ChannelType::Stream,
            visibility,
            &owners.first().copied().unwrap_or(pk(0)),
            1_000,
        );
        c.owners = owners.iter().map(|pk| to_hex(pk)).collect();
        c.add_policy = add_policy;
        c
    }

    #[test]
    fn a_permanent_ban_blocks_forever_and_a_temporary_one_lapses() {
        let permanent = Restriction {
            banned: true,
            ..Restriction::default()
        };
        assert!(permanent.blocks_write(u64::MAX));

        let temporary = Restriction {
            banned: true,
            banned_until: Some(2_000),
            ..Restriction::default()
        };
        assert!(temporary.blocks_write(1_999), "live until its expiry");
        assert!(!temporary.blocks_write(2_000), "the expiry instant is free");
    }

    #[test]
    fn a_timeout_blocks_until_its_expiry() {
        let doc = Restriction {
            timeout_until: Some(5_000),
            ..Restriction::default()
        };
        assert!(doc.blocks_write(4_999));
        assert!(!doc.blocks_write(5_000));
    }

    //
    // `buzz-web`'s composer banner is driven ONLY by
    // `features/moderation/lib/timeout.ts::parseTimeoutRejection`, whose
    // documented "load-bearing parse contract" is, verbatim:
    //
    //     const TIMEOUT_PREFIX = "restricted: you are timed out until";
    //     if (!trimmed.startsWith(TIMEOUT_PREFIX)) return null;
    //     const seconds = Number.parseInt(trimmed.slice(TIMEOUT_PREFIX.length).trim(), 10);
    //     if (!Number.isSafeInteger(seconds) || seconds <= 0) return { expiresAtMs: null };
    //     return { expiresAtMs: seconds * 1000 };
    //
    // The relay used to answer `restricted: restricted: you are banned or timed
    // out`, which fails that `startsWith` twice over (doubled token, wrong
    // sentence) and carries no expiry, so the banner could never render.

    /// The client's parser, ported so the two contracts are checked against each
    /// other rather than against a copy of the relay's own wording.
    /// Returns `Some(None)` for "a timeout with an unusable expiry" and
    /// `Some(Some(ms))` for a parsed one, mirroring `TimeoutRejection`.
    fn parse_timeout_rejection(message: &str) -> Option<Option<u64>> {
        const TIMEOUT_PREFIX: &str = "restricted: you are timed out until";
        let trimmed = message.trim();
        let rest = trimmed.strip_prefix(TIMEOUT_PREFIX)?.trim();
        // `Number.parseInt` stops at the first non-digit and yields NaN when the
        // first character is not one.
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        match digits.parse::<u64>() {
            Ok(secs) if secs > 0 => Some(Some(secs * 1000)),
            _ => Some(None),
        }
    }

    #[test]
    fn a_timeout_refusal_matches_the_client_parse_contract_including_its_expiry() {
        let until_ms = 1_700_003_600_000;
        let doc = Restriction {
            timeout_until: Some(until_ms),
            ..Restriction::default()
        };
        let block = doc
            .write_block(1_700_000_000_000)
            .expect("the timeout is live");
        let wire = BuzzError::Unauthorized(block.refusal_detail()).to_string();

        assert_eq!(wire, "restricted: you are timed out until 1700003600");
        assert_eq!(
            parse_timeout_rejection(&wire),
            Some(Some(until_ms)),
            "the composer banner cannot arm from {wire:?}"
        );
    }

    #[test]
    fn no_refusal_carries_a_doubled_wire_token() {
        let cases = [
            Restriction {
                banned: true,
                ..Restriction::default()
            },
            Restriction {
                banned: true,
                banned_until: Some(9_000),
                ..Restriction::default()
            },
            Restriction {
                timeout_until: Some(9_000),
                ..Restriction::default()
            },
        ];
        for doc in cases {
            let detail = doc.write_block(0).expect("blocked at 0").refusal_detail();
            assert!(
                !detail.starts_with("restricted: "),
                "the detail re-spells the token, so the wire reads \
                 'restricted: restricted: …': {detail:?}"
            );
            let wire = BuzzError::Unauthorized(detail).to_string();
            assert!(
                !wire.contains("restricted: restricted:"),
                "doubled token on the wire: {wire:?}"
            );
        }
    }

    #[test]
    fn a_ban_is_never_mistaken_for_a_timeout_and_a_temporary_one_says_when() {
        let permanent = Restriction {
            banned: true,
            ..Restriction::default()
        };
        let wire =
            BuzzError::Unauthorized(permanent.write_block(0).expect("banned").refusal_detail())
                .to_string();
        assert_eq!(wire, "restricted: you are banned from this community");
        assert_eq!(
            parse_timeout_rejection(&wire),
            None,
            "a ban must not arm the timeout banner"
        );

        let temporary = Restriction {
            banned: true,
            banned_until: Some(1_700_003_600_000),
            ..Restriction::default()
        };
        let wire =
            BuzzError::Unauthorized(temporary.write_block(0).expect("banned").refusal_detail())
                .to_string();
        assert_eq!(
            wire, "restricted: you are banned from this community until 1700003600",
            "a temporary ban that says nothing about its end is the permanent-ban \
             bug in another guise"
        );
        assert_eq!(parse_timeout_rejection(&wire), None);
    }

    /// A member who is BOTH banned and timed out hears about the ban: the
    /// timeout's expiry would promise access the ban does not return.
    #[test]
    fn a_live_ban_outranks_a_live_timeout() {
        let doc = Restriction {
            banned: true,
            banned_until: None,
            timeout_until: Some(5_000),
        };
        assert_eq!(
            doc.write_block(0),
            Some(WriteBlock::Banned { until_ms: None })
        );
        // …and once the ban lapses the timeout is still reported.
        let doc = Restriction {
            banned: true,
            banned_until: Some(1_000),
            timeout_until: Some(5_000),
        };
        assert_eq!(
            doc.write_block(2_000),
            Some(WriteBlock::TimedOut { until_ms: 5_000 })
        );
    }

    /// The predicate the write gate consults and the sentence the member is told
    /// must agree at every instant, so a refusal can never be silent.
    #[test]
    fn blocks_write_and_write_block_agree_over_the_whole_matrix() {
        for banned in [false, true] {
            for banned_until in [None, Some(1_000), Some(3_000)] {
                for timeout_until in [None, Some(2_000), Some(4_000)] {
                    let doc = Restriction {
                        banned,
                        banned_until,
                        timeout_until,
                    };
                    for now in [0, 999, 1_000, 1_999, 2_000, 3_000, 4_000, u64::MAX] {
                        assert_eq!(
                            doc.blocks_write(now),
                            doc.write_block(now).is_some(),
                            "{doc:?} at {now}"
                        );
                    }
                }
            }
        }
    }

    /// The live pumps evict on a ban, never on a timeout, and never on a ban that
    /// has already lapsed.
    ///
    /// The lapsed case is reachable: nothing normalizes `banned` until a LATER
    /// moderation command arrives, and the tombstone those pumps watch is also
    /// bumped by an ordinary channel removal — so before this was time-aware, a
    /// member whose one-hour ban ended yesterday was told "your access to this
    /// community was revoked" and hung up on, while the write path admitted them.
    #[test]
    fn only_a_live_ban_costs_a_member_their_live_session() {
        let timed_out = Restriction {
            timeout_until: Some(9_000),
            ..Restriction::default()
        };
        assert!(timed_out.blocks_write(0), "a timeout blocks writes");
        assert!(
            !timed_out.ban_blocks_access(0),
            "a timeout must not evict a reader"
        );

        let lapsed = Restriction {
            banned: true,
            banned_until: Some(1_000),
            ..Restriction::default()
        };
        assert!(lapsed.ban_blocks_access(999), "still banned at 999");
        assert!(
            !lapsed.ban_blocks_access(1_000),
            "the ban lapsed; the document just has not been rewritten yet"
        );
        assert!(
            !lapsed.blocks_write(1_000),
            "the write path already admits them, so eviction contradicts it"
        );

        let permanent = Restriction {
            banned: true,
            ..Restriction::default()
        };
        assert!(permanent.ban_blocks_access(u64::MAX));
        assert!(!Restriction::default().ban_blocks_access(0));
    }

    /// A sub-second expiry rounds UP, so the client's countdown never expires
    /// while the relay is still refusing the write.
    #[test]
    fn an_expiry_is_rounded_up_to_the_next_whole_second() {
        let until_ms = 1_500;
        let doc = Restriction {
            timeout_until: Some(until_ms),
            ..Restriction::default()
        };
        let detail = doc.write_block(0).expect("blocked").refusal_detail();
        assert_eq!(detail, "you are timed out until 2");
        let parsed = parse_timeout_rejection(&BuzzError::Unauthorized(detail).to_string())
            .expect("a timeout refusal")
            .expect("with a usable expiry");
        assert!(
            parsed >= until_ms,
            "the client would stop showing the block at {parsed} while the relay \
             still refuses until {until_ms}"
        );
    }

    /// The wire contract is `expiration` in epoch-SECONDS (NIP-40); the
    /// pre-fix `until` (epoch-ms) stays a fallback and loses a conflict.
    #[test]
    fn moderation_expiry_reads_expiration_seconds_with_until_ms_fallback() {
        let ev = |tags: Vec<Vec<String>>| Event {
            id: [0u8; 32],
            pubkey: pk(1),
            created_at: 1_000,
            kind: kind::KIND_MODERATION_TIMEOUT,
            tags,
            content: String::new(),
            sig: [0u8; 64],
        };
        let t = |n: &str, v: &str| vec![n.to_string(), v.to_string()];

        assert_eq!(
            moderation_expiry_ms(&ev(vec![t("expiration", "1700604800")])),
            Some(1_700_604_800_000),
            "seconds on the wire, milliseconds in the document"
        );
        assert_eq!(
            moderation_expiry_ms(&ev(vec![t("until", "1700003600000")])),
            Some(1_700_003_600_000)
        );
        assert_eq!(
            moderation_expiry_ms(&ev(vec![t("until", "1"), t("expiration", "1700604800"),])),
            Some(1_700_604_800_000),
            "the wire contract wins a conflict"
        );
        assert_eq!(
            moderation_expiry_ms(&ev(vec![t("expiration", "soon")])),
            None
        );
        assert_eq!(moderation_expiry_ms(&ev(vec![])), None);
    }

    #[test]
    fn sole_owner_cannot_be_removed_demoted_or_leave() {
        let owner = pk(1);
        let c = channel(Visibility::Private, &[owner], AddPolicy::Anyone);
        // 9001 self-remove
        assert!(authorize_membership_command(
            kind::KIND_NIP29_REMOVE_USER,
            &c,
            &owner,
            Some(Role::Owner),
            true,
            Some(&owner),
            true,
            None,
            1,
        )
        .is_err());
        // 9022 leave
        assert!(authorize_membership_command(
            kind::KIND_NIP29_LEAVE_REQUEST,
            &c,
            &owner,
            Some(Role::Owner),
            true,
            None,
            false,
            None,
            1,
        )
        .is_err());
        // 9000 demote self to member (the divergence from buzz)
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &c,
            &owner,
            Some(Role::Owner),
            true,
            Some(&owner),
            true,
            Some(Role::Member),
            1,
        )
        .is_err());
    }

    #[test]
    fn an_owner_may_leave_when_another_owner_remains() {
        let owner = pk(1);
        let c = channel(Visibility::Private, &[owner, pk(2)], AddPolicy::Anyone);
        assert!(authorize_membership_command(
            kind::KIND_NIP29_LEAVE_REQUEST,
            &c,
            &owner,
            Some(Role::Owner),
            true,
            None,
            false,
            None,
            2,
        )
        .is_ok());
    }

    #[test]
    fn rejoining_after_removal_never_restores_an_elevated_role() {
        let who = pk(4);
        for role in [Role::Owner, Role::Admin] {
            let mut removed = Membership::new(&who, role, 10);
            removed.removed_at = Some(20);
            let revived = join_membership_doc(Some(removed), &who, 30);
            assert!(revived.is_live());
            assert_eq!(revived.role, Role::Member, "removed {role:?} rejoins as member");
        }
        let live = Membership::new(&who, Role::Admin, 10);
        assert_eq!(join_membership_doc(Some(live), &who, 30).role, Role::Admin);
        assert_eq!(join_membership_doc(None, &who, 30).role, Role::Member);
    }

    #[test]
    fn a_guarded_put_user_never_overwrites_a_moderator_row_it_reads_at_write_time() {
        let (admin, actor) = (pk(4), pk(5));
        for role in [Role::Admin, Role::Owner] {
            let row = Membership::new(&admin, role, 10);
            assert!(
                put_user_doc(Some(row.clone()), &admin, Role::Member, Some(&actor), true, 30)
                    .is_none(),
                "a protected write refuses a live {role:?}"
            );
            let demoted =
                put_user_doc(Some(row), &admin, Role::Member, Some(&actor), false, 30).unwrap();
            assert_eq!(demoted.role, Role::Member, "a moderator's own write demotes");
        }
        let member = Membership::new(&admin, Role::Member, 10);
        assert!(put_user_doc(Some(member), &admin, Role::Member, None, true, 30).is_some());
        assert!(put_user_doc(None, &admin, Role::Member, None, true, 30).is_some());
        let mut removed = Membership::new(&admin, Role::Admin, 10);
        removed.removed_at = Some(20);
        assert!(put_user_doc(Some(removed), &admin, Role::Member, None, true, 30).is_some());
    }

    #[test]
    fn a_non_moderator_cannot_change_a_moderators_role() {
        for target_role in [Role::Admin, Role::Owner] {
            assert!(authorize_target_role_change(
                kind::KIND_NIP29_PUT_USER,
                false,
                Some(target_role)
            )
            .is_err());
            assert!(authorize_target_role_change(
                kind::KIND_NIP29_PUT_USER,
                true,
                Some(target_role)
            )
            .is_ok());
        }
        assert!(
            authorize_target_role_change(kind::KIND_NIP29_PUT_USER, false, Some(Role::Member))
                .is_ok()
        );
        assert!(authorize_target_role_change(kind::KIND_NIP29_PUT_USER, false, None).is_ok());
    }

    #[test]
    fn a_non_member_cannot_add_to_a_private_channel() {
        let c = channel(Visibility::Private, &[pk(1)], AddPolicy::Anyone);
        let stranger = pk(9);
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &c,
            &stranger,
            None,
            false,
            Some(&pk(8)),
            false,
            Some(Role::Member),
            1,
        )
        .is_err());
    }

    #[test]
    fn a_plain_member_cannot_grant_an_elevated_role() {
        let c = channel(Visibility::Private, &[pk(1)], AddPolicy::Anyone);
        let member = pk(2);
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &c,
            &member,
            Some(Role::Member),
            false,
            Some(&pk(8)),
            false,
            Some(Role::Admin),
            1,
        )
        .is_err());
        // ...but may add a plain member.
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &c,
            &member,
            Some(Role::Member),
            false,
            Some(&pk(8)),
            false,
            Some(Role::Member),
            1,
        )
        .is_ok());
    }

    #[test]
    fn add_policy_gates_who_may_add() {
        let member = pk(2);
        // Nobody: even an owner cannot add.
        let nobody = channel(Visibility::Open, &[pk(1)], AddPolicy::Nobody);
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &nobody,
            &pk(1),
            Some(Role::Owner),
            true,
            Some(&pk(8)),
            false,
            Some(Role::Member),
            1,
        )
        .is_err());
        // OwnerOnly: a plain member cannot, an owner can.
        let owner_only = channel(Visibility::Open, &[pk(1)], AddPolicy::OwnerOnly);
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &owner_only,
            &member,
            Some(Role::Member),
            false,
            Some(&pk(8)),
            false,
            Some(Role::Member),
            1,
        )
        .is_err());
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &owner_only,
            &pk(1),
            Some(Role::Owner),
            true,
            Some(&pk(8)),
            false,
            Some(Role::Member),
            1,
        )
        .is_ok());
    }

    #[test]
    fn self_add_short_circuits_on_an_open_channel() {
        let c = channel(Visibility::Open, &[pk(1)], AddPolicy::Anyone);
        let me = pk(5);
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &c,
            &me,
            None,
            false,
            Some(&me),
            false,
            Some(Role::Member),
            1,
        )
        .is_ok());
    }

    #[test]
    fn a_self_grant_of_an_elevated_role_is_refused() {
        // Adversarial-review regression (CRITICAL): the self short-circuit must
        // NOT let a caller grant THEMSELVES owner/admin. A non-member of an open
        // channel naming their own pubkey with role=owner must be refused.
        let c = channel(Visibility::Open, &[pk(1)], AddPolicy::Anyone);
        let attacker = pk(9);
        for role in [Role::Owner, Role::Admin] {
            assert!(
                authorize_membership_command(
                    kind::KIND_NIP29_PUT_USER,
                    &c,
                    &attacker,
                    None,
                    false,
                    Some(&attacker),
                    false,
                    Some(role),
                    1,
                )
                .is_err(),
                "a non-moderator cannot self-grant {role:?}"
            );
        }
        // A plain member of a private channel cannot self-promote to owner either.
        let priv_c = channel(Visibility::Private, &[pk(1)], AddPolicy::Anyone);
        let member = pk(2);
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &priv_c,
            &member,
            Some(Role::Member),
            false,
            Some(&member),
            false,
            Some(Role::Owner),
            1,
        )
        .is_err());
        // But a self-join at member is still fine.
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &c,
            &pk(7),
            None,
            false,
            Some(&pk(7)),
            false,
            Some(Role::Member),
            1,
        )
        .is_ok());
    }

    #[test]
    fn an_admin_cannot_remove_an_owner_even_when_another_owner_remains() {
        let (owner_a, owner_b, admin) = (pk(1), pk(2), pk(3));
        let c = channel(Visibility::Private, &[owner_a, owner_b], AddPolicy::Anyone);
        assert!(authorize_membership_command(
            kind::KIND_NIP29_REMOVE_USER,
            &c,
            &admin,
            Some(Role::Admin),
            false,
            Some(&owner_b),
            true,
            None,
            2,
        )
        .is_err());
        // An owner may still remove a fellow owner who is not the last one.
        assert!(authorize_membership_command(
            kind::KIND_NIP29_REMOVE_USER,
            &c,
            &owner_a,
            Some(Role::Owner),
            true,
            Some(&owner_b),
            true,
            None,
            2,
        )
        .is_ok());
        // And an admin may still remove a plain member.
        assert!(authorize_membership_command(
            kind::KIND_NIP29_REMOVE_USER,
            &c,
            &admin,
            Some(Role::Admin),
            false,
            Some(&pk(9)),
            false,
            None,
            2,
        )
        .is_ok());
    }

    #[test]
    fn an_admin_cannot_demote_an_owner_to_remove_them() {
        let (owner_a, owner_b, admin) = (pk(1), pk(2), pk(3));
        let c = channel(Visibility::Private, &[owner_a, owner_b], AddPolicy::Anyone);
        for role in [Role::Admin, Role::Member] {
            assert!(
                authorize_membership_command(
                    kind::KIND_NIP29_PUT_USER,
                    &c,
                    &admin,
                    Some(Role::Admin),
                    false,
                    Some(&owner_b),
                    true,
                    Some(role),
                    2,
                )
                .is_err(),
                "an admin cannot set an owner's role to {role:?}"
            );
        }
        // An owner may demote a fellow owner who is not the last one.
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &c,
            &owner_a,
            Some(Role::Owner),
            true,
            Some(&owner_b),
            true,
            Some(Role::Member),
            2,
        )
        .is_ok());
    }

    #[test]
    fn only_an_owner_may_grant_ownership_unless_the_room_has_none() {
        let (owner, admin) = (pk(1), pk(3));
        let c = channel(Visibility::Private, &[owner], AddPolicy::Anyone);
        // An admin cannot make themselves, or anyone else, an owner.
        for target in [admin, pk(9)] {
            assert!(authorize_membership_command(
                kind::KIND_NIP29_PUT_USER,
                &c,
                &admin,
                Some(Role::Admin),
                false,
                Some(&target),
                false,
                Some(Role::Owner),
                1,
            )
            .is_err());
        }
        // An owner can.
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &c,
            &owner,
            Some(Role::Owner),
            true,
            Some(&pk(9)),
            false,
            Some(Role::Owner),
            1,
        )
        .is_ok());
        // A room with no owner left can still be recovered by an admin.
        let ownerless = channel(Visibility::Private, &[], AddPolicy::Anyone);
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &ownerless,
            &admin,
            Some(Role::Admin),
            false,
            Some(&admin),
            false,
            Some(Role::Owner),
            0,
        )
        .is_ok());
        // Admins keep granting and changing the roles below owner.
        assert!(authorize_membership_command(
            kind::KIND_NIP29_PUT_USER,
            &c,
            &admin,
            Some(Role::Admin),
            false,
            Some(&pk(9)),
            false,
            Some(Role::Admin),
            1,
        )
        .is_ok());
    }

    #[test]
    fn only_a_moderator_may_remove_someone_else() {
        let c = channel(Visibility::Private, &[pk(1)], AddPolicy::Anyone);
        let member = pk(2);
        let victim = pk(3);
        // A plain member cannot remove another.
        assert!(authorize_membership_command(
            kind::KIND_NIP29_REMOVE_USER,
            &c,
            &member,
            Some(Role::Member),
            false,
            Some(&victim),
            false,
            None,
            1,
        )
        .is_err());
        // An owner can.
        assert!(authorize_membership_command(
            kind::KIND_NIP29_REMOVE_USER,
            &c,
            &pk(1),
            Some(Role::Owner),
            true,
            Some(&victim),
            false,
            None,
            1,
        )
        .is_ok());
        // Anyone may remove themselves.
        assert!(authorize_membership_command(
            kind::KIND_NIP29_REMOVE_USER,
            &c,
            &member,
            Some(Role::Member),
            false,
            Some(&member),
            false,
            None,
            1,
        )
        .is_ok());
    }

    #[test]
    fn join_is_refused_on_a_private_channel_and_allowed_on_an_open_one() {
        let private = channel(Visibility::Private, &[pk(1)], AddPolicy::Anyone);
        assert!(authorize_membership_command(
            kind::KIND_NIP29_JOIN_REQUEST,
            &private,
            &pk(5),
            None,
            false,
            None,
            false,
            None,
            1,
        )
        .is_err());
        let open = channel(Visibility::Open, &[pk(1)], AddPolicy::Anyone);
        assert!(authorize_membership_command(
            kind::KIND_NIP29_JOIN_REQUEST,
            &open,
            &pk(5),
            None,
            false,
            None,
            false,
            None,
            1,
        )
        .is_ok());
    }

    fn edit(tags: Vec<Vec<String>>) -> Event {
        Event {
            id: [7u8; 32],
            pubkey: pk(1),
            created_at: 1_700_000_000,
            kind: kind::KIND_NIP29_EDIT_METADATA,
            tags,
            content: String::new(),
            sig: [0u8; 64],
        }
    }

    fn tag(name: &str, value: &str) -> Vec<String> {
        vec![name.into(), value.into()]
    }

    #[test]
    fn canonical_channel_name_strips_leading_hash_and_space() {
        assert_eq!(canonical_channel_name("##  general "), "general");
        assert_eq!(canonical_channel_name("a#b"), "a#b");
        assert_eq!(canonical_channel_name("   "), "");
    }

    #[test]
    fn privileged_edits_need_a_moderator_member_edits_need_membership() {
        let c = channel(Visibility::Private, &[pk(1)], AddPolicy::Anyone);
        // A plain member may set the topic.
        assert!(
            authorize_channel_edit(&c, Some(Role::Member), &edit(vec![tag("topic", "t")])).is_ok()
        );
        // ...but not the name (privileged).
        assert!(
            authorize_channel_edit(&c, Some(Role::Member), &edit(vec![tag("name", "x")])).is_err()
        );
        // An owner may.
        assert!(
            authorize_channel_edit(&c, Some(Role::Owner), &edit(vec![tag("name", "x")])).is_ok()
        );
        // A non-member may set nothing.
        assert!(authorize_channel_edit(&c, None, &edit(vec![tag("topic", "t")])).is_err());
    }

    #[test]
    fn an_archived_channel_accepts_only_the_unarchive() {
        let mut c = channel(Visibility::Open, &[pk(1)], AddPolicy::Anyone);
        c.archived_at = Some(5);
        assert!(
            authorize_channel_edit(&c, Some(Role::Owner), &edit(vec![tag("name", "x")])).is_err()
        );
        assert!(authorize_channel_edit(
            &c,
            Some(Role::Owner),
            &edit(vec![tag("archived", "false")])
        )
        .is_ok());
    }

    fn threaded(e_tags: Vec<Vec<String>>) -> Event {
        Event {
            id: [4u8; 32],
            pubkey: pk(1),
            created_at: 1_700_000_000,
            kind: kind::KIND_STREAM_MESSAGE,
            tags: e_tags,
            content: "hi".into(),
            sig: [0u8; 64],
        }
    }

    #[test]
    fn event_ids_are_canonicalized_to_lowercase_before_they_become_keys() {
        let upper = "AB".repeat(32);
        let lower = "ab".repeat(32);
        let e = threaded(vec![vec![
            "e".into(),
            upper.clone(),
            String::new(),
            "reply".into(),
        ]]);
        assert_eq!(thread_ancestry(&e), Some((lower.clone(), lower.clone())));
        let mut r = threaded(vec![vec!["e".into(), upper]]);
        r.kind = kind::KIND_REACTION;
        assert_eq!(
            reaction_target_and_emoji(&r),
            Some((lower, "hi".to_string()))
        );
    }

    #[test]
    fn thread_ancestry_reads_nip10_markers() {
        let root = "a".repeat(64);
        let reply = "b".repeat(64);
        // (root, reply) ⇒ parent = reply.
        let e = threaded(vec![
            vec!["e".into(), root.clone(), String::new(), "root".into()],
            vec!["e".into(), reply.clone(), String::new(), "reply".into()],
        ]);
        assert_eq!(thread_ancestry(&e), Some((root.clone(), reply)));
        // (None, reply) ⇒ root = parent = reply.
        let only_reply = "c".repeat(64);
        let e = threaded(vec![vec![
            "e".into(),
            only_reply.clone(),
            String::new(),
            "reply".into(),
        ]]);
        assert_eq!(thread_ancestry(&e), Some((only_reply.clone(), only_reply)));
        // (root, None) ⇒ not a reply.
        let e = threaded(vec![vec![
            "e".into(),
            root.clone(),
            String::new(),
            "root".into(),
        ]]);
        assert_eq!(thread_ancestry(&e), None);
        // No markers ⇒ not a reply.
        assert_eq!(thread_ancestry(&threaded(vec![])), None);
        // An `e` tag without a 4th-field marker is ignored.
        let e = threaded(vec![vec!["e".into(), root, String::new()]]);
        assert_eq!(thread_ancestry(&e), None);
    }

    fn reaction_event(e_tags: &[&str], content: &str) -> Event {
        Event {
            id: [3u8; 32],
            pubkey: pk(1),
            created_at: 1_700_000_000,
            kind: kind::KIND_REACTION,
            tags: e_tags.iter().map(|t| tag("e", t)).collect(),
            content: content.to_string(),
            sig: [0u8; 64],
        }
    }

    #[test]
    fn reaction_target_is_the_last_valid_e_tag_and_empty_content_is_plus() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        // Last valid e tag wins.
        let (target, emoji) = reaction_target_and_emoji(&reaction_event(&[&a, &b], "🔥")).unwrap();
        assert_eq!(target, b);
        assert_eq!(emoji, "🔥");
        // Empty content ⇒ "+".
        let (_, emoji) = reaction_target_and_emoji(&reaction_event(&[&a], "")).unwrap();
        assert_eq!(emoji, "+");
    }

    #[test]
    fn reaction_without_a_valid_target_or_with_a_long_emoji_is_none() {
        // No e tag.
        assert!(reaction_target_and_emoji(&reaction_event(&[], "👍")).is_none());
        // e tag not 64-hex.
        assert!(reaction_target_and_emoji(&reaction_event(&["short"], "👍")).is_none());
        // Emoji over the char cap.
        let a = "a".repeat(64);
        let long = "x".repeat(MAX_REACTION_EMOJI_CHARS + 1);
        assert!(reaction_target_and_emoji(&reaction_event(&[&a], &long)).is_none());
    }

    #[test]
    fn malformed_edit_values_are_refused() {
        let c = channel(Visibility::Open, &[pk(1)], AddPolicy::Anyone);
        let mod_role = Some(Role::Owner);
        assert!(
            authorize_channel_edit(&c, mod_role, &edit(vec![tag("archived", "maybe")])).is_err()
        );
        assert!(authorize_channel_edit(&c, mod_role, &edit(vec![tag("ttl", "0")])).is_err());
        assert!(authorize_channel_edit(&c, mod_role, &edit(vec![tag("ttl", "-3")])).is_err());
        assert!(authorize_channel_edit(&c, mod_role, &edit(vec![tag("name", "  ")])).is_err());
        assert!(authorize_channel_edit(&c, mod_role, &edit(vec![tag("nope", "x")])).is_err());
        // A valid ttl clear and a valid positive ttl both pass.
        assert!(authorize_channel_edit(&c, mod_role, &edit(vec![tag("ttl", "")])).is_ok());
        assert!(authorize_channel_edit(&c, mod_role, &edit(vec![tag("ttl", "60")])).is_ok());
    }

    #[test]
    fn replacement_order_is_newest_then_lowest_id_and_total() {
        // Newer created_at wins regardless of id.
        assert!(version_beats(10, "ff", 9, "00"));
        assert!(!version_beats(9, "00", 10, "ff"));
        // Equal timestamps: the LOWEST id wins.
        assert!(version_beats(10, "0a", 10, "0b"));
        assert!(!version_beats(10, "0b", 10, "0a"));
        // An event never beats itself (the re-drive no-op).
        assert!(!version_beats(10, "0a", 10, "0a"));
        // Antisymmetric: exactly one of two distinct versions wins.
        for (a, b) in [((10, "0a"), (10, "0b")), ((11, "ff"), (10, "00"))] {
            assert_ne!(
                version_beats(a.0, a.1, b.0, b.1),
                version_beats(b.0, b.1, a.0, a.1)
            );
        }
    }

    #[test]
    fn replacement_cells_cover_the_three_shapes() {
        let event = |kind_: Kind, tags: Vec<Vec<String>>| Event {
            id: [1; 32],
            pubkey: pk(1),
            created_at: 1,
            kind: kind_,
            tags,
            content: String::new(),
            sig: [0; 64],
        };
        // NIP-01 replaceable: the empty cell, whatever the tags say.
        assert_eq!(
            replacement_d_tag(&event(0, vec![tag("d", "ignored")])),
            Some(String::new())
        );
        assert_eq!(
            replacement_d_tag(&event(10000, vec![])),
            Some(String::new())
        );
        // NIP-33: the first d value; absent d ⇒ the "" cell (the convention).
        assert_eq!(
            replacement_d_tag(&event(30078, vec![tag("d", "inbox")])),
            Some("inbox".to_string())
        );
        assert_eq!(
            replacement_d_tag(&event(30078, vec![])),
            Some(String::new())
        );
        // Regular and ephemeral kinds never replace.
        assert_eq!(
            replacement_d_tag(&event(kind::KIND_STREAM_MESSAGE, vec![])),
            None
        );
        assert_eq!(
            replacement_d_tag(&event(kind::KIND_PRESENCE_UPDATE, vec![])),
            None
        );
    }

    /// The trigger table: which admitted kinds invalidate which projection —
    /// and, load-bearingly, that a projection invalidates NOTHING, which is
    /// what stops an emission from triggering an emission.
    #[test]
    fn projections_are_triggered_by_commands_and_never_by_themselves() {
        assert_eq!(
            projections_for(kind::KIND_NIP29_CREATE_GROUP),
            &[kind::KIND_NIP29_GROUP_METADATA]
        );
        assert_eq!(
            projections_for(kind::KIND_NIP29_EDIT_METADATA),
            &[kind::KIND_NIP29_GROUP_METADATA]
        );
        for command in [
            kind::KIND_NIP29_PUT_USER,
            kind::KIND_NIP29_REMOVE_USER,
            kind::KIND_NIP29_JOIN_REQUEST,
            kind::KIND_NIP29_LEAVE_REQUEST,
        ] {
            assert_eq!(
                projections_for(command),
                &[
                    kind::KIND_NIP29_GROUP_MEMBERS,
                    kind::KIND_NIP29_GROUP_ADMINS
                ],
                "kind {command} must republish the roster"
            );
        }
        // The projections themselves, and everything else.
        for quiet in [
            kind::KIND_NIP29_GROUP_METADATA,
            kind::KIND_NIP29_GROUP_ADMINS,
            kind::KIND_NIP29_GROUP_MEMBERS,
            kind::KIND_STREAM_MESSAGE,
            kind::KIND_REACTION,
            kind::KIND_PRESENCE_UPDATE,
            kind::KIND_MODERATION_BAN,
        ] {
            assert!(
                projections_for(quiet).is_empty(),
                "kind {quiet} must not trigger a projection"
            );
        }
        // Every kind this module can emit is a kind it will not react to: the
        // no-loop invariant, checked over the whole space rather than by
        // inspection.
        for k in 0..=65_535u32 {
            if kind::is_group_projection(k) {
                assert!(projections_for(k).is_empty(), "kind {k} would loop");
            }
            for &emitted in projections_for(k) {
                assert!(
                    kind::is_group_projection(emitted),
                    "kind {k} emits {emitted}, which is not a projection"
                );
            }
        }
    }

    /// The 39000 tag vocabulary a real client parses: `d`, `h`, `name`, and the
    /// two NIP-29 marker pairs, each derived from the rule it describes.
    #[test]
    fn metadata_tags_describe_the_channel_the_gate_enforces() {
        let mut open = Channel::new(
            "c1".into(),
            "general".into(),
            ChannelType::Stream,
            Visibility::Open,
            &pk(1),
            1_000,
        );
        open.description = Some("the room".into());
        open.topic = Some("shipping".into());
        let mut tags = vec![tag("d", "c1"), tag("h", "c1")];
        push_metadata_tags(&open, &mut tags);
        let flat: Vec<Vec<&str>> = tags
            .iter()
            .map(|t| t.iter().map(String::as_str).collect())
            .collect();
        assert!(flat.contains(&vec!["name", "general"]));
        assert!(flat.contains(&vec!["about", "the room"]));
        assert!(flat.contains(&vec!["topic", "shipping"]));
        assert!(flat.contains(&vec!["t", "stream"]));
        // An open channel is public (readable) AND open (self-joinable): both
        // markers mirror a rule this relay actually enforces.
        assert!(flat.contains(&vec!["public"]));
        assert!(flat.contains(&vec!["open"]));
        assert!(!flat.contains(&vec!["private"]));
        assert!(!flat.contains(&vec!["closed"]));
        // Absent optional fields contribute no empty tags.
        assert!(!flat.iter().any(|t| t.first() == Some(&"purpose")));
        assert!(!flat.iter().any(|t| t.first() == Some(&"archived")));
        assert!(!flat.iter().any(|t| t.first() == Some(&"ttl")));
        // `picture` is deliberately never emitted: no channel field backs it.
        assert!(!flat.iter().any(|t| t.first() == Some(&"picture")));

        let mut private = Channel::new(
            "c2".into(),
            "secrets".into(),
            ChannelType::Dm,
            Visibility::Private,
            &pk(1),
            1_000,
        );
        private.archived_at = Some(2_000);
        private.ttl_seconds = Some(3_600);
        // An empty description must not become an empty `about` tag.
        private.description = Some(String::new());
        let mut tags = vec![];
        push_metadata_tags(&private, &mut tags);
        let flat: Vec<Vec<&str>> = tags
            .iter()
            .map(|t| t.iter().map(String::as_str).collect())
            .collect();
        assert!(flat.contains(&vec!["private"]));
        assert!(flat.contains(&vec!["closed"]));
        assert!(flat.contains(&vec!["hidden"]), "a DM is hidden from lists");
        assert!(flat.contains(&vec!["t", "dm"]));
        assert!(flat.contains(&vec!["archived", "true"]));
        assert!(flat.contains(&vec!["ttl", "3600"]));
        assert!(!flat.iter().any(|t| t.first() == Some(&"about")));
    }

    /// [`MAX_PROJECTION_MEMBERS`] is derived from the serialized-event ceiling;
    /// this pins the derivation against drift. Worst case on every axis: the
    /// longest role on every member, a channel id at the key layer's component
    /// cap in both `d` and `h`, the truncation marker at its widest value, and
    /// `created_at` at its widest — and [`Event::sign_new`], which refuses an
    /// event over [`crate::relay::event::MAX_SERIALIZED_EVENT_BYTES`], must still
    /// mint it. If a future tag pushes this over, the constant must shrink
    /// with it, and this test is what says so.
    #[test]
    fn a_roster_at_the_member_cap_still_signs() {
        let channel = "c".repeat(keys::MAX_COMPONENT_BYTES);
        let mut tags = vec![
            vec!["d".to_string(), channel.clone()],
            vec!["h".to_string(), channel],
        ];
        for i in 0..MAX_PROJECTION_MEMBERS {
            tags.push(vec![
                "p".to_string(),
                format!("{i:064x}"),
                String::new(),
                Role::Member.as_str().to_string(),
            ]);
        }
        push_truncation_tag(&mut tags, MAX_PROJECTION_MEMBERS, usize::MAX);
        let event = Event::sign_new(
            &[1u8; 32],
            kind::KIND_NIP29_GROUP_MEMBERS,
            tags,
            "",
            u64::MAX,
        )
        .expect("a cap-sized roster must be mintable");
        assert!(
            event.serialized_len() <= crate::relay::event::MAX_SERIALIZED_EVENT_BYTES,
            "{} > {}",
            event.serialized_len(),
            crate::relay::event::MAX_SERIALIZED_EVENT_BYTES
        );
    }

    /// The marker only exists when something was cut: a complete roster's tag
    /// list must stay byte-identical to what was published before the marker
    /// was invented, or every deploy would republish every channel.
    #[test]
    fn the_truncation_tag_appears_only_when_members_were_cut() {
        let mut complete = vec![vec!["d".to_string(), "c1".to_string()]];
        push_truncation_tag(&mut complete, 7, 7);
        assert_eq!(complete.len(), 1, "complete ⇒ no marker");

        let mut cut = vec![vec!["d".to_string(), "c1".to_string()]];
        push_truncation_tag(&mut cut, 7, 9);
        assert_eq!(
            cut.last(),
            Some(&vec!["truncated".to_string(), "9".to_string()])
        );
    }
}
