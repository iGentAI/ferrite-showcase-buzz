/**
 * Sending into a thread, browser edition — `send_channel_message` and
 * `get_thread_replies`.
 *
 * The desktop app answers both from Rust: the send command validates each tag
 * argument, resolves the parent's thread root from its local cache, signs a
 * kind-9 (or forum-kind) event and POSTs it to the relay; the thread read walks
 * the relay's `thread_metadata`. A tab has no cache and no metadata route, so
 * both are rebuilt from the two relay faces this build already uses:
 *
 *   * the WRITE is one signed Nostr event on the app's own socket. The relay
 *     validates a submission identically on both faces (`accept_event` is the
 *     single entry point for the HTTP route and the WebSocket `EVENT` verb), so
 *     the socket — the path every other write in this backend takes — loses no
 *     validation. What upstream's comment ("media/emoji sends MUST go through
 *     REST") really guards against is the CLIENT's own WebSocket send helper,
 *     which builds its own tag set and drops extras; here the extras are on the
 *     event we sign, so nothing is lost.
 *
 *   * the READ is a `#e` REQ on the thread root. Every descendant of a root
 *     carries the root in its `e` tags (`buildReplyTags`: a depth-1 reply marks
 *     it `reply`, a deeper one `root`), so one filter returns the whole subtree
 *     and the relay re-authorizes each event on delivery. What `#e` also
 *     matches — a quote or reference that is not a reply — is dropped here by
 *     checking the NIP-10 markers, the same resolution the relay's own
 *     `thread_ancestry` applies.
 *
 * Thread SHAPE (which `e`/`p`/`h` tags a reply carries) is not restated here:
 * `buildReplyTags`, `getThreadReference` and `resolveReplyRootId` from the
 * messages feature are the client's single definition of it, and this module
 * reuses them so the send path, the optimistic cache entry the caller builds
 * from our return value, and the relay's ancestry resolution can never
 * disagree.
 */

import {
  buildReplyTags,
  getThreadReference,
  normalizeMentionPubkeys,
  resolveReplyRootId,
} from "@/features/messages/lib/threading";
import { SENT_FROM_THREAD_TAG } from "@/features/messages/lib/sentFromThread";
import {
  CHANNEL_TIMELINE_CONTENT_KINDS,
  KIND_FORUM_COMMENT,
  KIND_FORUM_POST,
  KIND_STREAM_MESSAGE,
} from "@/shared/constants/kinds";

import type { RelaySubscriptionFilter } from "@/shared/api/relayClientShared";
import type { RelayEvent } from "@/shared/api/types";
import {
  publishSigned,
  relayEvents,
  type RelayIdentity,
} from "@/shared/browser-backend/relayConnection";

/** The relay clamps a REQ `limit` to 500 (`query::MAX_PAGE_EVENTS`). */
const RELAY_PAGE_LIMIT = 500;

/**
 * How many overlapping `until` pages the subtree walk will take before calling
 * the thread unreadable. 40 pages × 500 events is far past any real thread —
 * the cap exists so a pathological loop cannot spin forever, and hitting it
 * throws rather than silently serving a truncated thread as if it were whole.
 */
const MAX_SUBTREE_PAGES = 40;

/**
 * The page size when the caller sends none. `useThreadReplies` always asks for
 * 200; the desktop command's default is the same page.
 */
const DEFAULT_THREAD_PAGE = 200;

/** The relay refuses a reply deeper than this (`effects::MAX_THREAD_DEPTH`). */
const MAX_THREAD_DEPTH = 100;

/**
 * Kinds a reply PARENT can be. The timeline content kinds cover stream
 * threads; forum posts and comments (45001/45003) are their own query path but
 * still parent a `send_channel_message` reply (`useCreateForumReplyMutation`),
 * so the parent lookup must be able to see them or a forum comment's root
 * would silently fall back to "parent is the root" even when it is not.
 */
const PARENT_LOOKUP_KINDS = [
  ...CHANNEL_TIMELINE_CONTENT_KINDS,
  KIND_FORUM_POST,
  KIND_FORUM_COMMENT,
];

