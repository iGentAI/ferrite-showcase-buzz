//! Conversation compaction: the trigger, keep/drop algorithm, the 20 000-token
//! retention cap, the two prompt templates, and the event-sourced
//! [`fold_session_events`] (including the post-marker live-tail replay). The
//! trigger estimate counts the caller's fixed per-request overhead (system
//! prompt + tool schemas); see [`fixed_overhead_tokens`].
//!
//! Everything here is pure; the single model call that produces the summary is
//! performed by the integration layer behind the [`Summarizer`] seam.

use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json;

use crate::agents::conv::{
    decode_tool_result, ConvMessage, ConvRole, ToolCallBatch, ToolCallRecord, ToolUse,
    TOOL_CALL_EVENT_KIND, TOOL_RESULT_EVENT_KIND,
};
use crate::agents::error::EventDoc;

/// The verbatim local summarization prompt (appended to drive the summary turn).
pub const SUMMARIZATION_PROMPT: &str = include_str!("templates/compact/prompt.md");
/// The verbatim prefix prepended to a produced summary (and used to detect a
/// summary message already present in history).
pub const SUMMARY_PREFIX: &str = include_str!("templates/compact/summary_prefix.md");

/// Maximum tokens of real user messages retained verbatim through a compaction.
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;

/// Approximate bytes-per-token used by the heuristic estimator.
const BYTES_PER_TOKEN: usize = 4;

/// Estimate tokens for a text via the byte heuristic, `ceil(bytes / 4)`.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(BYTES_PER_TOKEN)
}

/// The auto-compaction token limit: `min(config_override, context_window*19/20)`.
/// A config override can only LOWER the limit. A zero/unknown context window
/// disables compaction (returns [`i64::MAX`]).
///
/// The default headroom is 5%, down from the original 10%: the estimate now
/// counts the fixed per-request overhead (system prompt + tool schemas) that
/// the wider margin used to absorb blindly, so the remaining headroom only
/// has to cover the byte heuristic's own error.
pub fn auto_compact_limit(context_window: i64, config_override: Option<i64>) -> i64 {
    if context_window <= 0 {
        return i64::MAX;
    }
    let ceiling = context_window / 20 * 19;
    match config_override {
        Some(c) if c < ceiling => c,
        _ => ceiling,
    }
}

/// Whether to compact: the running token total has reached the limit. Checked
/// PRE-turn (so compaction fires on the *next* turn after the limit is crossed).
pub fn should_compact(running_total_tokens: i64, limit: i64) -> bool {
    running_total_tokens >= limit
}

/// Wrap a raw summary with the verbatim prefix.
pub fn wrap_summary(summary: &str) -> String {
    format!("{SUMMARY_PREFIX}\n{summary}")
}

/// Whether a message text is a compaction summary (begins with the prefix).
pub fn is_summary_message(text: &str) -> bool {
    text.starts_with(&format!("{SUMMARY_PREFIX}\n"))
}

/// The event kind under which a [`CompactionMarker`] is recorded.
pub const COMPACTION_EVENT_KIND: &str = "compaction";

/// A compaction marker appended to the durable event log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct CompactionMarker {
    /// The wrapped summary (already prefixed via [`wrap_summary`]).
    pub summary: String,
    /// Seqs of the real user events retained verbatim (oldest-first).
    pub retained_user_event_seqs: Vec<u64>,
    /// Monotone generation; the LATEST marker wins on fold.
    pub window_generation: u64,
    /// The last seq the summary covers: the snapshot it was made from. The
    /// summarizer call is slow and other turns on the thread keep recording
    /// while it runs, so the marker lands later than this; the fold replays
    /// everything after it, not after the marker. Absent on older markers,
    /// which covered everything before themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_through_seq: Option<u64>,
}

impl CompactionMarker {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("CompactionMarker serializes")
    }
    pub fn from_bytes(b: &[u8]) -> Option<CompactionMarker> {
        serde_json::from_slice(b).ok()
    }
}

/// A durable user event candidate for the fold (seq + its text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEvent {
    pub seq: u64,
    pub text: String,
}

