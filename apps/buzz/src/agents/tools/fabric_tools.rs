//! The Fabric A2A client: JSON-RPC over the audited egress handle, the turn it
//! drives and the task tools.

use ferrite_app::serde_json;
use ferrite_app::serde_json::Map;
use ferrite_app::serde_json::Value;
use crate::agents::error::Result;
use super::{BUDGET_EXHAUSTED, CloudToolRegistry, FABRIC_A2A_MSG_SEQ, FABRIC_A2A_POLL_MS, FABRIC_A2A_SEND_RETRY_MS, FABRIC_TASK_RECORD_RETRY_MS, FabricTurnOutcome, clamp_fabric_result_text, fabric_context_id, fabric_final_text, fabric_task_key, fabric_task_state, scoped_fabric_thread_key};

impl<'a> CloudToolRegistry<'a> {
    /// One A2A JSON-RPC exchange with the bound Fabric peer. Success is the
    /// `result` member; a JSON-RPC `error` becomes an `Err` carrying its code
    /// and message so callers can recognize the MEASURED failure modes
    /// (-32603 "Turn already in progress", -32602 missing messageId). Every
    /// call carries the per-app client token — the peer's caller gate refuses
    /// anything else — and rides the audited egress handle like every other
    /// outbound call in this registry.
    pub(super) async fn fabric_rpc(&self, request: &Value) -> std::result::Result<Value, String> {
        let Some(fabric) = &self.fabric_a2a else {
            return Err("the fabric a2a peer is not configured on this deployment".to_string());
        };
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            (
                "authorization".to_string(),
                format!("Bearer {}", fabric.client_token),
            ),
        ];
        let raw = self
            .egress_json(
                &fabric.egress_handle,
                &fabric.endpoint,
                request.to_string().as_bytes(),
                &headers,
            )
            .await?;
        if let Some(error) = raw.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown a2a error");
            return Err(format!("a2a error {code}: {message}"));
        }
        raw.get("result")
            .cloned()
            .ok_or_else(|| "the a2a reply carries neither result nor error".to_string())
    }

    /// A fresh `messageId` for one `message/send` — see [`FABRIC_A2A_MSG_SEQ`].
    /// The time and process-local sequence alone repeat across instances serving
    /// one scope in the same millisecond, so a CSPRNG nonce makes it unique.
    pub(super) fn fabric_message_id(&self) -> std::result::Result<String, String> {
        let seq = FABRIC_A2A_MSG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut hash = 0xcbf29ce484222325u64;
        for b in self.scope.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        // Entropy is a platform host call. Without it the id would repeat across
        // instances, so the send is refused rather than risking a collision.
        let nonce = crate::crypto_kernel::random_u64()
            .map_err(|e| format!("no entropy for a unique a2a message id: {e}"))?;
        Ok(format!("buzz-{hash:016x}-{now:x}-{seq:x}-{nonce:016x}", now = self.ctx.now_ms()))
    }

    /// One A2A `message/send`: start a turn (fresh `contextId` ⇒ new Fabric
    /// thread; a known one ⇒ the SAME durable thread) or continue an earlier
    /// task (`taskId`). The body is byte-for-byte the shape proven live.
    pub(super) async fn fabric_send(
        &self,
        prompt: &str,
        context_id: Option<&str>,
        task_id: Option<&str>,
    ) -> std::result::Result<Value, String> {
        let mut message = Map::new();
        message.insert("role".to_string(), serde_json::json!("user"));
        message.insert("messageId".to_string(), serde_json::json!(self.fabric_message_id()?));
        message.insert(
            "parts".to_string(),
            serde_json::json!([{ "kind": "text", "text": prompt }]),
        );
        if let Some(context_id) = context_id {
            message.insert("contextId".to_string(), serde_json::json!(context_id));
        }
        if let Some(task_id) = task_id {
            message.insert("taskId".to_string(), serde_json::json!(task_id));
        }
        self.fabric_rpc(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": { "message": Value::Object(message) },
        }))
        .await
    }

    /// One `tasks/get` poll. This is not a passive read: the sidecar refreshes
    /// task status ON the read, so an unpolled task stays `working` forever.
    /// `historyLength` bounds the transcript the reply would otherwise carry.
    pub(super) async fn fabric_poll_task(&self, task_id: &str) -> std::result::Result<Value, String> {
        self.fabric_rpc(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tasks/get",
            "params": { "id": task_id, "historyLength": 2 },
        }))
        .await
    }

    /// Drive one COMPLETE Fabric A2A turn for the turn pipeline's harness
    /// path: send, then poll to a terminal state. The rules encoded here are
    /// measured, not guessed:
    /// * poll no faster than [`FABRIC_A2A_POLL_MS`] (the limiter answers a
    ///   breach with HTTP 429, and a poll is what ADVANCES task status);
    /// * a send into a context whose turn still runs fails -32603 — re-tried
    ///   on a pause, because the sidecar's own retry window (~4 s) is far
    ///   shorter than a 30-120 s turn;
    /// * the caller's absolute `deadline_ms` bounds the WHOLE turn, send
    ///   retries included, so a wedged peer ends in a legible error instead
    ///   of a hung dispatch.
    pub(crate) async fn fabric_turn(
        &self,
        prompt: &str,
        context_id: &str,
        deadline_ms: u64,
    ) -> std::result::Result<FabricTurnOutcome, String> {
        let sent = loop {
            // Checked BEFORE every send, the first included: a call that starts
            // after its whole-turn budget (or a retry whose sleep crossed it) must
            // not begin billable remote work.
            if self.ctx.now_ms() >= deadline_ms {
                return Ok(FabricTurnOutcome {
                    task_id: String::new(),
                    state: BUDGET_EXHAUSTED.to_string(),
                    text: None,
                });
            }
            match self.fabric_send(prompt, Some(context_id), None).await {
                Ok(result) => break result,
                Err(e) if e.contains("already in progress") || e.contains("provider error 429") => {
                    if self.ctx.now_ms() >= deadline_ms {
                        // Same wedge as a task stuck `working`, just reported at
                        // send time instead of at the ceiling: Fabric serialises
                        // per thread, so a context whose turn never ends refuses
                        // every later send with -32603. Report it as the
                        // retire-this-context outcome rather than a bare error,
                        // or this channel is silenced for good.
                        return Ok(FabricTurnOutcome {
                            task_id: String::new(),
                            state: BUDGET_EXHAUSTED.to_string(),
                            text: None,
                        });
                    }
                    let remaining = deadline_ms.saturating_sub(self.ctx.now_ms());
                    ferrite_app::sleep_ms(FABRIC_A2A_SEND_RETRY_MS.min(remaining)).await;
                }
                Err(e) => return Err(e),
            }
        };
        let task_id = sent
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if task_id.is_empty() {
            return Err("the a2a peer answered message/send without a task id".to_string());
        }
        let mut latest = sent;
        loop {
            let state = fabric_task_state(&latest);
            if !matches!(state.as_str(), "" | "submitted" | "working") {
                let text = if state == "completed" {
                    fabric_final_text(&latest)
                } else {
                    None
                };
                return Ok(FabricTurnOutcome {
                    task_id,
                    state,
                    text,
                });
            }
            if self.ctx.now_ms() >= deadline_ms {
                // NOT an Err: a task still WORKING at the ceiling means this
                // Fabric CONTEXT is wedged, and Fabric serialises per context,
                // so every later mention in this channel would queue behind it
                // forever. Reporting it as an outcome lets the caller retire the
                // context the same way it retires a FAILED one. Measured: one
                // task stuck `working` silenced #general permanently, while the
                // identical prompt on a fresh context completed in ~50 s.
                return Ok(FabricTurnOutcome {
                    task_id,
                    state: BUDGET_EXHAUSTED.to_string(),
                    text: None,
                });
            }
            // The sleep never runs past the deadline, and a sleep that reaches it
            // ends the loop at the check above instead of starting another read.
            let remaining = deadline_ms.saturating_sub(self.ctx.now_ms());
            ferrite_app::sleep_ms(FABRIC_A2A_POLL_MS.min(remaining)).await;
            if self.ctx.now_ms() >= deadline_ms {
                continue;
            }
            match self.fabric_poll_task(&task_id).await {
                Ok(result) => latest = result,
                // Auth and egress refusals are permanent — more polling
                // cannot fix a rejected credential or a sealed destination.
                Err(e)
                    if e.contains("egress denied")
                        || e.contains("provider error 401")
                        || e.contains("provider error 403") =>
                {
                    return Err(e)
                }
                // Anything else (a 429, a transient 5xx, a network blip) is
                // absorbed: status only moves when a read succeeds, so keep
                // polling until the deadline says otherwise.
                Err(_) => {}
            }
        }
    }

    /// The `fabric_ask`/`fabric_ask_result` tools: the model-facing face of
    /// the same seam the turn pipeline drives. `fabric_ask` returns the task
    /// id immediately (a Fabric turn takes 30-120 s — blocking a tool step on
    /// it would eat the whole turn budget); the task id lands in the durable
    /// event log as this tool's result, which is what makes it re-findable
    /// across a replayed turn. Failures are soft tool results (the web-tool
    /// contract), never plane errors.
    ///
    /// Both tools are scoped to this registry's own conversation (`scope`,
    /// the turn's `{agent}:{thread}`): a `thread_key` names a Fabric thread
    /// inside that conversation, never another channel's, and a task id can
    /// be continued or polled only from the conversation that started it.
    /// The client token is shared by the whole workspace, so without this a
    /// persona in one channel could read another channel's Fabric exchange.
    pub(super) async fn invoke_fabric_a2a(&self, tool: &str, args: &[u8]) -> Result<Vec<u8>> {
        if self.fabric_a2a.is_none() {
            return Self::err("the fabric a2a peer is not configured on this deployment");
        }
        let v: Value = serde_json::from_slice(args).unwrap_or(serde_json::json!({}));
        match tool {
            "fabric_ask" => {
                let prompt = v.get("prompt").and_then(Value::as_str).unwrap_or("").trim();
                if prompt.is_empty() {
                    return Self::err("fabric_ask requires a non-empty 'prompt'");
                }
                let task_id = v
                    .get("task_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                if let Some(task_id) = task_id {
                    if let Err(refusal) = self.fabric_task_owned(task_id).await {
                        return Self::err(refusal);
                    }
                }
                let context_id = v
                    .get("thread_key")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|key| fabric_context_id(&scoped_fabric_thread_key(&self.scope, key), 1));
                match self
                    .fabric_send(prompt, context_id.as_deref(), task_id)
                    .await
                {
                    Ok(result) => {
                        // A started turn without a task id can be neither polled nor
                        // continued: a malformed peer reply, not a success.
                        let Some(id) = result.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) else {
                            return Self::ok(serde_json::json!({
                                "ok": false,
                                "error": "the a2a peer answered message/send without a task id; the turn may have started but cannot be followed",
                                "kind": "fabric-a2a",
                            }));
                        };
                        {
                            // The remote task is already running (and billed): ownership is
                            // retried here rather than the send, and if it still cannot be
                            // recorded the result KEEPS the task id and says plainly that a
                            // second fabric_ask would start a second, separately paid task.
                            let mut recorded = self.record_fabric_task(id).await;
                            for _ in 0..2 {
                                if recorded.is_ok() {
                                    break;
                                }
                                ferrite_app::sleep_ms(FABRIC_TASK_RECORD_RETRY_MS).await;
                                recorded = self.record_fabric_task(id).await;
                            }
                            if let Err(e) = recorded {
                                return Self::ok(serde_json::json!({
                                    "ok": false,
                                    "task_started": true,
                                    "task_id": id,
                                    "error": format!(
                                        "the fabric task {id} STARTED, but this conversation could not record it, \
                                         so fabric_ask_result will refuse to poll it: {e}. Do not call fabric_ask \
                                         again for the same request; that would start and bill a second task."
                                    ),
                                    "kind": "fabric-a2a",
                                }));
                            }
                        }
                        Self::ok(serde_json::json!({
                            "ok": true,
                            "task_id": result.get("id").cloned().unwrap_or(Value::Null),
                            "context_id": result.get("contextId").cloned().unwrap_or(Value::Null),
                            "state": fabric_task_state(&result),
                            "note": "a Fabric turn takes 30-120 seconds; call fabric_ask_result with this task_id until the state is terminal",
                        }))
                    }
                    Err(e) => Self::ok(serde_json::json!({ "error": e, "kind": "fabric-a2a" })),
                }
            }
            "fabric_ask_result" => {
                let task_id = v.get("task_id").and_then(Value::as_str).unwrap_or("");
                if task_id.is_empty() {
                    return Self::err("fabric_ask_result requires 'task_id'");
                }
                if let Err(refusal) = self.fabric_task_owned(task_id).await {
                    return Self::err(refusal);
                }
                match self.fabric_poll_task(task_id).await {
                    Ok(result) => {
                        let state = fabric_task_state(&result);
                        // Clamped char-safe with a marker BEFORE it enters the
                        // JSON envelope: `Self::ok`'s raw byte truncate would
                        // otherwise cut an oversized peer answer mid-UTF-8
                        // into invalid JSON with no marker.
                        let text = if state == "completed" {
                            fabric_final_text(&result).map(|text| clamp_fabric_result_text(&text))
                        } else {
                            None
                        };
                        Self::ok(serde_json::json!({
                            "ok": true,
                            "task_id": task_id,
                            "state": state,
                            "text": text,
                        }))
                    }
                    Err(e) => Self::ok(serde_json::json!({ "error": e, "kind": "fabric-a2a" })),
                }
            }
            other => Self::err(format!("unknown fabric a2a tool: {other}")),
        }
    }

    /// Record that this conversation started Fabric task `task_id`, so it
    /// may later continue or poll it.
    pub(super) async fn record_fabric_task(&self, task_id: &str) -> std::result::Result<(), String> {
        let key = fabric_task_key(&self.scope, task_id)
            .ok_or_else(|| "the peer returned an unusable task id".to_string())?;
        let mut store = self.ctx.store(&self.store_handle).map_err(|e| e.to_string())?;
        store.put(&key, b"1").await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// `Ok` when this conversation started Fabric task `task_id`; otherwise
    /// the refusal the model sees.
    pub(super) async fn fabric_task_owned(&self, task_id: &str) -> std::result::Result<(), String> {
        let unknown = || {
            format!("unknown task_id {task_id:?}: only tasks started by fabric_ask in this conversation can be continued or polled")
        };
        let Some(key) = fabric_task_key(&self.scope, task_id) else {
            return Err(unknown());
        };
        let mut store = self.ctx.store(&self.store_handle).map_err(|e| e.to_string())?;
        match store.get(&key).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(unknown()),
            Err(e) => Err(format!("fabric task lookup failed: {e}")),
        }
    }
}
