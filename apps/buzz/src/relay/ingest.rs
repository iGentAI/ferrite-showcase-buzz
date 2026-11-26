//! Ingest: the one path by which an event enters the workspace.
//!
//! Buzz did this inside a Postgres transaction. There is no transaction here and
//! no single writer: several instances of this app consume the same request
//! queue, and any request can be redelivered. So admission is a **guarded saga**:
//!
//! 1. **Elect one writer.** A single APE `commit_n_if` on `admit/{c}/{id}` with
//!    `expected = None` creates the admission marker and the saga document
//!    together. Exactly one caller wins; everyone else learns the event is
//!    already admitted and gets the winner's document.
//! 2. **Apply effects, each self-recognizing.** The event record is written
//!    before any index key, so a reader that finds an index entry without its
//!    event skips it rather than serving a half-written row (the Store has no
//!    *atomic* multi-key write — `put_batch` is a throughput batch that can
//!    partially commit). The audit append carries a per-event marker inside the
//!    same envelope as the chain entry, so re-entry after a crash cannot append a
//!    second entry for one event.
//! 3. **Finish explicitly.** A final guarded commit flips the saga to `Complete`.
//!    Until then the triggering queue envelope stays unacked, so redelivery is
//!    the resume driver.
//! 4. **Duplicates drive, never wait.** A resubmit of an event whose saga is
//!    incomplete finishes that saga before replying, so `duplicate:` can never
//!    mean "accepted" for an event that was never actually delivered.
//!
//! One thing this module deliberately does *not* rely on: redelivery being
//! infinite. The queue dead-letters after its attempt budget, so a saga that
//! keeps failing eventually stops being retried by the queue — the repair sweep
//! ([`needs_repair`]) is what closes that hole, by finding admitted-but-incomplete
//! sagas and driving them.

use ferrite_app::tracing;
use ferrite_app::client::{ApeClusterClient, StoreClusterClient, TopicLogClient};
use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json;

use crate::relay::audit::{AuditAction, AuditDraft};
use crate::relay::crypto::to_hex;
use crate::relay::delivery::FeedRecord;
use crate::relay::event::Event;
use crate::relay::keys;
use crate::relay::kind;
use crate::relay::{BuzzError, Result};

/// How many times a guarded commit is retried when it loses its race. Losing
/// means another instance moved the guard, so the loser re-reads and tries again;
/// this bounds a pathological ping-pong rather than a normal contention case.
pub const GUARD_RETRIES: u32 = 16;

/// Where a saga has got to. The states are coarse on purpose: every effect is
/// idempotent, so re-running a step is always safe and the state exists to bound
/// *work*, not to make effects correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", rename_all = "snake_case")]
pub enum SagaState {
    /// Admitted; effects may or may not have been applied.
    Admitted,
    /// Every effect applied and observed.
    Complete,
}

/// The admission marker and saga record for one event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Admission {
    /// Hex event id — the identity this saga is keyed by.
    pub event_id: String,
    pub community: String,
    /// The channel the event resolved to, which for reactions and deletions is
    /// derived from the target event rather than from a client-supplied tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    pub admitted_at_ms: u64,
    pub state: SagaState,
    /// The audit sequence this event was recorded at, once it has been.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_seq: Option<u64>,
    /// When a NIP-09 deletion retired this event ([`tombstone`]).
    ///
    /// The admission document is the ONE row every writer of this event id
    /// already reads, which is why the tombstone lives here rather than under a
    /// key of its own: a re-driven saga sees it for free (no extra read on the
    /// hot path), and a driver that is mid-flight when the deletion lands loses
    /// its completion CAS — because that CAS guards on these very bytes — and so
    /// must re-read and converge instead of silently re-creating the rows the
    /// deletion just removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at_ms: Option<u64>,
}

impl Admission {
    #[must_use]
    pub fn new(event: &Event, community: &str, channel: Option<String>, now_ms: u64) -> Self {
        Self {
            event_id: to_hex(&event.id),
            community: community.to_string(),
            channel,
            admitted_at_ms: now_ms,
            state: SagaState::Admitted,
            audit_seq: None,
            deleted_at_ms: None,
        }
    }

    #[must_use]
    pub const fn is_complete(&self) -> bool {
        matches!(self.state, SagaState::Complete)
    }

    /// Whether a NIP-09 deletion has retired this event. A tombstoned saga
    /// applies no effect, stores nothing, and delivers nothing — it only
    /// finishes erasing.
    #[must_use]
    pub const fn is_deleted(&self) -> bool {
        self.deleted_at_ms.is_some()
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|e| BuzzError::Internal(format!("admission encode: {e}")))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes)
            .map_err(|e| BuzzError::Internal(format!("admission decode: {e}")))
    }
}

/// The result of trying to admit an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitOutcome {
    /// This caller won the election and must drive the saga.
    Admitted { doc: Admission, raw: Vec<u8> },
    /// Already admitted. The caller must drive the saga if it is incomplete, then
    /// answer `duplicate:` — a duplicate reply is only honest once the first
    /// admission's effects are actually visible.
    Duplicate { doc: Admission, raw: Vec<u8> },
}

impl AdmitOutcome {
    #[must_use]
    pub const fn doc(&self) -> &Admission {
        match self {
            Self::Admitted { doc, .. } | Self::Duplicate { doc, .. } => doc,
        }
    }

