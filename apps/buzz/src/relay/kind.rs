//! The event-kind registry: every kind number buzz speaks, and the predicates
//! that classify them.
//!
//! Transliterated from `buzz/crates/buzz-core/src/kind.rs`, which is the
//! upstream source of truth for these numbers. Two upstream helpers that took a
//! `nostr::Event` (`is_unshared_persona_event`, `persona_event_is_shared`) do
//! not live here: this crate has its own [`crate::relay::event::Event`], so the
//! persona share gate is implemented in [`crate::relay::access`] and this module only
//! exposes the kind test it needs ([`is_persona`]).
//!
//! # Why the numbers matter
//!
//! A kind number is not a label — it is the routing key for storage shape
//! (regular / replaceable / parameterized-replaceable / ephemeral) *and* for
//! access control (author-only, `#p`-gated, result-gated). A wrong number is a
//! silent data-leak or a silent data-loss, so [`ALL_KINDS`] plus the tests at
//! the bottom of this file pin the whole registry: 129 entries, no duplicates,
//! and every access-set member is a registered kind.
//!
//! # Layout of the number space
//!
//! | Range         | Meaning                                        |
//! |---------------|------------------------------------------------|
//! | `0`, `3`, `41`| NIP-01 replaceable singletons                  |
//! | `10000–19999` | NIP-01 replaceable, keyed `(pubkey, kind)`     |
//! | `20000–29999` | ephemeral — never stored                       |
//! | `30000–39999` | NIP-33 parameterized replaceable, `+ d_tag`    |
//! | everything else | regular: append-only, stored forever         |

/// A Nostr event kind.
///
/// NIP-01 specifies kind as an unsigned integer; `u32` covers the full space
/// without truncation, and every kind buzz registers also fits in `u16`
/// (asserted at compile time at the bottom of this module).
pub type Kind = u32;

/// NIP-01: User profile metadata.
pub const KIND_PROFILE: Kind = 0;
/// NIP-01: Short text note.
pub const KIND_TEXT_NOTE: Kind = 1;
/// NIP-02: Contact list / follow list.
pub const KIND_CONTACT_LIST: Kind = 3;
/// NIP-09: Event deletion request.
pub const KIND_DELETION: Kind = 5;
/// NIP-25: Reaction. Content is an emoji char or `+`/`-`.
pub const KIND_REACTION: Kind = 7;
/// NIP-01: Channel metadata (replaceable). Registered for completeness; buzz
/// models channels with its own kinds and does not accept this one.
pub const KIND_CHANNEL_METADATA: Kind = 41;
/// NIP-17: Outer envelope for private DMs — hides sender, content, timestamp.
pub const KIND_GIFT_WRAP: Kind = 1059;
/// NIP-94: File metadata attachment.
pub const KIND_FILE_METADATA: Kind = 1063;

/// NIP-51: Mute list (replaceable) — pubkeys/events/threads/words a user muted.
/// User-owned global state keyed by `(pubkey, kind)`; never channel-scoped.
pub const KIND_MUTE_LIST: Kind = 10000;
/// NIP-51: Pin list (replaceable) — events pinned to the user's profile.
pub const KIND_PIN_LIST: Kind = 10001;
/// NIP-65: Relay list metadata (replaceable) — outbox-model read/write relays.
pub const KIND_NIP65_RELAY_LIST_METADATA: Kind = 10002;
/// NIP-51: Bookmark list (replaceable).
pub const KIND_BOOKMARK_LIST: Kind = 10003;
/// NIP-51: Emoji list (replaceable) — preferred emojis and emoji-set pointers.
pub const KIND_EMOJI_LIST: Kind = 10030;

/// NIP-51: Follow set (parameterized replaceable) — named curated pubkey lists,
/// keyed `(pubkey, kind, d_tag)` on top of the single kind:3 contact list.
pub const KIND_FOLLOW_SET: Kind = 30000;
/// NIP-51: Bookmark set (parameterized replaceable) — named collections.
pub const KIND_BOOKMARK_SET: Kind = 30003;
/// NIP-51 / NIP-30: Emoji set (parameterized replaceable). Each member
/// publishes their own set; the workspace palette is the client-side union.
pub const KIND_EMOJI_SET: Kind = 30030;
/// NIP-23: Long-form content (parameterized replaceable). Stored globally.
pub const KIND_LONG_FORM: Kind = 30023;
/// NIP-78 / NIP-RS: Per-client read state for cross-device read-position sync
/// (parameterized replaceable). Content is NIP-44 encrypted to the author.
pub const KIND_READ_STATE: Kind = 30078;
/// NIP-38: User status, general/music/custom `d` tag (parameterized replaceable).
pub const KIND_USER_STATUS: Kind = 30315;

/// NIP-42 auth event. Never stored — it carries bearer tokens.
///
/// Deliberately absent from [`ALL_KINDS`], matching buzz: it is a transport
/// handshake, not a registered content kind.
pub const KIND_AUTH: Kind = 22242;
/// BUD-01: Blossom upload auth. Verified then discarded, never stored.
pub const KIND_BLOSSOM_AUTH: Kind = 24242;
/// Buzz one-time identity-binding proof (ephemeral, never stored).
///
/// Deliberately absent from [`ALL_KINDS`], matching buzz.
pub const KIND_NOSTR_IDENTITY_BINDING: Kind = 24243;
/// NIP-98: HTTP auth event. Verified then discarded, never stored.
pub const KIND_HTTP_AUTH: Kind = 27235;

// Agent kinds: NIP-AE, NIP-AP, NIP-AM, NIP-ER and NIP-PL.

/// Agent metadata + owner reference (replaceable, agent-authored).
pub const KIND_AGENT_PROFILE: Kind = 10100;

/// NIP-AE: Agent Engram (parameterized replaceable, agent-authored).
///
/// Encrypted memory record addressed by `(agent_pubkey, kind, d_tag)`, where
/// `d_tag` is an HMAC over the agent↔owner conversation key.
pub const KIND_AGENT_ENGRAM: Kind = 30174;

/// NIP-AP: Agent Persona (parameterized replaceable, owner-authored).
///
/// Addressed by `(pubkey, kind, d_tag)` where `d_tag` is the persona slug.
/// Read access is *author-only-unless-shared*: a persona without an exact
/// `["shared","true"]` tag reaches only its author, so system prompts and
/// `respond_to_allowlist` pubkeys are not published community-wide as a
/// side effect of device sync. The per-event half of that gate lives in
/// [`crate::relay::access::is_unshared_persona`]; the kind half is [`is_persona`].
pub const KIND_PERSONA: Kind = 30175;

/// NIP-AP: Agent Team (parameterized replaceable, owner-authored).
/// A user-facing grouping of personas, keyed by `(pubkey, kind, team_id)`.
pub const KIND_TEAM: Kind = 30176;

/// NIP-AP: Managed Agent (parameterized replaceable, owner-authored).
///
/// Keyed by `(pubkey, kind, agent_pubkey)`. Content is an explicit opt-IN
/// projection: it must never carry the agent's secret key, auth tag, env vars,
/// or runtime fields, because these events are world-readable.
pub const KIND_MANAGED_AGENT: Kind = 30177;

/// NIP-AP: Team Catalog (parameterized replaceable, owner-authored).
///
/// Upstream v0.5.x (block/buzz PR #3358) added this as the SECOND member of
/// its shared-gated set beside personas: a catalog entry is author-only until
/// it carries exactly `["shared","true"]`, because a team catalog can name
/// personas — and through them instructions — its author never opted into
/// publishing. Same gate here: see [`SHARED_GATED_KINDS`] and
/// [`crate::relay::access::is_unshared_gated`].
pub const KIND_TEAM_CATALOG: Kind = 30178;

/// NIP-PMA: Private Managed Agent (parameterized replaceable, **author-only**).
///
/// Upstream v0.5.x (block/buzz PR #5133, sync 002) accepts this at ingest as
/// owner-scoped global user data: an owner-encrypted aggregate addressed by
/// `(owner pubkey, kind, agent pubkey)` whose content is NIP-44 ciphertext from
/// the owner's key to itself — the runnable identity and configuration that
/// [`KIND_MANAGED_AGENT`] deliberately projects OUT. The signed outer tags carry
/// only the agent coordinate, CAS generation/predecessor and active/deleted
/// state. Upstream lists it in `AUTHOR_ONLY_KINDS`; so does this port, because
/// without that line an unregistered 30179 was accepted here as an ordinary
/// world-readable parameterized-replaceable event. Nothing this deployment ships
/// publishes one yet (upstream's own codec says relays must not act on the
/// aggregate until its CAS transactions exist), so the port is the READ gate and
/// the storage shape, not the aggregate semantics.
pub const KIND_PRIVATE_MANAGED_AGENT: Kind = 30179;

/// NIP-ER: Event Reminder (parameterized replaceable, author-only).
///
/// The public `not_before` tag tells the relay when the reminder is due; the
/// target, note, and state are NIP-44 encrypted to the author. See
/// [`AUTHOR_ONLY_KINDS`].
pub const KIND_EVENT_REMINDER: Kind = 30300;

/// NIP-PL: encrypted push lease (parameterized replaceable, author-only).
///
/// The source event holds endpoint-bearing NIP-44 ciphertext readable only by
/// its authenticated author; effective delivery state lives outside the log.
pub const KIND_PUSH_LEASE: Kind = 30350;

/// NIP-AM: Agent Turn Metric — durable per-turn token-usage record.
///
/// A *regular* stored event (append-only, never replaced), NIP-44 encrypted to
/// the agent's owner. Exactly one `p` tag (owner) and one `agent` tag; no `h`
/// tag — the channel is inside the ciphertext. Owner-scoped reads only.
pub const KIND_AGENT_TURN_METRIC: Kind = 44200;

/// NIP-56: Report an event, pubkey, or blob to moderators.
///
/// Reports are signals, not triggers: accepted at ingest, queued for review,
/// never auto-actioned and never fanned out publicly.
pub const KIND_REPORT: Kind = 1984;

/// Buzz product feedback submission. Accepted at ingest, sidecarred, never
/// stored or fanned out as an event.
pub const KIND_PRODUCT_FEEDBACK: Kind = 42000;

