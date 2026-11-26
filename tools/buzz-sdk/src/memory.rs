//! Durable agent memory, stored in the workspace itself.
//!
//! A memory is an event: kind [`MEMORY_KIND`] (NIP-AE "agent engram"), authored by
//! the agent, addressed to the agent's own pubkey with a `p` tag, and keyed by a
//! `d` tag derived from its text. That choice is the whole design: memory then
//! inherits the relay's durability, its audit chain, and its read gate, instead of
//! the SDK inventing a second store beside the log — which is exactly the kind of
//! reduced-service duplication this tree refuses.
//!
//! ## What that buys, and what it costs
//!
//! * **Durable and shared.** Two processes running the same agent identity see
//!   the same memories, and a restart loses nothing. There is no local file.
//! * **Idempotent writes.** The `d` tag is a hash of `(scope, text)`, so
//!   remembering the same thing twice writes the same addressable record rather
//!   than accumulating duplicates.
//! * **Recall is a scan, not an index.** The relay's read route pages timelines;
//!   engrams are community-global events, so [`Memory::recall`] pages the global
//!   timeline and filters by kind and author client-side, bounded by
//!   [`Memory::max_pages`]. That is honest but not cheap: a busy community makes
//!   recall walk unrelated traffic. The fix belongs in the relay (a by-author +
//!   by-kind read route — `buzz::relay::query::by_author` already exists
//!   internally and is simply not exposed over HTTP), not in a client-side index
//!   here.
//! * **Not private.** Engrams are **not** in the relay's `#p`-gated kind set
//!   ([`buzz::relay::kind::P_GATED_KINDS`]), so an engram in an open community is
//!   readable by that community, `p` tag or not. The SDK does not encrypt it.
//!   **Do not put secrets, credentials, or anything you would not post in a
//!   channel into a memory.** Making engram content private is a relay change
//!   (gate the kind, or NIP-44 encrypt to the agent's own key); until then this
//!   doc is the contract.
//!
//! ## Recall quality
//!
//! Scoring is [`relevance`]: prefix-tolerant word overlap against the query,
//! newest-first as a tiebreak. It is a heuristic, not a retrieval engine — no
//! embeddings, no real stemming, no ranking model. It finds "the migration review
//! note" when you ask about *migrations* (the prefix rule covers that much), and it
//! will miss a synonym entirely. Said plainly so nobody mistakes it for semantic
//! search.
//!
//! ## Why not `ferrite-memory`
//!
//! The platform owns a memory service (`ferrite-memory`: engram lifecycle,
//! embeddings, ranked recall) and a constrained persona hosted by the
//! relay reaches it through the framework's `memory` capability.
//! This module does **not** wrap it and is not a reduced copy of it. A buzz
//! memory is deliberately a *signed workspace event*: it inherits the relay's
//! audit chain, durability, and read gate, and it is visible to the community as
//! any authored record is — properties a side memory store would not have. The
//! ranking here is weak on purpose (no second index to keep consistent with the
//! log); when an agent wants real ranked recall it should run as a constrained
//! persona and use the `memory` capability, not ask this client-side scan to be
//! something it is not.

use std::collections::HashSet;
use std::sync::Mutex;

use buzz::relay::crypto::{sha256::sha256, to_hex};
use buzz::relay::kind::KIND_AGENT_ENGRAM;
use buzz::relay::{Event, EventId, Kind};

use crate::error::Result;
use crate::transport::QueryRequest;
use crate::workspace::Workspace;

/// The event kind memories are stored as: NIP-AE agent engram, parameterized
/// replaceable, agent-authored.
pub const MEMORY_KIND: Kind = KIND_AGENT_ENGRAM;

/// Default scope, used when an agent does not name one.
pub const DEFAULT_SCOPE: &str = "default";

/// Longest memory this type will write, in characters.
///
/// A memory is a note, not a document: keeping them short is what makes recall
/// affordable and what keeps a prompt from being eaten by one rambling note. The
/// relay's own event bound is far larger; this is the SDK being opinionated.
pub const MAX_MEMORY_CHARS: usize = 2_000;

