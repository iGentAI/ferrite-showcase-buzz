//! `POST /buzz/api/agents/approve` and `POST /buzz/api/agents/revoke`: approve
//! one gated tool on one thread, and take that approval back.
//!
//! A persona definition can require a human approval before a tool runs. The
//! turn pipeline has always honoured the gate; what was missing was any way to
//! pass it, so a gated tool could never run. These routes are that way.
//!
//! # Who may approve or revoke
//!
//! The persona's OWNER (the member who created it) or a workspace OPERATOR,
//! for both actions. This is narrower on purpose than the rule for pausing or
//! reconfiguring a persona, which also admits community moderators. An approval
//! lets the persona act with its own access in a way its owner deliberately
//! withheld, so it is the owner's call, with operators as the backstop for a
//! persona whose owner is gone.
//!
//! # What a grant covers
//!
//! One tool, on one thread, for the life of that thread until it is revoked
//! (the owner's decision: no time expiry, and a revoke action). Both actions are
//! recorded as session events in the thread's log
//! ([`crate::agents::approvals`]), which the next turn reads at its start; a
//! turn already running is not affected. After a revoke the tool is back to
//! `approval_required`. A tool the persona's definition does not gate is
//! refused on approve rather than recorded; a revoke is accepted for any tool,
//! and reports `"revoked": false` when there was no live approval to revoke.
//!
//! Body (both routes): `{ "agent", "tool", "channel" | "thread" }`. `channel`
//! names a buzz channel (its persona thread is derived exactly as a mention's
//! is), `thread` names a session thread directly (an ACP session's `acp-…`
//! thread).

use ferrite_app::serde_json::{self, Value};
use ferrite_app::{tracing, AppContext, IngressRequest};

use crate::agents::approvals::{grant_tool_approval, revoke_tool_approval, ApprovalError};
use crate::relay::{keys, planes, store, BuzzError, Result, DEFAULT_COMMUNITY};

/// An authorized approve or revoke request: who asked, and what it names.
struct Decision {
    agent: String,
    tool: String,
    thread: String,
    /// The caller's pubkey, hex.
    by: String,
}

/// Authenticate the caller, parse the body, and check the caller is the
/// persona's owner or an operator. Shared by both routes, so they can never
/// disagree about who may act.
async fn authorize(ctx: &AppContext, req: &IngressRequest, action: &str) -> Result<Decision> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config =
        planes::store(ctx, store::CONFIG).map_err(crate::relay::ingress::storage_error)?;
    let caller = crate::relay::ingress::authenticate_request(&mut config, community, req, now_ms).await?;

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("{action} body: {e}")))?;
    let agent = identifier(&body, "agent")?;
    let tool = identifier(&body, "tool")?;
    let thread = match (
        body.get("channel").and_then(Value::as_str).map(str::trim),
        body.get("thread").and_then(Value::as_str).map(str::trim),
    ) {
        (Some(channel), None) if !channel.is_empty() => {
            crate::relay::agents::dispatch_thread(channel)
        }
        (None, Some(thread)) if !thread.is_empty() => thread.to_string(),
        _ => {
            return Err(BuzzError::InvalidEvent(
                "name exactly one of 'channel' or 'thread'".into(),
            ))
        }
    };

    let is_operator = crate::relay::ingress::is_operator(ctx, &mut config, &caller.pubkey).await?;
    if !is_operator {
        if !is_agent_owner(&mut config, community, &agent, &caller.pubkey).await? {
            return Err(BuzzError::Unauthorized(format!(
                "only {agent:?}'s owner or an operator may {action} its tools"
            )));
        }
        crate::relay::ingress::write_gate(ctx, community, &caller.pubkey, now_ms).await?;
    }
    Ok(Decision {
        agent,
        tool,
        thread,
        by: crate::relay::crypto::to_hex(&caller.pubkey),
    })
}

fn approval_error(e: ApprovalError) -> BuzzError {
    match e {
        ApprovalError::NotGated(m)
        | ApprovalError::InvalidScope(m)
        | ApprovalError::TooManyCycles(m) => BuzzError::InvalidEvent(m),
        ApprovalError::Plane(m) => BuzzError::Storage(m),
    }
}

/// Handle `POST /buzz/api/agents/approve`.
pub(crate) async fn approve_tool(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let d = authorize(ctx, req, "approve").await?;
    let seq = grant_tool_approval(ctx, &d.agent, &d.thread, &d.tool)
        .await
        .map_err(approval_error)?;
    tracing::info!(
        "approval granted: agent={} tool={} thread={} approved_by={} seq={seq}",
        d.agent,
        d.tool,
        d.thread,
        d.by
    );
    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "agent": d.agent,
        "tool": d.tool,
        "thread": d.thread,
        "approved_by": d.by,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// Handle `POST /buzz/api/agents/revoke`.
pub(crate) async fn revoke_tool(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let d = authorize(ctx, req, "revoke").await?;
    let revoked = revoke_tool_approval(ctx, &d.agent, &d.thread, &d.tool)
        .await
        .map_err(approval_error)?;
    match revoked {
        Some(seq) => tracing::info!(
            "approval revoked: agent={} tool={} thread={} revoked_by={} seq={seq}",
            d.agent,
            d.tool,
            d.thread,
            d.by
        ),
        None => tracing::info!(
            "approval revoke found nothing live: agent={} tool={} thread={} revoked_by={}",
            d.agent,
            d.tool,
            d.thread,
            d.by
        ),
    }
    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "agent": d.agent,
        "tool": d.tool,
        "thread": d.thread,
        "revoked": revoked.is_some(),
        "revoked_by": d.by,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// Standing ownership of `agent`, recorded at persona creation.
async fn is_agent_owner(
    config: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    agent: &str,
    who: &crate::relay::event::PubKey,
) -> Result<bool> {
    if let Some(owner) = store::get_raw(config, &keys::agent_owner(community, agent)).await? {
        if String::from_utf8_lossy(&owner).trim() == crate::relay::crypto::to_hex(who) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A required agent or tool identifier: the charset both share (ASCII
/// alphanumerics, `-` and `_`), at most 64 bytes. Refused, not repaired, so the
/// grant names exactly what the caller meant.
fn identifier(body: &Value, field: &str) -> Result<String> {
    let raw = body
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let valid = !raw.is_empty()
        && raw.len() <= 64
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if valid {
        Ok(raw.to_string())
    } else {
        Err(BuzzError::InvalidEvent(format!(
            "this needs a '{field}' of letters, digits, '-' or '_' (at most 64)"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_refused_rather_than_repaired() {
        let ok = serde_json::json!({ "tool": " buzz_post " });
        assert_eq!(identifier(&ok, "tool").expect("a valid tool name"), "buzz_post");
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "tool": "" }),
            serde_json::json!({ "tool": "buzz post" }),
            serde_json::json!({ "tool": "a/b" }),
            serde_json::json!({ "tool": "x".repeat(65) }),
            serde_json::json!({ "tool": 7 }),
        ] {
            assert!(identifier(&bad, "tool").is_err(), "{bad}");
        }
    }
}
