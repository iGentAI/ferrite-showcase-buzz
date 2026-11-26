/**
 * NOTES (buzz's "pulse") from a browser tab.
 *
 * A note is a NIP-01 **kind 1**, and this relay stores, indexes and serves
 * them like any other event.
 *
 * | command | how |
 * | --- | --- |
 * | `publish_note` | sign and publish a kind 1 |
 * | `get_note` | query by id |
 * | `get_user_notes` | query by author, paged with `until` |
 * | `get_global_notes` | query the kind, paged |
 * | `get_liked_notes` | this author's kind-7 reactions, then their targets |
 *
 * `get_liked_notes` is the only one that assembles rather than queries, and the
 * reason is worth stating: a "like" is not a property of a note, it is a
 * reaction event pointing AT one. So the honest read is two hops — the reactions
 * this person authored, then the notes those name — where the desktop does it in
 * one because it keeps a local join table. Two hops is the cost of not having
 * one, not a shortcut.
 */
import type { RelayEvent } from "@/shared/api/types";
import {
  publishSigned,
  relayEvents,
  type RelayIdentity,
} from "@/shared/browser-backend/relayConnection";
import {
  cursorFrom,
  pageOf,
  untilFrom,
  type Cursor,
} from "@/shared/browser-backend/reads";
// The mention normalisation and the `imeta`-only media gate are the channel
// send path's, so a note and a message cannot disagree about either.
import { normalizeMentionPubkeys } from "@/features/messages/lib/threading";
import { validatedExtraTags } from "@/shared/browser-backend/messageSend";

/** NIP-01 text note — what buzz calls a pulse note. */
const KIND_NOTE = 1;
/** NIP-25 reaction, which is what a "like" is. */
const KIND_REACTION = 7;

/** The relay clamps a REQ `limit` to 500. */
const PAGE = 500;

/** One note in the shape `social.ts` maps into the UI. */
type RawUserNote = {
  id: string;
  pubkey: string;
  created_at: number;
  content: string;
  tags: string[][];
};

type RawUserNotesResponse = {
  notes: RawUserNote[];
  next_cursor: { before: number; before_id: string } | null;
};

function toRawNote(event: RelayEvent): RawUserNote {
  return {
    id: event.id,
    pubkey: event.pubkey,
    created_at: event.created_at,
    content: event.content ?? "",
    tags: event.tags ?? [],
  };
}

/**
 * One page of notes plus the cursor that resumes after it — the shared
 * `pageOf`, mapped into this API's own wire names. The cursor discipline
 * (order by `(created_at DESC, id ASC)`, inclusive `until`, resume exactly
 * past the boundary, full upstream page means "probably more") lives in
 * `reads.ts`, once.
 */
function page(
  events: RelayEvent[],
  limit: number,
  cursor: Cursor,
  capped: boolean,
): RawUserNotesResponse {
  const { rows, next } = pageOf(events, limit, cursor, capped);
  return {
    notes: rows.map(toRawNote),
    next_cursor: next ? { before: next.created_at, before_id: next.id } : null,
  };
}

const HEX_PUBKEY = /^[0-9a-f]{64}$/i;

/**
 * `publish_note`.
 *
 * The command contract (`shared/api/social.ts`) carries `mentionPubkeys` and
 * `mediaTags` as well as the content: each mention becomes a `p` tag, which is
 * what notifies a person and wakes a mentioned relay agent, and the composer's
 * attachment metadata rides as `imeta` tags.
 */
export async function publishNote(
  identity: RelayIdentity,
  args: {
    content?: string;
    replyTo?: string | null;
    mentionPubkeys?: string[] | null;
    mediaTags?: string[][] | null;
  },
): Promise<{ event_id: string; accepted: boolean; message: string }> {
  const content = args.content ?? "";
  if (!content.trim()) throw new Error("A note needs something to say.");
  const tags: string[][] = [];
  // NIP-10: a reply names its target with a marked `e` tag, so a thread can be
  // reconstructed by anybody rather than only by whoever holds the reply.
  if (args.replyTo) tags.push(["e", args.replyTo, "", "reply"]);
  const mentions = args.mentionPubkeys ?? [];
  for (const pubkey of mentions) {
    if (typeof pubkey !== "string" || !HEX_PUBKEY.test(pubkey)) {
      throw new Error(`publish_note: "${String(pubkey)}" is not a hex pubkey.`);
    }
  }
  if (mentions.length > 0) {
    const self = await identity.pubkey();
    for (const pubkey of normalizeMentionPubkeys(mentions, self)) {
      tags.push(["p", pubkey]);
    }
  }
  tags.push(...validatedExtraTags({ mediaTags: args.mediaTags }, "publish_note"));
  const signed = (await identity.sign({ kind: KIND_NOTE, content, tags })) as unknown as {
    id: string;
  };
  await publishSigned(signed, "Could not publish that note.");
  return { event_id: signed.id, accepted: true, message: "" };
}

/** `get_note`. */
export async function getNote(args: { noteId?: string }): Promise<RawUserNote | null> {
  const noteId = args.noteId ?? "";
  if (!noteId) return null;
  const events = await relayEvents({ ids: [noteId], kinds: [KIND_NOTE], limit: 1 });
  const found = events.find((event) => event.id === noteId);
  // `null` rather than a throw: "this note is not on the relay" is a state the
  // caller renders, not an error — its own signature says so.
  return found ? toRawNote(found) : null;
}

