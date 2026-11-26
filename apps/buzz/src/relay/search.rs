//! Search: an inverted index on ordered Store keys.
//!
//! Buzz routed NIP-50 to a separate search engine. There is none here — an app
//! has one dependency and six planes — so search is built out of the primitive
//! every other read path in this relay is built out of: a prefix-scannable,
//! lexicographically ordered key space.
//!
//! ## Why not the cheap version
//!
//! The obvious shortcut is to scan recent events and filter their content in
//! memory. It was refused deliberately: it answers "no results" for anything
//! older than the scan window, which is indistinguishable from "that was never
//! said" and is precisely the accepted-but-not-honoured shape this codebase
//! removes wherever it appears. A search that silently cannot see last month is
//! worse than one that says it is unsupported.
//!
//! ## The layout
//!
//! One key per (term, event): `sidx/{c}/{term}/{inv16}/{id}`, written at ingest
//! beside the timeline, kind, author, e-tag and mention indexes, with the SAME
//! inverted timestamp — so a term's bucket scans newest-first under the Store's
//! ascending prefix scan, exactly like every other index here.
//!
//! ## How AND works, and why it is cheap
//!
//! An index key is fully determined by `(term, created_at, event_id)`. Nothing
//! else goes into it. So once ONE term's bucket has yielded a candidate, that
//! candidate's membership in every OTHER term's bucket is a key that can be
//! COMPUTED — there is nothing to search for.
//!
//! A conjunctive query is therefore:
//!
//! 1. one ordered walk of the driving term's bucket (newest-first, paged), then
//! 2. one BATCHED point-read that asks "do these exact keys exist" for every
//!    other term times every candidate.
//!
//! Not an N-way merge, and not a scan. The driving term is the LONGEST one, on
//! the heuristic that longer words are rarer and the rarest term is the cheapest
//! driver — a cost heuristic only, never a correctness one, since the point-reads
//! decide membership whatever order the walk happens in.
//!
//! ## What it costs to write
//!
//! Up to [`MAX_TERMS_PER_EVENT`] extra index keys per indexed event. That is the
//! real price of this feature and it is why the cap is small, why terms are
//! deduplicated and length-bounded, and why only kinds a person would actually
//! search are indexed at all ([`is_searchable_kind`]).
//!
//! ## What it deliberately does not do
//!
//! No stemming, no ranking. Results come back newest-first, which is what a chat
//! search wants and what the key ordering already provides for nothing. Terms are
//! folded to lowercase but not transliterated, so matching is case-insensitive
//! without pretending to implement any language's collation.

use ferrite_app::client::StoreClusterClient;

use crate::relay::event::{Event, PubKey};
use crate::relay::{keys, kind, query, store, BuzzError, Result};

/// The most terms one event contributes to the index.
///
/// This IS the write amplification: an indexed event costs up to this many extra
/// keys. A conversational message rarely reaches it; a pasted document would, and
/// is truncated to its first distinct terms rather than being allowed to write an
/// unbounded number of rows.
pub const MAX_TERMS_PER_EVENT: usize = 32;

/// The most terms one query may carry. Each additional term is another batched
/// point-read per candidate, so the bound is on the reader's cost.
pub const MAX_QUERY_TERMS: usize = 8;

/// Shortest indexed term. One-character tokens match nearly everything and cost a
/// row each; two is the useful floor for a chat corpus ("ok", "pr", "ci").
pub const MIN_TERM_CHARS: usize = 2;

/// Longest indexed term, in BYTES, so the key stays bounded whatever the script.
/// A longer word is truncated rather than dropped: its prefix still finds it.
pub const MAX_TERM_BYTES: usize = 32;

/// Candidates examined per page of the driving term's bucket. A `u32` because
/// that is what `store::scan` takes.
const SEARCH_PAGE: u32 = 64;

/// How many pages of the driving bucket one query will walk before answering with
/// what it has. A conjunctive query can discard most of a page, so one page is not
/// enough to be useful — and an unbounded walk would turn one request into a scan
/// of every message containing a common word.
const SEARCH_MAX_PAGES: usize = 8;

/// Whether this kind's content is worth indexing.
///
/// Messages and forum text — what a person means by "search". Deliberately NOT
/// reactions (an emoji), profiles (a name, already searchable by other means),
/// ephemeral events (never stored at all), or relay projections (derived state,
/// not speech).
#[must_use]
pub const fn is_searchable_kind(k: u32) -> bool {
    matches!(
        k,
        kind::KIND_STREAM_MESSAGE
            | kind::KIND_STREAM_MESSAGE_EDIT
            | kind::KIND_STREAM_MESSAGE_V2
            | kind::KIND_FORUM_POST
            | kind::KIND_FORUM_COMMENT
    )
}

