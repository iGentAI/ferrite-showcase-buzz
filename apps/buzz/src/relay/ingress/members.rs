//! Membership changes for a channel or a community.

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
use super::{CLAIM_RETRIES, MODERATORS_CONFIG, authenticate_request, storage};
use super::agents::{caller_is_operator};
use super::channels::{is_community_moderator};
use super::observability::{record_audit_or_alert};

/// The pubkey a membership-write body names.
pub(super) fn body_pubkey(body: &Value) -> Result<crate::relay::event::PubKey> {
    body.get("pubkey")
        .and_then(Value::as_str)
        .and_then(crate::relay::crypto::from_hex32)
        .ok_or_else(|| BuzzError::InvalidEvent("this needs a 32-byte hex 'pubkey'".into()))
}

/// `POST /buzz/api/members/add` — admit a pubkey to the community.
///
/// Moderator or operator. The row is the same one an invite claim materializes,
/// create-if-absent, so adding somebody who is already a member is not an error.
pub(super) async fn members_add(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    if !is_community_moderator(&mut config, community, &caller.pubkey).await?
        && !caller_is_operator(ctx, &mut config, &caller.pubkey).await?
    {
        return Err(BuzzError::Unauthorized(
            "only a community moderator or an operator may add a member".into(),
        ));
    }
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("member body: {e}")))?;
    let target = body_pubkey(&body)?;

    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let doc = serde_json::json!({
        "role": "member",
        "claimed_at_ms": now_ms,
        "source": "moderator-add",
        "added_by": crate::relay::crypto::to_hex(&caller.pubkey),
    });
    let bytes = store::encode_doc(&doc)?;
    let added = events
        .put_if_version(&keys::community_mem(community, &target), &bytes, None)
        .await
        .map_err(storage)?
        .is_ok();

    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let audit = crate::relay::audit::AuditDraft::new(
        community,
        crate::relay::audit::AuditAction::ConfigUpdated,
        now_ms,
    )
    .actor(&caller.pubkey)
    .object(crate::relay::crypto::to_hex(&target))
    .with("action", "member-add");
    record_audit_or_alert(ctx, &mut guards, community, audit, "community member add").await;

    serde_json::to_vec(&serde_json::json!({ "ok": true, "added": added }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/members/remove` — remove a pubkey from the community.
///
/// Moderator or operator, and a moderator may NOT remove another moderator:
/// that would make the moderator set mutable by its own members, which is the
/// escalation the role route exists to keep with operators.
pub(super) async fn members_remove(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    let is_operator = caller_is_operator(ctx, &mut config, &caller.pubkey).await?;
    if !is_community_moderator(&mut config, community, &caller.pubkey).await? && !is_operator {
        return Err(BuzzError::Unauthorized(
            "only a community moderator or an operator may remove a member".into(),
        ));
    }
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("member body: {e}")))?;
    let target = body_pubkey(&body)?;
    if !is_operator && is_community_moderator(&mut config, community, &target).await? {
        return Err(BuzzError::Unauthorized(
            "only an operator may remove a moderator".into(),
        ));
    }

    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    store::delete_key(&mut events, &keys::community_mem(community, &target)).await?;
    // The removal must reach a live reader's pump on its next batch, not on the
    // cache's next expiry — the same tombstone every membership removal bumps.
    // A failed write is an error, not an acknowledged removal: without the
    // tombstone a live reader keeps its stream until the cache expires. The
    // delete above and this write are both idempotent, so the caller's retry
    // finishes the removal.
    store::put_raw(
        &mut events,
        &keys::revoked(community, &target),
        now_ms.to_string().as_bytes(),
    )
    .await?;

    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let audit = crate::relay::audit::AuditDraft::new(
        community,
        crate::relay::audit::AuditAction::ConfigUpdated,
        now_ms,
    )
    .actor(&caller.pubkey)
    .object(crate::relay::crypto::to_hex(&target))
    .with("action", "member-remove");
    record_audit_or_alert(
        ctx,
        &mut guards,
        community,
        audit,
        "community member remove",
    )
    .await;

    serde_json::to_vec(&serde_json::json!({ "ok": true, "removed": true }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/members/role` — grant or revoke community moderator status.
///
/// **OPERATOR ONLY, and that is the entire reason this route can exist.** The
/// moderator set is what authorizes kinds 9040–9044, invite minting, and member
/// management; letting a moderator edit it would make `role(pk, "admin")` a way
/// for any moderator to mint more moderators, which is a privilege escalation
/// dressed as a role change. Splitting it this way is what makes the add/remove
/// routes above safe to give to moderators at all.
pub(super) async fn members_role(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    // The body names who becomes (or stops being) a moderator and is unsigned,
    // so the operator's header must commit to it: a captured header must not
    // authorize promoting somebody else.
    let (caller, _) = crate::relay::auth::authenticate_event(
        &mut config,
        community,
        super::header(req, "authorization"),
        &req.method,
        &req.path,
        req.verified_host(),
        Some(&req.body),
        now_ms,
    )
    .await?;
    if !caller_is_operator(ctx, &mut config, &caller.pubkey).await? {
        return Err(BuzzError::Unauthorized(
            "only an operator may change community moderator status".into(),
        ));
    }
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("role body: {e}")))?;
    let target = body_pubkey(&body)?;
    let role = body.get("role").and_then(Value::as_str).unwrap_or_default();
    let make_admin = match role {
        "admin" => true,
        "member" => false,
        _ => {
            return Err(BuzzError::InvalidEvent(
                "role must be \"admin\" or \"member\"".into(),
            ))
        }
    };

    let target_hex = crate::relay::crypto::to_hex(&target);
    // The moderator set is ONE document, so two operators acting at once must not
    // overwrite each other — in the worst order that reinstates a moderator who
    // had just been revoked. Same CAS-with-retry the registry mutation uses:
    // re-read the winner's document and re-decide rather than assume we lost.
    let mut changed = false;
    let mut settled = false;
    for _ in 0..CLAIM_RETRIES {
        let current = store::get_raw_versioned(&mut config, MODERATORS_CONFIG).await?;
        let (expected, mut moderators) = match &current {
            Some((bytes, version)) => (
                Some(*version),
                serde_json::from_slice::<Vec<String>>(bytes).map_err(|e| {
                    // Strict on the MUTATION path, like the registry: rewriting a
                    // document we could not read would silently drop every
                    // moderator it named.
                    BuzzError::Internal(format!("the moderator set is malformed: {e}"))
                })?,
            ),
            None => (None, Vec::new()),
        };
        let before = moderators.len();
        if make_admin {
            if !moderators.contains(&target_hex) {
                if moderators.len() >= crate::relay::operator::MAX_MODERATORS {
                    return Err(BuzzError::InvalidEvent(
                        "this community already has the maximum number of moderators".into(),
                    ));
                }
                moderators.push(target_hex.clone());
            }
        } else {
            moderators.retain(|m| m != &target_hex);
        }
        if moderators.len() == before {
            settled = true; // already in the wanted state
            break;
        }
        // Through the SAME validator the operator config route uses, so this
        // document is byte-identical in shape to the one that route produces.
        let canonical = crate::relay::operator::registry_entry("buzz-community-moderators")
            .ok_or_else(|| BuzzError::Internal("the moderator key left the table".into()))?
            .validate(&serde_json::json!(moderators))?;
        match config
            .put_if_version(MODERATORS_CONFIG, &canonical, expected)
            .await
            .map_err(storage)?
        {
            Ok(_) => {
                changed = true;
                settled = true;
                break;
            }
            Err(_) => continue,
        }
    }
    if !settled {
        return Err(BuzzError::Conflict(
            "the moderator set is contended; retry".into(),
        ));
    }

    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let audit = crate::relay::audit::AuditDraft::new(
        community,
        crate::relay::audit::AuditAction::ConfigUpdated,
        now_ms,
    )
    .actor(&caller.pubkey)
    .object(target_hex)
    .with("action", "member-role")
    .with("role", role.to_string());
    record_audit_or_alert(ctx, &mut guards, community, audit, "community member role").await;

    serde_json::to_vec(&serde_json::json!({ "ok": true, "role": role, "changed": changed }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}
