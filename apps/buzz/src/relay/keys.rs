//! Every Store key, APE key, topic name, and Blob key this app uses.
//!
//! One module owns the whole namespace, so the layout can be diffed against
//! `docs/keys.md`, and so no call site ever formats a key inline — an ad-hoc
//! `format!("evt/{c}/{id}")` at a call site is how two writers end up disagreeing
//! about one row.
//!
//! | plane | key | value |
//! |---|---|---|
//! | ape | `admit/{c}/{id}` | admission marker + saga doc |
//! | ape | `auditref/{c}/{id}` | audit sequence for one event (ASCII decimal) |
//! | ape | `audit/{c}/{seq20}` | one audit-chain entry |
//! | ape | `audit/{c}/head` | audit-chain head |
//! | ape | `guard/ch/{c}/{chan}` | channel document |
//! | ape | `guard/th/{c}/{root}` | thread document |
//! | ape | `guard/dtag/{c}/{kind10}/{pk}/{d}` | replaceable-event head (NIP-33) |
//! | store | `evt/{c}/{id}` | canonical event JSON (written before its indexes) |
//! | store | `ts/{c}/{chan\|!g}/{inv16}/{id}` | timeline index (empty value) |
//! | store | `kind/{c}/{kind10}/{inv16}/{id}` | kind index (empty value) |
//! | store | `author/{c}/{pk}/{kind10}/{inv16}/{id}` | author index (empty value) |
//! | store | `etag/{c}/{target}/{inv16}/{id}` | reply/reaction index (empty value) |
//! | store | `mention/{c}/{who}/{inv16}/{id}` | `#p` index (empty value) |
//! | store | `sidx/{c}/{term}/{inv16}/{id}` | full-text term index (empty value) |
//! | store | `mem/{c}/{chan}/{pk}` | membership row |
//! | store | `presence/{c}/{pk}` | presence heartbeat (TTL) |
//! | store | `rate/{c}/{pk}/{class}` | rate-limit window counter (TTL) |
//! | store | `replay/{c}/{id}` | NIP-98 replay marker (TTL) |
//! | store | `upload/{id}` | operator UI-upload session (TTL) |
//! | store | `upchunk/{id}/{sha16}/{i06}` | staged UI-upload chunk (TTL) |
//! | store | `updone/{id}/{sha16}` | one upload path verified + published (TTL) |
//! | store | `mup/{id}` | media upload session (TTL) |
//! | store | `much/{id}/{i06}` | staged media chunk (TTL) |
//! | store | `sess/{c}/{session}` | SSE session document |
//! | store | `keyring/{c}/{agent}` | agent signing key, sealed under the KEK |
//! | topic | `delivery-{c}-{gen}` | live delivery feed |
//! | topic | `changefeed-{c}` | durable consumer feed |
//! | blob | `media/{sha}.{ext}` | media object |
//! | blob | `ui/{path}` | web asset |
//!
//! ## Three rules the whole layout depends on
//!
//! **1. Newest-first under an ascending scan.** Buzz orders every timeline by
//! `created_at DESC`, and the Store only scans a prefix *ascending*
//! (`ferrite-store/src/cluster_node.rs:4645`). So index keys do not store the
//! timestamp; they store its **inversion**, `u64::MAX - created_at`, as 16
//! big-endian hex digits ([`inverted_ts`]). A larger `created_at` produces a
//! smaller field, so the ascending scan hands back the newest event first and
//! paging forward walks backwards in time — no reverse scan, no client-side sort,
//! no full-bucket read. The field is fixed width, which is what makes the
//! ordering purely lexicographic and [`parse_index_tail`] unambiguous.
//!
//! **2. User text can never break the separator scheme.** Channel ids, `e`-tag
//! targets, `p`-tag values, d-tags, agent names, and session ids all arrive from
//! the wire. A raw `"a/b"` channel id would forge a two-component path, and
//! `"../.."`-style text would be even worse in a Blob key. Every such component
//! therefore goes through [`component`], which is total and injective:
//!
//! * a **safe** component (non-empty, ≤ [`MAX_COMPONENT_BYTES`], printable
//!   ASCII, no `/`, not opening with `!`) is used verbatim;
//! * anything else becomes `!` + lowercase hex of its bytes;
//! * anything longer than the cap becomes `!!` + hex of its SHA-256.
//!
//! The three images are disjoint (a safe component never starts with `!`; the
//! second form's second byte is always a hex digit, never `!`), so no component
//! can ever be spelled two ways or impersonate another one. [`is_safe_component`]
//! is public so a *mutation* path (creating a channel, naming an agent) can
//! refuse ugly input outright instead of silently accepting an escaped form.
//!
//! Escaping is not universal, though. The rule, and it is a security rule:
//!
//! * **Index buckets and free text are escaped** ([`ts_index`], [`etag_index`],
//!   [`mention_index`], [`dtag_head`], [`sess`], [`keyring`], [`rate`]). These
//!   builders are total because ingest must be able to key *every* accepted
//!   event, and because nothing about a bucket grants access.
//! * **Authorization keys are validated** ([`guard_channel`], [`mem`],
//!   [`mem_prefix`], [`guard_thread`], [`media_object`], [`ui_asset`]). These are the keys whose *presence* answers a question —
//!   "is this channel private", "is this reader a member", "serve these bytes" —
//!   so an unspellable id yields no key at all and the caller fails closed.
//!
//! **3. Buckets end in `/`.** Every scan prefix here is `/`-terminated, so
//! channel `gen` cannot bleed into channel `general`'s rows.
//!
//! Hex ids get one extra step: a value that parses as 32-byte hex is re-emitted
//! lowercase — by [`id_component`] where the component is free text that *may* be
//! an id (`e`-tag targets, `p`-tag values), and by outright validation where it
//! must be one ([`guard_thread`], [`media_object`]). Either way `AB…` and `ab…`
//! cannot split one thread's guard or one target's reply index across two
//! spellings.

use crate::relay::crypto::sha256::sha256;
use crate::relay::crypto::{from_hex32, to_hex};
use crate::relay::event::{EventId, PubKey};
use crate::relay::{BuzzError, Result};

/// Longest user-supplied component kept verbatim (or hex-escaped). Beyond this,
/// a component is replaced by its SHA-256 so one hostile 100 KiB tag value cannot
/// inflate every index key it touches.
pub const MAX_COMPONENT_BYTES: usize = 96;

/// Opens every escaped component. Chosen because it is outside both the safe
/// alphabet and the hex alphabet, which is what keeps the encodings disjoint.
const ESCAPE: u8 = b'!';

/// The timeline bucket for events with no channel (profiles, gift wraps,
/// membership notices, workflow system events).
///
/// `!g` is unreachable by [`component`] — an escaped component is `!` followed by
/// a hex digit or a second `!`, and `g` is neither — so no channel id, however
/// hostile, can be encoded into the global bucket and have its events read as
/// community-global.
pub const GLOBAL_BUCKET: &str = "!g";

/// Width of the inverted-timestamp field, in hex digits.
pub const INV_TS_HEX_LEN: usize = 16;

/// Width of an event id or pubkey field, in hex digits.
pub const ID_HEX_LEN: usize = 64;

/// Whether `s` can be used verbatim as one key component.
///
/// Rejects: empty, longer than [`MAX_COMPONENT_BYTES`], any byte outside
/// printable ASCII `0x21..=0x7e` (so control characters, newlines, spaces, and
/// every non-ASCII byte are out), `/`, and a leading `!` (reserved for escapes).
///
/// Use this to *refuse* input at a mutation boundary (creating a channel, naming
/// an agent). Index-key builders do not refuse — they escape via [`component`] —
/// while authorization-key builders refuse by calling this; see the module docs
/// for which is which and why.
#[must_use]
pub fn is_safe_component(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_COMPONENT_BYTES {
        return false;
    }
    let bytes = s.as_bytes();
    if bytes.first() == Some(&ESCAPE) {
        return false;
    }
    bytes
        .iter()
        .all(|&c| (0x21..=0x7e).contains(&c) && c != b'/')
}

/// Encode one component so it cannot break or forge a key.
///
/// Total and injective for inputs up to [`MAX_COMPONENT_BYTES`]; longer inputs
/// collapse to their SHA-256, so two distinct over-long components share a
/// bucket only under a SHA-256 collision. Output is always non-empty printable
/// ASCII containing no `/`.
#[must_use]
pub fn component(s: &str) -> String {
    if is_safe_component(s) {
        return s.to_string();
    }
    escaped(s)
}

/// The escaped form of a component: `!` + hex for anything up to the cap, `!!` +
/// hex of the SHA-256 above it. Shared by [`component`] and [`topic_component`]
/// so both namespaces escape identically.
fn escaped(s: &str) -> String {
    if s.len() <= MAX_COMPONENT_BYTES {
        let mut out = String::with_capacity(1 + s.len() * 2);
        out.push('!');
        out.push_str(&to_hex(s.as_bytes()));
        out
    } else {
        let mut out = String::with_capacity(2 + ID_HEX_LEN);
        out.push_str("!!");
        out.push_str(&to_hex(&sha256(s.as_bytes())));
        out
    }
}

