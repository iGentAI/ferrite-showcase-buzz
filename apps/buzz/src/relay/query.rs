//! Historical reads: byte-budgeted paging over the ordered index keys.
//!
//! Buzz answered a `REQ` with one SQL query and one result set. Here a read is a
//! walk of [`crate::relay::keys`] index rows, and two platform facts shape every line of
//! it:
//!
//! * **A reply has a size ceiling.** `IngressReply.body` rides serde_json as a
//!   number array (≈3.6× inflation), so the ~16 MiB queue message is an effective
//!   ~4 MiB of body (the platform gateway's ingress body limit). Every page is
//!   therefore bounded by *both* [`MAX_PAGE_EVENTS`] and [`MAX_PAGE_BYTES`],
//!   whichever binds first, and hands back a cursor. One 500-event page of 4 KiB
//!   messages is 2 MiB; one 500-event page of 200 KiB messages is not a page, it
//!   is a dropped request — so the byte budget, not the count, is usually what
//!   ends a page.
//! * **A scan page is clamped to 1024 rows.** So [`timeline`] and friends loop
//!   internally, asking for exactly as many rows as they still have room for, and
//!   stop at [`MAX_SCAN_ROUNDS`] so one request can never walk an unbounded
//!   number of dead index rows.
//!
//! Two ordering/consistency rules come from the write side:
//!
//! * Index keys carry `u64::MAX - created_at`, so an *ascending* prefix scan is
//!   buzz's `created_at DESC`. Paging forward walks backwards in time.
//! * Ingest writes the event body **before** its index keys, so an index row with
//!   no body means a saga is still in flight (or the body was redacted). Such a
//!   row is **skipped, never surfaced** — a half-written row must not become a
//!   visible event. The cursor still advances past it, because a paging loop that
//!   refused to move would re-read the same dead row forever; the live delivery
//!   feed carries that event to subscribers regardless, and a later re-query sees
//!   it once the body is there.
//!
//! **This module performs no access control.** It selects an index and reads
//! bodies; deciding who may see a result is [`crate::relay::access`]'s job, and every
//! caller must run it over the returned events. Keeping the gate out of here is
//! deliberate: one chokepoint, not two half-copies.
//!
//! Cursors are opaque hex of the last index key consumed. They carry no
//! authority: [`decode_cursor`] requires the cursor to be a well-formed index key
//! *inside the prefix the caller is already reading*, so an edited cursor can
//! only reposition a caller within their own result set.

use ferrite_app::client::StoreClusterClient;
use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::tracing;

use crate::relay::crypto::{from_hex, to_hex};
use crate::relay::event::{Event, EventId, PubKey};
use crate::relay::kind::{self, Kind};
use crate::relay::{keys, store, BuzzError, Result};

/// Hard cap on events in one page, whatever a caller asks for.
pub const MAX_PAGE_EVENTS: usize = 500;

/// Hard cap on the serialized bytes of one page's events.
///
/// Sized against the ENCODED reply, not the page. `IngressReply.body` is a
/// `Vec<u8>` carried over the queue by `serde_json`, which writes a byte vector as
/// a JSON array of decimal integers — roughly **3.6 bytes of envelope per byte of
/// body**. So a page of `B` bytes costs `~3.6·B` on the wire, and the queue record
/// it rides caps at 16 MiB, giving an effective body ceiling near 4.4 MiB.
///
/// A 2 MiB page was tried first and is wrong twice over: `~7.5 MB` of encoded
/// reply is close enough to the ceiling to be reckless, and — the part that
/// actually bit — encoding *and then decoding* a two-million-element number array
/// takes long enough to blow the caller's 120-second ingress deadline, so the
/// request dies of a timeout for work that completed. A severe gate
/// (`severe_a_query_page_never_exceeds_the_byte_budget`) pins this.
///
/// 512 KiB keeps the encoded reply near 1.8 MB: comfortably inside the ceiling,
/// and fast to encode. A caller wanting more follows the cursor, which is the
/// paging contract anyway.
pub const MAX_PAGE_BYTES: usize = 512 * 1024;

/// Compile-time guard on the page/envelope relationship.
///
/// The budget is on the PAGE, but the bound it must respect is on the ENCODED
/// reply: `IngressReply.body` rides the queue as a `serde_json` array of decimal
/// integers, ~3.6 bytes per body byte, inside a 16 MiB record. A page budget that
/// breaks that has to fail the BUILD, not a test — a runtime assertion over
/// constants folds away to nothing, which is exactly the trap this replaces.
const _ENVELOPE_HEADROOM: () = {
    const ENCODING_FACTOR: usize = 4; // 3.6×, rounded up
    const QUEUE_RECORD_CAP: usize = 16 * 1024 * 1024;
    assert!(
        MAX_PAGE_BYTES * ENCODING_FACTOR < QUEUE_RECORD_CAP / 2,
        "the page byte budget encodes to more than half the queue record cap"
    );
};

/// Rows requested per internal scan round. Equal to the batched-read bound, so
/// one scan page is exactly one `multi_get` batch.
pub const SCAN_BATCH: u32 = 256;

/// Most scan rounds one request may perform. Bounds the work a page can do when
/// an index holds many rows whose bodies are missing or undecodable.
pub const MAX_SCAN_ROUNDS: usize = 32;

/// Longest cursor accepted, in hex characters. Index keys are a few hundred bytes;
/// this refuses a padded cursor before decoding it.
pub const MAX_CURSOR_HEX: usize = 1_024;

/// Bodies read before the page knows how big its events are.
///
/// A scan page is cheap (index values are empty), event bodies are not: they can
/// be 240 KiB each. Reading them in adaptively sized sub-batches — this many
/// first, then as many as the *observed* average says will fit — keeps a bucket of
/// large events from pulling tens of megabytes to fill a 2 MiB page, while a
/// bucket of ordinary chat messages still finishes in two reads.
pub const FIRST_BODY_BATCH: usize = 32;

/// Bytes charged per event for the JSON array separator around it.
const ELEMENT_OVERHEAD: usize = 1;

/// One page of a historical read.
///
/// Invariant: `complete == next_cursor.is_none()`. A caller pages until
/// `complete`, and never has to guess whether an empty page means "done" or
/// "keep going".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Page {
    /// Newest first (except for the author index without a kind, which is
    /// kind-major — see [`keys::author_prefix`]).
    pub events: Vec<Event>,
    /// Opaque resume position: pass it back as `cursor` for the next page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Whether the index was walked to its end.
    pub complete: bool,
}

impl Page {
    /// An empty, finished page — the answer for an index with nothing in it.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            events: Vec::new(),
            next_cursor: None,
            complete: true,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// The dual budget that ends a page: a count and a byte total.
#[derive(Debug, Clone, Copy)]
pub struct PageBudget {
    max_events: usize,
    max_bytes: usize,
    events: usize,
    bytes: usize,
}

impl PageBudget {
    /// A budget for `limit` events, silently clamped to [`MAX_PAGE_EVENTS`], with
    /// the standard [`MAX_PAGE_BYTES`] ceiling.
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self {
            max_events: if limit > MAX_PAGE_EVENTS {
                MAX_PAGE_EVENTS
            } else {
                limit
            },
            max_bytes: MAX_PAGE_BYTES,
            events: 0,
            bytes: 0,
        }
    }

    /// A budget with an explicit byte ceiling (also clamped to
    /// [`MAX_PAGE_BYTES`]), for callers whose envelope is tighter than the api
    /// reply's.
    #[must_use]
    pub const fn with_max_bytes(limit: usize, max_bytes: usize) -> Self {
        let mut budget = Self::new(limit);
        budget.max_bytes = if max_bytes > MAX_PAGE_BYTES {
            MAX_PAGE_BYTES
        } else {
            max_bytes
        };
        budget
    }

