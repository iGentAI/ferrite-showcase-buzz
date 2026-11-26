import {
  createContext,
  useCallback,
  useContext,
  useMemo,
  useRef,
  useState,
} from "react";
import type { ReactNode } from "react";

import type { Community } from "./types";
import {
  clearCommunityStorage,
  loadActiveCommunityId,
  loadCommunities,
  saveActiveCommunityId,
  saveCommunities,
} from "./communityStorage";
import { removeSelfProfileCachesForRelay } from "@/features/profile/lib/selfProfileStorage";
import { removeUserLabelCacheForRelay } from "@/features/profile/lib/userLabelStorage";
import { removeChannelSnapshotForRelay } from "@/features/channels/channelSnapshot";
import { removeProjectSnapshotForRelay } from "@/features/projects/projectSnapshot";
import { clearChannelHeadCache } from "@/shared/api/tauriChannelHeadCache";
import { getIdentity } from "@/shared/api/tauriIdentity";
import { clearSavedCommunitySnapshot } from "@/features/agents/activeAgentTurnsStore";
import { canonicalRelayUrl } from "@/features/agents/managedAgentRuntimeStatus";
import {
  clearCommunityDestinations,
  removeCommunityDestination,
} from "./communityNavigationStorage";

export type UpdateCommunityResult =
  | { kind: "updated"; requiresReinit: boolean }
  | { kind: "unchanged" }
  | { kind: "duplicate-relay" }
  | { kind: "not-found" };

/**
 * Pure decision logic for updateCommunity — determines the outcome from a
 * synchronous snapshot of communities without side effects.  Extracted so the
 * 5-case result matrix is unit-testable outside React.
 */
/**
 * Whether two relay URLs name the same relay. Case, a default port or a
 * trailing slash must not make one relay two communities, each with its own
 * view of relay-scoped local state (finding_254274ca5f20b568).
 */
export function sameRelayUrl(a: string, b: string): boolean {
  return (canonicalRelayUrl(a) ?? a.trim()) === (canonicalRelayUrl(b) ?? b.trim());
}

export function resolveCommunityUpdateResult(
  communities: Community[],
  activeId: string | null,
  id: string,
  updates: Partial<
    Pick<Community, "name" | "relayUrl" | "token" | "pubkey" | "reposDir">
  >,
): UpdateCommunityResult {
  const current = communities.find((w) => w.id === id);
  if (!current) return { kind: "not-found" };

  if (
    updates.relayUrl !== undefined &&
    updates.relayUrl !== current.relayUrl &&
    communities.some(
      (w) => w.id !== id && sameRelayUrl(w.relayUrl, updates.relayUrl ?? ""),
    )
  ) {
    return { kind: "duplicate-relay" };
  }

  const hasChange =
    (updates.name !== undefined && updates.name !== current.name) ||
    (updates.relayUrl !== undefined && updates.relayUrl !== current.relayUrl) ||
    clearableFieldChanged(updates, current, "token") ||
    (updates.pubkey !== undefined && updates.pubkey !== current.pubkey) ||
    clearableFieldChanged(updates, current, "reposDir");

  if (!hasChange) return { kind: "unchanged" };

  const isActive = id === activeId;
  const backendFieldsChanged =
    isActive &&
    ((updates.relayUrl !== undefined &&
      updates.relayUrl !== current.relayUrl) ||
      clearableFieldChanged(updates, current, "token") ||
      clearableFieldChanged(updates, current, "reposDir"));

  return { kind: "updated", requiresReinit: backendFieldsChanged };
}

/**
 * `token` and `reposDir` are optional overrides the edit dialog can clear. A
 * cleared field arrives as the key PRESENT with an `undefined` value, which
 * is a real change when the community currently has a value — otherwise a
 * relay API token the user removed would stay stored and keep being applied
 * on every boot. An absent key still means "leave unchanged".
 */
function clearableFieldChanged(
  updates: Partial<Pick<Community, "token" | "reposDir">>,
  current: Community,
  key: "token" | "reposDir",
): boolean {
  return Object.hasOwn(updates, key) && updates[key] !== current[key];
}

export type CommunityLocalStatePurge = {
  removeSelfProfileCachesForRelay: (relayUrl: string) => void;
  removeUserLabelCacheForRelay: (relayUrl: string) => void;
  removeChannelSnapshotForRelay: (relayUrl: string) => void;
  removeProjectSnapshotForRelay: (relayUrl: string) => void;
  clearPersistedChannelHeads: (relayUrl: string) => void;
  clearSavedCommunitySnapshot: (communityId: string) => void;
  removeCommunityDestination: (communityId: string) => void;
};