/// Encode a component that is *supposed* to be a 32-byte hex id (an event id, a
/// pubkey, a digest).
///
/// A well-formed id is canonicalized to lowercase hex; anything else falls back
/// to [`component`]. No collision is possible between the two branches: a string
/// that looks like 64 hex digits always takes the first branch, and every escaped
/// form starts with `!`, which is not a hex digit.
#[must_use]
pub fn id_component(s: &str) -> String {
    match from_hex32(s) {
        Some(bytes) => to_hex(&bytes),
        None => component(s),
    }
}

/// `u64::MAX - created_at` as 16 big-endian hex digits.
///
/// This is the inversion rule 1 in the module docs describes: it turns the
/// Store's ascending prefix scan into buzz's `created_at DESC` order. Fixed width
/// is mandatory — a variable-width field would sort `"9"` after `"10"`.
#[must_use]
pub fn inverted_ts(created_at: u64) -> String {
    to_hex(&(u64::MAX - created_at).to_be_bytes())
}

/// A `u32` as 10 zero-padded decimal digits, so numeric order is byte order
/// (`kind 9` must sort before `kind 10`, not after it).
#[must_use]
fn u32_field(v: u32) -> String {
    format!("{v:010}")
}

/// A `u64` as 20 zero-padded decimal digits (the width of `u64::MAX`).
#[must_use]
fn u64_field(v: u64) -> String {
    format!("{v:020}")
}

/// Append `/{inv16}/{id}` — the tail every timeline-ordered index key ends with.
fn push_index_tail(key: &mut Vec<u8>, created_at: u64, id: &EventId) {
    key.extend_from_slice(inverted_ts(created_at).as_bytes());
    key.push(b'/');
    key.extend_from_slice(to_hex(id).as_bytes());
}

/// `admit/{c}/{id}` — the admission marker and saga document (APE, guarded).
#[must_use]
pub fn admit(community: &str, id: &EventId) -> Vec<u8> {
    format!("admit/{}/{}", component(community), to_hex(id)).into_bytes()
}

/// `admit/{c}/` — prefix for the repair sweep that re-drives admitted-but-
/// incomplete sagas.
#[must_use]
pub fn admit_prefix(community: &str) -> Vec<u8> {
    format!("admit/{}/", component(community)).into_bytes()
}

/// `auditref/{c}/{id}` — per-event audit marker, committed in the same envelope
/// as the chain entry so a resumed saga cannot append twice.
#[must_use]
pub fn auditref(community: &str, id: &EventId) -> Vec<u8> {
    format!("auditref/{}/{}", component(community), to_hex(id)).into_bytes()
}

/// `audit/{c}/{seq20}` — one entry of the hash chain. Zero-padded so an
/// ascending scan of [`audit_prefix`] is chain order.
#[must_use]
pub fn audit_entry(community: &str, seq: u64) -> Vec<u8> {
    format!("audit/{}/{}", component(community), u64_field(seq)).into_bytes()
}

/// `audit/{c}/head` — the chain head, guarded by every append.
///
/// `head` can never collide with an entry key: entries are always 20 decimal
/// digits. It sorts *after* every entry (`h` > `9`), so a full-prefix scan sees
/// the chain in order and the head last.
#[must_use]
pub fn audit_head(community: &str) -> Vec<u8> {
    format!("audit/{}/head", component(community)).into_bytes()
}

/// `audit/{c}/` — prefix covering every chain entry plus the head.
#[must_use]
pub fn audit_prefix(community: &str) -> Vec<u8> {
    format!("audit/{}/", component(community)).into_bytes()
}

/// Accept a component only if it is usable verbatim.
///
/// The error never quotes the offending value: it would put attacker-controlled
/// text into logs, and the caller already has it.
fn require_safe(value: &str, what: &str) -> Result<()> {
    if is_safe_component(value) {
        Ok(())
    } else {
        Err(BuzzError::InvalidEvent(format!(
            "{what} is not a valid key component"
        )))
    }
}

/// `guard/ch/{c}/{chan}` — the channel document; every membership or metadata
/// mutation is a compare-and-swap on it.
///
/// **Fallible where the index builders are not, and the asymmetry is the point.**
/// This key *authorizes*: it is where a reader learns a channel's visibility. A
/// channel id that is not a safe component can never have been created (creation
/// validates ids with [`is_safe_component`]), so escaping it would only
/// manufacture a key that is guaranteed to miss while making the channel look
/// resolvable. Refusing keeps the rule "one channel id has exactly one guard key,
/// or none at all", and lets callers fail closed on `Err`.
pub fn guard_channel(community: &str, channel: &str) -> Result<Vec<u8>> {
    require_safe(channel, "channel id")?;
    Ok(format!("guard/ch/{}/{}", component(community), channel).into_bytes())
}

/// `memlock/{c}/{channel}` — the per-channel lease that orders membership
/// effects, on the `guards` APE plane. Holds the event id of the effect in
/// flight and the time its lease lapses.
pub fn membership_lock(community: &str, channel: &str) -> Result<Vec<u8>> {
    require_safe(channel, "channel id")?;
    Ok(format!("memlock/{}/{}", component(community), channel).into_bytes())
}

/// `guard/th/{c}/{root}` — the thread document, keyed by the canonical lowercase
/// root event id.
///
/// A thread root is always an event id, so anything else is refused rather than
/// escaped: a thread that could be addressed by a non-id string would be a second
/// spelling for the same conversation.
pub fn guard_thread(community: &str, root_id: &str) -> Result<Vec<u8>> {
    let root = from_hex32(root_id).ok_or_else(|| {
        BuzzError::InvalidEvent("thread root is not a 32-byte hex event id".into())
    })?;
    Ok(format!("guard/th/{}/{}", component(community), to_hex(&root)).into_bytes())
}

/// `thm/{c}/{event}` — per-event thread metadata (depth, ancestry, and the reply
/// counters buzz keeps in its `thread_metadata` table). Keyed by the event id it
/// describes; `reply_count` counts an event's direct children, `descendant_count`
/// (meaningful on a root's own row) the whole subtree.
#[must_use]
pub fn thread_meta(community: &str, event_id: &str) -> Vec<u8> {
    format!("thm/{}/{}", component(community), id_component(event_id)).into_bytes()
}

/// `guard/dtag/{c}/{kind10}/{pk}/{d}` — the replaceable-event head for one
/// `(kind, author, d-tag)` triple (NIP-33). This is the guard that replaces
/// buzz's advisory lock around replacement.
///
/// The d-tag is escaped, not validated: NIP-33 d-tags are arbitrary text by
/// specification, so refusing one would refuse a legal replaceable event.
#[must_use]
pub fn dtag_head(community: &str, kind: u32, author: &PubKey, d_tag: &str) -> Vec<u8> {
    format!(
        "guard/dtag/{}/{}/{}/{}",
        component(community),
        u32_field(kind),
        to_hex(author),
        component(d_tag)
    )
    .into_bytes()
}

/// `evt/{c}/{id}` — the canonical event. Written *before* any index key, so an
/// index entry without a body means a saga is still in flight, never a
/// half-written row.
#[must_use]
pub fn evt(community: &str, id: &EventId) -> Vec<u8> {
    format!("evt/{}/{}", component(community), to_hex(id)).into_bytes()
}

/// `evt/{c}/` — prefix over every stored event body (reindex and repair paths).
#[must_use]
pub fn evt_prefix(community: &str) -> Vec<u8> {
    format!("evt/{}/", component(community)).into_bytes()
}

/// `ts/{c}/{chan|!g}/` — the timeline bucket for one channel, or the global
/// bucket when `channel` is `None`.
#[must_use]
pub fn ts_prefix(community: &str, channel: Option<&str>) -> Vec<u8> {
    let bucket = match channel {
        Some(c) => component(c),
        None => GLOBAL_BUCKET.to_string(),
    };
    format!("ts/{}/{}/", component(community), bucket).into_bytes()
}

/// `ts/{c}/{chan|!g}/{inv16}/{id}` — newest-first timeline index.
#[must_use]
pub fn ts_index(community: &str, channel: Option<&str>, created_at: u64, id: &EventId) -> Vec<u8> {
    let mut key = ts_prefix(community, channel);
    push_index_tail(&mut key, created_at, id);
    key
}

/// `kind/{c}/{kind10}/` — every event of one kind, newest first.
#[must_use]
pub fn kind_prefix(community: &str, kind: u32) -> Vec<u8> {
    format!("kind/{}/{}/", component(community), u32_field(kind)).into_bytes()
}

/// `kind/{c}/{kind10}/{inv16}/{id}` — kind index.
#[must_use]
pub fn kind_index(community: &str, kind: u32, created_at: u64, id: &EventId) -> Vec<u8> {
    let mut key = kind_prefix(community, kind);
    push_index_tail(&mut key, created_at, id);
    key
}

/// `author/{c}/{pk}/[{kind10}/]` — one author's events.
///
/// The kind sits *above* the timestamp, so `Some(kind)` scans one kind newest
/// first, while `None` scans every kind of that author in **kind-major** order
/// (each kind newest-first inside its own run). A caller that needs one strictly
/// time-ordered stream across kinds must merge the per-kind pages.
#[must_use]
pub fn author_prefix(community: &str, author: &PubKey, kind: Option<u32>) -> Vec<u8> {
    match kind {
        Some(k) => format!(
            "author/{}/{}/{}/",
            component(community),
            to_hex(author),
            u32_field(k)
        ),
        None => format!("author/{}/{}/", component(community), to_hex(author)),
    }
    .into_bytes()
}

