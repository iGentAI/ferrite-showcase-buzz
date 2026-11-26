use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::{serde_json, AppContext};

use crate::binding::Identity;
use crate::error::{ProcessError, Result};
use crate::event::Event;
use crate::{planes, walked_bindings, CellContext, SECRET_CELL_KEY};

#[derive(Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
struct Head {
    operation: String,
    event: Event,
}

async fn checkpoint(ctx: &AppContext, prefix: &str, head: &Head) -> Result<Event> {
    let key = format!("{prefix}/operation/{}", head.operation).into_bytes();
    let bytes = serde_json::to_vec(&head.event)
        .map_err(|e| ProcessError::Internal(format!("binding event encode: {e}")))?;
    planes::claim_once(ctx, &key, &bytes).await?;
    planes::load(ctx, &key)
        .await?
        .ok_or_else(|| ProcessError::Storage("reserved binding event vanished".into()))
}

/// The `created_at` of the newest binding this cell published in the room
/// before it kept a record there, so the first reservation is ordered after
/// it; `None` when the room's timeline holds no such binding.
async fn legacy_instant(
    ctx: &AppContext,
    cell: &CellContext,
    identity: &Identity,
) -> Result<Option<u64>> {
    let bindings = walked_bindings(ctx, cell, &identity.room_id()?, &identity.binding_id()?).await?;
    Ok(bindings.iter().map(|(at, _, _)| *at).max())
}

fn order_prefix(cell: &CellContext, identity: &Identity) -> Result<String> {
    Ok(format!(
        "binding-order/{}/{}",
        cell.api.pubkey_hex(),
        identity.room_id()?
    ))
}

/// The binding event this cell last reserved for the room, from its own
/// store; `None` when it has reserved none there.
///
/// Every binding the cell publishes is reserved here first ([`reserve`]), and
/// the reservations are totally ordered, so this is the room's current
/// binding as the cell decided it, whatever has been posted in the room since.
pub(super) async fn recorded(
    ctx: &AppContext,
    cell: &CellContext,
    identity: &Identity,
) -> Result<Option<Event>> {
    let head_key = format!("{}/head", order_prefix(cell, identity)?).into_bytes();
    Ok(planes::load::<Head>(ctx, &head_key).await?.map(|head| head.event))
}

pub(super) async fn reserve(
    ctx: &AppContext,
    cell: &CellContext,
    identity: &Identity,
    operation: &str,
    draft: Event,
) -> Result<Event> {
    let prefix = order_prefix(cell, identity)?;
    let operation_key = format!("{prefix}/operation/{operation}").into_bytes();
    let head_key = format!("{prefix}/head").into_bytes();
    for _ in 0..32 {
        if let Some(event) = planes::load(ctx, &operation_key).await? {
            return Ok(event);
        }
        let head = planes::load_versioned::<Head>(ctx, &head_key).await?;
        let (previous_at, version) = match head {
            Some((head, version)) => {
                let previous = checkpoint(ctx, &prefix, &head).await?;
                if head.operation == operation {
                    return Ok(previous);
                }
                (Some(head.event.created_at), Some(version))
            }
            None => (legacy_instant(ctx, cell, identity).await?, None),
        };
        let created_at = match previous_at {
            Some(previous) => draft.created_at.max(
                previous
                    .checked_add(1)
                    .ok_or_else(|| ProcessError::Storage("binding timestamp exhausted".into()))?,
            ),
            None => draft.created_at,
        };
        // The relay refuses an event more than 900 seconds old as well as
        // one 900 seconds ahead. A draft stamped before an egress outage is
        // moved up to a recent instant (still after the previous binding, so
        // order holds) rather than signed with a time the relay rejects.
        let created_at = created_at.max((ctx.now_ms() / 1000).saturating_sub(600));
        // Buzz admission permits at most 900 seconds of clock skew
        // (apps/buzz/src/relay/ingress/events.rs, accept_event).
        if created_at > (ctx.now_ms() / 1000).saturating_add(900) {
            return Err(ProcessError::Egress(
                "binding order is ahead of the relay clock; retry after the clock advances".into(),
            ));
        }
        let event = Event::sign(
            &planes::secret_key(ctx, SECRET_CELL_KEY)?,
            draft.kind,
            draft.tags.clone(),
            &draft.content,
            created_at,
        )?;
        let head = Head {
            operation: operation.to_string(),
            event,
        };
        if planes::save_if_version(ctx, &head_key, &head, version).await? {
            return checkpoint(ctx, &prefix, &head).await;
        }
    }
    Err(ProcessError::Storage(
        "binding order changed during every reservation attempt".into(),
    ))
}
