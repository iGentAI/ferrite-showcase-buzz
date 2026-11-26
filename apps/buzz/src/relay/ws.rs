//! The NIP-01 + NIP-42 WebSocket protocol, as a pure layer.
//!
//! buzz's desktop client speaks nostr over one socket: `["REQ", subId, filter]`,
//! `["CLOSE", subId]`, `["EVENT", event]`, `["AUTH", signedEvent]` up; `EVENT`,
//! `EOSE`, `OK`, `CLOSED`, `NOTICE`, `AUTH` down. That wire form is fixed — the
//! client is already written against it — so this module is a transliteration of
//! it, not a design.
//!
//! Everything here is **pure**: parsing, rendering, the per-connection
//! subscription table, the NIP-42 rules, the read-plan choice, and the bounds.
//! [`crate::relay::ingress`] owns the queue loop, the store, and the feed; keeping the
//! protocol away from I/O is what makes the abuse cases (a truncated frame, a
//! forged AUTH, a subscription flood) testable without a fleet.
//!
//! Four properties are load-bearing:
//!
//! * **Parsing is total.** Every byte string is either a [`ClientMessage`] or a
//!   [`BuzzError`] whose `Display` is a `NOTICE`-able reason. There is no panic
//!   path (the crate denies indexing and `unwrap`) and no silent drop: a frame
//!   the relay cannot understand is answered, because a client that is told
//!   nothing retries forever.
//! * **The connection is bounded.** Subscriptions per connection, filters per
//!   REQ, values per filter, and the stored-event `limit` all have ceilings, and
//!   they are the SAME numbers [`crate::relay::ingress::relay_info`] advertises — a
//!   client that reads NIP-11 and obeys it is never refused.
//! * **Authentication binds a pubkey, nothing else.** Until a NIP-42 exchange
//!   completes, [`Connection::reader`] is [`ANON_READER`] — a pubkey no signed
//!   event can ever have — so `access::may_deliver` and
//!   `access::filter_authorized` decide what an unauthenticated socket may see
//!   with no separate "anonymous" policy to keep in sync.
//! * **The challenge is derived, not random.** The platform injects no RNG, so a
//!   connection's challenge is an HMAC of its gateway correlation id under the
//!   same sealed key that signs stream tokens — unguessable without being random,
//!   exactly as [`crate::relay::session::Session::open`] derives session ids.

use ferrite_app::serde::de::{self, IgnoredAny, SeqAccess, Visitor};
use ferrite_app::serde::{Deserialize, Deserializer, Serialize};
use ferrite_app::serde_json::{self, Value};

use crate::relay::crypto::{ct_eq, from_hex32, hmac::hmac_sha256_concat, to_hex};
use crate::relay::event::{Event, EventId, PubKey};
use crate::relay::filter::Filter;
use crate::relay::kind::{self, Kind};
use crate::relay::{BuzzError, Result};

/// How many live subscriptions one connection may hold.
///
/// buzz's client opens one per view — the visible channel's rows, its mentions,
/// typing, presence, user status, huddles, plus short-lived history REQs it
/// closes at EOSE — so a single-digit cap would break the real client. This is
/// the number NIP-11 advertises as `max_subscriptions`.
///
/// **Measured, not guessed.** The real client peaks at 34 concurrent REQs on a
/// workspace with only the two starter channels, and the count grows with the
/// channel list. At 32 the relay answered `CLOSED … too many subscriptions`,
/// which buzz classifies as TERMINAL (`relayClosedPolicy.ts`) — so the
/// subscription was not retried and the feature behind it (reactions, mentions)
/// silently never rendered. The cap exists to bound one connection's fan of
/// index walks, and 256 does that while leaving a real workspace room to grow.
pub const MAX_SUBSCRIPTIONS: usize = 256;

/// Byte ceiling on the SERIALIZED subscription table one connection may hold.
///
/// [`MAX_SUBSCRIPTIONS`] bounds the count, but the table itself is stored
/// inside the connection DOCUMENT in the config store, and the store's record
/// cap (16 MiB) is what that document must fit under. The per-field value
/// caps bound list LENGTHS, not value bytes, so without this budget one 1 MiB
/// REQ per subscription could push the document past the record cap at ~17
/// subscriptions — and a document too large to write is not a refusable REQ,
/// it is a permanently failing store write that redelivery retries forever
/// while the pump re-reads megabytes per poll.
///
/// 256 KiB is ~1000× the real client's ~250 B per subscription, and small
/// enough that the document stays cheap to re-read on every pump iteration.
pub const MAX_SUBSCRIPTION_TABLE_BYTES: usize = 256 * 1024;

/// Should a failed inbound frame be left for REDELIVERY, or acknowledged?
///
/// This is the second half of F2, and the honest form of it. The original
/// worry — an oversized connection document that the store can never write, so
/// the frame that caused it is retried "forever" — is now unreachable through a
/// `REQ`: [`Connection::upsert_subscription`] refuses on the CANDIDATE table's
/// byte budget and the relay answers `CLOSED`, so no unwritable document is ever
/// attempted. The other three writers of that document (a NIP-42 `bind`, a
/// `CLOSE` removing a subscription, an expiry `touch`) can only shrink it or
/// change fixed-width fields, and the one field that is not client data
/// (`reply_to`) is written once at open, on a path that acks its envelope before
/// doing any work.
///
/// Two things remain true regardless, and this predicate is about them:
///
/// * "forever" was never the platform's behaviour — a poison envelope
///   dead-letters after the queue's attempt budget
///   (`ferrite-queue/src/durable_queue.rs`: `max_attempts` total deliveries
///   across crashes). So the worst case is a bounded burn, not an unbounded one.
/// * a burn is still wrong when the failure CANNOT succeed on a retry (an
///   invariant violation, a vanished reply-to binding, a refusal that escaped
///   the in-band path). Those get acknowledged with a `NOTICE` instead, so the
///   client is told rather than left waiting on an answer that will never come.
///
/// The split is [`BuzzError::is_retryable`], which the whole crate already uses
/// for "may the caller usefully retry this identical request": a lost race, a
/// plane failure and a quota are transient; everything else is terminal.
#[must_use]
pub const fn frame_should_redeliver(error: &BuzzError) -> bool {
    error.is_retryable()
}

/// How many filters one REQ may carry. buzz's client sends exactly one; the
/// allowance is for a standard client that batches, and the cap is what stops a
/// REQ from becoming an unbounded fan of index walks.
pub const MAX_FILTERS_PER_REQ: usize = 8;

/// How many values one filter field (`ids`, `authors`, `#e`, …) may list. The
/// client chunks its `#e` backfill at 100 ids, so this is generous; the cap
/// exists because every value can cost an index walk.
pub const MAX_FILTER_VALUES: usize = 512;

/// The largest stored-event page one REQ may return, matching
/// [`crate::relay::query::MAX_PAGE_EVENTS`]: a client may ask for more (buzz's aux
/// backfill asks for 10 000) and is silently served this many, which is the
/// NIP-01 contract for `limit`.
pub const MAX_REQ_LIMIT: usize = crate::relay::query::MAX_PAGE_EVENTS;

/// The `limit` a filter that states none is served. NIP-01 leaves it to the
/// relay; buzz's client always states one, so this only bounds a hand-written
/// filter.
pub const DEFAULT_REQ_LIMIT: usize = 100;

/// Byte ceiling on one REQ's stored answer, mirroring
/// [`crate::relay::query::MAX_PAGE_BYTES`]. Each event is its own WebSocket frame, so
/// this bounds the burst a single REQ can push into the frame queue rather than
/// any one frame.
pub const MAX_REQ_BYTES: usize = crate::relay::query::MAX_PAGE_BYTES;

/// Longest subscription id accepted (NIP-01's own bound). The id is echoed in
/// every frame and stored in the connection document, so it is length-checked
/// and charset-checked on the way in.
pub const MAX_SUB_ID_CHARS: usize = 64;

/// Largest inbound frame accepted, matching the gateway's `MAX_WS_MESSAGE`.
/// Defence in depth: the gateway refuses larger frames before they reach the
/// queue, and this refuses one that arrived some other way.
pub const MAX_CLIENT_MESSAGE_BYTES: usize = 1024 * 1024;

/// Accepted `created_at` skew for a NIP-42 authentication event, in seconds.
/// NIP-42's own recommendation (±10 minutes), and wider than the NIP-98 window
/// ([`crate::relay::auth::MAX_AUTH_SKEW_SECS`]) because the challenge — not the clock —
/// is what makes an AUTH event unreplayable.
pub const AUTH_SKEW_SECS: u64 = 600;

