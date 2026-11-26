//! The access gate: who may see which event.
//!
//! This is the security chokepoint every read and every live delivery passes
//! through, transliterated from buzz's `filter_fanout_by_access`
//! (`buzz/crates/buzz-relay/src/handlers/event.rs:115-222`) and its REQ-side
//! read gates. Two properties are load-bearing and preserved exactly:
//!
//! 1. **Fail closed.** If a fact needed for the decision could not be resolved —
//!    a channel's visibility, a reader's membership — the answer is *deny*, never
//!    "probably fine". A leaked private message cannot be un-leaked.
//! 2. **One chokepoint.** Historical queries, live feed delivery, and search hit
//!    re-authorization all call [`may_deliver`]; there is no second, subtly
//!    different copy of the policy for one of those paths to drift into.
//!
//! The module is deliberately pure: callers resolve the facts (with their own
//! caching and its own staleness rules) and hand them in as [`ChannelAccess`].
//! Policy that cannot perform I/O cannot accidentally depend on I/O succeeding.

use crate::relay::event::{Event, PubKey};
use crate::relay::kind;

/// The channel-side facts a delivery decision needs, resolved by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelAccess {
    /// The event carries no channel — it is global within the community
    /// (profiles, gift-wrapped DMs, membership notices, workflow system events).
    Global,
    /// The event's channel exists and is not private.
    Open,
    /// The event's channel is private; membership was resolved.
    Private { reader_is_member: bool },
    /// A required lookup failed or the channel is unknown. Always denies.
    Unresolved,
}

/// Why a delivery was refused. Carried so the caller can log and meter refusals
/// without re-deriving the reason, and so tests assert on the specific gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The event belongs to a different community than the reader's session.
    ForeignCommunity,
    /// An author-only kind addressed to someone who is not the author.
    AuthorOnly,
    /// A `#p`-gated kind whose `p` tag does not name the reader.
    NotAddressed,
    /// An unshared shared-gated event (persona / team catalog) belonging to
    /// another author.
    UnsharedPersona,
    /// A private channel the reader is not a member of.
    NotAMember,
    /// A fact needed for the decision could not be resolved.
    Unresolved,
}

/// The outcome of an access decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(DenyReason),
}