/// Split text into the distinct terms it will be indexed or searched under.
///
/// Lowercased, split on anything that is not alphanumeric, deduplicated, bounded
/// in count and in byte length. The SAME function serves both the write path and
/// the read path, which is the only way a query can be guaranteed to produce the
/// terms an event was actually indexed under — two tokenizers would be two
/// opinions about what a word is, and the failure would be silent (a query that
/// finds nothing).
#[must_use]
pub fn terms_of(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    // A closure would borrow `out` mutably while the loop also pushes, so the
    // flush is written inline at both exits instead.
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            // Lowercase per character: `to_lowercase` on the whole string would
            // allocate a second copy of a message that may be kilobytes long.
            for lower in ch.to_lowercase() {
                if current.len() + lower.len_utf8() <= MAX_TERM_BYTES {
                    current.push(lower);
                }
            }
            continue;
        }
        push_term(&mut out, &mut current);
        if out.len() >= MAX_TERMS_PER_EVENT {
            return out;
        }
    }
    push_term(&mut out, &mut current);
    out.truncate(MAX_TERMS_PER_EVENT);
    out
}

/// Move a completed token into the term list, if it is one worth keeping.
fn push_term(out: &mut Vec<String>, current: &mut String) {
    if current.chars().count() >= MIN_TERM_CHARS && !out.iter().any(|t| t == current) {
        out.push(core::mem::take(current));
    } else {
        current.clear();
    }
}

/// The terms a query will be answered by: the same tokenization as the write
/// path, bounded by [`MAX_QUERY_TERMS`], longest first.
///
/// Longest first is a COST decision, not a correctness one: the first term drives
/// the ordered walk and the rest become point-reads, so the walk should start
/// from the rarest bucket, and length is the only rarity signal available without
/// keeping term counts.
#[must_use]
pub fn query_terms(query: &str) -> Vec<String> {
    let mut terms = terms_of(query);
    terms.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    terms.truncate(MAX_QUERY_TERMS);
    terms
}

/// Every index key an event contributes to search.
///
/// Empty for a kind that is not searchable, which is what keeps the write
/// amplification confined to the kinds a person actually searches.
#[must_use]
pub fn index_keys_for(community: &str, event: &Event) -> Vec<Vec<u8>> {
    if !is_searchable_kind(event.kind) {
        return Vec::new();
    }
    terms_of(&event.content)
        .iter()
        .map(|term| keys::search_index(community, term, event.created_at, &event.id))
        .collect()
}

/// Optional narrowing beyond the terms — the search operators upstream's
/// client grew in v0.5.x (`from:` / `after:` / `before:`, PR #2871), which
/// arrive at the HTTP route as `authors` / `since` / `until`.
///
/// Honoured here rather than dropped, because dropping one silently returns
/// messages the caller explicitly excluded — the accepted-but-not-honoured
/// shape this module refuses. Each bound lands where it is cheapest: `until`
/// is a START POSITION on the newest-first index, `since` ENDS the walk (the
/// first too-old candidate proves everything further is older), and `authors`
/// filters on the body read the access gate already pays for.
#[derive(Default)]
pub struct SearchScope {
    /// Only events by one of these authors; empty means anyone.
    pub authors: Vec<PubKey>,
    /// Only events at or after this timestamp (inclusive, NIP-01 `since`).
    pub since: Option<u64>,
    /// Only events at or before this timestamp (inclusive, NIP-01 `until`).
    pub until: Option<u64>,
}

/// One search hit, with the position that resumes AFTER it.
///
/// The cursor travels with the event because the caller filters: a route drops
/// hits for access or channel scope, so "where do I resume" is a question only
/// the caller can answer, and answering it with the page's own end position
/// silently skips every hit it fetched but did not return.
pub struct SearchHit {
    pub event: Event,
    /// Opaque resume position immediately after this hit.
    pub cursor: String,
}