/// Moderation: ban a pubkey from the community (`p` target, optional
/// `expiration` + `reason`).
pub const KIND_MODERATION_BAN: Kind = 9040;
/// Moderation: lift a community ban (`p` target).
pub const KIND_MODERATION_UNBAN: Kind = 9041;
/// Moderation: timeout (write-block) a pubkey until an `expiration` timestamp.
pub const KIND_MODERATION_TIMEOUT: Kind = 9042;
/// Moderation: clear a timeout early (`p` target).
pub const KIND_MODERATION_UNTIMEOUT: Kind = 9043;
/// Moderation: resolve a report (`report` = report event id, `status` =
/// resolved|dismissed, `action` = delete|kick|ban|timeout|dismiss|escalate).
pub const KIND_MODERATION_RESOLVE_REPORT: Kind = 9044;

// NIP-29 group admin commands.

/// NIP-29: Add a user to a group.
pub const KIND_NIP29_PUT_USER: Kind = 9000;
/// NIP-29: Remove a user from a group.
pub const KIND_NIP29_REMOVE_USER: Kind = 9001;
/// NIP-29: Edit group metadata.
pub const KIND_NIP29_EDIT_METADATA: Kind = 9002;
/// NIP-29: Delete an event from a group.
pub const KIND_NIP29_DELETE_EVENT: Kind = 9005;
/// NIP-29: Create a new group.
pub const KIND_NIP29_CREATE_GROUP: Kind = 9007;
/// NIP-29: Delete a group.
pub const KIND_NIP29_DELETE_GROUP: Kind = 9008;
/// NIP-29: Create an invite to a group.
pub const KIND_NIP29_CREATE_INVITE: Kind = 9009;
/// NIP-29: Request to join a group.
pub const KIND_NIP29_JOIN_REQUEST: Kind = 9021;
/// NIP-29: Request to leave a group.
pub const KIND_NIP29_LEAVE_REQUEST: Kind = 9022;

/// Channel-vocabulary alias for [`KIND_NIP29_PUT_USER`] (9000).
///
/// buzz speaks "NIP-29 group"; this crate's channel/delivery layer speaks
/// "channel member". The alias keeps call sites readable without minting a
/// second number — it is intentionally *not* in [`ALL_KINDS`], which registers
/// each value exactly once under its buzz name.
pub const KIND_GROUP_ADD_USER: Kind = KIND_NIP29_PUT_USER;
/// Channel-vocabulary alias for [`KIND_NIP29_REMOVE_USER`] (9001). See
/// [`KIND_GROUP_ADD_USER`] for why aliases are not registered.
pub const KIND_GROUP_REMOVE_USER: Kind = KIND_NIP29_REMOVE_USER;

// NIP-43 relay membership.

/// NIP-43: Add a pubkey to the relay member list (admin command).
pub const RELAY_ADMIN_ADD_MEMBER: Kind = 9030;
/// NIP-43: Remove a pubkey from the relay member list (admin command).
pub const RELAY_ADMIN_REMOVE_MEMBER: Kind = 9031;
/// NIP-43: Change the role of an existing relay member (admin command).
pub const RELAY_ADMIN_CHANGE_ROLE: Kind = 9032;
/// Buzz: set the workspace profile/icon (admin/owner-signed command).
pub const RELAY_ADMIN_SET_WORKSPACE_PROFILE: Kind = 9033;
/// NIP-43: Membership list snapshot (relay-signed).
pub const KIND_NIP43_MEMBERSHIP_LIST: Kind = 13534;
/// NIP-43: Member added announcement (relay-signed).
pub const KIND_NIP43_MEMBER_ADDED: Kind = 8000;
/// NIP-43: Member removed announcement (relay-signed).
pub const KIND_NIP43_MEMBER_REMOVED: Kind = 8001;
/// NIP-43: User leave request (user-signed, ephemeral range).
pub const KIND_NIP43_LEAVE_REQUEST: Kind = 28936;

// NIP-IA identity archival.

/// NIP-IA: Request that the relay archive a target identity.
pub const KIND_IA_ARCHIVE_REQUEST: Kind = 9035;
/// NIP-IA: Request that the relay unarchive a target identity.
pub const KIND_IA_UNARCHIVE_REQUEST: Kind = 9036;
/// NIP-IA: Archived-identity delta (relay-signed).
pub const KIND_IA_ARCHIVED: Kind = 8002;
/// NIP-IA: Unarchived-identity delta (relay-signed).
pub const KIND_IA_UNARCHIVED: Kind = 8003;
/// NIP-IA: Archived identities list snapshot (relay-signed).
pub const KIND_IA_ARCHIVED_LIST: Kind = 13535;

// NIP-29 addressable group state (39000–39003) and query overlays.

/// NIP-29: Addressable group metadata state.
pub const KIND_NIP29_GROUP_METADATA: Kind = 39000;
/// NIP-29: Addressable group admins list.
pub const KIND_NIP29_GROUP_ADMINS: Kind = 39001;
/// NIP-29: Addressable group members list.
pub const KIND_NIP29_GROUP_MEMBERS: Kind = 39002;
/// NIP-29: Addressable group roles definition.
pub const KIND_NIP29_GROUP_ROLES: Kind = 39003;

/// Thread summary overlay (relay-synthesized at query time, never stored):
/// `e`/`d` = root event id, content = reply/descendant counts and participants.
pub const KIND_THREAD_SUMMARY: Kind = 39005;
/// Window bounds overlay (relay-synthesized, never stored): `d` =
/// `<channel_id>:<cursor>`, content = `{has_more, next_cursor}`. The only
/// authority on exhaustion — clients must not infer `has_more` from row counts.
pub const KIND_WINDOW_BOUNDS: Kind = 39006;

// Ephemeral (20000–29999): delivered live, never stored.

/// Ephemeral: user presence update (online/away/offline).
pub const KIND_PRESENCE_UPDATE: Kind = 20001;
/// Ephemeral: typing indicator for a channel.
pub const KIND_TYPING_INDICATOR: Kind = 20002;
/// NIP-AB: device pairing event. May be discarded after delivery.
pub const KIND_PAIRING: Kind = 24134;
/// Ephemeral: owner-scoped encrypted agent observer telemetry/control frame.
pub const KIND_AGENT_OBSERVER_FRAME: Kind = 24200;
/// Ephemeral: huddle emoji reaction burst, `h`-scoped to the huddle channel.
pub const KIND_HUDDLE_REACTION: Kind = 24810;

/// Channel chat message — the workhorse kind.
///
/// Agent shutdown is a convention on this kind, not a new one: the owner sends
/// a kind:9 with content `"!shutdown"` and a `#p` tag naming the agent.
pub const KIND_STREAM_MESSAGE: Kind = 9;
/// Second-generation stream message (v1 wrongly used the replaceable range).
pub const KIND_STREAM_MESSAGE_V2: Kind = 40002;
/// An edit of a stream message.
pub const KIND_STREAM_MESSAGE_EDIT: Kind = 40003;
/// A stream message pinned in a channel.
pub const KIND_STREAM_MESSAGE_PINNED: Kind = 40004;
/// A stream message bookmarked by a user.
pub const KIND_STREAM_MESSAGE_BOOKMARKED: Kind = 40005;
/// A stream message scheduled for future delivery.
pub const KIND_STREAM_MESSAGE_SCHEDULED: Kind = 40006;
/// A reminder attached to a stream message or time.
pub const KIND_STREAM_REMINDER: Kind = 40007;
/// A diff/patch message showing file changes (unified diff format).
pub const KIND_STREAM_MESSAGE_DIFF: Kind = 40008;
/// System message for channel state changes (join, leave, rename, …).
pub const KIND_SYSTEM_MESSAGE: Kind = 40099;
/// Canvas (shared document) for a channel.
pub const KIND_CANVAS: Kind = 40100;

/// Channel metadata with computed fields (relay-signed sidecar).
pub const KIND_CHANNEL_SUMMARY: Kind = 40901;
/// Bulk presence state (relay-signed sidecar).
pub const KIND_PRESENCE_SNAPSHOT: Kind = 40902;

// Direct messages (41000–41999) and their visibility projection.

/// A new direct-message conversation was created.
pub const KIND_DM_CREATED: Kind = 41001;
/// Open/create a DM (`p` tags = participants).
pub const KIND_DM_OPEN: Kind = 41010;
/// Add a member to a group DM.
pub const KIND_DM_ADD_MEMBER: Kind = 41011;
/// Hide a DM from the sidebar.
pub const KIND_DM_HIDE: Kind = 41012;

/// NIP-DV: per-viewer DM visibility snapshot (relay-signed, parameterized
/// replaceable, `d` = viewer pubkey). Republished on every hide/unhide, so the
/// latest event is always the authoritative hidden set for that viewer.
pub const KIND_DM_VISIBILITY: Kind = 30622;

// Agent job protocol (43000–43999).
//
// Deliberately not NIP-90 (5000–6999): buzz requires auth chains with
// depth ≤ 3 and breadth ≤ 10, which NIP-90 does not model.

/// An agent job was requested.
pub const KIND_JOB_REQUEST: Kind = 43001;
/// An agent accepted a job request.
pub const KIND_JOB_ACCEPTED: Kind = 43002;
/// Progress update for an in-flight agent job.
pub const KIND_JOB_PROGRESS: Kind = 43003;
/// Final result of a completed agent job.
pub const KIND_JOB_RESULT: Kind = 43004;
/// A job cancellation was requested.
pub const KIND_JOB_CANCEL: Kind = 43005;
/// An agent job failed with an error.
pub const KIND_JOB_ERROR: Kind = 43006;

// Membership notices: relay-signed, `#p`-gated, stored globally.

/// Relay-signed notice: the `p`-tagged pubkey was added to the `h` channel.
pub const KIND_MEMBER_ADDED_NOTIFICATION: Kind = 44100;
/// Relay-signed notice: the `p`-tagged pubkey was removed from the `h` channel.
pub const KIND_MEMBER_REMOVED_NOTIFICATION: Kind = 44101;

// Forum and social (45000–45999).

/// A forum post (thread root).
pub const KIND_FORUM_POST: Kind = 45001;
/// A vote on a forum post.
pub const KIND_FORUM_VOTE: Kind = 45002;
/// A comment reply on a forum post.
pub const KIND_FORUM_COMMENT: Kind = 45003;

