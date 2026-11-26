//! Operational faults, raised where an operator can actually see them.
//!
//! Buzz already detects the handful of conditions that silently break a
//! deployment. It reported them to `tracing`, which is inside the process: a
//! deployed app's log is not something anyone is subscribed to, so "every
//! persona went mute" or "no client can build a channel list any more" looked
//! from the outside like nothing at all.
//!
//! The platform has a channel for exactly this — `AppContext::notify_deduped`,
//! a publish-only handle scoped to the owning account's own notification feed —
//! and Buzz used none of it. This module is the seam.
//!
//! ## What belongs here, and what does not
//!
//! Only faults an OPERATOR must act on: a missing credential, a broken
//! configuration document, a dependency refusing service. Not a user's mistake
//! (a bad request is answered, not alerted), not a moderation event (that is
//! what the audit chain is for), and not anything on the hot path — a
//! notification is an I/O call, so it is raised on failure paths that are
//! already rare and already logging.
//!
//! ## Never fatal, never chatty
//!
//! A notification failure is swallowed: the platform's own contract says an
//! older fleet carries no notify plane and returns `NotFound`, and a
//! notification must never fail the operation it narrates. And every raise is
//! deduplicated on an hourly bucket, so a fault that recurs on every request
//! produces one notification an hour rather than a flood — while a fault that
//! comes back next week still gets a fresh one.

use ferrite_app::tracing;
use ferrite_app::AppContext;

/// The relay's signing identity is missing or unusable, so the NIP-29 group
/// projections a client builds its channel list and roster from cannot be
/// published. Silent by nature: chat keeps working, discovery quietly does not.
pub const RELAY_IDENTITY: &str = "buzz.relay_identity_unusable";

/// The persona registry document does not parse, so no mention can start a turn.
/// Chat is deliberately unaffected, which is precisely why this needs saying out
/// loud — the workspace looks healthy while every AI teammate is deaf.
pub const PERSONA_REGISTRY: &str = "buzz.persona_registry_malformed";

/// The model-provider boundary refused a turn for a reason that is not the
/// caller's: a missing or rejected credential, a denied egress, a provider
/// outage. Every persona is mute until it clears.
pub const PROVIDER_UNAVAILABLE: &str = "buzz.provider_unavailable";

/// A Fabric A2A conversation is wedged: its context passed the wall-clock
/// ceiling without reaching a terminal state. The context is retired
/// automatically, so this is a warning rather than an error — but a peer that
/// wedges repeatedly is an operator's problem.
pub const FABRIC_WEDGED: &str = "buzz.fabric_context_wedged";

/// A privileged action's audit line could not be appended: the chain head stayed
/// contended past the retry budget. The action itself STOOD — the room exists, the
/// config row is written — so nothing failed from the caller's side, and that is
/// precisely why it has to be said out loud. The chain's claim is that nothing is
/// missing from it; here something is.
pub const AUDIT_DROPPED: &str = "buzz.audit_line_dropped";

/// Longest body this module will send. Far below the platform's own bound, and
/// clamped on a char boundary, so a fault whose message embeds a provider's
/// entire error page cannot be refused for size.
const MAX_BODY: usize = 1024;

/// Raise one operational fault, at most once per hour per kind.
///
/// `severity` is the platform's vocabulary (`info` | `warn` | `error`). Best
/// effort throughout: a deployment without a notify plane, or a transient
/// failure reaching it, must not turn a diagnosed fault into a second one.
pub async fn operational(ctx: &AppContext, severity: &str, kind: &str, body: &str) {
    // The platform folds by `dedup`, so an hour bucket turns a persistent fault
    // into one notification per hour instead of one per request — while still
    // letting the same fault be reported again tomorrow.
    let bucket = ctx.now_ms() / (60 * 60 * 1000);
    let dedup = format!("{kind}/{bucket}");
    if let Err(e) = ctx
        .notify_deduped(severity, kind, clamp(body).as_str(), &dedup)
        .await
    {
        tracing::debug!("notification {kind} was not delivered: {e}");
    }
}

/// Clamp a body on a char boundary, marked so a reader knows it was cut.
///
/// The marker is counted INSIDE [`MAX_BODY`], not added to it: the bound exists
/// to keep the platform from refusing the notification for size, so a "maximum"
/// the result can exceed would be no bound at all.
fn clamp(body: &str) -> String {
    if body.len() <= MAX_BODY {
        return body.to_string();
    }
    const MARKER: char = '\u{2026}';
    let mut cut = MAX_BODY - MARKER.len_utf8();
    while !body.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{MARKER}", &body[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind must satisfy the platform's own validation, or the
    /// notification is refused at the boundary and the fault stays invisible —
    /// which is the exact failure this module exists to fix.
    #[test]
    fn every_kind_is_acceptable_to_the_platform() {
        for kind in [
            RELAY_IDENTITY,
            PERSONA_REGISTRY,
            PROVIDER_UNAVAILABLE,
            FABRIC_WEDGED,
            AUDIT_DROPPED,
        ] {
            assert!(!kind.is_empty() && kind.len() <= 64, "{kind}");
            assert!(
                kind.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')),
                "{kind} must be [A-Za-z0-9_.-]"
            );
        }
    }

    #[test]
    fn a_long_body_is_clamped_within_the_stated_maximum() {
        assert_eq!(clamp("short"), "short");
        for body in [
            "\u{e9}".repeat(MAX_BODY),    // multi-byte, so the cut lands mid-char
            "a".repeat(MAX_BODY * 3),     // plain ASCII, well over
            "\u{1f600}".repeat(MAX_BODY), // 4-byte chars
        ] {
            let clamped = clamp(&body);
            assert!(clamped.ends_with('\u{2026}'), "a cut body must say so");
            assert!(
                clamped.len() <= MAX_BODY,
                "clamped to {} bytes, over the {MAX_BODY}-byte bound",
                clamped.len()
            );
        }
        // A body exactly at the bound is passed through untouched.
        let exact = "a".repeat(MAX_BODY);
        assert_eq!(clamp(&exact), exact);
    }
}