/// One page of search results, and — crucially — whether there are more.
///
/// A bare `Vec` was the first shape and it was wrong in a way that mattered: the
/// walk is bounded ([`SEARCH_MAX_PAGES`]), so an empty result could mean "nothing
/// matches" OR "the budget ran out before anything did", and a caller could not
/// tell. For a conjunction that is not a corner case — pair a common word with a
/// rare one and the common bucket's first pages may hold no match at all — so an
/// opaque cutoff reproduces exactly the silently-incomplete behaviour this module
/// refuses to ship.
pub struct SearchPage {
    /// Matching events, newest first, each with its own resume position.
    pub events: Vec<SearchHit>,
    /// Where to resume, when the walk stopped before exhausting the driving
    /// bucket. Opaque to callers and bound to the term that produced it.
    pub next_cursor: Option<String>,
    /// Whether the driving bucket was walked to its end. `false` means the answer
    /// is a prefix of the truth, not the whole of it.
    pub complete: bool,
}

/// Encode a resume position. Hex of the index key, which is opaque to a caller
/// and cheap to validate on the way back in.
fn encode_cursor(key: &[u8]) -> String {
    crate::relay::crypto::to_hex(key)
}

/// Decode a resume position, REFUSING one that belongs to a different bucket.
///
/// A cursor is a scan position inside one term's bucket, so replaying it against
/// another term would resume at an arbitrary offset of an unrelated key space and
/// silently skip results. Bound to its prefix, a stale or forged cursor is a
/// clean refusal instead.
fn decode_cursor(cursor: &str, prefix: &[u8]) -> Result<Vec<u8>> {
    let bytes = crate::relay::crypto::from_hex(cursor)
        .ok_or_else(|| BuzzError::InvalidEvent("search cursor is not hex".into()))?;
    if !bytes.starts_with(prefix) {
        return Err(BuzzError::InvalidEvent(
            "search cursor does not belong to this query's terms".into(),
        ));
    }
    Ok(bytes)
}

/// How many bytes of IV a sealed cursor carries.
const SEAL_IV_BYTES: usize = 16;

/// The keystream for a sealed cursor: HMAC-SHA256 in counter mode under a key
/// derived for this one purpose.
fn cursor_keystream(key: &[u8], iv: &[u8], len: usize) -> Vec<u8> {
    use crate::relay::crypto::hmac::hmac_sha256;
    let enc = hmac_sha256(key, b"buzz-search-cursor/enc");
    let mut out = Vec::with_capacity(len.saturating_add(32));
    let mut block: u32 = 0;
    while out.len() < len {
        out.extend_from_slice(&hmac_sha256(&enc, &[iv, &block.to_be_bytes()].concat()));
        block = block.wrapping_add(1);
    }
    out.truncate(len);
    out
}

/// The synthetic IV of a cursor: a MAC over the plaintext, so the same position
/// always seals to the same string and opening can authenticate it.
fn cursor_iv(key: &[u8], plain: &[u8]) -> Vec<u8> {
    use crate::relay::crypto::hmac::hmac_sha256;
    let mac = hmac_sha256(key, b"buzz-search-cursor/mac");
    hmac_sha256(&mac, plain)
        .get(..SEAL_IV_BYTES)
        .map(<[u8]>::to_vec)
        .unwrap_or_default()
}

/// Seal a resume position for a caller.
///
/// A plain cursor is the hex of an index key, `sidx/{c}/{term}/{inv_ts}/{id}`,
/// and the walk picks it BEFORE any access decision, so a page whose hits were
/// all denied still handed back the id and exact timestamp of a message in a
/// room the caller may not read. Sealed (a deterministic authenticated
/// encryption: a MAC-derived IV plus an HMAC-SHA256 counter-mode keystream,
/// under the relay's sealed stream key), the cursor still resumes the walk
/// exactly where it stopped but says nothing about what it points at, and a
/// forged or altered one is refused by [`open_cursor`].
#[must_use]
pub fn seal_cursor(key: &[u8], cursor: &str) -> String {
    let plain = cursor.as_bytes();
    let iv = cursor_iv(key, plain);
    let stream = cursor_keystream(key, &iv, plain.len());
    let mut sealed = iv;
    sealed.extend(plain.iter().zip(stream.iter()).map(|(p, k)| p ^ k));
    crate::relay::crypto::to_hex(&sealed)
}

/// Open a cursor [`seal_cursor`] produced, refusing anything it did not.
pub fn open_cursor(key: &[u8], sealed: &str) -> Result<String> {
    let refused = || BuzzError::InvalidEvent("search cursor is not one this relay issued".into());
    let bytes = crate::relay::crypto::from_hex(sealed).ok_or_else(refused)?;
    if bytes.len() <= SEAL_IV_BYTES {
        return Err(refused());
    }
    let (iv, body) = bytes.split_at(SEAL_IV_BYTES);
    let stream = cursor_keystream(key, iv, body.len());
    let plain: Vec<u8> = body.iter().zip(stream.iter()).map(|(c, k)| c ^ k).collect();
    if !crate::crypto::ct_eq(&cursor_iv(key, &plain), iv) {
        return Err(refused());
    }
    String::from_utf8(plain).map_err(|_| refused())
}