/// Workflow definition (parameterized replaceable, `d` = workflow uuid).
pub const KIND_WORKFLOW_DEF: Kind = 30620;
/// Trigger workflow execution (command).
pub const KIND_WORKFLOW_TRIGGER: Kind = 46020;
/// Grant a pending approval (command).
pub const KIND_APPROVAL_GRANT: Kind = 46030;
/// Deny a pending approval (command).
pub const KIND_APPROVAL_DENY: Kind = 46031;
/// A workflow was triggered by a matching event.
pub const KIND_WORKFLOW_TRIGGERED: Kind = 46001;
/// A workflow step began execution.
pub const KIND_WORKFLOW_STEP_STARTED: Kind = 46002;
/// A workflow step completed successfully.
pub const KIND_WORKFLOW_STEP_COMPLETED: Kind = 46003;
/// A workflow step failed.
pub const KIND_WORKFLOW_STEP_FAILED: Kind = 46004;
/// The entire workflow completed successfully.
pub const KIND_WORKFLOW_COMPLETED: Kind = 46005;
/// The entire workflow failed.
pub const KIND_WORKFLOW_FAILED: Kind = 46006;
/// The workflow was cancelled before completion.
pub const KIND_WORKFLOW_CANCELLED: Kind = 46007;
/// A workflow step is waiting for human approval.
pub const KIND_WORKFLOW_APPROVAL_REQUESTED: Kind = 46010;
/// A pending workflow approval was granted.
pub const KIND_WORKFLOW_APPROVAL_GRANTED: Kind = 46011;
/// A pending workflow approval was denied.
pub const KIND_WORKFLOW_APPROVAL_DENIED: Kind = 46012;

// OURS-ONLY BLOCK: 47000–47099 is reserved by this port. Upstream block/buzz
// defines no 47xxx kind at the pin (verified against
// `upstream-contract/kind.rs`), and `UPSTREAM-CONTRACT.md` records the
// reservation so the next sync's kind-table diff dispositions it as ours
// rather than as upstream drift. If upstream ever claims a 47xxx number,
// reallocate OURS before applying that sync.

/// A member's signed request that the process cell perform an RCS lifecycle
/// verb (branch / merge / gated land / commit-on-behalf / refresh) for the
/// process room named by its `h` tag.
///
/// An ordinary client-signed room event on purpose: WHO may ask is a room
/// question (membership), while whether the ask is honoured is the CELL's
/// decision at execution time — judged against the roster and the
/// authorization matrix, and recorded in the result either way. The command
/// event id is the idempotency key a re-delivered command dedups on.
pub const KIND_PROCESS_COMMAND: Kind = 47001;

/// The process cell's signed answer to a [`KIND_PROCESS_COMMAND`]: outcome,
/// the RCS facts (commit id, land status, conflicts, test verdict reference),
/// and the command event id it answers.
///
/// Client-signed — the cell is a CLIENT of this relay, not the relay itself,
/// so this kind is deliberately NOT relay-only — but authorship-gated at
/// ingest to the pubkeys the operator registered under `buzz-process-cells`:
/// a result asserts "the platform did X", and accepting one from an arbitrary
/// member would let anyone forge an `rcs_land succeeded` record into a room.
/// Fail closed: no registered cell, no admissible result.
pub const KIND_PROCESS_RESULT: Kind = 47002;

// System and admin (48000–48999), and media (49000–49999).

/// An audit log entry was recorded.
pub const KIND_AUDIT_ENTRY: Kind = 48001;
/// A huddle (audio/video session) was started.
pub const KIND_HUDDLE_STARTED: Kind = 48100;
/// A participant joined a huddle.
pub const KIND_HUDDLE_PARTICIPANT_JOINED: Kind = 48101;
/// A participant left a huddle.
pub const KIND_HUDDLE_PARTICIPANT_LEFT: Kind = 48102;
/// A huddle ended.
pub const KIND_HUDDLE_ENDED: Kind = 48103;
/// Relay-synthesized authoritative liveness for an active huddle session
/// (upstream sync 002, `d` = session id, `h` = parent channel).
///
/// Upstream mints this on REQ from its audio-room / mesh lease state and never
/// stores it; its whole authority is the relay's signature. This port has no
/// audio rooms to attest, so it mints NONE — a REQ for 48104 answers EOSE with
/// no events, which is the truthful answer (no huddle is live here). The kind
/// is registered so that a client-submitted 48104 is refused at ingest
/// ([`is_relay_only_kind`]) instead of being stored as a forged liveness record
/// every synced client would act on.
pub const KIND_HUDDLE_LIVENESS: Kind = 48104;
/// Huddle channel guidelines/rules document.
pub const KIND_HUDDLE_GUIDELINES: Kind = 48106;

/// Media upload audit entry.
pub const KIND_MEDIA_UPLOAD: Kind = 49001;

// NIP-34 git collaboration.

/// NIP-34: Repository announcement (parameterized replaceable, `d` = repo id).
pub const KIND_GIT_REPO_ANNOUNCEMENT: Kind = 30617;
/// NIP-34: Repository state — branch/tag refs (parameterized replaceable).
pub const KIND_GIT_REPO_STATE: Kind = 30618;
/// NIP-MP: Multi-repo project (parameterized replaceable, `d` = project id).
///
/// Upstream v0.5.x (block/buzz PR #3171) accepts these at ingest for the
/// projects surface: a project groups NIP-34 repositories. It scopes by its
/// own repo references like the rest of the git family, never by channel.
pub const KIND_PROJECT: Kind = 30621;
/// NIP-34: Patch (git format-patch output).
pub const KIND_GIT_PATCH: Kind = 1617;
/// NIP-34: Pull request.
pub const KIND_GIT_PULL_REQUEST: Kind = 1618;
/// NIP-34: Pull request update (tip commit change).
pub const KIND_GIT_PR_UPDATE: Kind = 1619;
/// NIP-34: Issue.
pub const KIND_GIT_ISSUE: Kind = 1621;
/// NIP-34: Status — Open.
pub const KIND_GIT_STATUS_OPEN: Kind = 1630;
/// NIP-34: Status — Applied / Merged.
pub const KIND_GIT_STATUS_MERGED: Kind = 1631;
/// NIP-34: Status — Closed.
pub const KIND_GIT_STATUS_CLOSED: Kind = 1632;
/// NIP-34: Status — Draft.
pub const KIND_GIT_STATUS_DRAFT: Kind = 1633;

/// Lower bound of the ephemeral range (20000–29999). Never stored.
pub const EPHEMERAL_KIND_MIN: Kind = 20000;
/// Upper bound of the ephemeral range (20000–29999). Never stored.
pub const EPHEMERAL_KIND_MAX: Kind = 29999;
/// Lower bound of the NIP-33 parameterized replaceable range (30000–39999).
pub const PARAM_REPLACEABLE_KIND_MIN: Kind = 30000;
/// Upper bound of the NIP-33 parameterized replaceable range (30000–39999).
pub const PARAM_REPLACEABLE_KIND_MAX: Kind = 39999;

/// Every registered kind, in buzz's declaration order.
///
/// Used for duplicate detection and for iteration in tests. Three declared
/// constants are deliberately absent, exactly as upstream: [`KIND_AUTH`],
/// [`KIND_NOSTR_IDENTITY_BINDING`], and [`KIND_PUSH_LEASE`] — the first two are
/// handshake artifacts that are verified then dropped, and the third is
/// registered upstream through its own module rather than this list.
pub const ALL_KINDS: &[Kind] = &[
    KIND_PROFILE,
    KIND_TEXT_NOTE,
    KIND_CONTACT_LIST,
    KIND_MUTE_LIST,
    KIND_PIN_LIST,
    KIND_NIP65_RELAY_LIST_METADATA,
    KIND_BOOKMARK_LIST,
    KIND_EMOJI_LIST,
    KIND_FOLLOW_SET,
    KIND_BOOKMARK_SET,
    KIND_EMOJI_SET,
    KIND_CHANNEL_METADATA,
    KIND_DELETION,
    KIND_REACTION,
    KIND_GIFT_WRAP,
    KIND_FILE_METADATA,
    KIND_AGENT_PROFILE,
    KIND_AGENT_ENGRAM,
    KIND_EVENT_REMINDER,
    KIND_PERSONA,
    KIND_TEAM,
    KIND_MANAGED_AGENT,
    KIND_TEAM_CATALOG,
    KIND_PRIVATE_MANAGED_AGENT,
    KIND_REPORT,
    KIND_PRODUCT_FEEDBACK,
    KIND_NIP29_PUT_USER,
    KIND_NIP29_REMOVE_USER,
    KIND_NIP29_EDIT_METADATA,
    KIND_NIP29_DELETE_EVENT,
    KIND_NIP29_CREATE_GROUP,
    KIND_NIP29_DELETE_GROUP,
    KIND_NIP29_CREATE_INVITE,
    KIND_NIP29_JOIN_REQUEST,
    KIND_NIP29_LEAVE_REQUEST,
    KIND_MODERATION_BAN,
    KIND_MODERATION_UNBAN,
    KIND_MODERATION_TIMEOUT,
    KIND_MODERATION_UNTIMEOUT,
    KIND_MODERATION_RESOLVE_REPORT,
    RELAY_ADMIN_ADD_MEMBER,
    RELAY_ADMIN_REMOVE_MEMBER,
    RELAY_ADMIN_CHANGE_ROLE,
    RELAY_ADMIN_SET_WORKSPACE_PROFILE,
    KIND_NIP43_MEMBERSHIP_LIST,
    KIND_NIP43_MEMBER_ADDED,
    KIND_NIP43_MEMBER_REMOVED,
    KIND_NIP43_LEAVE_REQUEST,
    KIND_IA_ARCHIVE_REQUEST,
    KIND_IA_UNARCHIVE_REQUEST,
    KIND_IA_ARCHIVED,
    KIND_IA_UNARCHIVED,
    KIND_IA_ARCHIVED_LIST,
    KIND_NIP29_GROUP_METADATA,
    KIND_NIP29_GROUP_ADMINS,
    KIND_NIP29_GROUP_MEMBERS,
    KIND_NIP29_GROUP_ROLES,
    KIND_THREAD_SUMMARY,
    KIND_WINDOW_BOUNDS,
    KIND_PRESENCE_UPDATE,
    KIND_TYPING_INDICATOR,
    KIND_HUDDLE_REACTION,
    KIND_BLOSSOM_AUTH,
    KIND_PAIRING,
    KIND_AGENT_OBSERVER_FRAME,
    KIND_HTTP_AUTH,
    KIND_STREAM_MESSAGE,
    KIND_STREAM_MESSAGE_V2,
    KIND_STREAM_MESSAGE_EDIT,
    KIND_STREAM_MESSAGE_PINNED,
    KIND_STREAM_MESSAGE_BOOKMARKED,
    KIND_STREAM_MESSAGE_SCHEDULED,
    KIND_STREAM_REMINDER,
    KIND_STREAM_MESSAGE_DIFF,
    KIND_CANVAS,
    KIND_SYSTEM_MESSAGE,
    KIND_CHANNEL_SUMMARY,
    KIND_PRESENCE_SNAPSHOT,
    KIND_DM_VISIBILITY,
    KIND_DM_OPEN,
    KIND_DM_ADD_MEMBER,
    KIND_DM_HIDE,
    KIND_DM_CREATED,
    KIND_JOB_REQUEST,
    KIND_JOB_ACCEPTED,
    KIND_JOB_PROGRESS,
    KIND_JOB_RESULT,
    KIND_JOB_CANCEL,
    KIND_JOB_ERROR,
    KIND_MEMBER_ADDED_NOTIFICATION,
    KIND_MEMBER_REMOVED_NOTIFICATION,
    KIND_AGENT_TURN_METRIC,
    KIND_WORKFLOW_DEF,
    KIND_LONG_FORM,
    KIND_USER_STATUS,
    KIND_READ_STATE,
    KIND_FORUM_POST,
    KIND_FORUM_VOTE,
    KIND_FORUM_COMMENT,
    KIND_WORKFLOW_TRIGGER,
    KIND_APPROVAL_GRANT,
    KIND_APPROVAL_DENY,
    KIND_WORKFLOW_TRIGGERED,
    KIND_WORKFLOW_STEP_STARTED,
    KIND_WORKFLOW_STEP_COMPLETED,
    KIND_WORKFLOW_STEP_FAILED,
    KIND_WORKFLOW_COMPLETED,
    KIND_WORKFLOW_FAILED,
    KIND_WORKFLOW_CANCELLED,
    KIND_WORKFLOW_APPROVAL_REQUESTED,
    KIND_WORKFLOW_APPROVAL_GRANTED,
    KIND_WORKFLOW_APPROVAL_DENIED,
    KIND_AUDIT_ENTRY,
    KIND_HUDDLE_STARTED,
    KIND_HUDDLE_PARTICIPANT_JOINED,
    KIND_HUDDLE_PARTICIPANT_LEFT,
    KIND_HUDDLE_ENDED,
    KIND_HUDDLE_LIVENESS,
    KIND_HUDDLE_GUIDELINES,
    KIND_MEDIA_UPLOAD,
    KIND_GIT_REPO_ANNOUNCEMENT,
    KIND_GIT_REPO_STATE,
    KIND_PROJECT,
    KIND_GIT_PATCH,
    KIND_GIT_PULL_REQUEST,
    KIND_GIT_PR_UPDATE,
    KIND_GIT_ISSUE,
    KIND_GIT_STATUS_OPEN,
    KIND_GIT_STATUS_MERGED,
    KIND_GIT_STATUS_CLOSED,
    KIND_GIT_STATUS_DRAFT,
    KIND_PROCESS_COMMAND,
    KIND_PROCESS_RESULT,
];

