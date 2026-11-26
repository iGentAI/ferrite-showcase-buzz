use ferrite_app::{serde_json, AppContext};

use crate::cells::BranchState;
use crate::error::{ProcessError, Result};
use crate::{github, github_cell::DeliveryClaim, observation_key, planes};

const LEASE_MS: u64 = 60_000;

pub(super) struct Fence {
    key: Vec<u8>,
    pub claim: DeliveryClaim,
}

pub(super) async fn acquire(
    ctx: &AppContext,
    delivery: &github::Delivery,
) -> Result<Option<Fence>> {
    let key = delivery.key(crate::DEFAULT_COMMUNITY);
    for _ in 0..32 {
        let stored = planes::load_versioned::<DeliveryClaim>(ctx, &key).await?;
        let now = ctx.now_ms();
        let (mut claim, version) = match stored {
            Some((claim, version)) => {
                if claim.digest != delivery.body_sha256 {
                    return Err(ProcessError::Refused(
                        "delivery id reused with different bytes".into(),
                    ));
                }
                if claim.done {
                    return Ok(None);
                }
                if claim.lease_until_ms > now {
                    return Err(ProcessError::Storage(
                        "delivery is owned by an unexpired handler".into(),
                    ));
                }
                (claim, Some(version))
            }
            None => (
                DeliveryClaim {
                    digest: delivery.body_sha256.clone(),
                    at: now / 1000,
                    ..DeliveryClaim::default()
                },
                None,
            ),
        };
        claim.epoch = claim
            .epoch
            .checked_add(1)
            .ok_or_else(|| ProcessError::Storage("delivery epoch exhausted".into()))?;
        claim.lease_until_ms = now.saturating_add(LEASE_MS);
        if planes::save_if_version(ctx, &key, &claim, version).await? {
            return Ok(Some(Fence { key, claim }));
        }
    }
    Err(ProcessError::Storage(
        "delivery ownership remained contended".into(),
    ))
}

impl Fence {
    async fn update(
        &mut self,
        ctx: &AppContext,
        change: impl FnOnce(&mut DeliveryClaim),
    ) -> Result<()> {
        let (mut current, version) = planes::load_versioned::<DeliveryClaim>(ctx, &self.key)
            .await?
            .ok_or_else(|| ProcessError::Storage("delivery claim vanished".into()))?;
        if current.done
            || current.epoch != self.claim.epoch
            || current.lease_until_ms <= ctx.now_ms()
        {
            return Err(ProcessError::Storage(
                "delivery lease was lost or expired".into(),
            ));
        }
        current.lease_until_ms = ctx.now_ms().saturating_add(LEASE_MS);
        change(&mut current);
        if !planes::save_if_version(ctx, &self.key, &current, Some(version)).await? {
            return Err(ProcessError::Storage(
                "delivery lease changed during renewal".into(),
            ));
        }
        self.claim = current;
        Ok(())
    }

    pub async fn renew(&mut self, ctx: &AppContext) -> Result<()> {
        self.update(ctx, |_| {}).await
    }

    pub async fn complete(&mut self, ctx: &AppContext) -> Result<()> {
        self.update(ctx, |claim| claim.done = true).await
    }

    pub async fn plan(
        &mut self,
        ctx: &AppContext,
        delivery: &github::Delivery,
        payload: &serde_json::Value,
        instance: &str,
        state_key: &[u8],
    ) -> Result<github::Plan> {
        if let Some(plan) = &self.claim.plan {
            return Ok(plan.clone());
        }
        let id = &delivery.uuid;
        for _ in 0..32 {
            self.renew(ctx).await?;
            let stored = planes::load_versioned::<BranchState>(ctx, state_key).await?;
            let (mut state, version) = stored
                .map_or((BranchState::default(), None), |(state, version)| {
                    (state, Some(version))
                });
            let plan = match state.pending_delivery_plans.get(id) {
                Some(plan) => plan.clone(),
                None => {
                    let mut plan = github::plan(delivery, payload, instance, "g1")?;
                    if let Some(identity) = &mut plan.identity {
                        let observed = observation_key(
                            &identity.stable_id,
                            identity.head_stable_id.as_deref(),
                            &identity.git_ref,
                        );
                        // GitHub reports a deletion twice: a `delete` event
                        // and a `push` whose `deleted` is true. Both are
                        // absence; only the first retires the lifetime.
                        let deleting_push = delivery.event == "push"
                            && payload.get("deleted").and_then(serde_json::Value::as_bool) == Some(true);
                        let creates = matches!(delivery.event.as_str(), "push" | "create");
                        identity.generation = if delivery.event == "delete" || deleting_push {
                            let generation = state.generation(&observed);
                            if state.is_live(&observed) || delivery.event == "delete" {
                                state.note_absent(&observed);
                                state.note_retired(
                                    &observed,
                                    &generation,
                                    self.claim.at.saturating_mul(1000),
                                );
                            }
                            generation
                        } else if !creates {
                            // A check, review or pull-request event says
                            // nothing about whether the ref exists now: it
                            // neither resurrects a retired ref nor moves its
                            // tip.
                            state.generation(&observed)
                        } else {
                            let tip = payload
                                .get("after")
                                .and_then(serde_json::Value::as_str)
                                .or_else(|| {
                                    payload
                                        .get("pull_request")
                                        .and_then(|pr| pr.get("head"))
                                        .and_then(|head| head.get("sha"))
                                        .and_then(serde_json::Value::as_str)
                                })
                                .unwrap_or("unknown");
                            format!("g{}", state.note_present(&observed, tip))
                        };
                    }
                    state
                        .pending_delivery_plans
                        .insert(id.clone(), plan.clone());
                    self.renew(ctx).await?;
                    if !planes::save_if_version(ctx, state_key, &state, version).await? {
                        continue;
                    }
                    plan
                }
            };
            self.update(ctx, |claim| claim.plan = Some(plan.clone()))
                .await?;
            return Ok(plan);
        }
        Err(ProcessError::Storage(
            "delivery planning remained contended".into(),
        ))
    }
}

pub(super) async fn cleanup(ctx: &AppContext, state_key: &[u8], delivery_id: &str) -> Result<()> {
    for _ in 0..32 {
        let Some((mut state, version)) =
            planes::load_versioned::<BranchState>(ctx, state_key).await?
        else {
            return Ok(());
        };
        if state.pending_delivery_plans.remove(delivery_id).is_none() {
            return Ok(());
        }
        if planes::save_if_version(ctx, state_key, &state, Some(version)).await? {
            return Ok(());
        }
    }
    Err(ProcessError::Storage(
        "completed delivery cleanup remained contended".into(),
    ))
}
