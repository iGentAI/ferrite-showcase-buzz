//! Workflow gates: what runs, what is refused, and what an approval gate stops.
//!
//! Kind 30620 and the 46001–46012 lifecycle were declared and nothing executed
//! them, so a workflow could be written, stored, audited and delivered and would
//! never run. These gates assert the three properties that separate a working
//! executor from that, and each is chosen because it would be SILENTLY wrong
//! rather than loudly broken:
//!
//! * a triggering message actually produces the step's post;
//! * a definition that would trigger itself is refused at WRITE time, because a
//!   loop that is merely bounded still runs, emits, audits and delivers;
//! * an approval gate HALTS — a run that sailed past one would grant every
//!   approval it was supposed to wait for.

use super::common;

use std::future::Future;

use buzz::relay::channel::Visibility;
use buzz::relay::crypto::to_hex;
use buzz::relay::{kind, store, workflows};
use common::{
    Actor, COMMUNITY, FEED_GENERATION, T0_SECS, admission, body_json, gate_env, error_text, post,
    query, seed_channel_with_owner, signed, signed_at, submit, tag,
};
use ferrite_app::serde_json::{Value, json};
use common::SsoTestEnv as TestEnv;

const RELAY_IDENTITY_CONFIG: &[u8] = b"buzz-relay-identity";

/// Run the ingress scenarios on one bounded Tokio worker with the stack budget
/// their debug futures require. Construct the scenario on that worker and keep
/// its state on the heap; no fixture creates an operating-system thread directly.
fn run_workflow_scenario<F, Fut>(scenario: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let _guard = common::lock_scenario();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("workflow test runtime");
    runtime.block_on(async move {
        tokio::spawn(async move { Box::pin(scenario()).await })
            .await
            .expect("workflow scenario completes without a panic");
    });
}

/// The deployment's signing identity — a workflow step posts a RELAY-signed
/// message, so without one nothing can run at all.
async fn seed_relay_identity(env: &TestEnv) -> Actor {
    let relay = Actor::new("workflow-relay");
    let mut config = env.store("config");
    store::put_raw(
        &mut config,
        RELAY_IDENTITY_CONFIG,
        to_hex(&relay.sk).as_bytes(),
    )
    .await
    .expect("identity seed");
    relay
}

/// Publish a workflow definition into `channel`.
async fn define(
    env: &TestEnv,
    who: &Actor,
    channel: &str,
    id: &str,
    definition: Value,
) -> ferrite_app::IngressReply {
    let event = signed(
        who,
        kind::KIND_WORKFLOW_DEF,
        vec![tag("d", id), tag("h", channel)],
        &definition.to_string(),
    );
    submit(env, who, &event).await
}

