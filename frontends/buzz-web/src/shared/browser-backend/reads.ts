/**
 * Relay-backed reads and writes: a channel's older messages, a forum thread,
 * reaction tallies, a multi-author note timeline, an avatar change and a
 * persona publication.
 *
 * Reads go through the client's own live relay connection (NIP-01 filters),
 * which is the same door the rest of the client already uses; that keeps the
 * access decision the relay's, exactly once.
 *
 * The paging discipline is `notes.ts`'s, deliberately: order by
 * `(created_at DESC, id ASC)`, page on the pair, and treat a FULL upstream page
 * as "probably more behind it". Two rows in one second are common in chat, and
 * a cursor that cannot tell them apart either repeats a row forever or skips
 * one silently.
 */
import type { RelayEvent } from "@/shared/api/types";
import { CHANNEL_TIMELINE_CONTENT_KINDS } from "@/shared/constants/kinds";
import {
  publishSigned,
  relayEvents,
  type RelayIdentity,
} from "@/shared/browser-backend/relayConnection";

/** The relay clamps a REQ `limit` to 500. */
const PAGE = 500;

/** NIP-25 reaction. */
const KIND_REACTION = 7;
/** NIP-01 text note (buzz's "pulse"). */
const KIND_NOTE = 1;
/** Forum root post. */
const KIND_FORUM_POST = 45001;
/** Forum comment — a reply inside a thread. */
const KIND_FORUM_COMMENT = 45003;

/**
 * The kinds a channel TIMELINE renders.
 *
 * Enumerated rather than "everything with this `h` tag", because a channel also
 * carries membership commands, huddle lifecycle and canvas events; handing
 * those to a message list would render join notices as chat. These are the
 * message-shaped kinds: chat (9), its v2 form, edits, diffs, and the system
 * lines a room legitimately shows.
 */
const CHANNEL_MESSAGE_KINDS = [...CHANNEL_TIMELINE_CONTENT_KINDS];

/** The live relay connection, as `updateProfileAtRelay` uses it. */
const defaultProfileRelay = {
  events: relayEvents,
  publish: (signed: unknown): Promise<void> =>
    publishSigned(
      signed,
      "Failed to publish your profile.",
      "Publishing your profile timed out.",
    ),
};

/** Newest first, with an id tie-break — the order every cursor here pages on. */
export function newestFirst(
  a: { created_at: number; id: string },
  b: { created_at: number; id: string },
): number {
  if (a.created_at !== b.created_at) return b.created_at - a.created_at;
  return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
}

/** A caller's resume position in `(second, id)` space. */
export type Cursor = { before: number; beforeId: string } | null;

/** The cursor a caller supplied, if it is usable. */
export function cursorFrom(args: {
  before?: number | null;
  beforeId?: string | null;
}): Cursor {
  const before = args.before;
  if (typeof before !== "number" || before <= 0) return null;
  // A timestamp with no id is a cursor from an older caller: read as "strictly
  // before this second", which may skip same-second rows rather than repeat
  // them forever. Repeating forever is the failure with no exit.
  return { before, beforeId: args.beforeId ?? "" };
}

/**
 * One notes-timeline page: at most `perUser` notes per author, past `cursor`.
 * Rows the cursor has already returned (the inclusive `until` second) are
 * dropped before the per-author cap, so they cannot use up an author's quota
 * and push that author's unseen older notes off the page.
 */
export function notesTimelinePage<
  T extends { pubkey: string; created_at: number; id: string },
>(
  events: T[],
  perUser: number,
  cursor: Cursor,
  full: boolean,
): { rows: T[]; next: { created_at: number; id: string } | null } {
  const capped = capPerAuthor(
    events.filter((event) => pastCursor(event, cursor)),
    perUser,
  );
  return pageOf(capped, capped.length, cursor, full);
}

/** Is this row strictly after `cursor` in the page's own order? */
export function pastCursor(
  event: { created_at: number; id: string },
  cursor: Cursor,
): boolean {
  if (!cursor) return true;
  if (event.created_at !== cursor.before) return event.created_at < cursor.before;
  return cursor.beforeId !== "" && event.id > cursor.beforeId;
}

