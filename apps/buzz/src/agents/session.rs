//! The durable session/turn engine over the APE plane: `admit_message`/`advance`
//! hold the exactly-once guard-CAS logic. Event timestamps are `0`: the
//! constrained surface has no wall clock (see `error.rs`), and `ts_ms` is
//! metadata no logic reads.

use std::future::Future;
use std::io;

use ferrite_app::client::ApeClusterClient;

use crate::agents::error::{
    decode_doc, encode_doc, msgmark_key, sess_key, step_key, turn_key, AgentError, EventDoc,
    Result, Scope, SessionDoc, GUARD_RETRIES, MAX_EVENT_BYTES, MAX_STATE_BYTES,
};

/// The minimal APE operations the session engine needs. Abstracting the
/// concrete client behind this trait lets the engine and tool loop be driven by
/// an in-memory implementation in unit tests; the production path is the blanket
/// impl for [`ApeClusterClient`] below (the injected `ctx.ape()` handle).
pub trait ApeOps: Send {
    /// Read `key`.
    fn get(&mut self, key: &[u8]) -> impl Future<Output = io::Result<Option<Vec<u8>>>> + Send;
    /// Batched, point-consistent read for `keys` (positional).
    fn multi_get(
        &mut self,
        keys: &[&[u8]],
    ) -> impl Future<Output = io::Result<Vec<Option<Vec<u8>>>>> + Send;
    /// Atomically apply `entries` iff the value at `guard` equals `expected`
    /// (`None` = guard key absent). `Ok(Ok(()))` on success; `Ok(Err(current))`
    /// when the guard didn't match (carrying the guard's current value).
    fn commit_n_if(
        &mut self,
        guard: &[u8],
        expected: Option<&[u8]>,
        entries: &[(&[u8], &[u8])],
    ) -> impl Future<Output = io::Result<std::result::Result<(), Option<Vec<u8>>>>> + Send;
}

impl ApeOps for ApeClusterClient {
    async fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        ApeClusterClient::get(self, key).await
    }
    async fn multi_get(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Option<Vec<u8>>>> {
        ApeClusterClient::multi_get(self, keys).await
    }
    async fn commit_n_if(
        &mut self,
        guard: &[u8],
        expected: Option<&[u8]>,
        entries: &[(&[u8], &[u8])],
    ) -> io::Result<std::result::Result<(), Option<Vec<u8>>>> {
        ApeClusterClient::commit_n_if(self, guard, expected, entries).await
    }
}

/// The outcome of admitting an inbound message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitOutcome {
    /// The message was newly admitted; the turn began at `seq`.
    Admitted { seq: u64 },
    /// The message id was already admitted; this delivery is inert.
    Duplicate,
}

/// The session engine: one APE client driving guarded, event-sourced
/// transitions. Any number of engines may target the same session; they
/// serialize through guard retries.
pub struct SessionEngine<A = ApeClusterClient> {
    ape: A,
}

impl<A: ApeOps> SessionEngine<A> {
    /// Build an engine over an APE client.
    pub fn new(ape: A) -> Self {
        Self { ape }
    }

