import type { Community } from "./types";
import { homeDir } from "@tauri-apps/api/path";
import { setLocalStorageItemWithRecovery } from "@/shared/lib/localStorageQuota";
import { getStorageItem, removeStorageItem } from "@/shared/lib/safeStorage";

const COMMUNITIES_KEY = "buzz-communities";
const ACTIVE_COMMUNITY_KEY = "buzz-active-community-id";
const LEGACY_WORKSPACES_KEY = "buzz-workspaces";
const LEGACY_ACTIVE_WORKSPACE_KEY = "buzz-active-workspace-id";
const COMMUNITY_DISCOVERY_AFTER_LEAVE_KEY =
  "buzz-community-discovery-after-leave";

/**
 * Expand a leading `~` to the user's home directory. The backend rejects
 * `~`-prefixed paths (`std::fs` does not expand the shell tilde), so the UI
 * resolves it before save. Returns non-`~` input unchanged. Empty/whitespace
 * input returns `undefined` so callers can clear the override.
 */
export async function expandTilde(input: string): Promise<string | undefined> {
  const trimmed = input.trim();
  if (!trimmed) {
    return undefined;
  }
  if (trimmed === "~") {
    return homeDir();
  }
  if (trimmed.startsWith("~/")) {
    const home = await homeDir();
    const base = home.endsWith("/") ? home.slice(0, -1) : home;
    return `${base}/${trimmed.slice(2)}`;
  }
  return trimmed;
}

/**
 * Remove the user's `nsec` from a serialized community list. Older builds
 * stored the private key inside each community entry; the on-disk
 * `identity.key` is the only home of the key now, so no copy of a stored
 * list may carry it forward. Returns the cleaned JSON, or `null` when the
 * value is not a JSON array (such a value is not a usable community list and
 * is never copied).
 */
export function stripCommunityKeyMaterial(raw: string): string | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!Array.isArray(parsed)) {
    return null;
  }
  let didStrip = false;
  const cleaned = parsed.map((entry: unknown) => {
    if (entry && typeof entry === "object" && "nsec" in entry) {
      const { nsec: _nsec, ...rest } = entry as Record<string, unknown>;
      didStrip = true;
      return rest;
    }
    return entry;
  });
  return didStrip ? JSON.stringify(cleaned) : raw;
}

export function migrateLegacyCommunityStorage(given?: Storage): void {
  // Reading the localStorage getter can itself throw SecurityError when the
  // origin is denied storage, so it is resolved inside a try.
  let storage: Storage;
  try {
    storage = given ?? window.localStorage;
  } catch (error) {
    console.warn(
      "[communityStorage] migrateLegacyCommunityStorage: storage unavailable:",
      error,
    );
    return;
  }
  // Each step is fenced on its own: WebKit throws SecurityError when storage
  // access is denied for the origin (block/buzz#5078), and a quota failure
  // on the copy must not skip the key-material cleanup below.
  const step = (label: string, run: () => void) => {
    try {
      run();
    } catch (error) {
      console.warn(
        `[communityStorage] migrateLegacyCommunityStorage: ${label} failed (storage denied or full?):`,
        error,
      );
    }
  };

  step("copying the legacy community list", () => {
    if (storage.getItem(COMMUNITIES_KEY) !== null) return;
    const legacyCommunities = storage.getItem(LEGACY_WORKSPACES_KEY);
    const cleaned =
      legacyCommunities === null
        ? null
        : stripCommunityKeyMaterial(legacyCommunities);
    if (cleaned !== null) {
      storage.setItem(COMMUNITIES_KEY, cleaned);
    }
  });

  step("copying the legacy active community", () => {
    if (storage.getItem(ACTIVE_COMMUNITY_KEY) !== null) return;
    const legacyActiveCommunity = storage.getItem(LEGACY_ACTIVE_WORKSPACE_KEY);
    if (legacyActiveCommunity !== null) {
      storage.setItem(ACTIVE_COMMUNITY_KEY, legacyActiveCommunity);
    }
  });

  // The legacy list is read only while the current key is absent. Once the
  // current key exists (just copied above, or written by this build) the
  // legacy entry is dead data — and in nsec-era installs it holds the
  // plaintext private key — so it is removed on every boot. If the copy
  // failed, the legacy value is kept for the next attempt only without the
  // key: it is rewritten sanitized (a smaller value), and removed outright if
  // even that write fails or it is not a usable list.
  step("removing legacy key material", () => {
    const legacyCommunities = storage.getItem(LEGACY_WORKSPACES_KEY);
    if (legacyCommunities === null) return;
    if (storage.getItem(COMMUNITIES_KEY) !== null) {
      storage.removeItem(LEGACY_WORKSPACES_KEY);
      return;
    }
    const cleaned = stripCommunityKeyMaterial(legacyCommunities);
    if (cleaned === legacyCommunities) return;
    if (cleaned !== null) {
      try {
        storage.setItem(LEGACY_WORKSPACES_KEY, cleaned);
        return;
      } catch {
        // Fall through: the private key must not stay stored.
      }
    }
    storage.removeItem(LEGACY_WORKSPACES_KEY);
  });

  step("removing the legacy active community", () => {
    if (storage.getItem(ACTIVE_COMMUNITY_KEY) !== null) {
      storage.removeItem(LEGACY_ACTIVE_WORKSPACE_KEY);
    }
  });
}

