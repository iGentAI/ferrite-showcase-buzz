import { relayClient } from "@/shared/api/relayClient";
import {
  nip44DecryptFromSelf,
  nip44EncryptToSelf,
  signRelayEvent,
} from "@/shared/api/tauri";
import type { RelayEvent } from "@/shared/api/types";
import { KIND_CHANNEL_SECTIONS } from "@/shared/constants/kinds";
import { mergeChannelSectionStores } from "./channelSectionsMerge";
import {
  DEFAULT_STORE,
  parseChannelSectionPayload,
  type ChannelSection,
  type ChannelSectionStore,
} from "./channelSectionsStorage";
import {
  isAuthenticOwnSyncEvent,
  isPlausibleSyncTimestamp,
  syncPublishCreatedAt,
  syncPublishRetryDelayMs,
} from "./sidebarSyncEventTrust";
import {
  advanceWatermark,
  readWatermark,
  runBootstrap,
  type FetchResult,
} from "./sidebarSyncWatermark";

const D_TAG = "channel-sections";
const BLOB_TYPE = D_TAG;
const DEBOUNCE_MS = 2_000;
const BLOB_ADDRESS = { kind: KIND_CHANNEL_SECTIONS, dTag: D_TAG };

export type RemoteSections = {
  store: ChannelSectionStore;
  createdAt: number;
  eventId: string;
};

async function decryptAndParse(
  event: RelayEvent,
): Promise<RemoteSections | null> {
  try {
    const plaintext = await nip44DecryptFromSelf(event.content);
    const store = parseChannelSectionPayload(JSON.parse(plaintext));
    if (!store) return null;
    return { store, createdAt: event.created_at, eventId: event.id };
  } catch {
    return null;
  }
}

export class ChannelSectionSyncManager {
  private pubkey: string;
  private relayUrl: string;
  private debounceTimer: number | null = null;
  private lastRemoteCreatedAt: number;
  private pendingStore: ChannelSectionStore | null = null;
  // The store the first queued edit was made on. A debounced publish carries
  // every edit queued since then, so it merges against this base.
  private pendingBase: ChannelSectionStore | null = null;
  private lastPublishedStore: ChannelSectionStore | null = null;
  // The newest blob this manager knows to be on the relay: the fallback base
  // for an edit queued without one.
  private lastSyncedStore: ChannelSectionStore | null = null;
  private lastSyncedCreatedAt = 0;
  private destroyed = false;

  constructor(pubkey: string, relayUrl: string) {
    this.pubkey = pubkey;
    this.relayUrl = relayUrl;
    // Hydrate from localStorage so we never seed-publish if a remote blob has
    // been seen in a prior session.
    this.lastRemoteCreatedAt = readWatermark(pubkey, BLOB_TYPE, relayUrl);
  }

  async fetchRemoteSections(): Promise<FetchResult<RemoteSections>> {
    try {
      const events = await relayClient.fetchEvents({
        kinds: [KIND_CHANNEL_SECTIONS],
        authors: [this.pubkey],
        "#d": [D_TAG],
        limit: 1,
      });
      if (events.length === 0) {
        return { status: "absent" };
      }
      const event = events[0];
      // The filter names only this user, so an event that is not a verifiable
      // event of theirs is a relay fault, not proof that no blob exists: it
      // must not unlock the seed-publish path or move the watermark.
      if (!isAuthenticOwnSyncEvent(event, this.pubkey, BLOB_ADDRESS)) {
        return { status: "failed" };
      }
      // A genuine event exists — record its created_at regardless of whether
      // we can decrypt it, so seed-publish is blocked even when the payload is
      // unreadable (e.g. wrong key).
      this.recordRemoteHead(event.created_at);
      const result = await decryptAndParse(event);
      if (!result) {
        return { status: "failed", createdAt: event.created_at };
      }
      this.noteSynced(result.store, result.createdAt);
      return {
        status: "found",
        data: result,
        createdAt: result.createdAt,
        eventId: result.eventId,
      };
    } catch {
      return { status: "failed" };
    }
  }

  private noteSynced(store: ChannelSectionStore, createdAt: number): void {
    if (createdAt < this.lastSyncedCreatedAt) return;
    this.lastSyncedStore = store;
    this.lastSyncedCreatedAt = createdAt;
  }

  /** Update in-memory + persisted watermark. */
  private recordRemoteHead(createdAt: number): void {
    if (!isPlausibleSyncTimestamp(createdAt)) return;
    if (createdAt > this.lastRemoteCreatedAt) {
      this.lastRemoteCreatedAt = createdAt;
    }
    advanceWatermark(this.pubkey, BLOB_TYPE, this.relayUrl, createdAt);
  }

