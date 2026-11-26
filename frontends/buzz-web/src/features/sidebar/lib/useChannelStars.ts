import * as React from "react";

import { relayClient } from "@/shared/api/relayClient";
import {
  boundStarStore,
  DEFAULT_STORE,
  mergeStores,
  readChannelStarsStore,
  starredChannelIdsFromStore,
  storageKey,
  writeChannelStarsStore,
  type ChannelStarEntry,
  type ChannelStarStore,
} from "./channelStarsStorage";
import { ChannelStarSyncManager } from "./channelStarsSync";
import type { RemoteStars } from "./channelStarsSync";

function sameStarEntries(
  left: Record<string, ChannelStarEntry>,
  right: Record<string, ChannelStarEntry>,
): boolean {
  const keys = Object.keys(left);
  if (keys.length !== Object.keys(right).length) return false;
  return keys.every((key) => {
    if (!Object.prototype.hasOwnProperty.call(right, key)) return false;
    const l = left[key] as ChannelStarEntry;
    const r = right[key] as ChannelStarEntry;
    return l.starred === r.starred && l.updatedAt === r.updatedAt;
  });
}

export function useChannelStars(
  pubkey: string | undefined,
  relayUrl?: string,
): {
  starredChannelIds: Set<string>;
  starChannel: (channelId: string) => void;
  unstarChannel: (channelId: string) => void;
} {
  const [store, setStoreState] = React.useState<ChannelStarStore>(() => {
    if (!pubkey || !relayUrl) {
      return DEFAULT_STORE;
    }
    return readChannelStarsStore(pubkey, relayUrl);
  });
  // Every change goes through setStore, so storeRef always holds the newest
  // store. Merges, localStorage writes, publishes and the applied-remote refs
  // are handled here, outside React's state updaters: React may run an updater
  // more than once (StrictMode, or a restarted concurrent render), and an
  // updater with side effects that guards on refs it mutates drops the update
  // on its second run.
  const storeRef = React.useRef(store);
  const setStore = React.useCallback((next: ChannelStarStore) => {
    storeRef.current = next;
    setStoreState(next);
  }, []);

  const managerRef = React.useRef<ChannelStarSyncManager | null>(null);
  const lastAppliedRemoteTs = React.useRef(0);
  const lastAppliedEventId = React.useRef("");

  React.useEffect(() => {
    if (!pubkey || !relayUrl) {
      setStore(DEFAULT_STORE);
      lastAppliedRemoteTs.current = 0;
      lastAppliedEventId.current = "";
      return;
    }
    setStore(readChannelStarsStore(pubkey, relayUrl));
    lastAppliedRemoteTs.current = 0;
    lastAppliedEventId.current = "";
    managerRef.current = new ChannelStarSyncManager(pubkey, relayUrl);
    return () => {
      managerRef.current?.destroy();
      managerRef.current = null;
    };
  }, [pubkey, relayUrl, setStore]);

  React.useEffect(() => {
    if (!pubkey || !relayUrl) {
      return;
    }
    const key = storageKey(pubkey, relayUrl);
    const handler = (e: StorageEvent) => {
      if (e.key !== key) {
        return;
      }
      setStore(readChannelStarsStore(pubkey, relayUrl));
    };
    window.addEventListener("storage", handler);
    return () => {
      window.removeEventListener("storage", handler);
    };
  }, [pubkey, relayUrl, setStore]);

  const applyRemote = React.useCallback(
    (remote: RemoteStars) => {
      if (!pubkey || !relayUrl) return;
      if (remote.createdAt < lastAppliedRemoteTs.current) return;
      if (
        remote.createdAt === lastAppliedRemoteTs.current &&
        remote.eventId <= lastAppliedEventId.current
      )
        return;
      const merged = mergeStores(storeRef.current, remote.store);
      // Persist first. If the write fails nothing is applied, so a later
      // delivery of the same event is not mistaken for one already applied.
      if (!writeChannelStarsStore(pubkey, merged, relayUrl)) return;
      lastAppliedRemoteTs.current = remote.createdAt;
      lastAppliedEventId.current = remote.eventId;
      setStore(merged);
      // Local changes the remote blob lacks are still unsynchronized: publish
      // the merge instead of cancelling the pending publish that carried them.
      if (sameStarEntries(merged.channels, remote.store.channels)) {
        managerRef.current?.cancelPendingStarPublish();
      } else {
        managerRef.current?.publishStars(merged);
      }
    },
    [pubkey, relayUrl, setStore],
  );

  React.useEffect(() => {
    if (!pubkey || !relayUrl) return;
    let cancelled = false;
    const local = readChannelStarsStore(pubkey, relayUrl);
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

  // biome-ignore lint/correctness/useExhaustiveDependencies: relayUrl is intentional — rebinds subscription when the active relay changes even though it is not used inside the effect body directly (the manager via managerRef.current carries it)
  React.useEffect(() => {
    if (!pubkey) return;
    let unsub: (() => Promise<void>) | null = null;
    let cancelled = false;
    void managerRef.current
      ?.subscribeToStars((remote) => {
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
  }, [pubkey, relayUrl, applyRemote]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: relayUrl is intentional — rebinds reconnect listener when the active relay changes (community switch) even though it is not referenced directly inside the effect body
  React.useEffect(() => {
    if (!pubkey) return;
    let cancelled = false;
    const unsub = relayClient.subscribeToReconnects(() => {
      void managerRef.current?.fetchRemoteStars().then((result) => {
        if (cancelled) return;
        if (result.status === "found") {
          applyRemote(result.data);
        }
        const pending = managerRef.current?.getPendingStarStore();
        if (pending) {
          managerRef.current?.publishStars(pending);
        }
      });
    });
    return () => {
      cancelled = true;
      unsub();
    };
  }, [pubkey, relayUrl, applyRemote]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: store.channels is the relevant dep — the outer store identity can change without channels changing (e.g., on reconnect writes)
  const starredChannelIds = React.useMemo(
    () => starredChannelIdsFromStore(store),
    [store.channels],
  );

  const setStarState = React.useCallback(
    (channelId: string, starred: boolean) => {
      // With no community there is no store to change: writing the default
      // store here would wipe the stars this account has.
      if (!pubkey || !relayUrl) return;
      const entry: ChannelStarEntry = {
        starred,
        updatedAt: Math.floor(Date.now() / 1000),
      };
      const next = boundStarStore(
        {
          version: 1,
          channels: { ...storeRef.current.channels, [channelId]: entry },
        },
        channelId,
      );
      if (!writeChannelStarsStore(pubkey, next, relayUrl)) return;
      setStore(next);
      managerRef.current?.publishStars(next);
    },
    [pubkey, relayUrl, setStore],
  );

  const starChannel = React.useCallback(
    (channelId: string) => setStarState(channelId, true),
    [setStarState],
  );
  const unstarChannel = React.useCallback(
    (channelId: string) => setStarState(channelId, false),
    [setStarState],
  );

  return {
    starredChannelIds,
    starChannel,
    unstarChannel,
  };
}
