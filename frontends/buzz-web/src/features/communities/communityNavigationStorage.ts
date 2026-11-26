import { setLocalStorageItemWithRecovery } from "@/shared/lib/localStorageQuota";

const COMMUNITY_DESTINATIONS_KEY = "buzz-community-destinations";
let pendingCommunityRestoreId: string | null = null;

export type CommunityDestination =
  | { kind: "home" }
  | { kind: "channel"; channelId: string };

type CommunityDestinations = Record<string, CommunityDestination>;

function isCommunityDestination(value: unknown): value is CommunityDestination {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Record<string, unknown>;
  return (
    candidate.kind === "home" ||
    (candidate.kind === "channel" &&
      typeof candidate.channelId === "string" &&
      candidate.channelId.length > 0)
  );
}

/**
 * The storage to use, or null when it cannot be reached. Reading the
 * `localStorage` getter itself throws SecurityError when the origin is denied
 * storage (block/buzz#5078), so it is resolved here, inside a try, rather than
 * in a default parameter.
 */
function resolveStorage(storage?: Storage): Storage | null {
  if (storage) return storage;
  try {
    return typeof window === "undefined" ? null : window.localStorage;
  } catch {
    return null;
  }
}

function loadCommunityDestinations(
  storage: Storage | null,
): CommunityDestinations {
  if (!storage) return {};
  try {
    const raw = storage.getItem(COMMUNITY_DESTINATIONS_KEY);
    if (!raw) {
      return {};
    }

    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      return {};
    }

    return Object.fromEntries(
      Object.entries(parsed).filter(
        (entry): entry is [string, CommunityDestination] =>
          isCommunityDestination(entry[1]),
      ),
    );
  } catch {
    return {};
  }
}

function saveCommunityDestinations(
  destinations: CommunityDestinations,
  storage: Storage | null,
): void {
  if (!storage) return;
  const serialized = JSON.stringify(destinations);
  try {
    if (typeof window !== "undefined" && storage === window.localStorage) {
      setLocalStorageItemWithRecovery(COMMUNITY_DESTINATIONS_KEY, serialized);
      return;
    }
    storage.setItem(COMMUNITY_DESTINATIONS_KEY, serialized);
  } catch (error) {
    console.warn("[communityNavigationStorage] save failed:", error);
  }
}

export function loadCommunityDestination(
  communityId: string,
  storage?: Storage,
): CommunityDestination | null {
  const destinations = loadCommunityDestinations(resolveStorage(storage));
  return Object.hasOwn(destinations, communityId)
    ? destinations[communityId]
    : null;
}

export function saveCommunityDestination(
  communityId: string,
  destination: CommunityDestination,
  storage?: Storage,
): void {
  const resolved = resolveStorage(storage);
  saveCommunityDestinations(
    { ...loadCommunityDestinations(resolved), [communityId]: destination },
    resolved,
  );
}

export function removeCommunityDestination(
  communityId: string,
  storage?: Storage,
): void {
  if (pendingCommunityRestoreId === communityId) {
    pendingCommunityRestoreId = null;
  }
  const resolved = resolveStorage(storage);
  const destinations = loadCommunityDestinations(resolved);
  if (!Object.hasOwn(destinations, communityId)) {
    return;
  }
  delete destinations[communityId];
  saveCommunityDestinations(destinations, resolved);
}

export function clearCommunityDestinations(storage?: Storage): void {
  // Teardown (sign-out, last community removed) must finish even when
  // storage is denied, so the in-memory restore id is cleared first and the
  // removal cannot throw.
  pendingCommunityRestoreId = null;
  try {
    resolveStorage(storage)?.removeItem(COMMUNITY_DESTINATIONS_KEY);
  } catch (error) {
    console.warn("[communityNavigationStorage] clear failed:", error);
  }
}

export function markPendingCommunityRestore(communityId: string): void {
  pendingCommunityRestoreId = communityId;
}

export function consumePendingCommunityRestore(communityId: string): boolean {
  if (pendingCommunityRestoreId !== communityId) {
    return false;
  }
  pendingCommunityRestoreId = null;
  return true;
}