export function loadCommunities(): Community[] {
  try {
    migrateLegacyCommunityStorage();
    const raw = getStorageItem(COMMUNITIES_KEY);
    if (!raw) {
      return [];
    }
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) {
      return [];
    }
    if (parsed.length > 0) {
      removeStorageItem(COMMUNITY_DISCOVERY_AFTER_LEAVE_KEY);
    }
    // Migration: older builds stored the user's `nsec` in localStorage and
    // re-applied it to the backend on every reload, which silently overwrote
    // any `import_identity` result with the original generated key. The
    // on-disk `identity.key` file is the only source of truth now. Strip
    // any lingering `nsec` from existing entries on read and persist the
    // cleaned list back so it cannot leak into future sessions.
    let didStrip = false;
    const cleaned = (parsed as Array<Record<string, unknown>>).map((entry) => {
      if (entry && typeof entry === "object" && "nsec" in entry) {
        const { nsec: _nsec, ...rest } = entry;
        didStrip = true;
        return rest;
      }
      return entry;
    }) as Community[];
    if (didStrip) {
      const serialized = JSON.stringify(cleaned);
      if (!setLocalStorageItemWithRecovery(COMMUNITIES_KEY, serialized)) {
        // The key-bearing value must not survive a failed rewrite. Removing
        // it frees its space for one more attempt; if that also fails the
        // list lives on in memory (and is persisted by the next save) while
        // the stored copy with the key is already gone.
        removeStorageItem(COMMUNITIES_KEY);
        setLocalStorageItemWithRecovery(COMMUNITIES_KEY, serialized);
      }
    }
    return cleaned;
  } catch {
    return [];
  }
}

export function saveCommunities(communities: Community[]): boolean {
  const didSave = setLocalStorageItemWithRecovery(
    COMMUNITIES_KEY,
    JSON.stringify(communities),
  );
  if (didSave && communities.length > 0) {
    removeStorageItem(COMMUNITY_DISCOVERY_AFTER_LEAVE_KEY);
  }
  return didSave;
}

export function loadCommunityDiscoveryAfterLeave(storage?: Storage): boolean {
  try {
    return (
      (storage ?? window.localStorage).getItem(
        COMMUNITY_DISCOVERY_AFTER_LEAVE_KEY,
      ) === "1"
    );
  } catch (error) {
    // block/buzz#5078 — storage access can be denied for the origin; degrade
    // to the default ("didn't just leave") instead of crashing the boot path.
    console.warn(
      "[communityStorage] loadCommunityDiscoveryAfterLeave failed:",
      error,
    );
    return false;
  }
}

export function markCommunityDiscoveryAfterLeave(storage?: Storage): boolean {
  if (!storage) {
    return setLocalStorageItemWithRecovery(
      COMMUNITY_DISCOVERY_AFTER_LEAVE_KEY,
      "1",
    );
  }
  try {
    storage.setItem(COMMUNITY_DISCOVERY_AFTER_LEAVE_KEY, "1");
    return true;
  } catch {
    return false;
  }
}