    /// Charge `encoded_len` bytes for one event.
    ///
    /// Returns `false` when the event does not fit — the caller must stop and
    /// return a cursor instead of forcing the event in. The one exception is an
    /// empty page: the first event is always admitted, however large, because a
    /// page that can never contain its next event would stall paging forever.
    pub fn try_admit(&mut self, encoded_len: usize) -> bool {
        if self.events >= self.max_events {
            return false;
        }
        let projected = self
            .bytes
            .saturating_add(encoded_len)
            .saturating_add(ELEMENT_OVERHEAD);
        if projected > self.max_bytes && self.events > 0 {
            return false;
        }
        self.bytes = projected;
        self.events += 1;
        true
    }

    /// Events admitted so far.
    #[must_use]
    pub const fn events(&self) -> usize {
        self.events
    }

    /// Bytes charged so far.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    /// How many more events could still be admitted by count alone.
    #[must_use]
    pub const fn remaining_events(&self) -> usize {
        self.max_events.saturating_sub(self.events)
    }

    /// How many more bytes could still be admitted.
    #[must_use]
    pub const fn remaining_bytes(&self) -> usize {
        self.max_bytes.saturating_sub(self.bytes)
    }

    /// Whether either dimension is spent.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.events >= self.max_events || self.bytes >= self.max_bytes
    }
}

/// Most kinds one caller-supplied allowlist may name.
///
/// [`KindSelect::admits`] is a linear scan, run once per decoded row, so the
/// list has to stay short enough that scanning it is cheaper than the row it
/// rejects. buzz's whole channel-event vocabulary is 14 kinds wide
/// ([`crate::relay::ws::MAX_KIND_INDEXES`] is sized off the same fact), so 32 is room
/// to spare and still a bound a hostile caller cannot widen.
pub const MAX_SELECT_KINDS: usize = 32;

/// Which event kinds a read admits.
///
/// The index a read walks is **kind-agnostic**: `ts/{community}/{channel}/…`
/// holds every event tagged into that channel, so a timeline read pays for the
/// channel's whole vocabulary whether or not the caller can use it. On a real
/// room that is not a rounding error — a 580-member channel's NIP-29 roster
/// projection (kind 39002) is a single ~49 KB event that rides along on *every*
/// page, and at the measured ~1 ms per KB of reply it costs more than the
/// messages the caller asked for.
///
/// This is the read's way to say what it wants. Two rules, both applied to the
/// decoded event before the [`PageBudget`] is charged, so a rejected row costs
/// its store read but **not** its reply bytes and does not consume page budget:
///
/// * `deny` — kinds this read never admits, whatever else it says.
/// * `allow` — when present, the only kinds this read admits.
///
/// [`KindSelect::all`] is the default and admits everything, which is the
/// NIP-01 REQ contract: a `{"#h": [channel]}` filter must be answered from the
/// complete index, so the subscription path must never narrow it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindSelect {
    /// `None` admits any kind; `Some` is an allowlist.
    allow: Option<Vec<Kind>>,
    /// Applied first, and wins over `allow`.
    deny: Vec<Kind>,
}

impl Default for KindSelect {
    fn default() -> Self {
        Self::all()
    }
}

impl KindSelect {
    /// Admit every kind the index holds — the NIP-01 REQ contract.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            allow: None,
            deny: Vec::new(),
        }
    }

    /// Admit only these kinds, truncated to [`MAX_SELECT_KINDS`].
    ///
    /// An empty list admits nothing, which is the honest reading of "only these
    /// kinds" for an empty set; a caller that meant "everything" asks for
    /// [`KindSelect::all`].
    #[must_use]
    pub fn only(kinds: impl IntoIterator<Item = Kind>) -> Self {
        let mut allow: Vec<Kind> = Vec::new();
        for k in kinds {
            if allow.len() >= MAX_SELECT_KINDS {
                break;
            }
            if !allow.contains(&k) {
                allow.push(k);
            }
        }
        Self {
            allow: Some(allow),
            deny: Vec::new(),
        }
    }

    /// Admit every kind except these, truncated to [`MAX_SELECT_KINDS`].
    #[must_use]
    pub fn without(kinds: impl IntoIterator<Item = Kind>) -> Self {
        let mut deny: Vec<Kind> = Vec::new();
        for k in kinds {
            if deny.len() >= MAX_SELECT_KINDS {
                break;
            }
            if !deny.contains(&k) {
                deny.push(k);
            }
        }
        Self { allow: None, deny }
    }

    /// The selector for a **rendered timeline row**: everything except the
    /// relay-signed NIP-29 group projections (39000 metadata, 39001 admins,
    /// 39002 members).
    ///
    /// Those three are *channel state*, not timeline rows. A client builds its
    /// channel list and roster from them by their own addressable identity —
    /// `{"kinds": [39002], "#d": [channel]}` — never by reading them out of a
    /// page of messages, and every consumer this relay has drops them on
    /// arrival. But they carry the channel's `h` tag, so ingest files them in
    /// the channel timeline index alongside the messages, and a roster grows
    /// one `p` tag per member: on `general` (580 members) that is 49,676 bytes,
    /// **88% of a 20-row window reply**, re-sent on every page.
    ///
    /// This is deliberately narrow. It is a statement about what a *timeline
    /// row* is, which the relay owns, and NOT a guess at which message kinds
    /// some particular UI renders — that is the caller's to say, with
    /// [`KindSelect::only`]. Group projections stay fully readable through the
    /// index they belong to; this only stops them from riding along in a page
    /// of messages nobody reads them from.
    #[must_use]
    pub fn timeline_rows() -> Self {
        Self::without([
            kind::KIND_NIP29_GROUP_METADATA,
            kind::KIND_NIP29_GROUP_ADMINS,
            kind::KIND_NIP29_GROUP_MEMBERS,
        ])
    }

    /// Whether a decoded event of this kind belongs in the page.
    #[must_use]
    pub fn admits(&self, kind: Kind) -> bool {
        if self.deny.contains(&kind) {
            return false;
        }
        match self.allow.as_deref() {
            None => true,
            Some(allow) => allow.contains(&kind),
        }
    }

    /// Whether this selector admits everything — the fast path, and what the
    /// REQ/subscription reads must keep using.
    #[must_use]
    pub fn is_unrestricted(&self) -> bool {
        self.allow.is_none() && self.deny.is_empty()
    }
}

/// One scanned index row and the event body it points at.
///
/// `body` is `None` when the `evt/` record is not there — see the module docs for
/// why that is a skip and not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    pub index_key: Vec<u8>,
    pub id: EventId,
    pub body: Option<Vec<u8>>,
}

impl IndexRow {
    #[must_use]
    pub const fn new(index_key: Vec<u8>, id: EventId, body: Option<Vec<u8>>) -> Self {
        Self {
            index_key,
            id,
            body,
        }
    }
}

/// Folds index rows into a page under a [`PageBudget`].
///
/// Pure — no I/O — so the budget, the skip rule, and the cursor advance are
/// testable without a Store.
#[derive(Debug)]
pub struct PageAcc {
    budget: PageBudget,
    select: KindSelect,
    events: Vec<Event>,
    /// Last index key folded in (admitted *or* deliberately skipped). This, not
    /// the last row scanned, is what a stopped page resumes after.
    consumed: Option<Vec<u8>>,
    stopped: bool,
    skipped: usize,
    filtered: usize,
}