/// One remembered note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryNote {
    /// The note itself.
    pub text: String,
    /// When it was written, Unix seconds.
    pub created_at: u64,
    /// The addressable key (`d` tag) — a hash of scope and text.
    pub slug: String,
    /// The event id it was stored as, so an agent can cite it.
    pub id: EventId,
}

impl MemoryNote {
    /// The event id as hex, which is how a citation is written.
    #[must_use]
    pub fn id_hex(&self) -> String {
        to_hex(&self.id)
    }
}

/// An agent's memory over one workspace.
///
/// Holds a small write-through cache of notes written in this process so that a
/// `remember` followed immediately by a `recall` sees the note even before the
/// relay's read index has caught up (ingest writes an event body before its index
/// keys, so a fresh write is briefly invisible to a page read).
pub struct Memory {
    workspace: Workspace,
    scope: String,
    recent: Mutex<Vec<MemoryNote>>,
    max_pages: usize,
    page_limit: usize,
}

impl core::fmt::Debug for Memory {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Memory")
            .field("scope", &self.scope)
            .field("cached", &self.recent.lock().map(|r| r.len()).unwrap_or(0))
            .finish_non_exhaustive()
    }
}

impl Memory {
    /// Memory for an agent, in the default scope.
    #[must_use]
    pub fn new(workspace: Workspace) -> Self {
        Self::with_scope(workspace, DEFAULT_SCOPE)
    }

    /// Memory in a named scope.
    ///
    /// The scope is mixed into every key, so two agents sharing one identity (a
    /// reviewer and a release bot run from the same keypair) do not read each
    /// other's notes by accident.
    #[must_use]
    pub fn with_scope(workspace: Workspace, scope: impl Into<String>) -> Self {
        Self {
            workspace,
            scope: scope.into(),
            recent: Mutex::new(Vec::new()),
            max_pages: 4,
            page_limit: 100,
        }
    }

    /// How many pages of history [`Memory::recall`] may walk. Default 4.
    #[must_use]
    pub const fn max_pages(mut self, pages: usize) -> Self {
        self.max_pages = pages;
        self
    }

    /// How many events per page [`Memory::recall`] asks for. Default 100.
    #[must_use]
    pub const fn page_limit(mut self, limit: usize) -> Self {
        self.page_limit = limit;
        self
    }

    /// The scope these memories live in.
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// The addressable key for a note — a hash of scope and text.
    ///
    /// Deterministic, so writing the same note twice addresses the same record
    /// rather than growing the log. Pure function; no I/O.
    #[must_use]
    pub fn slug_for(scope: &str, text: &str) -> String {
        let digest = sha256(format!("{scope}\u{1f}{text}").as_bytes());
        // 16 hex characters is 64 bits — ample for a per-agent keyspace, and short
        // enough to stay inside the relay's key-component bound.
        to_hex(&digest).chars().take(16).collect()
    }

    /// Write a memory.
    ///
    /// Returns the note, including the event id it was stored as. Text longer
    /// than [`MAX_MEMORY_CHARS`] is truncated on a character boundary rather than
    /// refused: losing the tail of a note is better than losing a turn.
    ///
    /// Cost: one signature and one publish round trip.
    ///
    /// # Errors
    ///
    /// As [`Workspace::publish`].
    pub async fn remember(&self, text: impl Into<String>) -> Result<MemoryNote> {
        let mut text = text.into();
        if text.chars().count() > MAX_MEMORY_CHARS {
            text = text.chars().take(MAX_MEMORY_CHARS).collect();
        }
        let slug = Self::slug_for(&self.scope, &text);
        let event = self.workspace.sign(
            MEMORY_KIND,
            vec![
                vec!["d".to_string(), slug.clone()],
                // Addressed to the agent itself: this is the agent's own state,
                // not a message to anyone.
                vec!["p".to_string(), self.workspace.identity().pubkey_hex()],
                vec!["scope".to_string(), self.scope.clone()],
            ],
            &text,
        )?;
        self.workspace.publish(&event).await?;

        let note = MemoryNote {
            text,
            created_at: event.created_at,
            slug,
            id: event.id,
        };
        if let Ok(mut recent) = self.recent.lock() {
            recent.retain(|n| n.slug != note.slug);
            recent.push(note.clone());
            // Bounded: the cache is a freshness patch, not a store.
            let overflow = recent.len().saturating_sub(64);
            if overflow > 0 {
                recent.drain(..overflow);
            }
        }
        Ok(note)
    }