/// Fold the conversation for the next turn. With no marker, the naive view is
/// `reinjected_context ++ all user events`. With a marker, the view is
/// `reinjected_context ++ retained user events (by seq) ++ summary (last, user)`.
pub fn history_fold(
    reinjected_context: &[ConvMessage],
    user_events: &[UserEvent],
    marker: Option<&CompactionMarker>,
) -> Vec<ConvMessage> {
    let mut out: Vec<ConvMessage> = reinjected_context.to_vec();
    match marker {
        None => {
            for ue in user_events {
                out.push(ConvMessage::user(ue.text.clone()));
            }
        }
        Some(m) => {
            for &seq in &m.retained_user_event_seqs {
                if let Some(ue) = user_events.iter().find(|u| u.seq == seq) {
                    out.push(ConvMessage::user(ue.text.clone()));
                }
            }
            out.push(ConvMessage::user(m.summary.clone()));
        }
    }
    out
}

/// Fold a durable session event log into the model view for the next turn
/// (resume/fork stable). Without a marker the LIVE conversation is replayed in
/// order. With a marker, the compacted view is `reinjected_context → retained
/// real-user messages (by seq) → summary (last)`, then the LIVE tail after the
/// marker replayed structurally.
pub fn fold_session_events(
    events: &[EventDoc],
    reinjected_context: &[ConvMessage],
) -> Vec<ConvMessage> {
    let mut marker: Option<(u64, CompactionMarker)> = None;
    for ev in events {
        if ev.kind == COMPACTION_EVENT_KIND {
            if let Some(m) = CompactionMarker::from_bytes(&ev.data) {
                let newer = marker
                    .as_ref()
                    .map(|(_, cur)| m.window_generation >= cur.window_generation)
                    .unwrap_or(true);
                if newer {
                    marker = Some((ev.seq, m));
                }
            }
        }
    }

    let mut out: Vec<ConvMessage> = reinjected_context.to_vec();
    match marker {
        None => {
            replay_live(events.iter(), &mut out);
        }
        Some((marker_seq, m)) => {
            use std::collections::HashMap;
            let by_seq: HashMap<u64, &EventDoc> = events
                .iter()
                .filter(|e| e.kind == "message_in")
                .map(|e| (e.seq, e))
                .collect();
            for &seq in &m.retained_user_event_seqs {
                if let Some(ev) = by_seq.get(&seq) {
                    out.push(ConvMessage::user(
                        String::from_utf8_lossy(&ev.data).into_owned(),
                    ));
                }
            }
            out.push(ConvMessage::user(m.summary.clone()));
            let through = m.compacted_through_seq.unwrap_or(marker_seq).min(marker_seq);
            replay_live(
                events
                    .iter()
                    .filter(|e| e.seq > through && e.kind != COMPACTION_EVENT_KIND),
                &mut out,
            );
        }
    }
    out
}

/// The result the fold shows for a recorded tool call that has no recorded
/// result (a turn that failed between recording the batch and the result):
/// a provider refuses an assistant tool-use left unanswered.
pub const MISSING_TOOL_RESULT: &str =
    r#"{"error":"no result was recorded for this tool call"}"#;

