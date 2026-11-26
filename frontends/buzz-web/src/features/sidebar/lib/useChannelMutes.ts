import * as React from "react";

import { relayClient } from "@/shared/api/relayClient";
import {
  boundMuteStore,
  DEFAULT_STORE,
  mergeStores,
  mutedChannelIdsFromStore,
  readChannelMutesStore,
  storageKey,
  writeChannelMutesStore,
  type ChannelMuteEntry,
  type ChannelMuteStore,
} from "./channelMutesStorage";
import { ChannelMuteSyncManager } from "./channelMutesSync";
import type { RemoteMutes } from "./channelMutesSync";

function sameMuteEntries(
  left: Record<string, ChannelMuteEntry>,
  right: Record<string, ChannelMuteEntry>,
): boolean {
  const keys = Object.keys(left);
  if (keys.length !== Object.keys(right).length) return false;
  return keys.every((key) => {
    if (!Object.prototype.hasOwnProperty.call(right, key)) return false;
    const l = left[key] as ChannelMuteEntry;
    const r = right[key] as ChannelMuteEntry;
    return l.muted === r.muted && l.updatedAt === r.updatedAt;
  });
}

export function useChannelMutes(
  pubkey: string | undefined,
  relayUrl?: string,
): {
  mutedChannelIds: Set<string>;
  muteChannel: (channelId: string) => void;
  unmuteChannel: (channelId: string) => void;
} {
  const [store, setStoreState] = React.useState<ChannelMuteStore>(() => {
    if (!pubkey) {
      return DEFAULT_STORE;
    }
    return readChannelMutesStore(pubkey);
  });
  // Every change goes through setStore, so storeRef always holds the newest
  // store. Merges, localStorage writes, publishes and the applied-remote refs
  // are handled here, outside React's state updaters: React may run an updater
  // more than once (StrictMode, or a restarted concurrent render), and an
  // updater with side effects that guards on refs it mutates drops the update
  // on its second run.
  const storeRef = React.useRef(store);
  const setStore = React.useCallback((next: ChannelMuteStore) => {
    storeRef.current = next;
    setStoreState(next);
  }, []);

  const managerRef = React.useRef<ChannelMuteSyncManager | null>(null);
  const lastAppliedRemoteTs = React.useRef(0);
  const lastAppliedEventId = React.useRef("");

  React.useEffect(() => {
    if (!pubkey) {
      setStore(DEFAULT_STORE);
      lastAppliedRemoteTs.current = 0;
      lastAppliedEventId.current = "";
      return;
    }
    if (!relayUrl) {
      // Mutes are stored per pubkey, not per relay: without a relay yet the
      // local store stays loaded (a mutation then keeps every saved mute);
      // only the sync manager waits for the relay.
      setStore(readChannelMutesStore(pubkey));
      lastAppliedRemoteTs.current = 0;
      lastAppliedEventId.current = "";
      return;
    }
    setStore(readChannelMutesStore(pubkey));
    lastAppliedRemoteTs.current = 0;
    lastAppliedEventId.current = "";
    managerRef.current = new ChannelMuteSyncManager(pubkey, relayUrl);
    return () => {
      managerRef.current?.destroy();
      managerRef.current = null;
    };
  }, [pubkey, relayUrl, setStore]);

  React.useEffect(() => {
    if (!pubkey) {
      return;
    }
    const key = storageKey(pubkey);
    const handler = (e: StorageEvent) => {
      if (e.key !== key) {
        return;
      }
      setStore(readChannelMutesStore(pubkey));
    };
    window.addEventListener("storage", handler);
    return () => {
      window.removeEventListener("storage", handler);
    };
  }, [pubkey, setStore]);

  const applyRemote = React.useCallback(
    (remote: RemoteMutes) => {
      if (!pubkey) return;
      if (remote.createdAt < lastAppliedRemoteTs.current) return;
      if (
        remote.createdAt === lastAppliedRemoteTs.current &&
        remote.eventId <= lastAppliedEventId.current
      )
        return;
      const merged = mergeStores(storeRef.current, remote.store);
      // Persist first. If the write fails nothing is applied, so a later
      // delivery of the same event is not mistaken for one already applied.
      if (!writeChannelMutesStore(pubkey, merged)) return;
      lastAppliedRemoteTs.current = remote.createdAt;
      lastAppliedEventId.current = remote.eventId;
      setStore(merged);
      // Local changes the remote blob lacks are still unsynchronized: publish
      // the merge instead of cancelling the pending publish that carried them.
      if (sameMuteEntries(merged.channels, remote.store.channels)) {
        managerRef.current?.cancelPendingMutePublish();
      } else {
        managerRef.current?.publishMutes(merged);
      }
    },
    [pubkey, setStore],
  );

  React.useEffect(() => {
    if (!pubkey || !relayUrl) return;
    let cancelled = false;
    const local = readChannelMutesStore(pubkey);
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
      ?.subscribeToMutes((remote) => {
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
      void managerRef.current?.fetchRemoteMutes().then((result) => {
        if (cancelled) return;
        if (result.status === "found") {
          // applyRemote either cancels the pending publish (the remote holds
          // it) or publishes the merged store itself.
          applyRemote(result.data);
          return;
        }
        const pending = managerRef.current?.getPendingMuteStore();
        if (pending) {
          managerRef.current?.publishMutes(pending);
        }
      });
    });
    return () => {
      cancelled = true;
      unsub();
    };
  }, [pubkey, relayUrl, applyRemote]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: store.channels is the relevant dep — the outer store identity can change without channels changing (e.g., on reconnect writes)
  const mutedChannelIds = React.useMemo(
    () => mutedChannelIdsFromStore(store),
    [store.channels],
  );

  const setMuteState = React.useCallback(
    (channelId: string, muted: boolean) => {
      if (!pubkey) return;
      const entry: ChannelMuteEntry = {
        muted,
        updatedAt: Math.floor(Date.now() / 1000),
      };
      const next = boundMuteStore(
        {
          version: 1,
          channels: { ...storeRef.current.channels, [channelId]: entry },
        },
        channelId,
      );
      if (!writeChannelMutesStore(pubkey, next)) return;
      setStore(next);
      managerRef.current?.publishMutes(next);
    },
    [pubkey, setStore],
  );

  const muteChannel = React.useCallback(
    (channelId: string) => setMuteState(channelId, true),
    [setMuteState],
  );
  const unmuteChannel = React.useCallback(
    (channelId: string) => setMuteState(channelId, false),
    [setMuteState],
  );

  return {
    mutedChannelIds,
    muteChannel,
    unmuteChannel,
  };
}
