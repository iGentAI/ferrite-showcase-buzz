//! Server-Sent Events framing, as pure byte-in / frame-out logic.
//!
//! The relay's live stream arrives as SSE, and SSE has exactly one hard part:
//! *a frame is not a chunk*. One TCP read can carry half a `data:` line, three
//! whole events, or a lone `\r`. So the decoder here is a state machine that
//! owns a byte buffer and hands back frames only once their terminating blank
//! line has arrived.
//!
//! It lives outside [`crate::http`] on purpose: the framing is the part most
//! likely to be subtly wrong, and keeping it I/O-free means it is exercised by
//! ordinary unit tests — including under `--no-default-features`, where no HTTP
//! client is compiled at all.
//!
//! What it implements from the SSE specification, and nothing more (the relay is
//! the only producer this has to understand):
//!
//! * `event:` sets the frame name; `data:` lines accumulate, joined with `\n`;
//!   one optional space after the colon is stripped.
//! * A blank line dispatches. `\r\n` and `\n` are both line ends.
//! * Lines starting with `:` are comments — the gateway's keep-alive pings.
//! * `id:` and `retry:` are ignored: the relay carries its resume cursor inside
//!   the frame body (see [`buzz::relay::session::Cursor`]), so there is no
//!   `Last-Event-ID` protocol to honor here.
//!
//! Bounded by construction: the buffer and the accumulated data field are both
//! capped, so a producer that never sends a newline cannot grow this decoder
//! without limit.

use buzz::relay::delivery::Frame;

use crate::error::{Result, SdkError};

/// Largest single SSE frame the decoder will assemble, in bytes.
///
/// An event frame carries one workspace event, whose wire form the relay caps at
/// [`buzz::relay::event::MAX_SERIALIZED_EVENT_BYTES`] (240 KiB); JSON re-escaping
/// inside the frame envelope can inflate that, so this leaves generous headroom
/// and still refuses an unbounded stream.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// One dispatched SSE event: its optional name and its assembled data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, if the producer sent one.
    pub name: Option<String>,
    /// The `data:` field(s), joined with `\n` in arrival order.
    pub data: String,
}

impl SseEvent {
    /// Decode the data field into a relay [`Frame`].
    ///
    /// Returns `Ok(None)` for an event with no data — a comment-only keep-alive
    /// dispatch — rather than an error, because that is a healthy stream doing
    /// nothing.
    ///
    /// The frame's own `type` field is authoritative, not the SSE `event:` name:
    /// the name is a routing convenience for browser `EventSource` clients and
    /// the body is what the relay serialized.
    ///
    /// # Errors
    ///
    /// [`SdkError::Encoding`] if the data is not a frame this protocol defines.
    pub fn to_frame(&self) -> Result<Option<Frame>> {
        let data = self.data.trim();
        if data.is_empty() {
            return Ok(None);
        }
        let frame: Frame = serde_json::from_str(data).map_err(|e| {
            SdkError::encoding(format!(
                "stream frame{}: {e}",
                self.name
                    .as_deref()
                    .map(|n| format!(" '{n}'"))
                    .unwrap_or_default()
            ))
        })?;
        Ok(Some(frame))
    }
}

/// Incremental SSE decoder. Feed it bytes; take frames out.
///
/// Not `Clone`: it is a position in a stream, and two copies of one position is
/// never what a caller means.
#[derive(Debug, Default)]
pub struct SseDecoder {
    pending: Vec<u8>,
    name: Option<String>,
    data: String,
    /// Whether any `data:` line has been seen for the current frame. Distinct
    /// from `data.is_empty()`: `data:` with an empty value is a real field.
    saw_data: bool,
    /// Whether any field at all has been seen for the current frame, which is
    /// what makes a blank line a dispatch rather than a no-op.
    dirty: bool,
}