    #[must_use]
    pub fn raw(&self) -> &[u8] {
        match self {
            Self::Admitted { raw, .. } | Self::Duplicate { raw, .. } => raw,
        }
    }

    #[must_use]
    pub const fn was_duplicate(&self) -> bool {
        matches!(self, Self::Duplicate { .. })
    }
}

/// Whether a saga found in the store still needs work. Used by both the
/// duplicate path and the repair sweep.
#[must_use]
pub const fn needs_repair(doc: &Admission) -> bool {
    !doc.is_complete()
}

/// Retire one event id: mark its admission document deleted so no writer of
/// that id can ever store, materialize, or deliver it again (NIP-09).
///
/// This is the resurrection guard the deletion path needs, and it is a
/// *guarded* write on purpose:
///
/// * a saga that is **mid-flight** when the deletion lands holds the pre-
///   tombstone bytes and completes with `expected = those bytes`; this write
///   invalidates that CAS, so the driver fails, redelivery re-reads, and the
///   re-read sees the tombstone and erases instead of finishing;
/// * a saga that is **re-driven later** (a torn saga, the repair sweep, a
///   client re-publishing the same event) reads the tombstone at entry and
///   erases;
/// * an event this deployment never admitted gets a tombstone document
///   anyway (`Complete` + deleted), which is what makes a later arrival of the
///   same id a no-op rather than a resurrection.
///
/// Returns the channel the admission document records — the scope this event's
/// index rows were written under, which is the one thing an erase cannot
/// re-derive for a reaction or a deletion (their scope came from their target).
///
/// Callers must have already established that the deletion is AUTHORIZED for
/// this id — a tombstone is permanent, so a caller that cannot prove authorship
/// must not call this.
pub async fn tombstone(
    ape: &mut ApeClusterClient,
    community: &str,
    id: &[u8; 32],
    now_ms: u64,
) -> Result<Option<String>> {
    let key = keys::admit(community, id);
    for _ in 0..GUARD_RETRIES {
        let current = ape
            .get(&key)
            .await
            .map_err(|e| BuzzError::Storage(format!("tombstone read: {e}")))?;
        let mut doc = match current.as_deref() {
            Some(bytes) => {
                let doc = Admission::decode(bytes)?;
                if doc.is_deleted() {
                    return Ok(doc.channel);
                }
                doc
            }
            // Never admitted here (or the marker is gone): a synthetic,
            // already-complete document blocks the id outright.
            None => Admission {
                event_id: to_hex(id),
                community: community.to_string(),
                channel: None,
                admitted_at_ms: now_ms,
                state: SagaState::Complete,
                audit_seq: None,
                deleted_at_ms: None,
            },
        };
        doc.deleted_at_ms = Some(now_ms);
        let channel = doc.channel.clone();
        let next = doc.encode()?;
        match ape
            .commit_n_if(
                &key,
                current.as_deref(),
                &[(key.as_slice(), next.as_slice())],
            )
            .await
            .map_err(|e| BuzzError::Storage(format!("tombstone commit: {e}")))?
        {
            Ok(()) => return Ok(channel),
            Err(_) => continue, // a driver moved the document; re-read and re-mark
        }
    }
    Err(BuzzError::Conflict("tombstone contended".into()))
}