/// Is `kind` ephemeral (20000–29999)?
///
/// Ephemeral events are delivered live and never persisted, so a delivery pump
/// may drop them under backpressure without losing durable state.
#[must_use]
pub const fn is_ephemeral(kind: Kind) -> bool {
    kind >= EPHEMERAL_KIND_MIN && kind <= EPHEMERAL_KIND_MAX
}

/// Is `kind` replaceable per NIP-01 (0, 3, 41, 10000–19999)?
///
/// Replaceable events are keyed `(pubkey, kind)` — the highest `created_at`
/// wins. NIP-33 kinds are *not* included: they key on a `d` tag as well and go
/// through the parameterized path instead. The two sets are proven disjoint by
/// `replaceable_and_parameterized_are_disjoint` below.
#[must_use]
pub const fn is_replaceable(kind: Kind) -> bool {
    matches!(kind, 0 | 3 | KIND_CHANNEL_METADATA | 10000..=19999)
}

/// Is `kind` parameterized replaceable per NIP-33 (30000–39999)?
///
/// Keyed `(pubkey, kind, d_tag)`; the highest `created_at` per key wins.
#[must_use]
pub const fn is_parameterized_replaceable(kind: Kind) -> bool {
    kind >= PARAM_REPLACEABLE_KIND_MIN && kind <= PARAM_REPLACEABLE_KIND_MAX
}

/// Is `kind` a regular event — stored append-only, never replaced or dropped?
///
/// The complement of the other three shapes, so every kind has exactly one
/// storage shape. Chat messages (9), reactions (7), and agent turn metrics
/// (44200) are all regular.
#[must_use]
pub const fn is_regular(kind: Kind) -> bool {
    !is_ephemeral(kind) && !is_replaceable(kind) && !is_parameterized_replaceable(kind)
}

/// Kinds whose stored events are readable only by their author.
///
/// The relay must never reveal the existence, count, tags, content, schedule,
/// or search matches of these events to anyone but the authenticated author.
/// Enforced on the write path (reminder `not_before` validation) and on every
/// read path (see [`crate::relay::access::may_deliver`] and
/// [`crate::relay::access::filter_authorized`]).
///
/// A linear scan is correct and fast at this size; if it grows past ~4 entries,
/// switch to a sorted array with binary search.
pub const AUTHOR_ONLY_KINDS: &[Kind] = &[
    KIND_EVENT_REMINDER,
    KIND_PUSH_LEASE,
    KIND_PRIVATE_MANAGED_AGENT,
    KIND_REPORT,
    KIND_PRODUCT_FEEDBACK,
    // A DM hide is one viewer's sidebar preference; nobody else may learn who
    // hid which conversation (finding_05b906dac8d7bf7b).
    KIND_DM_HIDE,
];

/// Kinds needing a *result-level* gate beyond the filter-layer `#p` check: even
/// a reader who already knows an event id must match the event's `#p` tag.
///
/// This closes the kindless `{ids:[…]}` read path for events whose very
/// existence must not leak.
pub const RESULT_GATED_KINDS: &[Kind] = &[
    KIND_DM_VISIBILITY,
    KIND_AGENT_TURN_METRIC,
    // DM open / add-member commands name the conversation's participants in
    // `p` tags and carry no `h` tag, so without this they were community-global
    // and leaked the workspace's private DM graph (finding_05b906dac8d7bf7b).
    KIND_DM_OPEN,
    KIND_DM_ADD_MEMBER,
];

/// Kinds with `#p`-bound read access — readable only by the author and by the
/// pubkeys named in the event's `#p` tags.
///
/// Enforced at the filter layer (a REQ that can match one of these is refused
/// unless its `#p` is exactly the authenticated reader) and again per event at
/// delivery. Stored kinds in this set are additionally excluded from full-text
/// search indexing; the ephemeral members (e.g. [`KIND_AGENT_OBSERVER_FRAME`])
/// are never stored, so that second defense does not apply to them.
pub const P_GATED_KINDS: &[Kind] = &[
    KIND_AGENT_OBSERVER_FRAME,
    KIND_MEMBER_ADDED_NOTIFICATION,
    KIND_MEMBER_REMOVED_NOTIFICATION,
    KIND_GIFT_WRAP,
    KIND_DM_VISIBILITY,
    // NIP-AM turn metrics are encrypted to the owner and must not be readable
    // by any non-owner, including through an `ids` filter.
    KIND_AGENT_TURN_METRIC,
    // Readable by the author and the participants they name, nobody else.
    KIND_DM_OPEN,
    KIND_DM_ADD_MEMBER,
];

/// Is `kind` in [`AUTHOR_ONLY_KINDS`]?
#[must_use]
pub fn is_author_only(kind: Kind) -> bool {
    AUTHOR_ONLY_KINDS.contains(&kind)
}

/// Is `kind` in [`RESULT_GATED_KINDS`]?
#[must_use]
pub fn is_result_gated(kind: Kind) -> bool {
    RESULT_GATED_KINDS.contains(&kind)
}

/// Is `kind` in [`P_GATED_KINDS`]?
#[must_use]
pub fn is_p_gated(kind: Kind) -> bool {
    P_GATED_KINDS.contains(&kind)
}

/// Kinds with *author-only-unless-shared* read semantics: personas (30175)
/// and team-catalog entries (30178) — upstream's `SHARED_GATED_KINDS`,
/// membership-identical by the pinning test below.
///
/// An event of one of these kinds reaches a foreign reader only when it
/// carries exactly `["shared","true"]`; the per-event half of the gate is
/// [`crate::relay::access::is_unshared_gated`]. Deliberately NOT folded into
/// [`is_author_only`]: a shared member of this set MUST reach foreign readers,
/// which an author-only kind never may.
pub const SHARED_GATED_KINDS: &[Kind] = &[KIND_PERSONA, KIND_TEAM_CATALOG];

/// Is `kind` in [`SHARED_GATED_KINDS`]? `const` so membership is compile-
/// assertable; the slice exists for registration tests and documentation, and
/// `shared_gated_forms_agree` pins the two spellings together.
#[must_use]
pub const fn is_shared_gated(kind: Kind) -> bool {
    matches!(kind, KIND_PERSONA | KIND_TEAM_CATALOG)
}

/// Is `kind` one of the two relay-signed membership notices — 44100 (added) /
/// 44101 (removed)?
///
/// The pair selects one access rule nothing else selects: the notice's `p`-tag
/// SUBJECT may read it even in a private channel they are not — or are no
/// longer — a member of ([`crate::relay::access::may_deliver`]). A 44101 exists
/// precisely to tell its subject they lost access, so gating it on the access
/// whose loss it announces would censor every removal from a private room.
#[must_use]
pub const fn is_membership_notice(kind: Kind) -> bool {
    matches!(
        kind,
        KIND_MEMBER_ADDED_NOTIFICATION | KIND_MEMBER_REMOVED_NOTIFICATION
    )
}

