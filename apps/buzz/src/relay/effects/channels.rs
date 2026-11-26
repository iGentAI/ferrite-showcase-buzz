//! Channel edits and deletion, and the rules that authorize membership commands
//! and channel changes.

use ferrite_app::client::ApeClusterClient;
use crate::relay::channel::AddPolicy;
use crate::relay::channel::Channel;
use crate::relay::channel::Role;
use crate::relay::channel::Visibility;
use crate::relay::error::BuzzError;
use crate::relay::error::Result;
use crate::relay::event::Event;
use crate::relay::event::PubKey;
use crate::relay::kind::Kind;
use crate::relay::kind;
use super::membership::mutate_channel;

/// Apply a channel-metadata edit (9002) as one atomic compare-and-swap on the
/// channel document. buzz applies each 9002 tag as a *separate* SQL statement in
/// tag order (a documented non-atomicity gotcha where `ttl` + `archived:false`
/// interact); one CAS here is a deliberate improvement — every field of a single
/// edit lands together or not at all. `updated_at`/`seq` advance once.
///
/// `canvas` is intentionally NOT a 9002 field: buzz's canvas is kind 40100, an
/// ordinary channel event that is stored and delivered, not a channel-doc setter
/// (buzz's `set_canvas` has no caller). So an agent's canvas write flows through
/// the normal event path, unchanged.
pub(super) async fn apply_channel_edit(
    ape: &mut ApeClusterClient,
    community: &str,
    channel: &str,
    event: &Event,
    now_ms: u64,
) -> Result<()> {
    // Snapshot the tag values (borrowed from the event) before the CAS closure,
    // which must be `FnMut`.
    let name = event.first_tag("name").map(canonical_channel_name);
    let about = event.first_tag("about").map(str::to_string);
    let topic = event.first_tag("topic").map(str::to_string);
    let purpose = event.first_tag("purpose").map(str::to_string);
    let visibility = event.first_tag("visibility").and_then(Visibility::parse);
    let archived = event.first_tag("archived").map(|v| v == "true");
    // `Some(None)` = clear (permanent); `Some(Some(n))` = set; `None` = untouched.
    let ttl: Option<Option<u64>> = event.first_tag("ttl").map(|v| {
        if v.is_empty() {
            None
        } else {
            v.parse::<u64>().ok()
        }
    });

    mutate_channel(ape, community, channel, |doc| {
        if doc.channel_type == crate::relay::channel::ChannelType::Dm {
            return false;
        }
        let mut changed = false;
        if let Some(name) = &name {
            doc.name = name.clone();
            changed = true;
        }
        if let Some(about) = &about {
            doc.description = (!about.is_empty()).then(|| about.clone());
            changed = true;
        }
        if let Some(topic) = &topic {
            doc.topic = (!topic.is_empty()).then(|| topic.clone());
            changed = true;
        }
        if let Some(purpose) = &purpose {
            doc.purpose = (!purpose.is_empty()).then(|| purpose.clone());
            changed = true;
        }
        if let Some(visibility) = visibility {
            doc.visibility = visibility;
            changed = true;
        }
        if let Some(ttl) = &ttl {
            doc.ttl_seconds = *ttl;
            // Re-arm the deadline from now unless the channel is (staying)
            // archived; a cleared ttl clears the deadline.
            doc.ttl_deadline = match *ttl {
                Some(secs) if archived != Some(true) => {
                    Some(now_ms.saturating_add(secs.saturating_mul(1000)))
                }
                _ => None,
            };
            changed = true;
        }
        if let Some(archive) = archived {
            if archive {
                doc.archived_at = Some(now_ms);
            } else {
                doc.archived_at = None;
                // Unarchive re-arms the ttl clock from now (buzz's rule).
                if let Some(secs) = doc.ttl_seconds {
                    doc.ttl_deadline = Some(now_ms.saturating_add(secs.saturating_mul(1000)));
                }
            }
            changed = true;
        }
        if changed {
            doc.updated_at = now_ms;
            doc.seq = doc.seq.saturating_add(1);
        }
        changed
    })
    .await
}

