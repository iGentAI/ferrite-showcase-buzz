/**
 * Shared cache and scheduler behind message-link (`buzz://message?…`) pill
 * tooltips.
 *
 * Message content is authored by any relay peer, so one message packed with
 * links could otherwise spend the viewer's own relay query budget. A pill
 * resolving its linked message the moment it mounts would have no bound on how
 * many lookups one message could start, no concurrency limit, a cache that
 * grows for the whole session, and `ready` previews that outlive the linked
 * message's deletion or edit. This store bounds each of those:
 *
 * - lookups run through one shared scheduler (two at a time, like the link
 *   preview loader);
 * - lookups a pill starts on its own when it mounts ("eager") are admitted
 *   only while fewer than `maxEager` of them are queued or running; a pill
 *   that is refused resolves when the reader hovers or focuses it;
 * - the cache is an LRU of at most `maxEntries` entries;
 * - `ready` and `deleted` results are fresh for `settledTtlMs`,
 *   `unavailable` results for `unavailableTtlMs` (so a remounting row cannot
 *   re-fire a failed lookup immediately), and a stale entry is re-fetched
 *   the next time it is asked for;
 * - a deletion, NIP-29 delete, edit or moderation tombstone that references a
 *   cached message evicts it (`invalidateForEvent`), and subscribers for that
 *   message are told to drop what they show.
 */

export type MessageLinkMetadata = {
  author: string;
  createdAt: number;
  snippet: string;
};

export type CachedMessageLinkMetadata =
  | ({ kind: "ready" } & MessageLinkMetadata)
  | { kind: "deleted" }
  | { kind: "unavailable" };

export type MessageLinkRef = { channelId: string; messageId: string };

type LinkedEvent = {
  content: string;
  created_at: number;
  kind: number;
  pubkey: string;
  tags: string[][];
};

type LinkedProfile = {
  displayName?: string | null;
  nip05Handle?: string | null;
} | null;

export type MessageLinkMetadataDeps = {
  getEvent: (messageId: string) => Promise<LinkedEvent>;
  getProfile: (pubkey: string) => Promise<LinkedProfile>;
  isDefinitiveNotFound: (error: unknown) => boolean;
  now?: () => number;
  shortPubkey: (pubkey: string) => string;
  summarize: (content: string) => string;
  wait?: (ms: number) => Promise<void>;
};

export type MessageLinkMetadataOptions = {
  concurrency?: number;
  maxEager?: number;
  maxEntries?: number;
  retryDelayMs?: number;
  settledTtlMs?: number;
  unavailableTtlMs?: number;
};

export const MESSAGE_LINK_METADATA_DEFAULTS = {
  concurrency: 2,
  maxEager: 8,
  maxEntries: 500,
  retryDelayMs: 750,
  settledTtlMs: 2 * 60_000,
  unavailableTtlMs: 30_000,
} as const;

const PREVIEWABLE_MESSAGE_KINDS = new Set([9, 40002, 45001, 45003]);

/** Kinds whose `e` tags name a message whose preview may now be wrong. */
const INVALIDATING_KINDS = new Set([
  5, // NIP-09 deletion
  9005, // NIP-29 delete-event
  40003, // message edit
  40099, // system message (moderation tombstones)
]);

type CacheEntry = {
  expiresAt: number;
  messageId: string;
  pending: Promise<CachedMessageLinkMetadata> | null;
  value: CachedMessageLinkMetadata | null;
};

export type MessageLinkMetadataPeek = {
  fresh: boolean;
  value: CachedMessageLinkMetadata;
};

export function messageLinkMetadataKey(link: MessageLinkRef): string {
  return `${link.channelId}:${link.messageId}`;
}