/**
 * One page out of a fetched set, plus the cursor that resumes after it.
 *
 * `capped` says the upstream query returned as many rows as it was allowed to,
 * which is the case where there is most likely more behind it — the cap is what
 * produced the full result. Reading a full page as exhaustion is how a timeline
 * silently ends.
 */
export function pageOf<T extends { created_at: number; id: string }>(
  events: T[],
  limit: number,
  cursor: Cursor,
  capped: boolean,
): { rows: T[]; next: { created_at: number; id: string } | null } {
  const sorted = [...events].filter((e) => pastCursor(e, cursor)).sort(newestFirst);
  const rows = sorted.slice(0, limit);
  const last = rows[rows.length - 1];
  return {
    rows,
    next:
      last && (sorted.length > rows.length || capped)
        ? { created_at: last.created_at, id: last.id }
        : null,
  };
}

/** The `until` bound for a cursor — INCLUSIVE, because NIP-01's `until` is. */
export function untilFrom(cursor: Cursor): Record<string, number> {
  return cursor ? { until: cursor.before } : {};
}

/** Whether a reply carries buzz's broadcast marker, `["broadcast","1"]`. */
export function isBroadcast(tags: string[][]): boolean {
  return tags.some((tag) => tag[0] === "broadcast" && tag[1] === "1");
}

/**
 * A thread cursor, `"<seconds>:<id>"`.
 *
 * The id half is load-bearing rather than decoration: this API's cursor is a
 * single string, and a bare timestamp cannot tell a reply already shown from
 * another in the same second — which in a busy thread means either repeating
 * rows forever or dropping them. Carrying the id makes the boundary exact,
 * using the same `(created_at, id)` order every other read here pages on.
 */
export function threadCursorFrom(raw: unknown): Cursor {
  if (typeof raw !== "string" || raw === "") return null;
  const cut = raw.indexOf(":");
  const seconds = cut === -1 ? raw : raw.slice(0, cut);
  const id = cut === -1 ? "" : raw.slice(cut + 1);
  const before = Number(seconds);
  if (!Number.isFinite(before) || before <= 0) return null;
  return { before, beforeId: id };
}

/** The cursor string that resumes after `next`. */
export function threadCursorOf(
  next: { created_at: number; id: string } | null,
): string | null {
  return next ? `${next.created_at}:${next.id}` : null;
}

/** The `e` tag a NIP-10 reply names as its PARENT, if it marks one. */
/**
 * Whether an event that tags `rootId` is a reply in that thread, by NIP-10:
 * a marked `root` or `reply` reference to it, or in the deprecated unmarked
 * form the first (root) or last (reply) `e` tag. A `mention`-marked or other
 * reference is a quote, not a reply.
 */
export function isThreadReplyOf(tags: string[][], rootId: string): boolean {
  const eTags = tags.filter((tag) => tag[0] === "e" && typeof tag[1] === "string");
  const marked = eTags.filter((tag) => tag[3] === "root" || tag[3] === "reply");
  if (marked.length > 0) return marked.some((tag) => tag[1] === rootId);
  const positional = eTags.filter((tag) => tag[3] === undefined || tag[3] === "");
  if (positional.length === 0) return false;
  return positional[0][1] === rootId || positional[positional.length - 1][1] === rootId;
}

export function parentOf(tags: string[][], rootId: string): string | null {
  // NIP-10 marked form first: an explicit "reply" marker is unambiguous.
  for (const tag of tags) {
    if (tag[0] === "e" && tag[3] === "reply" && typeof tag[1] === "string") {
      return tag[1];
    }
  }
  // Then the marked root, which means "a direct reply to the root".
  for (const tag of tags) {
    if (tag[0] === "e" && tag[3] === "root" && typeof tag[1] === "string") {
      return tag[1];
    }
  }
  // Unmarked (deprecated positional) form: the LAST `e` tag is the parent.
  const es = tags.filter((tag) => tag[0] === "e" && typeof tag[1] === "string");
  const last = es[es.length - 1];
  return last ? last[1] : rootId ? rootId : null;
}