/// Replay a stream of durable events into conversation messages, pairing each
/// `tool_call` request with its results.
///
/// The folded exchange is always structurally valid: every call on an
/// assistant tool-use message is followed by exactly one result for it.
/// Results the loop records carry a [`crate::agents::conv::ToolResultTag`] naming their batch and
/// position, so they are paired by identity and emitted directly after their
/// batch wherever they sit in the log. Untagged results from before tags
/// existed are paired by position within their own batch only. A call with no
/// result gets [`MISSING_TOOL_RESULT`]; a result that answers no known call is
/// dropped rather than attached to the wrong one.
fn replay_live<'a>(events: impl Iterator<Item = &'a EventDoc>, out: &mut Vec<ConvMessage>) {
    use std::collections::{HashMap, HashSet, VecDeque};
    let events: Vec<&EventDoc> = events.collect();

    // Pass 1: the tagged batches and results present in this stretch of log.
    let mut tagged_batches: HashSet<String> = HashSet::new();
    let mut tagged_results: HashMap<(String, usize), String> = HashMap::new();
    for ev in &events {
        match ev.kind.as_str() {
            TOOL_CALL_EVENT_KIND => {
                if let Some(batch) = ToolCallBatch::from_bytes(&ev.data) {
                    if !batch.step.is_empty() {
                        tagged_batches.insert(batch.step);
                    }
                }
            }
            TOOL_RESULT_EVENT_KIND => {
                if let (Some(tag), payload) = decode_tool_result(&ev.data) {
                    tagged_results
                        .entry((tag.batch, tag.index))
                        .or_insert_with(|| String::from_utf8_lossy(payload).into_owned());
                }
            }
            _ => {}
        }
    }

    // Pass 2: the conversation. `legacy_pending` holds the unanswered calls
    // of the current untagged batch, in order; they are closed with a
    // synthesized result before anything else is emitted.
    let mut legacy_pending: VecDeque<String> = VecDeque::new();
    let close_legacy = |pending: &mut VecDeque<String>, out: &mut Vec<ConvMessage>| {
        for id in pending.drain(..) {
            out.push(ConvMessage::tool(id, MISSING_TOOL_RESULT));
        }
    };
    for ev in events {
        let text = || String::from_utf8_lossy(&ev.data).into_owned();
        match ev.kind.as_str() {
            "message_in" => {
                close_legacy(&mut legacy_pending, out);
                out.push(ConvMessage::user(text()));
            }
            "message_out" => {
                close_legacy(&mut legacy_pending, out);
                out.push(ConvMessage::assistant(text()));
            }
            TOOL_CALL_EVENT_KIND => {
                close_legacy(&mut legacy_pending, out);
                // Prefer the batch shape; fall back to the single-call shape
                // of older records.
                if let Some(batch) = ToolCallBatch::from_bytes(&ev.data) {
                    let calls = batch.calls;
                    out.push(ConvMessage::assistant_tool_use(batch.text, calls.clone()));
                    if batch.step.is_empty() {
                        legacy_pending.extend(calls.into_iter().map(|c| c.id));
                    } else {
                        for (i, c) in calls.into_iter().enumerate() {
                            let result = tagged_results
                                .get(&(batch.step.clone(), i))
                                .cloned()
                                .unwrap_or_else(|| MISSING_TOOL_RESULT.to_string());
                            out.push(ConvMessage::tool(c.id, result));
                        }
                    }
                } else if let Some(rec) = ToolCallRecord::from_bytes(&ev.data) {
                    legacy_pending.push_back(rec.id.clone());
                    out.push(ConvMessage::assistant_tool_use(
                        "",
                        vec![ToolUse {
                            id: rec.id,
                            name: rec.name,
                            args: rec.args,
                        }],
                    ));
                }
            }
            TOOL_RESULT_EVENT_KIND => {
                let (tag, payload) = decode_tool_result(&ev.data);
                if tag.is_some_and(|t| tagged_batches.contains(&t.batch)) {
                    // Already emitted directly after its batch.
                    continue;
                }
                // Untagged, or tagged for a batch recorded without a step:
                // positional within the current batch, dropped when no call
                // of that batch is still waiting.
                if let Some(id) = legacy_pending.pop_front() {
                    out.push(ConvMessage::tool(
                        id,
                        String::from_utf8_lossy(payload).into_owned(),
                    ));
                }
            }
            _ => {}
        }
    }
    close_legacy(&mut legacy_pending, out);
}

/// Choose which real-user `message_in` event seqs to retain when BUILDING a
/// compaction marker: real users only, newest-first under the budget, returned
/// oldest-first.
pub fn select_retained_user_seqs(
    events: &[EventDoc],
    is_real_user: impl Fn(&str) -> bool,
) -> Vec<u64> {
    let reals: Vec<(u64, String)> = events
        .iter()
        .filter(|e| e.kind == "message_in")
        .map(|e| (e.seq, String::from_utf8_lossy(&e.data).into_owned()))
        .filter(|(_, t)| is_real_user(t))
        .collect();
    let mut kept_rev: Vec<u64> = Vec::new();
    let mut used = 0usize;
    for (seq, text) in reals.iter().rev() {
        let toks = estimate_tokens(text);
        if used + toks > COMPACT_USER_MESSAGE_MAX_TOKENS && used > 0 {
            break;
        }
        kept_rev.push(*seq);
        used += toks;
        if used >= COMPACT_USER_MESSAGE_MAX_TOKENS {
            break;
        }
    }
    kept_rev.reverse();
    kept_rev
}

/// Produces a handoff summary of a rendered transcript — the single model call
/// compaction needs, behind a narrow seam so the policy stays testable.
pub trait Summarizer {
    fn summarize(
        &self,
        transcript: &str,
    ) -> impl std::future::Future<Output = crate::agents::error::Result<String>> + Send;
}

/// Render a folded conversation as plain text for the summarizer.
fn render_transcript(view: &[ConvMessage]) -> String {
    let mut s = String::new();
    for m in view {
        let role = match m.role {
            ConvRole::System => "system",
            ConvRole::User => "user",
            ConvRole::Assistant => "assistant",
            ConvRole::Tool => "tool",
        };
        if !m.text.is_empty() {
            s.push_str(&format!("{role}: {}\n", m.text));
        }
        for tc in &m.tool_calls {
            s.push_str(&format!("{role} called {}({})\n", tc.name, tc.args));
        }
    }
    s
}