/// Elect a writer for `event`.
///
/// The guard is the admission key itself with `expected = None`: a create-if-absent
/// that only one caller can win. This is the primitive the platform actually
/// provides for this — the SDK's saga helper commits *unguarded* and documents
/// that two concurrent beginners can both admit, which is precisely the race a
/// competing-consumer app must not have.
/// Atomically verify or claim workflow ID ownership on the `guards` APE plane
/// and against any pre-existing `wfdef/` index row on `store`, together with
/// event admission (`admit/{community}/{event_id}`).
///
/// For the first definition of `(community, channel, wf_id)` when no `wfown`
/// guard exists yet, any older `wfdef` index row is verified first (backfilling
/// `wfown` to that row's owner if another author is attempting replacement), and
/// then `wfown/{c}/{ch}/{wf}` and `admit/{c}/{event_id}` are committed in one
/// `commit_n_if` envelope guarded on `wfown == None` (or `wfown` is claimed for
/// an existing `admit/` record that has no guard before returning `Duplicate`).
pub async fn admit_workflow_definition(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    event: &Event,
    channel: Option<String>,
    now_ms: u64,
) -> Result<AdmitOutcome> {
    let (ch, wf_id) = crate::relay::workflows::definition_coordinates(channel.as_deref(), event)?;
    let key = keys::admit(community, &event.id);
    let doc = Admission::new(event, community, channel.clone(), now_ms);
    let raw = doc.encode()?;
    let owner_guard = keys::workflow_owner(community, ch, wf_id);
    let author_hex = to_hex(&event.pubkey);

    for _ in 0..GUARD_RETRIES {
        crate::relay::workflows::verify_existing_index_owner(
            store,
            ape,
            community,
            ch,
            wf_id,
            &event.pubkey,
        )
        .await?;
        let current_owner = ape
            .get(&owner_guard)
            .await
            .map_err(|e| BuzzError::Storage(format!("workflow owner read: {e}")))?;
        if let Some(owner_bytes) = current_owner {
            if owner_bytes.as_slice() != author_hex.as_bytes() {
                return Err(BuzzError::Unauthorized(
                    "workflow id in this channel is already owned by another author".into(),
                ));
            }
            return admit_by_key(ape, &key, doc, raw).await;
        }
        if let Some(existing_admit) = ape
            .get(&key)
            .await
            .map_err(|e| BuzzError::Storage(format!("admission read: {e}")))?
        {
            let existing_doc = Admission::decode(&existing_admit)?;
            // Claim the ownership guard without rewriting `key` so a
            // pre-upgrade admission preserves its saga progress while
            // participating in exclusive workflow ID ownership.
            match ape
                .commit_n_if(
                    &owner_guard,
                    None,
                    &[(owner_guard.as_slice(), author_hex.as_bytes())],
                )
                .await
                .map_err(|e| {
                    BuzzError::Storage(format!("workflow owner migration commit: {e}"))
                })? {
                Ok(()) => {
                    return Ok(AdmitOutcome::Duplicate {
                        doc: existing_doc,
                        raw: existing_admit,
                    });
                }
                Err(_) => continue,
            }
        }
        match ape
            .commit_n_if(
                &owner_guard,
                None,
                &[
                    (owner_guard.as_slice(), author_hex.as_bytes()),
                    (key.as_slice(), raw.as_slice()),
                ],
            )
            .await
            .map_err(|e| BuzzError::Storage(format!("workflow admission commit: {e}")))?
        {
            Ok(()) => return Ok(AdmitOutcome::Admitted { doc, raw }),
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict(
        "workflow ownership claim contended beyond the retry budget".into(),
    ))
}

/// Elect a writer for `event`.
///
/// The guard is the admission key itself with `expected = None`: a create-if-absent
/// that only one caller can win. For `KIND_WORKFLOW_DEF`, callers that may be
/// admitting the first definition or an older workflow must use
/// [`admit_workflow_definition`] (which inspects `wfdef` on `store`); calling
/// `admit` directly on `KIND_WORKFLOW_DEF` requires `wfown` to already be
/// established on `ape` for `event.pubkey` and refuses otherwise so an absent
/// guard can never be claimed without checking the owner in an older `wfdef` row.
pub async fn admit(
    ape: &mut ApeClusterClient,
    community: &str,
    event: &Event,
    channel: Option<String>,
    now_ms: u64,
) -> Result<AdmitOutcome> {
    let key = keys::admit(community, &event.id);
    let doc = Admission::new(event, community, channel.clone(), now_ms);
    let raw = doc.encode()?;

    if event.kind == kind::KIND_WORKFLOW_DEF {
        return Err(BuzzError::Unauthorized(
            "workflow definition admission requires store-backed ownership verification via admit_workflow_definition"
                .into(),
        ));
    }

    admit_by_key(ape, &key, doc, raw).await
}

async fn admit_by_key(
    ape: &mut ApeClusterClient,
    key: &[u8],
    doc: Admission,
    raw: Vec<u8>,
) -> Result<AdmitOutcome> {
    for _ in 0..GUARD_RETRIES {
        match ape
            .commit_n_if(key, None, &[(key, raw.as_slice())])
            .await
            .map_err(|e| BuzzError::Storage(format!("admission commit: {e}")))?
        {
            Ok(()) => return Ok(AdmitOutcome::Admitted { doc, raw }),
            Err(Some(existing)) => {
                let doc = Admission::decode(&existing)?;
                return Ok(AdmitOutcome::Duplicate { doc, raw: existing });
            }
            // The guard was reported absent yet the commit did not apply: another
            // caller landed between our read and our write. Re-read to find out who.
            Err(None) => {
                if let Some(existing) = ape
                    .get(key)
                    .await
                    .map_err(|e| BuzzError::Storage(format!("admission re-read: {e}")))?
                {
                    let doc = Admission::decode(&existing)?;
                    return Ok(AdmitOutcome::Duplicate { doc, raw: existing });
                }
            }
        }
    }
    Err(BuzzError::Conflict("admission contended".into()))
}

/// Write the event record and every index key that serves a read path.
///
/// Order is load-bearing: `evt/` first, indexes after. Every write is
/// value-idempotent, so a redelivered saga rewrites identical bytes.
/// Write the event body and its index keys. Returns `true` when the event body
/// was *newly* stored (its `evt/` key did not exist).
///
/// Informational only. It must not gate an effect: the body write is the saga's
/// first durable write and is not atomic with anything after it, so a saga that
/// tore after it re-drives with `false` even though nothing else was applied.
pub async fn apply_storage(
    store: &mut StoreClusterClient,
    community: &str,
    event: &Event,
    channel: Option<&str>,
) -> Result<bool> {
    let body =
        serde_json::to_vec(event).map_err(|e| BuzzError::Internal(format!("event encode: {e}")))?;
    // Create-if-absent, so the outcome tells us whether this is the first time the
    // event has been stored. A lost response to an applied create reports the
    // caller's own version as the mismatch, so `Err(_)` is treated as "already
    // present" — the safe direction for an idempotency gate (skip re-application).
    let evt_key = keys::evt(community, &event.id);
    let was_new = store
        .put_if_version(&evt_key, &body, None)
        .await
        .map_err(|e| BuzzError::Storage(format!("event put: {e}")))?
        .is_ok();

    // Index values are empty: the key *is* the index entry, and the event body is
    // fetched by id. That keeps an index rewrite trivially idempotent and keeps
    // one copy of the event.
    let index_keys = index_keys_for(community, event, channel);
    let entries: Vec<(&[u8], &[u8])> = index_keys.iter().map(|k| (k.as_slice(), &[][..])).collect();
    for chunk in entries.chunks(256) {
        store
            .put_batch(chunk)
            .await
            .map_err(|e| BuzzError::Storage(format!("index put: {e}")))?;
    }
    Ok(was_new)
}

/// The most distinct `e` values, and separately `p` values, one event indexes.
/// Large enough for a relay roster or a long thread reply chain, small enough
/// that one admitted event cannot fan out into tens of thousands of writes.
pub const MAX_INDEXED_VALUES_PER_TAG: usize = 1024;

/// Every index key an event contributes.
#[must_use]
pub fn index_keys_for(community: &str, event: &Event, channel: Option<&str>) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(8);
    out.push(keys::ts_index(
        community,
        channel,
        event.created_at,
        &event.id,
    ));
    out.push(keys::kind_index(
        community,
        event.kind,
        event.created_at,
        &event.id,
    ));
    out.push(keys::author_index(
        community,
        &event.pubkey,
        event.kind,
        event.created_at,
        &event.id,
    ));
    // Each distinct target is indexed once, and at most
    // MAX_INDEXED_VALUES_PER_TAG of them: one signed event can carry thousands
    // of duplicate or distinct `e`/`p` tags, and each would otherwise be its own
    // index write for the price of one message.
    let mut seen = std::collections::BTreeSet::new();
    for target in event.tag_values("e").filter(|t| seen.insert(*t)).take(MAX_INDEXED_VALUES_PER_TAG) {
        out.push(keys::etag_index(
            community,
            target,
            event.created_at,
            &event.id,
        ));
    }
    if !matches!(event.kind, kind::KIND_REPORT | kind::KIND_PRODUCT_FEEDBACK) {
        let mut seen = std::collections::BTreeSet::new();
        for who in event.tag_values("p").filter(|t| seen.insert(*t)).take(MAX_INDEXED_VALUES_PER_TAG) {
            out.push(keys::mention_index(
                community,
                who,
                event.created_at,
                &event.id,
            ));
        }
    }
    // Case variants of one target canonicalize to the same key: keep the first.
    let mut unique = std::collections::HashSet::new();
    out.retain(|key| unique.insert(key.clone()));
    // The full-text term index. Empty for every kind a person would not search,
    // which is what confines this feature's write amplification to messages and
    // forum text — see `search::MAX_TERMS_PER_EVENT` for the bound it carries.
    out.extend(crate::relay::search::index_keys_for(community, event));
    out
}

