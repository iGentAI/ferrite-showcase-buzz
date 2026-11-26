import { normalizeRelayUrl } from "@/shared/lib/normalizeRelayUrl";

const STORAGE_KEY_PREFIX = "buzz-channel-stars.v1";
export const MAX_CHANNEL_STAR_ENTRIES = 500;

export type ChannelStarEntry = {
  starred: boolean;
  updatedAt: number;
};

export type ChannelStarStore = {
  version: 1;
  channels: Record<string, ChannelStarEntry>;
};

export const DEFAULT_STORE: ChannelStarStore = Object.freeze({
  version: 1,
  channels: {},
});

/**
 * Stars are per account AND per community (relay), like channel sections and
 * the sort preference: channel ids such as `general` and DM ids repeat across
 * communities, and one shared store leaked stars between them and published
 * one community's stars onto another relay. Without `relayUrl` this is the
 * legacy pubkey-only key, read once for migration.
 */
export function storageKey(pubkey: string, relayUrl?: string): string {
  if (!relayUrl) return `${STORAGE_KEY_PREFIX}:${pubkey}`;
  return `${STORAGE_KEY_PREFIX}:${pubkey}:${encodeURIComponent(normalizeRelayUrl(relayUrl))}`;
}

export function parseStarPayload(json: unknown): ChannelStarStore | null {
  if (typeof json !== "object" || json === null) return null;
  const obj = json as Record<string, unknown>;
  if (obj.version !== 1) return null;
  const channels: Record<string, ChannelStarEntry> =
    typeof obj.channels === "object" &&
    obj.channels !== null &&
    !Array.isArray(obj.channels)
      ? Object.fromEntries(
          Object.entries(obj.channels as Record<string, unknown>).filter(
            (entry): entry is [string, ChannelStarEntry] => {
              const v = entry[1];
              return (
                typeof v === "object" &&
                v !== null &&
                typeof (v as Record<string, unknown>).starred === "boolean" &&
                typeof (v as Record<string, unknown>).updatedAt === "number" &&
                Number.isFinite(
                  (v as Record<string, unknown>).updatedAt as number,
                ) &&
                ((v as Record<string, unknown>).updatedAt as number) >= 0
              );
            },
          ),
        )
      : {};
  return boundStarStore({ version: 1, channels });
}

function parseStoredStars(raw: string | null): ChannelStarStore | null {
  if (!raw) return null;
  const parsed = JSON.parse(raw);
  if (typeof parsed !== "object" || parsed === null || parsed.version !== 1) {
    return null;
  }
  return parseStarPayload(parsed);
}

export function readChannelStarsStore(
  pubkey: string,
  relayUrl?: string,
): ChannelStarStore {
  try {
    const key = storageKey(pubkey, relayUrl);
    const raw = window.localStorage.getItem(key);
    if (raw !== null) return parseStoredStars(raw) ?? DEFAULT_STORE;
    // One-time migration of the legacy pubkey-only store into the first
    // community that reads it; removing it keeps it from seeding any other.
    if (relayUrl) {
      const legacyKey = storageKey(pubkey);
      const legacy = parseStoredStars(window.localStorage.getItem(legacyKey));
      if (legacy && Object.keys(legacy.channels).length > 0) {
        try {
          window.localStorage.setItem(key, JSON.stringify(legacy));
          window.localStorage.removeItem(legacyKey);
        } catch {
          // The migrated value is still returned.
        }
        return legacy;
      }
    }
    return DEFAULT_STORE;
  } catch {
    return DEFAULT_STORE;
  }
}

export function boundStarStore(
  store: ChannelStarStore,
  preservedKey?: string,
): ChannelStarStore {
  const preservedEntry =
    preservedKey === undefined ? undefined : store.channels[preservedKey];
  const entries = Object.entries(store.channels).filter(
    ([channelId]) => channelId !== preservedKey,
  );
  if (entries.length + (preservedEntry ? 1 : 0) <= MAX_CHANNEL_STAR_ENTRIES)
    return store;
  entries.sort(([leftId, left], [rightId, right]) => {
    if (left.updatedAt !== right.updatedAt)
      return left.updatedAt - right.updatedAt;
    return leftId < rightId ? -1 : leftId > rightId ? 1 : 0;
  });
  const retainedEntries = entries.slice(
    -(MAX_CHANNEL_STAR_ENTRIES - (preservedEntry ? 1 : 0)),
  );
  if (preservedEntry && preservedKey !== undefined) {
    retainedEntries.push([preservedKey, preservedEntry]);
  }
  return {
    ...store,
    channels: Object.fromEntries(retainedEntries),
  };
}

export function writeChannelStarsStore(
  pubkey: string,
  store: ChannelStarStore,
  relayUrl?: string,
): boolean {
  try {
    window.localStorage.setItem(
      storageKey(pubkey, relayUrl),
      JSON.stringify(boundStarStore(store)),
    );
    return true;
  } catch {
    return false;
  }
}

export function mergeStores(
  local: ChannelStarStore,
  remote: ChannelStarStore,
): ChannelStarStore {
  const allIds = new Set([
    ...Object.keys(local.channels),
    ...Object.keys(remote.channels),
  ]);
  const merged: Record<string, ChannelStarEntry> = {};
  for (const id of allIds) {
    const l = local.channels[id];
    const r = remote.channels[id];
    if (l && r) {
      merged[id] = l.updatedAt >= r.updatedAt ? l : r;
    } else {
      merged[id] = (l ?? r) as ChannelStarEntry;
    }
  }
  return boundStarStore({ version: 1, channels: merged });
}

export function starredChannelIdsFromStore(
  store: ChannelStarStore,
): Set<string> {
  return new Set(
    Object.entries(store.channels)
      .filter(([, entry]) => entry.starred)
      .map(([id]) => id),
  );
}