const DEFAULT_COMMUNITY_LOCAL_STATE_PURGE: CommunityLocalStatePurge = {
  removeSelfProfileCachesForRelay,
  removeUserLabelCacheForRelay,
  removeChannelSnapshotForRelay,
  removeProjectSnapshotForRelay,
  clearPersistedChannelHeads: (relayUrl) => {
    void getIdentity()
      .then((identity) =>
        clearChannelHeadCache({ pubkey: identity.pubkey, relayUrl }),
      )
      .catch((error) => {
        console.warn("Failed to clear persisted channel heads", error);
      });
  },
  clearSavedCommunitySnapshot,
  removeCommunityDestination,
};

/**
 * Drop every relay- or community-scoped local cache for a community that is
 * leaving this device: cached profiles and labels, channel and project
 * snapshots, persisted channel heads, the saved agent-turn snapshot and the
 * last navigation destination. Shared by single removal and clear-all, so
 * backing out of a community never leaves its content behind to be
 * rehydrated if the same relay is added again.
 */
export function purgeCommunityLocalState(
  community: Pick<Community, "id" | "relayUrl">,
  purge: CommunityLocalStatePurge = DEFAULT_COMMUNITY_LOCAL_STATE_PURGE,
): void {
  purge.removeSelfProfileCachesForRelay(community.relayUrl);
  purge.removeUserLabelCacheForRelay(community.relayUrl);
  purge.removeChannelSnapshotForRelay(community.relayUrl);
  purge.removeProjectSnapshotForRelay(community.relayUrl);
  purge.clearPersistedChannelHeads(community.relayUrl);
  purge.clearSavedCommunitySnapshot(community.id);
  purge.removeCommunityDestination(community.id);
}

/**
 * Local cleanup for `clearCommunities()`: purge each community's caches (as
 * `removeCommunity` does), then the community list and destinations.
 */
export function clearAllCommunityLocalState(
  communities: ReadonlyArray<Pick<Community, "id" | "relayUrl">>,
  purge: CommunityLocalStatePurge = DEFAULT_COMMUNITY_LOCAL_STATE_PURGE,
  clearStorage: () => void = () => {
    clearCommunityStorage();
    clearCommunityDestinations();
  },
): void {
  for (const community of communities) {
    purgeCommunityLocalState(community, purge);
  }
  clearStorage();
}

/**
 * Permute `communities` so that its order matches `orderedIds`.
 *
 * - Communities whose id appears in `orderedIds` are placed first, in the
 *   order given by `orderedIds`.
 * - Communities not mentioned in `orderedIds` (e.g. added after the drag
 *   completed) are appended at the end in their original relative order.
 *
 * Pure and side-effect-free — extracted so it can be unit-tested without
 * a DOM or React.
 */
export function applyCommunitiesOrder(
  communities: Community[],
  orderedIds: string[],
): Community[] {
  const byId = new Map(communities.map((c) => [c.id, c]));
  const seen = new Set<string>();
  const reordered: Community[] = [];

  for (const id of orderedIds) {
    const c = byId.get(id);
    if (c && !seen.has(id)) {
      reordered.push(c);
      seen.add(id);
    }
  }

  for (const c of communities) {
    if (!seen.has(c.id)) {
      reordered.push(c);
    }
  }

  return reordered;
}

export type CommunityRemovalResult = {
  communities: Community[];
  activeId: string | null;
};

export function resolveCommunityRemoval(
  communities: Community[],
  activeId: string | null,
  id: string,
): CommunityRemovalResult {
  const next = communities.filter((community) => community.id !== id);
  return {
    communities: next,
    activeId: activeId === id ? (next[0]?.id ?? null) : activeId,
  };
}

export type UseCommunitiesReturn = {
  communities: Community[];
  activeCommunity: Community | null;
  /** Counter bumped when the active community's config changes (relayUrl/token). */
  reinitKey: number;
  /** Add a community, deduplicating by relayUrl. Returns the final ID in the list. */
  addCommunity: (community: Community) => string;
  clearCommunities: () => void;
  removeCommunity: (id: string) => void;
  switchCommunity: (id: string) => void;
  /** Force the active community to re-init (e.g. after a deep-link reconnect). */
  reconnectCommunity: () => void;
  updateCommunity: (
    id: string,
    updates: Partial<
      Pick<Community, "name" | "relayUrl" | "token" | "pubkey" | "reposDir">
    >,
  ) => UpdateCommunityResult;
  /** Persist a new display order for the rail. IDs not in orderedIds keep their relative position at the end. */
  reorderCommunities: (orderedIds: string[]) => void;
};

const CommunitiesContext = createContext<UseCommunitiesReturn | null>(null);

export function CommunitiesProvider({ children }: { children: ReactNode }) {
  const value = useCommunitiesInternal();
  return (
    <CommunitiesContext.Provider value={value}>
      {children}
    </CommunitiesContext.Provider>
  );
}

