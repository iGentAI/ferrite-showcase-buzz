//! Community identity and admission: NIP-05, the join policy, invites and the
//! membership checks a write passes.

use ferrite_app::tracing;
use std::io;
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
use super::{COMMUNITY_CLOSED_CONFIG, INVITE_CLAIM_RATE_LIMIT, INVITE_CLAIM_RATE_WINDOW_MS, INVITE_KEY_CONFIG, INVITE_KEY_SECRET, JOIN_POLICY_CONFIG, PROCESS_CELLS_CONFIG, authenticate_request, query_param, storage};
use super::channels::{is_community_moderator};

/// `GET /buzz/api/archived-identities` — the identities this community has
/// retired (NIP-IA).
///
/// Members only, like the roster and for the same reason: who has left is a fact
/// about the community, not something to publish to the open internet.
pub(super) async fn archived_identities_route(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    enforce_read_standing(ctx, community, &caller.pubkey, now_ms).await?;

    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let (archived, total) =
        crate::relay::archive::archived_identities(&mut events, community).await?;
    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "archived": archived,
        "complete": total == archived.len(),
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `GET /buzz/api/nip05?name=<local>` — NIP-05 name resolution (public).
///
/// Returns `{ "names": { "<local>": "<hex pubkey>" } }` for a name the operator
/// has registered in the `buzz-nip05-names` config map (a JSON object of
/// `local → hex`). An unknown or missing name returns an empty `names` object,
/// exactly as NIP-05 specifies — never an error a client would have to special-
/// case, and never a signal that distinguishes "unregistered" from "malformed".
pub(super) async fn nip05(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let name = query_param(&req.path, "name");
    let mut out = serde_json::Map::new();
    if let Some(name) = name {
        let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
        if let Ok(Some(bytes)) = config.get(b"buzz-nip05-names").await {
            if let Ok(map) =
                serde_json::from_slice::<std::collections::BTreeMap<String, String>>(&bytes)
            {
                if let Some(pubkey) = map.get(&name) {
                    out.insert(name, Value::String(pubkey.clone()));
                }
            }
        }
    }
    serde_json::to_vec(&serde_json::json!({ "names": out }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// The invite HMAC key: derived from the injected secret when present, else
/// from the operator-config value (≥32 bytes). Absent both ⇒ invites are
/// unavailable — a guessable key would make every invite forgeable.
pub(super) async fn invite_key(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
) -> Result<[u8; 32]> {
    if let Some(secret) = ctx.secret(INVITE_KEY_SECRET) {
        return Ok(crate::relay::invites::derive_invite_key(secret.as_bytes()));
    }
    match config.get(INVITE_KEY_CONFIG).await {
        Ok(Some(material)) if material.len() >= 32 => {
            Ok(crate::relay::invites::derive_invite_key(&material))
        }
        Ok(_) => Err(BuzzError::Internal(
            "the invite key is not configured; invites are unavailable".into(),
        )),
        Err(e) => Err(BuzzError::Storage(format!("invite key read: {e}"))),
    }
}

/// The operator join-policy document, if configured.
pub(super) async fn join_policy_doc(
    config: &mut ferrite_app::client::StoreClusterClient,
) -> Result<Option<Value>> {
    match config.get(JOIN_POLICY_CONFIG).await {
        Ok(Some(bytes)) => serde_json::from_slice::<Value>(&bytes)
            .map(Some)
            .map_err(|e| BuzzError::Internal(format!("the join-policy document is not JSON: {e}"))),
        Ok(None) => Ok(None),
        Err(e) => Err(BuzzError::Storage(format!("join policy read: {e}"))),
    }
}

/// `GET /buzz/api/join-policy` — the public join policy every client-side
/// join surface shares (buzz's shape: `{}` when none is configured).
pub(super) async fn join_policy(ctx: &AppContext) -> Result<Vec<u8>> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let body = match join_policy_doc(&mut config).await? {
        Some(policy) => serde_json::json!({ "policy": policy }),
        None => serde_json::json!({}),
    };
    serde_json::to_vec(&body).map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `GET /buzz/api/me` — what the authenticated caller may do in this community.
///
/// The read a client needs and did not have. Every moderation affordance in
/// `buzz-web` is gated on a NIP-43 kind-13534 membership snapshot that this relay
/// does not publish and will not (see the 9030–9032 refusal in [`accept_event`]),
/// so the moderator UI was unreachable even for a real moderator. Rather than mint
/// a relay-signed snapshot of a member registry that does not exist, the relay
/// answers the question directly, about the caller and only the caller.
///
/// **Answers about ONE identity: the one the request authenticated as.** There is
/// no target parameter, by design — an endpoint that reported a third party's
/// sanctions would be a probe oracle for "who is banned here", which is exactly
/// the disclosure the `restricted: ` token is kept coarse to avoid.
///
/// The wire shape:
///
/// ```json
/// {
///   "pubkey": "<64-hex, the identity the relay authenticated>",
///   "moderator": true,
///   "role": "admin",
///   "membership_required": false,
///   "member": true,
///   "restriction": { "banned": false, "banned_until": null, "timed_out_until": null }
/// }
/// ```
///
/// * `moderator` is the relay's own vocabulary: membership of the
///   operator-provisioned `buzz-community-moderators` set, which is what actually
///   authorizes kinds 9040–9044 and invite minting.
/// * `role` restates it in the client's `RelayMemberRole` vocabulary so
///   `canManageCommunityMembers` works without the client inventing a mapping. It
///   is `"admin"` or `"member"` and **never `"owner"`** — this relay has no owner
///   concept, and reporting one would be a lie a client could act on.
/// * `member` is not a second opinion: it IS the closed-community write gate
///   ([`enforce_community_membership`]), asked without writing anything. An open
///   community answers `true` for anyone who authenticated.
/// * `restriction` carries the caller's own sanctions with expiries in epoch
///   SECONDS — the same units the refusal sentence uses. A client may still learn
///   a timeout reactively from a refused send (that is the documented v1 ruling);
///   this lets it also know BEFORE spending a doomed write.
pub(super) async fn me(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;

    let moderator = is_community_moderator(&mut config, community, &caller.pubkey).await?;
    let membership_required = community_is_closed(&mut config).await?;
    // Asked through the gate itself, so this can never disagree with what a write
    // would do. A STORAGE failure propagates rather than reading as "not a
    // member": answering `false` on a failed read would tell a member they had
    // been thrown out.
    let member = match enforce_community_membership(ctx, community, &caller.pubkey).await {
        Ok(()) => true,
        Err(BuzzError::Unauthorized(_)) => false,
        Err(e) => return Err(e),
    };

    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let restriction = crate::relay::effects::restriction_of(&mut events, community, &caller.pubkey)
        .await?
        .unwrap_or_default();
    // Reported only while LIVE: a lapsed sanction is not a sanction, and the
    // stored document can outlive its own expiry (see `apply_moderation`).
    let block = restriction.write_block(now_ms);
    let banned = matches!(
        block,
        Some(crate::relay::effects::WriteBlock::Banned { .. })
    );
    let banned_until = match block {
        Some(crate::relay::effects::WriteBlock::Banned { until_ms }) => {
            until_ms.map(|ms| ms.div_ceil(1000))
        }
        _ => None,
    };
    // Read off the document rather than off `block`, because a live ban outranks a
    // live timeout in the refusal and the timeout is still a fact the member is
    // entitled to: an unbanned-but-timed-out member must not see the block vanish.
    let timed_out_until = restriction
        .timeout_until
        .filter(|until| now_ms < *until)
        .map(|ms| ms.div_ceil(1000));

    serde_json::to_vec(&serde_json::json!({
        "pubkey": crate::relay::crypto::to_hex(&caller.pubkey),
        "moderator": moderator,
        "role": if moderator { "admin" } else { "member" },
        "membership_required": membership_required,
        "member": member,
        "restriction": {
            "banned": banned,
            "banned_until": banned_until,
            "timed_out_until": timed_out_until,
        },
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/invites` — mint a stateless invite code.
///
/// buzz gates minting on the community owner/admin roles; this port's
/// community-authority surface is the operator-seeded moderator set (the
/// moderation precedent), fail-closed: no moderators configured ⇒ no one
/// mints.
pub(super) async fn mint_invite(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    if !is_community_moderator(&mut config, community, &caller.pubkey).await? {
        return Err(BuzzError::Unauthorized(
            "only a community moderator may create invites".into(),
        ));
    }
    let ttl_secs = serde_json::from_slice::<Value>(&req.body)
        .ok()
        .and_then(|v| v.get("ttl_secs").and_then(Value::as_u64))
        .unwrap_or(crate::relay::invites::DEFAULT_INVITE_TTL_SECS);
    let key = invite_key(ctx, &mut config).await?;
    // The nonce seed: minter, instant, and the gateway's per-request id — so
    // identically-parameterised invites differ per request (see the module
    // doc's no-entropy divergence note).
    let mut seed = caller.pubkey.to_vec();
    seed.extend_from_slice(&now_ms.to_be_bytes());
    seed.extend_from_slice(req.correlation_id.as_bytes());
    let (code, expires_at) =
        crate::relay::invites::mint_invite(&key, community, ttl_secs, now_ms / 1000, &seed)?;
    serde_json::to_vec(&serde_json::json!({ "ok": true, "code": code, "expires_at": expires_at }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/invites/claim` — claim an invite code.
///
/// Signed by the JOINING pubkey and deliberately exempt from any membership
/// gate: the whole point is that the caller is not a member yet. NIP-98
/// proves control of the joining key; the HMAC proves an authority approved
/// the join. Claims are idempotent and rate-limited per pubkey.
pub(super) async fn claim_invite(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;

    // Bound brute-force probing (buzz's 10/min fixed window). Fails OPEN on
    // an ambiguous increment like the message tier — the code's 256-bit MAC,
    // not the limiter, is what makes forging infeasible.
    let rate_key = keys::rate(community, &caller.pubkey, "invite");
    match store::bump_expiring_counter(
        &mut config,
        &rate_key,
        1,
        now_ms.saturating_add(INVITE_CLAIM_RATE_WINDOW_MS),
    )
    .await
    {
        Ok(count) if count > INVITE_CLAIM_RATE_LIMIT => return Err(BuzzError::RateLimited),
        Ok(_) => {}
        Err(e) => tracing::warn!("invite claim limiter failed open: {e}"),
    }

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("claim body: {e}")))?;
    let code = body
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("claim needs a code".into()))?;

    let key = invite_key(ctx, &mut config).await?;
    let payload = crate::relay::invites::verify_invite(&key, community, code, now_ms / 1000)
        .map_err(|e| e.refusal())?;

    // A configured join policy makes the acceptance receipt mandatory, bound
    // to THIS code and the CURRENT policy version.
    let mut accepted_policy_version: Option<String> = None;
    if let Some(policy) = join_policy_doc(&mut config).await? {
        let version = policy
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let receipt = body
            .get("policy_receipt")
            .and_then(Value::as_str)
            .ok_or_else(|| BuzzError::Unauthorized("join_policy_required".into()))?;
        crate::relay::invites::verify_policy_acceptance(
            &key,
            receipt,
            code,
            version,
            now_ms / 1000,
        )
        .map_err(|_| BuzzError::Unauthorized("join_policy_required".into()))?;
        accepted_policy_version = Some(version.to_string());
    }

    // Materialize the community membership, exactly once (create-if-absent —
    // a re-claim is `already_member`, never an error).
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let mem_key = keys::community_mem(community, &caller.pubkey);
    let doc = serde_json::json!({
        "role": payload.r,
        "claimed_at_ms": now_ms,
        "policy_version": accepted_policy_version,
    });
    let doc_bytes = serde_json::to_vec(&doc)
        .map_err(|e| BuzzError::Internal(format!("membership encode: {e}")))?;
    let status = claim_status(events.put_if_version(&mem_key, &doc_bytes, None).await)?;

    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "status": status,
        "community": community,
        "role": payload.r,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// The answer to an invite claim from the create-if-absent write of the
/// membership row. Only a confirmed version conflict means the row was already
/// there; a storage failure says nothing about membership and is an error, so a
/// claimant is never told `already_member` when no row exists.
pub(super) fn claim_status<T, C>(outcome: io::Result<std::result::Result<T, C>>) -> Result<&'static str> {
    match outcome.map_err(storage)? {
        Ok(_) => Ok("joined"),
        Err(_) => Ok("already_member"),
    }
}

/// `POST /buzz/api/invites/accept-policy` — exchange explicit policy
/// acceptance for a short-lived, invite-bound receipt. Unauthenticated by
/// design (buzz's shape): it runs BEFORE the user has any standing, and a
/// receipt is worthless without a valid invite code to claim with.
pub(super) async fn accept_policy(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let Some(policy) = join_policy_doc(&mut config).await? else {
        return Err(BuzzError::NotFound("no join policy is configured".into()));
    };
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("acceptance body: {e}")))?;
    let code = body
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("acceptance needs the invite code".into()))?;
    let version = policy
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let accepted_version = body.get("policy_version").and_then(Value::as_str);
    let age_required = policy
        .get("age_attestation_required")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let age_confirmed = body
        .get("age_confirmed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if accepted_version != Some(version) || (age_required && !age_confirmed) {
        return Err(BuzzError::InvalidEvent("join_policy_not_accepted".into()));
    }
    let key = invite_key(ctx, &mut config).await?;
    let receipt =
        crate::relay::invites::mint_policy_acceptance(&key, code, version, now_ms / 1000)?;
    serde_json::to_vec(&serde_json::json!({ "ok": true, "receipt": receipt }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// The opt-in closed-community write gate: when the operator
/// sets `buzz-community-closed = "true"`, an event write requires the
/// community membership an invite claim materializes. Fail-closed on the
/// membership read; ABSENT config ⇒ open. buzz's
/// relay-membership gate also covers reads; the port's v1 gates writes and
/// records the difference in the plan.
pub(super) async fn enforce_community_membership(
    ctx: &AppContext,
    community: &str,
    actor: &crate::relay::event::PubKey,
) -> Result<()> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    if !community_is_closed(&mut config).await? {
        return Ok(());
    }
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let member = events
        .get(&keys::community_mem(community, actor))
        .await
        .map_err(|e| BuzzError::Storage(format!("community membership read: {e}")))?;
    if member.is_none() {
        // No `restricted: ` here: `BuzzError`'s Display adds the wire token, and
        // spelling it again put `restricted: restricted: …` in front of a user.
        return Err(BuzzError::Unauthorized(
            "this community is invite-only".into(),
        ));
    }
    Ok(())
}

/// Community read-side standing gate: enforces closed-community membership
/// (when `buzz-community-closed` is active) and active community bans
/// (`Restriction::ban_blocks_access`). Timeouts and identity archives block
/// writes only and do not revoke read access.
pub(crate) async fn enforce_read_standing(
    ctx: &AppContext,
    community: &str,
    actor: &crate::relay::event::PubKey,
    now_ms: u64,
) -> Result<()> {
    enforce_community_membership(ctx, community, actor).await?;
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    if let Some(restriction) =
        crate::relay::effects::restriction_of(&mut events, community, actor).await?
    {
        if let Some(block @ crate::relay::effects::WriteBlock::Banned { .. }) =
            restriction.write_block(now_ms)
        {
            return Err(BuzzError::Unauthorized(block.refusal_detail()));
        }
    }
    Ok(())
}

/// Whether the operator has closed this community (`buzz-community-closed`).
///
/// Absent or anything but the exact string `true` ⇒ OPEN, byte-identical to the
/// behaviour before the flag existed. An unreadable flag is a storage error: a
/// config-plane blip must not open a closed community to non-members.
pub(super) async fn community_is_closed(
    config: &mut ferrite_app::client::StoreClusterClient,
) -> Result<bool> {
    closed_flag(config.get(COMMUNITY_CLOSED_CONFIG).await)
}

pub(super) fn closed_flag(read: io::Result<Option<Vec<u8>>>) -> Result<bool> {
    let value = read.map_err(storage)?;
    Ok(matches!(value, Some(v) if String::from_utf8_lossy(&v).trim() == "true"))
}

/// Plan 031 Contract D: only an operator-registered process cell may author a
/// `KIND_PROCESS_RESULT`.
///
/// A result asserts "the platform performed this RCS action", so its authority
/// comes from WHO signed it — and unlike the community-closed flag above, this
/// read fails CLOSED in every direction: an absent document refuses the kind
/// outright (a workspace with no registered cell cannot receive forged
/// results), and a malformed document refuses rather than serving an empty
/// list that would be indistinguishable from "no cells". The refusal happens
/// in [`accept_event`] before admission, so a forged result stores nothing.
pub(super) async fn enforce_process_cell_authorship(
    ctx: &AppContext,
    author: &crate::relay::event::PubKey,
) -> Result<()> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let bytes = config
        .get(PROCESS_CELLS_CONFIG)
        .await
        .map_err(|e| BuzzError::Storage(format!("buzz-process-cells read: {e}")))?;
    let Some(bytes) = bytes else {
        return Err(BuzzError::Unauthorized(format!(
            "kind {} is process-cell-signed and no process cell is registered \
             (buzz-process-cells is not configured)",
            crate::relay::kind::KIND_PROCESS_RESULT
        )));
    };
    // The SHARED strict reader, not a local `Vec<String>` decode: a document
    // like `["<valid>", "junk"]` must refuse the kind outright rather than
    // authorize its valid entries, and the cap must hold against a document
    // seeded outside the operator route. One parser means the write boundary
    // and this gate cannot disagree about what the registry IS.
    let cells = crate::relay::operator::parse_process_cells(&bytes).map_err(|e| {
        BuzzError::Unauthorized(format!(
            "kind {} is refused: {e}",
            crate::relay::kind::KIND_PROCESS_RESULT
        ))
    })?;
    if !cells.contains(author) {
        return Err(BuzzError::Unauthorized(format!(
            "kind {} is process-cell-signed and this key is not a registered \
             process cell",
            crate::relay::kind::KIND_PROCESS_RESULT
        )));
    }
    Ok(())
}