/** `get_user_notes`. */
export async function getUserNotes(args: {
  pubkey?: string;
  limit?: number | null;
  before?: number | null;
  beforeId?: string | null;
}): Promise<RawUserNotesResponse> {
  const author = (args.pubkey ?? "").toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(author)) return { notes: [], next_cursor: null };
  const limit = Math.min(Math.max(args.limit ?? 30, 1), PAGE);
  const cursor = cursorFrom(args);
  const events = await relayEvents({
    kinds: [KIND_NOTE],
    authors: [author],
    limit: PAGE,
    ...untilFrom(cursor),
  });
  return page(events, limit, cursor, events.length >= PAGE);
}

/** `get_global_notes`. */
export async function getGlobalNotes(args: {
  limit?: number | null;
  before?: number | null;
  beforeId?: string | null;
}): Promise<RawUserNotesResponse> {
  const limit = Math.min(Math.max(args.limit ?? 30, 1), PAGE);
  const cursor = cursorFrom(args);
  const events = await relayEvents({
    kinds: [KIND_NOTE],
    limit: PAGE,
    ...untilFrom(cursor),
  });
  return page(events, limit, cursor, events.length >= PAGE);
}

/**
 * The safeguard on a liked-notes reaction walk: 200 pages, i.e. 100,000
 * reactions.
 *
 * NOT a paging budget — a paging budget cannot work here, and the reason is the
 * shape of the problem rather than the size of the number. The caller's cursor
 * is a position in NOTE time; the walk is over REACTION time. So a later page
 * restarts the walk from the newest reactions and rebuilds the same set: a note
 * liked before the window is unreachable from EVERY page, not merely the first,
 * and raising the ceiling only moves the wall.
 *
 * The set must be complete for a second reason too. This page is ordered by note
 * time, and a note written long ago can be liked today, so a partial target set
 * yields an order the complete set would not have had — wrong, not just short.
 *
 * Hence: walk to exhaustion, and treat this as the point where the operation is
 * REFUSED rather than silently truncated.
 */
const LIKED_WALK_SAFEGUARD = 200;

/**
 * `get_liked_notes` — the notes this person reacted to.
 *
 * Two hops, because a like is a reaction event pointing at a note rather than a
 * property of the note. Every reaction this author published is read, their `e`
 * targets collected, and those notes fetched by id.
 *
 * EVERY reaction, not a window — see [`LIKED_WALK_SAFEGUARD`] for why a window
 * cannot be made to work with a note-time cursor, and why a partial set would be
 * mis-ordered rather than merely short.
 *
 * Ordered by the NOTE's own time, which is the order the caller renders and the
 * one its cursor pages on. Ordering by the reaction instead would make the
 * cursor and the list disagree about what "next" means.
 */
export async function getLikedNotes(args: {
  pubkey?: string;
  authorPubkey?: string;
  limit?: number | null;
  before?: number | null;
  beforeId?: string | null;
}): Promise<RawUserNotesResponse> {
  const author = (args.authorPubkey ?? args.pubkey ?? "").toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(author)) return { notes: [], next_cursor: null };
  const limit = Math.min(Math.max(args.limit ?? 30, 1), PAGE);
  const cursor = cursorFrom(args);

  const targets = new Set<string>();
  let until: number | undefined;
  let exhausted = false;
  for (let read = 0; read < LIKED_WALK_SAFEGUARD; read += 1) {
    const reactions: RelayEvent[] = await relayEvents({
      kinds: [KIND_REACTION],
      authors: [author],
      limit: PAGE,
      ...(until === undefined ? {} : { until }),
    });
    for (const event of reactions) {
      // NIP-25: the LAST e tag names the reacted event; earlier ones are
      // thread ancestors. Only a 64-hex id is a target.
      const eTags = (event.tags ?? []).filter(
        (tag) => tag[0] === "e" && typeof tag[1] === "string",
      );
      const target = eTags.at(-1)?.[1]?.toLowerCase();
      if (target && /^[0-9a-f]{64}$/.test(target)) targets.add(target);
    }
    // A page short of the relay's cap is the end of this author's reactions.
    if (reactions.length < PAGE) {
      exhausted = true;
      break;
    }
    const oldest = reactions.reduce(
      (low, event) => Math.min(low, event.created_at),
      Number.POSITIVE_INFINITY,
    );
    if (!Number.isFinite(oldest) || oldest === until) {
      // A full page lying entirely inside ONE second. `until` cannot step past
      // it, so there is no way to continue — and continuing to pretend would
      // silently drop everything older. Said out loud instead.
      throw new Error(
        "This identity has too many reactions in a single second for the relay's " +
          "timestamp cursor to page past. Liked notes cannot be listed completely.",
      );
    }
    until = oldest;
  }
  if (!exhausted) {
    // The safeguard, not a page budget. Returning what was collected would be a
    // liked timeline that is MIS-ORDERED rather than short, because the ordering
    // is by note time and the missing likes are not the oldest notes.
    throw new Error(
      "This identity has more reactions than the liked-notes reader will walk. " +
        "Listing them completely is not possible from here.",
    );
  }
  if (targets.size === 0) return { notes: [], next_cursor: null };

  // Only kind 1: a reaction can target anything, and a liked CHAT message is
  // not a note — filtering by kind here is what keeps the pulse timeline from
  // rendering a channel message as one.
  const ids = [...targets];
  const notes: RelayEvent[] = [];
  for (let at = 0; at < ids.length; at += PAGE) {
    notes.push(
      ...(await relayEvents({
        ids: ids.slice(at, at + PAGE),
        kinds: [KIND_NOTE],
        limit: PAGE,
      })),
    );
  }
  // The set is complete, so paging within it is exact and needs no cap signal.
  return page(notes, limit, cursor, false);
}