impl PageAcc {
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self::with_budget(PageBudget::new(limit))
    }

    #[must_use]
    pub fn with_budget(budget: PageBudget) -> Self {
        Self::with_select(budget, KindSelect::all())
    }

    #[must_use]
    pub fn with_select(budget: PageBudget, select: KindSelect) -> Self {
        Self {
            budget,
            select,
            events: Vec::new(),
            consumed: None,
            stopped: false,
            skipped: 0,
            filtered: 0,
        }
    }

    /// Fold one batch, in order. Returns `false` once the budget refused a row:
    /// the caller must stop scanning, and the refused row is *not* consumed, so
    /// the next page starts exactly at it.
    pub fn fold(&mut self, rows: &[IndexRow]) -> bool {
        for row in rows {
            let Some(bytes) = row.body.as_deref() else {
                // Body not there: ingest writes it first, so the saga is still in
                // flight (or the event was redacted). Never surface a row without
                // its event.
                self.skip(&row.index_key);
                continue;
            };
            let event: Event = match store::decode_doc(&row.index_key, bytes) {
                Ok(event) => event,
                Err(e) => {
                    tracing::warn!("skipping undecodable event body: {e}");
                    self.skip(&row.index_key);
                    continue;
                }
            };
            if event.id != row.id {
                // The body under `evt/{id}` claims another id: an integrity fault,
                // and serving it would let one row impersonate another event.
                tracing::warn!(
                    "skipping event body whose id disagrees with its key: {}",
                    to_hex(&row.id)
                );
                self.skip(&row.index_key);
                continue;
            }
            if !self.select.admits(event.kind) {
                // Not a kind this read wants. Charged to `filtered`, never to
                // `skipped`: a skip means a row whose body should have been
                // there and was not, which is a saga-health signal, and folding
                // a deliberate filter into it would hide that.
                self.filter_out(&row.index_key);
                continue;
            }
            if !self.budget.try_admit(bytes.len()) {
                self.stopped = true;
                return false;
            }
            self.events.push(event);
            self.consumed = Some(row.index_key.clone());
        }
        true
    }

    fn skip(&mut self, index_key: &[u8]) {
        self.skipped += 1;
        self.consumed = Some(index_key.to_vec());
    }

    /// Walk past a row the [`KindSelect`] rejected. The cursor still advances:
    /// a filtered row is *consumed*, or the next page would start at it again
    /// and re-reject it forever.
    fn filter_out(&mut self, index_key: &[u8]) {
        self.filtered += 1;
        self.consumed = Some(index_key.to_vec());
    }

    /// Whether the budget ended this page.
    #[must_use]
    pub const fn stopped(&self) -> bool {
        self.stopped
    }

    /// Index rows walked past without an event (missing, undecodable, or
    /// mismatched bodies). Worth metering: a persistently non-zero count means
    /// sagas are not completing.
    #[must_use]
    pub const fn skipped(&self) -> usize {
        self.skipped
    }

    /// Rows walked past because their kind was not selected. Unlike
    /// [`PageAcc::skipped`] this is healthy: it is the read declining to spend
    /// reply bytes on an event the caller cannot use.
    #[must_use]
    pub const fn filtered(&self) -> usize {
        self.filtered
    }

    /// The budget as it now stands.
    #[must_use]
    pub const fn budget(&self) -> PageBudget {
        self.budget
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Turn a folded accumulator into the reply page.
///
/// `exhausted` means the scan reached the end of the prefix; `last_scanned` is
/// the last index key the scan returned, which is where an unfinished-but-not-
/// budget-stopped page resumes (past dead rows rather than at them).
///
/// Upholds `complete == next_cursor.is_none()`: if there is nowhere to resume
/// from, the page is declared complete rather than handed to a client that would
/// then poll the same position forever.
#[must_use]
pub fn finish_page(acc: PageAcc, exhausted: bool, last_scanned: Option<&[u8]>) -> Page {
    let resume = if exhausted && !acc.stopped {
        None
    } else if acc.stopped {
        acc.consumed.clone()
    } else {
        last_scanned.map(<[u8]>::to_vec)
    };
    let next_cursor = resume.as_deref().map(encode_cursor);
    Page {
        events: acc.events,
        complete: next_cursor.is_none(),
        next_cursor,
    }
}

/// How many event bodies to read next, given what the page has already admitted.
///
/// Never zero, never more than the page could still hold by count, and after the
/// first read never more than the remaining byte budget can plausibly absorb at
/// the average size seen so far. The `+ 1` is deliberate: a batch must always
/// include the event that will *exceed* the budget, or the page could never
/// discover that it is full.
#[must_use]
pub fn body_batch(budget: PageBudget) -> usize {
    let by_count = budget.remaining_events().max(1);
    if budget.events() == 0 {
        return FIRST_BODY_BATCH.min(by_count);
    }
    let average = (budget.bytes() / budget.events()).max(1);
    let by_bytes = budget.remaining_bytes() / average + 1;
    by_bytes.clamp(1, by_count)
}

/// Encode an index key as an opaque, URL-safe cursor.
#[must_use]
pub fn encode_cursor(index_key: &[u8]) -> String {
    to_hex(index_key)
}

/// Decode a cursor and verify it belongs to `prefix`.
///
/// Three checks, all load-bearing: the length bound stops a padded cursor before
/// it is decoded; the prefix check keeps a cursor from one channel, kind, or
/// community out of another's scan; and the tail check requires a real index key,
/// so a cursor cannot be crafted to land between rows.
pub fn decode_cursor(cursor: &str, prefix: &[u8]) -> Result<Vec<u8>> {
    if cursor.len() > MAX_CURSOR_HEX {
        return Err(BuzzError::InvalidEvent("cursor is too long".into()));
    }
    let raw =
        from_hex(cursor).ok_or_else(|| BuzzError::InvalidEvent("cursor is not hex".into()))?;
    if !raw.starts_with(prefix) {
        return Err(BuzzError::InvalidEvent(
            "cursor does not belong to this query".into(),
        ));
    }
    if keys::parse_index_tail(&raw).is_none() {
        return Err(BuzzError::InvalidEvent("cursor is not an index key".into()));
    }
    Ok(raw)
}

/// One page of a channel's timeline, newest first. `channel = None` reads the
/// community-global timeline.
pub async fn timeline(
    store: &mut StoreClusterClient,
    community: &str,
    channel: Option<&str>,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Page> {
    timeline_selected(store, community, channel, cursor, limit, KindSelect::all()).await
}

/// [`timeline`], restricted to the kinds the caller can actually use.
///
/// Same index, same order, same cursor contract — the only difference is that a
/// row whose kind the selector rejects is walked past instead of being encoded
/// into the reply. Two consequences worth stating, because both are the point:
///
/// * The page budget buys *wanted* rows. `limit = 20` returns 20 usable events
///   rather than 20 index rows of which some are unusable.
/// * A rejected row still costs its `evt/{id}` read — the kind lives in the
///   body, and the timeline index key does not carry it — but it costs **no
///   reply bytes**, which is the expensive half by a wide margin (~1 ms per KB
///   of reply, measured, versus a batched point read).
///
/// [`timeline`] itself is unchanged and still admits everything, because the
/// NIP-01 subscription path answers `{"#h": [channel]}` from this same index
/// and must not start hiding rows a filter asked for.
pub async fn timeline_selected(
    store: &mut StoreClusterClient,
    community: &str,
    channel: Option<&str>,
    cursor: Option<&str>,
    limit: usize,
    select: KindSelect,
) -> Result<Page> {
    let prefix = keys::ts_prefix(community, channel);
    page(store, community, &prefix, cursor, limit, select).await
}

/// One page of every event of `kind`, newest first.
pub async fn by_kind(
    store: &mut StoreClusterClient,
    community: &str,
    kind: u32,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Page> {
    let prefix = keys::kind_prefix(community, kind);
    page(store, community, &prefix, cursor, limit, KindSelect::all()).await
}

/// One page of one author's events.
///
/// `kind = Some(k)` is newest-first within that kind. `kind = None` walks every
/// kind the author wrote in **kind-major** order (newest-first inside each kind),
/// because the author index puts the kind above the timestamp; a caller needing
/// one time-ordered stream across kinds must merge per-kind pages.
pub async fn by_author(
    store: &mut StoreClusterClient,
    community: &str,
    author: &PubKey,
    kind: Option<u32>,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Page> {
    let prefix = keys::author_prefix(community, author, kind);
    page(store, community, &prefix, cursor, limit, KindSelect::all()).await
}

/// One page of events carrying a `p` tag for `who` (hex pubkey), newest first.
pub async fn by_mention(
    store: &mut StoreClusterClient,
    community: &str,
    who: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Page> {
    let prefix = keys::mention_prefix(community, who);
    page(store, community, &prefix, cursor, limit, KindSelect::all()).await
}

/// One page of events referencing `target` via an `e` tag — replies, reactions,
/// deletions — newest first.
pub async fn by_etag(
    store: &mut StoreClusterClient,
    community: &str,
    target: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Page> {
    let prefix = keys::etag_prefix(community, target);
    page(store, community, &prefix, cursor, limit, KindSelect::all()).await
}

/// Read one event by id.
///
/// A body whose own id disagrees with its key is an integrity fault and is
/// reported, not returned: on a point read there is no other row to fall back to,
/// and quietly serving it would let one record answer for another.
pub async fn event_by_id(
    store: &mut StoreClusterClient,
    community: &str,
    id: &EventId,
) -> Result<Option<Event>> {
    let key = keys::evt(community, id);
    match store::get_doc::<Event>(store, &key).await? {
        Some(event) if &event.id == id => Ok(Some(event)),
        Some(_) => Err(BuzzError::Internal(format!(
            "stored event body does not match its key: {}",
            to_hex(id)
        ))),
        None => Ok(None),
    }
}

/// Read many events by id in one batched pass, chunked to the platform's
/// 256-key bound.
///
/// Order follows `ids`. Missing, undecodable, and mismatched bodies are dropped,
/// so the result may be shorter than the input — callers that need to know which
/// ids resolved should compare by id, not by position.
pub async fn events_by_ids(
    store: &mut StoreClusterClient,
    community: &str,
    ids: &[EventId],
) -> Result<Vec<Event>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let body_keys: Vec<Vec<u8>> = ids.iter().map(|id| keys::evt(community, id)).collect();
    let bodies = store::multi_get_raw(store, &body_keys).await?;
    let mut out = Vec::with_capacity(ids.len());
    for ((id, key), body) in ids.iter().zip(body_keys.iter()).zip(bodies.iter()) {
        let Some(bytes) = body.as_deref() else {
            continue;
        };
        match store::decode_doc::<Event>(key, bytes) {
            Ok(event) if &event.id == id => out.push(event),
            Ok(_) => tracing::warn!("event body disagrees with its key: {}", to_hex(id)),
            Err(e) => tracing::warn!("undecodable event body: {e}"),
        }
    }
    Ok(out)
}

/// The pager behind every read path above.
///
/// Loops `scan_prefix` → `multi_get` → fold until the budget is spent, the prefix
/// is exhausted, or [`MAX_SCAN_ROUNDS`] rounds have been walked. Two sizing rules
/// keep one request's work proportional to the page it can actually return: the
/// scan asks for only as many rows as the page could still hold (so `limit = 20`
/// is one small round trip, not a 1024-row scan), and bodies are read in
/// [`body_batch`]-sized sub-batches with a fold between them (so a page of large
/// events stops reading when it is full, not after a whole scan page of bodies).
async fn page(
    store: &mut StoreClusterClient,
    community: &str,
    prefix: &[u8],
    cursor: Option<&str>,
    limit: usize,
    select: KindSelect,
) -> Result<Page> {
    let mut acc = PageAcc::with_select(PageBudget::new(limit), select);
    if acc.budget().remaining_events() == 0 {
        // A zero limit asks for nothing. The cursor is still validated before
        // it is echoed, and the page never claims the index was walked.
        if let Some(c) = cursor {
            decode_cursor(c, prefix)?;
        }
        return Ok(Page {
            events: Vec::new(),
            next_cursor: cursor.map(str::to_string),
            complete: false,
        });
    }

    let mut start_after: Option<Vec<u8>> = match cursor {
        Some(c) => Some(decode_cursor(c, prefix)?),
        None => None,
    };
    let mut last_scanned: Option<Vec<u8>> = None;
    let mut exhausted = false;

    for _ in 0..MAX_SCAN_ROUNDS {
        let want = u32::try_from(acc.budget().remaining_events())
            .unwrap_or(SCAN_BATCH)
            .min(SCAN_BATCH);
        let (rows, more) = store::scan(store, prefix, start_after.as_deref(), want).await?;
        if rows.is_empty() {
            // `more` is over-approximate, so an empty page is the real end.
            exhausted = true;
            break;
        }
        if let Some((key, _, _)) = rows.last() {
            last_scanned = Some(key.clone());
        }

        // Keys first. A row that is not one of our index keys is ignored rather
        // than trusted: the scan is prefix-bounded, but a malformed row must not
        // become a lookup for an event id we invented.
        let mut positions: Vec<(Vec<u8>, EventId)> = Vec::with_capacity(rows.len());
        for (key, _value, _version) in &rows {
            match keys::parse_index_tail(key) {
                Some((_created_at, id)) => positions.push((key.clone(), id)),
                None => tracing::warn!(
                    "ignoring unparsable index row: {}",
                    to_hex(key.get(..48).unwrap_or(key))
                ),
            }
        }

        // Bodies in adaptive sub-batches, folded between reads, so a page stops
        // *reading* as soon as it is full rather than after fetching a whole
        // scan page of possibly-large events.
        let mut taken = 0;
        let mut refused = false;
        while let Some(slice) = positions.get(taken..) {
            if slice.is_empty() {
                break;
            }
            let take = body_batch(acc.budget()).min(slice.len());
            let Some(batch) = slice.get(..take) else {
                break;
            };
            let body_keys: Vec<Vec<u8>> = batch
                .iter()
                .map(|(_key, id)| keys::evt(community, id))
                .collect();
            let bodies = store::multi_get_raw(store, &body_keys).await?;
            let folded: Vec<IndexRow> = batch
                .iter()
                .cloned()
                .zip(bodies)
                .map(|((index_key, id), body)| IndexRow::new(index_key, id, body))
                .collect();
            taken += take;
            if !acc.fold(&folded) {
                refused = true;
                break;
            }
        }
        if refused {
            break;
        }
        if !more {
            exhausted = true;
            break;
        }
        if acc.budget().is_full() {
            // Spent, by count or by bytes, with rows possibly left: return a
            // cursor rather than spend another round trip proving the index is
            // exhausted. Every row of this batch was consumed (the fold did not
            // refuse one), so `last_scanned` is the correct resume point.
            break;
        }
        start_after = last_scanned.clone();
    }

    Ok(finish_page(acc, exhausted, last_scanned.as_deref()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use ferrite_app::serde_json;

    const COMMUNITY: &str = "main";

    fn event(id_byte: u8, created_at: u64, content: &str) -> Event {
        Event {
            id: [id_byte; 32],
            pubkey: [1u8; 32],
            created_at,
            kind: 1111,
            tags: Vec::new(),
            content: content.to_string(),
            sig: [0u8; 64],
        }
    }

    fn body(event: &Event) -> Vec<u8> {
        serde_json::to_vec(event).unwrap()
    }

    /// A NIP-29 member roster shaped like the live one: one `p` tag per member,
    /// which is what makes it the biggest event in a real channel's timeline.
    /// `general` on the dev relay carries 580 of them at 49,676 bytes.
    fn roster(id_byte: u8, created_at: u64, members: usize) -> Event {
        let mut tags = vec![
            vec!["d".to_string(), "general".to_string()],
            vec!["h".to_string(), "general".to_string()],
        ];
        for i in 0..members {
            tags.push(vec![
                "p".to_string(),
                // A distinct 64-hex pubkey per member, as on the wire.
                format!("{i:064x}"),
                String::new(),
                "member".to_string(),
            ]);
        }
        Event {
            kind: kind::KIND_NIP29_GROUP_MEMBERS,
            tags,
            ..event(id_byte, created_at, "")
        }
    }

    /// One index row whose body is present and well-formed.
    fn row(event: &Event) -> IndexRow {
        IndexRow::new(
            keys::ts_index(COMMUNITY, Some("c"), event.created_at, &event.id),
            event.id,
            Some(body(event)),
        )
    }

    #[test]
    fn budget_clamps_the_requested_limit() {
        assert_eq!(PageBudget::new(10).remaining_events(), 10);
        assert_eq!(
            PageBudget::new(MAX_PAGE_EVENTS * 4).remaining_events(),
            MAX_PAGE_EVENTS
        );
        assert_eq!(PageBudget::new(0).remaining_events(), 0);
        assert!(PageBudget::new(0).is_full());
        // A caller cannot widen the byte ceiling past the reply bound.
        let wide = PageBudget::with_max_bytes(10, MAX_PAGE_BYTES * 8);
        assert!(!wide.is_full());
        let mut wide = wide;
        assert!(wide.try_admit(MAX_PAGE_BYTES - 1));
        assert!(!wide.try_admit(4));
    }

    #[test]
    fn budget_stops_on_event_count() {
        let mut budget = PageBudget::new(3);
        for _ in 0..3 {
            assert!(budget.try_admit(10));
        }
        assert!(!budget.try_admit(10));
        assert_eq!(budget.events(), 3);
        assert!(budget.is_full());
        assert_eq!(budget.remaining_events(), 0);
    }

    #[test]
    fn the_byte_budget_stops_a_page_before_the_event_count() {
        // Big events: bytes bind long before 500 events do. This is the case that
        // decides whether a reply fits in the gateway's ~4 MiB envelope.
        let mut budget = PageBudget::new(MAX_PAGE_EVENTS);
        let big = 200 * 1024;
        let mut admitted = 0;
        while budget.try_admit(big) {
            admitted += 1;
        }
        assert_eq!(admitted, MAX_PAGE_BYTES / (big + ELEMENT_OVERHEAD));
        assert!(
            admitted < MAX_PAGE_EVENTS,
            "count must not be the binding cap"
        );
        assert!(budget.bytes() <= MAX_PAGE_BYTES);
        assert!(
            budget.remaining_events() > 0,
            "stopped on bytes, not on count"
        );
    }

    #[test]
    fn the_first_event_is_admitted_however_large() {
        // Otherwise one oversized event would stall paging at its row forever.
        let mut budget = PageBudget::new(10);
        assert!(budget.try_admit(MAX_PAGE_BYTES * 2));
        assert_eq!(budget.events(), 1);
        assert!(!budget.try_admit(1));
        assert!(budget.is_full());
    }

    #[test]
    fn a_tight_byte_budget_admits_exactly_what_fits() {
        let mut budget = PageBudget::with_max_bytes(100, 1_000);
        assert!(budget.try_admit(400));
        assert!(budget.try_admit(400));
        assert!(!budget.try_admit(400));
        assert_eq!(budget.events(), 2);
        assert_eq!(budget.bytes(), 802);
    }

    #[test]
    fn fold_collects_events_in_scan_order() {
        let a = event(1, 3_000, "newest");
        let b = event(2, 2_000, "middle");
        let c = event(3, 1_000, "oldest");
        let mut acc = PageAcc::new(10);
        assert!(acc.fold(&[row(&a), row(&b), row(&c)]));
        assert!(!acc.stopped());
        assert_eq!(acc.skipped(), 0);
        assert_eq!(acc.len(), 3);
        let page = finish_page(acc, true, None);
        assert_eq!(
            page.events.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![a.id, b.id, c.id]
        );
        assert!(page.complete);
        assert_eq!(page.next_cursor, None);
    }

    #[test]
    fn an_index_row_without_a_body_is_skipped_never_surfaced() {
        let visible = event(1, 3_000, "visible");
        let in_flight = event(2, 2_000, "not yet written");
        let mut missing = row(&in_flight);
        missing.body = None;

        let mut acc = PageAcc::new(10);
        assert!(acc.fold(&[row(&visible), missing.clone()]));
        assert_eq!(acc.len(), 1);
        assert_eq!(acc.skipped(), 1);
        assert_eq!(acc.events[0].id, visible.id);

        // The cursor advanced *past* the dead row: a pager must make progress or
        // it re-reads the same row forever.
        let page = finish_page(acc, false, Some(missing.index_key.as_slice()));
        assert_eq!(
            page.next_cursor,
            Some(encode_cursor(&missing.index_key)),
            "cursor must clear the skipped row"
        );
        assert!(!page.complete);
    }

    #[test]
    fn an_undecodable_or_mismatched_body_is_skipped() {
        let good = event(1, 3_000, "fine");
        let mut garbage = row(&event(2, 2_000, "garbage"));
        garbage.body = Some(b"{not json".to_vec());
        // A body that claims a different id than the key it sits under.
        let mut impostor = row(&event(3, 1_000, "impostor"));
        impostor.body = Some(body(&event(9, 1_000, "someone else")));

        let mut acc = PageAcc::new(10);
        assert!(acc.fold(&[row(&good), garbage, impostor]));
        assert_eq!(acc.len(), 1);
        assert_eq!(acc.skipped(), 2);
        assert_eq!(acc.events[0].id, good.id);
    }

    #[test]
    fn fold_stops_on_the_budget_without_consuming_the_refused_row() {
        let a = event(1, 3_000, "a");
        let b = event(2, 2_000, "b");
        let c = event(3, 1_000, "c");
        let mut acc = PageAcc::new(2);
        assert!(!acc.fold(&[row(&a), row(&b), row(&c)]));
        assert!(acc.stopped());
        assert_eq!(acc.len(), 2);

        // The cursor is the last *admitted* row, so the refused one is the first
        // row of the next page — no event is skipped by paging.
        let last_scanned = row(&c).index_key;
        let page = finish_page(acc, false, Some(last_scanned.as_slice()));
        assert_eq!(page.next_cursor, Some(encode_cursor(&row(&b).index_key)));
        assert!(!page.complete);
    }

    #[test]
    fn a_byte_stopped_page_resumes_at_the_refused_row() {
        let a = event(1, 3_000, &"x".repeat(600));
        let b = event(2, 2_000, &"y".repeat(600));
        let mut acc = PageAcc::with_budget(PageBudget::with_max_bytes(100, 700));
        assert!(!acc.fold(&[row(&a), row(&b)]));
        assert_eq!(
            acc.len(),
            1,
            "the second event did not fit in the byte budget"
        );
        let last_scanned = row(&b).index_key;
        let page = finish_page(acc, false, Some(last_scanned.as_slice()));
        assert_eq!(page.next_cursor, Some(encode_cursor(&row(&a).index_key)));
    }

    #[test]
    fn finish_page_upholds_the_complete_invariant() {
        // Exhausted and not stopped: complete, no cursor.
        let done = finish_page(PageAcc::new(5), true, Some(&b"ignored"[..]));
        assert!(done.complete && done.next_cursor.is_none());

        // Not exhausted, nothing scanned: still complete, because there is
        // nowhere to resume from and a cursorless incomplete page would strand
        // the caller.
        let nothing = finish_page(PageAcc::new(5), false, None);
        assert!(nothing.complete && nothing.next_cursor.is_none());

        // Not exhausted, rows scanned: resume after the last row scanned, which
        // is past any dead rows at the tail of the batch.
        let e = event(1, 1_000, "x");
        let mut acc = PageAcc::new(5);
        assert!(acc.fold(&[row(&e)]));
        let more = finish_page(acc, false, Some(&b"ts/main/c/0/last"[..]));
        assert!(!more.complete);
        assert_eq!(more.next_cursor, Some(encode_cursor(b"ts/main/c/0/last")));
    }

    #[test]
    fn empty_pages_are_complete() {
        let page = Page::empty();
        assert!(page.is_empty() && page.complete && page.next_cursor.is_none());
        assert_eq!(page.len(), 0);
    }

    #[test]
    fn the_default_selector_admits_everything() {
        let all = KindSelect::all();
        assert!(all.is_unrestricted());
        assert_eq!(all, KindSelect::default());
        for k in [0, 1, 7, 9, 9021, 39000, 39002, u32::MAX] {
            assert!(all.admits(k), "kind {k} was refused by an open selector");
        }
    }

    #[test]
    fn only_is_an_allowlist_and_without_is_a_denylist() {
        let only = KindSelect::only([9, 7]);
        assert!(only.admits(9) && only.admits(7));
        assert!(!only.admits(9021) && !only.admits(39002));
        assert!(!only.is_unrestricted());

        let without = KindSelect::without([39002]);
        assert!(!without.admits(39002));
        assert!(without.admits(9) && without.admits(9021));
        assert!(!without.is_unrestricted());

        // "Only nothing" admits nothing — the honest reading of an empty set.
        let none = KindSelect::only([]);
        assert!(!none.admits(9));
        assert!(!none.is_unrestricted());
    }

    #[test]
    fn a_selector_list_is_deduped_and_bounded() {
        // A hostile caller cannot make `admits` expensive: the list is capped,
        // and duplicates never lengthen it.
        let dupes = KindSelect::only([9, 9, 9, 9]);
        assert!(dupes.admits(9));
        let huge = KindSelect::only(0..1_000);
        assert!(huge.admits(0));
        assert!(
            !huge.admits(MAX_SELECT_KINDS as Kind + 1),
            "the allowlist was not truncated"
        );
        let huge_deny = KindSelect::without(0..1_000);
        assert!(!huge_deny.admits(0));
        assert!(huge_deny.admits(MAX_SELECT_KINDS as Kind + 1));
    }

    #[test]
    fn timeline_rows_refuses_exactly_the_group_projections() {
        let select = KindSelect::timeline_rows();
        // Cross-checked against kind.rs rather than restated, so the two cannot
        // drift apart: a projection added there is refused here automatically.
        for k in 0..40_100u32 {
            assert_eq!(
                select.admits(k),
                !kind::is_group_projection(k),
                "timeline_rows disagrees with kind::is_group_projection on {k}"
            );
        }
        // The three by name, so the test still says what it means.
        assert!(!select.admits(39000));
        assert!(!select.admits(39001));
        assert!(!select.admits(39002));
        // ...and every kind a reader actually renders survives.
        for k in [
            1, 5, 7, 9, 9005, 9021, 40002, 40003, 40008, 40099, 43001, 48100,
        ] {
            assert!(select.admits(k), "timeline_rows dropped kind {k}");
        }
    }

    #[test]
    fn a_filtered_row_costs_no_budget_and_is_not_a_skip() {
        let wanted = event(1, 3_000, "keep me");
        let projection = roster(2, 2_000, 4);
        let also_wanted = event(3, 1_000, "keep me too");

        let mut acc = PageAcc::with_select(PageBudget::new(10), KindSelect::timeline_rows());
        assert!(acc.fold(&[row(&wanted), row(&projection), row(&also_wanted)]));
        assert_eq!(acc.len(), 2);
        assert_eq!(acc.filtered(), 1);
        assert_eq!(
            acc.skipped(),
            0,
            "a deliberate filter must not read as a missing body"
        );
        // The rejected event's bytes were never charged to the page.
        assert!(acc.budget().bytes() < body(&projection).len());
        assert_eq!(
            acc.events.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![wanted.id, also_wanted.id]
        );
    }

    #[test]
    fn the_cursor_advances_past_a_filtered_row() {
        // Otherwise the next page starts at the rejected row and re-rejects it
        // forever — the same trap the missing-body skip avoids.
        let projection = roster(1, 3_000, 2);
        let mut acc = PageAcc::with_select(PageBudget::new(10), KindSelect::timeline_rows());
        assert!(acc.fold(&[row(&projection)]));
        assert!(acc.is_empty());
        let page = finish_page(acc, false, Some(row(&projection).index_key.as_slice()));
        assert_eq!(
            page.next_cursor,
            Some(encode_cursor(&row(&projection).index_key)),
            "cursor must clear the filtered row"
        );
        assert!(!page.complete);
    }

    #[test]
    fn the_page_budget_buys_wanted_rows_not_index_rows() {
        // Interleave junk with messages: a limit of 3 must return 3 MESSAGES,
        // not 3 index rows of which one is a roster.
        let rows = [
            row(&event(1, 9_000, "a")),
            row(&roster(2, 8_000, 3)),
            row(&event(3, 7_000, "b")),
            row(&roster(4, 6_000, 3)),
            row(&event(5, 5_000, "c")),
        ];
        let mut acc = PageAcc::with_select(PageBudget::new(3), KindSelect::timeline_rows());
        acc.fold(&rows);
        assert_eq!(acc.len(), 3);
        assert_eq!(acc.filtered(), 2);
    }

    #[test]
    fn an_unrestricted_read_still_returns_group_projections() {
        // The NIP-01 REQ contract: `{"#h": [channel]}` is answered from this
        // index, so the DEFAULT read must not start hiding rows. If this ever
        // fails, `timeline` narrowed and the subscription face went with it.
        let projection = roster(1, 3_000, 4);
        let message = event(2, 2_000, "hello");
        let mut acc = PageAcc::new(10);
        assert!(acc.fold(&[row(&projection), row(&message)]));
        assert_eq!(acc.len(), 2);
        assert_eq!(acc.filtered(), 0);
        assert_eq!(
            acc.events.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![projection.id, message.id]
        );
    }

    /// The measured claim, pinned as an assertion on real serialized bytes.
    ///
    /// Mirrors the live `general` channel: a 580-member roster plus a handful of
    /// chat messages, which is what a 20-row window page actually contains.
    /// Live, that reply is 55.7 KB of which 49,676 bytes is the roster.
    #[test]
    fn filtering_projections_cuts_the_reply_by_the_size_of_the_roster() {
        let roster_event = roster(200, 9_999, 580);
        let roster_bytes = body(&roster_event).len();
        // The fixture must actually be the size the live one is, or the test
        // proves nothing about the live payload.
        assert!(
            (45_000..55_000).contains(&roster_bytes),
            "roster fixture is {roster_bytes} B, not live-sized"
        );

        let messages: Vec<Event> = (0..19)
            .map(|i| event(i, 9_000 - u64::from(i), "a normal chat message"))
            .collect();
        let mut rows = vec![row(&roster_event)];
        rows.extend(messages.iter().map(row));

        let unrestricted = {
            let mut acc = PageAcc::new(20);
            acc.fold(&rows);
            finish_page(acc, true, None)
        };
        let filtered = {
            let mut acc = PageAcc::with_select(PageBudget::new(20), KindSelect::timeline_rows());
            acc.fold(&rows);
            finish_page(acc, true, None)
        };

        let before = serde_json::to_vec(&unrestricted).unwrap().len();
        let after = serde_json::to_vec(&filtered).unwrap().len();
        assert!(
            after * 8 < before,
            "expected an 8x+ cut, got {before} B -> {after} B"
        );
        assert_eq!(
            before - after,
            roster_bytes + ELEMENT_OVERHEAD,
            "the cut is exactly the roster, plus the comma that separated it"
        );

        // ...and NOTHING a reader needs was lost: every message survives whole,
        // signature and tags included. This is the half that matters — a byte
        // win that drops a field is not a win.
        assert_eq!(filtered.events.len(), messages.len());
        for (got, want) in filtered.events.iter().zip(messages.iter()) {
            assert_eq!(got, want, "a surviving event was altered, not just kept");
            assert_eq!(got.sig, want.sig);
            assert_eq!(got.tags, want.tags);
        }
    }

    #[test]
    fn cursors_round_trip() {
        let prefix = keys::ts_prefix(COMMUNITY, Some("c"));
        let key = keys::ts_index(COMMUNITY, Some("c"), 1_700_000_000, &[7u8; 32]);
        let cursor = encode_cursor(&key);
        assert!(cursor.bytes().all(|b| b.is_ascii_hexdigit()), "{cursor}");
        assert_eq!(decode_cursor(&cursor, &prefix).unwrap(), key);
        // The cursor is opaque but recoverable, which is what paging relies on.
        assert_eq!(
            keys::parse_ts_index(&decode_cursor(&cursor, &prefix).unwrap()),
            Some((1_700_000_000, [7u8; 32]))
        );
    }

    #[test]
    fn cursors_from_another_bucket_are_refused() {
        let mine = keys::ts_prefix(COMMUNITY, Some("mine"));
        let theirs = keys::ts_index(COMMUNITY, Some("theirs"), 1_000, &[7u8; 32]);
        assert!(decode_cursor(&encode_cursor(&theirs), &mine).is_err());

        // Another community, another family, and the global bucket are all out.
        let other_community = keys::ts_index("other", Some("mine"), 1_000, &[7u8; 32]);
        assert!(decode_cursor(&encode_cursor(&other_community), &mine).is_err());
        let other_family = keys::kind_index(COMMUNITY, 1, 1_000, &[7u8; 32]);
        assert!(decode_cursor(&encode_cursor(&other_family), &mine).is_err());
        let global = keys::ts_index(COMMUNITY, None, 1_000, &[7u8; 32]);
        assert!(decode_cursor(&encode_cursor(&global), &mine).is_err());
    }

    #[test]
    fn malformed_cursors_are_refused_without_panicking() {
        let prefix = keys::ts_prefix(COMMUNITY, Some("c"));
        let valid = encode_cursor(&keys::ts_index(COMMUNITY, Some("c"), 1_000, &[7u8; 32]));
        let truncated = valid[..valid.len() - 2].to_string();
        let extended = format!("{valid}00");
        let padded = "61".repeat(MAX_CURSOR_HEX);
        for bad in [
            "",
            "zz",
            "abc",
            truncated.as_str(),
            extended.as_str(),
            padded.as_str(),
        ] {
            assert!(
                decode_cursor(bad, &prefix).is_err(),
                "accepted cursor {bad:?}"
            );
        }
        // A cursor inside the prefix but not a real index key is refused too, so
        // a caller cannot aim between rows.
        let mut half = prefix.clone();
        half.extend_from_slice(b"ffffffffffffffff/short");
        assert!(decode_cursor(&encode_cursor(&half), &prefix).is_err());
        // ... and the valid one still works, so the checks are not vacuous.
        assert!(decode_cursor(&valid, &prefix).is_ok());
    }

    #[test]
    fn the_first_body_read_is_bounded_then_sized_from_what_it_saw() {
        // Nothing known yet: read a bounded probe, not a whole scan page, so a
        // bucket of 240 KiB events cannot pull 60 MB to fill a 2 MiB page.
        let cold = PageBudget::new(MAX_PAGE_EVENTS);
        assert_eq!(body_batch(cold), FIRST_BODY_BATCH);
        // A small page never reads more rows than it could keep.
        assert_eq!(body_batch(PageBudget::new(5)), 5);

        // Ordinary chat messages: the next read is wide (count-bound).
        let mut small = PageBudget::new(MAX_PAGE_EVENTS);
        for _ in 0..FIRST_BODY_BATCH {
            assert!(small.try_admit(1_024));
        }
        assert_eq!(body_batch(small), small.remaining_events());

        // 200 KiB events: the next read is narrow (byte-bound), and still large
        // enough to include the event that will overflow the budget.
        let mut large = PageBudget::new(MAX_PAGE_EVENTS);
        assert!(large.try_admit(200 * 1024));
        let next = body_batch(large);
        assert!(next > 1 && next < FIRST_BODY_BATCH, "{next}");
        assert_eq!(next, large.remaining_bytes() / (200 * 1024 + 1) + 1);
    }

    #[test]
    fn body_batch_is_never_zero() {
        // A zero-size read would spin the pager without making progress.
        let mut spent = PageBudget::new(1);
        assert!(spent.try_admit(MAX_PAGE_BYTES * 4));
        assert!(spent.is_full());
        assert_eq!(spent.remaining_events(), 0);
        assert_eq!(spent.remaining_bytes(), 0);
        assert_eq!(body_batch(spent), 1);
        assert_eq!(body_batch(PageBudget::new(0)), 1);
    }

    #[test]
    fn page_bounds_stay_inside_the_reply_envelope() {
        // The page/envelope relationship is checked at COMPILE time (see
        // ENVELOPE_HEADROOM below) rather than here: every operand is a constant, so
        // a runtime `assert!` folds to `assert!(true)` and guards nothing. What is
        // left to check here are the relationships involving the platform's own
        // limits, which this module reads rather than defines.
        assert_eq!(MAX_PAGE_EVENTS, 500);
        assert_eq!(SCAN_BATCH as usize, store::MAX_MULTI_GET);
        // A scan round must survive the clamp `store::scan` applies to it.
        assert_eq!(SCAN_BATCH.clamp(1, store::MAX_SCAN_PAGE), SCAN_BATCH);
    }

    //
    // These exist because of a wrong diagnosis worth not repeating. A live
    // measurement recorded `{kinds:[39000],limit:500}` at 3,891 ms and
    // `{kinds:[39002],limit:500}` at 1,209 ms for the SAME event count, and
    // concluded the 39000 path must resolve to a different (or scanned) index.
    // It does not: both are [`by_kind`], both walk `keys::kind_prefix`, and the
    // asymmetry was a measurement artefact — `channelsFromRelay` issues the two
    // REQs concurrently on one socket, WebSocket reads do not overlap, and
    // whichever REQ lands second pays its own cost plus the first's. Reversing
    // the send order moves the "expensive" kind with it (measured live
    // 2026-07-29: 1,068/2,124 ms one way, 2,126/1,078 ms reversed).
    //
    // The tests below pin the two facts that refute the byte/index hypothesis
    // without needing a relay, so the next reader does not have to re-measure.

    /// Faithful mirror of the body-read loop inside [`page`], which is what
    /// decides how many store round trips a page makes: it calls the real
    /// [`body_batch`] and the real [`PageAcc::fold`], and only the loop skeleton
    /// is restated here. Returns the sub-batch sizes in order — one store
    /// `multi_get` each.
    fn body_read_batches(
        rows: &[IndexRow],
        budget: PageBudget,
        select: KindSelect,
    ) -> (Vec<usize>, PageAcc) {
        let mut acc = PageAcc::with_select(budget, select);
        let mut batches = Vec::new();
        let mut taken = 0;
        while let Some(slice) = rows.get(taken..) {
            if slice.is_empty() {
                break;
            }
            let take = body_batch(acc.budget()).min(slice.len());
            let Some(batch) = slice.get(..take) else {
                break;
            };
            batches.push(take);
            taken += take;
            if !acc.fold(batch) {
                break;
            }
        }
        (batches, acc)
    }

    /// A kind-index row for `event`, as [`by_kind`] would scan it.
    fn kind_row(event: &Event) -> IndexRow {
        IndexRow::new(
            keys::kind_index(COMMUNITY, event.kind, event.created_at, &event.id),
            event.id,
            Some(body(event)),
        )
    }

    /// A 39000 metadata projection, sized like the live one: 66,207 B over 134
    /// events is 494 B each, and this fixture lands within a few bytes of that
    /// (an `Event` serializes its id/pubkey/sig as hex, so the floor is ~340 B
    /// before a single tag).
    fn metadata(id_byte: u8, created_at: u64, channel: &str) -> Event {
        Event {
            kind: kind::KIND_NIP29_GROUP_METADATA,
            tags: vec![
                vec!["d".to_string(), channel.to_string()],
                vec!["h".to_string(), channel.to_string()],
                vec!["name".to_string(), channel.to_string()],
                vec!["about".to_string(), "x".repeat(80)],
                vec!["t".to_string(), "stream".to_string()],
            ],
            ..event(id_byte, created_at, "")
        }
    }

    #[test]
    fn a_pages_store_round_trips_track_row_count_not_byte_volume() {
        // The live shapes, to scale: 134 metadata events at 66 KB against 134
        // rosters at 178 KB — 2.7x the bytes for the same count, and the fixture
        // asserts that ratio so it cannot silently stop being the live one.
        const ROWS: usize = 134;
        let meta: Vec<Event> = (0..ROWS)
            .map(|i| metadata(i as u8, 9_000 - i as u64, &format!("room-{i}")))
            .collect();
        // One big room plus many small ones, which is what a real workspace is.
        let rosters: Vec<Event> = (0..ROWS)
            .map(|i| {
                let members = if i == 3 { 580 } else { 6 };
                roster(i as u8, 9_000 - i as u64, members)
            })
            .collect();

        let meta_rows: Vec<IndexRow> = meta.iter().map(kind_row).collect();
        let roster_rows: Vec<IndexRow> = rosters.iter().map(kind_row).collect();
        let meta_bytes: usize = meta.iter().map(|e| body(e).len()).sum();
        let roster_bytes: usize = rosters.iter().map(|e| body(e).len()).sum();
        // The live pair was 66,207 B against 177,627 B for 134 events each — a
        // 2.68x byte ratio at an identical count. Asserted so the fixture cannot
        // quietly stop being the shape the claim is about.
        assert!(
            roster_bytes > meta_bytes * 2,
            "fixture is not live-shaped: {meta_bytes} B vs {roster_bytes} B"
        );

        // Same limit, same row count, 2.7x the bytes: the SAME number of store
        // round trips and the same number of events. This is the assertion that
        // refutes "the roster read is cheaper/dearer because of its bytes".
        let (meta_batches, meta_acc) = body_read_batches(
            &meta_rows,
            PageBudget::new(MAX_PAGE_EVENTS),
            KindSelect::all(),
        );
        let (roster_batches, roster_acc) = body_read_batches(
            &roster_rows,
            PageBudget::new(MAX_PAGE_EVENTS),
            KindSelect::all(),
        );
        assert_eq!(
            meta_batches, roster_batches,
            "the two reads made different store round trips: {meta_batches:?} vs \
             {roster_batches:?}"
        );
        assert_eq!(meta_acc.len(), ROWS);
        assert_eq!(roster_acc.len(), ROWS);
        assert_eq!(meta_acc.skipped(), 0);
        assert_eq!(roster_acc.skipped(), 0);
        // Two round trips at this size — the first bounded probe, then one wide
        // read. Stated as a number so a regression that fragments the read into
        // 32-row batches fails here.
        assert_eq!(
            meta_batches,
            vec![FIRST_BODY_BATCH, ROWS - FIRST_BODY_BATCH]
        );
    }

    #[test]
    fn a_roster_index_becomes_byte_bound_as_rooms_grow() {
        // The converse, so the test above is not vacuous: bytes DO bind, just
        // not at today's size distribution. Every room at `general`'s live 580
        // members and the 39002 page stops on BYTES after a handful of rows —
        // a client that takes one page then sees a handful of channels and has
        // no way to tell that from "the workspace has a handful of channels"
        // except by honouring the cursor.
        let big: Vec<Event> = (0..134)
            .map(|i| roster(i as u8, 9_000 - i as u64, 580))
            .collect();
        let rows: Vec<IndexRow> = big.iter().map(kind_row).collect();
        let (_, acc) =
            body_read_batches(&rows, PageBudget::new(MAX_PAGE_EVENTS), KindSelect::all());
        assert!(
            acc.stopped(),
            "a page of 580-member rosters must stop on the byte budget"
        );
        assert!(
            acc.len() < 20,
            "expected a byte-bound handful, got {} events",
            acc.len()
        );
        assert!(
            acc.budget().remaining_events() > 400,
            "it was not count-bound"
        );
        // And it says so: a stopped page always hands back somewhere to resume.
        let page = finish_page(acc, false, Some(rows[rows.len() - 1].index_key.as_slice()));
        assert!(!page.complete && page.next_cursor.is_some());
    }

    #[test]
    fn a_page_budget_buys_index_rows_so_a_post_filtering_caller_must_page() {
        // The mechanism behind a live observation that looks like a relay bug and
        // is a paging bug: `{kinds:[39000],limit:1}` answered with ZERO events on
        // the dev relay while 134 rooms' metadata existed, and `limit:20`
        // answered with 5.
        //
        // This module performs no access control (see the module docs), so a page
        // is spent on index ROWS. Every caller that gates afterwards —
        // `access::may_deliver` on the REQ path — therefore receives at most
        // `limit` events and usually fewer, and the only correct way to get
        // `limit` DELIVERABLE events is to follow the cursor until `complete`.
        //
        // The contract that makes that possible is what is asserted here: a page
        // that did not exhaust its index never claims to be complete.
        let rows: Vec<IndexRow> = (0..10u8)
            .map(|i| metadata(i, 9_000 - u64::from(i), &format!("room-{i}")))
            .collect::<Vec<_>>()
            .iter()
            .map(kind_row)
            .collect();

        let (_, acc) = body_read_batches(&rows, PageBudget::new(1), KindSelect::all());
        assert_eq!(acc.len(), 1, "the budget bought one index row");
        assert_eq!(
            acc.filtered(),
            0,
            "nothing was declined here — the loss happens in the CALLER's gate"
        );
        let first = finish_page(acc, false, Some(rows[0].index_key.as_slice()));
        assert!(
            !first.complete && first.next_cursor.is_some(),
            "a page that stopped on its budget must offer a resume point"
        );

        // Simulate the caller's gate refusing that row (a private room the
        // reader is not in). The caller now holds ZERO events and a cursor — and
        // the cursor is the whole answer: paging on reaches the rest.
        let resumed = {
            let cursor = first.next_cursor.clone().expect("a cursor");
            let prefix = keys::kind_prefix(COMMUNITY, kind::KIND_NIP29_GROUP_METADATA);
            let resume = decode_cursor(&cursor, &prefix).expect("our own cursor");
            let start = rows
                .iter()
                .position(|r| r.index_key > resume)
                .expect("rows after the cursor");
            let (_, acc) = body_read_batches(&rows[start..], PageBudget::new(9), KindSelect::all());
            finish_page(acc, true, None)
        };
        assert_eq!(
            resumed.events.len(),
            9,
            "the remaining rooms must be reachable through the cursor"
        );
        assert!(resumed.complete && resumed.next_cursor.is_none());
    }
}