/// The reader identity an unauthenticated connection is gated as.
///
/// All-zero is not a valid BIP-340 x-only pubkey, so no verifiable event can
/// ever be *authored* by it: `may_deliver`'s author checks can never pass for an
/// anonymous socket, and its `#p` checks can only pass for an event whose own
/// author deliberately addressed the zero key. That makes "unauthenticated" a
/// value the existing access gate already handles, rather than a second policy.
pub const ANON_READER: PubKey = [0u8; 32];

/// One inbound frame, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    /// `["REQ", subId, filter, …]` — open (or replace) a subscription.
    Req {
        sub_id: String,
        filters: Vec<Filter>,
    },
    /// `["CLOSE", subId]` — drop a subscription.
    Close { sub_id: String },
    /// `["EVENT", event]` — publish.
    Event(Box<Event>),
    /// `["AUTH", event]` — the NIP-42 response to our challenge.
    Auth(Box<Event>),
    /// A REQ (or COUNT) whose subscription id was readable but whose CONTENT is
    /// refused — carried as a message rather than raised as a parse error so the
    /// answer can NAME the subscription.
    ///
    /// That distinction is the whole point. A parse error becomes a `NOTICE`, which
    /// has no subscription id, so a client is told that something was wrong but not
    /// what: no `EOSE`, no `CLOSED`, and a subscription it waits on forever. This
    /// variant becomes `["CLOSED", subId, reason]` instead.
    RefusedReq { sub_id: String, reason: String },
}

/// The refusal a COUNT is answered with.
///
/// NIP-45 is not implemented, and refusing it is a decision rather than a gap.
/// Counting truthfully means walking every matching row — unbounded work that an
/// anonymous caller could ask for — while counting up to a cap and reporting that
/// as `count` would be a wrong number presented as a right one. A client that is
/// told `CLOSED` with its own subscription id can act immediately; one that gets a
/// nameless NOTICE cannot.
pub const COUNT_UNSUPPORTED: &str = "unsupported: this relay does not implement NIP-45 COUNT";

impl ClientMessage {
    /// Parse one text frame.
    ///
    /// Strict and total: a non-array, an empty array, a non-string verb, an
    /// unknown verb, a missing element, a malformed subscription id, a
    /// non-object filter and an over-large filter are all errors whose
    /// `Display` is the reason a `NOTICE` carries. Nothing here can panic and
    /// nothing is dropped in silence.
    ///
    /// The frame is deserialized STRAIGHT from the bytes, never through
    /// `serde_json::Value`. That is not a performance choice: a `Value` map
    /// silently keeps the last of two identical keys, which would quietly undo
    /// [`Filter`]'s duplicate-field refusal — and a filter with `kinds` twice is
    /// exactly the shape of a gate bypass (one occurrence read by an
    /// authorization pre-check, the other by the matcher).
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_CLIENT_MESSAGE_BYTES {
            return Err(BuzzError::TooLarge {
                got: bytes.len(),
                max: MAX_CLIENT_MESSAGE_BYTES,
            });
        }
        serde_json::from_slice(bytes).map_err(|e| BuzzError::InvalidEvent(e.to_string()))
    }
}

/// Deserializes the NIP-01 array form: `[verb, …]`.
impl<'de> Deserialize<'de> for ClientMessage {
    fn deserialize<D: Deserializer<'de>>(d: D) -> core::result::Result<Self, D::Error> {
        struct MessageVisitor;

        impl<'de> Visitor<'de> for MessageVisitor {
            type Value = ClientMessage;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("a NIP-01 client message array")
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> core::result::Result<ClientMessage, A::Error> {
                let verb: String = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::custom("message must begin with a verb string"))?;
                let message = match verb.as_str() {
                    // COUNT rides the same arm because it has the same shape and
                    // the same failure mode: an answer that cannot name the
                    // subscription leaves the client waiting.
                    "REQ" | "COUNT" => {
                        // Read the id BEFORE validating it: a refusal has to be
                        // able to name the subscription, so the id must survive
                        // its own validation failure.
                        let raw = next_raw_sub_id(&mut seq)?;
                        let sub_id = echoable_sub_id(&raw);
                        let mut refusal = check_sub_id(&raw)
                            .err()
                            .map(|reason| format!("invalid: {reason}"));
                        if refusal.is_none() && verb == "COUNT" {
                            refusal = Some(COUNT_UNSUPPORTED.to_string());
                        }
                        let mut filters: Vec<Filter> = Vec::new();
                        // The sequence is drained either way — a refusal still has
                        // to consume the frame for the parse to finish.
                        while let Some(filter) = seq.next_element::<Filter>()? {
                            if refusal.is_some() {
                                continue;
                            }
                            if filters.len() >= MAX_FILTERS_PER_REQ {
                                refusal = Some(format!(
                                    "invalid: REQ carries over the {MAX_FILTERS_PER_REQ} filter limit"
                                ));
                                continue;
                            }
                            if let Err(reason) = check_filter_bounds(&filter) {
                                refusal = Some(format!("invalid: {reason}"));
                                continue;
                            }
                            filters.push(filter);
                        }
                        if refusal.is_none() && filters.is_empty() {
                            refusal = Some("invalid: REQ carries no filter".into());
                        }
                        match refusal {
                            Some(reason) => ClientMessage::RefusedReq { sub_id, reason },
                            None => ClientMessage::Req { sub_id, filters },
                        }
                    }
                    // A CLOSE keeps validating on the way in: a refused CLOSE
                    // leaves nothing open, so there is no subscription waiting for
                    // an answer that a named refusal would rescue.
                    "CLOSE" => ClientMessage::Close {
                        sub_id: next_sub_id(&mut seq)?,
                    },
                    "EVENT" => ClientMessage::Event(Box::new(next_event(&mut seq, "EVENT")?)),
                    "AUTH" => ClientMessage::Auth(Box::new(next_event(&mut seq, "AUTH")?)),
                    other => {
                        return Err(de::Error::custom(format!(
                            "unsupported message type {other:?}"
                        )))
                    }
                };
                // Drain anything the verb did not claim: an element the relay
                // does not read is not a reason to refuse a frame, and the
                // sequence must be consumed for the parse to finish.
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(message)
            }
        }

        d.deserialize_seq(MessageVisitor)
    }
}

/// Read and validate the next element as a subscription id.
fn next_sub_id<'de, A: SeqAccess<'de>>(seq: &mut A) -> core::result::Result<String, A::Error> {
    let id: String = seq
        .next_element()?
        .ok_or_else(|| de::Error::custom("message lacks a subscription id"))?;
    check_sub_id(&id).map_err(de::Error::custom)?;
    Ok(id)
}

/// Read the next element as a subscription id WITHOUT validating it.
///
/// The id is what makes a refusal answerable — a `CLOSED` has to name the
/// subscription that died — so on the REQ path it is read first and checked
/// afterwards. Bounded on the way OUT by [`echoable_sub_id`] instead.
fn next_raw_sub_id<'de, A: SeqAccess<'de>>(seq: &mut A) -> core::result::Result<String, A::Error> {
    seq.next_element()?
        .ok_or_else(|| de::Error::custom("message lacks a subscription id"))
}

/// The form of a subscription id that is safe to echo back in a `CLOSED`.
///
/// A client may send an arbitrarily long id, and echoing it whole would let one
/// frame choose the size of the answer. Truncated to [`MAX_SUB_ID_CHARS`], which is
/// enough for a client to recognise its own subscription; by CHARACTERS, so a
/// multi-byte id cannot be cut mid-character.
#[must_use]
pub fn echoable_sub_id(id: &str) -> String {
    id.chars().take(MAX_SUB_ID_CHARS).collect()
}

/// Read the next element as a signed event.
fn next_event<'de, A: SeqAccess<'de>>(
    seq: &mut A,
    verb: &str,
) -> core::result::Result<Event, A::Error> {
    seq.next_element::<Event>()?
        .ok_or_else(|| de::Error::custom(format!("{verb} carries no event")))
}

/// A subscription id must be non-empty, short, and printable ASCII.
///
/// It is echoed back in every frame and stored in the connection document, so a
/// control character or an unbounded string is refused rather than carried.
pub fn check_sub_id(id: &str) -> core::result::Result<(), String> {
    if id.is_empty() {
        return Err("subscription id must not be empty".into());
    }
    if id.chars().count() > MAX_SUB_ID_CHARS {
        return Err(format!(
            "subscription id is over {MAX_SUB_ID_CHARS} characters"
        ));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_graphic() || b == b' ' || b == b'-' || b == b'_')
    {
        return Err("subscription id must be printable ASCII".into());
    }
    Ok(())
}