/// The resume position that begins a walk at `until`, for the term that will
/// drive it.
///
/// A NIP-01 `until` is a deep-scrollback request, and on a newest-first index it
/// is simply a POSITION: every key with a later timestamp sorts before this one,
/// so starting after it skips exactly the events the caller excluded. The same
/// trick `walk_index` uses for every other index here.
///
/// Without it, a search filter carrying `until` walks from the newest row every
/// time — inspecting the same capped window of candidates and answering empty for
/// anything older, which looks exactly like "nothing was ever said".
#[must_use]
pub fn until_cursor(community: &str, terms: &[String], until: u64) -> Option<String> {
    let driver = terms.first()?;
    let mut key = keys::search_prefix(community, driver);
    key.extend_from_slice(keys::inverted_ts(until).as_bytes());
    Some(encode_cursor(&key))
}

/// Does this event satisfy a NIP-50 search term?
///
/// The LIVE counterpart of the index, and deliberately built from the same
/// [`terms_of`]: a subscription must not deliver something a historical query
/// would not have returned, nor withhold something it would. A second matcher —
/// substring, say — would disagree with the index in both directions, and the
/// disagreement would be invisible.
///
/// Non-searchable kinds never match, because they are never indexed: a reaction
/// carrying the letters of a query is not a search result.
#[must_use]
pub fn event_matches(query: &str, event: &Event) -> bool {
    if !is_searchable_kind(event.kind) {
        return false;
    }
    let wanted = query_terms(query);
    if wanted.is_empty() {
        // A query with no terms selects nothing, exactly as `search` answers
        // nothing for it.
        return false;
    }
    let have = terms_of(&event.content);
    wanted.iter().all(|term| have.iter().any(|h| h == term))
}

/// Answer one search: the newest events whose content carries EVERY term.
///
/// Returns stored events, newest-first, up to `limit`, together with whether the
/// driving bucket was exhausted and where to resume if it was not. Access is
/// deliberately NOT decided here — the caller re-authorizes each hit through the
/// one access chokepoint (`access::may_deliver`), exactly as the timeline and ws
/// read paths do, because an index is a routing structure and never an access
/// decision.
///
/// An empty term list answers with nothing rather than with everything: a query
/// of pure punctuation has no meaning, and returning the whole community for it
/// would be a surprising and expensive way to say so.
pub async fn search(
    store: &mut StoreClusterClient,
    community: &str,
    terms: &[String],
    limit: usize,
    cursor: Option<&str>,
    scope: &SearchScope,
) -> Result<SearchPage> {
    search_impl(store, community, terms, limit, cursor, scope, false).await
}

/// Product search returns the latest signed edit, never superseded wording.
/// Raw NIP-50 reads retain their immutable-event semantics through `search`.
pub async fn search_current(
    store: &mut StoreClusterClient,
    community: &str,
    terms: &[String],
    limit: usize,
    cursor: Option<&str>,
    scope: &SearchScope,
) -> Result<SearchPage> {
    search_impl(store, community, terms, limit, cursor, scope, true).await
}