/// Append the audit entry for an admitted event, exactly once.
///
/// The per-event marker (`auditref/{c}/{id}`) is committed in the *same* envelope
/// as the chain entry and the head bump. That is what makes re-entry safe: a
/// resumed saga sees its own marker and skips, instead of appending a second
/// entry for the same event or wedging on a guard its own earlier attempt moved.
///
/// The mechanics live in [`crate::relay::audit::append`], which the privileged
/// routes share: how a tamper-evident chain grows is one decision, not two.
pub async fn append_audit(
    ape: &mut ApeClusterClient,
    community: &str,
    event: &Event,
    now_ms: u64,
) -> Result<u64> {
    let draft = AuditDraft::new(community, AuditAction::EventCreated, now_ms)
        .actor(&event.pubkey)
        .event(&event.id)
        .with("kind", event.kind.to_string());
    crate::relay::audit::append(
        ape,
        community,
        draft,
        Some(&keys::auditref(community, &event.id)),
    )
    .await
}

/// Publish the event to the live delivery feed and the changefeed.
///
/// Both are at-least-once by nature — the topic client retries an append whose
/// response was lost — so consumers dedup by event id. Ephemeral events ride the
/// feed but are never stored, searched, or audited, exactly as in buzz.
pub async fn publish_feed(
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    event: &Event,
    channel: Option<String>,
) -> Result<u64> {
    let record = FeedRecord::new(community.to_string(), channel, event.clone());
    let payload = record.encode()?;
    let topic = keys::feed_topic(community, generation);
    let offset = append_creating(topics, &topic, &event.id, &payload).await?;

    if !kind::is_ephemeral(event.kind) {
        let changefeed = keys::changefeed_topic(community);
        append_creating(topics, &changefeed, &event.id, &payload).await?;
    }
    Ok(offset)
}