/// Is `kind` the persona kind (30175), which uses author-only-unless-shared
/// read semantics?
///
/// This is only the *kind* half of the gate. A persona event still reaches
/// foreign readers when it carries exactly `["shared","true"]`; that per-event
/// half lives in [`crate::relay::access::is_unshared_persona`]. Keeping the two halves
/// apart is deliberate: the share gate must not be folded into
/// [`is_author_only`], because shared personas MUST reach foreign readers for
/// the opt-in agent catalog query to work.
#[must_use]
pub const fn is_persona(kind: Kind) -> bool {
    kind == KIND_PERSONA
}

/// May `kind` only be authored by the relay itself?
///
/// Client submission of these must be rejected: they are relay-signed
/// projections whose authority comes from the relay's own key. Accepting a
/// forged [`is_group_projection`] event would be the worst of them — anyone
/// could then hand every client a member list of their own invention.
#[must_use]
pub const fn is_relay_only_kind(kind: Kind) -> bool {
    is_group_projection(kind)
        || is_relay_emitted_workflow_kind(kind)
        || matches!(
            kind,
            KIND_NIP43_MEMBERSHIP_LIST
                | KIND_CHANNEL_SUMMARY
                | KIND_PRESENCE_SNAPSHOT
                | KIND_DM_VISIBILITY
                | KIND_THREAD_SUMMARY
                | KIND_WINDOW_BOUNDS
                // The membership notices the relay mints for an added or removed
                // participant ([`crate::relay::effects::emit_membership_notices`]). A
                // client-forged one would be a fake "you are in this room" that a
                // real client acts on by refetching — harmless in itself, but the
                // whole authority of a notice is the relay's signature, so it is
                // refused on the way in like every other relay-signed kind.
                | KIND_MEMBER_ADDED_NOTIFICATION
                | KIND_MEMBER_REMOVED_NOTIFICATION
                // NIP-43's own announcements, which are documented relay-signed
                // and were reachable by a client for want of this line.
                | KIND_NIP43_MEMBER_ADDED
                | KIND_NIP43_MEMBER_REMOVED
                // NIP-IA: the archive deltas and the snapshot
                // ([`crate::relay::archive`] mints all three). The snapshot is
                // the one that matters most — NIP-IA calls it authoritative, so
                // a forged 13535 would hand every client an archived-identity
                // list of somebody else's invention.
                | KIND_IA_ARCHIVED
                | KIND_IA_UNARCHIVED
                | KIND_IA_ARCHIVED_LIST
                // Upstream synthesizes huddle liveness from its audio-room state
                // and signs it as the relay. Nothing mints one here, and a
                // client-submitted one would be a forged "this call is live".
                | KIND_HUDDLE_LIVENESS
                // System and audit kinds no client authors. Nothing here mints
                // them either, so a client-submitted one is only ever a forgery:
                // buzz-web treats 40099 as a relay system row, and 39003 would
                // otherwise slip past the group-projection gate as a global
                // event (finding_9d6952a3a7dfa9dd).
                | KIND_NIP29_GROUP_ROLES
                | KIND_SYSTEM_MESSAGE
                | KIND_DM_CREATED
                | KIND_AUDIT_ENTRY
                | KIND_MEDIA_UPLOAD
        )
}

/// Is `kind` a workflow lifecycle event the RELAY emits (46001–46007, 46010)?
///
/// Narrower than [`is_workflow_execution_kind`] on purpose, and the exclusions
/// are the point: 46011 and 46012 are a PERSON's approval decision, so a client
/// must be able to submit them or no gate could ever be answered.
///
/// The approval REQUEST (46010) is the sharpest member. A forged one puts a gate
/// in front of somebody carrying a token the forger chose — and a decision on it
/// is then a signature the forger holds over a step that was never real.
#[must_use]
pub const fn is_relay_emitted_workflow_kind(kind: Kind) -> bool {
    matches!(
        kind,
        KIND_WORKFLOW_TRIGGERED
            | KIND_WORKFLOW_STEP_STARTED
            | KIND_WORKFLOW_STEP_COMPLETED
            | KIND_WORKFLOW_STEP_FAILED
            | KIND_WORKFLOW_COMPLETED
            | KIND_WORKFLOW_FAILED
            | KIND_WORKFLOW_CANCELLED
            | KIND_WORKFLOW_APPROVAL_REQUESTED
    )
}

/// Is `kind` one of the three relay-signed NIP-29 group projections —
/// 39000 (metadata), 39001 (admins), 39002 (members)?
///
/// This is the set a real buzz client builds its channel list and roster from
/// ([`crate::relay::effects::publish_group_projections`] mints them), and it selects
/// two rules that nothing else selects:
///
/// * they are **relay-only** ([`is_relay_only_kind`]) — a client-submitted one
///   is refused, because their whole authority is the relay's signature;
/// * they are **channel-describing**, so the access gate must scope them to
///   the channel their `d` tag names rather than let them fall through to the
///   community-global allow ([`crate::relay::access::may_deliver`]).
///
/// [`KIND_NIP29_GROUP_ROLES`] (39003) is deliberately excluded, as it is from
/// [`is_channel_metadata`]: this port defines no role vocabulary event, and
/// nothing mints one.
#[must_use]
pub const fn is_group_projection(kind: Kind) -> bool {
    matches!(
        kind,
        KIND_NIP29_GROUP_METADATA | KIND_NIP29_GROUP_ADMINS | KIND_NIP29_GROUP_MEMBERS
    )
}

/// Is `kind` community-global — never channel-scoped, even if the event
/// carries an `h` tag?
///
/// Ingest forces `channel = None` for these. The signature covers the tags, so
/// a stray `h` tag cannot be stripped from the stored event; the scope decision
/// is made here instead, once, rather than trusting the author's tag.
#[must_use]
pub const fn is_global_only_kind(kind: Kind) -> bool {
    matches!(
        kind,
        KIND_PROFILE
            | KIND_TEXT_NOTE
            | KIND_CONTACT_LIST
            | KIND_LONG_FORM
            | KIND_USER_STATUS
            | KIND_READ_STATE
            // NIP-51 lists/sets and the NIP-65 relay list: user-owned global
            // state keyed by (pubkey, kind[, d_tag]), same shape as kind:3.
            | KIND_MUTE_LIST
            | KIND_PIN_LIST
            | KIND_NIP65_RELAY_LIST_METADATA
            | KIND_BOOKMARK_LIST
            | KIND_FOLLOW_SET
            | KIND_BOOKMARK_SET
            | KIND_EMOJI_SET
            | KIND_EMOJI_LIST
            // Agent-owned addressable state.
            | KIND_AGENT_ENGRAM
            | KIND_EVENT_REMINDER
            | KIND_AGENT_PROFILE
            | KIND_PERSONA
            | KIND_TEAM
            | KIND_MANAGED_AGENT
            | KIND_TEAM_CATALOG
            | KIND_PRIVATE_MANAGED_AGENT
            | KIND_PUSH_LEASE
            // NIP-34 git events scope by `a` tag (repo), not `h` tag (channel).
            | KIND_GIT_REPO_ANNOUNCEMENT
            | KIND_GIT_REPO_STATE
            | KIND_GIT_PATCH
            | KIND_GIT_PULL_REQUEST
            | KIND_GIT_PR_UPDATE
            | KIND_GIT_ISSUE
            | KIND_GIT_STATUS_OPEN
            | KIND_GIT_STATUS_MERGED
            | KIND_GIT_STATUS_CLOSED
            | KIND_GIT_STATUS_DRAFT
            // NIP-MP projects group NIP-34 repos and scope the same way.
            | KIND_PROJECT
            // Community moderation commands are community-global, like the
            // NIP-43 9030-series.
            | KIND_MODERATION_BAN
            | KIND_MODERATION_UNBAN
            | KIND_MODERATION_TIMEOUT
            | KIND_MODERATION_UNTIMEOUT
            | KIND_MODERATION_RESOLVE_REPORT
            // NIP-43 admin commands and leave requests.
            | RELAY_ADMIN_ADD_MEMBER
            | RELAY_ADMIN_REMOVE_MEMBER
            | RELAY_ADMIN_CHANGE_ROLE
            | RELAY_ADMIN_SET_WORKSPACE_PROFILE
            | KIND_NIP43_LEAVE_REQUEST
            // NIP-IA requests drive relay-global archive state.
            | KIND_IA_ARCHIVE_REQUEST
            | KIND_IA_UNARCHIVE_REQUEST
            // NIP-56 abuse reports and private product feedback are never channel-scoped.
            | KIND_REPORT
            | KIND_PRODUCT_FEEDBACK
            // NIP-AM: the channel is inside the ciphertext, so there is no `h`.
            | KIND_AGENT_TURN_METRIC
    )
}

/// Does `kind` require an `h` tag naming the channel it belongs to?
///
/// Disjoint from [`is_global_only_kind`] — proven by
/// `global_only_and_channel_scoped_are_disjoint` below. Note that
/// [`KIND_NIP29_CREATE_GROUP`] is absent: it *creates* the channel, so it
/// cannot reference one.
#[must_use]
pub const fn requires_h_channel_scope(kind: Kind) -> bool {
    matches!(
        kind,
        KIND_STREAM_MESSAGE
            | KIND_STREAM_MESSAGE_V2
            | KIND_STREAM_MESSAGE_EDIT
            | KIND_STREAM_MESSAGE_PINNED
            | KIND_STREAM_MESSAGE_BOOKMARKED
            | KIND_STREAM_MESSAGE_SCHEDULED
            | KIND_STREAM_REMINDER
            | KIND_STREAM_MESSAGE_DIFF
            | KIND_CANVAS
            | KIND_FORUM_POST
            | KIND_FORUM_VOTE
            | KIND_FORUM_COMMENT
            | KIND_NIP29_PUT_USER
            | KIND_NIP29_REMOVE_USER
            | KIND_NIP29_EDIT_METADATA
            | KIND_NIP29_DELETE_EVENT
            | KIND_NIP29_DELETE_GROUP
            | KIND_NIP29_LEAVE_REQUEST
            | KIND_HUDDLE_STARTED
            | KIND_HUDDLE_PARTICIPANT_JOINED
            | KIND_HUDDLE_PARTICIPANT_LEFT
            | KIND_HUDDLE_ENDED
            | KIND_HUDDLE_LIVENESS
            | KIND_HUDDLE_GUIDELINES
            | KIND_PROCESS_COMMAND
            | KIND_PROCESS_RESULT
            // Channel commands and channel-bound signals: without an `h` tag
            // they skipped the channel write gate, dropped their effect, and
            // were stored on the community-global feed (finding_9d6952a3a7dfa9dd).
            | KIND_NIP29_JOIN_REQUEST
            | KIND_WORKFLOW_TRIGGER
            | KIND_DM_HIDE
            | KIND_TYPING_INDICATOR
            | KIND_HUDDLE_REACTION
    )
}

