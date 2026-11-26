/**
 * SEARCH from a browser tab.
 *
 * The relay carries a real inverted index on its own Store keys, so this is
 * a thin client of `POST /buzz/api/search`. Everything that matters happens
 * there: which events match, and — more importantly — which of them this caller
 * may see. A term said in a private room has an index row exactly like one said
 * in the open, and only the relay's access gate keeps the first out of a
 * non-member's results.
 */
import { apiPost } from "@/shared/lib/api";
import {
  KIND_STREAM_MESSAGE,
  KIND_STREAM_MESSAGE_EDIT,
} from "@/shared/constants/kinds";

/** One event as the relay serialises it. */
type RelayEvent = {
  id: string;
  pubkey: string;
  created_at: number;
  kind: number;
  tags: string[][];
  content: string;
};

/** The hit shape `tauri.ts` maps into the UI's `SearchHit`. */
type RawSearchHit = {
  event_id: string;
  content: string;
  kind: number;
  pubkey: string;
  channel_id: string | null;
  channel_name: string | null;
  created_at: number;
  score: number;
};

/** The channel an event names, which is where the UI links the hit to. */
function channelOf(event: RelayEvent): string | null {
  for (const tag of event.tags ?? []) {
    if (tag[0] === "h" && typeof tag[1] === "string" && tag[1]) {
      return tag[1];
    }
  }
  return null;
}

/**
 * How many `/search` requests one command may make. The relay bounds each walk
 * and over-fetches `limit * 2` before its access and channel filters discard
 * hits, so one request can legitimately return few or none while matches lie
 * further back; a handful of follow-ups on its cursor fills most answers
 * without letting one keystroke turn into an unbounded scan.
 */
const MAX_SEARCH_REQUESTS = 8;

/**
 * `search_messages` — conjunctive full-text search over stored messages.
 *
 * `score` is a constant. The index is ordered by time, not by relevance, so
 * there is no ranking to report; inventing one here would be a number the UI
 * would then sort by, and it would mean nothing. `channel_name` is likewise left
 * to the client, which already holds the channel list the id resolves against.
 *
 * The relay's `complete` is the exhaustion authority (`search_route`: "a client
 * pages until `complete` is true and never infers it from a short page"), so
 * this follows `next_cursor` until `limit` hits are collected or the relay says
 * the walk is complete, and hands both back: `complete: false` with a cursor
 * means "more to load", which an empty `hits` alone cannot say. `found` stays
 * the number of hits returned. `post` is injectable for node tests.
 */
export async function searchMessages(
  args: {
    q?: string;
    limit?: number;
    channelId?: string | null;
    /** `from:` resolved to pubkeys by the caller (upstream's search operators). */
    authors?: string[];
    /** `after:YYYY-MM-DD` as unix seconds, inclusive. */
    since?: number;
    /** `before:YYYY-MM-DD` as unix seconds, inclusive upper bound. */
    until?: number;
    /** A `next_cursor` from an earlier answer, to load the next page. */
    cursor?: string | null;
  },
  post: typeof apiPost = apiPost,
): Promise<{
  hits: RawSearchHit[];
  found: number;
  complete: boolean;
  next_cursor: string | null;
}> {
  const query = (args.q ?? "").trim();
  if (!query) {
    return { hits: [], found: 0, complete: true, next_cursor: null };
  }
  const limit = args.limit ?? 50;
  const events: RelayEvent[] = [];
  const seen = new Set<string>();
  let cursor: string | null =
    typeof args.cursor === "string" && args.cursor ? args.cursor : null;
  let complete = false;
  for (let request = 0; request < MAX_SEARCH_REQUESTS; request += 1) {
    const reply = await post<{
      events?: RelayEvent[];
      complete?: boolean;
      next_cursor?: string | null;
    }>("/search", {
      q: query,
      limit: limit - events.length,
      channel: args.channelId ?? undefined,
      // The operator fields ride through to the relay, which honours them in
      // the index walk — dropping one here would return messages the caller
      // explicitly excluded, and the result would LOOK right.
      authors:
        Array.isArray(args.authors) && args.authors.length > 0
          ? args.authors
          : undefined,
      since: typeof args.since === "number" ? args.since : undefined,
      until: typeof args.until === "number" ? args.until : undefined,
      cursor: cursor ?? undefined,
    });
    for (const event of reply.events ?? []) {
      if (seen.has(event.id)) continue;
      seen.add(event.id);
      events.push(event);
    }
    // Anything but an explicit `true` is "not known to be exhausted".
    complete = reply.complete === true;
    cursor = complete ? null : (reply.next_cursor ?? null);
    if (complete || events.length >= limit || cursor === null) break;
  }
  return {
    hits: events.map((event) => ({
      event_id:
        event.kind === KIND_STREAM_MESSAGE_EDIT
          ? (event.tags.find((tag) => tag[0] === "e")?.[1] ?? event.id)
          : event.id,
      content: event.content ?? "",
      // A search hit is a projection of the message, not a signed edit event.
      // Navigation caches it under the original id; retaining the edit kind
      // there replaces the renderable message with an auxiliary event and
      // blanks the target until a reload clears that transient cache.
      kind:
        event.kind === KIND_STREAM_MESSAGE_EDIT
          ? KIND_STREAM_MESSAGE
          : event.kind,
      pubkey: event.pubkey,
      channel_id: channelOf(event),
      channel_name: null,
      created_at: event.created_at,
      score: 1,
    })),
    found: events.length,
    complete,
    next_cursor: complete ? null : cursor,
  };
}