/// Append to `topic`, creating it if the platform reports it absent.
///
/// A topic is **not** created by the deployment: a topics binding carries
/// `LogAdmin` precisely so an app can create one on demand through the conductor
/// manifest, and nothing else in the fleet knows the names this app derives from
/// its community and feed generation. Without this, every append — and therefore
/// every event submission — fails `NotFound` on a fresh deployment.
///
/// This is the *self-healing* half of a two-part rule; the other half is
/// `ingress::serve`, which prepares the current generation's topics at boot so the
/// manifest round trip is off the request path. Keeping the lazy half is what makes
/// a feed **rotation** work: the generation is in the topic name, so a bump
/// introduces a topic while the app is running, and a boot-only create would fail
/// every append until a restart.
///
/// The create is idempotent at the manifest authority and is only reached by the
/// one append that discovers the absence, so the steady state stays at one round
/// trip and a create that races another instance's is harmless — both callers then
/// append to the same topic.
async fn append_creating(
    topics: &mut TopicLogClient,
    topic: &str,
    key: &[u8],
    payload: &[u8],
) -> Result<u64> {
    match topics.append(topic, key, payload).await {
        Ok(offset) => Ok(offset),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            topics
                .create_topic(topic)
                .await
                .map_err(|e| BuzzError::Storage(format!("feed topic {topic} create: {e}")))?;
            topics
                .append(topic, key, payload)
                .await
                .map_err(|e| BuzzError::Storage(format!("feed append {topic}: {e}")))
        }
        Err(e) => Err(BuzzError::Storage(format!("feed append {topic}: {e}"))),
    }
}

/// How many feed generations one retirement call may delete. A generation is
/// one whole-topic `Delete`, so this bounds a mistake as much as it bounds the
/// work: an operator who has skipped many rotations retires them a few at a
/// time and can see each answer.
pub const MAX_RETIRED_GENERATIONS: usize = 8;

/// The feed topics of `community` that generation `live` has superseded, oldest
/// first and at most [`MAX_RETIRED_GENERATIONS`] of them.
///
/// STRICTLY OLDER, and that is the whole safety argument: this app appends to
/// and reads from exactly one generation, so a topic below the live one is
/// unreachable by every code path in the binary that is asking. The live
/// generation and anything above it — a deployment mid-rollout has both — are
/// never returned, which is the same ordering rule the SQL app's chunk sweep
/// follows: retire only what a flip has already superseded.
///
/// Pure, because the alternative is deciding what to delete inside the call
/// that deletes it.
#[must_use]
pub fn superseded_feed_generations(topics: &[String], community: &str, live: u32) -> Vec<String> {
    let mut found: Vec<(u32, String)> = topics
        .iter()
        .filter_map(|name| {
            keys::feed_generation_of(name, community)
                .filter(|g| *g < live)
                .map(|g| (g, name.clone()))
        })
        .collect();
    found.sort_unstable();
    found.truncate(MAX_RETIRED_GENERATIONS);
    found.into_iter().map(|(_, name)| name).collect()
}

/// Delete the feed topics `live` has superseded, and answer with what went.
///
/// This is buzz's ONLY lever over feed size: the log plane carries no retention
/// declaration, no prefix trim and no per-record erasure (a known
/// platform gap in the log lifecycle), so a whole-topic `Delete` of a superseded generation is the one
/// disposal the platform admits. Rotating the live generation is a separate,
/// coarser act — see `ingress::FEED_GENERATION`.
///
/// `dry_run` plans without deleting, because "which topics would go" is the
/// question an operator asks first about an irreversible operation.
pub async fn retire_superseded_feeds(
    topics: &mut TopicLogClient,
    community: &str,
    live: u32,
    dry_run: bool,
) -> Result<Vec<String>> {
    let listed = topics
        .list_topics()
        .await
        .map_err(|e| BuzzError::Storage(format!("feed topic list: {e}")))?;
    let names: Vec<String> = listed.into_iter().map(|(name, _, _)| name).collect();
    let plan = superseded_feed_generations(&names, community, live);
    if dry_run {
        return Ok(plan);
    }
    let mut retired = Vec::new();
    for topic in plan {
        match topics.delete_topic(&topic).await {
            // `false` is "already absent", which is the same end state.
            Ok(_) => retired.push(topic),
            Err(e) => {
                // Stop at the first refusal rather than reporting a partial
                // sweep as a whole one; the operator re-runs.
                tracing::warn!("feed topic {topic} was not retired: {e}");
                break;
            }
        }
    }
    Ok(retired)
}

/// Flip the saga to `Complete` under a guard on its own document.
///
/// `expected` must be the bytes the document currently holds; passing anything
/// else fails closed rather than overwriting a concurrent driver's progress.
pub async fn complete(
    ape: &mut ApeClusterClient,
    community: &str,
    doc: &Admission,
    expected: &[u8],
    audit_seq: Option<u64>,
) -> Result<()> {
    let key = keys::admit(community, &decode_id(&doc.event_id)?);
    let mut done = doc.clone();
    done.state = SagaState::Complete;
    done.audit_seq = audit_seq.or(doc.audit_seq);
    let bytes = done.encode()?;

    match ape
        .commit_n_if(&key, Some(expected), &[(key.as_slice(), bytes.as_slice())])
        .await
        .map_err(|e| BuzzError::Storage(format!("saga completion: {e}")))?
    {
        Ok(()) => Ok(()),
        Err(current) => {
            // Someone else completed it. That is a fine outcome — the effects are
            // applied either way — but only if they really did complete it and did
            // not tombstone it while we were mid-flight.
            let already_complete = current
                .as_deref()
                .and_then(|b| Admission::decode(b).ok())
                .is_some_and(|d| d.is_complete() && !d.is_deleted() && doc.is_deleted() == d.is_deleted());
            if already_complete {
                Ok(())
            } else {
                Err(BuzzError::Conflict("saga completion contended".into()))
            }
        }
    }
}