export function createMessageLinkMetadataStore(
  deps: MessageLinkMetadataDeps,
  options: MessageLinkMetadataOptions = {},
) {
  const {
    concurrency,
    maxEager,
    maxEntries,
    retryDelayMs,
    settledTtlMs,
    unavailableTtlMs,
  } = { ...MESSAGE_LINK_METADATA_DEFAULTS, ...options };
  const now = deps.now ?? Date.now;
  const wait =
    deps.wait ??
    ((ms: number) =>
      new Promise<void>((resolve) => {
        setTimeout(resolve, ms);
      }));

  const cache = new Map<string, CacheEntry>();
  const listeners = new Map<string, Set<() => void>>();
  // Each queued lookup can also be dropped: a reset settles it as
  // unavailable instead of running it against the next community's relay.
  const queue: Array<{ run: () => void; drop: () => void }> = [];
  let active = 0;
  let eagerOutstanding = 0;
  let generation = 0;

  const drain = () => {
    while (active < Math.max(1, concurrency)) {
      const next = queue.shift();
      if (!next) return;
      active += 1;
      next.run();
    }
  };

  const schedule = (
    task: () => Promise<CachedMessageLinkMetadata>,
  ): Promise<CachedMessageLinkMetadata> =>
    new Promise<CachedMessageLinkMetadata>((resolve, reject) => {
      queue.push({
        run: () => {
          void task()
            .then(resolve, reject)
            .finally(() => {
              active -= 1;
              drain();
            });
        },
        drop: () => resolve({ kind: "unavailable" }),
      });
      drain();
    });

  /** Re-insert so Map order tracks recency; evict the least recent. */
  const touch = (key: string, entry: CacheEntry) => {
    cache.delete(key);
    cache.set(key, entry);
    while (cache.size > Math.max(1, maxEntries)) {
      const oldest = cache.keys().next().value;
      if (oldest === undefined) break;
      cache.delete(oldest);
    }
  };

  const notify = (key: string) => {
    for (const listener of listeners.get(key) ?? []) listener();
  };

  const getEventWithRetry = async (messageId: string, startedIn: number) => {
    try {
      return await deps.getEvent(messageId);
    } catch (error) {
      if (deps.isDefinitiveNotFound(error)) throw error;
      await wait(retryDelayMs);
      // The disconnect of a community switch looks transient; never retry a
      // previous community's message id on the next one's relay
      // (finding_92a9be9600f6d34d).
      if (generation !== startedIn) throw error;
      return deps.getEvent(messageId);
    }
  };

  const resolve = async (
    link: MessageLinkRef,
    startedIn: number,
  ): Promise<CachedMessageLinkMetadata> => {
    try {
      if (generation !== startedIn) return { kind: "unavailable" };
      const event = await getEventWithRetry(link.messageId, startedIn);
      if (generation !== startedIn) return { kind: "unavailable" };
      const eventChannelId = event.tags.find((tag) => tag[0] === "h")?.[1];
      if (
        eventChannelId !== link.channelId ||
        !PREVIEWABLE_MESSAGE_KINDS.has(event.kind)
      ) {
        return { kind: "unavailable" };
      }
      const profile = await deps.getProfile(event.pubkey).catch(() => null);
      return {
        kind: "ready",
        author:
          profile?.displayName?.trim() ||
          profile?.nip05Handle?.trim() ||
          deps.shortPubkey(event.pubkey),
        createdAt: event.created_at,
        snippet: deps.summarize(event.content),
      };
    } catch (error) {
      return deps.isDefinitiveNotFound(error)
        ? { kind: "deleted" }
        : { kind: "unavailable" };
    }
  };

  const peek = (link: MessageLinkRef): MessageLinkMetadataPeek | undefined => {
    const entry = cache.get(messageLinkMetadataKey(link));
    if (!entry?.value) return undefined;
    return { fresh: entry.expiresAt > now(), value: entry.value };
  };

  /**
   * Resolve a link's metadata. Returns the cached value when it is fresh, the
   * in-flight request when one exists, or starts a lookup. An `eager` lookup
   * (a pill resolving itself on mount) is refused with `null` while
   * `maxEager` eager lookups are already outstanding.
   */
  const load = (
    link: MessageLinkRef,
    { eager }: { eager: boolean },
  ): Promise<CachedMessageLinkMetadata> | null => {
    const key = messageLinkMetadataKey(link);
    const existing = cache.get(key);
    if (existing) {
      touch(key, existing);
      if (existing.pending) return existing.pending;
      if (existing.value && existing.expiresAt > now()) {
        return Promise.resolve(existing.value);
      }
    }
    if (eager && eagerOutstanding >= Math.max(0, maxEager)) return null;

    const entry: CacheEntry = {
      expiresAt: existing?.expiresAt ?? 0,
      messageId: link.messageId,
      pending: null,
      value: existing?.value ?? null,
    };
    const startedIn = generation;
    if (eager) eagerOutstanding += 1;
    const request = schedule(() => resolve(link, startedIn)).finally(() => {
      // A reset zeroed the eager count; a lookup from before it must not
      // drive the new count negative or hold its budget.
      if (eager && generation === startedIn) eagerOutstanding -= 1;
    });
    entry.pending = request;
    touch(key, entry);
    void request.then((value) => {
      // A reset, an invalidation or an eviction replaced this entry while the
      // lookup ran; its answer must not resurrect it.
      if (generation !== startedIn || cache.get(key) !== entry) return;
      entry.pending = null;
      entry.value = value;
      entry.expiresAt =
        now() +
        (value.kind === "unavailable" ? unavailableTtlMs : settledTtlMs);
    });
    return request;
  };

  /** Evict every cached link to a message this relay event deletes or edits. */
  const invalidateForEvent = (event: {
    kind: number;
    tags: string[][];
  }): void => {
    if (!INVALIDATING_KINDS.has(event.kind)) return;
    const targets = new Set(
      event.tags
        .filter((tag) => tag[0] === "e" && typeof tag[1] === "string")
        .map((tag) => tag[1].toLowerCase()),
    );
    if (targets.size === 0) return;
    for (const [key, entry] of [...cache]) {
      if (!targets.has(entry.messageId.toLowerCase())) continue;
      cache.delete(key);
      notify(key);
    }
  };

  const subscribe = (link: MessageLinkRef, listener: () => void) => {
    const key = messageLinkMetadataKey(link);
    let set = listeners.get(key);
    if (!set) {
      set = new Set();
      listeners.set(key, set);
    }
    set.add(listener);
    return () => {
      set.delete(listener);
      if (set.size === 0 && listeners.get(key) === set) listeners.delete(key);
    };
  };

  const reset = () => {
    generation += 1;
    eagerOutstanding = 0;
    for (const dropped of queue.splice(0)) dropped.drop();
    const keys = [...cache.keys()];
    cache.clear();
    for (const key of keys) notify(key);
  };

  return {
    invalidateForEvent,
    load,
    peek,
    reset,
    size: () => cache.size,
    subscribe,
  };
}

export type MessageLinkMetadataStore = ReturnType<
  typeof createMessageLinkMetadataStore
>;