/**
 * Whether `id` can exist on the relay at all. A Nostr event id is the sha256
 * of the serialized event — 64 hex characters, always. Anything else (an
 * `optimistic-<uuid>` cache placeholder, a `summary:`/`bounds:` synthetic row)
 * is a client-local id, and a relay read for it is guaranteed empty by
 * construction — the distinction that separates "not delivered yet" from
 * "not on this relay".
 */
export function isRelayEventId(id: string): boolean {
  return /^[0-9a-f]{64}$/i.test(id);
}

/** buzz's Rust `send_channel_message` reply, verbatim (see `tauri.ts`). */
type RawSendChannelMessageResult = {
  event_id: string;
  parent_event_id: string | null;
  root_event_id: string | null;
  depth: number;
  created_at: number;
};

/** Forward keyset cursor over `(created_at, event_id)`, as the wire spells it. */
export type RawThreadCursor = {
  created_at: number;
  event_id: string;
};

/** buzz's Rust `get_thread_replies` reply, verbatim (see `tauri.ts`). */
export type RawThreadRepliesResponse = {
  events: RelayEvent[];
  next_cursor: RawThreadCursor | null;
};

/**
 * The desktop command's per-argument tag gates, reproduced exactly: `media`
 * carries only `imeta` tags, `emoji` only NIP-30 `emoji` tags, `mentions` only
 * reference `mention` tags. The gate is the injection defense the client's own
 * `splitOutgoingTags` documents — any other prefix arriving on one of these
 * channels is content trying to become a tag, and it is refused rather than
 * signed.
 */
export function validatedExtraTags(
  input: {
    mediaTags?: string[][] | null;
    emojiTags?: string[][] | null;
    mentionTags?: string[][] | null;
  },
  command = "send_channel_message",
): string[][] {
  const gate = (tags: string[][] | null | undefined, allowed: string) => {
    for (const tag of tags ?? []) {
      if (!Array.isArray(tag) || tag[0] !== allowed) {
        throw new Error(
          `${command}: only "${allowed}" tags may ride the ${allowed} argument (got "${String(tag?.[0])}").`,
        );
      }
    }
    return tags ?? [];
  };
  return [
    ...gate(input.mediaTags, "imeta"),
    ...gate(input.emojiTags, "emoji"),
    ...gate(input.mentionTags, "mention"),
  ];
}

/**
 * The author's link-preview tags, gated like the other tag arguments: either
 * the single suppression tag `["link-preview","none"]`, or only v1 snapshot
 * tags (`buildLinkPreviewSnapshotTag`, eleven string cells). Anything else
 * on this argument is refused rather than signed. A reader still validates each
 * snapshot against the content and the relay origin when it renders one
 * (`parseLinkPreviewSnapshots`).
 */
export function validatedLinkPreviewTags(tags: unknown): string[][] {
  if (tags === undefined || tags === null) return [];
  if (!Array.isArray(tags)) {
    throw new Error("send_channel_message: linkPreviewTags must be a list of tags.");
  }
  const allStrings = (tag: unknown): tag is string[] =>
    Array.isArray(tag) && tag.every((cell) => typeof cell === "string");
  const isSuppression = (tag: string[]) =>
    tag.length === 2 && tag[0] === "link-preview" && tag[1] === "none";
  const isSnapshot = (tag: string[]) =>
    tag.length === 11 && tag[0] === "link-preview" && tag[1] === "snapshot";
  for (const tag of tags) {
    if (!allStrings(tag) || tag[0] !== "link-preview") {
      throw new Error(
        `send_channel_message: only "link-preview" tags may ride the link-preview argument (got "${String(
          Array.isArray(tag) ? tag[0] : tag,
        )}").`,
      );
    }
  }
  const typed = tags as string[][];
  const suppressed = typed.some(isSuppression);
  if (suppressed ? typed.length !== 1 : !typed.every(isSnapshot)) {
    throw new Error("send_channel_message: invalid link-preview snapshot tag.");
  }
  return typed;
}

/**
 * The "sent from thread" marker (`buildSentFromThreadTag`): one tag named
 * `buzz:sent-from-thread` carrying the root id and optionally an excerpt.
 */