/// `author/{c}/{pk}/{kind10}/{inv16}/{id}` — author index.
#[must_use]
pub fn author_index(
    community: &str,
    author: &PubKey,
    kind: u32,
    created_at: u64,
    id: &EventId,
) -> Vec<u8> {
    let mut key = author_prefix(community, author, Some(kind));
    push_index_tail(&mut key, created_at, id);
    key
}

/// `etag/{c}/{target}/` — everything that referenced one event (replies,
/// reactions, deletions), newest first.
#[must_use]
pub fn etag_prefix(community: &str, target: &str) -> Vec<u8> {
    format!("etag/{}/{}/", component(community), id_component(target)).into_bytes()
}

/// `etag/{c}/{target}/{inv16}/{id}` — `e`-tag index.
#[must_use]
pub fn etag_index(community: &str, target: &str, created_at: u64, id: &EventId) -> Vec<u8> {
    let mut key = etag_prefix(community, target);
    push_index_tail(&mut key, created_at, id);
    key
}

/// `mention/{c}/{who}/` — everything addressed to one pubkey, newest first.
#[must_use]
pub fn mention_prefix(community: &str, who: &str) -> Vec<u8> {
    format!("mention/{}/{}/", component(community), id_component(who)).into_bytes()
}

/// `mention/{c}/{who}/{inv16}/{id}` — `p`-tag index.
#[must_use]
pub fn mention_index(community: &str, who: &str, created_at: u64, id: &EventId) -> Vec<u8> {
    let mut key = mention_prefix(community, who);
    push_index_tail(&mut key, created_at, id);
    key
}

/// `sidx/{c}/{term}/` — every event carrying one search term, newest first.
#[must_use]
pub fn search_prefix(community: &str, term: &str) -> Vec<u8> {
    format!("sidx/{}/{}/", component(community), component(term)).into_bytes()
}

/// `sidx/{c}/{term}/{inv16}/{id}` — full-text term index.
///
/// The term is ESCAPED rather than validated, like every other index bucket: the
/// write path must be able to key every term in every accepted message, and a
/// bucket grants no access by existing. What makes this family useful is that the
/// key is fully determined by `(term, created_at, id)` — so a conjunctive query
/// can COMPUTE the key that would prove a candidate also carries another term,
/// instead of merging several ordered walks (see [`crate::relay::search`]).
#[must_use]
pub fn search_index(community: &str, term: &str, created_at: u64, id: &EventId) -> Vec<u8> {
    let mut key = search_prefix(community, term);
    push_index_tail(&mut key, created_at, id);
    key
}

/// Recover `(created_at, event_id)` from any index key built here.
///
/// Every index family ends in `/{inv16}/{id64}`, so one parser serves them all —
/// which is what lets the pager treat a cursor as opaque bytes and still know
/// which event a row points at. Strict on purpose: exact field widths, lowercase
/// hex only, so a key can never round-trip into a *different* key.
#[must_use]
pub fn parse_index_tail(key: &[u8]) -> Option<(u64, EventId)> {
    let text = core::str::from_utf8(key).ok()?;
    let (head, id_hex) = text.rsplit_once('/')?;
    let (_, inv_hex) = head.rsplit_once('/')?;
    if inv_hex.len() != INV_TS_HEX_LEN || id_hex.len() != ID_HEX_LEN {
        return None;
    }
    if !is_lower_hex(inv_hex) || !is_lower_hex(id_hex) {
        return None;
    }
    let inv_bytes: [u8; 8] = crate::relay::crypto::from_hex(inv_hex)?.try_into().ok()?;
    let id = from_hex32(id_hex)?;
    Some((u64::MAX - u64::from_be_bytes(inv_bytes), id))
}

/// Recover `(created_at, event_id)` from a `ts/` timeline key specifically.
///
/// Same recovery as [`parse_index_tail`], but refuses keys from another family,
/// so a caller paging a timeline cannot be handed a cursor from the kind index
/// and quietly accept it.
#[must_use]
pub fn parse_ts_index(key: &[u8]) -> Option<(u64, EventId)> {
    if !key.starts_with(b"ts/") {
        return None;
    }
    parse_index_tail(key)
}

