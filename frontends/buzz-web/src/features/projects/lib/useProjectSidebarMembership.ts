import * as React from "react";

import { relayClient } from "@/shared/api/relayClient";
import {
  EMPTY_PROJECT_SIDEBAR_MEMBERSHIP_STORE,
  mergeProjectSidebarMembershipStores,
  PROJECT_SIDEBAR_MEMBERSHIP_EVENT,
  projectSidebarMembershipStoresEqual,
  projectSidebarMembershipStorageKey,
  type ProjectSidebarMembershipChange,
  readProjectSidebarMembershipStore,
  selectedProjectAddressesFromStore,
  writeProjectSidebarMembershipStore,
  type ProjectSidebarMembershipStore,
} from "./projectSidebarMembership";
import {
  ProjectSidebarMembershipSyncManager,
  type RemoteProjectSidebarMembership,
} from "./projectSidebarMembershipSync";

export function useProjectSidebarMembership(
  relayUrl: string | null,
  pubkey: string | undefined,
): string[] {
  const [store, setStoreState] =
    React.useState<ProjectSidebarMembershipStore>(
      EMPTY_PROJECT_SIDEBAR_MEMBERSHIP_STORE,
    );
  // Every change goes through setStore, so storeRef holds the newest store.
  // Remote merges, writes, publishes and the applied-remote refs run here,
  // outside React's state updaters: React may run an updater twice
  // (StrictMode, a restarted render), and an updater that advances the refs
  // it guards on returned the stale store on its second run.
  const storeRef = React.useRef(store);
  const setStore = React.useCallback(
    (next: ProjectSidebarMembershipStore) => {
      storeRef.current = next;
      setStoreState(next);
    },
    [],
  );
  const managerRef = React.useRef<ProjectSidebarMembershipSyncManager | null>(
    null,
  );
  const lastAppliedRemoteTs = React.useRef(0);
  const lastAppliedEventId = React.useRef("");

  React.useEffect(() => {
    if (!relayUrl || !pubkey) {
      setStore(EMPTY_PROJECT_SIDEBAR_MEMBERSHIP_STORE);
      lastAppliedRemoteTs.current = 0;
      lastAppliedEventId.current = "";
      return;
    }
    setStore(readProjectSidebarMembershipStore(relayUrl, pubkey));
    lastAppliedRemoteTs.current = 0;
    lastAppliedEventId.current = "";
    managerRef.current = new ProjectSidebarMembershipSyncManager(
      pubkey,
      relayUrl,
    );
    return () => {
      managerRef.current?.destroy();
      managerRef.current = null;
    };
  }, [pubkey, relayUrl, setStore]);

  React.useEffect(() => {
    if (!relayUrl || !pubkey) return;
    const scopeKey = projectSidebarMembershipStorageKey(relayUrl, pubkey);
    // The event is global; a change for another community or account (an
    // async create that resolved after a switch) is not this scope's to
    // publish, and publishing it would bypass this scope's bootstrap hold.
    const refreshAndPublish = (event: Event) => {
      const detail = (event as CustomEvent<ProjectSidebarMembershipChange>)
        .detail;
      if (
        !detail ||
        projectSidebarMembershipStorageKey(detail.relayOrigin, detail.pubkey) !==
          scopeKey
      ) {
        return;
      }
      const next = readProjectSidebarMembershipStore(relayUrl, pubkey);
      setStore(next);
      managerRef.current?.publishMembership(next);
    };
    globalThis.addEventListener(
      PROJECT_SIDEBAR_MEMBERSHIP_EVENT,
      refreshAndPublish,
    );
    return () => {
      globalThis.removeEventListener(
        PROJECT_SIDEBAR_MEMBERSHIP_EVENT,
        refreshAndPublish,
      );
    };
  }, [pubkey, relayUrl, setStore]);

  React.useEffect(() => {
    if (!relayUrl || !pubkey) return;
    const key = projectSidebarMembershipStorageKey(relayUrl, pubkey);
    const handleStorage = (event: StorageEvent) => {
      if (event.key === key) {
        setStore(readProjectSidebarMembershipStore(relayUrl, pubkey));
      }
    };
    globalThis.addEventListener("storage", handleStorage);
    return () => globalThis.removeEventListener("storage", handleStorage);
  }, [pubkey, relayUrl, setStore]);

  const applyRemote = React.useCallback(
    (remote: RemoteProjectSidebarMembership): void => {
      if (!relayUrl || !pubkey) return;
      if (remote.createdAt < lastAppliedRemoteTs.current) return;
      if (
        remote.createdAt === lastAppliedRemoteTs.current &&
        remote.eventId <= lastAppliedEventId.current
      ) {
        return;
      }
      lastAppliedRemoteTs.current = remote.createdAt;
      lastAppliedEventId.current = remote.eventId;
      managerRef.current?.cancelPendingPublish();
      const merged = mergeProjectSidebarMembershipStores(
        storeRef.current,
        remote.store,
      );
      // The write records merged as the authoritative in-session state even
      // when the localStorage mirror fails, so the merge always applies.
      writeProjectSidebarMembershipStore(relayUrl, pubkey, merged, false);
      setStore(merged);
      if (!projectSidebarMembershipStoresEqual(merged, remote.store)) {
        managerRef.current?.publishMembership(merged);
      }
    },
    [pubkey, relayUrl, setStore],
  );

  React.useEffect(() => {
    if (!relayUrl || !pubkey) return;
    let cancelled = false;
    const local = readProjectSidebarMembershipStore(relayUrl, pubkey);
    void managerRef.current?.bootstrap(local).then((result) => {
      if (cancelled) return;
      if (result.action === "apply-remote") {
        applyRemote(result.data);
      }
    });
    return () => {
      cancelled = true;
    };
  }, [applyRemote, pubkey, relayUrl]);

  React.useEffect(() => {
    if (!relayUrl || !pubkey) return;
    let unsubscribe: (() => Promise<void>) | null = null;
    let cancelled = false;
    void managerRef.current
      ?.subscribe((remote) => {
        if (!cancelled) applyRemote(remote);
      })
      .then((dispose) => {
        if (cancelled) {
          void dispose();
        } else {
          unsubscribe = dispose;
        }
      });
    return () => {
      cancelled = true;
      if (unsubscribe) void unsubscribe();
    };
  }, [applyRemote, pubkey, relayUrl]);

  React.useEffect(() => {
    if (!relayUrl || !pubkey) return;
    let cancelled = false;
    const unsubscribe = relayClient.subscribeToReconnects(() => {
      void managerRef.current?.fetchRemoteMembership().then((result) => {
        if (cancelled) return;
        if (result.status === "found") {
          applyRemote(result.data);
        }
        const pending = managerRef.current?.getPendingStore();
        if (pending) managerRef.current?.publishMembership(pending);
      });
    });
    return () => {
      cancelled = true;
      unsubscribe();
    };
  }, [applyRemote, pubkey, relayUrl]);

  return React.useMemo(() => selectedProjectAddressesFromStore(store), [store]);
}