/**
 * Depth for every reply in a thread, as the RELAY derives it: a root is 0 and a
 * reply is `parent.depth + 1`.
 *
 * Computed over the fetched set, so a reply whose parent is not in the set
 * (fetched a page later, or deleted) falls back to depth 2 — upstream's own
 * fallback for "a reply to a reply we do not hold". Guessing 1 instead would
 * promote it to the top level of a window, which is a visible wrong answer.
 */
export function depthsFor(
  rootId: string,
  replies: Array<{ id: string; tags: string[][] }>,
): Map<string, number> {
  const parents = new Map<string, string | null>();
  for (const reply of replies) parents.set(reply.id, parentOf(reply.tags, rootId));
  const depths = new Map<string, number>();
  const resolve = (id: string, seen: Set<string>): number => {
    const known = depths.get(id);
    if (known !== undefined) return known;
    const parent = parents.get(id) ?? null;
    let depth: number;
    if (parent === null || parent === rootId) {
      depth = 1;
    } else if (!parents.has(parent)) {
      depth = 2; // a parent we do not hold
    } else if (seen.has(parent)) {
      depth = 2; // a tag cycle: refuse to loop, and do not claim top level
    } else {
      seen.add(id);
      depth = resolve(parent, seen) + 1;
    }
    depths.set(id, depth);
    return depth;
  };
  for (const reply of replies) resolve(reply.id, new Set());
  return depths;
}

/**
 * The event a NIP-25 reaction targets: the LAST `e` tag.
 *
 * Not "every `e` tag", which is the trap this function exists to avoid. A
 * reaction to a threaded message carries the root as well as its parent (NIP-10
 * marks both), so counting one reaction against every `e` tag it mentions
 * credits the root with a reaction nobody gave it — and the same reaction is
 * then double-counted across two notes. NIP-25 is explicit that the last `e`
 * tag is the target, which is also how the relay's own reaction tallies key
 * their rows.
 */
export function reactionTarget(tags: string[][]): string | null {
  let target: string | null = null;
  for (const tag of tags) {
    if (tag[0] === "e" && typeof tag[1] === "string") target = tag[1];
  }
  return target;
}

/** Tally reactions by `(target, emoji)`, deduplicating by author. */
export function tallyReactions(
  events: Array<{ pubkey: string; content: string; tags: string[][] }>,
  wanted: string[],
): Array<{ note_id: string; emoji: string; count: number; pubkeys: string[] }> {
  const keep = new Set(wanted);
  // Keyed by target then emoji, so one author reacting twice with the same
  // emoji counts once — a relay may hold a re-published reaction.
  const byTarget = new Map<string, Map<string, Set<string>>>();
  for (const event of events) {
    const emoji = (event.content ?? "").trim();
    if (!emoji) continue;
    const target = reactionTarget(event.tags ?? []);
    if (!target || !keep.has(target)) continue;
    const emojis = byTarget.get(target) ?? new Map<string, Set<string>>();
    const authors = emojis.get(emoji) ?? new Set<string>();
    authors.add(event.pubkey);
    emojis.set(emoji, authors);
    byTarget.set(target, emojis);
  }
  const out: Array<{ note_id: string; emoji: string; count: number; pubkeys: string[] }> = [];
  for (const [noteId, emojis] of byTarget) {
    for (const [emoji, authors] of emojis) {
      out.push({
        note_id: noteId,
        emoji,
        count: authors.size,
        pubkeys: [...authors].sort(),
      });
    }
  }
  // Stable order so a re-read does not reshuffle a rendered row.
  return out.sort((a, b) =>
    a.note_id === b.note_id
      ? a.emoji < b.emoji
        ? -1
        : a.emoji > b.emoji
          ? 1
          : 0
      : a.note_id < b.note_id
        ? -1
        : 1,
  );
}

/** Keep at most `perAuthor` newest rows per author. */
export function capPerAuthor<T extends { pubkey: string; created_at: number; id: string }>(
  events: T[],
  perAuthor: number,
): T[] {
  const byAuthor = new Map<string, T[]>();
  for (const event of [...events].sort(newestFirst)) {
    const rows = byAuthor.get(event.pubkey) ?? [];
    if (rows.length < perAuthor) rows.push(event);
    byAuthor.set(event.pubkey, rows);
  }
  return [...byAuthor.values()].flat().sort(newestFirst);
}