export function useCommunities(): UseCommunitiesReturn {
  const ctx = useContext(CommunitiesContext);
  if (!ctx) {
    throw new Error("useCommunities must be used within a CommunitiesProvider");
  }
  return ctx;
}

function useCommunitiesInternal(): UseCommunitiesReturn {
  const [communities, setCommunitiesState] =
    useState<Community[]>(loadCommunities);
  const [activeId, setActiveId] = useState<string | null>(
    loadActiveCommunityId,
  );
  const [reinitKey, setReinitKey] = useState(0);
  const communitiesRef = useRef(communities);
  communitiesRef.current = communities;

  const activeCommunity = useMemo(
    () => communities.find((w) => w.id === activeId) ?? communities[0] ?? null,
    [communities, activeId],
  );
  // The community actually shown. A null or dangling stored id falls back to
  // the first community, and every mutation must agree with that fallback or
  // it skips the reinit and remount the shown community needs
  // (finding_9b45c875a4c8e30a).
  const effectiveActiveId = activeCommunity?.id ?? null;

  const addCommunity = useCallback((community: Community): string => {
    const existing = communitiesRef.current.find((w) =>
      sameRelayUrl(w.relayUrl, community.relayUrl),
    );
    const resolvedId = existing?.id ?? community.id;
    setCommunitiesState((prev) => {
      const dup = prev.find((w) => sameRelayUrl(w.relayUrl, community.relayUrl));
      let next: Community[];
      if (dup) {
        next = prev.map((w) =>
          w.id === dup.id
            ? {
                ...w,
                name: community.name || w.name,
                token: community.token ?? w.token,
                pubkey: community.pubkey ?? w.pubkey,
              }
            : w,
        );
      } else {
        next = [...prev, community];
      }
      saveCommunities(next);
      return next;
    });
    return resolvedId;
  }, []);

  const clearCommunities = useCallback(() => {
    // Side effects stay outside the updater (StrictMode may run it twice).
    clearAllCommunityLocalState(communitiesRef.current);
    setCommunitiesState([]);
    setActiveId(null);
  }, []);

  const removeCommunity = useCallback(
    (id: string) => {
      const removed = communitiesRef.current.find(
        (community) => community.id === id,
      );
      if (!removed) return;

      // Relay membership is revoked by the caller before this local cleanup.
      // Keep side effects outside the updater — updaters can execute twice under
      // React StrictMode.
      purgeCommunityLocalState(removed);

      setCommunitiesState((prev) => {
        const result = resolveCommunityRemoval(prev, effectiveActiveId, id);
        if (result.communities.length === 0) {
          clearCommunityStorage();
          setActiveId(null);
        } else {
          saveCommunities(result.communities);

          if (result.activeId !== effectiveActiveId && result.activeId) {
            saveActiveCommunityId(result.activeId);
            setActiveId(result.activeId);
          }
        }

        return result.communities;
      });
    },
    [effectiveActiveId],
  );

  const switchCommunity = useCallback(
    (id: string) => {
      if (id === effectiveActiveId) return;
      if (!communitiesRef.current.some((community) => community.id === id)) {
        return;
      }
      saveActiveCommunityId(id);
      setActiveId(id);
    },
    [effectiveActiveId],
  );

  const reconnectCommunity = useCallback(() => {
    setReinitKey((k) => k + 1);
  }, []);

  const updateCommunity = useCallback(
    (
      id: string,
      updates: Partial<
        Pick<Community, "name" | "relayUrl" | "token" | "pubkey" | "reposDir">
      >,
    ): UpdateCommunityResult => {
      const result = resolveCommunityUpdateResult(
        communitiesRef.current,
        effectiveActiveId,
        id,
        updates,
      );

      if (result.kind === "updated") {
        // A community moved to another relay must not keep the old relay's
        // caches, snapshots or saved turns (finding_254274ca5f20b568).
        const before = communitiesRef.current.find((w) => w.id === id);
        if (
          before &&
          updates.relayUrl !== undefined &&
          !sameRelayUrl(before.relayUrl, updates.relayUrl)
        ) {
          purgeCommunityLocalState(before);
        }
        setCommunitiesState((prev) => {
          const next = prev.map((w) =>
            w.id === id ? { ...w, ...updates } : w,
          );
          saveCommunities(next);
          return next;
        });

        if (result.requiresReinit) {
          setReinitKey((k) => k + 1);
        }
      }

      return result;
    },
    [effectiveActiveId],
  );

  const reorderCommunities = useCallback((orderedIds: string[]) => {
    setCommunitiesState((prev) => {
      const next = applyCommunitiesOrder(prev, orderedIds);
      saveCommunities(next);
      return next;
    });
  }, []);

  return {
    communities,
    activeCommunity,
    reinitKey,
    addCommunity,
    clearCommunities,
    removeCommunity,
    switchCommunity,
    reconnectCommunity,
    updateCommunity,
    reorderCommunities,
  };
}