  cancelPendingPublish(): void {
    if (this.debounceTimer !== null) {
      window.clearTimeout(this.debounceTimer);
      this.debounceTimer = null;
    }
  }

  getPendingStore(): ChannelSectionStore | null {
    return this.pendingStore;
  }

  getPendingBase(): ChannelSectionStore | null {
    return this.pendingBase;
  }

  /**
   * Replace the queued edit after a remote blob arrived: drop the old queue
   * and, when `store` still differs from the remote, queue it against the
   * remote as its new base.
   */
  rebasePending(
    store: ChannelSectionStore | null,
    base: ChannelSectionStore,
  ): void {
    this.cancelPendingPublish();
    this.pendingStore = null;
    this.pendingBase = null;
    this.lastSyncedStore = base;
    if (store) this.publishSections(store, base);
  }

  /**
   * Queue `store` for a debounced publish. `base` is the store the edit was
   * made on (the hook's state before the edit); it lets the publish merge the
   * edit with whatever the relay holds by then instead of overwriting it.
   */
  publishSections(
    store: ChannelSectionStore,
    base?: ChannelSectionStore,
  ): void {
    if (this.pendingStore === null || this.pendingBase === null) {
      this.pendingBase = base ?? this.lastSyncedStore ?? DEFAULT_STORE;
    }
    this.pendingStore = store;
    const pendingBase = this.pendingBase;
    if (this.debounceTimer !== null) {
      window.clearTimeout(this.debounceTimer);
    }
    this.debounceTimer = window.setTimeout(() => {
      this.debounceTimer = null;
      // The base is read when the publish starts: a publish that finished
      // meanwhile may have advanced it (settlePending).
      void this.doPublish(store, this.pendingBase ?? pendingBase);
    }, DEBOUNCE_MS);
  }

  private settlePending(
    store: ChannelSectionStore,
    published?: ChannelSectionStore,
  ): void {
    // Only the publish of the newest queued edit settles the queue; an edit
    // queued while this one was in flight keeps its store, and its base
    // moves to what was just published, so the next merge does not revert
    // keys this publish set and the newer edit removed.
    if (this.pendingStore !== store) {
      if (this.pendingStore !== null && published) {
        this.pendingBase = published;
      }
      return;
    }
    this.pendingStore = null;
    this.pendingBase = null;
  }

  private async fetchOwnBlobBeforePublish(
    store: ChannelSectionStore,
    base: ChannelSectionStore,
  ): Promise<ChannelSectionStore> {
    try {
      const events = await relayClient.fetchEvents({
        kinds: [KIND_CHANNEL_SECTIONS],
        authors: [this.pubkey],
        "#d": [D_TAG],
        limit: 1,
      });
      if (events.length === 0) return store;
      const event = events[0];
      if (!isAuthenticOwnSyncEvent(event, this.pubkey, BLOB_ADDRESS)) return store;
      // Snapshot the watermark before advancing it: after recordRemoteHead
      // runs, lastRemoteCreatedAt equals event.created_at, so the comparison
      // remote.createdAt > lastRemoteCreatedAt would always be false and
      // silently suppress the merge.
      const headBeforeFetch = this.lastRemoteCreatedAt;
      this.recordRemoteHead(event.created_at);
      const remote = await decryptAndParse(event);
      if (!remote) return store;
      // Merge whenever the relay holds the newest blob this manager knows of,
      // or a newer one. The queued edit may predate it: another device's
      // write, or this manager's own last publish (a merge) whose echo has not
      // reached the hook yet. Merging against the edit's own base keeps both
      // sides; a relay answering with an older blob than we know is ignored.
      if (
        remote.createdAt > headBeforeFetch ||
        remote.createdAt >= this.lastSyncedCreatedAt
      ) {
        return mergeChannelSectionStores(base, store, remote.store);
      }
      return store;
    } catch {
      return store;
    }
  }

  private isIdenticalToLastPublished(store: ChannelSectionStore): boolean {
    if (!this.lastPublishedStore) return false;
    const lastSections = this.lastPublishedStore.sections;
    const currentSections = store.sections;
    if (lastSections.length !== currentSections.length) return false;
    for (let i = 0; i < currentSections.length; i++) {
      const last = lastSections[i] as ChannelSection | undefined;
      const current = currentSections[i] as ChannelSection;
      if (
        !last ||
        last.id !== current.id ||
        last.name !== current.name ||
        last.icon !== current.icon ||
        last.order !== current.order
      )
        return false;
    }
    const lastAssignKeys = Object.keys(this.lastPublishedStore.assignments);
    const currentAssignKeys = Object.keys(store.assignments);
    if (lastAssignKeys.length !== currentAssignKeys.length) return false;
    for (const key of currentAssignKeys) {
      if (this.lastPublishedStore.assignments[key] !== store.assignments[key])
        return false;
    }
    return true;
  }