fn is_lower_hex(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `mem/{c}/{chan}/{pk}` — one membership row.
///
/// Validated like [`guard_channel`] and for the same reason: membership is an
/// authorization fact, so an unspellable channel id must produce no key rather
/// than a key that misses.
pub fn mem(community: &str, channel: &str, pubkey: &PubKey) -> Result<Vec<u8>> {
    require_safe(channel, "channel id")?;
    Ok(format!(
        "mem/{}/{}/{}",
        component(community),
        channel,
        to_hex(pubkey)
    )
    .into_bytes())
}

/// `mem/{c}/{chan}/` — every member of one channel.
pub fn mem_prefix(community: &str, channel: &str) -> Result<Vec<u8>> {
    require_safe(channel, "channel id")?;
    Ok(format!("mem/{}/{}/", component(community), channel).into_bytes())
}

/// `rxn/{c}/{target}/{actor}/{emoji_hash}` — one reaction row, keyed exactly like
/// buzz's `reactions` PK `(event_id, pubkey, emoji)`. `target` and `actor` are
/// 64-hex; the emoji is hashed to a fixed 16-hex prefix so an arbitrary-length,
/// arbitrary-byte emoji cannot break the key charset or bound (the full emoji
/// lives in the row value, so the hash need only disambiguate, not round-trip).
///
/// The actor goes through [`id_component`] like the target does. Every caller
/// passes a 64-hex pubkey today, but the signature is `&str`, and the reactor is
/// parsed back out of this key (`effects::reactor_from_key`), so a raw `/` in
/// the actor would forge a path component and mis-attribute the reaction.
#[must_use]
pub fn reaction(community: &str, target_hex: &str, actor_hex: &str, emoji: &str) -> Vec<u8> {
    let digest = sha256(emoji.as_bytes());
    let mut emoji_hash = String::with_capacity(16);
    for byte in &digest[..8] {
        use core::fmt::Write;
        let _ = write!(emoji_hash, "{byte:02x}");
    }
    format!(
        "rxn/{}/{}/{}/{}",
        component(community),
        id_component(target_hex),
        id_component(actor_hex),
        emoji_hash
    )
    .into_bytes()
}

/// `restrict/{c}/{pk}` — a member's community moderation state (ban / timeout).
/// Its presence-and-contents decide whether the write path accepts the author.
#[must_use]
pub fn restriction(community: &str, pubkey: &PubKey) -> Vec<u8> {
    format!("restrict/{}/{}", component(community), to_hex(pubkey)).into_bytes()
}

/// `rxn/{c}/{target}/` — every reaction on one target event.
#[must_use]
pub fn reaction_prefix(community: &str, target_hex: &str) -> Vec<u8> {
    format!("rxn/{}/{}/", component(community), id_component(target_hex)).into_bytes()
}

/// `agentcmd/{c}/{correlation}` — the durable reservation for one persona
/// command: the `created_at` pinned by the FIRST execution, so a retried
/// command rebuilds a byte-identical event whose id the admission marker
/// absorbs (the deterministic-replay rule `event::sign_new` documents).
#[must_use]
pub fn agent_cmd(community: &str, correlation: &str) -> Vec<u8> {
    format!(
        "agentcmd/{}/{}",
        component(community),
        component(correlation)
    )
    .into_bytes()
}

/// `cmem/{c}/{pk}` — one pubkey's COMMUNITY membership (buzz's
/// `relay_members` row): materialized by an invite claim, consulted by the
/// opt-in closed-community write gate.
#[must_use]
pub fn community_mem(community: &str, pubkey: &PubKey) -> Vec<u8> {
    format!("cmem/{}/{}", component(community), to_hex(pubkey)).into_bytes()
}

/// `cmem/{c}/` — every community membership row, for the roster read.
///
/// The rows have always been written (an invite claim, the persona reconciler)
/// and never read back as a SET, so the community was unenumerable even to the
/// people in it. The trailing `/` is what keeps one community's roster out of
/// another's, exactly as it does for every other bucket here.
#[must_use]
pub fn community_mem_prefix(community: &str) -> Vec<u8> {
    format!("cmem/{}/", component(community)).into_bytes()
}

/// `wfdef/{c}/{chan}/{workflow}` — the definitions of one channel, resolving a
/// workflow id to the event that carries it.
///
/// An index rather than a scan of the kind bucket: the trigger path reads this
/// for every message in a workflow-bearing channel, and walking every 30620 in
/// the community to find the handful in one room would make the feature's cost
/// scale with the workspace instead of with the channel.
#[must_use]
pub fn workflow_definition_index(community: &str, channel: &str, workflow: &str) -> Vec<u8> {
    format!(
        "wfdef/{}/{}/{}",
        component(community),
        component(channel),
        component(workflow)
    )
    .into_bytes()
}

/// `wfdef/{c}/{chan}/` — every workflow defined in one channel.
#[must_use]
pub fn workflow_definition_index_prefix(community: &str, channel: &str) -> Vec<u8> {
    format!("wfdef/{}/{}/", component(community), component(channel)).into_bytes()
}

/// `wfown/{c}/{chan}/{workflow}` — exclusive workflow ownership guard on the
/// `guards` APE plane. Holds the creator's 64-hex pubkey so no admission,
/// resumed pre-upgrade saga, or index update can proceed for a workflow id
/// without claiming or matching its owner.
#[must_use]
pub fn workflow_owner(community: &str, channel: &str, workflow: &str) -> Vec<u8> {
    format!(
        "wfown/{}/{}/{}",
        component(community),
        component(channel),
        component(workflow)
    )
    .into_bytes()
}

/// `wfrun/{c}/{run}` — one workflow run's state.
#[must_use]
pub fn workflow_run(community: &str, run: &str) -> Vec<u8> {
    format!("wfrun/{}/{}", component(community), component(run)).into_bytes()
}

/// `wfadm/{c}/{trigger}` — which workflows one triggering message admitted, and
/// what each cost against the per-message step budget. Written create-if-absent
/// by the first pass over the trigger, so a re-drive replays that decision
/// instead of recomputing it from definitions that may have changed since.
#[must_use]
pub fn workflow_trigger_admission(community: &str, trigger: &str) -> Vec<u8> {
    format!("wfadm/{}/{}", component(community), component(trigger)).into_bytes()
}

/// `wfidx/{c}/{workflow}/{inv16}/{run}` — a workflow's runs, newest first.
///
/// The same inversion every other index here uses, so "this workflow's recent
/// runs" is an ascending prefix scan rather than a sort.
#[must_use]
pub fn workflow_run_index(community: &str, workflow: &str, started_at: u64, run: &str) -> Vec<u8> {
    format!(
        "wfidx/{}/{}/{}/{}",
        component(community),
        component(workflow),
        inverted_ts(started_at),
        component(run)
    )
    .into_bytes()
}

/// `wfidx/{c}/{workflow}/` — every run of one workflow.
#[must_use]
pub fn workflow_run_index_prefix(community: &str, workflow: &str) -> Vec<u8> {
    format!("wfidx/{}/{}/", component(community), component(workflow)).into_bytes()
}

/// `wfch/{c}/{chan}` — "this channel has at least one workflow".
///
/// Its PRESENCE is the whole signal, which is the point: an admitted message
/// checks one point read and only scans definitions when the marker is there, so
/// a workspace whose channels have no workflows pays almost nothing for the
/// feature existing.
#[must_use]
pub fn workflow_channel_marker(community: &str, channel: &str) -> Vec<u8> {
    format!("wfch/{}/{}", component(community), component(channel)).into_bytes()
}

/// `wfpin/{c}/{event}` — the definition a parked run pinned, kept while its
/// gate can still be answered: editing or pausing the workflow replaces (and
/// deletes) the 30620 event the run names.
#[must_use]
pub fn workflow_pinned_definition(community: &str, definition_event: &str) -> Vec<u8> {
    format!("wfpin/{}/{}", component(community), component(definition_event)).into_bytes()
}

/// `wftok/{c}/{token}` — an open approval gate, resolving a token to its run.
///
/// Keyed by the token because that is what a 46011 presents; the run id is the
/// value. Without it, resuming a run would mean scanning every run for a
/// matching gate.
#[must_use]
pub fn workflow_approval(community: &str, token: &str) -> Vec<u8> {
    format!("wftok/{}/{}", component(community), component(token)).into_bytes()
}

/// `iarch/{c}/{pk}` — one archived identity (NIP-IA). Its PRESENCE is the
/// archived state, which is what lets the read be a point lookup rather than a
/// scan of a list document.
#[must_use]
pub fn archived_identity(community: &str, pubkey: &PubKey) -> Vec<u8> {
    format!("iarch/{}/{}", component(community), to_hex(pubkey)).into_bytes()
}

/// `iunarch/{c}/{pk}` — the admission instant (ms) of the newest unarchive of
/// an identity. Kept outside `iarch/` so the archived list never scans it; an
/// archive request admitted before it is stale and changes nothing.
#[must_use]
pub fn unarchived_identity_watermark(community: &str, pubkey: &PubKey) -> Vec<u8> {
    format!("iunarch/{}/{}", component(community), to_hex(pubkey)).into_bytes()
}

/// `iarch/{c}/` — every archived identity, for the published 13535 list.
#[must_use]
pub fn archived_identity_prefix(community: &str) -> Vec<u8> {
    format!("iarch/{}/", component(community)).into_bytes()
}

/// `presence/{c}/{pk}` — a presence heartbeat, written with a TTL. Expiry *is*
/// the offline signal: buzz's `SET EX 90` becomes observable absence.
#[must_use]
pub fn presence(community: &str, pubkey: &PubKey) -> Vec<u8> {
    format!("presence/{}/{}", component(community), to_hex(pubkey)).into_bytes()
}

/// `presence/{c}/` — every live presence record.
#[must_use]
pub fn presence_prefix(community: &str) -> Vec<u8> {
    format!("presence/{}/", component(community)).into_bytes()
}

/// `revoked/{c}/{pk}` — the per-subscriber revocation tombstone (design §6.3
/// rule b): bumped by the effect step whenever a ban or a membership removal
/// lands for this pubkey — strictly BEFORE the saga's feed append — and read
/// once per SSE batch. Enforcement therefore never depends on the feed record
/// having been read: a saga that commits its state write and crashes before
/// its feed append is still caught on the very next delivered batch.
#[must_use]
pub fn revoked(community: &str, pubkey: &PubKey) -> Vec<u8> {
    format!("revoked/{}/{}", component(community), to_hex(pubkey)).into_bytes()
}

/// `rate/{c}/{pk}/{class}` — one fixed-window rate-limit counter, written with a
/// TTL of the window length (buzz's Redis Lua window, minus Redis).
#[must_use]
pub fn rate(community: &str, pubkey: &PubKey, class: &str) -> Vec<u8> {
    format!(
        "rate/{}/{}/{}",
        component(community),
        to_hex(pubkey),
        component(class)
    )
    .into_bytes()
}

/// `replay/{c}/{id}` — NIP-98 single-use marker for one signed request, written
/// with a TTL just past the accepted clock drift.
#[must_use]
pub fn replay(community: &str, id: &EventId) -> Vec<u8> {
    format!("replay/{}/{}", component(community), to_hex(id)).into_bytes()
}

/// `upload/{id}` — one operator UI-bundle upload session, written with a TTL.
///
/// The upload staging rows live in the CONFIG store rather than the blob plane
/// because expiring docs give an abandoned upload automatic cleanup, which the
/// blob plane (no TTL, no scan) cannot; a chunk is ≤128 KiB, far under the
/// 16 MiB store record cap. The id is relay-minted hex but escaped anyway, so
/// no id — even a forged one — can traverse.
#[must_use]
pub fn upload(id: &str) -> Vec<u8> {
    format!("upload/{}", component(id)).into_bytes()
}

/// `upchunk/{id}/{path_sha16}/{i06}` — one staged chunk of one uploaded file.
///
/// The path is keyed by `hex16(sha256(path))`, not the raw text, so key length
/// is bounded and separator-free whatever the manifest path says; the index is
/// zero-padded so an ascending listing is assembly order (the reassembly reads
/// by explicit index anyway, via `multi_get`).
#[must_use]
pub fn upchunk(id: &str, path_sha16: &str, index: u32) -> Vec<u8> {
    format!(
        "upchunk/{}/{}/{index:06}",
        component(id),
        component(path_sha16)
    )
    .into_bytes()
}

/// `upchunk/{id}/{path_sha16}/` — every staged chunk of one file.
#[must_use]
pub fn upchunk_prefix(id: &str, path_sha16: &str) -> Vec<u8> {
    format!("upchunk/{}/{}/", component(id), component(path_sha16)).into_bytes()
}

/// `updone/{id}/{path_sha16}` — one uploaded file is verified and published.
///
/// A row PER PATH, deliberately not a set inside the session document: the
/// session names every file in the bundle (~500 of them, ~100 KiB), and a
/// done-set living inside it would make each file's completion a read-modify-write of
/// the whole thing — quadratic bytes and a CAS that two concurrent file workers
/// contend on. A row keyed by the path is a
/// single small unconditional write instead, and `finish` reads them back in
/// `multi_get` batches. Written with the SESSION's expiry, so an abandoned
/// upload still self-erases with no sweeper.
#[must_use]
pub fn updone(id: &str, path_sha16: &str) -> Vec<u8> {
    format!("updone/{}/{}", component(id), component(path_sha16)).into_bytes()
}

/// `sess/{c}/{session}` — the SSE session document.
///
/// The session id also arrives from a URL path (`GET /buzz/sub/{session}`), so it
/// is escaped like any other wire component: a traversal attempt cannot address
/// another record.
#[must_use]
pub fn sess(community: &str, session_id: &str) -> Vec<u8> {
    format!("sess/{}/{}", component(community), component(session_id)).into_bytes()
}

/// `sess/{c}/` — every session (the quiesce check before retiring a feed
/// generation).
#[must_use]
pub fn sess_prefix(community: &str) -> Vec<u8> {
    format!("sess/{}/", component(community)).into_bytes()
}

/// `sesscur/{c}/{session}/{generation}` — one pump's resume cursor.
///
/// Deliberately NOT a field of the session document. A pump is not the owner of
/// the session's identity, lease or generation, so it must never write a record
/// that carries them: a blind write-back of the whole document would revert a
/// newer connection's generation bump and hand the session to a pump whose client
/// has already gone. Keying the cursor by generation as well as session means a
/// superseded pump's final write cannot be mistaken for the live one's progress.
#[must_use]
pub fn sess_cursor(community: &str, session_id: &str, generation: u64) -> Vec<u8> {
    format!(
        "sesscur/{}/{}/{}",
        component(community),
        component(session_id),
        generation
    )
    .into_bytes()
}

/// `wsconn/{c}/{correlation}` — one WebSocket connection's state: its NIP-42
/// challenge, the pubkey AUTH bound, and its live subscription table.
///
/// Durable rather than in-memory because a connection's frames arrive on a
/// SHARED queue: the instance that leases a `REQ` need not be the instance
/// running that connection's live pump, so a subscription the pump cannot read
/// is a subscription that is never served. Keyed by the gateway's correlation id,
/// which is the connection's identity, and escaped like any wire component.
#[must_use]
pub fn wsconn(community: &str, correlation: &str) -> Vec<u8> {
    format!("wsconn/{}/{}", component(community), component(correlation)).into_bytes()
}

/// `wscur/{c}/{correlation}` — one WebSocket pump's resume cursor.
///
/// Split out of the connection document for the same reason
/// [`sess_cursor`] is split out of the session: the pump owns its progress and
/// nothing else, so writing the whole document back would let it revert a
/// subscription a concurrently-served `REQ` had just added.
#[must_use]
pub fn wsconn_cursor(community: &str, correlation: &str) -> Vec<u8> {
    format!("wscur/{}/{}", component(community), component(correlation)).into_bytes()
}

/// `agentpaused/{c}/{agent}` — "this persona is registered but not answering".
///
/// Its PRESENCE is the whole signal, and it is a row of its own rather than a
/// field of the registry document: that document is one CAS point for the entire
/// roster, so pausing an agent through it would contend with every concurrent
/// creation for no reason.
#[must_use]
pub fn agent_paused(community: &str, agent: &str) -> Vec<u8> {
    format!("agentpaused/{}/{}", component(community), component(agent)).into_bytes()
}

/// `mem/{c}/{agent}/` — one agent's memory entries, on the memory plane.
///
/// A different PLANE from the channel-membership `mem/` family, which is why the
/// same stem is not a collision: this key is only ever built against the
/// `agent-memory` binding.
#[must_use]
pub fn agent_memory_prefix(community: &str, agent: &str) -> Vec<u8> {
    format!("mem/{}/{}/", component(community), component(agent)).into_bytes()
}

/// `agentowner/{c}/{agent}` — who created a client-made cloud persona.
///
/// Deliberately NOT a field of the agent's definition document: that document
/// is parsed with `deny_unknown_fields` by the agent runtime's own loader, so
/// a creator stamped inside it would make the definition unreadable. Keyed
/// like every other per-agent row, so retiring a persona can ask "is this
/// caller the one who made it?" instead of widening who may remove anything.
#[must_use]
pub fn agent_owner(community: &str, agent: &str) -> Vec<u8> {
    format!("agentowner/{}/{}", component(community), component(agent)).into_bytes()
}

/// `keyring/{c}/{agent}` — an agent's signing key, sealed under the app's KEK
/// secret. The app cannot mint keys (no entropy source); it stores what an
/// operator provisioned.
#[must_use]
pub fn keyring(community: &str, agent: &str) -> Vec<u8> {
    format!("keyring/{}/{}", component(community), component(agent)).into_bytes()
}

/// `keyring/{c}/` — every provisioned agent key.
#[must_use]
pub fn keyring_prefix(community: &str) -> Vec<u8> {
    format!("keyring/{}/", component(community)).into_bytes()
}

/// Whether `s` is usable verbatim in a topic name: non-empty, ≤ 64 bytes, and
/// `[a-z0-9-]` only. Narrower than [`is_safe_component`] because topic names are
/// operator-visible identifiers, not opaque keys.
#[must_use]
pub fn is_safe_topic_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Encode a component of a topic name. Same escape scheme as [`component`], and
/// `!` is outside the safe topic alphabet, so the two spaces stay disjoint.
///
/// Note it escapes against the *topic* alphabet, not the key alphabet: `Main` is
/// a fine Store component but not a topic component, so it is escaped here.
#[must_use]
pub fn topic_component(s: &str) -> String {
    if is_safe_topic_component(s) {
        return s.to_string();
    }
    escaped(s)
}

/// `delivery-{c}-{gen}` — the live delivery feed for one generation.
///
/// The generation is in the *name* because topic offsets are dense per topic and
/// restart at zero after a rotation: a cursor is always `(generation, offset)`,
/// never a bare offset. Generations are compared numerically by the app, so the
/// decimal is unpadded.
#[must_use]
pub fn feed_topic(community: &str, generation: u32) -> String {
    format!("delivery-{}-{}", topic_component(community), generation)
}

/// `delivery-{c}-` — prefix for enumerating a community's feed generations when
/// retiring one ([`crate::relay::ingest::retire_superseded_feeds`]).
#[must_use]
pub fn feed_topic_prefix(community: &str) -> String {
    format!("delivery-{}-", topic_component(community))
}

/// The generation `topic` carries, if it is one of `community`'s feed topics.
///
/// The inverse of [`feed_topic`], and the reason retirement can be decided from
/// a topic listing alone. It is deliberately strict — the whole tail must be
/// decimal digits that fit a `u32` — because the caller DELETES what this
/// returns a generation for. Another community's feed, the changefeed, and any
/// name that merely starts the same way all answer `None`.
#[must_use]
pub fn feed_generation_of(topic: &str, community: &str) -> Option<u32> {
    let tail = topic.strip_prefix(&feed_topic_prefix(community))?;
    if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    tail.parse().ok()
}

/// `changefeed-{c}` — the durable feed every non-live consumer reads (workflow
/// triggers, search upsert, dispatcher enqueue, reminder arming). It is never
/// rotated, so consumers keep a bare offset.
#[must_use]
pub fn changefeed_topic(community: &str) -> String {
    format!("changefeed-{}", topic_component(community))
}

/// `media/{sha}.{ext}` — one media object.
///
/// Returns `None` unless `sha_hex` is a 32-byte hex digest and `ext` is a short
/// lowercase alphanumeric extension, and canonicalizes the digest to lowercase.
/// Validated rather than escaped, because the key is also the public URL suffix
/// under the `media/` content route (`ferrite.json`: `/buzz/blob/` → `media/`) —
/// the digest *is* the capability, so a malformed one must be refused, not
/// rewritten into something that would address a different object.
///
/// **The extension is not decoration.** The gateway derives `Content-Type` from
/// it and from nothing else, so it is
/// what decides whether an uploaded image renders or downloads — and, for a
/// public route served from the app's own origin, what a browser is willing to
/// execute. It is therefore chosen by the relay's own allowlist
/// ([`crate::relay::media::extension_for_mime`]) from bytes it has verified,
/// never taken from a caller-supplied filename.
#[must_use]
pub fn media_object(sha_hex: &str, ext: &str) -> Option<String> {
    let digest = from_hex32(sha_hex)?;
    if ext.is_empty()
        || ext.len() > 8
        || !ext
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return None;
    }
    Some(format!("media/{}.{ext}", to_hex(&digest)))
}