export function validatedSentFromThreadTag(tag: unknown): string[][] {
  if (tag === undefined || tag === null) return [];
  if (
    !Array.isArray(tag) ||
    (tag.length !== 2 && tag.length !== 3) ||
    !tag.every((cell) => typeof cell === "string") ||
    tag[0] !== SENT_FROM_THREAD_TAG ||
    !tag[1].trim()
  ) {
    throw new Error(
      `send_channel_message: sentFromThreadTag must be one "${SENT_FROM_THREAD_TAG}" tag naming the thread root.`,
    );
  }
  return [tag as string[]];
}

/**
 * Every caller-supplied tag of one `send_channel_message`, gated, in the order
 * `useSendMessageMutation` rebuilds its optimistic entry: imeta, emoji,
 * mention, link-preview, then the sent-from-thread marker. Ignoring the
 * link-preview and marker arguments would make the published event lose the
 * author's preview snapshot and the marker the optimistic row shows.
 */
export function sendChannelMessageExtraTags(input: {
  mediaTags?: unknown;
  emojiTags?: unknown;
  mentionTags?: unknown;
  linkPreviewTags?: unknown;
  sentFromThreadTag?: unknown;
}): string[][] {
  return [
    ...validatedExtraTags({
      mediaTags: Array.isArray(input.mediaTags)
        ? (input.mediaTags as string[][])
        : null,
      emojiTags: Array.isArray(input.emojiTags)
        ? (input.emojiTags as string[][])
        : null,
      mentionTags: Array.isArray(input.mentionTags)
        ? (input.mentionTags as string[][])
        : null,
    }),
    ...validatedLinkPreviewTags(input.linkPreviewTags),
    ...validatedSentFromThreadTag(input.sentFromThreadTag),
  ];
}

/**
 * The full tag set of one outgoing message, in the exact order the caller's
 * optimistic cache entry rebuilds it (`useSendMessageMutation`): the reply
 * tags — or `h` + author `p` + mention `p`s for a top-level message — then
 * the gated extras (`sendChannelMessageExtraTags`: imeta, emoji, mention,
 * link-preview, sent-from-thread). A reply's `e`/`p`/`h` shape comes from
 * `buildReplyTags`, the same function the caller uses, so the two cannot
 * drift.
 */
export function outgoingMessageTags(input: {
  channelId: string;
  authorPubkey: string;
  parentEventId: string | null;
  rootEventId: string | null;
  mentionPubkeys: string[];
  extraTags: string[][];
}): string[][] {
  const base = input.parentEventId
    ? buildReplyTags(
        input.channelId,
        input.authorPubkey,
        input.parentEventId,
        input.rootEventId ?? input.parentEventId,
        input.mentionPubkeys,
      )
    : [
        ["h", input.channelId],
        ["p", input.authorPubkey],
        ...normalizeMentionPubkeys(
          input.mentionPubkeys,
          input.authorPubkey,
        ).map((pk) => ["p", pk]),
      ];
  return [...base, ...input.extraTags];
}

/**
 * How deep `eventId` sits in its thread: 0 for the root (or a non-reply), 1
 * for a reply to the root, and so on — the relay's own depth vocabulary
 * (`effects::apply_thread`). Walks the NIP-10 parent chain through
 * `eventsById`. The root id is passed explicitly because the root is NEVER in
 * a `#e` subtree read (it does not e-tag itself), so "the parent is the root"
 * must be recognized by id, not by presence. A link that leaves the map below
 * the root counts as one more hop, mirroring the relay's missing-metadata
 * stub, and a tag cycle stops at the relay's depth cap rather than spinning.
 */
export function threadDepthOf(
  eventId: string,
  eventsById: Map<string, RelayEvent>,
  rootEventId: string,
): number {
  if (eventId === rootEventId) return 0;
  let depth = 0;
  const seen = new Set<string>([eventId]);
  let current = eventsById.get(eventId);
  if (!current) return 1;
  while (current) {
    const ref = getThreadReference(current.tags);
    if (!ref.parentId) return depth;
    depth += 1;
    if (ref.parentId === rootEventId) return depth;
    if (depth >= MAX_THREAD_DEPTH || seen.has(ref.parentId)) return depth;
    seen.add(ref.parentId);
    current = eventsById.get(ref.parentId);
  }
  // The chain continues above what we could read; count the unseen ancestor.
  return depth + 1;
}