/// Is `kind` a workflow *execution* event (46001–46012)?
///
/// These must never trigger a workflow — that is what stops a workflow from
/// looping on its own output.
#[must_use]
pub const fn is_workflow_execution_kind(kind: Kind) -> bool {
    kind >= KIND_WORKFLOW_TRIGGERED && kind <= KIND_WORKFLOW_APPROVAL_DENIED
}

/// Is `kind` a NIP-43 relay membership admin command (9030–9032) or the buzz
/// workspace-profile command (9033)?
#[must_use]
pub const fn is_relay_admin_kind(kind: Kind) -> bool {
    matches!(
        kind,
        RELAY_ADMIN_ADD_MEMBER
            | RELAY_ADMIN_REMOVE_MEMBER
            | RELAY_ADMIN_CHANGE_ROLE
            | RELAY_ADMIN_SET_WORKSPACE_PROFILE
    )
}

/// Is `kind` a NIP-IA identity archival *request* (9035–9036)?
///
/// Only the user-signed request kinds match. The relay-signed deltas and
/// snapshot (8002/8003/13535) are emitted, never ingested as commands.
#[must_use]
pub const fn is_identity_archive_request_kind(kind: Kind) -> bool {
    matches!(kind, KIND_IA_ARCHIVE_REQUEST | KIND_IA_UNARCHIVE_REQUEST)
}

/// Is `kind` a command that must execute transactionally rather than simply
/// being stored?
#[must_use]
pub const fn is_command_kind(kind: Kind) -> bool {
    matches!(
        kind,
        KIND_WORKFLOW_DEF
            | KIND_DM_OPEN
            | KIND_DM_ADD_MEMBER
            | KIND_DM_HIDE
            | KIND_WORKFLOW_TRIGGER
            | KIND_APPROVAL_GRANT
            | KIND_APPROVAL_DENY
    )
}

/// Is `kind` a community moderation command (9040–9044)?
///
/// The canonical route check for the command dispatcher — narrower than
/// [`is_moderation_command`], which also counts kind:1984 reports because those
/// change what a moderator sees.
#[must_use]
pub const fn is_moderation_command_kind(kind: Kind) -> bool {
    matches!(
        kind,
        KIND_MODERATION_BAN
            | KIND_MODERATION_UNBAN
            | KIND_MODERATION_TIMEOUT
            | KIND_MODERATION_UNTIMEOUT
            | KIND_MODERATION_RESOLVE_REPORT
    )
}

// A delivery pump caches "can this reader see this channel?" answers. buzz
// invalidated those caches over a cross-pod bus; here the feed itself is the
// bus, and these three predicates decide which records force a re-read. They
// are deliberately over-inclusive: an unnecessary invalidation costs one
// lookup, a missed one serves a removed member their old channel.

/// Does `kind` change who belongs to a channel or community?
///
/// NIP-29 membership commands 9000 / 9001 / 9021 / 9022 (buzz `kind.rs`:275,
/// 277, 289, 291) plus the relay-signed membership notices 44100 / 44101
/// (buzz `kind.rs`:472, 476).
///
/// The NIP-43 relay-level admin commands are not here: they change community
/// membership, not channel membership, and are covered by the session gate.
#[must_use]
pub fn is_membership_change(kind: Kind) -> bool {
    matches!(
        kind,
        KIND_NIP29_PUT_USER
            | KIND_NIP29_REMOVE_USER
            | KIND_NIP29_JOIN_REQUEST
            | KIND_NIP29_LEAVE_REQUEST
            | KIND_MEMBER_ADDED_NOTIFICATION
            | KIND_MEMBER_REMOVED_NOTIFICATION
    )
}

/// Does `kind` carry a moderation decision or signal?
///
/// The 9040–9044 command block (buzz `kind.rs`:298–310) plus NIP-56 reports,
/// kind 1984 (buzz `kind.rs`:267). Reports are included because a report is
/// what makes a moderation queue visible; a cache that missed them would show a
/// stale queue.
///
/// Use [`is_moderation_command_kind`] instead when routing a command for
/// execution — a kind:1984 report is a signal, never an executable command.
#[must_use]
pub fn is_moderation_command(kind: Kind) -> bool {
    is_moderation_command_kind(kind) || kind == KIND_REPORT
}

/// Does `kind` change a channel's identity or visibility?
///
/// The NIP-29 lifecycle commands 9002 / 9007 / 9008 (buzz `kind.rs`:279, 283,
/// 285) plus the addressable group state 39000–39002 (buzz `kind.rs`:362, 364,
/// 366): metadata, admins, members.
///
/// [`KIND_NIP29_GROUP_ROLES`] (39003, buzz `kind.rs`:368) is deliberately
/// excluded: a role definition changes what a member may *do*, not what a
/// reader may *see*, so it does not invalidate a visibility cache.
#[must_use]
pub fn is_channel_metadata(kind: Kind) -> bool {
    matches!(
        kind,
        KIND_NIP29_EDIT_METADATA
            | KIND_NIP29_CREATE_GROUP
            | KIND_NIP29_DELETE_GROUP
            | KIND_NIP29_GROUP_METADATA
            | KIND_NIP29_GROUP_ADMINS
            | KIND_NIP29_GROUP_MEMBERS
    )
}

// These catch a mistyped constant at build time rather than at runtime, which
// matters because several of these numbers select an access gate.

const _: () = assert!(is_replaceable(KIND_AGENT_PROFILE)); // 10100 ∈ 10000–19999
const _: () = assert!(is_parameterized_replaceable(KIND_AGENT_ENGRAM)); // 30174
const _: () = assert!(is_parameterized_replaceable(KIND_PERSONA)); // 30175
const _: () = assert!(is_parameterized_replaceable(KIND_TEAM)); // 30176
const _: () = assert!(is_parameterized_replaceable(KIND_MANAGED_AGENT)); // 30177
const _: () = assert!(is_parameterized_replaceable(KIND_EVENT_REMINDER)); // 30300
const _: () = assert!(is_parameterized_replaceable(KIND_PUSH_LEASE)); // 30350
const _: () = assert!(is_parameterized_replaceable(KIND_WORKFLOW_DEF)); // 30620
const _: () = assert!(is_parameterized_replaceable(KIND_DM_VISIBILITY)); // 30622
const _: () = assert!(is_parameterized_replaceable(KIND_THREAD_SUMMARY)); // 39005
const _: () = assert!(is_parameterized_replaceable(KIND_WINDOW_BOUNDS)); // 39006
const _: () = assert!(is_parameterized_replaceable(KIND_GIT_REPO_ANNOUNCEMENT)); // 30617
const _: () = assert!(is_parameterized_replaceable(KIND_GIT_REPO_STATE)); // 30618

// The group projections MUST be parameterized-replaceable: the replacement
// engine is the only thing that retires a superseded roster, and `d` = the
// channel is the cell it replaces within.
const _: () = assert!(is_parameterized_replaceable(KIND_NIP29_GROUP_METADATA)); // 39000
const _: () = assert!(is_parameterized_replaceable(KIND_NIP29_GROUP_ADMINS)); // 39001
const _: () = assert!(is_parameterized_replaceable(KIND_NIP29_GROUP_MEMBERS)); // 39002
                                                                               // …and relay-only, or a client could forge a member list.
const _: () = assert!(is_relay_only_kind(KIND_NIP29_GROUP_METADATA));
const _: () = assert!(is_relay_only_kind(KIND_NIP29_GROUP_ADMINS));
const _: () = assert!(is_relay_only_kind(KIND_NIP29_GROUP_MEMBERS));
// …and never global-only: their access gate is their channel's.
const _: () = assert!(!is_global_only_kind(KIND_NIP29_GROUP_MEMBERS));

// Turn metrics are append-only: replacing or dropping them would corrupt the
// owner's billing history.
const _: () = assert!(is_regular(KIND_AGENT_TURN_METRIC));
const _: () = assert!(is_regular(KIND_STREAM_MESSAGE));
const _: () = assert!(!is_ephemeral(KIND_REPORT));

// Every registered kind fits nostr's u16-backed Kind on the wire.
const _: () = assert!(KIND_AUTH <= u16::MAX as Kind);
const _: () = assert!(KIND_MEDIA_UPLOAD <= u16::MAX as Kind);
const _: () = assert!(KIND_AGENT_TURN_METRIC <= u16::MAX as Kind);
const _: () = assert!(EPHEMERAL_KIND_MIN < EPHEMERAL_KIND_MAX);
const _: () = assert!(PARAM_REPLACEABLE_KIND_MIN < PARAM_REPLACEABLE_KIND_MAX);

const _: () = assert!(is_moderation_command_kind(KIND_MODERATION_BAN));
const _: () = assert!(is_moderation_command_kind(KIND_MODERATION_RESOLVE_REPORT));
const _: () = assert!(!is_moderation_command_kind(KIND_REPORT));
const _: () = assert!(is_persona(KIND_PERSONA));
const _: () = assert!(!is_persona(KIND_TEAM));

// The upstream v0.5.x additions (sync 001). Both are addressable state, both
// stay CLIENT-submittable (they are user-owned documents, not relay authority),
// and the team catalog joins the persona under the share gate.
const _: () = assert!(is_parameterized_replaceable(KIND_TEAM_CATALOG)); // 30178
const _: () = assert!(is_parameterized_replaceable(KIND_PROJECT)); // 30621
const _: () = assert!(is_shared_gated(KIND_PERSONA));
const _: () = assert!(is_shared_gated(KIND_TEAM_CATALOG));
const _: () = assert!(!is_shared_gated(KIND_TEAM));
const _: () = assert!(!is_shared_gated(KIND_MANAGED_AGENT));
const _: () = assert!(!is_relay_only_kind(KIND_TEAM_CATALOG));
const _: () = assert!(!is_relay_only_kind(KIND_PROJECT));