fn decode_id(hex: &str) -> Result<[u8; 32]> {
    crate::relay::crypto::from_hex32(hex)
        .ok_or_else(|| BuzzError::Internal("admission event id is not 32-byte hex".into()))
}

/// Drive an admitted saga to completion.
///
/// The parameter list is wide because a saga touches three planes plus its own
/// identity and clock; bundling them into a struct would hide which plane each
/// step uses, which is the one thing a reader of a saga needs to see.
#[allow(clippy::too_many_arguments)]
///
/// Safe to call any number of times for the same event, from any instance: every
/// step is idempotent and self-recognizing. Returns the audit sequence the event
/// was recorded at.
///
/// `relay_seckey` is the relay's OWN signing identity, used only by the
/// relay-signed NIP-29 projection step. `None` disables that step, which is what
/// a deployment with no configured identity gets.
pub async fn drive(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    event: &Event,
    outcome: &AdmitOutcome,
    now_ms: u64,
    relay_seckey: Option<&[u8; 32]>,
) -> Result<Option<u64>> {
    let doc = outcome.doc();
    // Checked BEFORE `is_complete`, so a deletion that crashed between the
    // tombstone and the row removal is finished by the next driver of this id
    // rather than left half-erased.
    if doc.is_deleted() {
        return erase_tombstoned(store, ape, community, doc, outcome.raw()).await;
    }
    let channel = doc.channel.clone();
    if event.kind == kind::KIND_WORKFLOW_DEF {
        crate::relay::workflows::claim_or_verify_owner(
            store,
            ape,
            community,
            channel.as_deref(),
            event,
        )
        .await?;
    }
    if doc.is_complete() {
        return Ok(doc.audit_seq);
    }
    let audit_seq = apply_all(
        store,
        ape,
        topics,
        community,
        generation,
        event,
        channel.as_deref(),
        now_ms,
    )
    .await?;

    // The relay-signed NIP-29 projections (39000/39001/39002) a real client
    // builds its channel list and roster from. Deliberately placed here:
    //
    // * AFTER the feed append, so the event a member actually submitted reaches
    //   subscribers before the relay's derived view of it;
    // * NOT gated on the event being new, because a saga that
    //   tore between storage and completion must still get its roster out — the
    //   step recognizes an already-published projection and costs one read;
    // * best-effort inside, so a projection failure can never wedge a saga whose
    //   state change is already committed.
    crate::relay::effects::emit_group_projections(
        store,
        ape,
        topics,
        community,
        generation,
        event,
        channel.as_deref(),
        relay_seckey,
        now_ms,
    )
    .await?;

    // The relay-signed membership notices (44100/44101) an added or removed
    // participant learns from. Placed after the projections for the same reason
    // those come after the feed append — the derived views go out in the order a
    // client consumes them — and stamped with the trigger's ADMISSION instant
    // rather than `now_ms`, because that is the one clock reading every driver of
    // this saga agrees on, which is what makes the minted notice's id
    // deterministic and therefore minted exactly once.
    crate::relay::effects::emit_membership_notices(
        store,
        ape,
        topics,
        community,
        generation,
        event,
        channel.as_deref(),
        relay_seckey,
        doc.admitted_at_ms / 1000,
    )
    .await?;

    // NIP-IA identity archiving (9035/9036): materialize the row, then publish
    // the notice and the refreshed 13535 list. Placed with the other
    // relay-signed steps and for the same reasons — after the feed append, so a
    // member's own request reaches subscribers before the relay's derived view
    // of it, and best-effort INSIDE, so a failed derived view cannot wedge a
    // state change that is already committed.
    //
    // Deliberately NOT gated on `was_new`: the row write is create-if-absent and
    // the removal is idempotent, so a saga that tore between storage and here
    // must still archive rather than depend on winning the race to be the first
    // driver — the same argument the deletion path makes.
    //
    // In `drive` ONLY. `drive_relay_minted` has no archive step, which is what
    // stops an emitted 8002/8003/13535 from triggering an archive of its own.
    crate::relay::archive::apply_archive(
        store,
        ape,
        topics,
        community,
        generation,
        event,
        relay_seckey,
        doc.admitted_at_ms,
        now_ms,
    )
    .await?;

    complete(ape, community, doc, outcome.raw(), audit_seq).await?;
    Ok(audit_seq)
}

/// Drive an admitted saga for an event the RELAY minted — today, one of the
/// NIP-29 group projections ([`crate::relay::effects::publish_group_projections`]) or a
/// membership notice ([`crate::relay::effects::emit_membership_notices`]).
///
/// Byte-identical to [`drive`] except that it has no projection step, and that
/// is the whole point: a projection projects nothing, so the emitter calling
/// this instead of `drive` makes "an emission can never trigger an emission" a
/// property of the call graph rather than of an argument someone must remember
/// to pass. (It also keeps the two functions out of a call cycle, which an
/// `async fn` pair cannot be without an inference-defeating box.)
#[allow(clippy::too_many_arguments)]
pub async fn drive_relay_minted(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    event: &Event,
    outcome: &AdmitOutcome,
    now_ms: u64,
) -> Result<Option<u64>> {
    debug_assert!(
        kind::is_relay_only_kind(event.kind),
        "drive_relay_minted is for relay-signed kinds only"
    );
    let doc = outcome.doc();
    if doc.is_deleted() {
        return erase_tombstoned(store, ape, community, doc, outcome.raw()).await;
    }
    if doc.is_complete() {
        return Ok(doc.audit_seq);
    }
    let channel = doc.channel.clone();
    let audit_seq = apply_all(
        store,
        ape,
        topics,
        community,
        generation,
        event,
        channel.as_deref(),
        now_ms,
    )
    .await?;
    complete(ape, community, doc, outcome.raw(), audit_seq).await?;
    Ok(audit_seq)
}