    /// The most recent memories, newest first.
    ///
    /// Cost: up to [`Memory::max_pages`] read round trips.
    ///
    /// # Errors
    ///
    /// As [`Workspace::query`].
    pub async fn load(&self, limit: usize) -> Result<Vec<MemoryNote>> {
        let mut notes = self.stored().await?;
        notes.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        notes.truncate(limit);
        Ok(notes)
    }

    /// Recall the memories most relevant to `query`.
    ///
    /// [`relevance`] scoring, newest-first tiebreak (see the module docs on what
    /// this is and is not). An empty query degrades to [`Memory::load`].
    ///
    /// # Errors
    ///
    /// As [`Workspace::query`].
    pub async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryNote>> {
        if query.trim().is_empty() {
            return self.load(limit).await;
        }
        let wanted = tokenize(query);
        let mut scored: Vec<(usize, MemoryNote)> = self
            .stored()
            .await?
            .into_iter()
            .map(|note| (relevance(&wanted, &tokenize(&note.text)), note))
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.created_at.cmp(&a.1.created_at))
                .then_with(|| b.1.id.cmp(&a.1.id))
        });
        Ok(scored
            .into_iter()
            .filter(|(score, _)| *score > 0)
            .map(|(_, note)| note)
            .take(limit)
            .collect())
    }

    /// Every memory this agent can see: the process cache plus what the relay
    /// holds, deduplicated by slug (newest wins).
    async fn stored(&self) -> Result<Vec<MemoryNote>> {
        let mut notes: Vec<MemoryNote> = self
            .recent
            .lock()
            .map(|recent| recent.clone())
            .unwrap_or_default();

        let mut cursor: Option<String> = None;
        for _ in 0..self.max_pages.max(1) {
            let mut request = QueryRequest::new()
                .limit(self.page_limit)
                .kinds(vec![MEMORY_KIND]);
            if let Some(cursor) = cursor.clone() {
                request = request.cursor(cursor);
            }
            let page = self.workspace.query(&request).await?;
            let empty = page.events.is_empty();
            for event in &page.events {
                if let Some(note) = self.note_from(event) {
                    notes.push(note);
                }
            }
            cursor = page.next_cursor.clone();
            if page.complete || cursor.is_none() || empty {
                break;
            }
        }

        // Newest wins per slug: a rewritten memory must not surface twice.
        notes.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        let mut seen: HashSet<String> = HashSet::new();
        notes.retain(|note| seen.insert(note.slug.clone()));
        Ok(notes)
    }

    /// Read one event as a memory, if it is one of this agent's, in this scope.
    ///
    /// Author and scope are both checked: the kind alone is not enough, because
    /// engrams are world-readable and another agent's notes will appear in the
    /// same global timeline page.
    fn note_from(&self, event: &Event) -> Option<MemoryNote> {
        if event.kind != MEMORY_KIND || event.pubkey != self.workspace.pubkey() {
            return None;
        }
        if event.first_tag("scope").unwrap_or(DEFAULT_SCOPE) != self.scope {
            return None;
        }
        Some(MemoryNote {
            text: event.content.clone(),
            created_at: event.created_at,
            slug: event
                .d_tag()
                .map(str::to_string)
                .unwrap_or_else(|| Self::slug_for(&self.scope, &event.content)),
            id: event.id,
        })
    }
}

/// Lowercase word tokens, longer than two characters.
///
/// The two-character floor drops "a", "of", "to" — the words that would otherwise
/// make every note match every query. Public because it is half of the SDK's one
/// relevance heuristic, and a caller who wants to score something else should not
/// have to reimplement it.
#[must_use]
pub fn tokenize(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() > 2)
        .map(str::to_lowercase)
        .collect()
}

