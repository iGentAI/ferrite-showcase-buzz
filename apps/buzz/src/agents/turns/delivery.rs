//! Delivering a turn's answer to its channel.

use std::time::Duration;
use ferrite_app::rt;
use ferrite_app::serde_json;
use crate::agents::tools::CloudToolRegistry;
use super::{A2A_REPLY_MAX_BYTES};
use super::a2a_lease::{PendingAnswer, marker_is_owned_by, marker_pending, stage_pending};

/// Why a staged answer was not delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeliverError {
    /// Another attempt owns the turn now: this driver must stop posting.
    Superseded,
    /// Every attempt failed or was uncertain; the answer stays staged.
    Failed(String),
}

impl core::fmt::Display for DeliverError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Superseded => f.write_str("superseded by a newer attempt"),
            Self::Failed(e) => f.write_str(e),
        }
    }
}

/// Post a staged answer into the channel through the persona surface. Threaded
/// under the triggering message first (`reply_to` IS that event's id); only an
/// explicit refusal of the threaded form switches to a top-level post, and the
/// switch is staged under the marker BEFORE the top-level write, so neither this
/// driver nor a reclaiming one ever posts the other form afterwards. A transport
/// failure or unreadable reply retries the SAME write.
///
/// The threaded form is LOCKED durably (`uncertain`) BEFORE every threaded
/// write, so a driver that crashes after its write committed leaves the lock
/// behind. A refusal switches the form only when no earlier threaded write —
/// by this driver or any other — can have committed.
///
/// FENCED: before every attempt the marker is re-read. A driver whose attempt
/// no longer holds a LIVE lease stops — a stale driver must not post after a
/// newer one took the turn over or after its own lease lapsed (a reclaim is
/// then possible at any moment) — and the text and form posted are the ones
/// DURABLY staged under the marker, never only this driver's in-memory copy.
pub(super) async fn deliver_answer(
    registry: &CloudToolRegistry<'_>,
    markers: &mut ferrite_app::client::StoreClusterClient,
    marker_key: &str,
    attempt: u32,
    now: impl Fn() -> u64,
    channel: &str,
    reply_to: &str,
    pending: &mut PendingAnswer,
) -> std::result::Result<(), DeliverError> {
    let mut last = String::new();
    for i in 0..ANSWER_POST_ATTEMPTS {
        if i > 0 {
            rt::time::sleep(Duration::from_millis(ANSWER_POST_BACKOFF_MS)).await;
        }
        match markers.get(marker_key.as_bytes()).await {
            Ok(Some(raw)) if marker_is_owned_by(Some(&raw), attempt, now()) => {
                match marker_pending(&raw) {
                    Some(durable) => *pending = durable,
                    // Staging always precedes delivery and every rewrite
                    // carries it; a marker without it is not this delivery's.
                    None => return Err(DeliverError::Superseded),
                }
            }
            Ok(_) => return Err(DeliverError::Superseded),
            Err(e) => {
                last = format!("marker read failed: {e}");
                continue;
            }
        }
        // Whether an EARLIER threaded write may have committed: read before
        // this attempt locks the form for its own write.
        let prior_uncertain = pending.uncertain;
        if !pending.top_level && !pending.uncertain {
            pending.uncertain = true;
            if !stage_pending(markers, marker_key, attempt, now(), pending).await {
                return Err(DeliverError::Superseded);
            }
        }
        let args = if pending.top_level {
            serde_json::json!({ "channel": channel, "content": pending.text })
        } else {
            serde_json::json!({ "channel": channel, "content": pending.text, "reply_to": reply_to })
        };
        match buzz_post_outcome(registry, &args).await {
            PostOutcome::Posted => return Ok(()),
            outcome => match next_delivery_step(pending, prior_uncertain, outcome) {
                DeliveryStep::Restage(why) => {
                    // The form changed (or became locked): durable BEFORE the
                    // next write, so no driver ever posts the other form after.
                    if !stage_pending(markers, marker_key, attempt, now(), pending).await {
                        return Err(DeliverError::Superseded);
                    }
                    last = why;
                }
                DeliveryStep::Again(why) => last = why,
            },
        }
    }
    Err(DeliverError::Failed(last))
}