// The process cell is a CLIENT — the relay never signs
// process results, and these asserts keep the two authority models from being
// merged by a refactor. Both kinds are room-scoped, regular, client-signed;
// the result's authorship gate lives at ingest (`enforce_process_cell_authorship`),
// not in this registry.
const _: () = assert!(!is_relay_only_kind(KIND_PROCESS_COMMAND));
const _: () = assert!(!is_relay_only_kind(KIND_PROCESS_RESULT));
const _: () = assert!(requires_h_channel_scope(KIND_PROCESS_COMMAND));
const _: () = assert!(requires_h_channel_scope(KIND_PROCESS_RESULT));
const _: () = assert!(is_regular(KIND_PROCESS_COMMAND));
const _: () = assert!(is_regular(KIND_PROCESS_RESULT));
const _: () = assert!(!is_global_only_kind(KIND_PROCESS_COMMAND));
const _: () = assert!(!is_global_only_kind(KIND_PROCESS_RESULT));
const _: () = assert!(is_global_only_kind(KIND_TEAM_CATALOG));
const _: () = assert!(is_global_only_kind(KIND_PROJECT));

// Sync 002. NIP-PMA 30179 is owner-scoped global user data exactly as upstream
// classifies it (parameterized replaceable, global-only, client-signed); its
// author-only read gate is `AUTHOR_ONLY_KINDS`, pinned by the registry tests.
const _: () = assert!(is_parameterized_replaceable(KIND_PRIVATE_MANAGED_AGENT)); // 30179
const _: () = assert!(is_global_only_kind(KIND_PRIVATE_MANAGED_AGENT));
const _: () = assert!(!is_relay_only_kind(KIND_PRIVATE_MANAGED_AGENT));
const _: () = assert!(!is_shared_gated(KIND_PRIVATE_MANAGED_AGENT));
// Huddle liveness 48104 is relay-signed upstream and minted by nothing here:
// relay-only so a client cannot forge it, channel-scoped like the rest of the
// 4810x family, regular-shaped.
const _: () = assert!(is_relay_only_kind(KIND_HUDDLE_LIVENESS));
const _: () = assert!(requires_h_channel_scope(KIND_HUDDLE_LIVENESS));
const _: () = assert!(is_regular(KIND_HUDDLE_LIVENESS));
const _: () = assert!(!is_global_only_kind(KIND_HUDDLE_LIVENESS));

// The relay-signed NIP-IA deltas and snapshot: a client-submitted 13535 would
// be an archived-identity list of somebody else's invention, and NIP-IA treats
// the snapshot as authoritative.
const _: () = assert!(is_relay_only_kind(KIND_IA_ARCHIVED));
const _: () = assert!(is_relay_only_kind(KIND_IA_UNARCHIVED));
const _: () = assert!(is_relay_only_kind(KIND_IA_ARCHIVED_LIST));
// ...while the REQUESTS stay submittable, or nobody could ask to be archived.
const _: () = assert!(!is_relay_only_kind(KIND_IA_ARCHIVE_REQUEST));
const _: () = assert!(!is_relay_only_kind(KIND_IA_UNARCHIVE_REQUEST));