impl Decision {
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// May `reader` receive `event`?
///
/// Gate order matters and matches buzz: community fence, then author-only kinds,
/// then `#p`-gated kinds, then the persona share gate, then channel membership.
/// The earlier gates run before the channel gate because the kinds they protect
/// are stored globally (`channel_id = None`) and would otherwise skip straight to
/// "allowed" on the [`ChannelAccess::Global`] path.
#[must_use]
pub fn may_deliver(
    event: &Event,
    reader: &PubKey,
    access: ChannelAccess,
    same_community: bool,
) -> Decision {
    if !same_community {
        return Decision::Deny(DenyReason::ForeignCommunity);
    }

    let is_author = &event.pubkey == reader;

    // Author-only kinds (reminders, push leases) never leave their author.
    if kind::is_author_only(event.kind) && !is_author {
        return Decision::Deny(DenyReason::AuthorOnly);
    }

    // `#p`-gated kinds (gift-wrapped DMs, membership notices, agent observer
    // frames, encrypted turn metrics) reach the author and the addressed pubkey.
    if kind::is_p_gated(event.kind) && !is_author && !addresses(event, reader) {
        return Decision::Deny(DenyReason::NotAddressed);
    }

    // Result-gated kinds are the same shape with a narrower rule: only the
    // addressed owner, not merely any `p` tag holder.
    if kind::is_result_gated(event.kind) && !is_author && !addresses(event, reader) {
        return Decision::Deny(DenyReason::NotAddressed);
    }

    // Shared-gated kinds (personas, team-catalog entries) are private drafts
    // until explicitly shared.
    if is_unshared_gated(event) && !is_author {
        return Decision::Deny(DenyReason::UnsharedPersona);
    }

    // A relay-signed group projection (39000/39001/39002) *is* a description of
    // one channel: its name, its admins, its member list. So it must be gated
    // by that channel and can never take the community-global path — a 39002
    // that reached this gate as `Global` is a projection whose channel the
    // caller could not resolve, and serving it would hand a private room's
    // roster to a stranger. The channel gate below then gives NIP-29 exactly:
    // an open channel's projection is community-readable, a private channel's
    // is member-only.
    //
    // Note this is not redundant with the resolution step: it is what makes the
    // whole class fail CLOSED if a projection is ever stored without a
    // resolvable channel (a hand-seeded row, an older emitter, a future import
    // path). The author is refused too — the relay reads its own projections
    // through no privileged path.
    if kind::is_group_projection(event.kind) && matches!(access, ChannelAccess::Global) {
        return Decision::Deny(DenyReason::Unresolved);
    }

    match access {
        ChannelAccess::Global | ChannelAccess::Open => Decision::Allow,
        ChannelAccess::Private {
            reader_is_member: true,
        } => Decision::Allow,
        ChannelAccess::Private {
            reader_is_member: false,
        } => {
            // The relay-signed membership notices (44100/44101) are the one
            // exemption from the membership gate, and only for their `p`-tag
            // SUBJECT. A 44101 tells its subject they were REMOVED — and by the
            // time they read it they are exactly a non-member, because the
            // revocation deliberately commits before the notice is pushed
            // (§6.3 rule b). Without this arm the revocation censors the notice
            // announcing the revocation, and a private room's 44100 history the
            // subject legitimately received as a member vanishes retroactively.
            //
            // The exemption is narrow by construction: the kinds are relay-only
            // (a client-forged one is refused at ingest), `#p`-gated (a
            // non-subject non-author was already denied `NotAddressed` above),
            // and a notice's content names only the room, the subject, and the
            // trigger id — facts the subject already holds. Nothing else about
            // the room becomes readable: every other kind still takes the
            // `NotAMember` arm below.
            if kind::is_membership_notice(event.kind) && addresses(event, reader) {
                Decision::Allow
            } else {
                Decision::Deny(DenyReason::NotAMember)
            }
        }
        ChannelAccess::Unresolved => Decision::Deny(DenyReason::Unresolved),
    }
}

/// Does the event carry a `p` tag naming this pubkey?
#[must_use]
pub fn addresses(event: &Event, reader: &PubKey) -> bool {
    let want = crate::relay::crypto::to_hex(reader);
    event.tag_values("p").any(|v| v.eq_ignore_ascii_case(&want))
}

/// A shared-gated event ([`kind::SHARED_GATED_KINDS`]: persona 30175, team
/// catalog 30178) that has not been marked exactly `["shared","true"]`.
///
/// Exactly that spelling and no other — ingest refuses the non-canonical
/// spellings so nothing that reads as "shared" to a person is quietly treated
/// as unshared here.
#[must_use]
pub fn is_unshared_gated(event: &Event) -> bool {
    if !kind::is_shared_gated(event.kind) {
        return false;
    }
    // Exactly one `["shared","true"]` tag and no other `shared` tag: a longer
    // tag, a second copy or a conflicting value fails closed, as upstream's
    // event_is_shared does.
    let mut shared = 0usize;
    for tag in event.tags.iter().filter(|t| t.first().is_some_and(|k| k == "shared")) {
        if tag.len() != 2 || tag.get(1).is_none_or(|v| v != "true") {
            return true;
        }
        shared += 1;
    }
    shared != 1
}

/// A persona event that has not been marked `["shared","true"]` — the
/// persona-specific member of [`is_unshared_gated`], kept because the ingest
/// persona contract and several documents name personas specifically.
#[must_use]
pub fn is_unshared_persona(event: &Event) -> bool {
    kind::is_persona(event.kind) && is_unshared_gated(event)
}

/// Whether a *filter* is allowed to ask for a sensitive kind at all.
///
/// buzz refuses the subscription rather than silently returning nothing, so a
/// client learns it asked for something it may not have. A filter touching
/// `#p`-gated kinds must constrain `#p` to the reader; one touching author-only
/// kinds must constrain `authors` to the reader. The `ids` shortcut is
/// deliberately *not* an escape hatch for result-gated kinds — buzz closed that
/// hole, and reopening it would let a caller enumerate ids to read others' data.
#[must_use]
pub fn filter_authorized(
    kinds: Option<&[crate::relay::kind::Kind]>,
    authors_hex: Option<&[String]>,
    p_tags_hex: Option<&[String]>,
    reader: &PubKey,
) -> bool {
    let Some(kinds) = kinds else {
        // An unconstrained-kind filter is a global subscription; sensitive kinds
        // are excluded from those at delivery time by `may_deliver`, so the
        // filter itself is admissible.
        return true;
    };
    let reader_hex = crate::relay::crypto::to_hex(reader);
    let names_only_reader = |list: Option<&[String]>| {
        list.is_some_and(|l| !l.is_empty() && l.iter().all(|v| v.eq_ignore_ascii_case(&reader_hex)))
    };

    for k in kinds {
        if kind::is_author_only(*k) && !names_only_reader(authors_hex) {
            return false;
        }
        if (kind::is_p_gated(*k) || kind::is_result_gated(*k))
            && !names_only_reader(p_tags_hex)
            && !names_only_reader(authors_hex)
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::relay::crypto::to_hex;
    use crate::relay::event::Event;

    fn pk(b: u8) -> PubKey {
        [b; 32]
    }

    fn ev(kind: u32, author: PubKey, tags: Vec<Vec<String>>) -> Event {
        Event {
            id: [0u8; 32],
            pubkey: author,
            created_at: 1_000,
            kind,
            tags,
            content: String::new(),
            sig: [0u8; 64],
        }
    }

    fn p_tag(who: PubKey) -> Vec<String> {
        vec!["p".into(), to_hex(&who)]
    }

    #[test]
    fn open_channel_events_reach_any_reader() {
        let e = ev(kind::KIND_STREAM_MESSAGE, pk(1), vec![]);
        assert!(may_deliver(&e, &pk(2), ChannelAccess::Open, true).is_allowed());
    }

    #[test]
    fn private_channel_requires_membership() {
        let e = ev(kind::KIND_STREAM_MESSAGE, pk(1), vec![]);
        assert!(may_deliver(
            &e,
            &pk(2),
            ChannelAccess::Private {
                reader_is_member: true
            },
            true
        )
        .is_allowed());
        assert_eq!(
            may_deliver(
                &e,
                &pk(2),
                ChannelAccess::Private {
                    reader_is_member: false
                },
                true
            ),
            Decision::Deny(DenyReason::NotAMember)
        );
    }

    #[test]
    fn unresolved_facts_fail_closed() {
        let e = ev(kind::KIND_STREAM_MESSAGE, pk(1), vec![]);
        assert_eq!(
            may_deliver(&e, &pk(2), ChannelAccess::Unresolved, true),
            Decision::Deny(DenyReason::Unresolved)
        );
        // Even the author is refused when the channel cannot be resolved: the
        // gate does not know the event belongs where it claims.
        assert_eq!(
            may_deliver(&e, &pk(1), ChannelAccess::Unresolved, true),
            Decision::Deny(DenyReason::Unresolved)
        );
    }

    #[test]
    fn foreign_community_is_refused_first() {
        let e = ev(kind::KIND_STREAM_MESSAGE, pk(1), vec![]);
        assert_eq!(
            may_deliver(&e, &pk(1), ChannelAccess::Open, false),
            Decision::Deny(DenyReason::ForeignCommunity)
        );
    }

    #[test]
    fn author_only_kinds_reach_only_their_author() {
        let author = pk(1);
        let e = ev(kind::KIND_EVENT_REMINDER, author, vec![]);
        assert!(may_deliver(&e, &author, ChannelAccess::Global, true).is_allowed());
        assert_eq!(
            may_deliver(&e, &pk(2), ChannelAccess::Global, true),
            Decision::Deny(DenyReason::AuthorOnly)
        );
    }

    #[test]
    fn p_gated_kinds_reach_author_and_addressee_only() {
        let author = pk(1);
        let addressee = pk(2);
        let e = ev(kind::KIND_GIFT_WRAP, author, vec![p_tag(addressee)]);
        assert!(may_deliver(&e, &author, ChannelAccess::Global, true).is_allowed());
        assert!(may_deliver(&e, &addressee, ChannelAccess::Global, true).is_allowed());
        assert_eq!(
            may_deliver(&e, &pk(3), ChannelAccess::Global, true),
            Decision::Deny(DenyReason::NotAddressed)
        );
    }

    #[test]
    fn p_tag_matching_is_case_insensitive_hex() {
        let addressee = pk(0xab);
        let mut tag = p_tag(addressee);
        tag[1] = tag[1].to_uppercase();
        let e = ev(kind::KIND_GIFT_WRAP, pk(1), vec![tag]);
        assert!(addresses(&e, &addressee));
    }

    #[test]
    fn unshared_persona_stays_with_its_author() {
        let author = pk(1);
        let unshared = ev(kind::KIND_PERSONA, author, vec![]);
        assert!(is_unshared_persona(&unshared));
        assert!(may_deliver(&unshared, &author, ChannelAccess::Open, true).is_allowed());
        assert_eq!(
            may_deliver(&unshared, &pk(2), ChannelAccess::Open, true),
            Decision::Deny(DenyReason::UnsharedPersona)
        );

        let shared = ev(
            kind::KIND_PERSONA,
            author,
            vec![vec!["shared".into(), "true".into()]],
        );
        assert!(!is_unshared_persona(&shared));
        assert!(may_deliver(&shared, &pk(2), ChannelAccess::Open, true).is_allowed());
    }

    /// The relay's own member list follows the channel it describes: readable
    /// in an open room, member-only in a private one.
    #[test]
    fn a_group_projection_is_gated_by_the_channel_it_describes() {
        let relay = pk(0xee);
        for kind_ in [
            kind::KIND_NIP29_GROUP_METADATA,
            kind::KIND_NIP29_GROUP_ADMINS,
            kind::KIND_NIP29_GROUP_MEMBERS,
        ] {
            let projection = ev(
                kind_,
                relay,
                vec![
                    vec!["d".into(), "chan-1".into()],
                    vec!["h".into(), "chan-1".into()],
                    p_tag(pk(1)),
                ],
            );
            // Open channel: any workspace reader may see the roster.
            assert!(
                may_deliver(&projection, &pk(9), ChannelAccess::Open, true).is_allowed(),
                "kind {kind_} must be readable in an open channel"
            );
            // Private channel: members only, even though the reader is `p`-tagged
            // in it (being NAMED in a roster is not membership in the room).
            assert!(may_deliver(
                &projection,
                &pk(1),
                ChannelAccess::Private {
                    reader_is_member: true
                },
                true
            )
            .is_allowed());
            assert_eq!(
                may_deliver(
                    &projection,
                    &pk(1),
                    ChannelAccess::Private {
                        reader_is_member: false
                    },
                    true
                ),
                Decision::Deny(DenyReason::NotAMember),
                "kind {kind_} leaked a private channel's roster to a non-member"
            );
        }
    }

    /// The fail-closed half: a projection whose channel could not be resolved
    /// must NOT fall through to the community-global allow — not even for the
    /// relay that signed it.
    #[test]
    fn a_channelless_group_projection_is_refused_to_everyone() {
        let relay = pk(0xee);
        let orphan = ev(kind::KIND_NIP29_GROUP_MEMBERS, relay, vec![p_tag(pk(1))]);
        for reader in [pk(1), pk(9), relay] {
            assert_eq!(
                may_deliver(&orphan, &reader, ChannelAccess::Global, true),
                Decision::Deny(DenyReason::Unresolved),
                "a channel-less 39002 must never be served globally"
            );
        }
        assert_eq!(
            may_deliver(&orphan, &pk(9), ChannelAccess::Unresolved, true),
            Decision::Deny(DenyReason::Unresolved)
        );
        // The client-signed membership COMMAND keeps its old, global semantics:
        // this gate is about the relay's projections, not about 9000/9001.
        let command = ev(kind::KIND_NIP29_PUT_USER, pk(1), vec![p_tag(pk(2))]);
        assert!(may_deliver(&command, &pk(9), ChannelAccess::Global, true).is_allowed());
    }

    /// The membership-notice exemption: the `p`-tag subject reads their own
    /// 44100/44101 in a private room they are NOT a member of — and nobody
    /// else gains anything from the exemption.
    #[test]
    fn a_membership_notice_reaches_its_subject_in_a_private_room() {
        let relay = pk(0xee);
        let subject = pk(1);
        let outside = ChannelAccess::Private {
            reader_is_member: false,
        };
        for kind_ in [
            kind::KIND_MEMBER_ADDED_NOTIFICATION,
            kind::KIND_MEMBER_REMOVED_NOTIFICATION,
        ] {
            let notice = ev(
                kind_,
                relay,
                vec![vec!["h".into(), "chan-1".into()], p_tag(subject)],
            );
            assert!(
                may_deliver(&notice, &subject, outside, true).is_allowed(),
                "kind {kind_} must reach its subject after their membership is gone"
            );
            // A non-subject is still refused — at the `#p` gate, before the
            // channel arm is even reached.
            assert_eq!(
                may_deliver(&notice, &pk(9), outside, true),
                Decision::Deny(DenyReason::NotAddressed),
                "kind {kind_} leaked to a non-subject"
            );
            // The AUTHOR (the relay) gets no channel-gate exemption: subject only.
            assert_eq!(
                may_deliver(&notice, &relay, outside, true),
                Decision::Deny(DenyReason::NotAMember)
            );
            // Fail-closed is preserved: an unresolvable channel refuses even
            // the subject.
            assert_eq!(
                may_deliver(&notice, &subject, ChannelAccess::Unresolved, true),
                Decision::Deny(DenyReason::Unresolved)
            );
        }
    }

    /// The exemption is those two kinds and nothing else: an ordinary
    /// `#p`-gated kind addressed to the reader stays member-only in a private
    /// room (the 39002-roster rule next door already pins the projections).
    #[test]
    fn the_notice_exemption_does_not_widen_to_other_p_gated_kinds() {
        let e = ev(kind::KIND_GIFT_WRAP, pk(1), vec![p_tag(pk(2))]);
        assert_eq!(
            may_deliver(
                &e,
                &pk(2),
                ChannelAccess::Private {
                    reader_is_member: false
                },
                true
            ),
            Decision::Deny(DenyReason::NotAMember)
        );
    }

    #[test]
    fn shared_tag_must_be_exactly_true() {
        let e = ev(
            kind::KIND_PERSONA,
            pk(1),
            vec![vec!["shared".into(), "yes".into()]],
        );
        assert!(is_unshared_persona(&e));
    }

    /// Kind 30178 rides the SAME share gate as personas (upstream's
    /// SHARED_GATED_KINDS, sync 001): unshared reaches only its author,
    /// shared reaches anyone the channel gate admits.
    #[test]
    fn an_unshared_team_catalog_stays_with_its_author() {
        let author = pk(1);
        let unshared = ev(kind::KIND_TEAM_CATALOG, author, vec![]);
        assert!(is_unshared_gated(&unshared));
        assert!(!is_unshared_persona(&unshared), "not a persona");
        assert!(may_deliver(&unshared, &author, ChannelAccess::Global, true).is_allowed());
        assert_eq!(
            may_deliver(&unshared, &pk(2), ChannelAccess::Global, true),
            Decision::Deny(DenyReason::UnsharedPersona)
        );
        // The non-canonical spelling reads as unshared here; ingest refuses it
        // before storage, so this arm is the defence in depth.
        let misspelled = ev(
            kind::KIND_TEAM_CATALOG,
            author,
            vec![vec!["shared".into(), "yes".into()]],
        );
        assert!(is_unshared_gated(&misspelled));

        let shared = ev(
            kind::KIND_TEAM_CATALOG,
            author,
            vec![vec!["shared".into(), "true".into()]],
        );
        assert!(!is_unshared_gated(&shared));
        assert!(may_deliver(&shared, &pk(2), ChannelAccess::Global, true).is_allowed());

        // A longer tag, a duplicate and a conflicting value all fail closed.
        for tags in [
            vec![vec!["shared".into(), "true".into(), "extra".into()]],
            vec![
                vec!["shared".into(), "true".into()],
                vec!["shared".into(), "true".into()],
            ],
            vec![
                vec!["shared".into(), "true".into()],
                vec!["shared".into(), "false".into()],
            ],
        ] {
            assert!(is_unshared_gated(&ev(kind::KIND_TEAM_CATALOG, author, tags)));
        }
    }

    #[test]
    fn filters_touching_author_only_kinds_must_name_the_reader() {
        let reader = pk(1);
        let mine = vec![to_hex(&reader)];
        let theirs = vec![to_hex(&pk(2))];
        let kinds = [kind::KIND_EVENT_REMINDER];

        assert!(filter_authorized(Some(&kinds), Some(&mine), None, &reader));
        assert!(!filter_authorized(
            Some(&kinds),
            Some(&theirs),
            None,
            &reader
        ));
        assert!(!filter_authorized(Some(&kinds), None, None, &reader));
        // An empty authors list constrains nothing.
        assert!(!filter_authorized(Some(&kinds), Some(&[]), None, &reader));
    }

    #[test]
    fn filters_touching_p_gated_kinds_must_constrain_p_or_author() {
        let reader = pk(1);
        let mine = vec![to_hex(&reader)];
        let kinds = [kind::KIND_GIFT_WRAP];

        assert!(filter_authorized(Some(&kinds), None, Some(&mine), &reader));
        assert!(filter_authorized(Some(&kinds), Some(&mine), None, &reader));
        assert!(!filter_authorized(Some(&kinds), None, None, &reader));
        assert!(!filter_authorized(
            Some(&kinds),
            None,
            Some(&[to_hex(&pk(7))]),
            &reader
        ));
    }

    #[test]
    fn ordinary_kinds_need_no_filter_constraint() {
        let reader = pk(1);
        assert!(filter_authorized(
            Some(&[kind::KIND_STREAM_MESSAGE]),
            None,
            None,
            &reader
        ));
        assert!(filter_authorized(None, None, None, &reader));
    }
}