impl SseDecoder {
    /// A decoder positioned at the start of a stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes; returns every event completed by it.
    ///
    /// Chunk boundaries are irrelevant to the result: splitting the same stream
    /// differently produces the same sequence of events. That property is what
    /// the tests pin.
    ///
    /// Cost: one copy of the incoming bytes into the internal buffer, then one
    /// pass over the buffered lines. No allocation per line beyond the strings
    /// handed back.
    ///
    /// # Errors
    ///
    /// * [`SdkError::Encoding`] if a line is not UTF-8. (A multi-byte character
    ///   cannot straddle a line break, so a *chunk* boundary never causes this —
    ///   only genuinely invalid bytes do.)
    /// * [`SdkError::Encoding`] if the buffered frame exceeds
    ///   [`MAX_FRAME_BYTES`].
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>> {
        if self.pending.len().saturating_add(chunk.len()) > MAX_FRAME_BYTES {
            // Refuse rather than grow: a producer that never terminates a line
            // must not be able to consume this process's memory.
            self.pending.clear();
            return Err(SdkError::encoding(format!(
                "stream frame exceeded {MAX_FRAME_BYTES} bytes without a line break"
            )));
        }
        self.pending.extend_from_slice(chunk);

        let mut out = Vec::new();
        // `split` on the line terminator leaves the trailing (incomplete) piece
        // as the new pending buffer.
        while let Some(index) = self.pending.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=index).collect();
            line.pop(); // the '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8(line)
                .map_err(|_| SdkError::encoding("stream line is not valid UTF-8"))?;
            if let Some(event) = self.line(&line)? {
                out.push(event);
            }
        }
        Ok(out)
    }

    /// Absorb one complete line, returning an event if the line dispatched one.
    fn line(&mut self, line: &str) -> Result<Option<SseEvent>> {
        if line.is_empty() {
            return Ok(self.dispatch());
        }
        if line.starts_with(':') {
            // A comment. The gateway sends these as socket keep-alives; they are
            // not application liveness (that is the relay's `heartbeat` frame).
            return Ok(None);
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            // A line with no colon is a field with an empty value.
            None => (line, ""),
        };
        match field {
            "event" => {
                self.name = Some(value.to_string());
                self.dirty = true;
            }
            "data" => {
                if self.saw_data {
                    self.data.push('\n');
                }
                if self.data.len().saturating_add(value.len()) > MAX_FRAME_BYTES {
                    self.reset();
                    return Err(SdkError::encoding(format!(
                        "stream frame data exceeded {MAX_FRAME_BYTES} bytes"
                    )));
                }
                self.data.push_str(value);
                self.saw_data = true;
                self.dirty = true;
            }
            // `id` and `retry` are accepted and ignored: resume position rides
            // inside the frame body, and reconnect policy is the client's.
            _ => {}
        }
        Ok(None)
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if !self.dirty {
            return None;
        }
        let event = SseEvent {
            name: self.name.take(),
            data: core::mem::take(&mut self.data),
        };
        self.saw_data = false;
        self.dirty = false;
        Some(event)
    }

    /// Forget the partially assembled frame (used when refusing an oversize one).
    fn reset(&mut self) {
        self.pending.clear();
        self.name = None;
        self.data.clear();
        self.saw_data = false;
        self.dirty = false;
    }

    /// Whether any bytes are buffered — i.e. the stream ended mid-frame.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty() && !self.dirty
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use buzz::relay::session::{Cursor, ResetReason};

    fn feed(chunks: &[&str]) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::new();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(decoder.push(chunk.as_bytes()).unwrap());
        }
        out
    }

    #[test]
    fn a_heartbeat_frame_parses() {
        let wire = "event: heartbeat\ndata: {\"type\":\"heartbeat\",\"offset\":\"0:17\"}\n\n";
        let events = feed(&[wire]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name.as_deref(), Some("heartbeat"));
        let frame = events[0].to_frame().unwrap().unwrap();
        assert_eq!(frame, Frame::heartbeat_at(Cursor::new(0, 17)));
    }

    #[test]
    fn a_reset_frame_parses_with_its_reason() {
        let frame = Frame::reset(ResetReason::RetentionClamped, Cursor::new(2, 9));
        let wire = format!(
            "event: reset\ndata: {}\n\n",
            serde_json::to_string(&frame).unwrap()
        );
        let events = feed(&[&wire]);
        assert_eq!(events[0].to_frame().unwrap().unwrap(), frame);
        let Frame::Reset { reason, from } = events[0].to_frame().unwrap().unwrap() else {
            panic!("expected a reset");
        };
        assert_eq!(reason, "retention_clamped");
        assert_eq!(from, "2:9");
    }

    #[test]
    fn a_multi_line_data_field_is_joined_with_newlines() {
        // SSE has no way to send a raw newline inside one `data:` line, so a
        // producer splits it — the pieces must rejoin, not concatenate.
        let events = feed(&["event: note\ndata: line one\ndata: line two\ndata:\ndata: four\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line one\nline two\n\nfour");
    }

    #[test]
    fn a_frame_split_across_chunks_at_every_byte_still_parses() {
        let frame = Frame::event_at(
            Cursor::new(0, 5),
            buzz::relay::event::Event::sign_new(&[7u8; 32], 9, vec![], "hi", 1_000).unwrap(),
        );
        let wire = format!(
            "event: event\ndata: {}\n\n",
            serde_json::to_string(&frame).unwrap()
        );
        let bytes = wire.as_bytes();
        for split in 1..bytes.len() {
            let mut decoder = SseDecoder::new();
            let mut got = Vec::new();
            got.extend(decoder.push(&bytes[..split]).unwrap());
            got.extend(decoder.push(&bytes[split..]).unwrap());
            assert_eq!(got.len(), 1, "split at {split} lost the frame");
            assert_eq!(got[0].to_frame().unwrap().unwrap(), frame);
        }
    }

    #[test]
    fn crlf_and_lf_are_both_line_ends() {
        let events = feed(&[
            "event: heartbeat\r\ndata: {\"type\":\"heartbeat\",\"offset\":\"1:2\"}\r\n\r\n",
        ]);
        assert_eq!(events.len(), 1);
        assert!(events[0].to_frame().unwrap().is_some());
    }

    #[test]
    fn comments_and_unknown_fields_are_ignored() {
        let events = feed(&[
            ": ping\n",
            "id: 12\nretry: 500\n",
            "event: heartbeat\ndata: {\"type\":\"heartbeat\",\"offset\":\"0:1\"}\n\n",
        ]);
        assert_eq!(events.len(), 1, "only the real frame dispatches");
        assert_eq!(events[0].name.as_deref(), Some("heartbeat"));
    }

    #[test]
    fn blank_lines_alone_dispatch_nothing() {
        assert!(feed(&["\n\n\n"]).is_empty());
        let decoder = SseDecoder::new();
        assert!(decoder.is_idle());
    }

    #[test]
    fn several_frames_in_one_chunk_all_arrive_in_order() {
        let wire = "event: heartbeat\ndata: {\"type\":\"heartbeat\",\"offset\":\"0:1\"}\n\n\
                    event: heartbeat\ndata: {\"type\":\"heartbeat\",\"offset\":\"0:2\"}\n\n";
        let events = feed(&[wire]);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1].to_frame().unwrap().unwrap(),
            Frame::heartbeat_at(Cursor::new(0, 2))
        );
    }

    #[test]
    fn an_incomplete_frame_is_withheld_until_its_blank_line() {
        let mut decoder = SseDecoder::new();
        assert!(decoder
            .push(b"event: heartbeat\ndata: {\"type\":\"heartbeat\",\"offset\":\"0:1\"}\n")
            .unwrap()
            .is_empty());
        assert!(!decoder.is_idle(), "a frame is still being assembled");
        assert_eq!(decoder.push(b"\n").unwrap().len(), 1);
        assert!(decoder.is_idle());
    }

    #[test]
    fn empty_data_is_not_an_error_and_bad_json_is() {
        let none = SseEvent {
            name: Some("heartbeat".into()),
            data: String::new(),
        };
        assert!(none.to_frame().unwrap().is_none());

        let bad = SseEvent {
            name: Some("event".into()),
            data: "{not json".into(),
        };
        let err = bad.to_frame().unwrap_err();
        assert!(matches!(err, SdkError::Encoding(_)), "{err}");
    }

    #[test]
    fn an_unterminated_stream_cannot_grow_without_bound() {
        let mut decoder = SseDecoder::new();
        let blob = vec![b'x'; MAX_FRAME_BYTES / 2 + 1];
        assert!(decoder.push(&blob).unwrap().is_empty());
        let err = decoder.push(&blob).unwrap_err();
        assert!(matches!(err, SdkError::Encoding(_)), "{err}");
    }

    #[test]
    fn invalid_utf8_in_a_line_is_refused_without_panicking() {
        let mut decoder = SseDecoder::new();
        let err = decoder.push(b"data: \xff\xfe\n").unwrap_err();
        assert!(matches!(err, SdkError::Encoding(_)), "{err}");
    }
}
