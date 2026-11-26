//! Live delivery: the durable feed and the per-connection pumps that tail it.
//!
//! Buzz fanned events out over Redis pub/sub to sockets it held. This app holds
//! no sockets, so delivery inverts: every accepted event is appended to a
//! durable per-community **feed**, and each SSE connection runs a pump that
//! reads the feed from that connection's cursor and pushes what the subscriber
//! is allowed to see. There is no fan-out registry — the log *is* the fan-out,
//! which is why a reconnect can resume exactly and a late joiner can replay.
//!
//! Three properties the code below is shaped by, each learned from the platform
//! rather than assumed:
//!
//! * **Delivery is at-least-once.** A topic append is retried by the client when
//!   a response is lost, so a failover alone can duplicate a record without any
//!   crash. Consumers therefore dedup by event id ([`SeenIds`]) rather than
//!   trusting the feed to hold each event once.
//! * **Cursors need their generation.** Offsets are dense *per topic* and restart
//!   at zero in a rotated generation, and the topic front fails softly — a
//!   too-old cursor is silently clamped, a too-new one silently returns nothing.
//!   A pump that tracked only an offset would strand itself, so it compares the
//!   generation and the first record's offset and emits a `reset` when either
//!   disagrees with what the client asked for.
//! * **Invalidation rides the feed.** Buzz dropped its access caches the moment a
//!   membership change committed, via a dedicated Redis bus. That bus is gone;
//!   instead a pump watches for membership and moderation records in the feed it
//!   is already reading and drops the affected cache entries *before* delivering
//!   anything further, which keeps removal enforcement prompt without inventing
//!   a second channel.

use std::collections::{HashMap, VecDeque};

use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json;

use crate::relay::access::{may_deliver, ChannelAccess, Decision};
use crate::relay::channel::Visibility;
use crate::relay::event::{Event, EventId, PubKey};
use crate::relay::filter::{filters_match, Filter};
use crate::relay::kind;
use crate::relay::session::{Cursor, ResetReason};
use crate::relay::{BuzzError, Result};

/// How many feed records one pump batch reads.
///
/// This number is a MEMORY bound. Each record can be a whole event, so the worst
/// case per batch is `BATCH_LIMIT * crate::relay::event::MAX_SERIALIZED_EVENT_BYTES`.
/// At 256 that is 60 MiB *per pump*, and there is one pump per connection with no
/// cap on connections. Measured on the live deployment at 256: ~10 MiB of RSS per
/// subscribing connection, 40 concurrent sessions at 439 MiB (86% of the app's
/// 512 MiB cgroup), and 56 sessions OOM-killed the relay outright
/// (`memory.events: oom_kill 1`). Any keypair can open those sessions, so a bound
/// that high is a remotely reachable availability failure.
///
/// 16 is too far the other way. Two live scenarios that assert a SECOND client
/// receives live events (a DM notice reaching its recipient, a renamed room's
/// traffic reaching a colleague) failed on two consecutive runs at 16 and passed
/// at 256. A pump advances at most BATCH_LIMIT records per cycle over the SHARED
/// community feed, so under concurrent load a consumer that cannot keep pace with
/// the feed's growth falls permanently behind.
///
/// 64 is the middle the measurements support. The OOM was driven by ~40 KB of
/// ACTUAL cost per record (10 MiB observed per connection at 256), so 64 records
/// is ~2.5 MiB per pump — 48 concurrent sessions land near 120 MiB, comfortably
/// inside the 512 MiB cgroup — with 4x the drain rate of 16. The
/// theoretical worst case (64 * 240 KiB = 15 MiB per batch) is not
/// multiplied by the number of connections: [`MAX_HELD_BATCHES`] bounds how many
/// batches the whole process holds at once.
///
/// The RIGHT fix is a byte budget, the way `query::PageBudget` bounds a read
/// page: stop at N bytes rather than N records. The topics client an app is
/// given exposes only `read_from(topic, offset, limit)` with a record count, so
/// that is not expressible here yet.
pub const BATCH_LIMIT: u32 = 64;