/** Only 64-hex pubkeys, lowercased — a malformed one would filter to nothing. */
export function pubkeysOf(values: unknown): string[] {
  if (!Array.isArray(values)) return [];
  return values
    .filter((v): v is string => typeof v === "string")
    .map((v) => v.toLowerCase())
    .filter((v) => /^[0-9a-f]{64}$/.test(v));
}

/**
 * `get_channel_messages_before` — a channel's older history.
 *
 * The scroll-back half of the timeline. `get_channel_window` loads the head;
 * this loads what came before a cursor.
 */
export async function getChannelMessagesBefore(args: {
  channelId?: string;
  before?: number | null;
  beforeId?: string | null;
  limit?: number | null;
}): Promise<{
  events: RelayEvent[];
  next_cursor: { created_at: number; event_id: string } | null;
}> {
  const channelId = args.channelId ?? "";
  if (!channelId) return { events: [], next_cursor: null };
  const limit = Math.min(Math.max(args.limit ?? 50, 1), PAGE);
  const cursor = cursorFrom(args);
  const events = await relayEvents({
    kinds: CHANNEL_MESSAGE_KINDS,
    "#h": [channelId],
    limit: PAGE,
    ...untilFrom(cursor),
  });
  const { rows, next } = pageOf(events, limit, cursor, events.length >= PAGE);
  return {
    events: rows,
    next_cursor: next ? { created_at: next.created_at, event_id: next.id } : null,
  };
}

type RawThreadSummary = {
  reply_count: number;
  descendant_count: number;
  last_reply_at: number | null;
  participants: string[];
};

type RawForumPost = {
  event_id: string;
  pubkey: string;
  content: string;
  kind: number;
  created_at: number;
  channel_id: string;
  tags: string[][];
  sig: string;
  thread_summary: RawThreadSummary | null;
  reactions: unknown;
};

type RawThreadReply = {
  event_id: string;
  pubkey: string;
  content: string;
  kind: number;
  created_at: number;
  channel_id: string;
  tags: string[][];
  sig: string;
  parent_event_id: string | null;
  root_event_id: string | null;
  depth: number;
  broadcast: boolean;
  reactions: unknown;
};

function channelOf(event: RelayEvent): string {
  for (const tag of event.tags ?? []) {
    if (tag[0] === "h" && typeof tag[1] === "string") return tag[1];
  }
  return "";
}

/**
 * `get_forum_thread` — one forum post and its replies.
 *
 * Paged newest-first on `(created_at, id)`, exactly like every other read here,
 * and only then sorted oldest-first for display. Fetching a newest-bounded page
 * and slicing it from the oldest end would return rows the cursor then walks
 * over again — a thread that repeats itself as you page.
 *
 * Two honest limits:
 *
 * * `reactions` is null and left to the client's own per-target read, exactly as
 *   `getForumPosts` does: inventing tallies here would be a second opinion about
 *   a count the reactions route already owns.
 * * `total_replies` is the number of replies this read could SEE. It is the true
 *   total whenever `next_cursor` is null; on a thread deeper than one relay page
 *   it undercounts until the caller pages on. The desktop reports a true total
 *   from its local mirror — a tab has no mirror, and an invented total would be
 *   worse than one that grows as you read.
 */