/**
 * One page of a thread's replies from its raw `#e` matches: descendants only
 * (the NIP-10 markers must resolve into THIS root — a quote that merely e-tags
 * the root is not a reply), the root itself excluded, chronological with the
 * event-id tiebreak, sliced strictly after the cursor so same-second replies
 * can never be skipped across a page boundary.
 *
 * `next_cursor` is non-null only when more replies remain past this page,
 * which also satisfies the caller's weaker documented contract ("non-null only
 * when a full page was returned") without costing it one guaranteed-empty
 * round trip after every thread whose size is an exact page multiple.
 */
export function threadRepliesPage(
  subtree: RelayEvent[],
  rootEventId: string,
  options: {
    limit: number;
    cursor: RawThreadCursor | null;
    depthLimit: number | null;
  },
): RawThreadRepliesResponse {
  const byId = new Map(subtree.map((event) => [event.id, event]));
  const descendants = [...byId.values()].filter((event) => {
    if (event.id === rootEventId) return false;
    const ref = getThreadReference(event.tags);
    if (!ref.parentId) return false;
    if (ref.rootId !== rootEventId && ref.parentId !== rootEventId) {
      return false;
    }
    return (
      options.depthLimit == null ||
      threadDepthOf(event.id, byId, rootEventId) <= options.depthLimit
    );
  });

  descendants.sort((left, right) => {
    if (left.created_at !== right.created_at) {
      return left.created_at - right.created_at;
    }
    return left.id < right.id ? -1 : left.id > right.id ? 1 : 0;
  });

  const cursor = options.cursor;
  const after = cursor
    ? descendants.filter(
        (event) =>
          event.created_at > cursor.created_at ||
          (event.created_at === cursor.created_at &&
            event.id > cursor.event_id),
      )
    : descendants;

  const events = after.slice(0, options.limit);
  const last = events[events.length - 1];
  return {
    events,
    next_cursor:
      after.length > events.length && last
        ? { created_at: last.created_at, event_id: last.id }
        : null,
  };
}

function subtreeFilter(
  rootEventId: string,
  channelId: string | null,
  until?: number,
): RelaySubscriptionFilter {
  return {
    kinds: [...CHANNEL_TIMELINE_CONTENT_KINDS],
    "#e": [rootEventId],
    ...(channelId ? { "#h": [channelId] } : {}),
    ...(until === undefined ? {} : { until }),
    limit: RELAY_PAGE_LIMIT,
  };
}

/**
 * Every stored event referencing `rootEventId`, however many pages that takes.
 *
 * A REQ answers with at most `limit` rows and MAY answer with fewer (the
 * access gate and the frame budget both shave pages), so a short page is not
 * exhaustion, and neither is a page that adds nothing new: `until` is
 * inclusive, so the boundary second is re-delivered first, and a shaved page
 * can consist of nothing else while older replies remain. Only an EMPTY page
 * ends the walk. A page that adds nothing moves the cursor one second earlier;
 * a FULL page that adds nothing and never leaves the boundary second means
 * that second holds more events than a page, which this cursor cannot step
 * through, so the walk refuses rather than serve the thread truncated.
 * `fetchPage` is injectable so the walk can be driven in node.
 */
export async function fetchThreadSubtree(
  rootEventId: string,
  channelId: string | null,
  fetchPage: (filter: RelaySubscriptionFilter) => Promise<RelayEvent[]> = relayEvents,
): Promise<RelayEvent[]> {
  const collected = new Map<string, RelayEvent>();
  let until: number | undefined;
  for (let page = 0; page < MAX_SUBTREE_PAGES; page += 1) {
    const batch = await fetchPage(subtreeFilter(rootEventId, channelId, until));
    if (batch.length === 0) return [...collected.values()];
    let added = 0;
    for (const event of batch) {
      if (!collected.has(event.id)) {
        collected.set(event.id, event);
        added += 1;
      }
    }
    const oldest = Math.min(...batch.map((event) => event.created_at));
    if (added === 0) {
      if (batch.length >= RELAY_PAGE_LIMIT && oldest === until) {
        throw new Error(
          `Thread ${rootEventId} has more events in one second than a relay page holds; refusing to serve it truncated.`,
        );
      }
      until = oldest - 1;
      continue;
    }
    until = oldest;
  }
  throw new Error(
    `Thread ${rootEventId} has more events than this build can page (${
      MAX_SUBTREE_PAGES * RELAY_PAGE_LIMIT
    }); refusing to serve it truncated.`,
  );
}