async fn search_impl(
    store: &mut StoreClusterClient,
    community: &str,
    terms: &[String],
    limit: usize,
    cursor: Option<&str>,
    scope: &SearchScope,
    current: bool,
) -> Result<SearchPage> {
    let mut latest = std::collections::BTreeMap::new();
    let Some(driver) = terms.first() else {
        // No terms: nothing to walk, and nothing further to find.
        return Ok(SearchPage {
            events: Vec::new(),
            next_cursor: None,
            complete: true,
        });
    };
    let prefix = keys::search_prefix(community, driver);
    let mut scan_from: Option<Vec<u8>> = match cursor {
        Some(cursor) => Some(decode_cursor(cursor, &prefix)?),
        // `until` is a position on the newest-first index: everything later
        // sorts before it, so starting there skips exactly what the caller
        // excluded (the `until_cursor` construction, applied directly).
        None => scope.until.map(|until| {
            let mut key = prefix.clone();
            key.extend_from_slice(keys::inverted_ts(until).as_bytes());
            key
        }),
    };
    // Asked for nothing, so nothing is read. Checked BEFORE the walk rather than
    // inside it: a test after a push would make `limit: 0`, which is exactly what
    // a NIP-01 live-only subscription asks for, return one event. The position is
    // handed straight back, since none was consumed.
    if limit == 0 {
        return Ok(SearchPage {
            events: Vec::new(),
            next_cursor: cursor.map(str::to_string),
            complete: false,
        });
    }
    let rest = terms.get(1..).unwrap_or_default();

    let mut out: Vec<SearchHit> = Vec::new();
    for _ in 0..SEARCH_MAX_PAGES {
        let (rows, more) = store::scan(store, &prefix, scan_from.as_deref(), SEARCH_PAGE).await?;
        if rows.is_empty() {
            // The bucket is exhausted: this answer is the whole truth.
            return Ok(SearchPage {
                events: out,
                next_cursor: None,
                complete: true,
            });
        }
        // Advance past every row EXAMINED, including any that will not parse, so
        // the next page cannot re-read them.
        scan_from = rows.last().map(|(key, _, _)| key.clone());

        // The driving bucket is already newest-first, so this page's candidates
        // arrive in the order they will be answered in. Each keeps its own key,
        // because a cursor must point at the last CONSUMED candidate.
        let mut candidates: Vec<(Vec<u8>, u64, crate::relay::event::EventId)> = rows
            .iter()
            .filter_map(|(key, _, _)| {
                keys::parse_index_tail(key).map(|(at, id)| (key.clone(), at, id))
            })
            .collect();
        // `since` ends the walk rather than filtering it: the first candidate
        // older than the bound proves everything beyond it is older too, so
        // within the requested range the answer is then COMPLETE.
        let mut since_exhausted = false;
        if let Some(since) = scope.since {
            if let Some(cut) = candidates.iter().position(|(_, at, _)| *at < since) {
                candidates.truncate(cut);
                since_exhausted = true;
            }
        }
        // A supplied cursor takes precedence over `until` for the start
        // position, so a resumed page re-checks the bound per candidate.
        if let Some(until) = scope.until {
            candidates.retain(|(_, at, _)| *at <= until);
        }
        if candidates.is_empty() {
            if since_exhausted || !more {
                return Ok(SearchPage {
                    events: out,
                    next_cursor: None,
                    complete: true,
                });
            }
            continue;
        }

        // The conjunction, as COMPUTED keys rather than a search: an index key is
        // fully determined by (term, created_at, id), so "does this candidate also
        // carry that term" is a point read on a key we can build.
        let mut matched: Vec<&(Vec<u8>, u64, crate::relay::event::EventId)> = Vec::new();
        if rest.is_empty() {
            matched.extend(candidates.iter());
        } else {
            let mut probes: Vec<Vec<u8>> = Vec::with_capacity(candidates.len() * rest.len());
            for (_, created_at, id) in &candidates {
                for term in rest {
                    probes.push(keys::search_index(community, term, *created_at, id));
                }
            }
            let found = store::multi_get_raw(store, &probes).await?;
            for (i, candidate) in candidates.iter().enumerate() {
                let from = i.saturating_mul(rest.len());
                let slice = found.get(from..from.saturating_add(rest.len()));
                if slice.is_some_and(|s| s.iter().all(Option::is_some)) {
                    matched.push(candidate);
                }
            }
        }
        if !matched.is_empty() {
            // One batched read for the bodies too: an index entry without its
            // event means a saga is still in flight, and is skipped rather than
            // served as a half-written row.
            let body_keys: Vec<Vec<u8>> = matched
                .iter()
                .map(|(_, _, id)| keys::evt(community, id))
                .collect();
            let bodies = store::multi_get_raw(store, &body_keys).await?;
            for ((index_key, _, _), (key, body)) in
                matched.iter().zip(body_keys.iter().zip(bodies.iter()))
            {
                let Some(bytes) = body else { continue };
                let Ok(event) = store::decode_doc::<Event>(key, bytes) else {
                    continue;
                };
                // The author bound, on the body the access gate needs anyway.
                // Skipped hits are re-examined on a resumed walk and skipped
                // again, so the cursor stays sound.
                if !scope.authors.is_empty() && !scope.authors.contains(&event.pubkey) {
                    continue;
                }
                if current && !is_current_message(store, community, &event, &mut latest).await? {
                    continue;
                }
                // Resume AFTER this candidate. Pointing at the last row merely
                // examined would skip everything between it and this one,
                // because the scan is exclusive of its start key.
                out.push(SearchHit {
                    event,
                    cursor: encode_cursor(index_key),
                });
                if out.len() >= limit {
                    let cursor = out.last().map(|hit| hit.cursor.clone());
                    return Ok(SearchPage {
                        events: out,
                        next_cursor: cursor,
                        complete: false,
                    });
                }
            }
        }
        if since_exhausted {
            // The bound was reached: within the requested range this answer is
            // the whole truth, whatever the bucket still holds beyond it.
            return Ok(SearchPage {
                events: out,
                next_cursor: None,
                complete: true,
            });
        }
        if !more {
            return Ok(SearchPage {
                events: out,
                next_cursor: None,
                complete: true,
            });
        }
    }
    // The page budget ran out with the bucket still unexhausted. Say so, and say
    // where to continue — an empty page here means "nothing matched YET", which is
    // a different claim from "nothing matches".
    Ok(SearchPage {
        events: out,
        next_cursor: scan_from.as_deref().map(encode_cursor),
        complete: false,
    })
}