/// `mup/{id}` — one in-flight media upload session, written with a TTL.
///
/// Its own family rather than the UI bundle's `upload/`: an operator pushing a
/// web bundle and a member attaching an image are different authorities writing
/// at different rates, and sharing one prefix would let a collision between them
/// be possible in principle and confusing in practice. The id is relay-minted hex
/// but escaped anyway, so no id — even a forged one — can traverse.
#[must_use]
pub fn media_upload(id: &str) -> Vec<u8> {
    format!("mup/{}", component(id)).into_bytes()
}

/// `much/{id}/{i06}` — one staged chunk of one media upload.
///
/// Zero-padded so an ascending listing is assembly order, though reassembly reads
/// by explicit index via `multi_get`. Staged in the STORE rather than the blob
/// plane for the reason the UI bundle's chunks are: expiring docs give an
/// abandoned upload automatic cleanup, which the blob plane (no TTL, no scan)
/// cannot.
#[must_use]
pub fn media_stage(id: &str, index: u32) -> Vec<u8> {
    format!("much/{}/{index:06}", component(id)).into_bytes()
}

/// `much/{id}/` — every staged chunk of one media upload.
#[must_use]
pub fn media_stage_prefix(id: &str) -> Vec<u8> {
    format!("much/{}/", component(id)).into_bytes()
}