/// The worst-case bytes one batch can buffer, stated so the bound above is
/// checkable rather than asserted. Kept well under the 4 MiB reply ceiling.
const _: () = assert!(
    BATCH_LIMIT as usize * crate::relay::event::MAX_SERIALIZED_EVENT_BYTES < 16 * 1024 * 1024
);

/// Bound serialized records in each server page.
pub const BATCH_BYTES: u64 = 4 * 1024 * 1024;

/// How many feed batches may be held in memory at once, across EVERY pump in
/// this process (SSE and WebSocket alike).
///
/// [`BATCH_LIMIT`] bounds one batch by records, and the default build can only
/// trim a page to [`BATCH_BYTES`] after `read_from` has materialized all of it,
/// so one read can peak at `BATCH_LIMIT * MAX_SERIALIZED_EVENT_BYTES` (~15 MiB).
/// With one pump per connection and no cap on connections, that product was the
/// remotely reachable OOM the comment on [`BATCH_LIMIT`] records. A pump now
/// holds a [`BatchSlot`] from its read until it has delivered the batch, so the
/// worst case is this many batches, not one per connection: 16 × 15 MiB is
/// 240 MiB inside the 512 MiB cgroup. Pumps beyond it WAIT for a slot rather than
/// being refused, and an empty read gives its slot back before the pump sleeps,
/// so idle connections hold nothing. A batch is delivered in milliseconds, so
/// 16 concurrent batches is far more throughput than one instance's pumps need.
pub const MAX_HELD_BATCHES: usize = 16;

/// How often a pump waiting for a [`BatchSlot`] tries again.
const BATCH_SLOT_POLL_MS: u64 = 10;

/// The process-wide count [`MAX_HELD_BATCHES`] bounds.
static HELD_BATCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Permission to hold one feed batch in memory; released on drop.
#[derive(Debug)]
pub struct BatchSlot(());