/// What a non-posted attempt means for the staged answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeliveryStep {
    /// `pending` changed; stage it before the next attempt.
    Restage(String),
    /// Retry the same form.
    Again(String),
}

/// The form rule, pure. `prior_uncertain` says whether a threaded write BEFORE
/// this attempt may have committed (the lock this attempt found). Only a
/// definitive refusal with no such earlier write switches to top-level; the
/// switch also clears the lock, since nothing threaded can have landed.
pub(super) fn next_delivery_step(
    pending: &mut PendingAnswer,
    prior_uncertain: bool,
    outcome: PostOutcome,
) -> DeliveryStep {
    match outcome {
        PostOutcome::Posted => DeliveryStep::Again(String::new()),
        PostOutcome::Refused(e) if !pending.top_level && !prior_uncertain => {
            pending.top_level = true;
            pending.uncertain = false;
            DeliveryStep::Restage(format!("the threaded form was refused ({e}); switched to a top-level post"))
        }
        PostOutcome::Refused(e) if !pending.top_level => DeliveryStep::Again(format!(
            "{e} (an earlier threaded attempt may have posted, so the form is not switched)"
        )),
        PostOutcome::Refused(e) | PostOutcome::Uncertain(e) => DeliveryStep::Again(e),
    }
}

/// What one `buzz_post` attempt established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PostOutcome {
    Posted,
    /// The relay answered and said no: nothing was written.
    Refused(String),
    /// No readable answer: the write may or may not have committed.
    Uncertain(String),
}

pub(super) fn classify_post_result(result: std::result::Result<Vec<u8>, String>) -> PostOutcome {
    let bytes = match result {
        Ok(bytes) => bytes,
        Err(e) => return PostOutcome::Uncertain(e),
    };
    // ONE rule for live and recorded results ([`crate::agents::tools::post_record`]):
    // a refusal is only an explicit negative acknowledgement with a stated error;
    // any other shape (null, `{}`, an empty or non-string error, an unexpected
    // envelope, `retryable`) does not prove nothing was written.
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Err(e) => PostOutcome::Uncertain(format!("unreadable post result: {e}")),
        Ok(value) => match crate::agents::tools::post_record(&value) {
            crate::agents::tools::PostRecord::Landed => PostOutcome::Posted,
            crate::agents::tools::PostRecord::NotPosted => PostOutcome::Refused(value.to_string()),
            crate::agents::tools::PostRecord::Unknown => {
                PostOutcome::Uncertain(format!("unrecognised post result: {value}"))
            }
        },
    }
}

pub(super) async fn buzz_post_outcome(registry: &CloudToolRegistry<'_>, args: &serde_json::Value) -> PostOutcome {
    classify_post_result(
        registry
            .invoke_buzz("buzz_post", args.to_string().as_bytes())
            .await
            .map_err(|e| e.to_string()),
    )
}


/// How many times a transient answer-post is attempted before it is treated as
/// undeliverable. Small on purpose: a genuine refusal (an unregistered persona,
/// a moderation block) will fail every attempt and must not spin — the value of
/// retrying is a relay/egress blip, which clears in well under a second.
pub(super) const ANSWER_POST_ATTEMPTS: usize = 3;
/// Backoff between answer-post attempts.
pub(super) const ANSWER_POST_BACKOFF_MS: u64 = 150;

/// Clamp a channel reply to [`A2A_REPLY_MAX_BYTES`] on a char boundary — a
/// chat channel is not a report sink, and an unbounded answer must not
/// become one.
pub(super) fn clamp_channel_reply(text: &str) -> String {
    const MARK: &str = "\n[truncated]";
    if text.len() <= A2A_REPLY_MAX_BYTES {
        return text.to_string();
    }
    // The marker counts against the cap: cutting at the full cap and then
    // appending it produced a reply the relay's message bound could refuse.
    let mut cut = A2A_REPLY_MAX_BYTES - MARK.len();
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{MARK}", &text[..cut])
}