/// Refuse a filter whose value lists are unbounded: every value can cost an
/// index walk, so an unbounded list is an unbounded read.
pub fn check_filter_bounds(filter: &Filter) -> core::result::Result<(), String> {
    let check = |name: &str, len: usize| -> core::result::Result<(), String> {
        if len > MAX_FILTER_VALUES {
            return Err(format!(
                "filter field {name} lists {len} values, over the {MAX_FILTER_VALUES} value limit"
            ));
        }
        Ok(())
    };
    check("ids", filter.ids.as_ref().map_or(0, Vec::len))?;
    check("authors", filter.authors.as_ref().map_or(0, Vec::len))?;
    check("kinds", filter.kinds.as_ref().map_or(0, Vec::len))?;
    for (key, values) in &filter.tags {
        check(key, values.len())?;
    }
    Ok(())
}

/// One outbound frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayMessage {
    /// `["EVENT", subId, event]`
    Event { sub_id: String, event: Box<Event> },
    /// `["EOSE", subId]` — the end of the stored events for that REQ.
    Eose { sub_id: String },
    /// `["OK", eventId, accepted, message]`
    Ok {
        event_id: String,
        accepted: bool,
        message: String,
    },
    /// `["CLOSED", subId, message]` — the subscription will serve nothing.
    Closed { sub_id: String, message: String },
    /// `["NOTICE", message]`. A `rate-limited:` prefix is back-pressure to the
    /// client, which gates its own operations on it.
    Notice { message: String },
    /// `["AUTH", challenge]` — the NIP-42 challenge.
    Auth { challenge: String },
}

impl RelayMessage {
    /// The JSON array this frame is on the wire.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let value = match self {
            Self::Event { sub_id, event } => {
                let event = serde_json::to_value(event.as_ref())
                    .map_err(|e| BuzzError::Internal(format!("event encode: {e}")))?;
                Value::Array(vec![
                    Value::String("EVENT".into()),
                    Value::String(sub_id.clone()),
                    event,
                ])
            }
            Self::Eose { sub_id } => Value::Array(vec![
                Value::String("EOSE".into()),
                Value::String(sub_id.clone()),
            ]),
            Self::Ok {
                event_id,
                accepted,
                message,
            } => Value::Array(vec![
                Value::String("OK".into()),
                Value::String(event_id.clone()),
                Value::Bool(*accepted),
                Value::String(message.clone()),
            ]),
            Self::Closed { sub_id, message } => Value::Array(vec![
                Value::String("CLOSED".into()),
                Value::String(sub_id.clone()),
                Value::String(message.clone()),
            ]),
            Self::Notice { message } => Value::Array(vec![
                Value::String("NOTICE".into()),
                Value::String(message.clone()),
            ]),
            Self::Auth { challenge } => Value::Array(vec![
                Value::String("AUTH".into()),
                Value::String(challenge.clone()),
            ]),
        };
        serde_json::to_vec(&value).map_err(|e| BuzzError::Internal(format!("frame encode: {e}")))
    }

    /// A refusal for one subscription, in the relay's own vocabulary.
    #[must_use]
    pub fn closed(sub_id: &str, message: impl Into<String>) -> Self {
        Self::Closed {
            sub_id: sub_id.to_string(),
            message: message.into(),
        }
    }

    /// `["OK", id, true, ""]`.
    #[must_use]
    pub fn accepted(event_id: &EventId) -> Self {
        Self::Ok {
            event_id: to_hex(event_id),
            accepted: true,
            message: String::new(),
        }
    }

    /// `["OK", id, false, reason]`, where `reason` is a [`BuzzError`]'s rendered
    /// form — already prefixed with the wire token a client branches on.
    #[must_use]
    pub fn refused(event_id: &EventId, reason: &BuzzError) -> Self {
        Self::Ok {
            event_id: to_hex(event_id),
            accepted: false,
            message: reason.to_string(),
        }
    }
}

/// One live subscription: its client-chosen id and the filters it asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Subscription {
    pub id: String,
    pub filters: Vec<Filter>,
}

/// The durable state of one WebSocket connection.
///
/// It lives in the store rather than in the serving instance's memory because
/// the two halves of a connection are not co-located: the gateway publishes each
/// inbound frame onto a shared queue, so a `REQ` may be leased by one app
/// instance while the connection's live pump runs in another. In-memory state
/// would make a subscription invisible to the pump that has to serve it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Connection {
    /// The NIP-42 challenge issued at open. Constant for the connection's life:
    /// its freshness comes from the AUTH event's own `created_at` window.
    pub challenge: String,
    /// The authenticated pubkey, hex. `None` until a NIP-42 exchange succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pubkey: Option<String>,
    /// The live subscription table, in the order the client opened it.
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
    /// Advanced by every open for this correlation id, so a redelivered
    /// `WS_OPEN` supersedes the previous pump instead of running beside it.
    pub generation: u64,
    /// The frame-queue group this connection's frames must be published to (the
    /// open envelope's `reply_to`, never a statically bound queue).
    pub reply_to: String,
    pub opened_at_ms: u64,
    /// When this document self-deletes if the client goes quiet. Refreshed by
    /// every inbound frame; the pump exits when it lapses, so a socket the
    /// gateway never closed cannot leave a pump polling forever.
    pub expires_at_ms: u64,
}

/// How long a connection document lives without an inbound frame.
pub const CONNECTION_TTL_MS: u64 = 60 * 60 * 1000;

impl Connection {
    /// Open state for `correlation_id`, with the challenge derived from it.
    #[must_use]
    pub fn open(secret: &[u8], correlation_id: &str, reply_to: &str, now_ms: u64) -> Self {
        Self {
            challenge: derive_challenge(secret, correlation_id),
            pubkey: None,
            subscriptions: Vec::new(),
            generation: 1,
            reply_to: reply_to.to_string(),
            opened_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(CONNECTION_TTL_MS),
        }
    }

    /// The authenticated pubkey, when NIP-42 has bound one.
    #[must_use]
    pub fn authenticated(&self) -> Option<PubKey> {
        self.pubkey.as_deref().and_then(from_hex32)
    }

    /// The identity every access decision for this connection is made against:
    /// the authenticated pubkey, or [`ANON_READER`] while unauthenticated.
    #[must_use]
    pub fn reader(&self) -> PubKey {
        self.authenticated().unwrap_or(ANON_READER)
    }

    /// Bind the authenticated pubkey (idempotent for the same key).
    pub fn bind(&mut self, pubkey: &PubKey) {
        self.pubkey = Some(to_hex(pubkey));
    }

    /// Add or replace a subscription. An `Err` is the refusal, worded for the
    /// `CLOSED` frame — NIP-01 asks the relay to refuse the subscription
    /// rather than silently serve nothing, and nothing already held is
    /// evicted or altered by a refusal.
    ///
    /// A repeated id REPLACES its filters (NIP-01), which is also what the
    /// client's CLOSED-retry path relies on: it re-sends the same `subId`.
    ///
    /// Two bounds are checked on the CANDIDATE table: the subscription count
    /// ([`MAX_SUBSCRIPTIONS`]) and its serialized size
    /// ([`MAX_SUBSCRIPTION_TABLE_BYTES`]) — the size because this table lives
    /// in the durable connection document, and a table the store cannot write
    /// is worse than a refused REQ (the write fails FOREVER, so the frame
    /// redelivers forever).
    pub fn upsert_subscription(
        &mut self,
        id: String,
        filters: Vec<Filter>,
    ) -> core::result::Result<(), String> {
        let mut next = self.subscriptions.clone();
        if let Some(existing) = next.iter_mut().find(|s| s.id == id) {
            existing.filters = filters;
        } else {
            if next.len() >= MAX_SUBSCRIPTIONS {
                return Err(format!(
                    "error: too many subscriptions (max {MAX_SUBSCRIPTIONS})"
                ));
            }
            next.push(Subscription { id, filters });
        }
        // An unserializable table reads as over budget: the store write it is
        // headed for could only fail harder.
        let bytes = serde_json::to_vec(&next).map_or(usize::MAX, |b| b.len());
        if bytes > MAX_SUBSCRIPTION_TABLE_BYTES {
            return Err(format!(
                "error: this connection's subscription filters exceed the \
                 {MAX_SUBSCRIPTION_TABLE_BYTES}-byte budget; close other \
                 subscriptions or narrow the filters"
            ));
        }
        self.subscriptions = next;
        Ok(())
    }

    /// Drop a subscription; `false` when it was not there.
    pub fn remove_subscription(&mut self, id: &str) -> bool {
        let before = self.subscriptions.len();
        self.subscriptions.retain(|s| s.id != id);
        before != self.subscriptions.len()
    }

    /// The filters of one subscription, if it exists.
    #[must_use]
    pub fn subscription(&self, id: &str) -> Option<&Subscription> {
        self.subscriptions.iter().find(|s| s.id == id)
    }

    /// Refresh the connection's expiry.
    pub fn touch(&mut self, now_ms: u64) {
        self.expires_at_ms = now_ms.saturating_add(CONNECTION_TTL_MS);
    }