export async function getForumThread(args: {
  channelId?: string;
  eventId?: string;
  limit?: number | null;
  cursor?: string | null;
}): Promise<{
  root: RawForumPost;
  replies: RawThreadReply[];
  total_replies: number;
  next_cursor: string | null;
}> {
  const eventId = args.eventId ?? "";
  if (!eventId) throw new Error("get_forum_thread needs an eventId.");
  const channelId = args.channelId ?? "";
  const limit = Math.min(Math.max(args.limit ?? 100, 1), PAGE);
  const cursor = threadCursorFrom(args.cursor);

  const [roots, replyEvents] = await Promise.all([
    relayEvents({ ids: [eventId], kinds: [KIND_FORUM_POST], limit: 1 } as never),
    relayEvents({
      kinds: [KIND_FORUM_COMMENT, KIND_FORUM_POST, 9],
      "#e": [eventId],
      ...(channelId ? { "#h": [channelId] } : {}),
      limit: PAGE,
      ...untilFrom(cursor),
    } as never),
  ]);
  const root = roots.find(
    (event) =>
      event.id === eventId &&
      event.kind === KIND_FORUM_POST &&
      (!channelId || channelOf(event) === channelId),
  );
  if (!root) throw new Error("That post is not on this relay.");

  // The root itself can come back in an `#e` query when something tags it in a
  // way that names itself; drop it rather than render the post as its own reply.
  // An event that only quotes the post, or that lives in another channel, is
  // not a reply in this thread.
  const replies = replyEvents.filter(
    (event) =>
      event.id !== eventId &&
      (!channelId || channelOf(event) === channelId) &&
      isThreadReplyOf(event.tags ?? [], eventId),
  );
  const { rows, next } = pageOf(replies, limit, cursor, replyEvents.length >= PAGE);
  // Depths are computed over the WHOLE fetched set rather than the page, so a
  // reply whose parent sits just outside the page still resolves.
  const depths = depthsFor(
    eventId,
    replies.map((event) => ({ id: event.id, tags: event.tags ?? [] })),
  );
  // Oldest-first for DISPLAY only: a thread reads top to bottom.
  const shown = [...rows].sort(
    (a, b) => a.created_at - b.created_at || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0),
  );
  const newestReply = [...replies].sort(newestFirst)[0];

  return {
    root: {
      event_id: root.id,
      pubkey: root.pubkey,
      content: root.content ?? "",
      kind: root.kind,
      created_at: root.created_at,
      channel_id: channelId || channelOf(root),
      tags: root.tags ?? [],
      sig: (root as RelayEvent & { sig?: string }).sig ?? "",
      thread_summary: {
        reply_count: replies.length,
        descendant_count: replies.length,
        last_reply_at: newestReply ? newestReply.created_at : null,
        participants: [
          ...new Set([...replies].sort(newestFirst).map((event) => event.pubkey)),
        ],
      },
      reactions: null,
    },
    replies: shown.map((event) => ({
      event_id: event.id,
      pubkey: event.pubkey,
      content: event.content ?? "",
      kind: event.kind,
      created_at: event.created_at,
      channel_id: channelId || channelOf(event),
      tags: event.tags ?? [],
      sig: (event as RelayEvent & { sig?: string }).sig ?? "",
      parent_event_id: parentOf(event.tags ?? [], eventId),
      root_event_id: eventId,
      depth: depths.get(event.id) ?? 2,
      broadcast: isBroadcast(event.tags ?? []),
      reactions: null,
    })),
    total_replies: replies.length,
    next_cursor: threadCursorOf(next),
  };
}

/**
 * How many note ids one reaction query asks about, and how many pages it will
 * walk per batch.
 *
 * A chunk of ids can attract far more than one page of reactions — a popular
 * note alone can — and a clamped read would silently drop whatever fell off the
 * end, producing a tally that is WRONG rather than short. So ids are asked
 * about in small batches and each batch is walked to exhaustion.
 */
const REACTION_ID_CHUNK = 40;
const REACTION_MAX_PAGES = 40;

