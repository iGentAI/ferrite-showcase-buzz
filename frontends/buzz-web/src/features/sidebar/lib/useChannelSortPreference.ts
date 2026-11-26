import * as React from "react";

import { relayClient } from "@/shared/api/relayClient";
import {
  boundChannelSortStore,
  DEFAULT_STORE,
  readChannelSortStore,
  sortModeForGroup,
  storageKey,
  stripOrphanedSectionModes,
  writeChannelSortStore,
  type ChannelSortGroupKey,
  type ChannelSortMode,
  type ChannelSortStore,
} from "./channelSortPreference";
import { ChannelSortSyncManager } from "./channelSortSync";
import type { RemoteSortPrefs } from "./channelSortSync";

/**
 * Persistent per-group sidebar sort preferences, scoped by pubkey + relay so
 * they don't bleed across identities or communities (same scoping as channel
 * sections). Each sidebar grouping (starred, channels, forums, dms, and each
 * custom section) carries its own saved Recent/A–Z mode; unset groups default
 * to A–Z. Mirrors changes made in other windows via the storage event.
 *
 * Preferences sync across clients via encrypted NIP-78 app data (kind 30078,
 * d-tag `channel-sort`), following the channel-sections pattern: localStorage
 * stays the instant/offline cache, the relay blob is the cross-client source
 * of truth, and conflicts resolve with whole-blob last-write-wins.
 *
 * When `liveSectionIds` is provided and non-empty, writes also prune
 * `section:<id>` entries whose custom section no longer exists, so deleted
 * sections don't leave stale keys in localStorage. An empty list is not
 * trusted for pruning: it is also what the caller passes while the sections
 * blob has not loaded yet (or failed to load), and pruning then would delete
 * and publish away every per-section preference. The store stays bounded by
 * `MAX_CHANNEL_SORT_GROUPS` either way.
 */
export function useChannelSortPreference(
  pubkey: string | undefined,
  relayUrl?: string,
  liveSectionIds?: string[],
): {
  sortModeFor: (group: ChannelSortGroupKey) => ChannelSortMode;
  setSortModeFor: (group: ChannelSortGroupKey, mode: ChannelSortMode) => void;
} {
  const [store, setStoreState] = React.useState<ChannelSortStore>(() => {
    if (!pubkey) return DEFAULT_STORE;
    return readChannelSortStore(pubkey, relayUrl);
  });
  // Side effects (ref guards, storage writes, publishes) run outside React's
  // state updaters, which StrictMode and concurrent renders may call twice
  // or discard; the ref holds the latest store for them
  // (finding_6dd5c5a3eb65d895).
  const storeRef = React.useRef(store);
  const setStore = React.useCallback((next: ChannelSortStore) => {
    storeRef.current = next;
    setStoreState(next);
  }, []);

  const managerRef = React.useRef<ChannelSortSyncManager | null>(null);
  const lastAppliedRemoteTs = React.useRef(0);
  const lastAppliedEventId = React.useRef("");

  React.useEffect(() => {
    if (!pubkey || !relayUrl) {
      setStore(DEFAULT_STORE);
      lastAppliedRemoteTs.current = 0;
      lastAppliedEventId.current = "";
      return;
    }
    setStore(readChannelSortStore(pubkey, relayUrl));
    lastAppliedRemoteTs.current = 0;
    lastAppliedEventId.current = "";
    managerRef.current = new ChannelSortSyncManager(pubkey, relayUrl);
    return () => {
      managerRef.current?.destroy();
      managerRef.current = null;
    };
  }, [pubkey, relayUrl]);

  React.useEffect(() => {
    if (!pubkey) return;
    const key = storageKey(pubkey, relayUrl);
    const handler = (e: StorageEvent) => {
      if (e.key !== key) return;
      setStore(readChannelSortStore(pubkey, relayUrl));
    };
    window.addEventListener("storage", handler);
    return () => {
      window.removeEventListener("storage", handler);
    };
  }, [pubkey, relayUrl]);

  const applyRemote = React.useCallback(
    (remote: RemoteSortPrefs): void => {
      if (!pubkey) return;
      if (remote.createdAt < lastAppliedRemoteTs.current) return;
      if (
        remote.createdAt === lastAppliedRemoteTs.current &&
        remote.eventId <= lastAppliedEventId.current
      )
        return;
      // Only a remote that was actually stored counts as applied.
      if (!writeChannelSortStore(pubkey, remote.store, relayUrl)) return;
      lastAppliedRemoteTs.current = remote.createdAt;
      lastAppliedEventId.current = remote.eventId;
      managerRef.current?.cancelPendingPublish();
      setStore(remote.store);
    },
    [pubkey, relayUrl, setStore],
  );

  React.useEffect(() => {
    if (!pubkey || !relayUrl) return;
    let cancelled = false;
    const local = readChannelSortStore(pubkey, relayUrl);
    void managerRef.current?.bootstrap(local).then((result) => {
      if (cancelled) return;
      if (result.action === "apply-remote") {
        applyRemote(result.data);
      }
      // "hold": seed already performed by bootstrap (if first-sync), or blocked.
    });
    return () => {
      cancelled = true;
    };
  }, [pubkey, relayUrl, applyRemote]);

  React.useEffect(() => {
    if (!pubkey) return;
    let unsub: (() => Promise<void>) | null = null;
    let cancelled = false;
    void managerRef.current
      ?.subscribeToSortPrefs((remote) => {
        if (cancelled) return;
        applyRemote(remote);
      })
      .then((dispose) => {
        if (cancelled) {
          void dispose();
        } else {
          unsub = dispose;
        }
      });
    return () => {
      cancelled = true;
      if (unsub) void unsub();
    };
  }, [pubkey, applyRemote]);

  React.useEffect(() => {
    if (!pubkey) return;
    let cancelled = false;
    const unsub = relayClient.subscribeToReconnects(() => {
      void managerRef.current?.fetchRemoteSortPrefs().then((result) => {
        if (cancelled) return;
        if (result.status === "found") {
          applyRemote(result.data);
        }
        const pending = managerRef.current?.getPendingStore();
        if (pending) {
          managerRef.current?.publishSortPrefs(pending);
        }
      });
    });
    return () => {
      cancelled = true;
      unsub();
    };
  }, [pubkey, applyRemote]);

  const sortModeFor = React.useCallback(
    (group: ChannelSortGroupKey) => sortModeForGroup(store, group),
    [store],
  );

  const setSortModeFor = React.useCallback(
    (group: ChannelSortGroupKey, mode: ChannelSortMode) => {
      if (!pubkey) return;
      {
        const prev = storeRef.current;
        const withUpdate: ChannelSortStore = {
          ...prev,
          groups: { ...prev.groups, [group]: mode },
        };
        // Prune sort modes left behind by deleted custom sections on write so
        // the stored map can't grow unboundedly with stale `section:` keys.
        const next = boundChannelSortStore(
          liveSectionIds && liveSectionIds.length > 0
            ? stripOrphanedSectionModes(withUpdate, liveSectionIds)
            : withUpdate,
        );
        if (!writeChannelSortStore(pubkey, next, relayUrl)) return;
        managerRef.current?.publishSortPrefs(next);
        setStore(next);
      }
    },
    [pubkey, relayUrl, liveSectionIds, setStore],
  );

  return { sortModeFor, setSortModeFor };
}