/// Resolve an edit's original without altering any signed event. Reference
/// reads are bounded and fail explicitly if the history cannot be resolved.
async fn is_current_message(
    store: &mut StoreClusterClient,
    community: &str,
    event: &Event,
    latest: &mut std::collections::BTreeMap<
        crate::relay::event::EventId,
        Option<crate::relay::event::EventId>,
    >,
) -> Result<bool> {
    let original = if event.kind == kind::KIND_STREAM_MESSAGE_EDIT {
        let Some(id) = event
            .tag_values("e")
            .next()
            .and_then(crate::relay::crypto::from_hex32)
        else {
            return Ok(false);
        };
        let Some(original) = query::event_by_id(store, community, &id).await? else {
            return Ok(false);
        };
        if original.pubkey != event.pubkey
            || original.channel_id() != event.channel_id()
            || !matches!(
                original.kind,
                kind::KIND_STREAM_MESSAGE | kind::KIND_STREAM_MESSAGE_V2
            )
        {
            return Ok(false);
        }
        original
    } else if matches!(
        event.kind,
        kind::KIND_STREAM_MESSAGE | kind::KIND_STREAM_MESSAGE_V2
    ) {
        event.clone()
    } else {
        return Ok(true);
    };
    if !latest.contains_key(&original.id) {
        let target = crate::relay::crypto::to_hex(&original.id);
        let mut cursor = None;
        let mut resolved = false;
        for _ in 0..8 {
            let page = query::by_etag(
                store,
                community,
                &target,
                cursor.as_deref(),
                query::MAX_PAGE_EVENTS,
            )
            .await?;
            if let Some(edit) = page.events.iter().find(|edit| {
                edit.kind == kind::KIND_STREAM_MESSAGE_EDIT
                    && edit.pubkey == original.pubkey
                    && edit.channel_id() == original.channel_id()
                    && edit.created_at >= original.created_at
                    && edit.tag_values("e").next() == Some(target.as_str())
            }) {
                latest.insert(original.id, Some(edit.id));
                resolved = true;
                break;
            }
            if page.complete {
                latest.insert(original.id, None);
                resolved = true;
                break;
            }
            cursor = page.next_cursor;
        }
        if !resolved {
            // One candidate whose references outgrow the budget must not abort
            // the whole search: anybody could otherwise poison every query that
            // matches their message by replying to it heavily
            // (finding_48bebe6867061e82). The original stays the current
            // version, as it is when it has no edit, and an edit is not shown.
            ferrite_app::tracing::warn!("edit history of {target} exceeds the search budget; showing the original");
            latest.insert(original.id, None);
        }
    }
    Ok(match latest.get(&original.id).copied().flatten() {
        Some(edit) => event.id == edit,
        None => event.id == original.id,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_cursor_round_trips_hides_its_position_and_refuses_forgery() {
        let key = [7u8; 32];
        let plain = encode_cursor(b"sidx/main/secret/ffffffffffffff00/abcdef");
        let sealed = seal_cursor(&key, &plain);
        assert_eq!(open_cursor(&key, &sealed).unwrap(), plain);
        // Deterministic, so a re-driven page hands back the same string.
        assert_eq!(seal_cursor(&key, &plain), sealed);
        // Nothing of the index key is readable in it.
        let bytes = crate::relay::crypto::from_hex(&sealed).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !sealed.contains(&plain) && !text.contains("sidx"),
            "{sealed}"
        );
        // A raw index-key cursor, a flipped bit, and another key's seal all fail.
        assert!(open_cursor(&key, &plain).is_err());
        let mut tampered = bytes.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(open_cursor(&key, &crate::relay::crypto::to_hex(&tampered)).is_err());
        assert!(open_cursor(&[8u8; 32], &sealed).is_err());
    }

    #[test]
    fn tokenization_lowercases_splits_and_deduplicates() {
        assert_eq!(terms_of("Hello World"), vec!["hello", "world"]);
        // Punctuation splits; repeats collapse; case folds.
        assert_eq!(
            terms_of("Deploy, deploy... DEPLOY!"),
            vec!["deploy"],
            "one term, however it was spelled"
        );
        assert_eq!(terms_of("ship-it now"), vec!["ship", "it", "now"]);
        // Single characters are below the floor and cost a row each.
        assert_eq!(terms_of("a b cd"), vec!["cd"]);
        assert_eq!(terms_of(""), Vec::<String>::new());
        assert_eq!(terms_of("!!! ??? ..."), Vec::<String>::new());
    }

    #[test]
    fn the_query_and_the_write_path_tokenize_identically() {
        // Two tokenizers would be two opinions about what a word is, and the
        // failure mode is silent: a query that simply finds nothing.
        for text in [
            "Deploy the RELAY now",
            "caf\u{e9} r\u{e9}sum\u{e9}",
            "snake_case and kebab-case",
            "123 456",
        ] {
            let mut written = terms_of(text);
            let mut asked = query_terms(text);
            written.sort();
            asked.sort();
            assert_eq!(written, asked, "{text:?}");
        }
    }

    #[test]
    fn a_query_is_driven_by_its_longest_term() {
        // A cost decision: the first term drives the ordered walk, the rest
        // become point reads, so the walk should start from the rarest bucket.
        let terms = query_terms("the deployment ci");
        assert_eq!(terms.first().map(String::as_str), Some("deployment"));
        // Deterministic among equal lengths, so a repeated query is a repeated
        // plan rather than a coin toss.
        assert_eq!(query_terms("bbb aaa"), query_terms("aaa bbb"));
    }

    #[test]
    fn terms_are_bounded_in_count_and_in_bytes() {
        // The write amplification is capped: a pasted document cannot write an
        // unbounded number of index rows.
        let many = (0..500)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(terms_of(&many).len(), MAX_TERMS_PER_EVENT);
        // A long word is truncated, not dropped — its prefix still finds it.
        let long = "z".repeat(200);
        let terms = terms_of(&long);
        assert_eq!(terms.len(), 1);
        assert!(terms[0].len() <= MAX_TERM_BYTES);
        // And a query cannot ask for more probes than the bound allows.
        assert_eq!(query_terms(&many).len(), MAX_QUERY_TERMS);
    }

    #[test]
    fn a_multibyte_term_is_never_split_mid_character() {
        // The length bound is in BYTES so keys stay bounded whatever the script,
        // which makes it possible to truncate inside a character if done wrong.
        let text = "\u{4f60}\u{597d}".repeat(40); // 3 bytes per char
        for term in terms_of(&text) {
            assert!(term.len() <= MAX_TERM_BYTES);
        }
        assert_eq!(
            terms_of("caf\u{c9}"),
            vec!["caf\u{e9}"],
            "folded, not mangled"
        );
    }

    #[test]
    fn only_the_kinds_a_person_searches_are_indexed() {
        assert!(is_searchable_kind(kind::KIND_STREAM_MESSAGE));
        assert!(is_searchable_kind(kind::KIND_FORUM_POST));
        // Not an emoji, not a profile, not a relay projection, not presence.
        assert!(!is_searchable_kind(kind::KIND_REACTION));
        assert!(!is_searchable_kind(kind::KIND_PROFILE));
        assert!(!is_searchable_kind(kind::KIND_NIP29_GROUP_METADATA));
        assert!(!is_searchable_kind(kind::KIND_PRESENCE_UPDATE));
    }

    #[test]
    fn index_keys_are_written_only_for_searchable_kinds() {
        let mut event = Event {
            id: [7u8; 32],
            pubkey: [1u8; 32],
            created_at: 1_700_000_000,
            kind: kind::KIND_STREAM_MESSAGE,
            tags: Vec::new(),
            content: "deploy the relay".into(),
            sig: [0u8; 64],
        };
        let written = index_keys_for("main", &event);
        assert_eq!(written.len(), 3, "deploy, the, relay");
        // Every key is distinct and lands in the search family.
        let mut sorted = written.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), written.len());
        assert!(written.iter().all(|k| k.starts_with(b"sidx/")));

        // A reaction with the same content contributes nothing.
        event.kind = kind::KIND_REACTION;
        assert!(index_keys_for("main", &event).is_empty());
    }

    #[test]
    fn live_matching_agrees_with_what_the_index_would_have_returned() {
        // The live pump and the historical walk must not disagree: a subscriber
        // should never receive something a query would not return, nor miss
        // something it would. Both sides go through `terms_of`.
        let message = |content: &str| Event {
            id: [1u8; 32],
            pubkey: [2u8; 32],
            created_at: 1_700_000_000,
            kind: kind::KIND_STREAM_MESSAGE,
            tags: Vec::new(),
            content: content.into(),
            sig: [0u8; 64],
        };
        let event = message("Deploy the relay tonight");
        assert!(event_matches("deploy", &event), "case folds");
        assert!(event_matches("DEPLOY relay", &event), "conjunction");
        assert!(!event_matches("deploy database", &event), "AND, not OR");
        assert!(
            !event_matches("deployment", &event),
            "no stemming, by design"
        );
        // Punctuation-only queries select nothing rather than everything.
        assert!(!event_matches("!!!", &event));
        assert!(!event_matches("", &event));
        // A kind that is never indexed can never match live either.
        let mut reaction = message("deploy");
        reaction.kind = kind::KIND_REACTION;
        assert!(!event_matches("deploy", &reaction));
    }

    #[test]
    fn an_until_bound_is_a_start_position_in_the_driving_bucket() {
        // On a newest-first index, `until` is a position: every later event
        // sorts before it. Without this the walk restarts at the newest row and
        // a deep scrollback query answers empty forever.
        let terms = query_terms("deployment");
        let cursor = until_cursor("main", &terms, 1_700_000_000).expect("a term to drive");
        let prefix = keys::search_prefix("main", "deployment");
        let decoded = decode_cursor(&cursor, &prefix).expect("belongs to the bucket");

        // A key AT the bound sorts after the cursor, so the scan includes it...
        let at_bound = keys::search_index("main", "deployment", 1_700_000_000, &[5u8; 32]);
        assert!(at_bound > decoded, "the bound itself must be reachable");
        // ...and a NEWER key sorts before it, so the scan skips it.
        let newer = keys::search_index("main", "deployment", 1_700_000_001, &[5u8; 32]);
        assert!(newer < decoded, "newer events must be skipped");
        // An older one is reachable too.
        let older = keys::search_index("main", "deployment", 1_699_999_999, &[5u8; 32]);
        assert!(older > decoded);
        // No terms, no bound to build.
        assert_eq!(until_cursor("main", &[], 1).as_deref(), None);
    }

    #[test]
    fn a_cursor_is_bound_to_the_bucket_that_produced_it() {
        // A cursor is a scan position inside ONE term's key space. Replaying it
        // against another term would resume at an arbitrary offset of an
        // unrelated bucket and silently skip results, so it is refused.
        let deploy = keys::search_index("main", "deploy", 1_700_000_000, &[3u8; 32]);
        let encoded = encode_cursor(&deploy);
        assert!(decode_cursor(&encoded, &keys::search_prefix("main", "deploy")).is_ok());
        assert!(decode_cursor(&encoded, &keys::search_prefix("main", "relay")).is_err());
        assert!(decode_cursor(&encoded, &keys::search_prefix("other", "deploy")).is_err());
        assert!(decode_cursor("not hex", &keys::search_prefix("main", "deploy")).is_err());
        // It round-trips exactly, so resuming lands where the walk stopped.
        assert_eq!(
            decode_cursor(&encoded, &keys::search_prefix("main", "deploy")).unwrap(),
            deploy
        );
    }

    #[test]
    fn an_index_key_is_determined_by_its_term_time_and_id() {
        // This is what makes AND a computed point-read rather than a merge: given
        // a candidate from one bucket, its key in every other bucket is known.
        let id = [9u8; 32];
        let a = keys::search_index("main", "deploy", 1_700_000_000, &id);
        let b = keys::search_index("main", "deploy", 1_700_000_000, &id);
        assert_eq!(a, b, "the same triple must produce the same key");
        assert_ne!(a, keys::search_index("main", "relay", 1_700_000_000, &id));
        assert_ne!(a, keys::search_index("main", "deploy", 1_700_000_001, &id));
        assert_ne!(
            a,
            keys::search_index("main", "deploy", 1_700_000_000, &[8u8; 32])
        );
        // And the tail parses back, which is how a walk yields candidates.
        assert_eq!(keys::parse_index_tail(&a), Some((1_700_000_000, id)));
    }
}
