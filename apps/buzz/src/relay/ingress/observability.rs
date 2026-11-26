//! The audit trail and the metrics route.

use ferrite_app::tracing;
use ferrite_app::serde_json;
use ferrite_app::serde_json::Value;
use ferrite_app::AppContext;
use ferrite_app::IngressRequest;
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::store;
use crate::relay::BuzzError;
use crate::relay::Result;
use crate::relay::APP_ID;
use crate::relay::DEFAULT_COMMUNITY;
use super::storage;
use super::operator_plane::{operator_caller};
use super::identity::{relay_identity_sourced, stream_key};

/// Append one audit entry for a PRIVILEGED action, fighting for the head.
///
/// The mechanics — the progress-bounded retry, the definitive-versus-ambiguous
/// rule, the counters — live in [`crate::relay::audit::append`], which the ingest
/// saga shares. This passes no idempotency marker because a privileged action has
/// no natural key, and that is the honest choice: two config writes ARE two
/// actions, so recording both is right and collapsing them under a synthetic key
/// would lose one.
pub(super) async fn record_audit(
    guards: &mut ferrite_app::client::ApeClusterClient,
    community: &str,
    draft: crate::relay::audit::AuditDraft,
) -> Result<u64> {
    crate::relay::audit::append(guards, community, draft, None).await
}

/// Append a privileged action's audit line, and RAISE A FAULT if it could not be
/// written.
///
/// The privileged routes deliberately do not fail their caller over the audit:
/// the room exists, the config row is written, the bundle is live, and refusing
/// the caller would neither undo that nor record it. But an unwritten line must
/// not be INVISIBLE either. The chain's entire claim is that nothing is missing
/// from it, so a line that never landed is exactly the class of fault
/// [`crate::relay::alerts`] exists for — and `tracing::warn!` on its own is inside
/// the process, which is the gap that module was added to close.
pub(super) async fn record_audit_or_alert(
    ctx: &AppContext,
    guards: &mut ferrite_app::client::ApeClusterClient,
    community: &str,
    draft: crate::relay::audit::AuditDraft,
    what: &str,
) {
    let Err(e) = record_audit(guards, community, draft).await else {
        return;
    };
    tracing::warn!("{what} audit append failed: {e}");
    crate::relay::alerts::operational(
        ctx,
        "error",
        crate::relay::alerts::AUDIT_DROPPED,
        &format!("{what}: {e}"),
    )
    .await;
}

/// `GET /buzz/api/metrics` — a bounded JSON snapshot. There is no Prometheus
/// endpoint to scrape (an app binds no port), so the numbers are served like any
/// other read.
///
/// **The route is public; its payload is TIERED.** The document carries the
/// relay's identity source and its operational queue depths, which must not be
/// answered to anybody. The split, and the reason for each side of it:
///
/// * **Public** — `ok`, `app`, `community`, `identity.configured`,
///   `identity.pubkey`, `stream_key_configured`. A liveness probe holds no key,
///   and this app cannot bind a port for a private one, so the probe surface has
///   to stay open. The identity fields are public *because they already are*:
///   `GET /buzz/api/relay-info` publishes the relay's pubkey as NIP-11's own
///   field, present exactly when an identity is configured, and NIP-11 is an
///   unauthenticated document by design. Hiding here what a sibling public route
///   publishes by specification would be theatre, not a gate. The two
///   `*_configured` booleans stay open for a second reason as well: they are THE
///   documented check for the platform's secrets-do-not-survive-a-restart gap
///   they say nothing an attacker cannot
///   learn by opening a stream and watching it refuse, and a redeploy check that
///   needs a signed request is a redeploy check that stops being run.
/// * **Operator only** (a valid NIP-98 header from a pubkey in the operator set)
///   — `identity.source` and the whole `requests` block. `source` says whether
///   the signing key came from the secrets plane or from the config-store
///   FALLBACK, which is a "which deployments run the weaker seam" selector and
///   serves no client. The queue counters are a workspace-volume disclosure and,
///   worse, a live feedback oracle: an attacker watching `pending` and
///   `dead_lettered` learns whether their load is actually landing.
///
/// `scope` names which document you got, so an operator whose header was
/// mis-signed sees `"public"` instead of silently reading a half-empty snapshot
/// and concluding the relay has no identity. For the same reason the four
/// `requests` counters are `null` with `requests.readable: false` when the
/// queue plane refuses the read — see [`requests_counts`].
///
/// The `identity` block exists at all because the relay's signing identity can
/// vanish SILENTLY: per-app secrets inject only at deploy time, so a fleet
/// restart relaunches the app with `ctx.secret()` empty (a known platform gap)
/// and projections stop. Only the PUBLIC key ever goes on
/// the wire, on either tier.
/// The `requests` block of the operator metrics document: the four queue
/// counters, or four `null`s and `readable: false` when the plane did not
/// answer.
///
/// A refused `Counts` and a drained queue are the SAME call here — `Counts`
/// requires `QueueAdmin`, which an application token is not guaranteed to
/// carry (a known platform gap in queue observability). Folding the refusal to zero would make a backlog dashboard
/// read healthiest exactly when it has lost its authorization, so the refusal
/// is `null`: unknown, and visibly so. `head_seq` in the same document
/// answers `null` for the same reason.
///
/// Callers must treat `null` as "not measured": an alert on `pending` must not
/// fire on it, and an alert on the ABSENCE of a number should.
pub(super) fn requests_counts(counts: Option<(u64, u64, u64, u64)>) -> Value {
    let (published, acked, dlq, pending) = match counts {
        Some(c) => (Some(c.0), Some(c.1), Some(c.2), Some(c.3)),
        None => (None, None, None, None),
    };
    serde_json::json!({
        "readable": counts.is_some(),
        "published": published,
        "acked": acked,
        "dead_lettered": dlq,
        "pending": pending,
    })
}