/// `ui/{path}` — a web asset under the public `/buzz/ui/` content route.
///
/// The only key builder where `/` is *meaningful*, so escaping is not an option:
/// the gateway maps the URL suffix straight onto the key. The path is therefore
/// validated and refused when it could traverse or inject — non-empty, ≤ 512
/// bytes, printable ASCII, no `\`, no empty segment (so no leading, trailing, or
/// doubled `/`), and no `.` or `..` segment.
#[must_use]
pub fn ui_asset(path: &str) -> Option<String> {
    if path.is_empty() || path.len() > 512 {
        return None;
    }
    if !path
        .bytes()
        .all(|b| (0x21..=0x7e).contains(&b) && b != b'\\')
    {
        return None;
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return None;
        }
    }
    Some(format!("ui/{path}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const ID_HEX: &str = "abababababababababababababababababababababababababababababababab";
    const PK_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    /// `u64::MAX - 1_700_000_000`, big-endian hex.
    const INV_1_7E9: &str = "ffffffff9aac0eff";

    fn id() -> EventId {
        [0xab; 32]
    }

    fn pk() -> PubKey {
        [0x01; 32]
    }

    fn s(key: &[u8]) -> String {
        String::from_utf8(key.to_vec()).unwrap()
    }

    #[test]
    fn ape_keys_have_exact_bytes() {
        assert_eq!(s(&admit("main", &id())), format!("admit/main/{ID_HEX}"));
        assert_eq!(s(&admit_prefix("main")), "admit/main/");
        assert_eq!(
            s(&auditref("main", &id())),
            format!("auditref/main/{ID_HEX}")
        );
        assert_eq!(
            s(&audit_entry("main", 7)),
            "audit/main/00000000000000000007"
        );
        assert_eq!(s(&audit_head("main")), "audit/main/head");
        assert_eq!(s(&audit_prefix("main")), "audit/main/");
        assert_eq!(
            s(&guard_channel("main", "chan-1").unwrap()),
            "guard/ch/main/chan-1"
        );
        assert_eq!(
            s(&guard_thread("main", ID_HEX).unwrap()),
            format!("guard/th/main/{ID_HEX}")
        );
        assert_eq!(
            s(&dtag_head("main", 30_023, &pk(), "spec")),
            format!("guard/dtag/main/0000030023/{PK_HEX}/spec")
        );
    }

    #[test]
    fn store_record_keys_have_exact_bytes() {
        assert_eq!(s(&evt("main", &id())), format!("evt/main/{ID_HEX}"));
        assert_eq!(s(&evt_prefix("main")), "evt/main/");
        assert_eq!(
            s(&mem("main", "chan-1", &pk()).unwrap()),
            format!("mem/main/chan-1/{PK_HEX}")
        );
        assert_eq!(
            s(&mem_prefix("main", "chan-1").unwrap()),
            "mem/main/chan-1/"
        );
        assert_eq!(
            s(&presence("main", &pk())),
            format!("presence/main/{PK_HEX}")
        );
        assert_eq!(s(&presence_prefix("main")), "presence/main/");
        assert_eq!(
            s(&rate("main", &pk(), "msg")),
            format!("rate/main/{PK_HEX}/msg")
        );
        assert_eq!(s(&replay("main", &id())), format!("replay/main/{ID_HEX}"));
        assert_eq!(s(&sess("main", "s-42")), "sess/main/s-42");
        assert_eq!(s(&sess_prefix("main")), "sess/main/");
        assert_eq!(s(&keyring("main", "triage-bot")), "keyring/main/triage-bot");
        assert_eq!(s(&keyring_prefix("main")), "keyring/main/");
    }

    #[test]
    fn index_keys_have_exact_bytes() {
        assert_eq!(
            s(&ts_index("main", Some("chan-1"), 1_700_000_000, &id())),
            format!("ts/main/chan-1/{INV_1_7E9}/{ID_HEX}")
        );
        assert_eq!(
            s(&ts_index("main", None, 1_700_000_000, &id())),
            format!("ts/main/!g/{INV_1_7E9}/{ID_HEX}")
        );
        assert_eq!(s(&ts_prefix("main", Some("chan-1"))), "ts/main/chan-1/");
        assert_eq!(s(&ts_prefix("main", None)), "ts/main/!g/");
        assert_eq!(
            s(&kind_index("main", 1111, 1_700_000_000, &id())),
            format!("kind/main/0000001111/{INV_1_7E9}/{ID_HEX}")
        );
        assert_eq!(s(&kind_prefix("main", 1111)), "kind/main/0000001111/");
        assert_eq!(
            s(&author_index("main", &pk(), 1111, 1_700_000_000, &id())),
            format!("author/main/{PK_HEX}/0000001111/{INV_1_7E9}/{ID_HEX}")
        );
        assert_eq!(
            s(&author_prefix("main", &pk(), None)),
            format!("author/main/{PK_HEX}/")
        );
        assert_eq!(
            s(&author_prefix("main", &pk(), Some(1111))),
            format!("author/main/{PK_HEX}/0000001111/")
        );
        assert_eq!(
            s(&etag_index("main", ID_HEX, 1_700_000_000, &id())),
            format!("etag/main/{ID_HEX}/{INV_1_7E9}/{ID_HEX}")
        );
        assert_eq!(
            s(&mention_index("main", PK_HEX, 1_700_000_000, &id())),
            format!("mention/main/{PK_HEX}/{INV_1_7E9}/{ID_HEX}")
        );
    }

    #[test]
    fn topic_and_blob_names_have_exact_bytes() {
        assert_eq!(feed_topic("main", 0), "delivery-main-0");
        assert_eq!(feed_topic("main", 12), "delivery-main-12");
        assert_eq!(feed_topic_prefix("main"), "delivery-main-");
        assert_eq!(changefeed_topic("main"), "changefeed-main");
        assert_eq!(
            media_object(ID_HEX, "png"),
            Some(format!("media/{ID_HEX}.png"))
        );
        assert_eq!(s(&media_upload("ab12")), "mup/ab12");
        assert_eq!(s(&media_stage("ab12", 3)), "much/ab12/000003");
        assert_eq!(s(&media_stage_prefix("ab12")), "much/ab12/");
        assert_eq!(ui_asset("app/main.js"), Some("ui/app/main.js".into()));
    }

    #[test]
    fn timeline_keys_sort_newest_first() {
        let old = ts_index("main", Some("c"), 1_699_999_999, &[1; 32]);
        let mid = ts_index("main", Some("c"), 1_700_000_000, &[2; 32]);
        let new = ts_index("main", Some("c"), 1_700_000_001, &[3; 32]);

        let mut keys = vec![old.clone(), new.clone(), mid.clone()];
        keys.sort();
        assert_eq!(
            keys,
            vec![new, mid, old],
            "ascending scan must be newest-first"
        );
    }

    #[test]
    fn every_index_family_sorts_newest_first() {
        let earlier = 1_000_u64;
        let later = 2_000_u64;
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (
                ts_index("main", None, later, &[9; 32]),
                ts_index("main", None, earlier, &[9; 32]),
            ),
            (
                kind_index("main", 1, later, &[9; 32]),
                kind_index("main", 1, earlier, &[9; 32]),
            ),
            (
                author_index("main", &pk(), 1, later, &[9; 32]),
                author_index("main", &pk(), 1, earlier, &[9; 32]),
            ),
            (
                etag_index("main", ID_HEX, later, &[9; 32]),
                etag_index("main", ID_HEX, earlier, &[9; 32]),
            ),
            (
                mention_index("main", PK_HEX, later, &[9; 32]),
                mention_index("main", PK_HEX, earlier, &[9; 32]),
            ),
        ];
        for (newer, older) in pairs {
            assert!(newer < older, "{} !< {}", s(&newer), s(&older));
        }
    }

    #[test]
    fn inverted_timestamps_are_fixed_width_and_monotone() {
        assert_eq!(inverted_ts(0), "ffffffffffffffff");
        assert_eq!(inverted_ts(1), "fffffffffffffffe");
        assert_eq!(inverted_ts(u64::MAX), "0000000000000000");
        assert_eq!(inverted_ts(1_700_000_000), INV_1_7E9);
        // Fixed width is what makes lexicographic order numeric order.
        for t in [0_u64, 1, 999, 1_000, u64::MAX / 3, u64::MAX] {
            assert_eq!(inverted_ts(t).len(), INV_TS_HEX_LEN);
        }
        // A decimal field would sort "9" after "10"; the hex inversion does not.
        assert!(inverted_ts(10) < inverted_ts(9));
    }

    #[test]
    fn zero_padded_numeric_fields_sort_numerically() {
        assert!(kind_prefix("main", 9) < kind_prefix("main", 10));
        assert!(audit_entry("main", 9) < audit_entry("main", 10));
        assert!(audit_entry("main", 2) < audit_entry("main", 1_000_000));
        // The head sorts after every entry, so a chain scan reads entries first.
        assert!(audit_entry("main", u64::MAX) < audit_head("main"));
        assert!(media_stage("u", 9) < media_stage("u", 10));
    }

    #[test]
    fn parse_ts_index_round_trips() {
        for created_at in [0_u64, 1, 1_700_000_000, u64::MAX - 1, u64::MAX] {
            let key = ts_index("main", Some("chan-1"), created_at, &id());
            assert_eq!(parse_ts_index(&key), Some((created_at, id())));
            let global = ts_index("main", None, created_at, &id());
            assert_eq!(parse_ts_index(&global), Some((created_at, id())));
        }
    }

    #[test]
    fn parse_index_tail_serves_every_family() {
        let t = 1_700_000_000_u64;
        assert_eq!(
            parse_index_tail(&kind_index("main", 7, t, &id())),
            Some((t, id()))
        );
        assert_eq!(
            parse_index_tail(&author_index("main", &pk(), 7, t, &id())),
            Some((t, id()))
        );
        assert_eq!(
            parse_index_tail(&etag_index("main", ID_HEX, t, &id())),
            Some((t, id()))
        );
        assert_eq!(
            parse_index_tail(&mention_index("main", PK_HEX, t, &id())),
            Some((t, id()))
        );
        // A hostile channel id cannot break the tail parser either.
        assert_eq!(
            parse_index_tail(&ts_index("main", Some("a/b\n"), t, &id())),
            Some((t, id()))
        );
    }

    #[test]
    fn parse_ts_index_refuses_other_families_and_garbage() {
        let t = 1_700_000_000_u64;
        assert_eq!(parse_ts_index(&kind_index("main", 7, t, &id())), None);
        assert_eq!(parse_ts_index(b""), None);
        assert_eq!(parse_ts_index(b"ts/main/c/"), None);
        assert_eq!(parse_ts_index(b"ts/main/c/deadbeef/short"), None);
        // Wrong field widths.
        assert_eq!(
            parse_ts_index(format!("ts/main/c/ffff/{ID_HEX}").as_bytes()),
            None
        );
        assert_eq!(
            parse_ts_index(format!("ts/main/c/{INV_1_7E9}/{ID_HEX}ab").as_bytes()),
            None
        );
        // Uppercase hex is refused: a key must have exactly one spelling.
        assert_eq!(
            parse_ts_index(format!("ts/main/c/{}/{ID_HEX}", INV_1_7E9.to_uppercase()).as_bytes()),
            None
        );
        assert_eq!(
            parse_ts_index(format!("ts/main/c/{INV_1_7E9}/{}", ID_HEX.to_uppercase()).as_bytes()),
            None
        );
        // A sign-prefixed field must not sneak through an integer parser.
        assert_eq!(
            parse_ts_index(format!("ts/main/c/+fffffffffffffff/{ID_HEX}").as_bytes()),
            None
        );
        // Not UTF-8 at all.
        assert_eq!(parse_ts_index(&[0xff, 0xfe, b'/']), None);
    }

    #[test]
    fn hostile_components_are_rejected_by_the_predicate() {
        for bad in [
            "",
            "/",
            "a/b",
            "a\nb",
            "a b",
            "\0",
            "\u{7f}",
            "café",
            "!escaped",
            "!!escaped",
        ] {
            assert!(!is_safe_component(bad), "accepted {bad:?}");
        }
        assert!(!is_safe_component(&"a".repeat(MAX_COMPONENT_BYTES + 1)));
        assert!(is_safe_component(&"a".repeat(MAX_COMPONENT_BYTES)));
        for good in ["a", "chan-1", "General_Channel", "30023", "a.b", "#tag"] {
            assert!(is_safe_component(good), "rejected {good:?}");
        }
    }

    #[test]
    fn hostile_components_are_escaped_into_one_safe_component() {
        // A separator in user text must not create a component boundary.
        assert_eq!(component("a/b"), "!612f62");
        assert_eq!(component("a\nb"), "!610a62");
        assert_eq!(component(""), "!");
        // Over-long components collapse to a digest, so key size stays bounded.
        let long = "z".repeat(300);
        let escaped = component(&long);
        assert!(escaped.starts_with("!!"), "{escaped}");
        assert_eq!(escaped.len(), 2 + ID_HEX_LEN);
        assert_eq!(component(&long), escaped, "escaping is deterministic");
        // Every escape is itself a safe single component: no `/`, printable.
        for hostile in ["a/b", "a\nb", "", &long, "\u{7f}", "café"] {
            let enc = component(hostile);
            assert!(!enc.contains('/'), "{enc}");
            assert!(enc.bytes().all(|b| (0x21..=0x7e).contains(&b)), "{enc}");
        }
    }

    #[test]
    fn escaped_components_cannot_impersonate_a_literal_one() {
        // The literal text of an escape is itself escaped, so the two spaces
        // never meet: a channel *named* "!612f62" is not channel "a/b".
        assert_ne!(component("a/b"), component("!612f62"));
        assert_eq!(component("!612f62"), "!21363132663632");
        // Distinct hostile inputs stay distinct.
        assert_ne!(component("a/b"), component("a\\b"));
        assert_ne!(component("a/b"), component("ab"));
        // A short component can never collide with a digested one: the markers
        // differ in their second byte, and `!` is not a hex digit.
        let digested = component(&"z".repeat(300));
        for probe in ["a/b", "", "\n"] {
            assert_ne!(component(probe), digested);
        }
    }

    #[test]
    fn a_hostile_channel_cannot_reach_the_global_bucket() {
        // The global timeline must be unreachable from the channel namespace, or
        // a channel could publish into every subscriber's community-wide view.
        for probe in [GLOBAL_BUCKET, "!g", "g", "!", "!!g", " !g"] {
            assert_ne!(
                s(&ts_prefix("main", Some(probe))),
                s(&ts_prefix("main", None)),
                "channel {probe:?} reached the global bucket"
            );
        }
    }

    #[test]
    fn a_hostile_reactor_cannot_forge_a_reaction_key_component() {
        // `rxn/{c}/{target}/{actor}/{emoji_hash}`: exactly four separators, and
        // the second-to-last component (what the reactor is parsed back from)
        // stays one escaped component rather than a smuggled path.
        let target = to_hex(&[7u8; 32]);
        let key = s(&reaction("main", &target, "a/b", "+"));
        assert_eq!(key.matches('/').count(), 4, "{key}");
        let actor = key.rsplit('/').nth(1).unwrap();
        assert!(actor.starts_with('!'), "{key}");
        // A well-formed pubkey keeps its canonical lowercase spelling, so rows
        // written before this change still resolve.
        let pk = to_hex(&[0xabu8; 32]);
        let key = s(&reaction("main", &target, &pk.to_uppercase(), "+"));
        assert_eq!(key.rsplit('/').nth(1).unwrap(), pk);
    }

    #[test]
    fn a_hostile_channel_cannot_forge_another_key() {
        // `chan/../..`-style text stays one component, so it cannot climb out of
        // the timeline family or into another channel's rows.
        let forged = s(&ts_index("main", Some("../../evt/main"), 1, &id()));
        assert!(forged.starts_with("ts/main/!"), "{forged}");
        assert_eq!(forged.matches('/').count(), 4);

        // Nor can a channel id smuggle in a second community.
        let cross = s(&ts_prefix("main", Some("x/other")));
        assert!(!cross.contains("/other/"), "{cross}");
    }

    #[test]
    fn bucket_prefixes_do_not_bleed_into_longer_names() {
        // The trailing `/` is what keeps channel "gen" out of channel "general".
        let short = ts_prefix("main", Some("gen"));
        let long_key = ts_index("main", Some("general"), 1, &id());
        assert!(!long_key.starts_with(&short));
        assert!(ts_index("main", Some("gen"), 1, &id()).starts_with(&short));

        let m_short = mention_prefix("main", "ab");
        assert!(!mention_index("main", "abc", 1, &id()).starts_with(&m_short));

        let a_short = author_prefix("main", &pk(), None);
        assert!(author_index("main", &pk(), 1, 1, &id()).starts_with(&a_short));
        assert!(!author_index("main", &[0x02; 32], 1, 1, &id()).starts_with(&a_short));

        // Two families whose names share a stem must not scan into each other.
        assert!(!auditref("main", &id()).starts_with(&audit_prefix("main")));
        assert!(audit_entry("main", 1).starts_with(&audit_prefix("main")));
        assert!(
            !mention_index("main", PK_HEX, 1, &id()).starts_with(&mem_prefix("main", "c").unwrap())
        );
        assert!(!etag_index("main", ID_HEX, 1, &id()).starts_with(&evt_prefix("main")));
    }

    #[test]
    fn hex_ids_are_canonicalized_so_one_target_is_one_bucket() {
        let upper = ID_HEX.to_uppercase();
        assert_eq!(etag_prefix("main", ID_HEX), etag_prefix("main", &upper));
        assert_eq!(
            mention_prefix("main", PK_HEX),
            mention_prefix("main", &PK_HEX.to_uppercase())
        );
        assert_eq!(
            guard_thread("main", ID_HEX).unwrap(),
            guard_thread("main", &upper).unwrap()
        );
        // Non-hex targets still get a bucket, just an escaped one.
        assert_eq!(s(&etag_prefix("main", "not-an-id")), "etag/main/not-an-id/");
        assert_eq!(s(&etag_prefix("main", "a/b")), "etag/main/!612f62/");
        // And a 64-char non-hex string is not mistaken for an id.
        let fake = "z".repeat(64);
        assert_ne!(etag_prefix("main", &fake), etag_prefix("main", ID_HEX));
    }

    #[test]
    fn different_families_never_collide() {
        let t = 1_700_000_000_u64;
        let all = vec![
            admit("main", &id()),
            auditref("main", &id()),
            audit_entry("main", 1),
            audit_head("main"),
            guard_channel("main", "c").unwrap(),
            guard_thread("main", ID_HEX).unwrap(),
            dtag_head("main", 1, &pk(), "d"),
            evt("main", &id()),
            ts_index("main", Some("c"), t, &id()),
            ts_index("main", None, t, &id()),
            kind_index("main", 1, t, &id()),
            author_index("main", &pk(), 1, t, &id()),
            etag_index("main", ID_HEX, t, &id()),
            mention_index("main", PK_HEX, t, &id()),
            mem("main", "c", &pk()).unwrap(),
            presence("main", &pk()),
            rate("main", &pk(), "msg"),
            replay("main", &id()),
            sess("main", "s"),
            keyring("main", "agent"),
        ];
        let mut sorted = all.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len());
    }

    #[test]
    fn two_communities_never_share_a_row() {
        assert_ne!(evt("main", &id()), evt("other", &id()));
        assert_ne!(ts_prefix("main", None), ts_prefix("other", None));
        // A community id carrying a separator cannot reach into another one.
        let hostile = s(&evt("main/../other", &id()));
        assert!(hostile.starts_with("evt/!"), "{hostile}");
        assert_eq!(hostile.matches('/').count(), 2);
    }

    #[test]
    fn topic_names_stay_in_their_alphabet() {
        assert!(is_safe_topic_component("main"));
        assert!(is_safe_topic_component("team-42"));
        assert!(!is_safe_topic_component("Main"));
        assert!(!is_safe_topic_component("a_b"));
        assert!(!is_safe_topic_component("a/b"));
        assert!(!is_safe_topic_component(""));
        assert_eq!(feed_topic("a/b", 1), "delivery-!612f62-1");
        assert_eq!(changefeed_topic("Main"), "changefeed-!4d61696e");
        // Each generation is its own topic; a rotation cannot alias the old one.
        assert_ne!(feed_topic("main", 1), feed_topic("main", 2));
        assert!(feed_topic("main", 7).starts_with(&feed_topic_prefix("main")));
        // The feed and the changefeed are different topics.
        assert_ne!(feed_topic("main", 0), changefeed_topic("main"));
    }

    /// The parse that decides what a retirement sweep may DELETE, so every
    /// near-miss has to answer `None` rather than a plausible number.
    #[test]
    fn only_a_communitys_own_feed_topic_yields_a_generation() {
        assert_eq!(feed_generation_of("delivery-main-0", "main"), Some(0));
        assert_eq!(feed_generation_of("delivery-main-12", "main"), Some(12));
        // A round trip, for every generation the app can name.
        for g in [0, 1, 7, u32::MAX] {
            assert_eq!(feed_generation_of(&feed_topic("main", g), "main"), Some(g));
        }
        // Another community's feed, and a community whose name escapes.
        assert_eq!(feed_generation_of("delivery-other-3", "main"), None);
        assert_eq!(feed_generation_of("delivery-main-3", "other"), None);
        assert_eq!(feed_generation_of(&feed_topic("a/b", 4), "a/b"), Some(4));
        assert_eq!(feed_generation_of(&feed_topic("a/b", 4), "main"), None);
        // Not a generation: the changefeed, a bare prefix, a non-decimal tail,
        // a sub-name, and a number no `u32` holds.
        assert_eq!(feed_generation_of("changefeed-main", "main"), None);
        assert_eq!(feed_generation_of("delivery-main-", "main"), None);
        assert_eq!(feed_generation_of("delivery-main-1a", "main"), None);
        assert_eq!(feed_generation_of("delivery-main-1-shadow", "main"), None);
        assert_eq!(feed_generation_of("delivery-main--1", "main"), None);
        assert_eq!(feed_generation_of("delivery-main-4294967296", "main"), None);
    }

    #[test]
    fn media_keys_require_a_real_digest_and_a_safe_extension() {
        assert_eq!(
            media_object(&ID_HEX.to_uppercase(), "png"),
            media_object(ID_HEX, "png")
        );
        let too_long = format!("{ID_HEX}ab");
        for bad in ["", "abcd", ID_HEX_SHORT, too_long.as_str(), "zz"] {
            assert_eq!(media_object(bad, "png"), None, "accepted digest {bad:?}");
        }
        // The extension is a public URL suffix AND the gateway's Content-Type
        // input, so it is validated, never interpolated.
        for bad in ["", "PNG", "p/g", "p.g", "../x", "toolongext", "pn g", "p-g"] {
            assert_eq!(
                media_object(ID_HEX, bad),
                None,
                "accepted extension {bad:?}"
            );
        }
        for good in ["png", "jpg", "webp", "mp4", "avif", "txt", "md", "zip"] {
            assert!(media_object(ID_HEX, good).is_some(), "refused {good:?}");
        }
        // A traversal attempt is refused outright, never rewritten: this key is
        // a public URL suffix.
        assert_eq!(media_object("../../ui/app.js", "png"), None);
    }

    #[test]
    fn media_staging_keys_cannot_traverse_or_collide_with_the_bundle_path() {
        // A hostile id is escaped into ONE component, never a second path level.
        let hostile = s(&media_upload("../../keyring/main/agent"));
        assert!(hostile.starts_with("mup/!"), "{hostile}");
        assert_eq!(hostile.matches('/').count(), 1);
        let hostile = s(&media_stage("../sess/main", 0));
        assert!(hostile.contains("/!"), "{hostile}");
        assert_eq!(hostile.matches('/').count(), 2);
        // Chunks stay under their own upload's prefix, and one upload's chunks
        // never bleed into another's.
        assert!(media_stage("aa", 1).starts_with(&media_stage_prefix("aa")));
        assert!(!media_stage("aab", 1).starts_with(&media_stage_prefix("aa")));
        // The member and operator upload families are disjoint, so the same id
        // used by both addresses different rows.
        assert_ne!(media_upload("x"), upload("x"));
        assert_ne!(media_stage("x", 0), upchunk("x", "p", 0));
        assert!(!media_stage("x", 0).starts_with(b"mup/"));
    }

    const ID_HEX_SHORT: &str = "ababababababababababababababababababababababababababababababab";

    #[test]
    fn ui_asset_paths_are_validated_not_escaped() {
        assert_eq!(ui_asset("index.html"), Some("ui/index.html".into()));
        assert_eq!(ui_asset("a/b/c.js"), Some("ui/a/b/c.js".into()));
        for bad in [
            "",
            "/leading",
            "trailing/",
            "double//slash",
            "../escape",
            "a/../b",
            "a/./b",
            ".",
            "..",
            "back\\slash",
            "with space.js",
            "new\nline",
        ] {
            assert_eq!(ui_asset(bad), None, "accepted {bad:?}");
        }
        assert_eq!(ui_asset(&"a".repeat(513)), None);
        assert!(ui_asset(&"a".repeat(512)).is_some());
    }

    #[test]
    fn upload_staging_keys_round_trip_and_cannot_traverse() {
        assert_eq!(s(&upload("ab12cd34ef56ab12")), "upload/ab12cd34ef56ab12");
        assert_eq!(
            s(&upchunk("ab12cd34ef56ab12", "0011223344556677", 3)),
            "upchunk/ab12cd34ef56ab12/0011223344556677/000003"
        );
        assert_eq!(
            s(&upchunk_prefix("ab12cd34ef56ab12", "0011223344556677")),
            "upchunk/ab12cd34ef56ab12/0011223344556677/"
        );
        // Zero-padded indexes keep numeric order lexicographic.
        assert!(upchunk("u", "p", 9) < upchunk("u", "p", 10));
        // Chunks stay under their own file's prefix, and prefixes end in `/`
        // so one file's chunks never bleed into another's.
        assert!(upchunk("u", "aa", 1).starts_with(&upchunk_prefix("u", "aa")));
        assert!(!upchunk("u", "aab", 1).starts_with(&upchunk_prefix("u", "aa")));
        // A hostile id or path component is escaped, never a second path level.
        let hostile = s(&upload("../../replay/main/x"));
        assert!(hostile.starts_with("upload/!"), "{hostile}");
        assert_eq!(hostile.matches('/').count(), 1);
        let hostile = s(&upchunk("u", "../sess/main", 0));
        assert!(hostile.contains("/!"), "{hostile}");
        assert_eq!(hostile.matches('/').count(), 3);
        // The two families never collide with each other or their neighbours.
        assert!(!upchunk("u", "p", 0).starts_with(b"upload/"));
        assert_ne!(upload("s"), sess("main", "s"));
    }

    #[test]
    fn session_ids_from_a_url_path_cannot_traverse() {
        assert_eq!(s(&sess("main", "deadbeef")), "sess/main/deadbeef");
        let hostile = s(&sess("main", "../../keyring/main/agent"));
        assert!(hostile.starts_with("sess/main/!"), "{hostile}");
        assert_eq!(hostile.matches('/').count(), 2);
        assert_ne!(sess("main", "../keyring/main/a"), keyring("main", "a"));
    }

    #[test]
    fn authorization_keys_refuse_what_index_keys_escape() {
        // The two-tier rule: a bucket escapes hostile text, an authorization key
        // refuses it, so a caller resolving access fails closed instead of
        // reading a manufactured row.
        for hostile in ["", "a/b", "a\nb", "!spoof", &"c".repeat(300)] {
            assert!(guard_channel("main", hostile).is_err(), "{hostile:?}");
            assert!(mem("main", hostile, &pk()).is_err(), "{hostile:?}");
            assert!(mem_prefix("main", hostile).is_err(), "{hostile:?}");
            // ... while the index bucket for the same text still exists.
            assert!(!ts_prefix("main", Some(hostile)).is_empty());
        }
        assert!(guard_channel("main", "chan-1").is_ok());
        assert!(mem("main", "chan-1", &pk()).is_ok());

        // A thread root must be a real event id.
        for hostile in ["", "abcd", ID_HEX_SHORT, "../../evt/main/x"] {
            assert!(guard_thread("main", hostile).is_err(), "{hostile:?}");
        }
        assert!(guard_thread("main", ID_HEX).is_ok());
    }
}