/**
 * `send_channel_message` — sign one message event and publish it on the relay
 * socket. This is the path every reply, media send, custom-emoji send and
 * forum post takes (`useSendMessageMutation`, `useCreateForumPostMutation`,
 * the home-feed reply box, the profile-popover DM, the agent `!shutdown`
 * button); plain top-level stream messages use the client's own WebSocket
 * helper instead and never arrive here.
 *
 * The parent's thread ROOT is resolved the way the client itself resolves it
 * (`resolveReplyRootId` over a relay read instead of the cache), because the
 * `e` tags we sign are the relay's only ancestry input (`thread_ancestry`) —
 * a wrong root here would fork the thread for every other reader. `depth` is
 * the relay's own vocabulary (0 top-level, 1 reply-to-root, …), walked over
 * one page of the root's subtree; past the newest 500 thread events the walk
 * degrades to the relay's own missing-metadata stub rather than guessing.
 */
export async function sendChannelMessageViaRelay(
  identity: RelayIdentity,
  raw: unknown,
): Promise<RawSendChannelMessageResult> {
  const input = (raw ?? {}) as {
    channelId?: unknown;
    content?: unknown;
    parentEventId?: unknown;
    mediaTags?: unknown;
    emojiTags?: unknown;
    mentionTags?: unknown;
    linkPreviewTags?: unknown;
    sentFromThreadTag?: unknown;
    mentionPubkeys?: unknown;
    kind?: unknown;
    expectedSignerPubkey?: unknown;
    rootEventId?: unknown;
  };
  const channelId =
    typeof input.channelId === "string" ? input.channelId : "";
  if (!channelId) throw new Error("send_channel_message needs a channelId.");
  const expectedSigner =
    typeof input.expectedSignerPubkey === "string" && input.expectedSignerPubkey
      ? input.expectedSignerPubkey.toLowerCase()
      : null;
  const content =
    typeof input.content === "string" ? input.content.trim() : "";
  const parentEventId =
    typeof input.parentEventId === "string" && input.parentEventId
      ? input.parentEventId
      : null;
  if (parentEventId && !isRelayEventId(parentEventId)) {
    // A reply target that is structurally not a Nostr event id is a
    // client-local placeholder — an `optimistic-<uuid>` cache row for a send
    // the relay has not confirmed yet. Signing it anyway would PUBLISH an
    // event whose reply `e` tag points at an id that can never exist: the
    // relay's `thread_ancestry` hex-gates it (no thread metadata forms), but
    // every client's `getThreadReference` does not, so the reply renders
    // forever as a reply to nothing. Refusing is the only honest answer, and
    // the sentence is user-facing (the composer shows a failed send's
    // message and keeps the draft).
    throw new Error(
      "The message you are replying to is still sending — wait for it to be delivered, then reply.",
    );
  }
  const kind =
    typeof input.kind === "number" ? input.kind : KIND_STREAM_MESSAGE;
  const mentionPubkeys = Array.isArray(input.mentionPubkeys)
    ? input.mentionPubkeys.filter(
        (pk): pk is string => typeof pk === "string" && pk.length > 0,
      )
    : [];
  const extraTags = sendChannelMessageExtraTags(input);

  const authorPubkey = await identity.pubkey();
  // The caller names the identity it means to speak as; a switch before
  // signing must fail closed, not post as someone else (finding_cb19d19e5fe1a1b4).
  if (expectedSigner && authorPubkey.toLowerCase() !== expectedSigner) {
    throw new Error("The signing identity changed before this message was sent; it was not sent.");
  }

  let rootEventId: string | null = null;
  let depth = 0;
  if (parentEventId) {
    // The parent, by id — the relay is the only store a tab has. A parent the
    // relay does not return (deleted, or out of reach) degrades to "parent is
    // the root", exactly what `resolveReplyRootId` answers on a cache miss.
    const parentPage = await relayEvents({
      ids: [parentEventId],
      kinds: PARENT_LOOKUP_KINDS,
      limit: 1,
    });
    const parent = parentPage.find((event) => event.id === parentEventId);
    // When the relay no longer returns the parent (deleted), the caller's own
    // root id keeps the reply in its thread instead of forking a new one.
    const callerRootId =
      typeof input.rootEventId === "string" && isRelayEventId(input.rootEventId)
        ? input.rootEventId
        : null;
    const resolvedRootId: string =
      !parent && callerRootId
        ? callerRootId
        : resolveReplyRootId(parentEventId, parentPage);
    rootEventId = resolvedRootId;
    if (!parent || !getThreadReference(parent.tags).parentId) {
      depth = 1;
    } else {
      const subtree = await relayEvents(subtreeFilter(rootEventId, channelId));
      const byId = new Map(subtree.map((event) => [event.id, event]));
      byId.set(parent.id, parent);
      depth = 1 + threadDepthOf(parentEventId, byId, rootEventId);
    }
  }

  const signed = await identity.sign({
    kind,
    content,
    tags: outgoingMessageTags({
      channelId,
      authorPubkey,
      parentEventId,
      rootEventId,
      mentionPubkeys,
      extraTags,
    }),
  });

  // The identity can change across the relay round trips above (a NIP-07
  // account switch): what was signed must still be the author the tags name.
  const signedPubkey = String((signed as { pubkey?: unknown }).pubkey ?? "").toLowerCase();
  if (signedPubkey !== authorPubkey.toLowerCase()) {
    throw new Error("The signing identity changed while this message was prepared; it was not sent.");
  }

  await publishSigned(signed, "Could not send that message.");

  // The SIGNED event is the truth for both fields: a NIP-07 extension stamps
  // its own `created_at`, and the caller keys its cache entry on this id.
  return {
    event_id: String((signed as { id?: unknown }).id ?? ""),
    parent_event_id: parentEventId,
    root_event_id: rootEventId,
    depth,
    created_at: Number(
      (signed as { created_at?: unknown }).created_at ??
        Math.floor(Date.now() / 1000),
    ),
  };
}