    /// Whether the document is still within its expiry.
    #[must_use]
    pub const fn is_live(&self, now_ms: u64) -> bool {
        now_ms < self.expires_at_ms
    }

    /// Whether the expiry is close enough that an inbound frame should refresh
    /// it. Checked so a chatty connection does not pay a store write per frame.
    #[must_use]
    pub const fn needs_touch(&self, now_ms: u64) -> bool {
        self.expires_at_ms.saturating_sub(now_ms) < CONNECTION_TTL_MS / 2
    }
}

/// Derive a connection's NIP-42 challenge.
///
/// There is no RNG on this platform, so the challenge is an HMAC of the
/// gateway's per-connection correlation id under the sealed stream-token key —
/// unguessable to anyone without the key, and stable for the connection so a
/// redelivered open re-issues the same one.
#[must_use]
pub fn derive_challenge(secret: &[u8], correlation_id: &str) -> String {
    to_hex(&hmac_sha256_concat(
        secret,
        &[b"buzz/ws-challenge", correlation_id.as_bytes()],
    ))
}

/// How long a socket ticket admits an open.
///
/// A client asks for a ticket immediately before it dials, so a minute is far
/// more than the round trip needs while keeping a ticket that leaks through a
/// logged URL short-lived.
pub const TICKET_TTL_MS: u64 = 60_000;

/// Mint the ticket that admits one WebSocket open: `{expiry_ms}.{hex mac}`.
///
/// Every HTTP envelope passes the Ferrite SSO gate before the app sees it, but
/// the gateway forwards no headers on a `ws` route (upstream refuses
/// `forward_headers` there), so the session cookie cannot be checked on the
/// socket itself. The ticket carries that check across: only the gated HTTP face
/// mints one (`POST /buzz/api/ws-ticket`), and the ws face refuses an open that
/// does not present a live one. The expiry is inside the MAC, so it cannot be
/// extended by editing the visible half; the label keeps a ticket from ever
/// verifying as a stream token or a challenge minted under the same key.
#[must_use]
pub fn mint_ticket(secret: &[u8], now_ms: u64) -> String {
    let expiry = now_ms.saturating_add(TICKET_TTL_MS).to_string();
    let mac = hmac_sha256_concat(secret, &[b"buzz/ws-ticket", expiry.as_bytes()]);
    format!("{expiry}.{}", to_hex(&mac))
}

/// Whether `presented` is a ticket this deployment minted and has not expired.
///
/// Refuses a malformed shape, an expiry further out than one TTL (no mint ever
/// produces one, so it can only be a forgery or a key reused elsewhere), an
/// expired ticket, and a MAC that does not match (constant-time compare).
#[must_use]
pub fn verify_ticket(secret: &[u8], presented: &str, now_ms: u64) -> bool {
    let Some((expiry_str, mac_hex)) = presented.split_once('.') else {
        return false;
    };
    let Ok(expiry) = expiry_str.parse::<u64>() else {
        return false;
    };
    if now_ms >= expiry || expiry > now_ms.saturating_add(TICKET_TTL_MS) {
        return false;
    }
    let expected = to_hex(&hmac_sha256_concat(
        secret,
        &[b"buzz/ws-ticket", expiry_str.as_bytes()],
    ));
    ct_eq(expected.as_bytes(), mac_hex.as_bytes())
}

/// Verify a NIP-42 authentication event against the challenge we issued.
///
/// All five rules are required, and the order is deliberate: the cheap checks
/// run before the signature so a flood of garbage cannot spend the relay's CPU
/// on secp256k1 verification.
///
/// 1. kind 22242 (an authentication event is never stored, so accepting another
///    kind here would let an ordinary event authenticate a socket);
/// 2. a `challenge` tag equal, byte for byte, to the one this connection was
///    issued — compared in constant time, and the only reason an AUTH event
///    cannot be replayed onto another connection;
/// 3. a `relay` tag whose host is this relay's, when the operator has told us
///    what that is (the gateway forwards no Host header, so an unconfigured
///    deployment can only require the tag's presence — see `relay_host`);
/// 4. `created_at` within [`AUTH_SKEW_SECS`];
/// 5. a valid id and BIP-340 signature.
pub fn verify_auth(
    challenge: &str,
    event: &Event,
    relay_url: Option<&str>,
    now_ms: u64,
) -> Result<PubKey> {
    if event.kind != kind::KIND_AUTH {
        return Err(BuzzError::Unauthorized(format!(
            "an authentication event must be kind {}",
            kind::KIND_AUTH
        )));
    }
    let Some(claimed) = event.first_tag("challenge") else {
        return Err(BuzzError::Unauthorized(
            "the authentication event lacks a challenge tag".into(),
        ));
    };
    if !ct_eq(claimed.as_bytes(), challenge.as_bytes()) {
        return Err(BuzzError::Unauthorized(
            "the authentication event answers a different challenge".into(),
        ));
    }
    let Some(claimed_relay) = event.first_tag("relay") else {
        return Err(BuzzError::Unauthorized(
            "the authentication event lacks a relay tag".into(),
        ));
    };
    if let Some(expected) = relay_url {
        match (relay_host(claimed_relay), relay_host(expected)) {
            (Some(got), Some(want)) if got.eq_ignore_ascii_case(&want) => {}
            _ => {
                return Err(BuzzError::Unauthorized(
                    "the authentication event names another relay".into(),
                ))
            }
        }
    } else if relay_host(claimed_relay).is_none() {
        return Err(BuzzError::Unauthorized(
            "the authentication event's relay tag is not a URL".into(),
        ));
    }
    let skew = (now_ms / 1000).abs_diff(event.created_at);
    if skew > AUTH_SKEW_SECS {
        return Err(BuzzError::Unauthorized(format!(
            "the authentication event is {skew}s from relay time"
        )));
    }
    event
        .verify()
        .map_err(|e| BuzzError::Unauthorized(format!("authentication event: {e}")))?;
    Ok(event.pubkey)
}