impl BatchSlot {
    /// Take a slot if one is free.
    #[must_use]
    pub fn try_acquire() -> Option<Self> {
        use std::sync::atomic::Ordering;
        HELD_BATCHES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                (held < MAX_HELD_BATCHES).then_some(held + 1)
            })
            .ok()
            .map(|_| Self(()))
    }

    /// Wait for a slot.
    pub async fn acquire() -> Self {
        loop {
            if let Some(slot) = Self::try_acquire() {
                return slot;
            }
            ferrite_app::sleep_ms(BATCH_SLOT_POLL_MS).await;
        }
    }

    /// How many slots are held right now.
    #[must_use]
    pub fn held() -> usize {
        HELD_BATCHES.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Drop for BatchSlot {
    fn drop(&mut self) {
        HELD_BATCHES.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Run one feed read while holding a [`BatchSlot`], keeping the slot only when
/// the read produced records to deliver.
///
/// The slot is taken BEFORE the read is polled, because the read is what
/// materializes the page: a pump that reads first and asks for a slot afterwards
/// already holds the batch the slot was meant to bound, so with many
/// connections the process holds one page per pump rather than
/// [`MAX_HELD_BATCHES`]. That was the parked-read build's order (a slot only
/// after `read_wait` returned), and this is the one place both pumps and both
/// builds now read through, so they cannot drift apart again.
///
/// `read` is a future and futures are lazy, so building it costs nothing until
/// it is awaited here. `holds_records` says whether the result is a batch that
/// will be held until delivery; when it is not (an empty page), the slot is
/// released before returning so an idle pump's sleep or next park holds
/// nothing. Under the parked read a slot IS held for the park itself (up to
/// [`WAIT_MS`]); an append wakes every parked reader at once, so slots turn
/// over as soon as there is anything to deliver.
pub async fn read_holding_slot<O>(
    read: impl core::future::Future<Output = O>,
    holds_records: impl FnOnce(&O) -> bool,
) -> (Option<BatchSlot>, O) {
    let slot = BatchSlot::acquire().await;
    let out = read.await;
    let slot = holds_records(&out).then_some(slot);
    (slot, out)
}

/// The worst case the slots allow, stated so it is checked rather than asserted:
/// well inside the app's 512 MiB cgroup.
const _: () = assert!(
    MAX_HELD_BATCHES * BATCH_LIMIT as usize * crate::relay::event::MAX_SERIALIZED_EVENT_BYTES
        < 256 * 1024 * 1024
);

/// Wake periodically to check the connection lease and application revocations.
/// Appends wake the server wait immediately; this is not a polling delay.
pub const WAIT_MS: u64 = 1_000;

/// How many recently delivered event ids a pump remembers for deduplication.
/// Sized well past a plausible duplicate window (a failover retry lands within
/// milliseconds) without becoming a memory sink.
pub const SEEN_CAPACITY: usize = 4_096;

/// One record in the delivery feed.
///
/// It carries the event plus the facts a gate needs that are *not* recoverable
/// from the event itself: the community it was admitted into, and the channel it
/// resolved to (which for reactions and deletions is derived from the target
/// event, not from a tag the author supplied).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct FeedRecord {
    pub community: String,
    /// Resolved channel id, or `None` for events that are global in the community.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    pub event: Event,
}

impl FeedRecord {
    #[must_use]
    pub const fn new(community: String, channel: Option<String>, event: Event) -> Self {
        Self {
            community,
            channel,
            event,
        }
    }

    /// Ephemeral events (presence, typing) ride the feed but were never stored,
    /// so a pump may drop them first under backpressure: re-sending a stale
    /// typing indicator is worse than not sending it.
    #[must_use]
    pub const fn is_droppable(&self) -> bool {
        kind::is_ephemeral(self.event.kind)
    }

    /// Records that change who may see what. A pump invalidates its access cache
    /// when one passes, which is what replaces buzz's cross-pod invalidation bus.
    #[must_use]
    pub fn invalidates_access(&self) -> bool {
        kind::is_membership_change(self.event.kind)
            || kind::is_moderation_command(self.event.kind)
            || kind::is_channel_metadata(self.event.kind)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|e| BuzzError::Internal(format!("feed encode: {e}")))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes)
            .map_err(|e| BuzzError::InvalidEvent(format!("feed decode: {e}")))
    }
}

/// What a pump sends to one connection. Serialized into the SSE `data` field —
/// never the event *name*, because the gateway's stream face has no `id:` field
/// and a per-frame numeric name would make a standards-compliant `EventSource`
/// undeliverable (it dispatches by exact event type).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", tag = "type", rename_all = "lowercase")]
pub enum Frame {
    /// A delivered event with the cursor to resume after it.
    Event { offset: String, event: Event },
    /// Liveness. The gateway keep-alives the socket on its own, so without an
    /// application-level beat a client cannot distinguish a healthy quiet stream
    /// from a pump that died with the socket still open.
    Heartbeat { offset: String },
    /// Start over: the client's cursor cannot be honored.
    Reset { reason: String, from: String },
}

impl Frame {
    /// The stable SSE event name for this frame.
    #[must_use]
    pub const fn event_name(&self) -> &'static str {
        match self {
            Self::Event { .. } => "event",
            Self::Heartbeat { .. } => "heartbeat",
            Self::Reset { .. } => "reset",
        }
    }

    #[must_use]
    pub fn event_at(cursor: Cursor, event: Event) -> Self {
        Self::Event {
            offset: cursor.encode(),
            event,
        }
    }

    #[must_use]
    pub fn heartbeat_at(cursor: Cursor) -> Self {
        Self::Heartbeat {
            offset: cursor.encode(),
        }
    }

    #[must_use]
    pub fn reset(reason: ResetReason, from: Cursor) -> Self {
        Self::Reset {
            reason: reason.as_str().to_string(),
            from: from.encode(),
        }
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|e| BuzzError::Internal(format!("frame encode: {e}")))
    }
}