/// Token estimate of the fixed per-request overhead the folded view never
/// contains: the rendered system prompt plus the serialized tool schemas.
/// Both ride EVERY model call, so a trigger that ignores them lets the real
/// request overrun the window while the estimate still reads under the limit.
pub fn fixed_overhead_tokens(
    system_prompt: &str,
    tool_schemas: &[crate::agents::tools::ToolSchema],
) -> i64 {
    let t: usize = estimate_tokens(system_prompt)
        + tool_schemas
            .iter()
            .map(|s| {
                estimate_tokens(&s.name)
                    + estimate_tokens(&s.description)
                    + estimate_tokens(&s.input_schema.to_string())
            })
            .sum::<usize>();
    t as i64
}

/// Token estimate of the CURRENT folded view plus the fixed per-request
/// overhead, so once a marker shrinks the view the trigger stops firing.
/// This one function is the estimator for both the trigger and the view —
/// no split-brain.
fn folded_view_tokens(events: &[EventDoc], fixed_overhead: i64) -> i64 {
    let view = fold_session_events(events, &[]);
    let t: usize = view
        .iter()
        .map(|m| {
            estimate_tokens(&m.text)
                + m.tool_calls
                    .iter()
                    .map(|tc| estimate_tokens(&tc.args))
                    .sum::<usize>()
        })
        .sum();
    t as i64 + fixed_overhead
}

fn latest_marker_generation(events: &[EventDoc]) -> u64 {
    events
        .iter()
        .filter(|e| e.kind == COMPACTION_EVENT_KIND)
        .filter_map(|e| CompactionMarker::from_bytes(&e.data))
        .map(|m| m.window_generation)
        .max()
        .unwrap_or(0)
}