export function clearCommunityStorage(storage?: Storage): void {
  // Runs inside React state updaters (last-community removal) and on
  // sign-out, so it must never throw: block/buzz#5078 — WebKit throws
  // SecurityError from removeItem when storage access is denied. Each key is
  // removed independently so one failure cannot strand the others.
  for (const key of [
    COMMUNITIES_KEY,
    ACTIVE_COMMUNITY_KEY,
    LEGACY_WORKSPACES_KEY,
    LEGACY_ACTIVE_WORKSPACE_KEY,
  ]) {
    try {
      (storage ?? localStorage).removeItem(key);
    } catch (error) {
      console.warn(
        "[communityStorage] clearCommunityStorage could not remove",
        key,
        error,
      );
    }
  }
}

export function loadActiveCommunityId(): string | null {
  try {
    migrateLegacyCommunityStorage();
  } catch (error) {
    console.warn("[communityStorage] legacy migration failed:", error);
  }
  // block/buzz#5078 — WebKit can throw SecurityError from a denied-storage
  // getItem. Fail closed so the boot path renders the default community UI
  // instead of unmounting the root.
  return getStorageItem(ACTIVE_COMMUNITY_KEY);
}

export function saveActiveCommunityId(id: string): boolean {
  return setLocalStorageItemWithRecovery(ACTIVE_COMMUNITY_KEY, id);
}

export function normalizeRelayUrl(url: string): string {
  if (!url.startsWith("ws://") && !url.startsWith("wss://")) {
    return `wss://${url}`;
  }
  return url;
}

function isLocalRelayHost(hostname: string): boolean {
  return ["localhost", "127.0.0.1", "[::1]", "0.0.0.0"].includes(hostname);
}

export function shouldAutoConnectDefaultRelay(relayUrl: string): boolean {
  try {
    const parsed = new URL(relayUrl);
    return (
      (parsed.protocol === "ws:" || parsed.protocol === "wss:") &&
      !isLocalRelayHost(parsed.hostname)
    );
  } catch {
    return false;
  }
}

export function deriveCommunityName(relayUrl: string): string {
  try {
    const url = new URL(
      relayUrl.replace("ws://", "http://").replace("wss://", "https://"),
    );
    const host = url.hostname;
    if (isLocalRelayHost(host)) {
      return "Local Dev";
    }
    const parts = host.split(".");
    // Detect staging environments (e.g. buzz-oss.stage.blox.sqprod.co)
    if (parts.some((p) => p === "stage" || p === "staging")) {
      return "Buzz (staging)";
    }
    // Use the first subdomain segment or the domain itself
    if (parts.length >= 2) {
      return parts[0] === "relay" ? parts[1] : parts[0];
    }
    return host;
  } catch {
    return "Community";
  }
}

export function initFirstCommunity(
  relayUrl: string,
  pubkey: string,
  name?: string,
): Community | null {
  const normalizedUrl = normalizeRelayUrl(relayUrl);
  const trimmedName = name?.trim();
  const community: Community = {
    id: crypto.randomUUID(),
    name: trimmedName || deriveCommunityName(normalizedUrl),
    relayUrl: normalizedUrl,
    // Compiled default relays must admit the first token-less connection; there
    // is no invite-token prompt on this auto-connect path.
    pubkey,
    addedAt: new Date().toISOString(),
  };
  // block/buzz#5078 — read the prior active id through the throw-safe helper;
  // a denied-storage origin would otherwise kill onboarding before a single
  // write is attempted.
  const previousActiveCommunityId = getStorageItem(ACTIVE_COMMUNITY_KEY);
  const didSaveActiveCommunity = saveActiveCommunityId(community.id);
  if (!didSaveActiveCommunity) {
    return null;
  }

  if (!saveCommunities([community])) {
    // A failed setItem leaves the existing communities value untouched. Roll
    // back only the active-ID write so inconsistent pre-existing data is never
    // destroyed while recovering from a quota failure.
    try {
      if (previousActiveCommunityId === null) {
        localStorage.removeItem(ACTIVE_COMMUNITY_KEY);
      } else {
        localStorage.setItem(ACTIVE_COMMUNITY_KEY, previousActiveCommunityId);
      }
    } catch {
      // Best effort: persistence is already unavailable, and callers will stay
      // on setup instead of reloading.
    }
    return null;
  }

  return community;
}