/** `get_note_reactions` — tallies for a batch of notes. */
export async function getNoteReactions(args: {
  noteIds?: unknown;
}): Promise<Array<{ note_id: string; emoji: string; count: number; pubkeys: string[] }>> {
  const ids = Array.isArray(args.noteIds)
    ? args.noteIds.filter((v): v is string => typeof v === "string")
    : [];
  if (ids.length === 0) return [];
  const tallies: Array<{
    note_id: string;
    emoji: string;
    count: number;
    pubkeys: string[];
  }> = [];
  for (let at = 0; at < ids.length; at += REACTION_ID_CHUNK) {
    const chunk = ids.slice(at, at + REACTION_ID_CHUNK);
    // Every reaction on this batch, walked page by page. Deduplicated by event
    // id because an inclusive `until` re-reads the boundary second.
    const seen = new Map<string, { pubkey: string; content: string; tags: string[][] }>();
    let until: number | undefined;
    let exhausted = false;
    for (let page = 0; page < REACTION_MAX_PAGES; page += 1) {
      const events = await relayEvents({
        kinds: [KIND_REACTION],
        "#e": chunk,
        limit: PAGE,
        ...(until === undefined ? {} : { until }),
      });
      for (const event of events) {
        seen.set(event.id, {
          pubkey: event.pubkey,
          content: event.content ?? "",
          tags: event.tags ?? [],
        });
      }
      if (events.length < PAGE) {
        exhausted = true;
        break;
      }
      const oldest = events.reduce(
        (low, event) => Math.min(low, event.created_at),
        Number.POSITIVE_INFINITY,
      );
      if (!Number.isFinite(oldest) || oldest === until) {
        // A full page inside ONE second: `until` cannot step past it, so there
        // is no way to continue. Said out loud rather than returning a count
        // that is quietly missing everything older.
        throw new Error(
          "These notes have too many reactions in a single second for the relay's " +
            "timestamp cursor to page past, so their tallies cannot be counted exactly.",
        );
      }
      until = oldest;
    }
    if (!exhausted) {
      throw new Error(
        "These notes have more reactions than the tally reader will walk, so their " +
          "counts cannot be reported exactly.",
      );
    }
    tallies.push(...tallyReactions([...seen.values()], chunk));
  }
  return tallies;
}

/** `get_notes_timeline` — several authors' notes, newest first. */
export async function getNotesTimeline(args: {
  pubkeys?: unknown;
  limitPerUser?: number | null;
  before?: number | null;
  beforeId?: string | null;
}): Promise<{
  notes: Array<{
    id: string;
    pubkey: string;
    created_at: number;
    content: string;
    tags: string[][];
  }>;
  next_cursor: { before: number; before_id: string } | null;
}> {
  const authors = pubkeysOf(args.pubkeys);
  if (authors.length === 0) return { notes: [], next_cursor: null };
  const perUser = Math.min(Math.max(args.limitPerUser ?? 20, 1), PAGE);
  const cursor = cursorFrom(args);
  const events = await relayEvents({
    kinds: [KIND_NOTE],
    authors,
    limit: PAGE,
    ...untilFrom(cursor),
  });
  // The per-author cap is applied BEFORE paging, because it is part of what the
  // caller asked for rather than a display limit: "at most N per user".
  const { rows, next } = notesTimelinePage(events, perUser, cursor, events.length >= PAGE);
  return {
    notes: rows.map((event) => ({
      id: event.id,
      pubkey: event.pubkey,
      created_at: event.created_at,
      content: event.content ?? "",
      tags: event.tags ?? [],
    })),
    next_cursor: next ? { before: next.created_at, before_id: next.id } : null,
  };
}

/**
 * `update_profile_at_relay` — change this identity's avatar.
 *
 * Reads the CURRENT kind 0 first and republishes it with the new picture,
 * because a kind 0 is replaceable in whole: publishing only the avatar would
 * silently erase the display name and about text. `expectedPubkey` must be this
 * identity — a mismatch means the caller is trying to change somebody else's
 * profile, which is refused rather than quietly retargeted at our own.
 *
 * `expectedAvatarUrl` is the compare-and-set guard for a save that is deferred
 * and retried (`avatarProfileSync.ts` holds it until the upload is ready, then
 * retries for minutes): it is the picture the caller last saw. When the current
 * kind 0 carries a different picture, the avatar changed in the meantime, and
 * republishing would revert that change; the save is refused instead (unless
 * the current picture is already the requested one, which a lost reply to an
 * earlier attempt leaves behind). Absent means no expectation.
 *
 * The current event's tags are carried over (a kind 0 is replaced in whole, so
 * `tags: []` would erase claims such as NIP-39 `i` tags). `relay` is injectable
 * so the rule can be driven in node.
 */