  /** Re-run a publish that had to wait for a strictly newer timestamp. */
  private schedulePublishRetry(
    store: ChannelSectionStore,
    base: ChannelSectionStore,
  ): void {
    if (this.destroyed || this.debounceTimer !== null) return;
    this.debounceTimer = window.setTimeout(() => {
      this.debounceTimer = null;
      void this.doPublish(store, base);
    }, syncPublishRetryDelayMs(this.lastRemoteCreatedAt));
  }

  private async doPublish(
    store: ChannelSectionStore,
    base: ChannelSectionStore,
  ): Promise<void> {
    try {
      const merged = await this.fetchOwnBlobBeforePublish(store, base);
      // Guard: manager may have been destroyed while fetchOwnBlobBeforePublish
      // was awaited (community switch during in-flight fetch). If so, abort
      // before touching the relay.
      if (this.destroyed) return;
      if (this.isIdenticalToLastPublished(merged)) {
        this.settlePending(store);
        return;
      }
      const payload = {
        version: 1,
        sections: merged.sections,
        assignments: merged.assignments,
      };
      const ciphertext = await nip44EncryptToSelf(JSON.stringify(payload));
      const createdAt = syncPublishCreatedAt(this.lastRemoteCreatedAt);
      if (createdAt === null) {
        this.schedulePublishRetry(store, base);
        return;
      }
      const event = await signRelayEvent({
        kind: KIND_CHANNEL_SECTIONS,
        content: ciphertext,
        createdAt,
        tags: [
          ["d", D_TAG],
          ["t", D_TAG], // relay discoverability; not used in our filters
        ],
      });
      // Final guard immediately before the network call — sign/encrypt are
      // synchronous-ish but cheap; the relay socket may have moved to a
      // different community by the time we reach this point.
      if (this.destroyed) return;
      await relayClient.publishEvent(
        event,
        "Timed out publishing channel sections.",
        "Failed to publish channel sections.",
      );
      this.recordRemoteHead(event.created_at);
      this.noteSynced(merged, event.created_at);
      this.lastPublishedStore = merged;
      this.settlePending(store, merged);
    } catch (error) {
      console.warn("[channelSectionsSync] publish failed:", error);
    }
  }

  async subscribeToSections(
    onUpdate: (remote: RemoteSections) => void,
  ): Promise<() => Promise<void>> {
    return relayClient.subscribeLive(
      {
        kinds: [KIND_CHANNEL_SECTIONS],
        authors: [this.pubkey],
        "#d": [D_TAG],
        limit: 0,
      },
      (event: RelayEvent) => {
        if (!isAuthenticOwnSyncEvent(event, this.pubkey, BLOB_ADDRESS)) return;
        // Record the raw head before decrypt so an undecryptable live event
        // still advances the watermark and blocks future seed-publish.
        this.recordRemoteHead(event.created_at);
        void decryptAndParse(event).then((result) => {
          if (result) {
            this.noteSynced(result.store, result.createdAt);
            onUpdate(result);
          }
        });
      },
    );
  }

  /**
   * Fetches the remote blob on first mount, records the remote head, and
   * delegates the seed/hold/apply-remote decision to `runBootstrap`.
   */
  async bootstrap(localStore: ChannelSectionStore) {
    const fetchResult = await this.fetchRemoteSections();
    return runBootstrap({
      fetchResult,
      lastHead: this.lastRemoteCreatedAt,
      localStore,
      isLocalNonEmpty: (s) => s.sections.length > 0,
      publishFn: (s) => this.publishSections(s, DEFAULT_STORE),
    });
  }

  destroy(): void {
    // Cancel any pending publish and mark this manager as destroyed so any
    // in-flight doPublish() calls abort before reaching relayClient.
    // Pending debounce-window changes are intentionally dropped: flushing
    // could publish relay A's sections to relay B via the shared relayClient
    // singleton. On return, bootstrap's found path whole-blob-replaces from
    // remote, so any dropped pending edit is lost.
    this.destroyed = true;
    this.cancelPendingPublish();
    this.pendingStore = null;
    this.pendingBase = null;
  }
}