/// If the recorded history's folded view (plus the caller's fixed per-request
/// overhead — see [`fixed_overhead_tokens`]) has reached the auto-compaction
/// limit, summarize it and append a [`CompactionMarker`]. Returns whether it
/// compacted. Triggers on the FOLDED size, so after a marker shrinks the view
/// it will not re-fire until the conversation grows again.
pub async fn compact_if_needed<A, S>(
    engine: &mut crate::agents::session::SessionEngine<A>,
    scope: &crate::agents::error::Scope,
    summarizer: &S,
    context_window: i64,
    config_override: Option<i64>,
    fixed_overhead: i64,
) -> crate::agents::error::Result<bool>
where
    A: crate::agents::session::ApeOps,
    S: Summarizer,
{
    let snap = match engine.get(scope).await? {
        Some(s) => s,
        None => return Ok(false),
    };
    let events = engine.read_events(scope, 1, snap.seq).await?;

    let limit = auto_compact_limit(context_window, config_override);
    if !should_compact(folded_view_tokens(&events, fixed_overhead), limit) {
        return Ok(false);
    }

    let transcript = render_transcript(&fold_session_events(&events, &[]));
    let summary = summarizer.summarize(&transcript).await?;
    let retained = select_retained_user_seqs(&events, |t| {
        !crate::agents::fragments::is_injected_fragment(t)
    });
    let generation = latest_marker_generation(&events) + 1;
    let marker = CompactionMarker {
        summary: wrap_summary(&summary),
        retained_user_event_seqs: retained,
        window_generation: generation,
        compacted_through_seq: Some(snap.seq),
    };
    engine
        .record_event(
            scope,
            &format!("compact:{generation}"),
            COMPACTION_EVENT_KIND,
            &marker.to_bytes(),
        )
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: u64, kind: &str, data: &[u8]) -> EventDoc {
        EventDoc {
            seq,
            kind: kind.into(),
            data: data.to_vec(),
            ts_ms: 0,
        }
    }

    struct MockSummarizer;
    impl Summarizer for MockSummarizer {
        async fn summarize(&self, _transcript: &str) -> crate::agents::error::Result<String> {
            Ok("progress: built X; next: continue Y".to_string())
        }
    }

    #[test]
    fn fold_replays_live_tail_after_marker() {
        let marker = CompactionMarker {
            summary: wrap_summary("prior work summarized"),
            retained_user_event_seqs: vec![1],
            window_generation: 1,
            compacted_through_seq: None,
        };
        let call = ToolCallRecord {
            id: "c1".into(),
            name: "read_file".into(),
            args: "{}".into(),
        };
        let events = vec![
            ev(1, "message_in", b"first request"),
            ev(5, COMPACTION_EVENT_KIND, &marker.to_bytes()),
            ev(6, "message_in", b"now do the next thing"),
            ev(7, TOOL_CALL_EVENT_KIND, &call.to_bytes()),
            ev(8, "tool_result", b"file contents here"),
        ];
        let view = fold_session_events(&events, &[ConvMessage::system("BASE")]);
        assert!(
            view.iter().any(|m| is_summary_message(&m.text)),
            "summary kept"
        );
        assert!(
            view.iter().any(|m| m.text == "now do the next thing"),
            "post-marker user replayed"
        );
        assert!(
            view.iter().any(|m| m.role == ConvRole::Assistant && m.tool_calls.iter().any(|t| t.id == "c1")),
            "post-marker tool_use replayed",
        );
        assert!(
            view.iter()
                .any(|m| m.role == ConvRole::Tool && m.tool_call_id.as_deref() == Some("c1")),
            "post-marker tool_result replayed",
        );
    }

    #[test]
    fn fold_replays_a_batch_of_tool_calls_with_correct_pairing() {
        let batch = ToolCallBatch {
            text: "doing two things".into(),
            calls: vec![
                ToolUse {
                    id: "c1".into(),
                    name: "read_file".into(),
                    args: "{}".into(),
                },
                ToolUse {
                    id: "c2".into(),
                    name: "search".into(),
                    args: "{}".into(),
                },
            ],
            step: String::new(),
        };
        let events = vec![
            ev(1, "message_in", b"do the work"),
            ev(2, TOOL_CALL_EVENT_KIND, &batch.to_bytes()),
            ev(3, "tool_result", b"file body"),
            ev(4, "tool_result", b"hits"),
        ];
        let view = fold_session_events(&events, &[ConvMessage::system("BASE")]);
        // One assistant message carries BOTH calls.
        let asst = view
            .iter()
            .find(|m| m.role == ConvRole::Assistant && !m.tool_calls.is_empty())
            .expect("assistant tool-use message present");
        assert_eq!(
            asst.tool_calls.len(),
            2,
            "both calls on one assistant message"
        );
        assert_eq!(asst.text, "doing two things", "commentary text replayed");
        // Results pair FIFO: c1 -> "file body", c2 -> "hits".
        let results: Vec<&ConvMessage> = view.iter().filter(|m| m.role == ConvRole::Tool).collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(results[0].text, "file body");
        assert_eq!(results[1].tool_call_id.as_deref(), Some("c2"));
        assert_eq!(results[1].text, "hits");
    }

    fn call(id: &str) -> ToolUse {
        ToolUse {
            id: id.into(),
            name: "read_file".into(),
            args: "{}".into(),
        }
    }

    fn tagged(batch: &str, index: usize, id: &str, payload: &[u8]) -> Vec<u8> {
        crate::agents::conv::encode_tool_result(
            &crate::agents::conv::ToolResultTag {
                batch: batch.into(),
                index,
                id: id.into(),
            },
            payload,
        )
    }

    /// Every assistant tool-use is followed by exactly one result per call, in
    /// call order, before anything else — the shape providers require.
    fn assert_structurally_valid(view: &[ConvMessage]) {
        let mut i = 0;
        while i < view.len() {
            let m = &view[i];
            if m.role == ConvRole::Assistant && !m.tool_calls.is_empty() {
                for (k, c) in m.tool_calls.iter().enumerate() {
                    let r = view.get(i + 1 + k).expect("a result for every call");
                    assert_eq!(r.role, ConvRole::Tool, "{view:?}");
                    assert_eq!(r.tool_call_id.as_deref(), Some(c.id.as_str()), "{view:?}");
                }
                i += 1 + m.tool_calls.len();
            } else {
                assert_ne!(m.role, ConvRole::Tool, "orphan tool result at {i}: {view:?}");
                i += 1;
            }
        }
    }

    /// Review finding F224: one missing result used to shift every later
    /// result in the thread onto the wrong call, and a surplus one became
    /// `tool_call_id: "unknown"`.
    #[test]
    fn a_missing_legacy_result_does_not_shift_later_results() {
        let first = ToolCallBatch {
            text: String::new(),
            calls: vec![call("a1"), call("a2")],
            step: String::new(),
        };
        let second = ToolCallBatch {
            text: String::new(),
            calls: vec![call("b1")],
            step: String::new(),
        };
        let events = vec![
            ev(1, "message_in", b"go"),
            ev(2, TOOL_CALL_EVENT_KIND, &first.to_bytes()),
            ev(3, "tool_result", b"a1 result"),
            // a2's result was never recorded (the turn failed here).
            ev(4, "message_in", b"again"),
            ev(5, TOOL_CALL_EVENT_KIND, &second.to_bytes()),
            ev(6, "tool_result", b"b1 result"),
            ev(7, "tool_result", b"surplus"),
        ];
        let view = fold_session_events(&events, &[]);
        assert_structurally_valid(&view);
        let result_for = |id: &str| {
            view.iter()
                .find(|m| m.tool_call_id.as_deref() == Some(id))
                .map(|m| m.text.clone())
        };
        assert_eq!(result_for("a1").as_deref(), Some("a1 result"));
        assert_eq!(result_for("a2").as_deref(), Some(MISSING_TOOL_RESULT));
        assert_eq!(result_for("b1").as_deref(), Some("b1 result"));
        assert!(view.iter().all(|m| m.tool_call_id.as_deref() != Some("unknown")));
        assert!(view.iter().all(|m| m.text != "surplus"), "orphan dropped");
    }

    /// Tagged results pair by batch and position even when the log holds a
    /// missing result, a concurrent turn's message between a batch and its
    /// results, and a result for a batch this log does not contain.
    #[test]
    fn tagged_results_pair_by_identity_not_position() {
        let first = ToolCallBatch {
            text: String::new(),
            calls: vec![call("a1"), call("a2")],
            step: "t1:batch:0".into(),
        };
        let second = ToolCallBatch {
            text: String::new(),
            calls: vec![call("a1")], // a reused model id in a later turn
            step: "t2:batch:0".into(),
        };
        let events = vec![
            ev(1, "message_in", b"go"),
            ev(2, TOOL_CALL_EVENT_KIND, &first.to_bytes()),
            ev(3, "message_in", b"a concurrent mention"),
            ev(4, "tool_result", &tagged("t1:batch:0", 0, "a1", b"first a1")),
            // index 1 of t1:batch:0 never recorded
            ev(5, TOOL_CALL_EVENT_KIND, &second.to_bytes()),
            ev(6, "tool_result", &tagged("t2:batch:0", 0, "a1", b"second a1")),
            ev(7, "tool_result", &tagged("gone:batch:3", 0, "zz", b"stray")),
        ];
        let view = fold_session_events(&events, &[]);
        assert_structurally_valid(&view);
        let tools: Vec<&str> = view
            .iter()
            .filter(|m| m.role == ConvRole::Tool)
            .map(|m| m.text.as_str())
            .collect();
        assert_eq!(tools, ["first a1", MISSING_TOOL_RESULT, "second a1"]);
        // The concurrent message stays in the conversation, after the batch.
        assert!(view.iter().any(|m| m.text == "a concurrent mention"));
    }

    #[tokio::test]
    async fn compact_if_needed_triggers_records_marker_and_stops() {
        use crate::agents::session::testkit::MemApe;
        let mut eng = crate::agents::session::SessionEngine::new(MemApe::default());
        let scope = crate::agents::error::Scope::new("t", "a", "th").unwrap();
        eng.open(&scope).await.unwrap();
        let big = "x".repeat(50_000);
        for i in 0..3 {
            eng.admit_message(&scope, &format!("m{i}"), big.as_bytes())
                .await
                .unwrap();
        }

        let did = compact_if_needed(&mut eng, &scope, &MockSummarizer, 30_000, None, 0)
            .await
            .unwrap();
        assert!(did, "over-limit history compacts");

        let snap = eng.get(&scope).await.unwrap().unwrap();
        let events = eng.read_events(&scope, 1, snap.seq).await.unwrap();
        assert!(
            events.iter().any(|e| e.kind == COMPACTION_EVENT_KIND),
            "marker recorded"
        );
        let view = fold_session_events(&events, &[]);
        assert!(
            view.iter().any(|m| is_summary_message(&m.text)),
            "folded view carries the summary"
        );

        let again = compact_if_needed(&mut eng, &scope, &MockSummarizer, 30_000, None, 0)
            .await
            .unwrap();
        assert!(!again, "does not re-compact a small folded view");
    }

    #[tokio::test]
    async fn fixed_overhead_counts_toward_the_trigger() {
        use crate::agents::session::testkit::MemApe;
        let mut eng = crate::agents::session::SessionEngine::new(MemApe::default());
        let scope = crate::agents::error::Scope::new("t", "a", "th").unwrap();
        eng.open(&scope).await.unwrap();
        // ~10k tokens of history against a 30k window (28.5k limit): far
        // under the limit on its own...
        let msg = "x".repeat(40_000);
        eng.admit_message(&scope, "m0", msg.as_bytes())
            .await
            .unwrap();

        let did = compact_if_needed(&mut eng, &scope, &MockSummarizer, 30_000, None, 0)
            .await
            .unwrap();
        assert!(!did, "history alone stays under the limit");

        // ...but the same history plus a 20k-token fixed overhead (system
        // prompt + tool schemas riding every call) is a real overrun.
        let did = compact_if_needed(&mut eng, &scope, &MockSummarizer, 30_000, None, 20_000)
            .await
            .unwrap();
        assert!(did, "overhead-inclusive estimate compacts");
    }

    #[test]
    fn fixed_overhead_tokens_counts_prompt_and_schemas() {
        use crate::agents::tools::ToolSchema;
        assert_eq!(fixed_overhead_tokens("", &[]), 0);
        // 400 bytes of prompt -> 100 tokens.
        let prompt = "p".repeat(400);
        assert_eq!(fixed_overhead_tokens(&prompt, &[]), 100);
        // A schema counts its name, description, and SERIALIZED input schema
        // — the shape the provider actually receives.
        let schema = ToolSchema {
            name: "read_file".into(),
            description: "d".repeat(40),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let with_schema = fixed_overhead_tokens(&prompt, std::slice::from_ref(&schema));
        // 100 prompt + 3 name + 10 description + 5 for `{"type":"object"}`.
        assert_eq!(with_schema, 118);
    }

    /// A message recorded by another turn while the summarizer ran sits
    /// between the summarized snapshot and the marker. It is neither in the
    /// summary nor after the marker, and was silently dropped from every later
    /// fold (finding_5a6f6a422a134834).
    #[test]
    fn fold_keeps_events_recorded_while_the_summary_was_written() {
        let marker = CompactionMarker {
            summary: wrap_summary("S"),
            retained_user_event_seqs: vec![1],
            window_generation: 1,
            compacted_through_seq: Some(2),
        };
        let events = vec![
            ev(1, "message_in", b"u1"),
            ev(2, "message_out", b"a1"),
            ev(3, "message_in", b"arrived during summarize"),
            ev(4, COMPACTION_EVENT_KIND, &marker.to_bytes()),
            ev(5, "message_in", b"after"),
        ];
        let folded = fold_session_events(&events, &[]);
        let texts: Vec<&str> = folded.iter().map(|m| m.text.as_str()).collect();
        let summary = texts.iter().position(|t| t.contains('S')).expect("summary present");
        assert!(
            texts[summary..].contains(&"arrived during summarize"),
            "the mid-compaction message must follow the summary: {texts:?}"
        );
        assert!(texts[summary..].contains(&"after"), "{texts:?}");
        assert!(!texts[summary + 1..].contains(&"a1"), "summarized events are not replayed: {texts:?}");
    }

    #[test]
    fn fold_session_events_excludes_injected_and_uses_latest_marker() {
        let marker = CompactionMarker {
            summary: wrap_summary("S"),
            retained_user_event_seqs: vec![1, 4],
            window_generation: 1,
            compacted_through_seq: None,
        };
        let events = vec![
            ev(1, "message_in", b"u1"),
            ev(
                2,
                "message_in",
                "<goal_context>\nnudge\n</goal_context>".as_bytes(),
            ),
            ev(3, "tool_result", b"junk"),
            ev(4, "message_in", b"u2"),
            ev(5, COMPACTION_EVENT_KIND, &marker.to_bytes()),
        ];
        let ctx = vec![ConvMessage::system("BASE")];
        let folded = fold_session_events(&events, &ctx);
        assert_eq!(folded[0].text, "BASE");
        assert_eq!(folded[1].text, "u1");
        assert_eq!(folded[2].text, "u2");
        assert!(is_summary_message(&folded[3].text));
        assert_eq!(folded.len(), 4);
    }

    #[test]
    fn fold_without_marker_replays_live_conversation_incl_injected() {
        let events = vec![
            ev(1, "message_in", b"hi"),
            ev(2, "message_out", b"answer"),
            ev(
                3,
                "message_in",
                "<goal_context>\nCONTINUE\n</goal_context>".as_bytes(),
            ),
        ];
        let folded = fold_session_events(&events, &[]);
        assert_eq!(folded.len(), 3);
        assert_eq!(folded[0], ConvMessage::user("hi"));
        assert_eq!(folded[1], ConvMessage::assistant("answer"));
        assert!(folded[2].text.contains("CONTINUE"));
    }

    #[test]
    fn select_retained_user_seqs_picks_real_users_only() {
        let events = vec![
            ev(1, "message_in", b"u1"),
            ev(
                2,
                "message_in",
                "<goal_context>\nx\n</goal_context>".as_bytes(),
            ),
            ev(3, "message_out", b"assistant"),
            ev(4, "message_in", b"u2"),
        ];
        let is_real = |t: &str| !t.starts_with("<goal_context>");
        assert_eq!(select_retained_user_seqs(&events, is_real), vec![1, 4]);
    }

    #[test]
    fn auto_compact_limit_ninety_five_percent_and_config_lowers() {
        assert_eq!(auto_compact_limit(200_000, None), 190_000);
        assert_eq!(auto_compact_limit(200_000, Some(50_000)), 50_000);
        assert_eq!(auto_compact_limit(200_000, Some(999_000)), 190_000);
        assert_eq!(auto_compact_limit(0, None), i64::MAX);
    }

    #[test]
    fn trigger_is_inclusive_and_pre_turn() {
        let limit = auto_compact_limit(1000, None);
        assert!(!should_compact(949, limit));
        assert!(should_compact(950, limit));
        assert!(should_compact(951, limit));
    }

    #[test]
    fn summary_wrap_and_detect_roundtrip() {
        let wrapped = wrap_summary("did stuff; next: do more");
        assert!(is_summary_message(&wrapped));
        assert!(wrapped.ends_with("did stuff; next: do more"));
        assert!(!is_summary_message("a plain user message"));
    }

    #[test]
    fn fold_without_marker_is_naive() {
        let ctx = vec![ConvMessage::system("instructions")];
        let users = vec![
            UserEvent {
                seq: 1,
                text: "u1".into(),
            },
            UserEvent {
                seq: 3,
                text: "u2".into(),
            },
        ];
        let out = history_fold(&ctx, &users, None);
        assert_eq!(out[0].role, ConvRole::System);
        assert_eq!(out[1], ConvMessage::user("u1"));
        assert_eq!(out[2], ConvMessage::user("u2"));
    }

    #[test]
    fn fold_with_marker_order_is_context_then_users_then_summary() {
        let ctx = vec![
            ConvMessage::system("instructions"),
            ConvMessage::user("<env>...</env>"),
        ];
        let users = vec![
            UserEvent {
                seq: 1,
                text: "u1".into(),
            },
            UserEvent {
                seq: 3,
                text: "u2".into(),
            },
            UserEvent {
                seq: 5,
                text: "u3".into(),
            },
        ];
        let marker = CompactionMarker {
            summary: wrap_summary("S"),
            retained_user_event_seqs: vec![3, 5],
            window_generation: 1,
            compacted_through_seq: None,
        };
        let out = history_fold(&ctx, &users, Some(&marker));
        assert_eq!(out.len(), 5);
        assert_eq!(out[0].text, "instructions");
        assert_eq!(out[2], ConvMessage::user("u2"));
        assert_eq!(out[3], ConvMessage::user("u3"));
        assert_eq!(out[4].role, ConvRole::User);
        assert!(is_summary_message(&out[4].text));
        assert!(!out.iter().any(|m| m.text == "u1"));
    }

    #[test]
    fn fold_is_deterministic_over_event_order() {
        let ctx = vec![ConvMessage::system("i")];
        let users_a = vec![
            UserEvent {
                seq: 5,
                text: "u3".into(),
            },
            UserEvent {
                seq: 1,
                text: "u1".into(),
            },
            UserEvent {
                seq: 3,
                text: "u2".into(),
            },
        ];
        let marker = CompactionMarker {
            summary: wrap_summary("S"),
            retained_user_event_seqs: vec![1, 3, 5],
            window_generation: 2,
            compacted_through_seq: None,
        };
        let out1 = history_fold(&ctx, &users_a, Some(&marker));
        let mut users_b = users_a.clone();
        users_b.reverse();
        let out2 = history_fold(&ctx, &users_b, Some(&marker));
        assert_eq!(out1, out2);
        assert_eq!(out1[1].text, "u1");
        assert_eq!(out1[2].text, "u2");
        assert_eq!(out1[3].text, "u3");
    }
}