// The workflow lifecycle the relay emits...
const _: () = assert!(is_relay_only_kind(KIND_WORKFLOW_TRIGGERED));
const _: () = assert!(is_relay_only_kind(KIND_WORKFLOW_APPROVAL_REQUESTED));
// ...and the three things a PERSON sends, which must stay submittable: the two
// halves of an approval decision, and a manual trigger.
const _: () = assert!(!is_relay_only_kind(KIND_WORKFLOW_APPROVAL_GRANTED));
const _: () = assert!(!is_relay_only_kind(KIND_WORKFLOW_APPROVAL_DENIED));
const _: () = assert!(!is_relay_only_kind(KIND_WORKFLOW_TRIGGER));
const _: () = assert!(!is_relay_only_kind(KIND_WORKFLOW_DEF));

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn no_duplicate_kind_values() {
        let mut sorted: Vec<Kind> = ALL_KINDS.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "ALL_KINDS contains a duplicate value");
    }

    #[test]
    fn every_kind_has_exactly_one_storage_shape() {
        for &k in ALL_KINDS {
            let shapes = u8::from(is_ephemeral(k))
                + u8::from(is_replaceable(k))
                + u8::from(is_parameterized_replaceable(k))
                + u8::from(is_regular(k));
            assert_eq!(shapes, 1, "kind {k} has {shapes} storage shapes");
        }
    }

    #[test]
    fn access_set_members_are_registered() {
        // KIND_PUSH_LEASE is the one deliberate omission from ALL_KINDS,
        // matching buzz, where it is registered through its own module.
        for &k in AUTHOR_ONLY_KINDS {
            assert!(
                ALL_KINDS.contains(&k) || k == KIND_PUSH_LEASE,
                "author-only kind {k} is not registered"
            );
        }
        for &k in RESULT_GATED_KINDS {
            assert!(
                ALL_KINDS.contains(&k),
                "result-gated kind {k} is not registered"
            );
        }
        for &k in P_GATED_KINDS {
            assert!(ALL_KINDS.contains(&k), "p-gated kind {k} is not registered");
        }
        for &k in SHARED_GATED_KINDS {
            assert!(
                ALL_KINDS.contains(&k),
                "shared-gated kind {k} is not registered"
            );
        }
    }

    /// The const-fn spelling and the slice must never disagree about
    /// membership — one feeds compile asserts, the other registration tests.
    #[test]
    fn shared_gated_forms_agree() {
        for kind in 0..=65535u32 {
            assert_eq!(
                is_shared_gated(kind),
                SHARED_GATED_KINDS.contains(&kind),
                "kind {kind}"
            );
        }
        assert_eq!(SHARED_GATED_KINDS, &[KIND_PERSONA, KIND_TEAM_CATALOG]);
    }

    #[test]
    fn access_sets_hold_their_expected_members() {
        assert!(is_author_only(KIND_EVENT_REMINDER));
        assert!(is_author_only(KIND_PUSH_LEASE));
        assert!(is_author_only(KIND_PRIVATE_MANAGED_AGENT));
        assert!(is_author_only(KIND_REPORT));
        assert!(is_author_only(KIND_PRODUCT_FEEDBACK));
        assert!(!is_author_only(KIND_MANAGED_AGENT));
        assert!(!is_author_only(KIND_STREAM_MESSAGE));

        assert!(is_result_gated(KIND_DM_VISIBILITY));
        assert!(is_result_gated(KIND_AGENT_TURN_METRIC));
        assert!(!is_result_gated(KIND_STREAM_MESSAGE));

        assert!(is_p_gated(KIND_GIFT_WRAP));
        assert!(is_p_gated(KIND_AGENT_OBSERVER_FRAME));
        assert!(is_p_gated(KIND_MEMBER_ADDED_NOTIFICATION));
        assert!(is_p_gated(KIND_MEMBER_REMOVED_NOTIFICATION));
        assert!(is_p_gated(KIND_DM_VISIBILITY));
        assert!(is_p_gated(KIND_AGENT_TURN_METRIC));
        assert!(!is_p_gated(KIND_STREAM_MESSAGE));
    }

    #[test]
    fn range_predicates_classify_representatives() {
        // (kind, ephemeral, replaceable, param-replaceable, regular)
        let table: &[(Kind, bool, bool, bool, bool)] = &[
            (KIND_PROFILE, false, true, false, false),          // 0
            (KIND_TEXT_NOTE, false, false, false, true),        // 1
            (KIND_CONTACT_LIST, false, true, false, false),     // 3
            (KIND_STREAM_MESSAGE, false, false, false, true),   // 9
            (KIND_CHANNEL_METADATA, false, true, false, false), // 41
            (KIND_REPORT, false, false, false, true),           // 1984
            (KIND_NIP29_PUT_USER, false, false, false, true),   // 9000
            (KIND_MUTE_LIST, false, true, false, false),        // 10000
            (KIND_AGENT_PROFILE, false, true, false, false),    // 10100
            (KIND_NIP43_MEMBERSHIP_LIST, false, true, false, false), // 13534
            (KIND_PRESENCE_UPDATE, true, false, false, false),  // 20001
            (KIND_AGENT_OBSERVER_FRAME, true, false, false, false), // 24200
            (KIND_NIP43_LEAVE_REQUEST, true, false, false, false), // 28936
            (KIND_FOLLOW_SET, false, false, true, false),       // 30000
            (KIND_PERSONA, false, false, true, false),          // 30175
            (KIND_NIP29_GROUP_METADATA, false, false, true, false), // 39000
            (KIND_WINDOW_BOUNDS, false, false, true, false),    // 39006
            (KIND_STREAM_MESSAGE_V2, false, false, false, true), // 40002
            (KIND_AGENT_TURN_METRIC, false, false, false, true), // 44200
            (KIND_MEDIA_UPLOAD, false, false, false, true),     // 49001
        ];
        for &(k, eph, repl, param, reg) in table {
            assert_eq!(is_ephemeral(k), eph, "is_ephemeral({k})");
            assert_eq!(is_replaceable(k), repl, "is_replaceable({k})");
            assert_eq!(is_parameterized_replaceable(k), param, "is_param({k})");
            assert_eq!(is_regular(k), reg, "is_regular({k})");
        }
    }

    #[test]
    fn ephemeral_range_boundaries() {
        assert!(!is_ephemeral(19999));
        assert!(is_ephemeral(20000));
        assert!(is_ephemeral(29999));
        assert!(!is_ephemeral(30000));
    }

    #[test]
    fn replaceable_range_boundaries() {
        assert!(!is_replaceable(9999));
        assert!(is_replaceable(10000));
        assert!(is_replaceable(19999));
        assert!(!is_replaceable(20000));
        // The three singletons, and their neighbours which are not replaceable.
        assert!(is_replaceable(0));
        assert!(!is_replaceable(1));
        assert!(is_replaceable(3));
        assert!(!is_replaceable(4));
        assert!(is_replaceable(41));
        assert!(!is_replaceable(42));
    }

    #[test]
    fn parameterized_replaceable_range_boundaries() {
        assert!(!is_parameterized_replaceable(29999));
        assert!(is_parameterized_replaceable(30000));
        assert!(is_parameterized_replaceable(39999));
        assert!(!is_parameterized_replaceable(40000));
    }

    #[test]
    fn replaceable_and_parameterized_are_disjoint() {
        for kind in 0..=65535u32 {
            assert!(
                !(is_replaceable(kind) && is_parameterized_replaceable(kind)),
                "kind {kind} is both replaceable and parameterized replaceable"
            );
        }
    }

    #[test]
    fn global_only_and_channel_scoped_are_disjoint() {
        for kind in 0..=65535u32 {
            assert!(
                !(is_global_only_kind(kind) && requires_h_channel_scope(kind)),
                "kind {kind} is both global-only and channel-scoped"
            );
        }
    }

    #[test]
    fn relay_only_kinds_reject_client_authorship() {
        assert!(is_relay_only_kind(KIND_NIP43_MEMBERSHIP_LIST));
        assert!(is_relay_only_kind(KIND_CHANNEL_SUMMARY));
        assert!(is_relay_only_kind(KIND_PRESENCE_SNAPSHOT));
        assert!(is_relay_only_kind(KIND_DM_VISIBILITY));
        assert!(is_relay_only_kind(KIND_THREAD_SUMMARY));
        assert!(is_relay_only_kind(KIND_WINDOW_BOUNDS));
        // The group projections: forging one would forge a member list.
        assert!(is_relay_only_kind(KIND_NIP29_GROUP_METADATA));
        assert!(is_relay_only_kind(KIND_NIP29_GROUP_ADMINS));
        assert!(is_relay_only_kind(KIND_NIP29_GROUP_MEMBERS));
        // The relay-signed announcements and snapshots. 13535 is the one NIP-IA
        // calls authoritative, so a forged one rewrites who this community
        // considers retired.
        assert!(is_relay_only_kind(KIND_NIP43_MEMBER_ADDED));
        assert!(is_relay_only_kind(KIND_NIP43_MEMBER_REMOVED));
        assert!(is_relay_only_kind(KIND_IA_ARCHIVED));
        assert!(is_relay_only_kind(KIND_IA_UNARCHIVED));
        assert!(is_relay_only_kind(KIND_IA_ARCHIVED_LIST));
        // Huddle liveness is relay-synthesized upstream (sync 002); the rest of
        // the huddle family is a participant's own signed act and stays open.
        assert!(is_relay_only_kind(KIND_HUDDLE_LIVENESS));
        assert!(!is_relay_only_kind(KIND_HUDDLE_STARTED));
        assert!(!is_relay_only_kind(KIND_HUDDLE_ENDED));
        // The workflow lifecycle. A forged approval REQUEST would put a gate in
        // front of somebody with a token the forger chose.
        for k in [
            KIND_WORKFLOW_TRIGGERED,
            KIND_WORKFLOW_STEP_STARTED,
            KIND_WORKFLOW_STEP_COMPLETED,
            KIND_WORKFLOW_STEP_FAILED,
            KIND_WORKFLOW_COMPLETED,
            KIND_WORKFLOW_FAILED,
            KIND_WORKFLOW_CANCELLED,
            KIND_WORKFLOW_APPROVAL_REQUESTED,
        ] {
            assert!(is_relay_only_kind(k), "kind {k} is relay-emitted");
        }
        // User-signed kinds must stay submittable.
        assert!(!is_relay_only_kind(KIND_NIP43_LEAVE_REQUEST));
        assert!(!is_relay_only_kind(KIND_STREAM_MESSAGE));
        // A person asks to be archived, and answers an approval gate. Refusing
        // either would make the feature unusable rather than safe.
        assert!(!is_relay_only_kind(KIND_IA_ARCHIVE_REQUEST));
        assert!(!is_relay_only_kind(KIND_IA_UNARCHIVE_REQUEST));
        assert!(!is_relay_only_kind(KIND_WORKFLOW_APPROVAL_GRANTED));
        assert!(!is_relay_only_kind(KIND_WORKFLOW_APPROVAL_DENIED));
        assert!(!is_relay_only_kind(KIND_WORKFLOW_TRIGGER));
        assert!(!is_relay_only_kind(KIND_WORKFLOW_DEF));
        // The client-signed NIP-29 COMMANDS stay submittable: they are what a
        // member sends to change a roster the relay then projects.
        assert!(!is_relay_only_kind(KIND_NIP29_PUT_USER));
        assert!(!is_relay_only_kind(KIND_NIP29_REMOVE_USER));
        assert!(!is_relay_only_kind(KIND_NIP29_JOIN_REQUEST));
        assert!(!is_relay_only_kind(KIND_NIP29_LEAVE_REQUEST));
        assert!(!is_relay_only_kind(KIND_NIP29_EDIT_METADATA));
    }

    #[test]
    fn group_projections_are_exactly_39000_to_39002() {
        assert!(is_group_projection(39000));
        assert!(is_group_projection(39001));
        assert!(is_group_projection(39002));
        // 39003 (roles) is not one: nothing mints it.
        assert!(!is_group_projection(KIND_NIP29_GROUP_ROLES));
        assert!(!is_group_projection(38999));
        assert!(!is_group_projection(KIND_THREAD_SUMMARY));
        assert!(!is_group_projection(KIND_NIP29_PUT_USER));
        // Every projection is also channel metadata (it changes what a reader
        // may see), which is what makes a pump invalidate its access cache.
        for k in [
            KIND_NIP29_GROUP_METADATA,
            KIND_NIP29_GROUP_ADMINS,
            KIND_NIP29_GROUP_MEMBERS,
        ] {
            assert!(is_channel_metadata(k), "kind {k} must invalidate access");
        }
    }

    #[test]
    fn global_only_covers_user_owned_state() {
        for k in [
            KIND_PROFILE,
            KIND_CONTACT_LIST,
            KIND_LONG_FORM,
            KIND_USER_STATUS,
            KIND_READ_STATE,
            KIND_MUTE_LIST,
            KIND_EMOJI_SET,
            KIND_PERSONA,
            KIND_TEAM,
            KIND_MANAGED_AGENT,
            KIND_TEAM_CATALOG,
            KIND_PRIVATE_MANAGED_AGENT,
            KIND_REPORT,
            KIND_PRODUCT_FEEDBACK,
            KIND_PUSH_LEASE,
            KIND_AGENT_TURN_METRIC,
            KIND_MODERATION_BAN,
            KIND_IA_ARCHIVE_REQUEST,
            KIND_GIT_PATCH,
            KIND_PROJECT,
        ] {
            assert!(is_global_only_kind(k), "kind {k} must be global-only");
        }
        assert!(!is_global_only_kind(KIND_STREAM_MESSAGE));
    }

    #[test]
    fn channel_scope_required_for_stream_content_but_not_group_creation() {
        assert!(requires_h_channel_scope(KIND_STREAM_MESSAGE));
        assert!(requires_h_channel_scope(KIND_CANVAS));
        assert!(requires_h_channel_scope(KIND_FORUM_POST));
        assert!(requires_h_channel_scope(KIND_HUDDLE_STARTED));
        // Creating a group cannot reference the channel it is about to make.
        assert!(!requires_h_channel_scope(KIND_NIP29_CREATE_GROUP));
    }

    #[test]
    fn workflow_execution_range() {
        assert!(!is_workflow_execution_kind(46000));
        assert!(is_workflow_execution_kind(KIND_WORKFLOW_TRIGGERED));
        assert!(is_workflow_execution_kind(KIND_WORKFLOW_APPROVAL_DENIED));
        assert!(!is_workflow_execution_kind(KIND_WORKFLOW_TRIGGER));
        assert!(!is_workflow_execution_kind(KIND_APPROVAL_GRANT));
    }

    #[test]
    fn command_and_admin_predicates() {
        assert!(is_command_kind(KIND_WORKFLOW_DEF));
        assert!(is_command_kind(KIND_DM_OPEN));
        assert!(!is_command_kind(KIND_STREAM_MESSAGE));

        assert!(is_relay_admin_kind(RELAY_ADMIN_ADD_MEMBER));
        assert!(is_relay_admin_kind(RELAY_ADMIN_SET_WORKSPACE_PROFILE));
        assert!(!is_relay_admin_kind(KIND_NIP29_PUT_USER));

        assert!(is_identity_archive_request_kind(KIND_IA_ARCHIVE_REQUEST));
        assert!(is_identity_archive_request_kind(KIND_IA_UNARCHIVE_REQUEST));
        assert!(!is_identity_archive_request_kind(KIND_IA_ARCHIVED));
    }

    #[test]
    fn is_persona_accepts_only_30175() {
        assert!(is_persona(KIND_PERSONA));
        assert_eq!(KIND_PERSONA, 30175);
        assert!(!is_persona(KIND_TEAM));
        assert!(!is_persona(KIND_MANAGED_AGENT));
        assert!(!is_persona(KIND_STREAM_MESSAGE));
    }

    #[test]
    fn is_membership_change_accepts_its_members() {
        for k in [9000, 9001, 9021, 9022, 44100, 44101] {
            assert!(is_membership_change(k), "kind {k} changes membership");
        }
        assert!(is_membership_change(KIND_GROUP_ADD_USER));
        assert!(is_membership_change(KIND_GROUP_REMOVE_USER));
        assert!(!is_membership_change(KIND_STREAM_MESSAGE));
        // Group *creation* and metadata edits are channel metadata, not
        // membership.
        assert!(!is_membership_change(KIND_NIP29_CREATE_GROUP));
        assert!(!is_membership_change(KIND_NIP29_EDIT_METADATA));
    }

    #[test]
    fn is_moderation_command_accepts_commands_and_reports() {
        for k in 9040..=9044 {
            assert!(is_moderation_command(k), "kind {k} is a moderation command");
        }
        assert!(is_moderation_command(KIND_REPORT));
        assert_eq!(KIND_REPORT, 1984);
        assert!(!is_moderation_command(9039));
        assert!(!is_moderation_command(9045));
        assert!(!is_moderation_command(KIND_STREAM_MESSAGE));
    }

    #[test]
    fn is_channel_metadata_accepts_its_members() {
        for k in [9002, 9007, 9008] {
            assert!(is_channel_metadata(k), "kind {k} is channel metadata");
        }
        for k in 39000..=39002 {
            assert!(is_channel_metadata(k), "kind {k} is channel metadata");
        }
        // Roles change authority, not visibility.
        assert!(!is_channel_metadata(KIND_NIP29_GROUP_ROLES));
        assert!(!is_channel_metadata(KIND_STREAM_MESSAGE));
    }
}