/**
 * `get_thread_replies` — one keyset page of the reply subtree under a root,
 * oldest first, root excluded. The subtree is read whole (see
 * {@link fetchThreadSubtree}) and paged here, so the cursor contract holds
 * even though the relay's own `#e` index serves newest-first: the caller
 * (`useThreadReplies`) hands `next_cursor` straight back and expects the pages
 * to union into the complete thread with no same-second gaps.
 *
 * Edits, deletions and reactions for these replies are NOT resolved here —
 * the caller backfills them by `#e` reference itself (`withThreadAux`), the
 * same overlay path the channel timeline uses.
 */
export async function threadRepliesFromRelay(
  raw: unknown,
): Promise<RawThreadRepliesResponse> {
  const input = (raw ?? {}) as {
    rootEventId?: unknown;
    channelId?: unknown;
    limit?: unknown;
    depthLimit?: unknown;
    cursor?: unknown;
  };
  const rootEventId =
    typeof input.rootEventId === "string" ? input.rootEventId : "";
  if (!rootEventId) throw new Error("get_thread_replies needs a rootEventId.");
  const channelId =
    typeof input.channelId === "string" && input.channelId
      ? input.channelId
      : null;
  const limit =
    typeof input.limit === "number" && input.limit > 0
      ? Math.min(Math.floor(input.limit), RELAY_PAGE_LIMIT)
      : DEFAULT_THREAD_PAGE;
  const depthLimit =
    typeof input.depthLimit === "number" && input.depthLimit > 0
      ? Math.floor(input.depthLimit)
      : null;
  const rawCursor = (input.cursor ?? null) as {
    created_at?: unknown;
    event_id?: unknown;
  } | null;
  const cursor: RawThreadCursor | null =
    rawCursor &&
    typeof rawCursor.created_at === "number" &&
    typeof rawCursor.event_id === "string"
      ? { created_at: rawCursor.created_at, event_id: rawCursor.event_id }
      : null;

  const subtree = await fetchThreadSubtree(rootEventId, channelId);
  return threadRepliesPage(subtree, rootEventId, {
    limit,
    cursor,
    depthLimit,
  });
}