/// Finish a tombstoned saga: erase whatever of the event is still stored and
/// close the saga so its envelope stops redelivering.
///
/// Every step is idempotent. The row removal runs first and unconditionally,
/// because the driver that tombstoned the id may have died between the marker
/// and the erase, and because a mid-flight driver may have re-created rows
/// after it; the completion is what stops the queue from re-driving forever.
/// A completion that loses its CAS is not an error here — another driver of the
/// same tombstoned id got there first, and the erase already ran.
async fn erase_tombstoned(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    doc: &Admission,
    raw: &[u8],
) -> Result<Option<u64>> {
    crate::relay::effects::purge_event_rows(
        store,
        community,
        &doc.event_id,
        doc.channel.as_deref(),
    )
    .await?;
    if !doc.is_complete() {
        if let Err(e) = complete(ape, community, doc, raw, doc.audit_seq).await {
            tracing::warn!(
                "a deleted event's saga was not closed ({e}); the next driver closes it"
            );
        }
    }
    Ok(doc.audit_seq)
}

/// Every effect of an admitted event: storage, materialization, replacement,
/// audit, feed. Returns the audit sequence (`None` for an ephemeral event,
/// which is never stored or audited).
///
/// Shared by [`drive`] and [`drive_relay_minted`] so there is exactly ONE
/// ordering of the saga's steps. A second copy of this sequence would be a
/// second write path, and the two would drift.
#[allow(clippy::too_many_arguments)]
async fn apply_all(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    event: &Event,
    channel: Option<&str>,
    now_ms: u64,
) -> Result<Option<u64>> {
    // Ephemeral events are never stored, and carry no materialized effect.
    if !kind::is_ephemeral(event.kind) {
        if event.kind == kind::KIND_WORKFLOW_DEF {
            crate::relay::workflows::claim_or_verify_owner(
                store, ape, community, channel, event,
            )
            .await?;
        }
        apply_storage(store, community, event, channel).await?;

        // Materialize channel/membership/thread/reaction state from this event,
        // on EVERY drive of an incomplete saga. Gating this on `apply_storage`
        // reporting the body newly written is a race the body write cannot
        // settle: a saga that tore after the body landed (an index write failed,
        // the process died, the envelope was redelivered) re-drives with
        // `was_new` false and would skip the effect, yet still audit, deliver and
        // complete: a ban announced but never written to `restrict/`, a removal
        // that left the `mem/` row and its access.
        // So every effect is idempotent: moderation, membership, DM,
        // channel and reaction effects set state, a deletion erases, and the
        // thread effect bumps its counters only when it creates the event's own
        // row. A completed saga never reaches here (`drive` returns first).
        crate::relay::effects::apply_effects(store, ape, community, event, channel, now_ms).await?;
    }

    // NIP-16/33 replacement: converge this event's replaceable
    // cell onto its winner and delete the loser's rows. Deliberately NOT
    // `was_new`-gated: a re-driven saga re-creates rows apply_storage finds
    // absent (a previous replacement deleted them), and this step is what
    // re-converges the cell — every operation inside is idempotent.
    if !kind::is_ephemeral(event.kind) {
        crate::relay::effects::apply_replacement(store, ape, community, event, channel).await?;
    }

    let audit_seq = if kind::is_ephemeral(event.kind) {
        None
    } else {
        Some(append_audit(ape, community, event, now_ms).await?)
    };

    publish_feed(
        topics,
        community,
        generation,
        event,
        channel.map(ToOwned::to_owned),
    )
    .await?;
    Ok(audit_seq)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::relay::event::PubKey;

    fn pk(b: u8) -> PubKey {
        [b; 32]
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    /// The plan that decides which durable feeds get DELETED. Everything that
    /// is not strictly below the live generation has to survive it.
    #[test]
    fn only_generations_below_the_live_one_are_retired() {
        let listed = names(&[
            "delivery-main-0",
            "delivery-main-1",
            "delivery-main-2",
            "delivery-main-3",
            "changefeed-main",
            "delivery-other-0",
        ]);
        assert_eq!(
            superseded_feed_generations(&listed, "main", 2),
            names(&["delivery-main-0", "delivery-main-1"]),
            "oldest first, and nothing at or above the live generation"
        );
        // Generation zero has superseded nothing, which is the state a
        // deployment that has never rotated is in.
        assert!(superseded_feed_generations(&listed, "main", 0).is_empty());
        // The changefeed is never rotated and never retired; another
        // community's feed is not this caller's to delete.
        for topic in superseded_feed_generations(&listed, "main", 4) {
            assert!(topic.starts_with("delivery-main-"), "{topic}");
        }
        assert_eq!(superseded_feed_generations(&listed, "other", 4).len(), 1);
    }

    /// One call cannot delete an unbounded number of durable feeds.
    #[test]
    fn a_retirement_plan_is_bounded() {
        let listed: Vec<String> = (0..40).map(|g| format!("delivery-main-{g}")).collect();
        let plan = superseded_feed_generations(&listed, "main", 40);
        assert_eq!(plan.len(), MAX_RETIRED_GENERATIONS);
        assert_eq!(
            plan.first().map(String::as_str),
            Some("delivery-main-0"),
            "the oldest generation goes first: {plan:?}"
        );
    }

    fn ev(kind: u32, id: u8, tags: Vec<Vec<String>>) -> Event {
        Event {
            id: [id; 32],
            pubkey: pk(1),
            created_at: 1_700_000_000,
            kind,
            tags,
            content: "hi".into(),
            sig: [0u8; 64],
        }
    }

    #[test]
    fn admission_round_trips_and_starts_incomplete() {
        let doc = Admission::new(
            &ev(kind::KIND_STREAM_MESSAGE, 1, vec![]),
            "main",
            Some("c1".into()),
            42,
        );
        assert_eq!(doc.state, SagaState::Admitted);
        assert!(!doc.is_complete());
        assert!(needs_repair(&doc));
        let raw = doc.encode().unwrap();
        assert_eq!(Admission::decode(&raw).unwrap(), doc);
    }

    #[test]
    fn completed_admission_needs_no_repair() {
        let mut doc = Admission::new(&ev(kind::KIND_STREAM_MESSAGE, 1, vec![]), "main", None, 1);
        doc.state = SagaState::Complete;
        assert!(!needs_repair(&doc));
    }

    #[test]
    fn admission_encoding_is_byte_stable() {
        // The completion step guards on the stored bytes, so re-encoding an
        // unchanged document must reproduce them exactly.
        let doc = Admission::new(
            &ev(kind::KIND_STREAM_MESSAGE, 3, vec![]),
            "main",
            Some("c".into()),
            7,
        );
        let decoded = Admission::decode(&doc.encode().unwrap()).unwrap();
        assert_eq!(decoded.encode().unwrap(), doc.encode().unwrap());
    }

    #[test]
    fn index_keys_cover_every_read_path() {
        let e = ev(
            kind::KIND_STREAM_MESSAGE,
            5,
            vec![
                vec!["e".into(), "a".repeat(64)],
                vec!["p".into(), to_hex(&pk(2))],
                vec!["h".into(), "chan-1".into()],
            ],
        );
        let ks = index_keys_for("main", &e, Some("chan-1"));
        // timeline + kind + author + one e-tag + one p-tag + the terms of "hi"
        // (one term, two characters, so exactly at the floor).
        assert_eq!(ks.len(), 5 + 1);
        // All distinct: an index write must not collapse two different paths.
        let mut sorted = ks.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ks.len());
    }

    #[test]
    fn events_without_tags_still_index_the_core_paths() {
        // timeline + kind + author, plus the one search term in "hi".
        let ks = index_keys_for("main", &ev(kind::KIND_STREAM_MESSAGE, 6, vec![]), None);
        assert_eq!(ks.len(), 3 + 1);
        // A kind nobody searches contributes no term rows at all.
        let reaction = index_keys_for("main", &ev(kind::KIND_REACTION, 6, vec![]), None);
        assert_eq!(reaction.len(), 3);
    }

    #[test]
    fn multiple_e_and_p_tags_each_get_an_index_entry() {
        let e = ev(
            kind::KIND_STREAM_MESSAGE,
            7,
            vec![
                vec!["e".into(), "a".repeat(64)],
                vec!["e".into(), "b".repeat(64)],
                vec!["p".into(), to_hex(&pk(2))],
                vec!["p".into(), to_hex(&pk(3))],
            ],
        );
        assert_eq!(index_keys_for("main", &e, None).len(), 3 + 2 + 2 + 1);
    }

    /// One event's `e`/`p` tags fan out into at most a bounded number of
    /// distinct index writes (finding_a180076ac500dd6b).
    #[test]
    fn tag_index_fan_out_is_deduplicated_and_bounded() {
        let mut tags: Vec<Vec<String>> = (0..5_000)
            .map(|i| vec!["e".into(), format!("{:064x}", i)])
            .collect();
        tags.extend((0..5_000).map(|_| vec!["p".into(), to_hex(&pk(2))]));
        let keys = index_keys_for("main", &ev(kind::KIND_STREAM_MESSAGE, 8, tags), None);
        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "no key is written twice");
        assert!(
            keys.len() <= 3 + MAX_INDEXED_VALUES_PER_TAG + 1 + crate::relay::search::MAX_TERMS_PER_EVENT,
            "{} index keys for one event",
            keys.len()
        );
    }

    #[test]
    fn event_id_hex_must_be_32_bytes() {
        assert!(decode_id(&to_hex(&[7u8; 32])).is_ok());
        assert!(decode_id("abcd").is_err());
        assert!(decode_id("zz".repeat(32).as_str()).is_err());
    }
}
