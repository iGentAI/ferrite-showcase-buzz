use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::{serde_json, AppContext};

use crate::binding::Identity;
use crate::command::{Command, CommandResult, Verb};
use crate::error::{ProcessError, Result};
use crate::event::Event;
use crate::{commit_message, planes, rcs, result_for, say_at, CellContext};

#[derive(Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", tag = "state", rename_all = "snake_case")]
enum Mutation {
    OutcomeUnknown {
        branch: String,
        message: String,
        notice: String,
    },
    Completed {
        result: CommandResult,
    },
}

pub(super) fn owns(verb: Verb) -> bool {
    matches!(verb, Verb::Commit | Verb::Merge | Verb::Land)
}

fn key(control: &rcs::ControlPlane, command: &Command, request: &Event) -> Vec<u8> {
    format!(
        "mutation/{}/{}/{}",
        control.app(),
        command.binding_id,
        request.id
    )
    .into_bytes()
}

pub(super) async fn resume(
    ctx: &AppContext,
    cell: &CellContext,
    control: &rcs::ControlPlane,
    command: &Command,
    request: &Event,
) -> Result<Option<CommandResult>> {
    let key = key(control, command, request);
    let Some(record) = planes::load::<Mutation>(ctx, &key).await? else {
        return Ok(None);
    };
    match record {
        Mutation::Completed { result } => Ok(Some(result)),
        Mutation::OutcomeUnknown {
            branch,
            message,
            notice,
        } => {
            // A commit can still be reconciled from the branch log; until the
            // log has been read, its outcome is not settled.
            let mut log_read = command.verb != Verb::Commit;
            if command.verb == Verb::Commit {
                let call = control.log(&branch, 200);
                let reply = match planes::deliver(ctx, planes::CONTROL_EGRESS, &call).await {
                    Ok((status, body)) => rcs::parse_reply(status, &body),
                    Err(error) => Err(error),
                };
                if let Ok(rcs::RcsReply::Log(entries)) = reply {
                    log_read = true;
                    let trailer = format!("\nBuzz-Command: {}\n", request.id);
                    if let Some(entry) = entries
                        .iter()
                        .find(|entry| entry.message == message && entry.message.ends_with(&trailer))
                    {
                        let result = result_for(
                            &request.id,
                            command.verb,
                            &command.binding_id,
                            &rcs::RcsReply::Committed {
                                branch,
                                commit: entry.commit.clone(),
                            },
                        );
                        planes::save(
                            ctx,
                            &key,
                            &Mutation::Completed {
                                result: result.clone(),
                            },
                        )
                        .await?;
                        return Ok(Some(result));
                    }
                }
            }
            // Why: a log that could not be read says nothing about the
            // commit; telling the room its outcome is unknown would be
            // premature while the outage that lost the reply still lasts.
            if !log_read {
                return Err(ProcessError::Egress(format!(
                    "{}: the branch log could not be read to reconcile it; retried next cycle",
                    request.id
                )));
            }
            let mut notified = key;
            notified.extend_from_slice(b"/notified");
            if planes::claimed(ctx, &notified).await?.is_none() {
                let room = request
                    .channel()
                    .ok_or_else(|| ProcessError::Invalid("a mutation has no room".into()))?;
                // At the request's own instant, so a retried notice (a lost
                // relay reply, a failed claim, a peer cycle) is the same event
                // and the relay keeps one.
                say_at(ctx, cell, room, &notice, request.created_at).await?;
                planes::claim_once(ctx, &notified, b"true").await?;
            }
            let detail = format!(
                "{}: outcome unknown; this command will not be submitted again",
                request.id
            );
            // Told to the room and never resubmitted: settled, unless a commit's
            // log could not be read to reconcile it.
            Err(if log_read {
                ProcessError::Unsettled(detail)
            } else {
                ProcessError::Egress(detail)
            })
        }
    }
}

pub(super) async fn send(
    ctx: &AppContext,
    cell: &CellContext,
    control: &rcs::ControlPlane,
    identity: &Identity,
    command: &Command,
    request: &Event,
) -> Result<CommandResult> {
    let branch = identity
        .git_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(&identity.git_ref)
        .to_string();
    let message = commit_message(
        command.args.message.as_deref().unwrap_or(""),
        &request.pubkey,
        &request.id,
    );
    let call = match command.verb {
        Verb::Commit => control.commit(
            command
                .args
                .files
                .as_ref()
                .ok_or_else(|| ProcessError::Invalid("rcs_commit needs files".into()))?,
            &message,
            Some(&branch),
        ),
        Verb::Merge => control.merge(
            command.args.from.as_deref().unwrap_or(&branch),
            command.args.into.as_deref().unwrap_or(crate::command::TRUNK),
        ),
        Verb::Land => control.land(
            command.args.from.as_deref().unwrap_or(&branch),
            command.args.into.as_deref().unwrap_or("main"),
        ),
        _ => {
            return Err(ProcessError::Invalid(
                "this verb is not a guarded RCS mutation".into(),
            ))
        }
    };
    let notice = format!("{}: outcome unknown for command {}. This write will not be retried automatically. Check the platform history before issuing a new command.", command.verb.as_str(), request.id);
    let record = Mutation::OutcomeUnknown {
        branch,
        message,
        notice,
    };
    let encoded = serde_json::to_vec(&record)
        .map_err(|e| ProcessError::Internal(format!("mutation encode: {e}")))?;
    let key = key(control, command, request);
    if !planes::claim_once(ctx, &key, &encoded).await? {
        return resume(ctx, cell, control, command, request)
            .await?
            .ok_or_else(|| ProcessError::Storage("a mutation claim vanished".into()));
    }
    let reply = match planes::deliver(ctx, planes::CONTROL_EGRESS, &call).await {
        Ok((status, body)) => rcs::parse_reply(status, &body),
        Err(error) => Err(error),
    };
    match reply {
        Ok(reply) => {
            let result = result_for(&request.id, command.verb, &command.binding_id, &reply);
            planes::save(
                ctx,
                &key,
                &Mutation::Completed {
                    result: result.clone(),
                },
            )
            .await?;
            Ok(result)
        }
        Err(_) => resume(ctx, cell, control, command, request)
            .await?
            .ok_or_else(|| ProcessError::Storage("a mutation claim vanished".into())),
    }
}