    /// Open a session (create-once). Returns true if created, false if it
    /// already existed.
    pub async fn open(&mut self, scope: &Scope) -> Result<bool> {
        let sk = sess_key(scope);
        let init = SessionDoc {
            state: Vec::new(),
            seq: 0,
            turns: 0,
            status: "open".to_string(),
        };
        let enc = encode_doc(&init)?;
        let entries: [(&[u8], &[u8]); 1] = [(sk.as_slice(), enc.as_slice())];
        match self.ape.commit_n_if(&sk, None, &entries).await? {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Read the current session snapshot.
    pub async fn get(&mut self, scope: &Scope) -> Result<Option<SessionDoc>> {
        match self.ape.get(&sess_key(scope)).await? {
            Some(b) => Ok(Some(decode_doc(&b)?)),
            None => Ok(None),
        }
    }

    /// Admit an inbound message EXACTLY ONCE. The per-message marker is read in
    /// the SAME consistent snapshot as the session, so a redelivery can never
    /// double-admit. On first admission, advances `seq`/`turns` and records a
    /// `message_in` event in one envelope.
    pub async fn admit_message(
        &mut self,
        scope: &Scope,
        msg_id: &str,
        payload: &[u8],
    ) -> Result<AdmitOutcome> {
        if payload.len() > MAX_EVENT_BYTES {
            return Err(AgentError::RecordInvalid {
                reason: "payload too large".into(),
            });
        }
        let sk = sess_key(scope);
        let mk = msgmark_key(scope, msg_id);
        for _ in 0..GUARD_RETRIES {
            let snap = self.ape.multi_get(&[sk.as_slice(), mk.as_slice()]).await?;
            if snap.get(1).and_then(|m| m.as_ref()).is_some() {
                return Ok(AdmitOutcome::Duplicate);
            }
            let raw = snap
                .into_iter()
                .next()
                .flatten()
                .ok_or(AgentError::SessionNotOpen)?;
            let cur: SessionDoc = decode_doc(&raw)?;
            let mut next = cur.clone();
            next.seq = cur.seq + 1;
            next.turns = cur.turns + 1;
            next.status = "running".to_string();
            let event = EventDoc {
                seq: next.seq,
                kind: "message_in".to_string(),
                data: payload.to_vec(),
                ts_ms: 0, // no wall clock on the constrained surface (metadata only)
            };
            let next_enc = encode_doc(&next)?;
            let ev_enc = encode_doc(&event)?;
            let tk = turn_key(scope, next.seq);
            let entries: [(&[u8], &[u8]); 3] = [
                (sk.as_slice(), next_enc.as_slice()),
                (mk.as_slice(), b"1"),
                (tk.as_slice(), ev_enc.as_slice()),
            ];
            match self
                .ape
                .commit_n_if(&sk, Some(raw.as_slice()), &entries)
                .await?
            {
                Ok(()) => return Ok(AdmitOutcome::Admitted { seq: next.seq }),
                Err(_) => continue,
            }
        }
        Err(AgentError::Conflict)
    }

    /// Record a turn event (e.g. an LLM result or tool result), advancing the
    /// snapshot `seq` atomically with the event.
    pub async fn record_event(
        &mut self,
        scope: &Scope,
        step_id: &str,
        kind: &str,
        data: &[u8],
    ) -> Result<u64> {
        self.advance(scope, step_id, kind, data, |_s| {}).await
    }

    /// Finalize a turn: record a `message_out` event and set the new state +
    /// idle status, atomically.
    pub async fn finalize_turn(
        &mut self,
        scope: &Scope,
        step_id: &str,
        out_data: &[u8],
        new_state: &[u8],
    ) -> Result<u64> {
        let st = new_state.to_vec();
        self.advance(scope, step_id, "message_out", out_data, move |s| {
            s.state = st.clone();
            s.status = "idle".to_string();
        })
        .await
    }

    /// [`finalize_turn`](Self::finalize_turn) UNLESS `unless_step` is already
    /// recorded, decided atomically with the commit (the closing fence: the
    /// tool loop passes the next batch position, so a turn is never finalized
    /// while another driver has a batch there whose calls have not run). When
    /// `also_mark` is given, that step marker is committed in the SAME
    /// envelope as the final answer, so it exists exactly when this call's
    /// answer is the recorded one (the loop marks a cancellation this way).
    pub async fn finalize_turn_unless(
        &mut self,
        scope: &Scope,
        step_id: &str,
        out_data: &[u8],
        new_state: &[u8],
        unless_step: &str,
        also_mark: Option<&str>,
    ) -> Result<Advanced> {
        let st = new_state.to_vec();
        self.advance_marked(
            scope,
            step_id,
            "message_out",
            out_data,
            move |s| {
                s.state = st.clone();
                s.status = "idle".to_string();
            },
            Some(unless_step),
            also_mark,
        )
        .await
    }

    /// Read events in the seq range `[from_seq, to_seq]` (inclusive), ordered.
    /// Uses APE `multi_get` in 256-key batches (APE has no scan).
    pub async fn read_events(
        &mut self,
        scope: &Scope,
        from_seq: u64,
        to_seq: u64,
    ) -> Result<Vec<EventDoc>> {
        let mut out = Vec::new();
        if to_seq < from_seq {
            return Ok(out);
        }
        let mut seq = from_seq;
        while seq <= to_seq {
            let end = (seq + 255).min(to_seq);
            let keys: Vec<Vec<u8>> = (seq..=end).map(|s| turn_key(scope, s)).collect();
            let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
            let vals = self.ape.multi_get(&refs).await?;
            for v in vals.into_iter().flatten() {
                out.push(decode_doc::<EventDoc>(&v)?);
            }
            seq = end + 1;
        }
        Ok(out)
    }

    /// The transition seq at which `step_id` was applied, if it has been (reads
    /// the per-step marker the guarded transition committed). Higher layers use
    /// this to **self-recognize** an already-applied step before re-doing its
    /// work — making replay inert independent of any caller decision logic.
    pub async fn step_seq(&mut self, scope: &Scope, step_id: &str) -> Result<Option<u64>> {
        match self.ape.get(&step_key(scope, step_id)).await? {
            Some(b) => Ok(Some(read_seq(&b))),
            None => Ok(None),
        }
    }

    /// Read the event recorded at transition `seq` (`None` if absent).
    pub async fn event_at(&mut self, scope: &Scope, seq: u64) -> Result<Option<EventDoc>> {
        match self.ape.get(&turn_key(scope, seq)).await? {
            Some(b) => Ok(Some(decode_doc(&b)?)),
            None => Ok(None),
        }
    }

    /// One guarded transition, **idempotent per `step_id`**: the step marker
    /// (committed in the same envelope) is read in the SAME consistent snapshot
    /// as the session, so a lost-ack retry of the same step self-recognizes the
    /// already-applied commit (returning its seq) instead of committing a
    /// duplicate event. Retries on guard failure (a concurrent runner advanced).
    async fn advance<M: Fn(&mut SessionDoc)>(
        &mut self,
        scope: &Scope,
        step_id: &str,
        kind: &str,
        data: &[u8],
        mutate: M,
    ) -> Result<u64> {
        match self.advance_unless(scope, step_id, kind, data, mutate, None).await? {
            Advanced::Applied(seq) | Advanced::Existed(seq) | Advanced::Blocked(seq) => Ok(seq),
        }
    }

    /// Record a turn event at `step_id` UNLESS `unless_step` is already
    /// recorded, decided atomically: both step markers are read in the session's
    /// snapshot, and the commit is guarded on the session document, which every
    /// transition changes, so a step recorded concurrently forces a re-read.
    /// The tool loop uses it so a batch is never appended after the turn's final
    /// answer was recorded by another driver.
    pub async fn record_event_unless(
        &mut self,
        scope: &Scope,
        step_id: &str,
        kind: &str,
        data: &[u8],
        unless_step: &str,
    ) -> Result<Advanced> {
        self.advance_unless(scope, step_id, kind, data, |_s| {}, Some(unless_step))
            .await
    }

    async fn advance_unless<M: Fn(&mut SessionDoc)>(
        &mut self,
        scope: &Scope,
        step_id: &str,
        kind: &str,
        data: &[u8],
        mutate: M,
        unless_step: Option<&str>,
    ) -> Result<Advanced> {
        self.advance_marked(scope, step_id, kind, data, mutate, unless_step, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn advance_marked<M: Fn(&mut SessionDoc)>(
        &mut self,
        scope: &Scope,
        step_id: &str,
        kind: &str,
        data: &[u8],
        mutate: M,
        unless_step: Option<&str>,
        also_mark: Option<&str>,
    ) -> Result<Advanced> {
        if data.len() > MAX_EVENT_BYTES {
            return Err(AgentError::RecordInvalid {
                reason: "event too large".into(),
            });
        }
        let sk = sess_key(scope);
        let stepk = step_key(scope, step_id);
        let unlessk = unless_step.map(|s| step_key(scope, s));
        let alsok = also_mark.map(|s| step_key(scope, s));
        for _ in 0..GUARD_RETRIES {
            let mut keys: Vec<&[u8]> = vec![sk.as_slice(), stepk.as_slice()];
            if let Some(k) = &unlessk {
                keys.push(k.as_slice());
            }
            let snap = self.ape.multi_get(&keys).await?;
            // Self-recognition: this exact step already applied (lost ack, or
            // another writer). Reported apart from a fresh write, so a caller
            // can tell whether IT created the step.
            if let Some(step_bytes) = snap.get(1).and_then(|m| m.as_ref()) {
                return Ok(Advanced::Existed(read_seq(step_bytes)));
            }
            if let Some(blocking) = snap.get(2).and_then(|m| m.as_ref()) {
                return Ok(Advanced::Blocked(read_seq(blocking)));
            }
            let raw = snap
                .into_iter()
                .next()
                .flatten()
                .ok_or(AgentError::SessionNotOpen)?;
            let cur: SessionDoc = decode_doc(&raw)?;
            let mut next = cur.clone();
            next.seq = cur.seq + 1;
            mutate(&mut next);
            if next.state.len() > MAX_STATE_BYTES {
                return Err(AgentError::RecordInvalid {
                    reason: "state too large".into(),
                });
            }
            let event = EventDoc {
                seq: next.seq,
                kind: kind.to_string(),
                data: data.to_vec(),
                ts_ms: 0, // no wall clock on the constrained surface (metadata only)
            };
            let next_enc = encode_doc(&next)?;
            let ev_enc = encode_doc(&event)?;
            let tk = turn_key(scope, next.seq);
            let step_val = next.seq.to_le_bytes();
            let mut entries: Vec<(&[u8], &[u8])> = vec![
                (sk.as_slice(), next_enc.as_slice()),
                (tk.as_slice(), ev_enc.as_slice()),
                (stepk.as_slice(), step_val.as_slice()),
            ];
            if let Some(k) = &alsok {
                // Never overwrite a marker that already names another step's
                // event: the extra marker is written only when it is new.
                if self.ape.get(k).await?.is_none() {
                    entries.push((k.as_slice(), step_val.as_slice()));
                }
            }
            match self
                .ape
                .commit_n_if(&sk, Some(raw.as_slice()), &entries)
                .await?
            {
                Ok(()) => return Ok(Advanced::Applied(next.seq)),
                Err(_) => continue,
            }
        }
        Err(AgentError::Conflict)
    }
}

/// What a conditional record did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advanced {
    /// This call recorded the step, at this seq.
    Applied(u64),
    /// The step was ALREADY recorded (by an earlier attempt or another
    /// writer), at this seq; nothing was written.
    Existed(u64),
    /// The blocking step was already recorded, at this seq; nothing was written.
    Blocked(u64),
}

/// Decode a seq value stored in a step marker (little-endian, tolerant).
fn read_seq(bytes: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    let n = bytes.len().min(8);
    a[..n].copy_from_slice(&bytes[..n]);
    u64::from_le_bytes(a)
}

/// In-memory [`ApeOps`] for unit tests (ported verbatim from the original
/// crate's `session::testkit`). Single-threaded; `commit_n_if` implements the
/// same guard CAS as the real cluster.
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    pub(crate) struct MemApe {
        map: BTreeMap<Vec<u8>, Vec<u8>>,
    }

    impl ApeOps for MemApe {
        async fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
            Ok(self.map.get(key).cloned())
        }

        async fn multi_get(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Option<Vec<u8>>>> {
            Ok(keys.iter().map(|k| self.map.get(*k).cloned()).collect())
        }

        async fn commit_n_if(
            &mut self,
            guard: &[u8],
            expected: Option<&[u8]>,
            entries: &[(&[u8], &[u8])],
        ) -> io::Result<std::result::Result<(), Option<Vec<u8>>>> {
            let current = self.map.get(guard).cloned();
            let matches = current.as_deref() == expected;
            if !matches {
                return Ok(Err(current));
            }
            for (k, v) in entries {
                self.map.insert(k.to_vec(), v.to_vec());
            }
            Ok(Ok(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::MemApe;
    use super::*;

    fn scope() -> Scope {
        Scope::new("t", "a", "th").unwrap()
    }

    async fn open_engine() -> SessionEngine<MemApe> {
        let mut eng = SessionEngine::new(MemApe::default());
        assert!(eng.open(&scope()).await.unwrap(), "first open creates");
        eng
    }

    #[tokio::test]
    async fn admit_first_then_duplicate() {
        let mut eng = open_engine().await;
        let sc = scope();
        let first = eng.admit_message(&sc, "m1", b"hi").await.unwrap();
        assert_eq!(first, AdmitOutcome::Admitted { seq: 1 });
        let dup = eng.admit_message(&sc, "m1", b"hi").await.unwrap();
        assert_eq!(dup, AdmitOutcome::Duplicate);
        let snap = eng.get(&sc).await.unwrap().unwrap();
        assert_eq!(snap.seq, 1);
        assert_eq!(snap.turns, 1);
    }

    #[tokio::test]
    async fn admit_rejects_oversize() {
        let mut eng = open_engine().await;
        let sc = scope();
        let big = vec![0u8; MAX_EVENT_BYTES + 1];
        let err = eng.admit_message(&sc, "m1", &big).await.unwrap_err();
        assert!(
            matches!(err, AgentError::RecordInvalid { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn record_event_is_idempotent_per_step() {
        let mut eng = open_engine().await;
        let sc = scope();
        eng.admit_message(&sc, "m1", b"hi").await.unwrap();
        let s1 = eng
            .record_event(&sc, "s1", "tool_result", b"r")
            .await
            .unwrap();
        let s2 = eng
            .record_event(&sc, "s1", "tool_result", b"r")
            .await
            .unwrap();
        assert_eq!(s1, s2, "same step id self-recognizes its seq");
        let snap = eng.get(&sc).await.unwrap().unwrap();
        let events = eng.read_events(&sc, 1, snap.seq).await.unwrap();
        let results: Vec<_> = events.iter().filter(|e| e.kind == "tool_result").collect();
        assert_eq!(results.len(), 1, "exactly one tool_result recorded");
    }

    #[tokio::test]
    async fn seq_advances_monotonically() {
        let mut eng = open_engine().await;
        let sc = scope();
        eng.admit_message(&sc, "m1", b"hi").await.unwrap();
        let base = eng.get(&sc).await.unwrap().unwrap().seq;
        let a = eng
            .record_event(&sc, "s1", "tool_result", b"a")
            .await
            .unwrap();
        let b = eng
            .record_event(&sc, "s2", "tool_result", b"b")
            .await
            .unwrap();
        assert_eq!(a, base + 1);
        assert_eq!(b, base + 2);
    }

    #[tokio::test]
    async fn admitted_event_is_message_in() {
        let mut eng = open_engine().await;
        let sc = scope();
        eng.admit_message(&sc, "m1", b"payload").await.unwrap();
        let ev = eng.event_at(&sc, 1).await.unwrap().unwrap();
        assert_eq!(ev.kind, "message_in");
        assert_eq!(ev.data, b"payload");
    }
}
