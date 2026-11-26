use super::*;

/// The GitHub cell: verify deliveries, project them into rooms.
pub(crate) async fn webhook_loop(ctx: AppContext) -> io::Result<()> {
    let _ = ctx.queue(planes::REQUESTS)?;
    let _ = ctx.queue(planes::REPLIES)?;
    let secret = planes::secret(&ctx, SECRET_WEBHOOK)?;
    let instance =
        planes::secret(&ctx, SECRET_GITHUB_INSTANCE).unwrap_or_else(|_| "github.com".to_string());
    // When outstanding commands were last answered. Zero means "not yet", so the
    // first iteration sweeps.
    let mut last_sweep = 0u64;

    loop {
        // The deadline is checked on EVERY iteration rather than on an idle poll,
        // and that is the whole point of the cadence: a webhook queue with
        // traffic in it never reports idle, so an idleness-triggered sweep would
        // starve command answering for exactly as long as the repository was
        // busy — which is the silence this sweep exists to remove.
        let now = ctx.now_ms();
        if now.saturating_sub(last_sweep) >= GITHUB_COMMAND_SWEEP_MS {
            last_sweep = now;
            match github_command_sweep(&ctx, &instance).await {
                Ok(answered) if answered > 0 => {
                    tracing::info!("answered {answered} command(s)");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("github command sweep: {e}"),
            }
        }

        if let Err(e) = webhook_step(&ctx, &secret, &instance).await {
            tracing::warn!("webhook queue step: {e}");
            rt::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

pub(crate) async fn webhook_step(ctx: &AppContext, secret: &str, instance: &str) -> io::Result<()> {
        let Some(lease) = planes::poll_webhook(ctx, WEBHOOK_POLL_MS).await? else {
            return Ok(());
        };
        let Ok(request) = serde_json::from_slice::<IngressRequest>(&lease.payload) else {
            return planes::ack_webhook(ctx, lease.token).await;
        };
        let correlation = request.correlation_id.clone();
        let (status, body) =
            match handle_webhook(ctx, secret, instance, &request, &correlation).await {
                Ok(()) => (202u16, b"accepted".to_vec()),
                Err(e) if e.is_final() => {
                    tracing::warn!("webhook refused: {e}");
                    (
                        if matches!(e, ProcessError::Refused(_)) {
                            401
                        } else {
                            400
                        },
                        e.to_string().into_bytes(),
                    )
                }
                Err(e) => {
                    // Transient: leave the envelope unacked so the queue
                    // re-drives it, exactly as the relay's api loop does.
                    tracing::warn!("webhook deferred: {e}");
                    return Ok(());
                }
            };
        // Through the SDK's own constructor rather than a struct literal, so a
        // field added to the envelope does not break this app, and with the
        // media type set: the gateway writes exactly the headers it is handed
        // (`write_response`), so an untyped reply reaches the caller untyped.
        // Every answer on this route is a short plain-text line, never JSON.
        let mut reply = IngressReply::new(correlation, status, body);
        reply.headers.push(("content-type".into(), "text/plain; charset=utf-8".into()));
        let payload = serde_json::to_vec(&reply).map_err(io::Error::other)?;
        planes::publish_webhook_reply(ctx, &request.reply_to, &payload).await?;
        planes::ack_webhook(ctx, lease.token).await
}

/// Answer every outstanding command in the rooms this cell knows about: the
/// live refs' rooms, and (with a refusal) the rooms of retired lifetimes.
///
/// Its rooms come from its own observation state, which is the only record it has
/// of the refs it has seen. State is persisted AFTER the answers, so a crash
/// re-derives the same work, and a command already answered has its
/// [`AnswerRecord`] whatever this document says.
pub(crate) async fn github_command_sweep(ctx: &AppContext, instance: &str) -> Result<usize> {
    // A seed unique per sweep: the nonce discipline `NonceSource` documents.
    let seed = format!("{APP_ID}/github-sweep/{instance}/{}", ctx.now_ms());
    let cell = CellContext::build(ctx, &seed)?;
    let state_key = format!("ghstate/{DEFAULT_COMMUNITY}/{instance}").into_bytes();
    let mut state: BranchState = planes::load(ctx, &state_key).await?.unwrap_or_default();
    let before = state.clone();

    let rooms = rooms_to_answer(&state);
    // Rotated per sweep, for the reason `rotated` gives. The index is the cadence
    // WINDOW number — each sweep is its own call, so there is no counter to keep
    // across them, and the window itself numbers them.
    //
    // Reduced in `u64` BEFORE the conversion, so the conversion cannot fail and
    // fall back to a fixed order: the remainder is smaller than the room count,
    // which is a few hundred at most, whereas the window number is unbounded and
    // its width depends on the target. A fairness property must not rest on a
    // lossy cast.
    let cycle =
        usize::try_from((ctx.now_ms() / GITHUB_COMMAND_SWEEP_MS) % (rooms.len().max(1) as u64))
            .unwrap_or(0);
    let mut answered = 0usize;
    for (key, generation, live) in rotated(&rooms, cycle) {
        let budget = MAX_COMMANDS_PER_CYCLE.saturating_sub(answered);
        if budget == 0 {
            break;
        }
        let Some((stable_id, head, git_ref)) = parse_observation_key(&key) else {
            continue;
        };
        let identity = Identity::github(instance, &stable_id, head.as_deref(), &git_ref, &generation);
        let room = identity.room_id()?;
        let binding_id = identity.binding_id()?;
        let Some(visit) = visit_room(ctx, &cell, &room, &state).await? else {
            continue;
        };
        let mut done = BTreeSet::new();
        for work in &visit.owed {
            if done.len() >= budget {
                break;
            }
            let request = work.request();
            if let AnswerWork::Publish(_, record) = work {
                publish_answer(ctx, &cell, &room, &answer_key(&room, &request.id), record.clone()).await?;
                state.mark_answered(&request.id);
                done.insert(request.id.clone());
                continue;
            }
            // EVERY command is answered, including one whose envelope does not
            // parse: silence is the one outcome a requester cannot act on.
            let result = match Command::parse(&request.content) {
                Ok(_) if !live => {
                    retired_answer(&request.id, attempted_verb(&request.content), &binding_id)
                }
                Ok(command) if command.binding_id == binding_id => {
                    github_answer(&request.id, command.verb, &binding_id)
                }
                Ok(_) => CommandResult::new(
                    &request.id,
                    attempted_verb(&request.content),
                    &binding_id,
                    Outcome::Refused,
                    "this command names another binding",
                ),
                Err(e) => CommandResult::new(
                    &request.id,
                    attempted_verb(&request.content),
                    &binding_id,
                    Outcome::Refused,
                    &e.to_string(),
                ),
            };
            record_answer(ctx, &cell, &room, &result, &mut state).await?;
            done.insert(request.id.clone());
        }
        answered += done.len();
        let retired = (!live).then_some((key.as_str(), generation.as_str()));
        finish_visit(&mut state, &room, visit, &done, ctx.now_ms(), retired);
    }
    if state != before {
        state::save_commands(ctx, &state_key, &before, &state).await?;
    }
    Ok(answered)
}

/// A webhook delivery's durable claim.
///
/// TWO PHASE on purpose. A single claim taken before the work is done turns a
/// transient failure into permanent loss: the retry finds the claim, calls the
/// delivery a duplicate, and the projection never happens. So the claim records
/// `pending` first and is only marked `done` once the effects are published —
/// and it carries the FIRST attempt's instant, so a retry rebuilds
/// byte-identical events whose ids the relay has already admitted.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, ferrite_app::serde::Serialize, ferrite_app::serde::Deserialize,
)]
#[serde(crate = "ferrite_app::serde")]
pub(crate) struct DeliveryClaim {
    pub(crate) digest: String,
    /// Unix seconds of the FIRST attempt — reused by every retry.
    pub(crate) at: u64,
    pub(crate) done: bool,
    #[serde(default)]
    pub(crate) lease_until_ms: u64,
    #[serde(default)]
    pub(crate) epoch: u64,
    #[serde(default)]
    pub(crate) plan: Option<github::Plan>,
}

/// Admit, dedup, and project one webhook.
pub(crate) async fn handle_webhook(
    ctx: &AppContext,
    secret: &str,
    instance: &str,
    request: &IngressRequest,
    correlation: &str,
) -> Result<()> {
    let (delivery, payload) = github::admit(secret.as_bytes(), &request.headers, &request.body)?;
    let state_key = format!("ghstate/{DEFAULT_COMMUNITY}/{instance}").into_bytes();
    let Some(mut fence) = delivery::acquire(ctx, &delivery).await? else {
        return delivery::cleanup(ctx, &state_key, &delivery.uuid).await;
    };
    let plan = fence.plan(ctx, &delivery, &payload, instance, &state_key).await?;
    let created_at = fence.claim.at;
    let cell = CellContext::build(ctx, correlation)?;
    match plan.identity {
        None => {
            // No room, but community-global events (an issue, NIP-34 kind
            // 1621) are still published, without an `h` room tag.
            if plan.drafts.is_empty() {
                tracing::info!("delivery {} projected no room", delivery.uuid);
            } else {
                let seckey = planes::secret_key(ctx, SECRET_CELL_KEY)?;
                for draft in &plan.drafts {
                    let event = Event::sign(&seckey, draft.kind, draft.tags.clone(), &draft.content, created_at)?;
                    fence.renew(ctx).await?;
                    publish(ctx, &cell, &event).await?;
                }
            }
        }
        Some(identity) => {
            let name = identity
                .git_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(&identity.git_ref)
                .to_string();
            let display = payload
                .get("repository")
                .and_then(|r| r.get("full_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("github repository")
                .to_string();
            let clone_url = payload
                .get("repository")
                .and_then(|r| r.get("clone_url"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let display = format!("{display} · {name}");
            let room = identity.room_id()?;
            fence.renew(ctx).await?;
            create_room(ctx, &cell, &room, &display, created_at).await?;
            fence.renew(ctx).await?;
            let binding = reserve_binding_event(ctx, &cell, &identity, &display,
                clone_url.as_deref(), Lifecycle::Active, created_at, &format!("github/{}", delivery.uuid)).await?;
            fence.renew(ctx).await?;
            publish(ctx, &cell, &binding).await?;
            fence.renew(ctx).await?;
            say_at(ctx, &cell, &room, &plan.summary, binding.created_at).await?;
            let seckey = planes::secret_key(ctx, SECRET_CELL_KEY)?;
            for draft in &plan.drafts {
                let mut tags = draft.tags.clone();
                tags.push(vec!["h".to_string(), room.clone()]);
                let event = Event::sign(&seckey, draft.kind, tags, &draft.content, created_at)?;
                fence.renew(ctx).await?;
                publish(ctx, &cell, &event).await?;
            }
        }
    }
    fence.complete(ctx).await?;
    delivery::cleanup(ctx, &state_key, &delivery.uuid).await
}
