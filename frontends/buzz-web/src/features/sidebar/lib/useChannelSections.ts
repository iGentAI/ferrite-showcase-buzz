import * as React from "react";

import { reconcileRemoteSections } from "./channelSectionsMerge";
import { relayClient } from "@/shared/api/relayClient";
import {
  boundChannelSectionsStore,
  DEFAULT_STORE,
  readChannelSectionsStore,
  storageKey,
  writeChannelSectionsStore,
} from "./channelSectionsStorage";
import { ChannelSectionSyncManager } from "./channelSectionsSync";
import type { RemoteSections } from "./channelSectionsSync";
import { swapSectionOrder } from "./channelSectionsHelpers";

export type { ChannelSection } from "./channelSectionsStorage";

import type {
  ChannelSection,
  ChannelSectionStore,
} from "./channelSectionsStorage";

export function useChannelSections(
  pubkey: string | undefined,
  relayUrl?: string,
): {
  sections: ChannelSection[];
  assignments: Record<string, string>;
  createSection: (name: string, icon?: string) => ChannelSection | null;
  renameSection: (sectionId: string, newName: string, icon?: string) => void;
  deleteSection: (sectionId: string) => void;
  moveSectionUp: (sectionId: string) => void;
  moveSectionDown: (sectionId: string) => void;
  reorderSections: (orderedIds: string[]) => void;
  assignChannel: (channelId: string, sectionId: string) => void;
  unassignChannel: (channelId: string) => void;
} {
  const [store, setStoreState] = React.useState<ChannelSectionStore>(() => {
    if (!pubkey) {
      return DEFAULT_STORE;
    }
    return readChannelSectionsStore(pubkey, relayUrl);
  });
  // Every change goes through setStore, which resolves an updater once,
  // against the newest store, before handing React a plain value. React may
  // run an updater more than once (StrictMode, or a restarted concurrent
  // render); the updaters below write localStorage, publish and move refs, so
  // they must never be replayed. storeRef is also the one source mutators
  // derive from, rather than a separate localStorage read that can disagree
  // with what is on screen.
  const storeRef = React.useRef(store);
  const setStore = React.useCallback(
    (
      update:
        | ChannelSectionStore
        | ((prev: ChannelSectionStore) => ChannelSectionStore),
    ) => {
      const next =
        typeof update === "function" ? update(storeRef.current) : update;
      storeRef.current = next;
      setStoreState(next);
    },
    [],
  );

  const managerRef = React.useRef<ChannelSectionSyncManager | null>(null);
  const lastAppliedRemoteTs = React.useRef(0);
  const lastAppliedEventId = React.useRef("");

  React.useEffect(() => {
    if (!pubkey || !relayUrl) {
      setStore(DEFAULT_STORE);
      lastAppliedRemoteTs.current = 0;
      lastAppliedEventId.current = "";
      return;
    }
    setStore(readChannelSectionsStore(pubkey, relayUrl));
    lastAppliedRemoteTs.current = 0;
    lastAppliedEventId.current = "";
    managerRef.current = new ChannelSectionSyncManager(pubkey, relayUrl);
    return () => {
      managerRef.current?.destroy();
      managerRef.current = null;
    };
  }, [pubkey, relayUrl, setStore]);

  React.useEffect(() => {
    if (!pubkey) {
      return;
    }
    const key = storageKey(pubkey, relayUrl);
    const handler = (e: StorageEvent) => {
      if (e.key !== key) {
        return;
      }
      setStore(readChannelSectionsStore(pubkey, relayUrl));
    };
    window.addEventListener("storage", handler);
    return () => {
      window.removeEventListener("storage", handler);
    };
  }, [pubkey, relayUrl, setStore]);

  // Runs outside React's state updaters so its side effects happen once.
  const applyRemote = React.useCallback(
    (remote: RemoteSections): void => {
      if (!pubkey) return;
      if (remote.createdAt < lastAppliedRemoteTs.current) return;
      if (
        remote.createdAt === lastAppliedRemoteTs.current &&
        remote.eventId <= lastAppliedEventId.current
      )
        return;
      // Local edits still in their debounce window are merged onto the
      // remote rather than overwritten and cancelled
      // (finding_412874c055f620ab).
      const manager = managerRef.current;
      const { store: next, republish } = reconcileRemoteSections(
        remote.store,
        manager?.getPendingStore() ?? null,
        manager?.getPendingBase() ?? null,
      );
      if (!writeChannelSectionsStore(pubkey, next, relayUrl)) return;
      lastAppliedRemoteTs.current = remote.createdAt;
      lastAppliedEventId.current = remote.eventId;
      manager?.rebasePending(republish ? next : null, remote.store);
      setStore(next);
    },
    [pubkey, relayUrl, setStore],
  );

  React.useEffect(() => {
    if (!pubkey || !relayUrl) return;
    let cancelled = false;
    const local = readChannelSectionsStore(pubkey, relayUrl);
    void managerRef.current?.bootstrap(local).then((result) => {
      if (cancelled) return;
      if (result.action === "apply-remote") {
        applyRemote(result.data);
      }
      // "hold": seed already performed by bootstrap (if first-sync), or
      // blocked (failed fetch / prior watermark). Hook does nothing.
    });
    return () => {
      cancelled = true;
    };
  }, [pubkey, relayUrl, applyRemote, setStore]);

  React.useEffect(() => {
    if (!pubkey) return;
    let unsub: (() => Promise<void>) | null = null;
    let cancelled = false;
    void managerRef.current
      ?.subscribeToSections((remote) => {
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
  }, [pubkey, applyRemote, setStore]);

  React.useEffect(() => {
    if (!pubkey) return;
    let cancelled = false;
    const unsub = relayClient.subscribeToReconnects(() => {
      void managerRef.current?.fetchRemoteSections().then((result) => {
        if (cancelled) return;
        if (result.status === "found") {
          applyRemote(result.data);
        }
        const pending = managerRef.current?.getPendingStore();
        if (pending) {
          managerRef.current?.publishSections(pending);
        }
      });
    });
    return () => {
      cancelled = true;
      unsub();
    };
  }, [pubkey, applyRemote, setStore]);

  const sections = React.useMemo<ChannelSection[]>(
    () => store.sections.slice().sort((a, b) => a.order - b.order),
    [store.sections],
  );

  const createSection = React.useCallback(
    (name: string, icon?: string): ChannelSection | null => {
      if (!pubkey) return null;
      const current = storeRef.current;
      const maxOrder =
        current.sections.length > 0
          ? Math.max(...current.sections.map((s) => s.order))
          : -1;
      const section: ChannelSection = {
        id: crypto.randomUUID(),
        name,
        ...(icon ? { icon } : {}),
        order: maxOrder + 1,
      };
      const next = boundChannelSectionsStore({
        ...current,
        sections: [...current.sections, section],
      });
      // Report failure to the caller: nothing was created, so it must not go
      // on to assign a channel to a section that does not exist. That covers a
      // failed write and a new section the store bound did not keep.
      if (!next.sections.some((s) => s.id === section.id)) return null;
      if (!writeChannelSectionsStore(pubkey, next, relayUrl)) return null;
      setStore(next);
      managerRef.current?.publishSections(next, current);
      return section;
    },
    [pubkey, relayUrl, setStore],
  );

  const renameSection = React.useCallback(
    (sectionId: string, newName: string, icon?: string) => {
      if (!pubkey) {
        return;
      }
      setStore((prev) => {
        const next: ChannelSectionStore = {
          ...prev,
          sections: prev.sections.map((s) =>
            s.id === sectionId
              ? {
                  id: s.id,
                  name: newName,
                  ...(icon ? { icon } : {}),
                  order: s.order,
                }
              : s,
          ),
        };
        if (!writeChannelSectionsStore(pubkey, next, relayUrl)) {
          return prev;
        }
        managerRef.current?.publishSections(next, prev);
        return next;
      });
    },
    [pubkey, relayUrl, setStore],
  );

  const deleteSection = React.useCallback(
    (sectionId: string) => {
      if (!pubkey) {
        return;
      }
      setStore((prev) => {
        const assignments = { ...prev.assignments };
        for (const channelId of Object.keys(assignments)) {
          if (assignments[channelId] === sectionId) {
            delete assignments[channelId];
          }
        }
        const next: ChannelSectionStore = {
          ...prev,
          sections: prev.sections.filter((s) => s.id !== sectionId),
          assignments,
        };
        if (!writeChannelSectionsStore(pubkey, next, relayUrl)) {
          return prev;
        }
        managerRef.current?.publishSections(next, prev);
        return next;
      });
    },
    [pubkey, relayUrl, setStore],
  );

  const moveSectionUp = React.useCallback(
    (sectionId: string) => {
      if (!pubkey) return;
      setStore((prev) => {
        const next = swapSectionOrder(prev, sectionId, "up");
        if (!next || !writeChannelSectionsStore(pubkey, next, relayUrl))
          return prev;
        managerRef.current?.publishSections(next, prev);
        return next;
      });
    },
    [pubkey, relayUrl, setStore],
  );

  const moveSectionDown = React.useCallback(
    (sectionId: string) => {
      if (!pubkey) return;
      setStore((prev) => {
        const next = swapSectionOrder(prev, sectionId, "down");
        if (!next || !writeChannelSectionsStore(pubkey, next, relayUrl))
          return prev;
        managerRef.current?.publishSections(next, prev);
        return next;
      });
    },
    [pubkey, relayUrl, setStore],
  );

  const reorderSections = React.useCallback(
    (orderedIds: string[]) => {
      if (!pubkey) return;
      setStore((prev) => {
        const sections = prev.sections.map((s) => {
          const newOrder = orderedIds.indexOf(s.id);
          return newOrder === -1 ? s : { ...s, order: newOrder };
        });
        const next: ChannelSectionStore = { ...prev, sections };
        if (!writeChannelSectionsStore(pubkey, next, relayUrl)) return prev;
        managerRef.current?.publishSections(next, prev);
        return next;
      });
    },
    [pubkey, relayUrl, setStore],
  );

  const assignChannel = React.useCallback(
    (channelId: string, sectionId: string) => {
      if (!pubkey) {
        return;
      }
      setStore((prev) => {
        const assignments = { ...prev.assignments };
        delete assignments[channelId];
        assignments[channelId] = sectionId;
        const next = boundChannelSectionsStore({
          ...prev,
          assignments,
        });
        if (!writeChannelSectionsStore(pubkey, next, relayUrl)) {
          return prev;
        }
        managerRef.current?.publishSections(next, prev);
        return next;
      });
    },
    [pubkey, relayUrl, setStore],
  );

  const unassignChannel = React.useCallback(
    (channelId: string) => {
      if (!pubkey) {
        return;
      }
      setStore((prev) => {
        const assignments = { ...prev.assignments };
        delete assignments[channelId];
        const next: ChannelSectionStore = { ...prev, assignments };
        if (!writeChannelSectionsStore(pubkey, next, relayUrl)) {
          return prev;
        }
        managerRef.current?.publishSections(next, prev);
        return next;
      });
    },
    [pubkey, relayUrl, setStore],
  );

  return {
    sections,
    assignments: store.assignments,
    createSection,
    renameSection,
    deleteSection,
    moveSectionUp,
    moveSectionDown,
    reorderSections,
    assignChannel,
    unassignChannel,
  };
}
