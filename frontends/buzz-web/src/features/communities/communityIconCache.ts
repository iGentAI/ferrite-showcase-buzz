/**
 * Local cache of community icons keyed by relay URL, so the rail renders
 * icons instantly on boot and for unreachable relays. Values are small
 * `data:image/*` URLs from the relay's NIP-11 `icon` field; anything else is
 * refused (see `safeCommunityIconUrl`).
 */

const ICON_CACHE_KEY = "buzz-community-icons";
export const MAX_CACHED_COMMUNITY_ICONS = 32;
// Keep aligned with MAX_WORKSPACE_ICON_DATA_URL_LEN in
// crates/buzz-relay/src/handlers/relay_admin.rs.
export const MAX_CACHED_COMMUNITY_ICON_LENGTH = 98_304;

const SAFE_COMMUNITY_ICON_URL =
  /^data:image\/(?:png|jpeg|jpg|webp|gif|avif|svg\+xml)(?:;[^,]*)?,/i;

/**
 * The community icon is relay-controlled (any community admin, or the relay
 * operator) and flows into an `<img src>` sink for every configured
 * community on boot. A network URL there would make each member's client
 * report its IP address and app-open timing to a host the relay picked, so
 * only image data URLs — what the icon editor publishes — are accepted.
 * Mirrors the `data:image/` guard on `avatarDataUrl` in selfProfileStorage.
 */
export function safeCommunityIconUrl(icon: unknown): string | null {
  if (typeof icon !== "string") return null;
  if (icon.length > MAX_CACHED_COMMUNITY_ICON_LENGTH) return null;
  return SAFE_COMMUNITY_ICON_URL.test(icon) ? icon : null;
}

export function boundCommunityIconCache(
  cache: Record<string, string>,
): Record<string, string> {
  const entries = Object.entries(cache).filter(
    ([, icon]) => safeCommunityIconUrl(icon) !== null,
  );
  return Object.fromEntries(entries.slice(-MAX_CACHED_COMMUNITY_ICONS));
}

function loadCache(): Record<string, string> {
  try {
    const raw = localStorage.getItem(ICON_CACHE_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : null;
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      return boundCommunityIconCache(parsed as Record<string, string>);
    }
  } catch {
    // Corrupt cache — fall through to empty.
  }
  return {};
}

export function loadCachedCommunityIcon(relayUrl: string): string | null {
  return loadCache()[relayUrl] ?? null;
}

export function saveCachedCommunityIcon(
  relayUrl: string,
  icon: string | null,
): void {
  const cache = loadCache();
  const safeIcon = safeCommunityIconUrl(icon);
  if (safeIcon) {
    delete cache[relayUrl];
    cache[relayUrl] = safeIcon;
  } else {
    delete cache[relayUrl];
  }
  try {
    localStorage.setItem(
      ICON_CACHE_KEY,
      JSON.stringify(boundCommunityIconCache(cache)),
    );
  } catch {
    // Quota exceeded — the icon still renders from the in-memory query.
  }
}