export async function updateProfileAtRelay(
  identity: RelayIdentity,
  args: {
    expectedPubkey?: string;
    expectedAvatarUrl?: string | null;
    avatarUrl?: string;
  },
  relay: {
    events(filter: unknown): Promise<RelayEvent[]>;
    publish(signed: unknown): Promise<unknown>;
  } = defaultProfileRelay,
): Promise<{
  pubkey: string;
  display_name: string | null;
  avatar_url: string | null;
  about: string | null;
  nip05_handle: string | null;
  owner_pubkey: string | null;
  has_profile_event: boolean;
}> {
  const me = await identity.pubkey();
  const expected = (args.expectedPubkey ?? "").toLowerCase();
  if (expected && expected !== me.toLowerCase()) {
    throw new Error(
      "That profile belongs to another identity, and this tab holds only yours.",
    );
  }
  const existing = await relay.events({ kinds: [0], authors: [me], limit: 1 });
  let current: Record<string, unknown> = {};
  const newest = [...existing].sort(newestFirst)[0];
  if (newest?.content) {
    try {
      const parsed = JSON.parse(newest.content) as unknown;
      if (parsed && typeof parsed === "object") current = parsed as Record<string, unknown>;
    } catch {
      // A profile whose content is not JSON is not something to preserve; the
      // republished one is at least well-formed.
    }
  }
  const avatarUrl = args.avatarUrl ?? "";
  if (args.expectedAvatarUrl !== undefined) {
    const picture = typeof current.picture === "string" ? current.picture : "";
    const expectedPicture = args.expectedAvatarUrl ?? "";
    if (picture !== expectedPicture && picture !== avatarUrl) {
      throw new Error(
        "Your avatar changed since this save was queued, so the older picture was not applied.",
      );
    }
  }
  const tags = (newest?.tags ?? []).filter(
    (tag): tag is string[] =>
      Array.isArray(tag) && tag.every((value) => typeof value === "string"),
  );
  const content = JSON.stringify({ ...current, picture: avatarUrl });
  const signed = (await identity.sign({ kind: 0, content, tags })) as unknown as {
    id: string;
    pubkey?: string;
  };
  // The profile read above is an await: an identity switched meanwhile must
  // not receive this profile (finding_5c8ab8cc99005093).
  if (String(signed.pubkey ?? "").toLowerCase() !== me.toLowerCase()) {
    throw new Error("The signing identity changed before this profile save; it was not applied.");
  }
  await relay.publish(signed);
  const name = current.display_name ?? current.name;
  return {
    pubkey: me,
    display_name: typeof name === "string" ? name : null,
    avatar_url: avatarUrl || null,
    about: typeof current.about === "string" ? current.about : null,
    nip05_handle: typeof current.nip05 === "string" ? current.nip05 : null,
    owner_pubkey: null,
    has_profile_event: true,
  };
}

/**
 * `update_persona_and_publish` — save a persona AND report that the community
 * catalog received it.
 *
 * The promise the "Save and publish" affordance makes is about the CATALOG, not
 * about a local file, so the honest answer here is the relay's own agent
 * registry: the write that makes the persona appear for everyone is the same one
 * `update_persona` performs, and it is synchronous, so its acceptance IS the
 * publication. `queued` is therefore never reported — there is no queue behind
 * this path, and claiming one would invent a state a caller might wait on.
 *
 * The write is `saveCloudPersona`, the same one `update_persona` performs, so
 * the per-thread session-policy refusal and the runtime apply here too. The
 * registry stores only the name, instructions, runtime and model; any other
 * field the edit filled is named in `relayMessage` rather than confirmed as
 * published. `create` is injectable so this can be driven in node.
 */
export async function updatePersonaAndPublish(
  args: {
    input?: Record<string, unknown>;
  },
  create?: typeof import("@/shared/browser-backend/cloudAgents").createCloudAgent,
): Promise<{
  persona: unknown;
  publicationStatus: "published" | "queued";
  relayMessage?: string;
}> {
  const payload = args.input ?? {};
  const { personaFieldsNotStored, saveCloudPersona } = await import(
    "@/shared/browser-backend/cloudAgents"
  );
  const persona = await saveCloudPersona(payload, create);
  const unstored = personaFieldsNotStored(payload);
  if (unstored.length === 0) return { persona, publicationStatus: "published" };
  return {
    persona,
    publicationStatus: "published",
    relayMessage:
      `Published the name, instructions, runtime and model. This workspace ` +
      `does not store an agent's ${unstored.join(", ")}, so ` +
      `${unstored.length === 1 ? "that edit was" : "those edits were"} not published.`,
  };
}