pub(super) async fn metrics(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let identity = relay_identity_sourced(ctx, &mut config).await;
    let pubkey_hex = identity
        .as_ref()
        .and_then(|(seckey, _)| crate::relay::crypto::schnorr::xonly_pubkey(seckey))
        .map(|pk| crate::relay::crypto::to_hex(&pk));
    let stream_key_configured = stream_key(ctx, &mut config).await.is_ok();
    // An absent or invalid header is simply "not an operator" — the route must
    // still answer a probe. `require_payload = false`: a GET has no body.
    let is_operator = operator_caller(ctx, req, false).await.is_ok();
    let mut body = serde_json::json!({
        "ok": true,
        "app": APP_ID,
        "community": DEFAULT_COMMUNITY,
        "scope": if is_operator { "operator" } else { "public" },
        "identity": {
            "configured": identity.is_some(),
            "pubkey": pubkey_hex,
        },
        "stream_key_configured": stream_key_configured,
    });
    if !is_operator {
        return serde_json::to_vec(&body)
            .map_err(|e| BuzzError::Internal(format!("metrics encode: {e}")));
    }

    let mut queue = ctx.queue(store::REQUESTS).map_err(storage)?;
    let counts = queue.counts().await.ok();
    if let Some(obj) = body.as_object_mut() {
        if let Some(identity_obj) = obj.get_mut("identity").and_then(Value::as_object_mut) {
            identity_obj.insert(
                "source".to_string(),
                serde_json::json!(identity.as_ref().map(|(_, source)| source.as_str())),
            );
        }
        obj.insert("requests".to_string(), requests_counts(counts));
        // Idle pooled plane clients. This is the observable half of the handle
        // pooling: a deployment serving traffic with warm pools shows idle
        // clients here instead of re-dialling a plane per handle, so "we reuse
        // connections now" is something an operator can check rather than a
        // claim in a commit message.
        //
        // `built` is the number that matters: it counts fresh clients, each of
        // which cost a dial and a capability handshake. On a warm deployment it
        // settles while `handed_out` keeps climbing.
        obj.insert(
            "plane_clients".to_string(),
            serde_json::json!({
                "handed_out": crate::relay::planes::checkouts(),
                "built": crate::relay::planes::builds(),
                "idle": crate::relay::planes::idle_counts()
                    .into_iter()
                    .map(|(plane, idle)| (plane.to_string(), idle))
                    .collect::<std::collections::BTreeMap<String, usize>>(),
            }),
        );
        // Audit-chain contention. The head is ONE key per community and every
        // accepted event bumps it, so competition for it is inherent to the design
        // rather than a fault — which is why it gets a number instead of an
        // argument. `exhausted` is the one that must stay at zero: an exhausted
        // append is an action that happened with nothing in the chain saying so.
        //
        // `attempts`/`contended`/`exhausted` are PER-PROCESS, and a deployment
        // runs several instances competing on one request queue — so two metrics
        // reads can be answered by two different instances and the DIFFERENCE
        // between them means nothing (measured live: consecutive reads
        // alternating between 23 and 7). They describe contention on the
        // instance that answered, and are not a fleet-wide total.
        //
        // `head_seq` is the number that CAN be differenced: the chain head is
        // durable shared state on the guards plane, so every instance reports
        // the same value and a delta counts lines actually appended — whoever
        // appended them. `null` when the chain is empty or unreadable, which is
        // distinguishable from zero.
        let (attempts, contended, exhausted) = crate::relay::audit::contention();
        let head_seq = match planes::ape(ctx, store::GUARDS) {
            Ok(mut guards) => match guards.get(&keys::audit_head(DEFAULT_COMMUNITY)).await {
                Ok(Some(bytes)) => serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .and_then(|head| head.get("seq").and_then(Value::as_u64)),
                _ => None,
            },
            Err(_) => None,
        };
        obj.insert(
            "audit_chain".to_string(),
            serde_json::json!({
                "head_seq": head_seq,
                "attempts": attempts,
                "contended": contended,
                "exhausted": exhausted,
            }),
        );
    }
    serde_json::to_vec(&body).map_err(|e| BuzzError::Internal(format!("metrics encode: {e}")))
}