/// Shortest token that may match by prefix.
///
/// Four characters: enough that "migration"/"migrations" and "review"/"reviews"
/// match, short enough that "rel" does not match "release" and "relay" both.
pub const MIN_PREFIX_MATCH: usize = 4;

/// How many query tokens a text matches — the SDK's one relevance score.
///
/// Shared by [`Memory::recall`] and [`crate::turn::SearchBuilder`] so "relevant"
/// means the same thing in both places. A query token matches when it equals a
/// text token, or when one is a prefix of the other and the shorter is at least
/// [`MIN_PREFIX_MATCH`] characters — a crude stand-in for stemming that costs
/// nothing and catches plurals. Nothing here understands synonyms.
#[must_use]
pub fn relevance(query: &HashSet<String>, text: &HashSet<String>) -> usize {
    query
        .iter()
        .filter(|wanted| {
            text.iter().any(|word| {
                word == *wanted
                    || (wanted.chars().count() >= MIN_PREFIX_MATCH
                        && word.chars().count() >= MIN_PREFIX_MATCH
                        && (word.starts_with(wanted.as_str()) || wanted.starts_with(word)))
            })
        })
        .count()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use crate::testing::MemoryTransport;
    use crate::workspace::{FixedClock, Workspace};
    use std::sync::Arc;

    const SK: [u8; 32] = [
        0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0x87, 0x96, 0xa5, 0xb4, 0xc3, 0xd2, 0xe1,
        0xf0, 0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0x87, 0x96, 0xa5, 0xb4, 0xc3, 0xd2,
        0xe1, 0xf1,
    ];

    async fn harness() -> (Arc<MemoryTransport>, Workspace, FixedClock) {
        let transport = Arc::new(MemoryTransport::new());
        let clock = FixedClock::new(1_700_000_000_000);
        let ws = Workspace::builder("")
            .identity(Identity::from_secret(SK).unwrap())
            .transport(Arc::clone(&transport) as Arc<dyn crate::Transport>)
            .clock(Arc::new(clock.clone()))
            .connect()
            .await
            .unwrap();
        (transport, ws, clock)
    }

    #[tokio::test]
    async fn remembering_writes_an_addressed_engram() {
        let (transport, ws, _clock) = harness().await;
        let memory = Memory::new(ws.clone());
        let note = memory
            .remember("migrations need a rollback plan")
            .await
            .unwrap();

        let events = transport.published();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        event.verify().unwrap();
        assert_eq!(event.kind, MEMORY_KIND);
        assert_eq!(event.d_tag(), Some(note.slug.as_str()));
        assert_eq!(
            event.first_tag("p"),
            Some(ws.identity().pubkey_hex().as_str()),
            "a memory is addressed to the agent itself"
        );
        assert_eq!(event.first_tag("scope"), Some("default"));
        assert_eq!(event.channel_id(), None, "memories are not channel traffic");
        assert_eq!(note.id_hex(), to_hex(&event.id));
    }

    #[tokio::test]
    async fn the_same_note_twice_addresses_one_record() {
        let (transport, ws, _clock) = harness().await;
        let memory = Memory::new(ws);
        let first = memory.remember("prefer squash merges").await.unwrap();
        let second = memory.remember("prefer squash merges").await.unwrap();
        assert_eq!(first.slug, second.slug);
        assert_eq!(
            transport.published().len(),
            1,
            "an identical memory is the same event, absorbed as a duplicate"
        );
        assert_eq!(memory.load(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn recall_finds_by_word_overlap_and_ignores_noise() {
        let (_transport, ws, clock) = harness().await;
        let memory = Memory::new(ws);
        memory
            .remember("migration reviews need a rollback plan")
            .await
            .unwrap();
        clock.advance_ms(1_000);
        memory
            .remember("the deploy pipeline runs on fridays")
            .await
            .unwrap();
        clock.advance_ms(1_000);
        memory
            .remember("another migration touched the users table")
            .await
            .unwrap();

        let hits = memory.recall("migration review", 5).await.unwrap();
        assert_eq!(hits.len(), 2, "only the migration notes match");
        assert!(hits[0].text.contains("migration"));

        assert!(
            memory.recall("kubernetes", 5).await.unwrap().is_empty(),
            "no overlap means no hits, not a random note"
        );

        // An empty query is a recency read.
        let recent = memory.recall("   ", 2).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert!(recent[0].text.contains("users table"), "newest first");
    }

    #[tokio::test]
    async fn recall_reads_memories_written_before_this_process_started() {
        let (transport, ws, _clock) = harness().await;
        // A memory from a previous run of the same agent.
        let earlier = Event::sign_new(
            &SK,
            MEMORY_KIND,
            vec![
                vec!["d".into(), "abc123".into()],
                vec!["scope".into(), "default".into()],
            ],
            "the release checklist lives in RELEASING.md",
            1_600_000_000,
        )
        .unwrap();
        transport.seed(earlier);

        let memory = Memory::new(ws);
        let hits = memory.recall("release checklist", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "abc123");
    }

    #[tokio::test]
    async fn another_agents_engrams_and_other_scopes_are_not_mine() {
        let (transport, ws, _clock) = harness().await;
        transport.seed(
            Event::sign_new(
                &[0x77u8; 32],
                MEMORY_KIND,
                vec![vec!["d".into(), "theirs".into()]],
                "someone else remembered migrations",
                1_600_000_000,
            )
            .unwrap(),
        );
        transport.seed(
            Event::sign_new(
                &SK,
                MEMORY_KIND,
                vec![
                    vec!["d".into(), "other-scope".into()],
                    vec!["scope".into(), "release-bot".into()],
                ],
                "migrations for the release bot",
                1_600_000_001,
            )
            .unwrap(),
        );

        let memory = Memory::new(ws.clone());
        assert!(
            memory.recall("migrations", 5).await.unwrap().is_empty(),
            "author and scope both gate recall"
        );

        let scoped = Memory::with_scope(ws, "release-bot");
        assert_eq!(scoped.recall("migrations", 5).await.unwrap().len(), 1);
    }

    #[test]
    fn slugs_are_deterministic_scoped_and_bounded() {
        let a = Memory::slug_for("default", "same text");
        assert_eq!(a, Memory::slug_for("default", "same text"));
        assert_ne!(a, Memory::slug_for("other", "same text"));
        assert_ne!(a, Memory::slug_for("default", "different text"));
        assert_eq!(a.chars().count(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(
            buzz::relay::keys::is_safe_component(&a),
            "a slug must be usable as a store key component"
        );
    }

    #[tokio::test]
    async fn overlong_memories_are_truncated_not_refused() {
        let (_transport, ws, _clock) = harness().await;
        let memory = Memory::new(ws);
        let note = memory
            .remember("x".repeat(MAX_MEMORY_CHARS * 2))
            .await
            .unwrap();
        assert_eq!(note.text.chars().count(), MAX_MEMORY_CHARS);
    }

    #[test]
    fn tokenizing_drops_short_words_and_case() {
        let tokens = tokenize("The Migration, of a Table!");
        assert!(tokens.contains("migration"));
        assert!(tokens.contains("table"));
        assert!(!tokens.contains("of"), "stopword-length words are dropped");
        assert_eq!(relevance(&tokenize("migration"), &tokens), 1);
    }

    #[test]
    fn relevance_tolerates_plurals_but_not_short_prefixes() {
        let note = tokenize("migrations always need a rollback plan");
        assert_eq!(
            relevance(&tokenize("migration rule"), &note),
            1,
            "singular matches plural"
        );
        assert_eq!(relevance(&tokenize("rollbacks"), &note), 1);
        assert_eq!(
            relevance(&tokenize("mig"), &note),
            0,
            "a three-character token is below the prefix floor"
        );
        assert_eq!(
            relevance(&tokenize("kubernetes"), &note),
            0,
            "no synonyms, no fuzz"
        );
    }
}