/// The host of a relay URL, lowercased, without scheme, userinfo, port or path.
///
/// The port is dropped on purpose: a client that spells the default port
/// explicitly (`wss://buzz.example:443/`) is naming the same relay as one that
/// does not, and the app never learns which port the gateway terminated on.
#[must_use]
pub fn relay_host(url: &str) -> Option<String> {
    let after_scheme = match url.find("://") {
        Some(i) => url.get(i + 3..).unwrap_or(""),
        // A bare host is accepted; a bare path is not a relay URL.
        None if url.starts_with('/') => return None,
        None => url,
    };
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let authority = match authority.rsplit_once('@') {
        Some((_userinfo, host)) => host,
        None => authority,
    };
    // Bracketed IPv6 keeps its brackets; everything else loses a `:port`.
    let host = if authority.starts_with('[') {
        match authority.split_once(']') {
            Some((head, _)) => &authority[..head.len() + 1],
            None => authority,
        }
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Which index a REQ's stored answer should be read from.
///
/// The choice is narrowness first: an id list beats a reference, a reference
/// beats a channel, a channel beats a kind, and a kind beats the whole
/// community. Every plan is re-filtered by [`Filter::matches`] and re-authorized
/// by `access::may_deliver` afterwards, so a wrong-but-wider plan can only cost
/// work — never leak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadPlan {
    /// Whole ids, read as point lookups.
    Ids(Vec<EventId>),
    /// `#e` targets, read from the reference index.
    Etags(Vec<String>),
    /// `#p` values, read from the mention index.
    Mentions(Vec<String>),
    /// One `#h` channel's timeline.
    Channel(String),
    /// Several `#h` channels' timelines, merged newest-first.
    Channels(Vec<String>),
    /// One page per kind, merged newest-first.
    Kinds(Vec<Kind>),
    /// One author's events (optionally narrowed by kind).
    Authors(Vec<PubKey>, Vec<Kind>),
    /// Every event in the community: the kind index (which, unlike the
    /// timeline, holds channel events and channel-less ones alike) walked for
    /// every kind present, merged newest-first.
    Global,
}

/// How many `#h` channels one filter may merge before the community-wide
/// read is the cheaper plan.
pub const MAX_CHANNEL_INDEXES: usize = 16;

/// How many kind indexes a kinds-only filter may walk before it is cheaper to
/// walk the global timeline once. buzz's channel-event kind set is 14 wide.
pub const MAX_KIND_INDEXES: usize = 16;

/// Choose the index for one filter.
#[must_use]
pub fn plan_read(filter: &Filter) -> ReadPlan {
    if let Some(ids) = filter.ids.as_deref() {
        // Only whole ids can be point-read; a NIP-01 id PREFIX has to be found
        // by walking an index, so a prefix list falls through to a wider plan.
        let whole: Vec<EventId> = ids.iter().filter_map(|id| from_hex32(id)).collect();
        if whole.len() == ids.len() && !whole.is_empty() {
            return ReadPlan::Ids(whole);
        }
    }
    if let Some(targets) = filter.tag_filter("#e") {
        if !targets.is_empty() {
            return ReadPlan::Etags(targets.to_vec());
        }
    }
    // One channel is the common case and the narrowest index there is;
    // a few are merged, because the timeline index is per channel.
    if let Some(channels) = filter.tag_filter("#h") {
        if let [only] = channels {
            return ReadPlan::Channel(only.clone());
        }
        if channels.len() > 1 && channels.len() <= MAX_CHANNEL_INDEXES {
            let mut list = channels.to_vec();
            list.sort();
            list.dedup();
            return ReadPlan::Channels(list);
        }
    }
    let kinds = filter.kinds.clone().unwrap_or_default();
    if !kinds.is_empty() && kinds.iter().any(|k| kind::is_author_only(*k)) {
        if let Some(authors) = filter.authors.as_deref() {
            let whole: Vec<PubKey> = authors.iter().filter_map(|a| from_hex32(a)).collect();
            if whole.len() == authors.len() && !whole.is_empty() {
                return ReadPlan::Authors(whole, kinds);
            }
        }
    }
    if let Some(who) = filter.tag_filter("#p") {
        if !who.is_empty() {
            return ReadPlan::Mentions(who.to_vec());
        }
    }
    if let Some(authors) = filter.authors.as_deref() {
        let whole: Vec<PubKey> = authors.iter().filter_map(|a| from_hex32(a)).collect();
        if whole.len() == authors.len() && !whole.is_empty() {
            return ReadPlan::Authors(whole, kinds);
        }
    }
    if !kinds.is_empty() && kinds.len() <= MAX_KIND_INDEXES {
        return ReadPlan::Kinds(kinds);
    }
    ReadPlan::Global
}

/// The stored-event count one REQ may answer with: the filter's own `limit`,
/// clamped to [`MAX_REQ_LIMIT`], defaulting to [`DEFAULT_REQ_LIMIT`].
///
/// `limit: 0` is honored exactly — buzz's client uses it for live-only
/// subscriptions (presence, user status), and serving history there would
/// replay stale presence into a fresh session.
#[must_use]
pub fn effective_limit(filter: &Filter) -> usize {
    match filter.limit {
        Some(n) => usize::try_from(n)
            .unwrap_or(MAX_REQ_LIMIT)
            .min(MAX_REQ_LIMIT),
        None => DEFAULT_REQ_LIMIT,
    }
}

/// A synthetic query cursor that starts an index walk just past `until`.
///
/// Index keys are `{prefix}{inverted_ts}/{id}`, so they sort newest-first; a
/// cursor built from `inverted_ts(until + 1)` with the maximum id sorts strictly
/// before every key at or below `until` and strictly after every newer one.
/// Without this, deep scrollback (`fetchChannelHistoryBefore` sends `until`)
/// would have to walk every newer page first and would silently return nothing
/// on a busy channel.
#[must_use]
pub fn until_cursor(prefix: &[u8], until: u64) -> String {
    let mut key = prefix.to_vec();
    key.extend_from_slice(crate::relay::keys::inverted_ts(until.saturating_add(1)).as_bytes());
    key.push(b'/');
    key.extend_from_slice(&[b'f'; 64]);
    crate::relay::query::encode_cursor(&key)
}

/// Whether an unauthenticated connection may even ask for this filter.
///
/// `may_deliver` would refuse the events anyway, but NIP-42 asks a relay to say
/// so: a client that gets `CLOSED … auth-required:` authenticates and retries,
/// while one that gets an empty EOSE concludes the channel is empty.
#[must_use]
pub fn needs_authentication(filter: &Filter) -> bool {
    filter.kinds.as_deref().is_some_and(|kinds| {
        kinds
            .iter()
            .any(|k| kind::is_p_gated(*k) || kind::is_author_only(*k) || kind::is_result_gated(*k))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::relay::crypto::schnorr;
    use crate::relay::crypto::sha256::sha256;
    use ferrite_app::serde_json;

    const SECRET: &[u8] = b"a-sealed-stream-token-key-32-byte";
    const NOW_MS: u64 = 1_700_000_000_000;
    const NOW_SECS: u64 = NOW_MS / 1000;
    const RELAY: &str = "wss://buzz.test/buzz/ws/";

    fn actor(label: &str) -> ([u8; 32], PubKey) {
        let sk = sha256(label.as_bytes());
        let pk = schnorr::xonly_pubkey(&sk).expect("a valid secret key");
        (sk, pk)
    }

    fn auth_event(challenge: &str, relay: &str, created_at: u64) -> Event {
        let (sk, _) = actor("alice");
        Event::sign_new(
            &sk,
            kind::KIND_AUTH,
            vec![
                vec!["relay".into(), relay.to_string()],
                vec!["challenge".into(), challenge.to_string()],
            ],
            "",
            created_at,
        )
        .expect("signs")
    }

    fn note() -> Event {
        let (sk, _) = actor("bob");
        Event::sign_new(
            &sk,
            kind::KIND_STREAM_MESSAGE,
            vec![vec!["h".into(), "chan-1".into()]],
            "hello",
            NOW_SECS,
        )
        .expect("signs")
    }

    #[test]
    fn a_req_with_one_filter_parses() {
        let raw =
            serde_json::json!(["REQ", "live-1", { "kinds": [9], "#h": ["chan-1"], "limit": 50 }]).to_string();
        let ClientMessage::Req { sub_id, filters } =
            ClientMessage::parse(raw.as_bytes()).expect("parses")
        else {
            panic!("expected a REQ");
        };
        assert_eq!(sub_id, "live-1");
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].kinds.as_deref(), Some([9u32].as_slice()));
        assert_eq!(
            filters[0].tag_filter("#h").map(<[String]>::to_vec),
            Some(vec!["chan-1".to_string()])
        );
        assert_eq!(filters[0].limit, Some(50));
    }

    #[test]
    fn a_req_may_carry_several_filters_up_to_the_cap() {
        let mut parts = vec![serde_json::json!("REQ"), serde_json::json!("s")];
        for _ in 0..MAX_FILTERS_PER_REQ {
            parts.push(serde_json::json!({ "kinds": [9] }));
        }
        let ok = Value::Array(parts.clone()).to_string();
        match ClientMessage::parse(ok.as_bytes()).expect("parses") {
            ClientMessage::Req { filters, .. } => assert_eq!(filters.len(), MAX_FILTERS_PER_REQ),
            other => panic!("expected a REQ, got {other:?}"),
        }
        parts.push(serde_json::json!({ "kinds": [9] }));
        let too_many = Value::Array(parts).to_string();
        assert!(matches!(
            ClientMessage::parse(too_many.as_bytes()),
            Ok(ClientMessage::RefusedReq { .. })
        ));
    }

    #[test]
    fn close_event_and_auth_parse() {
        assert_eq!(
            ClientMessage::parse(serde_json::json!(["CLOSE", "live-1"]).to_string().as_bytes())
                .expect("parses"),
            ClientMessage::Close {
                sub_id: "live-1".into()
            }
        );
        let event = note();
        let published = serde_json::json!(["EVENT", serde_json::to_value(&event).unwrap()]).to_string();
        match ClientMessage::parse(published.as_bytes()).expect("parses") {
            ClientMessage::Event(e) => assert_eq!(*e, event),
            other => panic!("expected an EVENT, got {other:?}"),
        }
        let challenge = derive_challenge(SECRET, "conn-1");
        let auth = auth_event(&challenge, RELAY, NOW_SECS);
        let framed = serde_json::json!(["AUTH", serde_json::to_value(&auth).unwrap()]).to_string();
        match ClientMessage::parse(framed.as_bytes()).expect("parses") {
            ClientMessage::Auth(e) => assert_eq!(e.kind, kind::KIND_AUTH),
            other => panic!("expected an AUTH, got {other:?}"),
        }
    }

    /// Every malformed shape must be an error — never a panic, never a silent
    /// drop. This is the fuzz-shaped case list the ws face is exposed to.
    #[test]
    fn malformed_frames_are_refused_without_panicking() {
        for bad in [
            "",
            " ",
            "null",
            "{}",
            "[]",
            "[1,2]",
            "[\"REQ\"]",
            "[\"REQ\",\"s\",\"not-an-object\"]",
            "[\"REQ\",\"s\",[]]",
            "[\"REQ\",\"s\",{\"kinds\":[9],\"kinds\":[7]}]",
            "[\"CLOSE\"]",
            "[\"CLOSE\",7]",
            "[\"EVENT\"]",
            "[\"EVENT\",\"nope\"]",
            "[\"EVENT\",{}]",
            "[\"AUTH\"]",
            "[\"AUTH\",[]]",
            "[\"req\",\"s\",{}]",
            "{\"REQ\":1}",
            "[\"REQ\",\"s\",{",
            "\u{feff}[\"REQ\",\"s\",{}]",
        ] {
            let outcome = ClientMessage::parse(bad.as_bytes());
            assert!(outcome.is_err(), "accepted {bad:?}: {outcome:?}");
        }
    }

    /// A REQ the relay will not serve must come back as a NAMED refusal, never as
    /// a parse error: a `NOTICE` carries no subscription id, so a client that gets
    /// one waits forever for an `EOSE` that is never coming.
    #[test]
    fn a_refusable_req_is_named_rather_than_turned_into_a_notice() {
        let refusal = |raw: &str| match ClientMessage::parse(raw.as_bytes()) {
            Ok(ClientMessage::RefusedReq { sub_id, reason }) => (sub_id, reason),
            other => panic!("expected a named refusal for {raw}, got {other:?}"),
        };

        // An over-long id is refused, and the answer still names it — truncated to
        // the bound, so one frame cannot choose the size of the answer.
        let long = "x".repeat(MAX_SUB_ID_CHARS + 40);
        let (sub_id, reason) = refusal(&serde_json::json!(["REQ", long, { "kinds": [9] }]).to_string());
        assert_eq!(sub_id.chars().count(), MAX_SUB_ID_CHARS);
        assert!(
            long.starts_with(&sub_id),
            "the echo must be a prefix of what was sent"
        );
        assert!(reason.contains("over"), "{reason}");

        // An empty id: named as the empty string, which is what the client sent.
        let (sub_id, _) = refusal(&serde_json::json!(["REQ", "", { "kinds": [9] }]).to_string());
        assert_eq!(sub_id, "");

        // More filters than the cap.
        let mut parts = vec![serde_json::json!("REQ"), serde_json::json!("over-filtered")];
        for _ in 0..=MAX_FILTERS_PER_REQ {
            parts.push(serde_json::json!({ "kinds": [9] }));
        }
        let (sub_id, reason) = refusal(&Value::Array(parts).to_string());
        assert_eq!(sub_id, "over-filtered");
        assert!(reason.contains("filter limit"), "{reason}");

        // A filter whose value lists are unbounded.
        let ids: Vec<String> = (0..=MAX_FILTER_VALUES)
            .map(|i| format!("{i:064x}"))
            .collect();
        let (sub_id, _) = refusal(&serde_json::json!(["REQ", "too-many-ids", { "ids": ids }]).to_string());
        assert_eq!(sub_id, "too-many-ids");

        // A REQ with no filter at all.
        let (sub_id, reason) = refusal(&serde_json::json!(["REQ", "no-filters"]).to_string());
        assert_eq!(sub_id, "no-filters");
        assert!(reason.contains("no filter"), "{reason}");

        // COUNT is unimplemented, and says so against the caller's own id rather
        // than leaving the subscription unanswered.
        let (sub_id, reason) = refusal(&serde_json::json!(["COUNT", "how-many", { "kinds": [9] }]).to_string());
        assert_eq!(sub_id, "how-many");
        assert_eq!(reason, COUNT_UNSUPPORTED);

        // And a well-formed REQ is still a REQ.
        assert!(matches!(
            ClientMessage::parse(
                serde_json::json!(["REQ", "live", { "kinds": [9] }])
                    .to_string()
                    .as_bytes()
            ),
            Ok(ClientMessage::Req { .. })
        ));
    }

    #[test]
    fn an_oversized_frame_is_refused_before_it_is_parsed() {
        let huge = vec![b'a'; MAX_CLIENT_MESSAGE_BYTES + 1];
        match ClientMessage::parse(&huge) {
            Err(BuzzError::TooLarge { got, max }) => {
                assert_eq!(got, MAX_CLIENT_MESSAGE_BYTES + 1);
                assert_eq!(max, MAX_CLIENT_MESSAGE_BYTES);
            }
            other => panic!("expected a size refusal, got {other:?}"),
        }
    }

    #[test]
    fn subscription_ids_are_bounded_and_printable() {
        assert!(check_sub_id("live-3f7c").is_ok());
        assert!(check_sub_id(&"a".repeat(MAX_SUB_ID_CHARS)).is_ok());
        assert!(check_sub_id("").is_err());
        assert!(check_sub_id(&"a".repeat(MAX_SUB_ID_CHARS + 1)).is_err());
        assert!(check_sub_id("live\n1").is_err());
        assert!(check_sub_id("live\u{0}1").is_err());
        assert!(check_sub_id("live-é").is_err());
    }

    #[test]
    fn a_filter_with_unbounded_value_lists_is_refused() {
        // Named rather than raised: see the refusal test above for why the answer
        // has to carry the subscription id.
        let ids: Vec<String> = (0..=MAX_FILTER_VALUES)
            .map(|i| format!("{i:064x}"))
            .collect();
        let raw = serde_json::json!(["REQ", "s", { "ids": ids }]).to_string();
        assert!(matches!(
            ClientMessage::parse(raw.as_bytes()),
            Ok(ClientMessage::RefusedReq { .. })
        ));
        let tags: Vec<String> = (0..=MAX_FILTER_VALUES)
            .map(|i| format!("{i:064x}"))
            .collect();
        let raw = serde_json::json!(["REQ", "s", { "#e": tags }]).to_string();
        assert!(matches!(
            ClientMessage::parse(raw.as_bytes()),
            Ok(ClientMessage::RefusedReq { .. })
        ));
    }

    #[test]
    fn outbound_frames_are_the_exact_nip01_arrays() {
        let event = note();
        let rendered = |m: &RelayMessage| String::from_utf8(m.to_bytes().unwrap()).unwrap();
        assert_eq!(
            rendered(&RelayMessage::Eose {
                sub_id: "live-1".into()
            }),
            r#"["EOSE","live-1"]"#
        );
        assert_eq!(
            rendered(&RelayMessage::accepted(&event.id)),
            format!(r#"["OK","{}",true,""]"#, to_hex(&event.id))
        );
        assert_eq!(
            rendered(&RelayMessage::refused(
                &event.id,
                &BuzzError::Unauthorized("not a member of this channel".into())
            )),
            format!(
                r#"["OK","{}",false,"restricted: not a member of this channel"]"#,
                to_hex(&event.id)
            )
        );
        assert_eq!(
            rendered(&RelayMessage::closed("live-1", "restricted: nope")),
            r#"["CLOSED","live-1","restricted: nope"]"#
        );
        assert_eq!(
            rendered(&RelayMessage::Notice {
                message: "rate-limited: retry in 4s".into()
            }),
            r#"["NOTICE","rate-limited: retry in 4s"]"#
        );
        assert_eq!(
            rendered(&RelayMessage::Auth {
                challenge: "abc".into()
            }),
            r#"["AUTH","abc"]"#
        );
        // The EVENT frame carries the subscription id and the event object, in
        // that order — the client indexes rest[0]/rest[1] positionally.
        let framed = rendered(&RelayMessage::Event {
            sub_id: "live-1".into(),
            event: Box::new(event.clone()),
        });
        let parsed: Vec<Value> = serde_json::from_str(&framed).unwrap();
        assert_eq!(parsed[0], serde_json::json!("EVENT"));
        assert_eq!(parsed[1], serde_json::json!("live-1"));
        assert_eq!(parsed[2]["id"], serde_json::json!(to_hex(&event.id)));
    }

    #[test]
    fn a_refusal_carries_the_relays_own_wire_token() {
        let id = note().id;
        for (error, prefix) in [
            (BuzzError::InvalidEvent("bad".into()), "invalid: "),
            (BuzzError::Unauthorized("no".into()), "restricted: "),
            (BuzzError::RateLimited, "rate-limited: "),
            (BuzzError::Duplicate("seen".into()), "duplicate: "),
        ] {
            match RelayMessage::refused(&id, &error) {
                RelayMessage::Ok {
                    accepted, message, ..
                } => {
                    assert!(!accepted);
                    assert!(message.starts_with(prefix), "{message}");
                }
                other => panic!("expected an OK, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_correct_auth_event_binds_its_author() {
        let challenge = derive_challenge(SECRET, "conn-1");
        let event = auth_event(&challenge, RELAY, NOW_SECS);
        let (_, alice) = actor("alice");
        assert_eq!(
            verify_auth(&challenge, &event, Some(RELAY), NOW_MS).expect("verifies"),
            alice
        );
        // Within the window on both sides.
        assert!(verify_auth(
            &challenge,
            &auth_event(&challenge, RELAY, NOW_SECS - AUTH_SKEW_SECS),
            Some(RELAY),
            NOW_MS
        )
        .is_ok());
        assert!(verify_auth(
            &challenge,
            &auth_event(&challenge, RELAY, NOW_SECS + AUTH_SKEW_SECS),
            Some(RELAY),
            NOW_MS
        )
        .is_ok());
    }

    #[test]
    fn an_auth_event_for_another_challenge_is_refused() {
        let ours = derive_challenge(SECRET, "conn-1");
        let theirs = derive_challenge(SECRET, "conn-2");
        assert_ne!(ours, theirs);
        let event = auth_event(&theirs, RELAY, NOW_SECS);
        assert!(verify_auth(&ours, &event, Some(RELAY), NOW_MS).is_err());
        // An absent challenge tag is refused too.
        let (sk, _) = actor("alice");
        let bare = Event::sign_new(
            &sk,
            kind::KIND_AUTH,
            vec![vec!["relay".into(), RELAY.into()]],
            "",
            NOW_SECS,
        )
        .unwrap();
        assert!(verify_auth(&ours, &bare, Some(RELAY), NOW_MS).is_err());
    }

    #[test]
    fn an_auth_event_for_another_relay_is_refused() {
        let challenge = derive_challenge(SECRET, "conn-1");
        for elsewhere in [
            "wss://evil.example/buzz/ws/",
            "wss://buzz.test.evil.example/",
            "https://buzz-test/",
            "not-a-url",
        ] {
            let event = auth_event(&challenge, elsewhere, NOW_SECS);
            assert!(
                verify_auth(&challenge, &event, Some(RELAY), NOW_MS).is_err(),
                "accepted {elsewhere}"
            );
        }
        // The same host with an explicit port and a different path is the same
        // relay: the app never learns which port the gateway terminated on.
        let ok = auth_event(&challenge, "wss://BUZZ.test:443/buzz/ws/x", NOW_SECS);
        assert!(verify_auth(&challenge, &ok, Some(RELAY), NOW_MS).is_ok());
    }

    #[test]
    fn an_unconfigured_relay_url_still_requires_a_relay_tag() {
        let challenge = derive_challenge(SECRET, "conn-1");
        let (sk, _) = actor("alice");
        let no_relay = Event::sign_new(
            &sk,
            kind::KIND_AUTH,
            vec![vec!["challenge".into(), challenge.clone()]],
            "",
            NOW_SECS,
        )
        .unwrap();
        assert!(verify_auth(&challenge, &no_relay, None, NOW_MS).is_err());
        let with_relay = auth_event(&challenge, "wss://anything.example/", NOW_SECS);
        assert!(verify_auth(&challenge, &with_relay, None, NOW_MS).is_ok());
    }

    #[test]
    fn a_stale_or_future_auth_event_is_refused() {
        let challenge = derive_challenge(SECRET, "conn-1");
        for created_at in [
            NOW_SECS - AUTH_SKEW_SECS - 1,
            NOW_SECS + AUTH_SKEW_SECS + 1,
            0,
        ] {
            let event = auth_event(&challenge, RELAY, created_at);
            assert!(
                verify_auth(&challenge, &event, Some(RELAY), NOW_MS).is_err(),
                "accepted created_at {created_at}"
            );
        }
    }

    #[test]
    fn a_forged_auth_event_is_refused() {
        let challenge = derive_challenge(SECRET, "conn-1");
        let good = auth_event(&challenge, RELAY, NOW_SECS);

        // Someone else's pubkey on a valid signature: the id no longer matches.
        let (_, bob) = actor("bob");
        let mut stolen = good.clone();
        stolen.pubkey = bob;
        assert!(verify_auth(&challenge, &stolen, Some(RELAY), NOW_MS).is_err());

        // Tags edited after signing.
        let mut edited = good.clone();
        edited.tags.push(vec!["extra".into(), "x".into()]);
        assert!(verify_auth(&challenge, &edited, Some(RELAY), NOW_MS).is_err());

        // A flipped signature bit, with the id left intact.
        let mut flipped = good.clone();
        flipped.sig[10] ^= 0x08;
        assert!(verify_auth(&challenge, &flipped, Some(RELAY), NOW_MS).is_err());

        // The right shape under the wrong kind.
        let (sk, _) = actor("alice");
        let wrong_kind = Event::sign_new(
            &sk,
            kind::KIND_STREAM_MESSAGE,
            vec![
                vec!["relay".into(), RELAY.into()],
                vec!["challenge".into(), challenge.clone()],
            ],
            "",
            NOW_SECS,
        )
        .unwrap();
        assert!(verify_auth(&challenge, &wrong_kind, Some(RELAY), NOW_MS).is_err());
    }

    #[test]
    fn challenges_differ_per_connection_and_per_key() {
        let a = derive_challenge(SECRET, "conn-1");
        let b = derive_challenge(SECRET, "conn-2");
        let c = derive_challenge(b"another-key-entirely-------------", "conn-1");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
        assert_eq!(a, derive_challenge(SECRET, "conn-1"), "stable per open");
        assert!(!a.contains("conn-1"));
    }

    #[test]
    fn relay_hosts_are_normalized() {
        assert_eq!(
            relay_host("wss://buzz.test/x").as_deref(),
            Some("buzz.test")
        );
        assert_eq!(
            relay_host("WSS://Buzz.Test:443/").as_deref(),
            Some("buzz.test")
        );
        assert_eq!(relay_host("buzz.test").as_deref(), Some("buzz.test"));
        assert_eq!(
            relay_host("wss://u:p@buzz.test/").as_deref(),
            Some("buzz.test")
        );
        assert_eq!(relay_host("wss://[::1]:8443/").as_deref(), Some("[::1]"));
        assert_eq!(relay_host("/buzz/ws/"), None);
        assert_eq!(relay_host("wss://"), None);
        assert_eq!(relay_host(""), None);
    }

    #[test]
    fn a_fresh_connection_is_anonymous() {
        let conn = Connection::open(SECRET, "conn-1", "test-frames", NOW_MS);
        assert_eq!(conn.authenticated(), None);
        assert_eq!(conn.reader(), ANON_READER);
        assert!(conn.subscriptions.is_empty());
        assert_eq!(conn.generation, 1);
        assert!(conn.is_live(NOW_MS));
        assert!(!conn.is_live(NOW_MS + CONNECTION_TTL_MS));
    }

    #[test]
    fn binding_a_pubkey_makes_it_the_reader() {
        let (_, alice) = actor("alice");
        let mut conn = Connection::open(SECRET, "conn-1", "test-frames", NOW_MS);
        conn.bind(&alice);
        assert_eq!(conn.authenticated(), Some(alice));
        assert_eq!(conn.reader(), alice);
        // Round trips through the document form the store holds.
        let bytes = serde_json::to_vec(&conn).unwrap();
        let back: Connection = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, conn);
        assert_eq!(back.authenticated(), Some(alice));
    }

    #[test]
    fn the_subscription_table_replaces_by_id_and_is_bounded() {
        let mut conn = Connection::open(SECRET, "conn-1", "test-frames", NOW_MS);
        let narrow = Filter {
            kinds: Some(vec![9]),
            ..Filter::default()
        };
        for i in 0..MAX_SUBSCRIPTIONS {
            assert!(conn
                .upsert_subscription(format!("s{i}"), vec![Filter::default()])
                .is_ok());
        }
        assert_eq!(conn.subscriptions.len(), MAX_SUBSCRIPTIONS);
        // One more distinct id is refused, with the terminal message the
        // client classifies...
        let refused = conn.upsert_subscription("overflow".into(), vec![Filter::default()]);
        assert!(refused
            .as_ref()
            .is_err_and(|e| e.starts_with("error: too many subscriptions")));
        // ...but re-issuing an existing id REPLACES its filters (NIP-01), which
        // is what the client's CLOSED-retry path re-sends.
        assert!(conn
            .upsert_subscription("s0".into(), vec![narrow.clone()])
            .is_ok());
        assert_eq!(conn.subscriptions.len(), MAX_SUBSCRIPTIONS);
        assert_eq!(
            conn.subscription("s0").map(|s| s.filters.clone()),
            Some(vec![narrow])
        );
        assert!(conn.remove_subscription("s0"));
        assert!(!conn.remove_subscription("s0"));
        assert!(conn
            .upsert_subscription("overflow".into(), vec![Filter::default()])
            .is_ok());
    }

    /// The table is bounded in BYTES, not only in count: the document it lives
    /// in must fit the config store's record cap, and a per-field value cap
    /// bounds list lengths, not value bytes. One oversized REQ is refused
    /// without evicting or altering anything already held — and a replacement
    /// that would blow the budget leaves the original subscription intact.
    #[test]
    fn the_subscription_table_is_bounded_in_bytes_and_a_refusal_mutates_nothing() {
        let mut conn = Connection::open(SECRET, "conn-1", "test-frames", NOW_MS);
        // ~1 MiB of filter values in ONE subscription: 512 ids × 2 KiB each,
        // inside every per-list bound but far over the byte budget.
        let huge = Filter {
            ids: Some(vec!["a".repeat(2048); MAX_FILTER_VALUES]),
            ..Filter::default()
        };
        assert!(
            check_filter_bounds(&huge).is_ok(),
            "each list is under its length cap"
        );

        let narrow = Filter {
            kinds: Some(vec![9]),
            ..Filter::default()
        };
        assert!(conn
            .upsert_subscription("live".into(), vec![narrow.clone()])
            .is_ok());

        let refused = conn.upsert_subscription("flood".into(), vec![huge.clone()]);
        assert!(
            refused.as_ref().is_err_and(|e| e.contains("byte budget")),
            "an over-budget REQ is refused legibly: {refused:?}"
        );
        assert!(
            conn.subscription("flood").is_none(),
            "a refused REQ holds no slot"
        );

        // Replacing an existing subscription with an over-budget one is also
        // refused, and the original filters survive.
        let replaced = conn.upsert_subscription("live".into(), vec![huge]);
        assert!(replaced.is_err());
        assert_eq!(
            conn.subscription("live").map(|s| s.filters.clone()),
            Some(vec![narrow]),
            "a refused replacement leaves the original subscription intact"
        );
    }

    #[test]
    fn touching_extends_the_expiry_only_when_it_is_near() {
        let mut conn = Connection::open(SECRET, "conn-1", "test-frames", NOW_MS);
        assert!(!conn.needs_touch(NOW_MS));
        assert!(conn.needs_touch(NOW_MS + CONNECTION_TTL_MS / 2 + 1));
        conn.touch(NOW_MS + CONNECTION_TTL_MS / 2 + 1);
        assert!(!conn.needs_touch(NOW_MS + CONNECTION_TTL_MS / 2 + 1));
    }

    #[test]
    fn limits_are_clamped_and_zero_is_honored() {
        let with = |limit: Option<u32>| {
            effective_limit(&Filter {
                limit,
                ..Filter::default()
            })
        };
        assert_eq!(with(None), DEFAULT_REQ_LIMIT);
        assert_eq!(with(Some(0)), 0);
        assert_eq!(with(Some(50)), 50);
        assert_eq!(with(Some(10_000)), MAX_REQ_LIMIT);
        assert_eq!(with(Some(u32::MAX)), MAX_REQ_LIMIT);
    }

    #[test]
    fn plans_pick_the_narrowest_index() {
        let filter = |raw: Value| -> Filter { serde_json::from_value(raw).expect("filter") };
        let id = to_hex(&note().id);
        assert_eq!(
            plan_read(&filter(serde_json::json!({ "ids": [id.clone()] }))),
            ReadPlan::Ids(vec![note().id])
        );
        // A truncated id cannot be point-read, so the plan widens.
        assert_eq!(
            plan_read(&filter(serde_json::json!({ "ids": ["dead"], "kinds": [9] }))),
            ReadPlan::Kinds(vec![9])
        );
        assert_eq!(
            plan_read(&filter(serde_json::json!({ "kinds": [7], "#e": [id.clone()] }))),
            ReadPlan::Etags(vec![id.clone()])
        );
        assert_eq!(
            plan_read(&filter(serde_json::json!({ "kinds": [9], "#h": ["chan-1"] }))),
            ReadPlan::Channel("chan-1".into())
        );
        // The mention index only wins when no channel narrows it further.
        assert_eq!(
            plan_read(&filter(
                serde_json::json!({ "kinds": [9], "#h": ["chan-1"], "#p": [id.clone()] })
            )),
            ReadPlan::Channel("chan-1".into())
        );
        assert_eq!(
            plan_read(&filter(serde_json::json!({ "kinds": [9], "#p": [id.clone()] }))),
            ReadPlan::Mentions(vec![id.clone()])
        );
        assert_eq!(
            plan_read(&filter(serde_json::json!({ "authors": [id.clone()], "kinds": [9] }))),
            ReadPlan::Authors(vec![note().id], vec![9])
        );
        assert_eq!(plan_read(&filter(serde_json::json!({ "limit": 10 }))), ReadPlan::Global);
        // An empty tag list constrains to nothing; it must not select an index
        // it cannot read (and `Filter::matches` will refuse every event anyway).
        assert_eq!(plan_read(&filter(serde_json::json!({ "#e": [] }))), ReadPlan::Global);
        // More kinds than there are indexes worth walking falls back to one walk.
        let many: Vec<u32> = (0..MAX_KIND_INDEXES as u32 + 1).collect();
        assert_eq!(
            plan_read(&filter(serde_json::json!({ "kinds": many }))),
            ReadPlan::Global
        );
    }

    #[test]
    fn an_until_cursor_sorts_between_the_newer_and_the_wanted_rows() {
        let prefix = crate::relay::keys::ts_prefix("main", Some("chan-1"));
        let until = 1_700_000_000;
        let cursor = until_cursor(&prefix, until);
        // It decodes as a cursor for this very query...
        let raw = crate::relay::query::decode_cursor(&cursor, &prefix).expect("a valid cursor");
        let row = |created_at: u64, id: u8| {
            crate::relay::keys::ts_index("main", Some("chan-1"), created_at, &[id; 32])
        };
        // ...and a `scan(start_after = cursor)` therefore returns exactly the
        // rows at or below `until`: every newer row sorts at or before it,
        // every wanted row strictly after it.
        assert!(row(until + 2, 0xff) < raw, "a newer row is excluded");
        assert!(row(until + 1, 0xff) <= raw, "the boundary row is excluded");
        assert!(row(until + 1, 0x00) < raw, "the boundary row is excluded");
        assert!(raw < row(until, 0x00), "the newest wanted row is included");
        assert!(raw < row(until, 0xff));
        assert!(raw < row(until - 1, 0x00), "older rows are included");
    }

    #[test]
    fn gated_kinds_require_authentication() {
        let filter = |raw: Value| -> Filter { serde_json::from_value(raw).expect("filter") };
        assert!(needs_authentication(&filter(
            serde_json::json!({ "kinds": [kind::KIND_GIFT_WRAP] })
        )));
        assert!(needs_authentication(&filter(
            serde_json::json!({ "kinds": [kind::KIND_EVENT_REMINDER] })
        )));
        assert!(!needs_authentication(&filter(
            serde_json::json!({ "kinds": [kind::KIND_STREAM_MESSAGE] })
        )));
        // An unconstrained-kind filter is not itself gated: `may_deliver`
        // refuses the sensitive kinds inside it at delivery time.
        assert!(!needs_authentication(&filter(serde_json::json!({ "limit": 10 }))));
    }

    /// F2's second half: the ack discipline for a FAILED inbound frame.
    ///
    /// A transient failure is redelivered (redelivery is the frame's only retry);
    /// a terminal one is acknowledged with a `NOTICE`, because retrying it can
    /// only burn the envelope's attempt budget while the client waits for an
    /// answer that will never arrive.
    #[test]
    fn a_terminal_frame_failure_is_never_left_for_redelivery() {
        for transient in [
            BuzzError::Storage("the store is unreachable".into()),
            BuzzError::Conflict("the connection document contended".into()),
            BuzzError::RateLimited,
        ] {
            assert!(
                frame_should_redeliver(&transient),
                "{transient} can succeed on a retry and must redeliver"
            );
        }
        for terminal in [
            // The invariant-violation case: an encode that cannot fail failing,
            // a key that stopped being spellable. A retry re-runs the same bug.
            BuzzError::Internal("subscription table would not encode".into()),
            // A refusal that escaped the in-band path instead of becoming a
            // CLOSED/OK frame.
            BuzzError::InvalidEvent("frame is not a client message".into()),
            BuzzError::Unauthorized("not a member".into()),
            BuzzError::NotFound("this connection is not open".into()),
        ] {
            assert!(
                !frame_should_redeliver(&terminal),
                "{terminal} cannot succeed on a retry and must be acknowledged"
            );
        }
    }

    /// The anonymous reader can never be an author, and can never be treated as
    /// a channel member: it is the value the access gate already refuses.
    #[test]
    fn the_anonymous_reader_is_not_a_usable_identity() {
        assert!(schnorr::xonly_pubkey(&ANON_READER).is_none());
        let event = note();
        assert_ne!(event.pubkey, ANON_READER);
        assert!(!crate::relay::access::may_deliver(
            &event,
            &ANON_READER,
            crate::relay::access::ChannelAccess::Private {
                reader_is_member: false
            },
            true
        )
        .is_allowed());
    }
}