/// A bounded ring of recently seen event ids, for at-least-once deduplication.
#[derive(Debug, Default)]
pub struct SeenIds {
    order: VecDeque<EventId>,
    set: HashMap<EventId, ()>,
}

impl SeenIds {
    #[must_use]
    pub fn new() -> Self {
        Self {
            order: VecDeque::with_capacity(64),
            set: HashMap::with_capacity(64),
        }
    }

    /// Record an id; returns `true` if it is new (and therefore deliverable).
    pub fn insert(&mut self, id: EventId) -> bool {
        if self.set.contains_key(&id) {
            return false;
        }
        if self.order.len() >= SEEN_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        self.order.push_back(id);
        self.set.insert(id, ());
        true
    }

    #[must_use]
    pub fn contains(&self, id: &EventId) -> bool {
        self.set.contains_key(id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// Cached channel facts with an expiry, mirroring buzz's 10-second access caches.
#[derive(Debug, Clone, Copy)]
struct CachedChannel {
    visibility: Visibility,
    reader_is_member: bool,
    expires_at_ms: u64,
}

/// A pump's per-connection view of the access facts it needs.
///
/// The TTL is the backstop, not the mechanism: entries are dropped eagerly when a
/// membership or moderation record passes through the feed, so a removal takes
/// effect on the next batch rather than at the end of a TTL window.
#[derive(Debug, Default)]
pub struct AccessCache {
    entries: HashMap<String, CachedChannel>,
    ttl_ms: u64,
}

impl AccessCache {
    #[must_use]
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            entries: HashMap::new(),
            ttl_ms,
        }
    }

    /// Look up cached facts for a channel, if still fresh.
    #[must_use]
    pub fn get(&self, channel: &str, now_ms: u64) -> Option<ChannelAccess> {
        let e = self.entries.get(channel)?;
        if now_ms >= e.expires_at_ms {
            return None;
        }
        Some(match e.visibility {
            Visibility::Open => ChannelAccess::Open,
            Visibility::Private => ChannelAccess::Private {
                reader_is_member: e.reader_is_member,
            },
        })
    }

    pub fn put(
        &mut self,
        channel: &str,
        visibility: Visibility,
        reader_is_member: bool,
        now_ms: u64,
    ) {
        self.entries.insert(
            channel.to_string(),
            CachedChannel {
                visibility,
                reader_is_member,
                expires_at_ms: now_ms.saturating_add(self.ttl_ms),
            },
        );
    }

    /// Drop one channel's facts (a membership or visibility change touched it).
    pub fn invalidate(&mut self, channel: &str) {
        self.entries.remove(channel);
    }

    /// Drop everything (a ban, or a change whose scope we cannot narrow).
    pub fn invalidate_all(&mut self) {
        self.entries.clear();
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The pure delivery decision for one feed record.
///
/// Returns the frame to send, or `None` when the subscriber should not see this
/// record — because it is a duplicate, does not match their filters, or the
/// access gate refused it. Every refusal path is silent to the client on purpose:
/// telling a non-member that an event exists in a channel they cannot read is
/// itself a leak.
#[must_use]
pub fn evaluate(
    record: &FeedRecord,
    community: &str,
    subscriber: &PubKey,
    filters: &[Filter],
    access: ChannelAccess,
    cursor: Cursor,
    seen: &mut SeenIds,
) -> Option<Frame> {
    if record.community != community {
        return None;
    }
    if !filters_match(filters, &record.event, record.channel.as_deref()) {
        return None;
    }
    if !matches!(
        may_deliver(
            &record.event,
            subscriber,
            access,
            record.community == community
        ),
        Decision::Allow
    ) {
        return None;
    }
    // Dedup last: an event refused by the gate must not consume a slot in the
    // seen-ring, or a later legitimate delivery of the same id could be dropped.
    if !seen.insert(record.event.id) {
        return None;
    }
    Some(Frame::event_at(cursor, record.event.clone()))
}

/// Decide whether a batch that came back from the feed can be honored at the
/// cursor the client asked for.
///
/// `first_offset` is the offset of the first record the feed returned. Because
/// `read_from` silently clamps a below-retention cursor rather than erroring, a
/// first offset *ahead* of the request means records were dropped and the client
/// must resynchronize.
#[must_use]
pub fn check_continuity(
    requested: Cursor,
    live_generation: u32,
    first_offset: Option<u64>,
) -> Option<ResetReason> {
    if requested.generation != live_generation {
        return Some(ResetReason::GenerationRotated);
    }
    match first_offset {
        Some(off) if off > requested.offset => Some(ResetReason::RetentionClamped),
        _ => None,
    }
}

/// A server-retained base exposes loss even when the returned page is empty.
#[must_use]
pub fn check_wait_continuity(
    requested: Cursor, live_generation: u32, retained_base: u64, first_offset: Option<u64>,
) -> Option<ResetReason> {
    check_continuity(requested, live_generation, first_offset).or_else(|| {
        (retained_base > requested.offset).then_some(ResetReason::RetentionClamped)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::relay::crypto::to_hex;

    fn pk(b: u8) -> PubKey {
        [b; 32]
    }

    fn event(kind: u32, author: PubKey, id_byte: u8, tags: Vec<Vec<String>>) -> Event {
        Event {
            id: [id_byte; 32],
            pubkey: author,
            created_at: 1_000,
            kind,
            tags,
            content: "hello".into(),
            sig: [0u8; 64],
        }
    }

    fn record(event: Event, channel: Option<&str>) -> FeedRecord {
        FeedRecord::new("main".into(), channel.map(str::to_string), event)
    }

    fn any_filter() -> Vec<Filter> {
        vec![Filter::default()]
    }

    /// The slot count is process-wide, so the tests that fill it take turns.
    static SLOT_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn batch_slots_are_bounded_process_wide_and_released_on_drop() {
        let _serial = SLOT_TESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Whatever else holds slots, this test can take at most the remainder,
        // and never more than the bound.
        let mut held = Vec::new();
        while let Some(slot) = BatchSlot::try_acquire() {
            held.push(slot);
            assert!(held.len() <= MAX_HELD_BATCHES, "the bound was exceeded");
        }
        assert_eq!(BatchSlot::held(), MAX_HELD_BATCHES);
        assert!(BatchSlot::try_acquire().is_none(), "a full set admits nobody");
        held.pop();
        let again = BatchSlot::try_acquire();
        assert!(again.is_some(), "a dropped slot is available again");
        drop(again);
        drop(held);
    }

    /// **A feed read does not start until it holds a slot.**
    ///
    /// The read is what materializes a page, so a pump that reads before it
    /// takes a slot already holds the batch the slot exists to bound. The parked
    /// `read_wait` build did exactly that: it took a slot only after the read
    /// returned, so every pump's page was in memory at once. This fills every
    /// slot and shows the read stays unpolled until one is freed, that the read
    /// then runs holding it, and that an empty page gives it back.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_feed_read_waits_for_a_slot_and_an_empty_page_releases_it() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let _serial = SLOT_TESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut held = Vec::new();
        while let Some(slot) = BatchSlot::try_acquire() {
            held.push(slot);
        }
        assert_eq!(BatchSlot::held(), MAX_HELD_BATCHES);

        let started = Arc::new(AtomicBool::new(false));
        let read = {
            let started = Arc::clone(&started);
            async move {
                started.store(true, Ordering::SeqCst);
                // What the process holds while the page is being materialized.
                BatchSlot::held()
            }
        };
        let pump = tokio::spawn(read_holding_slot(read, |_| true));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !started.load(Ordering::SeqCst),
            "a feed read ran while every batch slot was held"
        );

        held.pop();
        let (slot, held_while_reading) = pump.await.expect("the pump task");
        assert!(started.load(Ordering::SeqCst), "a freed slot lets the read run");
        assert_eq!(
            held_while_reading, MAX_HELD_BATCHES,
            "the read ran holding a slot of its own"
        );
        assert!(slot.is_some(), "a page with records keeps its slot until delivery");
        drop(slot);

        let (slot, ()) = read_holding_slot(async {}, |_| false).await;
        assert!(slot.is_none(), "an empty page gives its slot back");
        assert_eq!(BatchSlot::held(), MAX_HELD_BATCHES - 1);
        drop(held);
    }

    #[test]
    fn feed_record_round_trips() {
        let r = record(
            event(kind::KIND_STREAM_MESSAGE, pk(1), 9, vec![]),
            Some("chan-1"),
        );
        let bytes = r.encode().unwrap();
        assert_eq!(FeedRecord::decode(&bytes).unwrap(), r);
    }

    #[test]
    fn frames_carry_the_offset_in_data_not_the_event_name() {
        let f = Frame::event_at(
            Cursor::new(2, 17),
            event(kind::KIND_STREAM_MESSAGE, pk(1), 1, vec![]),
        );
        assert_eq!(f.event_name(), "event");
        let json = f.to_json().unwrap();
        assert!(json.contains("\"offset\":\"2:17\""), "{json}");
        assert_eq!(
            Frame::heartbeat_at(Cursor::new(2, 17)).event_name(),
            "heartbeat"
        );
        assert_eq!(
            Frame::reset(ResetReason::GenerationRotated, Cursor::new(1, 5)).event_name(),
            "reset"
        );
    }

    #[test]
    fn matching_event_in_an_open_channel_is_delivered() {
        let mut seen = SeenIds::new();
        let r = record(
            event(kind::KIND_STREAM_MESSAGE, pk(1), 5, vec![]),
            Some("chan-1"),
        );
        let f = evaluate(
            &r,
            "main",
            &pk(2),
            &any_filter(),
            ChannelAccess::Open,
            Cursor::new(0, 1),
            &mut seen,
        );
        assert!(matches!(f, Some(Frame::Event { .. })));
    }

    #[test]
    fn a_private_channel_non_member_receives_nothing() {
        let mut seen = SeenIds::new();
        let r = record(
            event(kind::KIND_STREAM_MESSAGE, pk(1), 5, vec![]),
            Some("chan-1"),
        );
        assert!(evaluate(
            &r,
            "main",
            &pk(2),
            &any_filter(),
            ChannelAccess::Private {
                reader_is_member: false
            },
            Cursor::new(0, 1),
            &mut seen,
        )
        .is_none());
    }

    #[test]
    fn unresolved_access_delivers_nothing() {
        let mut seen = SeenIds::new();
        let r = record(
            event(kind::KIND_STREAM_MESSAGE, pk(1), 5, vec![]),
            Some("chan-1"),
        );
        assert!(evaluate(
            &r,
            "main",
            &pk(2),
            &any_filter(),
            ChannelAccess::Unresolved,
            Cursor::new(0, 1),
            &mut seen,
        )
        .is_none());
    }

    #[test]
    fn a_record_from_another_community_is_never_delivered() {
        let mut seen = SeenIds::new();
        let mut r = record(event(kind::KIND_STREAM_MESSAGE, pk(1), 5, vec![]), None);
        r.community = "other".into();
        assert!(evaluate(
            &r,
            "main",
            &pk(1),
            &any_filter(),
            ChannelAccess::Global,
            Cursor::new(0, 1),
            &mut seen,
        )
        .is_none());
    }

    #[test]
    fn duplicates_are_dropped_but_refusals_do_not_poison_the_ring() {
        let mut seen = SeenIds::new();
        let e = event(kind::KIND_STREAM_MESSAGE, pk(1), 7, vec![]);
        let r = record(e, Some("chan-1"));

        // Refused by the gate: must NOT be remembered as delivered.
        assert!(evaluate(
            &r,
            "main",
            &pk(2),
            &any_filter(),
            ChannelAccess::Private {
                reader_is_member: false
            },
            Cursor::new(0, 1),
            &mut seen,
        )
        .is_none());
        assert!(seen.is_empty());

        // Now allowed: delivered once, then deduplicated.
        assert!(evaluate(
            &r,
            "main",
            &pk(2),
            &any_filter(),
            ChannelAccess::Open,
            Cursor::new(0, 1),
            &mut seen,
        )
        .is_some());
        assert!(evaluate(
            &r,
            "main",
            &pk(2),
            &any_filter(),
            ChannelAccess::Open,
            Cursor::new(0, 2),
            &mut seen,
        )
        .is_none());
    }

    #[test]
    fn author_only_kinds_are_gated_in_the_feed_too() {
        let mut seen = SeenIds::new();
        let r = record(event(kind::KIND_EVENT_REMINDER, pk(1), 3, vec![]), None);
        assert!(evaluate(
            &r,
            "main",
            &pk(2),
            &any_filter(),
            ChannelAccess::Global,
            Cursor::new(0, 1),
            &mut seen,
        )
        .is_none());
        let mut seen2 = SeenIds::new();
        assert!(evaluate(
            &r,
            "main",
            &pk(1),
            &any_filter(),
            ChannelAccess::Global,
            Cursor::new(0, 1),
            &mut seen2,
        )
        .is_some());
    }

    #[test]
    fn seen_ring_evicts_oldest_and_stays_bounded() {
        let mut seen = SeenIds::new();
        for i in 0..(SEEN_CAPACITY + 10) {
            let mut id = [0u8; 32];
            id[0] = (i % 251) as u8;
            id[1] = (i / 251) as u8;
            seen.insert(id);
        }
        assert_eq!(seen.len(), SEEN_CAPACITY);
    }

    #[test]
    fn ephemeral_records_are_droppable_and_others_are_not() {
        assert!(record(
            event(kind::KIND_TYPING_INDICATOR, pk(1), 1, vec![]),
            Some("c")
        )
        .is_droppable());
        assert!(record(event(kind::KIND_PRESENCE_UPDATE, pk(1), 1, vec![]), None).is_droppable());
        assert!(!record(
            event(kind::KIND_STREAM_MESSAGE, pk(1), 1, vec![]),
            Some("c")
        )
        .is_droppable());
    }

    #[test]
    fn access_cache_expires_and_can_be_invalidated_eagerly() {
        let mut c = AccessCache::new(10_000);
        c.put("chan-1", Visibility::Private, true, 1_000);
        assert_eq!(
            c.get("chan-1", 1_000),
            Some(ChannelAccess::Private {
                reader_is_member: true
            })
        );
        // TTL backstop.
        assert_eq!(c.get("chan-1", 11_000), None);

        c.put("chan-1", Visibility::Private, true, 20_000);
        // Eager invalidation is what keeps removal prompt.
        c.invalidate("chan-1");
        assert_eq!(c.get("chan-1", 20_001), None);

        c.put("chan-1", Visibility::Open, false, 30_000);
        c.put("chan-2", Visibility::Open, false, 30_000);
        c.invalidate_all();
        assert!(c.is_empty());
    }

    #[test]
    fn membership_and_moderation_records_trigger_invalidation() {
        let m = record(
            event(kind::KIND_GROUP_ADD_USER, pk(1), 1, vec![]),
            Some("chan-1"),
        );
        assert!(m.invalidates_access());
        let plain = record(
            event(kind::KIND_STREAM_MESSAGE, pk(1), 2, vec![]),
            Some("chan-1"),
        );
        assert!(!plain.invalidates_access());
    }

    #[test]
    fn a_wait_page_bounds_records_and_keeps_lease_checks_prompt() {
        assert!(u64::try_from(crate::relay::event::MAX_SERIALIZED_EVENT_BYTES).unwrap() < BATCH_BYTES);
        assert!(BATCH_BYTES <= 4 * 1024 * 1024);
        assert!(WAIT_MS <= 1_000);
    }

    #[test]
    fn empty_wait_pages_expose_retention_loss_without_advancing_silently() {
        let cursor = Cursor::new(2, 10);
        assert_eq!(check_wait_continuity(cursor, 2, 11, None), Some(ResetReason::RetentionClamped));
        assert_eq!(check_wait_continuity(cursor, 2, 10, None), None);
        assert_eq!(check_wait_continuity(cursor, 3, 11, None), Some(ResetReason::GenerationRotated));
        assert_eq!(check_wait_continuity(cursor, 2, 0, Some(12)), Some(ResetReason::RetentionClamped));
    }

    #[test]
    fn released_read_from_pages_detect_loss_only_on_a_non_empty_page() {
        // The default build reads with `read_from`, which reports no retained
        // base, so `released_feed_page` hands back the REQUESTED offset in its
        // place. The second clause of `check_wait_continuity` can therefore
        // never fire on that path; this pins exactly what the shipped pump does
        // detect, so nobody reads the wait-path test above as covering it.
        let cursor = Cursor::new(2, 10);
        let released_base = cursor.offset;
        // A trimmed cursor that `read_from` clamps forward: detected at once.
        assert_eq!(
            check_wait_continuity(cursor, 2, released_base, Some(40)),
            Some(ResetReason::RetentionClamped)
        );
        // A trimmed cursor answered with an EMPTY page: not detected yet. The
        // pump sleeps and re-reads; the Reset arrives with the first record the
        // topic appends after the trim (the clamp case above). Loss detection
        // is only as prompt as the next record — recorded in the README.
        assert_eq!(check_wait_continuity(cursor, 2, released_base, None), None);
        // Healthy continuation stays quiet.
        assert_eq!(check_wait_continuity(cursor, 2, released_base, Some(10)), None);
    }

    #[test]
    fn continuity_detects_rotation_and_retention_clamping() {
        // Healthy: same generation, feed starts where we asked.
        assert_eq!(check_continuity(Cursor::new(2, 10), 2, Some(10)), None);
        // Nothing came back — not an error, just an empty tail.
        assert_eq!(check_continuity(Cursor::new(2, 10), 2, None), None);
        // The live feed moved to a new generation.
        assert_eq!(
            check_continuity(Cursor::new(1, 10), 2, Some(0)),
            Some(ResetReason::GenerationRotated)
        );
        // Retention ate the requested offset: the feed answered from further on.
        assert_eq!(
            check_continuity(Cursor::new(2, 10), 2, Some(40)),
            Some(ResetReason::RetentionClamped)
        );
    }

    #[test]
    fn p_gated_events_reach_only_the_addressed_subscriber() {
        let author = pk(1);
        let addressee = pk(2);
        let r = record(
            event(
                kind::KIND_GIFT_WRAP,
                author,
                4,
                vec![vec!["p".into(), to_hex(&addressee)]],
            ),
            None,
        );
        let mut s1 = SeenIds::new();
        assert!(evaluate(
            &r,
            "main",
            &addressee,
            &any_filter(),
            ChannelAccess::Global,
            Cursor::new(0, 1),
            &mut s1
        )
        .is_some());
        let mut s2 = SeenIds::new();
        assert!(evaluate(
            &r,
            "main",
            &pk(3),
            &any_filter(),
            ChannelAccess::Global,
            Cursor::new(0, 1),
            &mut s2
        )
        .is_none());
    }
}