/// Apply a channel deletion (9008) as a durable tombstone.
///
/// History is deliberately not physically erased: `deleted_at` makes the room
/// invisible to every read and unwritable through [`Channel::is_writable`],
/// while retaining the document and event log for audit/recovery. A repeated
/// delivery is an idempotent no-op.
pub(super) async fn apply_channel_delete(
    ape: &mut ApeClusterClient,
    community: &str,
    channel: &str,
    now_ms: u64,
) -> Result<()> {
    mutate_channel(ape, community, channel, |doc| {
        if doc.channel_type == crate::relay::channel::ChannelType::Dm || doc.deleted_at.is_some() {
            return false;
        }
        doc.deleted_at = Some(now_ms);
        doc.ttl_deadline = None;
        doc.updated_at = now_ms;
        doc.seq = doc.seq.saturating_add(1);
        true
    })
    .await
}

/// Pre-storage authorization for a membership command (9000/9001/9021/9022).
///
/// Pure by design so it is exhaustively unit-testable: the caller
/// ([`crate::relay::ingress`]) reads the channel document, the actor's live role, and
/// the owner set from the planes and passes them in; this decides accept/reject.
/// It runs *before* admission, so a rejected command is never stored — buzz's
/// `validate_admin_event` shape.
///
/// `actor_role` is the actor's role if they are a *live* member, else `None`.
/// `owner_count` is `channel.owners.len()`; `actor_is_owner` / `target_is_owner`
/// are membership in `channel.owners`. `target`/`requested_role` come from the
/// event's `p` and `role` tags.
///
/// **Deliberate divergence from buzz:** demoting or removing the *last* owner is
/// refused here. buzz guards only removal (an ownerless channel there is
/// recoverable by an operator); in this port an ownerless room has no operator to
/// recover it, so the guard also covers a `PUT_USER` that would demote the sole
/// owner.
///
/// **Owners outrank admins.** Only an owner may remove another owner, change an
/// owner's role, or grant ownership. Otherwise an admin could remove
/// a non-last owner, or demote one and then remove them, or grant themselves
/// ownership first. The one exception is a room that has no owner at all, where
/// an admin may grant ownership so the room can be recovered.
#[allow(clippy::too_many_arguments)]
pub fn authorize_membership_command(
    kind: Kind,
    channel: &Channel,
    actor: &PubKey,
    actor_role: Option<Role>,
    actor_is_owner: bool,
    target: Option<&PubKey>,
    target_is_owner: bool,
    requested_role: Option<Role>,
    owner_count: usize,
) -> Result<()> {
    if channel.channel_type == crate::relay::channel::ChannelType::Dm {
        return Err(BuzzError::Unauthorized(
            "direct message membership is immutable".into(),
        ));
    }
    let actor_is_moderator = actor_role.is_some_and(Role::is_moderator);
    match kind {
        kind::KIND_NIP29_JOIN_REQUEST => {
            // Open channels only; a private room is joined by invitation (a
            // member's PUT_USER), never by self-request. buzz's ingest pre-check.
            if channel.visibility.is_private() {
                // No `restricted: ` here — `BuzzError`'s Display adds the token,
                // and spelling it twice put `restricted: restricted: …` on the
                // wire, which is not a token any client branches on.
                return Err(BuzzError::Unauthorized("channel is private".into()));
            }
            Ok(())
        }
        kind::KIND_NIP29_PUT_USER => {
            let Some(target) = target else {
                return Err(BuzzError::InvalidEvent(
                    "membership command must name a target via a p tag".into(),
                ));
            };
            let Some(role) = requested_role else {
                return Err(BuzzError::InvalidEvent("unknown role".into()));
            };
            // Forbid demoting the last owner (divergence — see the doc above).
            if target_is_owner && role != Role::Owner && owner_count <= 1 {
                return Err(BuzzError::Unauthorized(
                    "cannot demote the last owner — transfer ownership first".into(),
                ));
            }
            // The elevated-role gate runs BEFORE the self short-circuit: you may
            // not grant yourself (or anyone) a role you do not already hold. A
            // self-targeted grant is otherwise a full channel takeover — anyone,
            // even a non-member of an open channel, could name their own pubkey
            // with role=owner and become an owner. (Found by adversarial review.)
            if role.is_moderator() && !actor_is_moderator {
                return Err(BuzzError::Unauthorized(
                    "only owners/admins may grant elevated roles".into(),
                ));
            }
            // Owners outrank admins (see the doc above). Like the elevated-role
            // gate, this runs before the self short-circuit.
            if target_is_owner && !actor_is_owner {
                return Err(BuzzError::Unauthorized(
                    "only an owner may change another owner's role".into(),
                ));
            }
            if role == Role::Owner && !actor_is_owner && owner_count > 0 {
                return Err(BuzzError::Unauthorized(
                    "only an owner may grant ownership".into(),
                ));
            }
            if channel.visibility.is_private() && actor_role.is_none() {
                return Err(BuzzError::Unauthorized(
                    "not a member of this channel".into(),
                ));
            }
            // A self-add at a non-elevated role short-circuits the add-policy
            // check — the benign self-join buzz allows on an open channel (or
            // an idempotent self-upsert by a live member of a private channel).
            if target == actor {
                return Ok(());
            }
            match channel.add_policy {
                AddPolicy::Anyone => Ok(()),
                AddPolicy::OwnerOnly if actor_is_moderator => Ok(()),
                AddPolicy::OwnerOnly => Err(BuzzError::Unauthorized(
                    "only owners/admins may add members to this channel".into(),
                )),
                AddPolicy::Nobody => Err(BuzzError::Unauthorized(
                    "this channel does not accept new members".into(),
                )),
            }
        }
        kind::KIND_NIP29_REMOVE_USER => {
            let Some(target) = target else {
                return Err(BuzzError::InvalidEvent(
                    "membership command must name a target via a p tag".into(),
                ));
            };
            let is_self = target == actor;
            if !is_self {
                // A non-member cannot remove anyone — "you must be in the channel
                // to remove anyone, even your own bot" (buzz).
                if actor_role.is_none() {
                    return Err(BuzzError::Unauthorized(
                        "not a member of this channel".into(),
                    ));
                }
                if !actor_is_moderator {
                    return Err(BuzzError::Unauthorized(
                        "only owners/admins may remove other members".into(),
                    ));
                }
                if target_is_owner && !actor_is_owner {
                    return Err(BuzzError::Unauthorized(
                        "only an owner may remove another owner".into(),
                    ));
                }
            }
            if target_is_owner && owner_count <= 1 {
                return Err(BuzzError::Unauthorized(
                    "cannot remove the last owner — transfer ownership first".into(),
                ));
            }
            Ok(())
        }
        kind::KIND_NIP29_LEAVE_REQUEST => {
            // Self-remove twin; the only guard is the last-owner one.
            if actor_is_owner && owner_count <= 1 {
                return Err(BuzzError::Unauthorized(
                    "cannot remove the last owner — transfer ownership first".into(),
                ));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// buzz's `canonical_channel_name`: strip every leading `#` and whitespace, then
/// trim trailing whitespace. An internal `#` is preserved (`"a#b"` stays `"a#b"`).
#[must_use]
pub fn canonical_channel_name(name: &str) -> String {
    name.trim_start_matches(|c: char| c == '#' || c.is_whitespace())
        .trim_end()
        .to_string()
}

/// The 9002 tags that only a moderator (owner/admin) may set.
pub(super) const PRIVILEGED_EDIT_TAGS: &[&str] = &["name", "about", "archived", "visibility", "ttl"];
/// The 9002 tags any active member may set.
pub(super) const MEMBER_EDIT_TAGS: &[&str] = &["topic", "purpose"];

/// Pre-storage authorization + validation for a channel-metadata edit (9002).
///
/// Pure, like [`authorize_membership_command`]. `actor_role` is the actor's live
/// role (`None` if not a live member). Rules (buzz's `validate_admin_event`):
///
/// * at least one recognized tag must be present;
/// * `archived` must be exactly `"true"`/`"false"`, `name` non-empty after
///   canonicalization, `visibility` ∈ {open, private}, `ttl` either `""` (clear)
///   or a positive integer;
/// * a privileged tag requires owner/admin; the two member tags (`topic`,
///   `purpose`) require only an active membership;
/// * an archived channel refuses every edit *except* one carrying
///   `["archived","false"]` (the unarchive).
pub fn authorize_channel_edit(
    channel: &Channel,
    actor_role: Option<Role>,
    event: &Event,
) -> Result<()> {
    if channel.channel_type == crate::relay::channel::ChannelType::Dm {
        return Err(BuzzError::Unauthorized(
            "direct message channels cannot be edited".into(),
        ));
    }
    let present: Vec<&str> = event
        .tags
        .iter()
        .filter_map(|t| t.first().map(String::as_str))
        .filter(|name| PRIVILEGED_EDIT_TAGS.contains(name) || MEMBER_EDIT_TAGS.contains(name))
        .collect();
    if present.is_empty() {
        return Err(BuzzError::InvalidEvent(
            "channel edit names no recognized field".into(),
        ));
    }

    // Per-tag value validation.
    if let Some(v) = event.first_tag("archived") {
        if v != "true" && v != "false" {
            return Err(BuzzError::InvalidEvent(
                "archived must be \"true\" or \"false\"".into(),
            ));
        }
    } else if event.has_tag("archived") {
        return Err(BuzzError::InvalidEvent("archived needs a value".into()));
    }
    if let Some(v) = event.first_tag("name") {
        if canonical_channel_name(v).is_empty() {
            return Err(BuzzError::InvalidEvent("channel name is required".into()));
        }
    }
    if let Some(v) = event.first_tag("visibility") {
        if Visibility::parse(v).is_none() {
            return Err(BuzzError::InvalidEvent("invalid visibility".into()));
        }
    }
    if let Some(v) = event.first_tag("ttl") {
        if !v.is_empty() && v.parse::<i64>().ok().filter(|n| *n > 0).is_none() {
            return Err(BuzzError::InvalidEvent(
                "ttl must be empty (clear) or a positive integer".into(),
            ));
        }
    } else if event.has_tag("ttl") {
        return Err(BuzzError::InvalidEvent("ttl needs a value".into()));
    }

    // The archived gate: only an unarchive may touch an archived room.
    let is_unarchive = event.first_tag("archived") == Some("false");
    if channel.archived_at.is_some() && !is_unarchive {
        return Err(BuzzError::Unauthorized("channel is archived".into()));
    }
    if channel.deleted_at.is_some() {
        return Err(BuzzError::Unauthorized("channel is deleted".into()));
    }

    // Authorization: any privileged tag ⇒ owner/admin; otherwise (only member
    // tags present) an active membership suffices.
    let touches_privileged = present.iter().any(|t| PRIVILEGED_EDIT_TAGS.contains(t));
    if touches_privileged {
        if !actor_role.is_some_and(Role::is_moderator) {
            return Err(BuzzError::Unauthorized(
                "only owners/admins may change this channel setting".into(),
            ));
        }
    } else if actor_role.is_none() {
        return Err(BuzzError::Unauthorized(
            "not a member of this channel".into(),
        ));
    }
    Ok(())
}

/// Pre-storage authorization for deleting a channel (9008).
///
/// A live owner/admin of the room may delete it, and so may a configured
/// community moderator or an operator of the deployment
/// (`actor_is_community_authority`). The second clause exists because a room's
/// owners can all be gone — a departed colleague, or a throwaway identity that
/// created it — and "an ownerless room has no operator to recover it" (see
/// [`authorize_membership_command`]) would otherwise leave it in the community
/// forever. An ordinary member is still refused. Unlike an ordinary write,
/// deletion is allowed while archived so an owner is never forced to reopen a
/// room merely to remove it. An already deleted room fails closed rather than
/// accepting a command that cannot change state.
pub fn authorize_channel_delete(
    channel: &Channel,
    actor_role: Option<Role>,
    actor_is_community_authority: bool,
) -> Result<()> {
    if channel.channel_type == crate::relay::channel::ChannelType::Dm {
        return Err(BuzzError::Unauthorized(
            "direct message channels cannot be deleted".into(),
        ));
    }
    if channel.deleted_at.is_some() {
        return Err(BuzzError::Unauthorized("channel is deleted".into()));
    }
    if !actor_role.is_some_and(Role::is_moderator) && !actor_is_community_authority {
        return Err(BuzzError::Unauthorized(
            "only owners/admins, community moderators or operators may delete this channel"
                .into(),
        ));
    }
    Ok(())
}