/// Every message in `channel` that the RELAY signed — what a workflow step
/// posts, as distinct from what a person did.
async fn relay_posts(env: &TestEnv, who: &Actor, relay: &Actor, channel: &str) -> Vec<String> {
    let page = query(env, who, &json!({ "channel": channel, "limit": 100 })).await;
    body_json(&page)
        .get("events")
        .and_then(Value::as_array)
        .map(|events| {
            events
                .iter()
                .filter(|e| {
                    e.get("pubkey").and_then(Value::as_str) == Some(relay.pk_hex().as_str())
                        && e.get("kind").and_then(Value::as_u64)
                            == Some(u64::from(kind::KIND_STREAM_MESSAGE))
                })
                .filter_map(|e| e.get("content").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// **A message runs the workflow it triggers, and the step's post lands.**
#[test]
fn severe_a_triggering_message_runs_the_workflow() {
    run_workflow_scenario(|| async {
        let env = gate_env().await;
        let relay = seed_relay_identity(&env).await;
        let ana = Actor::new("workflow-ana");
        seed_channel_with_owner(&env, "ops", Visibility::Open, &ana).await;

        let defined = define(
            &env,
            &ana,
            "ops",
            "deploy-notice",
            json!({
                "name": "deploy notice",
                "on": { "contains": "ship it" },
                "steps": [{ "type": "post", "id": "say", "text": "starting the deployment" }],
            }),
        )
        .await;
        assert_eq!(defined.status, 200, "{}", error_text(&defined));

        // Nothing has run yet.
        assert!(relay_posts(&env, &ana, &relay, "ops").await.is_empty());

        let trigger = signed(
            &ana,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", "ops")],
            "ok everyone, ship it",
        );
        let posted = submit(&env, &ana, &trigger).await;
        assert_eq!(posted.status, 200, "{}", error_text(&posted));

        let said = relay_posts(&env, &ana, &relay, "ops").await;
        assert!(
            said.iter().any(|text| text == "starting the deployment"),
            "the workflow was defined and triggered and posted nothing: {said:?}"
        );

        // A message that does NOT match runs nothing further.
        let quiet = signed(
            &ana,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", "ops")],
            "unrelated chatter",
        );
        assert_eq!(submit(&env, &ana, &quiet).await.status, 200);
        let after = relay_posts(&env, &ana, &relay, "ops").await;
        assert_eq!(
            after
                .iter()
                .filter(|t| *t == "starting the deployment")
                .count(),
            1,
            "a non-matching message triggered the workflow: {after:?}"
        );
    });
}

/// **A workflow that would trigger itself is refused when it is WRITTEN.**
///
/// Refusing at run time would still mean it ran: emitted, audited, delivered, and
/// only then stopped by a step cap. The cap exists as a backstop; this is the
/// rule.
#[test]
fn severe_a_self_triggering_workflow_is_refused_at_write_time() {
    run_workflow_scenario(|| async {
        let env = gate_env().await;
        seed_relay_identity(&env).await;
        let ana = Actor::new("workflow-ana");
        seed_channel_with_owner(&env, "loops", Visibility::Open, &ana).await;

        let refused = define(
            &env,
            &ana,
            "loops",
            "self-trigger",
            json!({
                "name": "loop",
                "on": { "contains": "ship it" },
                "steps": [{ "type": "post", "id": "say", "text": "ship it again" }],
            }),
        )
        .await;
        assert_eq!(refused.status, 400, "{}", error_text(&refused));
        assert!(
            error_text(&refused).contains("trigger it again"),
            "the refusal should name what is wrong: {}",
            error_text(&refused)
        );

        // The same steps WITHOUT a trigger are fine — it cannot fire itself if
        // nothing fires it.
        let manual = define(
            &env,
            &ana,
            "loops",
            "manual-only",
            json!({
                "name": "manual",
                "steps": [{ "type": "post", "id": "say", "text": "ship it again" }],
            }),
        )
        .await;
        assert_eq!(manual.status, 200, "{}", error_text(&manual));
    });
}

/// **A workflow id in a channel belongs to its author, and unsafe `d` tags are refused.**
#[test]
fn severe_workflow_id_cannot_be_hijacked_by_another_author_or_use_unsafe_d_tag() {
    run_workflow_scenario(|| async {
        let env = gate_env().await;
        let relay = seed_relay_identity(&env).await;
        let ana = Actor::new("workflow-owner-ana");
        let bob = Actor::new("workflow-peer-bob");
        seed_channel_with_owner(&env, "owned-wf", Visibility::Open, &ana).await;

        let bad_d = define(
            &env,
            &ana,
            "owned-wf",
            "bad/slash",
            json!({
                "name": "bad",
                "steps": [{ "type": "post", "id": "s1", "text": "hi" }],
            }),
        )
        .await;
        assert_eq!(bad_d.status, 400, "slash in d tag must be refused at ingest");

        let ok = define(
            &env,
            &ana,
            "owned-wf",
            "release-bot",
            json!({
                "name": "release",
                "on": { "contains": "run release-bot" },
                "steps": [{ "type": "post", "id": "s1", "text": "ana's workflow" }],
            }),
        )
        .await;
        assert_eq!(ok.status, 200, "{}", error_text(&ok));

        // Another author submitting a definition with the same `d` tag is refused
        // at admission with HTTP 403 Unauthorized.
        let hijack_event = signed(
            &bob,
            kind::KIND_WORKFLOW_DEF,
            vec![tag("h", "owned-wf"), tag("d", "release-bot")],
            &json!({
                "name": "hijacked",
                "on": { "contains": "run release-bot" },
                "steps": [{ "type": "post", "id": "s1", "text": "bob's workflow" }],
            })
            .to_string(),
        );
        let hijack_resp = submit(&env, &bob, &hijack_event).await;
        assert_eq!(
            hijack_resp.status,
            403,
            "hijack attempt must be refused with 403: {}",
            error_text(&hijack_resp)
        );

        // Separate replay/direct-invocation protection check on `on_definition`:
        // even if `on_definition` is invoked directly with another author's event,
        // or if a legacy index entry points to a missing event whose owner/version
        // cannot be determined, replacement is refused.
        {
            let mut events = env.store("events");
            let mut ape = env.ape("guards");
            let hijack_on_def = workflows::on_definition(
                &mut events,
                &mut ape,
                COMMUNITY,
                &hijack_event,
                Some("owned-wf"),
            )
            .await;
            assert!(
                matches!(
                    hijack_on_def,
                    Err(buzz::relay::error::BuzzError::Unauthorized(_))
                ),
                "on_definition replay check must refuse unauthorized author: {hijack_on_def:?}"
            );

            let legacy_key =
                buzz::relay::keys::workflow_definition_index(COMMUNITY, "owned-wf", "legacy-bot");
            let missing_event_hex = "deadbeef".repeat(8);
            store::put_raw(&mut events, &legacy_key, missing_event_hex.as_bytes())
                .await
                .expect("seed legacy index entry without event body");
            let legacy_hijack = signed(
                &bob,
                kind::KIND_WORKFLOW_DEF,
                vec![tag("h", "owned-wf"), tag("d", "legacy-bot")],
                &json!({
                    "name": "legacy-hijack",
                    "on": { "contains": "run legacy-bot" },
                    "steps": [{ "type": "post", "id": "s1", "text": "bob's legacy hijack" }],
                })
                .to_string(),
            );
            let legacy_resp = submit(&env, &bob, &legacy_hijack).await;
            assert_eq!(
                legacy_resp.status,
                403,
                "legacy index entry with missing event must refuse replacement at admission: {}",
                error_text(&legacy_resp)
            );
            let legacy_on_def = workflows::on_definition(
                &mut events,
                &mut ape,
                COMMUNITY,
                &legacy_hijack,
                Some("owned-wf"),
            )
            .await;
            assert!(
                matches!(
                    legacy_on_def,
                    Err(buzz::relay::error::BuzzError::Unauthorized(_))
                ),
                "on_definition on unverified legacy entry must refuse: {legacy_on_def:?}"
            );
            let after_raw = store::get_raw(&mut events, &legacy_key)
                .await
                .expect("read legacy key")
                .expect("legacy key still present");
            assert_eq!(
                after_raw,
                missing_event_hex.as_bytes(),
                "on_definition must not overwrite a legacy index entry whose owner/version cannot be determined"
            );
        }

        let trigger = signed(
            &ana,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", "owned-wf")],
            "please run release-bot now",
        );
        assert_eq!(submit(&env, &ana, &trigger).await.status, 200);
        let said = relay_posts(&env, &ana, &relay, "owned-wf").await;
        assert!(
            said.iter().any(|t| t == "ana's workflow"),
            "original author's workflow must remain active: {said:?}"
        );
        assert!(
            !said.iter().any(|t| t == "bob's workflow"),
            "another author must not hijack an existing workflow id in the channel: {said:?}"
        );
    });
}

/// **A pre-upgrade admission without an ownership guard meeting a competing
/// author on the same workflow ID publishes at most one definition.**
///
/// Covers both race orderings when an unfinished admission (`admit/{c}/{id}`
/// written before the `wfown/{c}/{ch}/{wf}` guard existed, with no `wfdef` row
/// yet) meets a competing author on the same workflow ID:
/// 1. When the pre-upgrade admission re-admits first, it claims `wfown` on the
///    duplicate path before driving, and the competing author is refused with
///    `403 Unauthorized`.
/// 2. When the competing author submits first and claims `wfown`, resuming the
///    pre-upgrade admission — whether via `submit` or directly via
///    `ingest::drive` — is refused with `403 Unauthorized` before storing or
///    indexing a second definition.
#[test]
fn severe_pre_upgrade_admission_without_guard_and_competing_author_publish_at_most_one_definition()
{
    run_workflow_scenario(|| async {
        let env = gate_env().await;
        let relay = seed_relay_identity(&env).await;
        let ana = Actor::new("workflow-preupgrade-ana");
        let bob = Actor::new("workflow-preupgrade-bob");
        seed_channel_with_owner(&env, "preupgrade-wf", Visibility::Open, &ana).await;

        // Case 1: Pre-upgrade unfinished admission for `ana` exists without
        // `wfown` or `wfdef`. Re-submitting `ana`'s event claims `wfown` on the
        // duplicate path and drives to completion; `bob` is refused with 403.
        let ana_def_1 = signed(
            &ana,
            kind::KIND_WORKFLOW_DEF,
            vec![tag("h", "preupgrade-wf"), tag("d", "race-1")],
            &json!({
                "name": "ana-race-1",
                "on": { "contains": "fire race-1" },
                "steps": [{ "type": "post", "id": "s1", "text": "ana won race-1" }],
            })
            .to_string(),
        );
        let bob_def_1 = signed(
            &bob,
            kind::KIND_WORKFLOW_DEF,
            vec![tag("h", "preupgrade-wf"), tag("d", "race-1")],
            &json!({
                "name": "bob-race-1",
                "on": { "contains": "fire race-1" },
                "steps": [{ "type": "post", "id": "s1", "text": "bob won race-1" }],
            })
            .to_string(),
        );
        {
            let mut ape = env.ape("guards");
            let admit_key = buzz::relay::keys::admit(COMMUNITY, &ana_def_1.id);
            let pre_doc = buzz::relay::ingest::Admission::new(
                &ana_def_1,
                COMMUNITY,
                Some("preupgrade-wf".into()),
                env.now_ms(),
            );
            let raw = pre_doc.encode().expect("encode pre-upgrade admission");
            ape.commit_n_if(&admit_key, None, &[(admit_key.as_slice(), raw.as_slice())])
                .await
                .expect("seed pre-upgrade admission")
                .expect("pre-upgrade admission absent");
        }

        let ana_resp_1 = submit(&env, &ana, &ana_def_1).await;
        assert_eq!(ana_resp_1.status, 200, "{}", error_text(&ana_resp_1));
        let bob_resp_1 = submit(&env, &bob, &bob_def_1).await;
        assert_eq!(
            bob_resp_1.status,
            403,
            "competing author must be refused after pre-upgrade admission claims ownership: {}",
            error_text(&bob_resp_1)
        );

        // Case 2: Pre-upgrade unfinished admission for `ana` exists without
        // `wfown` or `wfdef`, and `bob` submits first and claims `wfown`.
        // Resuming `ana`'s pre-upgrade admission — both directly via
        // `ingest::drive` and via `submit` — must refuse with `Unauthorized`
        // and never store or index `ana`'s definition.
        let ana_def_2 = signed(
            &ana,
            kind::KIND_WORKFLOW_DEF,
            vec![tag("h", "preupgrade-wf"), tag("d", "race-2")],
            &json!({
                "name": "ana-race-2",
                "on": { "contains": "fire race-2" },
                "steps": [{ "type": "post", "id": "s1", "text": "ana won race-2" }],
            })
            .to_string(),
        );
        let bob_def_2 = signed(
            &bob,
            kind::KIND_WORKFLOW_DEF,
            vec![tag("h", "preupgrade-wf"), tag("d", "race-2")],
            &json!({
                "name": "bob-race-2",
                "on": { "contains": "fire race-2" },
                "steps": [{ "type": "post", "id": "s1", "text": "bob won race-2" }],
            })
            .to_string(),
        );
        let pre_outcome_2 = {
            let mut ape = env.ape("guards");
            let admit_key = buzz::relay::keys::admit(COMMUNITY, &ana_def_2.id);
            let pre_doc = buzz::relay::ingest::Admission::new(
                &ana_def_2,
                COMMUNITY,
                Some("preupgrade-wf".into()),
                env.now_ms(),
            );
            let raw = pre_doc.encode().expect("encode pre-upgrade admission 2");
            ape.commit_n_if(&admit_key, None, &[(admit_key.as_slice(), raw.as_slice())])
                .await
                .expect("seed pre-upgrade admission 2")
                .expect("pre-upgrade admission 2 absent");
            buzz::relay::ingest::AdmitOutcome::Duplicate {
                doc: pre_doc,
                raw,
            }
        };

        let bob_resp_2 = submit(&env, &bob, &bob_def_2).await;
        assert_eq!(bob_resp_2.status, 200, "{}", error_text(&bob_resp_2));

        // Direct saga resume (`ingest::drive`) of the unguarded pre-upgrade
        // admission must fail closed with `Unauthorized` before storing `evt/`.
        {
            let mut events = env.store("events");
            let mut ape = env.ape("guards");
            let mut topics = env.topics("feed");
            let drive_res = buzz::relay::ingest::drive(
                &mut events,
                &mut ape,
                &mut topics,
                COMMUNITY,
                FEED_GENERATION,
                &ana_def_2,
                &pre_outcome_2,
                env.now_ms(),
                None,
            )
            .await;
            assert!(
                matches!(
                    drive_res,
                    Err(buzz::relay::error::BuzzError::Unauthorized(_))
                ),
                "direct drive of unowned pre-upgrade admission must fail Unauthorized: {drive_res:?}"
            );
            let ana_evt = store::get_raw(
                &mut events,
                &buzz::relay::keys::evt(COMMUNITY, &ana_def_2.id),
            )
            .await
            .expect("check ana_def_2 event body");
            assert!(
                ana_evt.is_none(),
                "unowned pre-upgrade admission must not persist its event body"
            );

            // Also test resuming from a later checkpoint (`SagaState::Complete`
            // with `audit_seq` populated): `drive` must still enforce ownership
            // at entry before returning `Ok(doc.audit_seq)`.
            let mut completed_doc = pre_outcome_2.doc().clone();
            completed_doc.state = buzz::relay::ingest::SagaState::Complete;
            completed_doc.audit_seq = Some(1);
            let completed_raw = completed_doc
                .encode()
                .expect("encode completed pre-upgrade admission");
            let completed_outcome = buzz::relay::ingest::AdmitOutcome::Duplicate {
                doc: completed_doc,
                raw: completed_raw,
            };
            let completed_drive_res = buzz::relay::ingest::drive(
                &mut events,
                &mut ape,
                &mut topics,
                COMMUNITY,
                FEED_GENERATION,
                &ana_def_2,
                &completed_outcome,
                env.now_ms(),
                None,
            )
            .await;
            assert!(
                matches!(
                    completed_drive_res,
                    Err(buzz::relay::error::BuzzError::Unauthorized(_))
                ),
                "direct drive of unowned pre-upgrade admission at later checkpoint must fail Unauthorized: {completed_drive_res:?}"
            );
        }

        // Re-submitting `ana_def_2` through ingress must also be refused with 403.
        let ana_resp_2 = submit(&env, &ana, &ana_def_2).await;
        assert_eq!(
            ana_resp_2.status,
            403,
            "re-submitting unowned pre-upgrade admission must return 403: {}",
            error_text(&ana_resp_2)
        );

        let trigger = signed(
            &ana,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", "preupgrade-wf")],
            "please fire race-1 and fire race-2 now",
        );
        assert_eq!(submit(&env, &ana, &trigger).await.status, 200);
        let said = relay_posts(&env, &ana, &relay, "preupgrade-wf").await;
        assert!(
            said.iter().any(|t| t == "ana won race-1")
                && !said.iter().any(|t| t == "bob won race-1"),
            "race-1 must publish only ana's definition: {said:?}"
        );
        assert!(
            said.iter().any(|t| t == "bob won race-2")
                && !said.iter().any(|t| t == "ana won race-2"),
            "race-2 must publish only bob's definition: {said:?}"
        );
    });
}


/// **An approval gate halts the run**, and a decision resumes it.
///
/// The halt is the whole point of a gate: a run that walked past one would
/// perform every step it was supposed to wait for, which is the most expensive
/// possible way to be wrong.
#[test]
fn severe_an_approval_gate_halts_the_run_until_it_is_decided() {
    run_workflow_scenario(|| async {
        let env = gate_env().await;
        let relay = seed_relay_identity(&env).await;
        let ana = Actor::new("workflow-ana");
        seed_channel_with_owner(&env, "gated", Visibility::Open, &ana).await;

        let defined = define(
            &env,
            &ana,
            "gated",
            "needs-approval",
            json!({
                "name": "gated deploy",
                // The trigger phrase deliberately appears in NEITHER step's text:
                // a workflow whose own post would re-fire it is refused at write
                // time, which this fixture originally tripped over.
                "on": { "contains": "cut a build" },
                "steps": [
                    { "type": "post", "id": "before", "text": "preparing the release" },
                    { "type": "approval", "id": "gate", "approver": ana.pk_hex() },
                    { "type": "post", "id": "after", "text": "the release is out" },
                ],
            }),
        )
        .await;
        assert_eq!(defined.status, 200, "{}", error_text(&defined));

        let trigger = signed(
            &ana,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", "gated")],
            "time to cut a build",
        );
        assert_eq!(submit(&env, &ana, &trigger).await.status, 200);

        // The step BEFORE the gate ran; the one after it did not.
        let said = relay_posts(&env, &ana, &relay, "gated").await;
        assert!(
            said.iter().any(|t| t == "preparing the release"),
            "the pre-gate step did not run: {said:?}"
        );
        assert!(
            !said.iter().any(|t| t == "the release is out"),
            "the run walked straight through its approval gate: {said:?}"
        );

        // The run is parked, and its gate carries a token.
        let runs = {
            let mut events = env.store("events");
            workflows::runs_of(&mut events, COMMUNITY, "needs-approval", 10)
                .await
                .expect("runs read")
        };
        assert_eq!(runs.len(), 1, "one trigger, one run");
        assert_eq!(runs[0].status, workflows::RunStatus::AwaitingApproval);

        // The token is on the relay-signed request event, which is how a client
        // learns what to approve. When the approver is a pubkey, the request also
        // carries a `p` tag so it indexes into the approver's `needs_action` feed.
        let page = query(&env, &ana, &json!({ "channel": "gated", "limit": 100 })).await;
        let req_event = body_json(&page)
            .get("events")
            .and_then(Value::as_array)
            .and_then(|events| {
                events
                    .iter()
                    .find(|e| {
                        e.get("kind").and_then(Value::as_u64)
                            == Some(u64::from(kind::KIND_WORKFLOW_APPROVAL_REQUESTED))
                    })
                    .cloned()
            })
            .expect("the gate published an approval request event");
        let has_p_tag = req_event
            .get("tags")
            .and_then(Value::as_array)
            .is_some_and(|tags| {
                tags.iter().any(|t| {
                    t.as_array().is_some_and(|arr| {
                        arr.first().and_then(Value::as_str) == Some("p")
                            && arr.get(1).and_then(Value::as_str) == Some(ana.pk_hex().as_str())
                    })
                })
            });
        assert!(has_p_tag, "approval request for a pubkey approver must carry a p tag");
        let token = req_event
            .get("tags")
            .and_then(Value::as_array)
            .and_then(|tags| {
                tags.iter().find_map(|t| {
                    let t = t.as_array()?;
                    (t.first()?.as_str()? == "token").then(|| t.get(1)?.as_str())?
                })
            })
            .map(str::to_string)
            .expect("the gate published a token");

        // Approving resumes it, and the remaining step runs.
        let granted = signed(
            &ana,
            kind::KIND_WORKFLOW_APPROVAL_GRANTED,
            vec![tag("token", &token)],
            "looks good",
        );
        let decided = submit(&env, &ana, &granted).await;
        assert_eq!(decided.status, 200, "{}", error_text(&decided));

        let said = relay_posts(&env, &ana, &relay, "gated").await;
        assert!(
            said.iter().any(|t| t == "the release is out"),
            "the approval did not resume the run: {said:?}"
        );
        let runs = {
            let mut events = env.store("events");
            workflows::runs_of(&mut events, COMMUNITY, "needs-approval", 10)
                .await
                .expect("runs read")
        };
        assert_eq!(runs[0].status, workflows::RunStatus::Completed);
    });
}

/// **Holding the broadcast token does not let a non-approver drive a run.**
///
/// The approval request puts its token in a tag every reader of the room sees.
/// A run that is Running while its gate row still exists (a driver that crashed
/// after the grant's CAS and before consuming the gate) used to be re-executed
/// for ANY decision presenting that token, before the approver check. Now the
/// re-entry asks for the gate's approver like the first grant does.
#[test]
fn severe_the_broadcast_token_alone_cannot_resume_a_run() {
    run_workflow_scenario(|| async {
        let env = gate_env().await;
        let relay = seed_relay_identity(&env).await;
        let ana = Actor::new("workflow-ana");
        let mallory = Actor::new("workflow-mallory");
        seed_channel_with_owner(&env, "gated2", Visibility::Open, &ana).await;

        let defined = define(
            &env,
            &ana,
            "gated2",
            "needs-approval-2",
            json!({
                "name": "gated deploy",
                "on": { "contains": "cut a build" },
                "steps": [
                    { "type": "approval", "id": "gate", "approver": ana.pk_hex() },
                    { "type": "post", "id": "after", "text": "the release is out" },
                ],
            }),
        )
        .await;
        assert_eq!(defined.status, 200, "{}", error_text(&defined));
        let trigger = signed(
            &ana,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", "gated2")],
            "time to cut a build",
        );
        assert_eq!(submit(&env, &ana, &trigger).await.status, 200);

        let token = {
            let page = query(&env, &mallory, &json!({ "channel": "gated2", "limit": 100 })).await;
            body_json(&page)["events"]
                .as_array()
                .and_then(|events| {
                    events.iter().find_map(|e| {
                        if e["kind"].as_u64()
                            != Some(u64::from(kind::KIND_WORKFLOW_APPROVAL_REQUESTED))
                        {
                            return None;
                        }
                        e["tags"].as_array()?.iter().find_map(|t| {
                            let t = t.as_array()?;
                            (t.first()?.as_str()? == "token").then(|| t.get(1)?.as_str())?
                        })
                    })
                })
                .map(str::to_string)
                .expect("a room member can read the broadcast token")
        };

        // Stand the run in the crash window: Running past the gate, gate row
        // still present.
        let mut events = env.store("events");
        let mut run = workflows::runs_of(&mut events, COMMUNITY, "needs-approval-2", 10)
            .await
            .expect("runs read")
            .remove(0);
        run.status = workflows::RunStatus::Running;
        run.next_step = 1;
        store::put_doc(
            &mut events,
            &buzz::relay::keys::workflow_run(COMMUNITY, &run.id),
            &run,
        )
        .await
        .expect("stand the run in the crash window");

        let forged = signed(
            &mallory,
            kind::KIND_WORKFLOW_APPROVAL_GRANTED,
            vec![tag("token", &token)],
            "",
        );
        let _ = submit(&env, &mallory, &forged).await;
        let said = relay_posts(&env, &ana, &relay, "gated2").await;
        assert!(
            !said.iter().any(|t| t == "the release is out"),
            "a non-approver holding the broadcast token drove the run: {said:?}"
        );

        // The named approver still recovers it.
        let granted = signed(
            &ana,
            kind::KIND_WORKFLOW_APPROVAL_GRANTED,
            vec![tag("token", &token)],
            "",
        );
        assert_eq!(submit(&env, &ana, &granted).await.status, 200);
        let said = relay_posts(&env, &ana, &relay, "gated2").await;
        assert!(
            said.iter().any(|t| t == "the release is out"),
            "the approver could not recover the run: {said:?}"
        );
    });
}

/// **One message cannot fan out past one workflow's worth of steps.**
///
/// Each run is capped, and so is the number of definitions read, but the product
/// let one rate-limited message start 32 × 32 steps of relay-signed events. The
/// steps a single trigger may start are now bounded across all workflows it
/// matches, so a second matching workflow that would exceed the bound is not run.
#[test]
fn severe_one_message_starts_at_most_one_workflows_worth_of_steps() {
    run_workflow_scenario(|| async {
        let env = gate_env().await;
        let relay = seed_relay_identity(&env).await;
        let ana = Actor::new("workflow-ana");
        seed_channel_with_owner(&env, "fanout", Visibility::Open, &ana).await;

        let per_workflow = workflows::MAX_TRIGGER_STEPS / 2 + 1;
        for name in ["fan-a", "fan-b"] {
            let steps: Vec<Value> = (0..per_workflow)
                .map(|i| json!({ "type": "post", "id": format!("s{i}"), "text": format!("{name} step {i}") }))
                .collect();
            let defined = define(
                &env,
                &ana,
                "fanout",
                name,
                json!({ "name": name, "on": { "contains": "xyzzy" }, "steps": steps }),
            )
            .await;
            assert_eq!(defined.status, 200, "{}", error_text(&defined));
        }

        let trigger = signed(&ana, kind::KIND_STREAM_MESSAGE, vec![tag("h", "fanout")], "xyzzy");
        assert_eq!(submit(&env, &ana, &trigger).await.status, 200);

        let said = relay_posts(&env, &ana, &relay, "fanout").await;
        assert_eq!(
            said.len(),
            per_workflow,
            "one message must start one workflow's worth of steps, not both: {said:?}"
        );
        let mut events = env.store("events");
        let mut runs = 0;
        for name in ["fan-a", "fan-b"] {
            runs += workflows::runs_of(&mut events, COMMUNITY, name, 10)
                .await
                .expect("runs read")
                .len();
        }
        assert_eq!(runs, 1, "the workflow past the budget must not start a run");
    });
}

/// **A workflow in one channel does not run on another channel's messages.**
#[test]
fn severe_a_workflow_is_scoped_to_its_own_channel() {
    run_workflow_scenario(|| async {
        let env = gate_env().await;
        let relay = seed_relay_identity(&env).await;
        let ana = Actor::new("workflow-ana");
        seed_channel_with_owner(&env, "here", Visibility::Open, &ana).await;
        seed_channel_with_owner(&env, "elsewhere", Visibility::Open, &ana).await;

        assert_eq!(
            define(
                &env,
                &ana,
                "here",
                "scoped",
                json!({
                    "name": "scoped",
                    "on": { "contains": "go" },
                    "steps": [{ "type": "post", "id": "say", "text": "ran in here" }],
                }),
            )
            .await
            .status,
            200
        );

        // The same phrase in the OTHER room must run nothing.
        let elsewhere = signed(
            &ana,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", "elsewhere")],
            "go go go",
        );
        assert_eq!(submit(&env, &ana, &elsewhere).await.status, 200);
        assert!(
            relay_posts(&env, &ana, &relay, "elsewhere")
                .await
                .is_empty(),
            "a workflow ran on another channel's message"
        );

        // The positive half of the boundary is already exercised independently by
        // `severe_a_triggering_message_runs_the_workflow`. Repeating it in this
        // same async test made the debug-build future exceed the test thread's
        // stack as the ingress state machine grew, aborting the gate before it
        // could report a verdict. Keep this case focused on the cross-channel
        // refusal it uniquely proves.
    });
}

/// Publish a workflow definition into `channel` at an explicit `created_at`, so a
/// later edit of the same `d` strictly replaces it.
async fn define_at(
    env: &TestEnv,
    who: &Actor,
    channel: &str,
    id: &str,
    definition: Value,
    created_at: u64,
) -> ferrite_app::IngressReply {
    let event = signed_at(
        who,
        kind::KIND_WORKFLOW_DEF,
        vec![tag("d", id), tag("h", channel)],
        &definition.to_string(),
        created_at,
    );
    submit(env, who, &event).await
}

/// Re-drive a trigger the way a redelivered ingest saga does: the same entry
/// point, the same event, with the relay's identity.
async fn redrive(
    env: &TestEnv,
    relay: &Actor,
    event: &buzz::relay::event::Event,
    channel: &str,
    manual: bool,
) {
    let ctx = env.context();
    let mut events = env.store("events");
    let mut config = env.store("config");
    let mut ape = ctx.ape("guards").expect("guards plane");
    let mut topics = ctx.topics("feed").expect("feed plane");
    let now_ms = env.now_ms();
    let driven = if manual {
        workflows::trigger_now(
            &mut events,
            &mut ape,
            &mut topics,
            &mut config,
            COMMUNITY,
            FEED_GENERATION,
            event,
            Some(channel),
            Some(&relay.sk),
            now_ms,
        )
        .await
    } else {
        workflows::maybe_trigger(
            &mut events,
            &mut ape,
            &mut topics,
            &mut config,
            COMMUNITY,
            FEED_GENERATION,
            event,
            Some(channel),
            Some(&relay.sk),
            now_ms,
        )
        .await
    };
    driven.expect("the re-drive completes");
}

/// **A re-driven trigger starts exactly what its first pass admitted.**
///
/// The per-message step budget was recomputed on every pass from the channel's
/// CURRENT definitions. Here the first pass starts `fan-a` and refuses `fan-b`
/// (together they exceed the budget); `fan-a` is then shortened, and the same
/// trigger is re-driven, as a redelivered saga would after a crash. Recomputed,
/// `fan-a` now looked cheap enough for `fan-b` to fit, so one message started
/// both — more than [`workflows::MAX_TRIGGER_STEPS`] across its passes. The
/// admission is now recorded once and replayed.
#[test]
fn severe_a_redriven_trigger_replays_its_first_admission() {
    run_workflow_scenario(|| async {
        let env = gate_env().await;
        let relay = seed_relay_identity(&env).await;
        let ana = Actor::new("workflow-ana");
        seed_channel_with_owner(&env, "replay", Visibility::Open, &ana).await;

        let per_workflow = workflows::MAX_TRIGGER_STEPS / 2 + 1;
        for name in ["fan-a", "fan-b"] {
            let steps: Vec<Value> = (0..per_workflow)
                .map(|i| json!({ "type": "post", "id": format!("s{i}"), "text": format!("{name} step {i}") }))
                .collect();
            let defined = define_at(
                &env,
                &ana,
                "replay",
                name,
                json!({ "name": name, "on": { "contains": "xyzzy" }, "steps": steps }),
                T0_SECS - 100,
            )
            .await;
            assert_eq!(defined.status, 200, "{}", error_text(&defined));
        }
        let trigger = signed_at(
            &ana,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", "replay")],
            "xyzzy",
            T0_SECS - 50,
        );
        assert_eq!(submit(&env, &ana, &trigger).await.status, 200);
        assert_eq!(relay_posts(&env, &ana, &relay, "replay").await.len(), per_workflow);

        // `fan-a` is edited down to one step after the trigger ran.
        let edited = define_at(
            &env,
            &ana,
            "replay",
            "fan-a",
            json!({
                "name": "fan-a",
                "on": { "contains": "xyzzy" },
                "steps": [{ "type": "post", "id": "only", "text": "fan-a shortened" }],
            }),
            T0_SECS,
        )
        .await;
        assert_eq!(edited.status, 200, "{}", error_text(&edited));

        redrive(&env, &relay, &trigger, "replay", false).await;

        let said = relay_posts(&env, &ana, &relay, "replay").await;
        assert_eq!(
            said.len(),
            per_workflow,
            "a re-drive must not start a workflow the first pass refused: {said:?}"
        );
        let mut events = env.store("events");
        let fan_b = workflows::runs_of(&mut events, COMMUNITY, "fan-b", 10)
            .await
            .expect("runs read");
        assert!(fan_b.is_empty(), "fan-b was started on a re-drive: {fan_b:?}");
    });
}

/// The approval token a gate published in `channel`.
async fn gate_token(env: &TestEnv, who: &Actor, channel: &str) -> String {
    let page = query(env, who, &json!({ "channel": channel, "limit": 100 })).await;
    body_json(&page)["events"]
        .as_array()
        .and_then(|events| {
            events.iter().find_map(|e| {
                if e["kind"].as_u64() != Some(u64::from(kind::KIND_WORKFLOW_APPROVAL_REQUESTED)) {
                    return None;
                }
                e["tags"].as_array()?.iter().find_map(|t| {
                    let t = t.as_array()?;
                    (t.first()?.as_str()? == "token").then(|| t.get(1)?.as_str())?
                })
            })
        })
        .map(str::to_string)
        .expect("the gate published a token")
}

/// Drive one gated run to completion from `start`, then stand it back in the
/// crash window after its grant (Running at the post-gate step, `resumed_at`
/// kept), re-drive `start`, and require that nothing was posted twice.
async fn redriven_past_a_gate_posts_once(manual: bool) {
    let env = gate_env().await;
    let relay = seed_relay_identity(&env).await;
    let ana = Actor::new("workflow-ana");
    let channel = if manual { "resume-manual" } else { "resume-message" };
    let workflow = if manual { "gated-manual" } else { "gated-message" };
    seed_channel_with_owner(&env, channel, Visibility::Open, &ana).await;
    let defined = define_at(
        &env,
        &ana,
        channel,
        workflow,
        json!({
            "name": "gated",
            "on": { "contains": "cut a build" },
            "steps": [
                { "type": "post", "id": "before", "text": "preparing the release" },
                { "type": "approval", "id": "gate", "approver": ana.pk_hex() },
                { "type": "post", "id": "after", "text": "the release is out" },
            ],
        }),
        T0_SECS - 700,
    )
    .await;
    assert_eq!(defined.status, 200, "{}", error_text(&defined));

    // The trigger and the grant carry DIFFERENT instants, so a re-drive that
    // stamped the post-gate step with the trigger's instant would mint a new id.
    let start = if manual {
        signed_at(
            &ana,
            kind::KIND_WORKFLOW_TRIGGER,
            vec![tag("h", channel), tag("workflow", workflow)],
            "",
            T0_SECS - 600,
        )
    } else {
        signed_at(
            &ana,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", channel)],
            "time to cut a build",
            T0_SECS - 600,
        )
    };
    let started = submit(&env, &ana, &start).await;
    assert_eq!(started.status, 200, "{}", error_text(&started));
    let token = gate_token(&env, &ana, channel).await;
    let granted = signed(
        &ana,
        kind::KIND_WORKFLOW_APPROVAL_GRANTED,
        vec![tag("token", &token)],
        "",
    );
    assert_eq!(submit(&env, &ana, &granted).await.status, 200);
    let out = |said: &[String]| said.iter().filter(|t| *t == "the release is out").count();
    assert_eq!(out(&relay_posts(&env, &ana, &relay, channel).await), 1);

    let mut events = env.store("events");
    let mut run = workflows::runs_of(&mut events, COMMUNITY, workflow, 10)
        .await
        .expect("runs read")
        .remove(0);
    assert_eq!(run.resumed_at, Some(T0_SECS), "the grant recorded its instant");
    run.status = workflows::RunStatus::Running;
    run.next_step = 2;
    run.completed_at = None;
    store::put_doc(&mut events, &buzz::relay::keys::workflow_run(COMMUNITY, &run.id), &run)
        .await
        .expect("stand the run in the crash window");

    redrive(&env, &relay, &start, channel, manual).await;

    let said = relay_posts(&env, &ana, &relay, channel).await;
    assert_eq!(
        out(&said),
        1,
        "the re-drive re-minted the post-gate step under a new id: {said:?}"
    );
    let run = workflows::read_run(&mut events, COMMUNITY, &run.id)
        .await
        .expect("run read")
        .expect("the run");
    assert_eq!(run.status, workflows::RunStatus::Completed);
}

/// **A run resumed past a gate re-drives to the same event ids, from a message.**
#[test]
fn severe_a_message_redrive_past_a_gate_mints_the_same_ids() {
    run_workflow_scenario(|| redriven_past_a_gate_posts_once(false));
}

/// **A run resumed past a gate re-drives to the same event ids, from a manual trigger.**
#[test]
fn severe_a_manual_redrive_past_a_gate_mints_the_same_ids() {
    run_workflow_scenario(|| redriven_past_a_gate_posts_once(true));
}

#[test]
fn severe_workflow_approval_enforces_run_channel_membership_writability_and_h_tag() {
    run_workflow_scenario(|| async {
        let env = gate_env().await;
        let relay = seed_relay_identity(&env).await;
        let ana = Actor::new("wf-sec-owner");
        let bob = Actor::new("wf-sec-approver");
        let mod_ = Actor::new("wf-sec-outside-mod");
        let priv_chan = "wf-private-room";
        let open_chan = "wf-open-decoy";

        seed_channel_with_owner(&env, priv_chan, Visibility::Private, &ana).await;
        common::seed_membership(&env, priv_chan, &bob, buzz::relay::channel::Role::Member).await;
        seed_channel_with_owner(&env, open_chan, Visibility::Open, &ana).await;
        {
            let mut config = env.store("config");
            store::put_raw(
                &mut config,
                b"buzz-community-moderators",
                ferrite_app::serde_json::to_vec(&[mod_.pk_hex()]).unwrap().as_slice(),
            )
            .await
            .expect("seed mod");
        }

        // 1. Named approver Bob is kicked from priv_chan after seeing the token:
        // neither h-less 46011 nor decoy-h 46011 may resume the workflow.
        assert_eq!(
            define(
                &env,
                &ana,
                priv_chan,
                "priv-wf-bob",
                json!({
                    "name": "private deploy",
                    "on": { "contains": "deploy-bob" },
                    "steps": [
                        { "type": "approval", "id": "gate", "approver": bob.pk_hex() },
                        { "type": "post", "id": "ship", "text": "bob shipped secret" },
                    ],
                }),
            )
            .await
            .status,
            200
        );
        let trig1 = signed_at(
            &ana,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", priv_chan)],
            "please deploy-bob",
            T0_SECS,
        );
        assert_eq!(submit(&env, &ana, &trig1).await.status, 200);
        let tok1 = gate_token(&env, &bob, priv_chan).await;

        let kick_bob = signed_at(
            &ana,
            kind::KIND_NIP29_REMOVE_USER,
            vec![tag("h", priv_chan), tag("p", &bob.pk_hex())],
            "",
            T0_SECS + 1,
        );
        assert_eq!(submit(&env, &ana, &kick_bob).await.status, 200);

        let h_less = signed_at(
            &bob,
            kind::KIND_WORKFLOW_APPROVAL_GRANTED,
            vec![tag("token", &tok1)],
            "",
            T0_SECS + 2,
        );
        assert_eq!(
            submit(&env, &bob, &h_less).await.status,
            403,
            "expelled private-channel approver must be refused"
        );
        let decoy_h = signed_at(
            &bob,
            kind::KIND_WORKFLOW_APPROVAL_GRANTED,
            vec![tag("token", &tok1), tag("h", open_chan)],
            "",
            T0_SECS + 3,
        );
        let decoy_resp = submit(&env, &bob, &decoy_h).await;
        assert!(
            decoy_resp.status == 400 || decoy_resp.status == 403,
            "mismatched h tag on approval must be refused: {}",
            error_text(&decoy_resp)
        );
        assert!(
            relay_posts(&env, &ana, &relay, priv_chan).await.is_empty(),
            "expelled approver must not cause relay posts in private channel"
        );

        // 2. Outside community moderator who is NOT a member of priv_chan cannot approve
        // a `moderator` gate inside priv_chan, and archiving priv_chan blocks live approvers too.
        assert_eq!(
            define_at(
                &env,
                &ana,
                priv_chan,
                "priv-wf-mod",
                json!({
                    "name": "mod deploy",
                    "on": { "contains": "deploy-mod" },
                    "steps": [
                        { "type": "approval", "id": "gate", "approver": "moderator" },
                        { "type": "post", "id": "ship", "text": "mod shipped secret" },
                    ],
                }),
                T0_SECS + 4,
            )
            .await
            .status,
            200
        );
        let trig2 = signed_at(
            &ana,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", priv_chan)],
            "please deploy-mod",
            T0_SECS + 5,
        );
        assert_eq!(submit(&env, &ana, &trig2).await.status, 200);
        let tok2 = {
            let page = query(&env, &ana, &json!({ "channel": priv_chan, "limit": 100 })).await;
            body_json(&page)["events"]
                .as_array()
                .unwrap()
                .iter()
                .find_map(|e| {
                    if e["kind"].as_u64() != Some(u64::from(kind::KIND_WORKFLOW_APPROVAL_REQUESTED)) {
                        return None;
                    }
                    let wf = e["tags"].as_array()?.iter().find_map(|t| {
                        let t = t.as_array()?;
                        (t.first()?.as_str()? == "workflow").then(|| t.get(1)?.as_str())?
                    })?;
                    if wf != "priv-wf-mod" {
                        return None;
                    }
                    e["tags"].as_array()?.iter().find_map(|t| {
                        let t = t.as_array()?;
                        (t.first()?.as_str()? == "token").then(|| t.get(1)?.as_str())?
                    })
                })
                .unwrap()
                .to_string()
        };

        let outside_mod_grant = signed_at(
            &mod_,
            kind::KIND_WORKFLOW_APPROVAL_GRANTED,
            vec![tag("token", &tok2)],
            "",
            T0_SECS + 6,
        );
        assert_eq!(
            submit(&env, &mod_, &outside_mod_grant).await.status,
            403,
            "community moderator outside private room must not approve its workflow"
        );

        // Archive priv_chan, then admit mod_ to membership row directly and verify archived channel still blocks!
        common::seed_membership(&env, priv_chan, &mod_, buzz::relay::channel::Role::Member).await;
        let archive = signed_at(
            &ana,
            kind::KIND_NIP29_EDIT_METADATA,
            vec![tag("h", priv_chan), tag("archived", "true")],
            "",
            T0_SECS + 7,
        );
        assert_eq!(submit(&env, &ana, &archive).await.status, 200);

        let archived_grant = signed_at(
            &mod_,
            kind::KIND_WORKFLOW_APPROVAL_GRANTED,
            vec![tag("token", &tok2)],
            "",
            T0_SECS + 8,
        );
        assert_eq!(
            submit(&env, &mod_, &archived_grant).await.status,
            403,
            "approval on an archived channel must be refused"
        );
        assert!(relay_posts(&env, &ana, &relay, priv_chan).await.is_empty());
    });
}


/// **Editing a workflow while a run waits at its gate does not strand the run.**
/// The newer 30620 replaces, and deletes, the event the parked run pinned; the
/// approval then found no definition and silently did nothing, leaving the run
/// awaiting approval forever (finding_6514a102b73fdda9).
#[test]
fn severe_an_edit_while_a_run_is_parked_does_not_strand_it() {
    run_workflow_scenario(|| async {
        let env = gate_env().await;
        let relay = seed_relay_identity(&env).await;
        let ana = Actor::new("workflow-ana-edit");
        seed_channel_with_owner(&env, "edited", Visibility::Open, &ana).await;
        let steps = |after: &str| {
            json!({
                "name": "gated deploy",
                "on": { "contains": "cut a build" },
                "steps": [
                    { "type": "post", "id": "before", "text": "preparing the release" },
                    { "type": "approval", "id": "gate", "approver": ana.pk_hex() },
                    { "type": "post", "id": "after", "text": after },
                ],
            })
        };
        let defined = define_at(&env, &ana, "edited", "edit-me", steps("the release is out"), T0_SECS - 10).await;
        assert_eq!(defined.status, 200, "{}", error_text(&defined));
        let trigger = signed_at(&ana, kind::KIND_STREAM_MESSAGE, vec![tag("h", "edited")], "time to cut a build", T0_SECS - 5);
        assert_eq!(submit(&env, &ana, &trigger).await.status, 200);

        let token = {
            let page = query(&env, &ana, &json!({ "channel": "edited", "limit": 100 })).await;
            body_json(&page)["events"]
                .as_array()
                .and_then(|events| {
                    events.iter().find_map(|e| {
                        (e["kind"].as_u64() == Some(u64::from(kind::KIND_WORKFLOW_APPROVAL_REQUESTED)))
                            .then(|| {
                                e["tags"].as_array()?.iter().find_map(|t| {
                                    let t = t.as_array()?;
                                    (t.first()?.as_str()? == "token").then(|| t.get(1)?.as_str().map(str::to_string))?
                                })
                            })
                            .flatten()
                    })
                })
                .expect("the gate published a token")
        };

        // The author edits the workflow while the run waits.
        let edited = define_at(&env, &ana, "edited", "edit-me", steps("a later version"), T0_SECS - 1).await;
        assert_eq!(edited.status, 200, "{}", error_text(&edited));

        let granted = signed(&ana, kind::KIND_WORKFLOW_APPROVAL_GRANTED, vec![tag("token", &token)], "go");
        assert_eq!(submit(&env, &ana, &granted).await.status, 200);

        let said = relay_posts(&env, &ana, &relay, "edited").await;
        assert!(
            said.iter().any(|t| t == "the release is out"),
            "the parked run must resume on the definition it pinned: {said:?}"
        );
        let runs = {
            let mut events = env.store("events");
            workflows::runs_of(&mut events, COMMUNITY, "edit-me", 10).await.expect("runs read")
        };
        assert_eq!(runs[0].status, workflows::RunStatus::Completed);
    });
}
