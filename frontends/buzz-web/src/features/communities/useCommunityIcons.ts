import { useQueries, useQuery } from "@tanstack/react-query";

import { fetchCommunityIcon } from "@/shared/api/communityProfile";

import type { Community } from "./types";
import {
  loadCachedCommunityIcon,
  safeCommunityIconUrl,
  saveCachedCommunityIcon,
} from "./communityIconCache";

export const communityIconQueryKey = (relayUrl: string) =>
  ["communityIcon", relayUrl] as const;

const ICON_STALE_MS = 5 * 60_000;

/**
 * Fetch a relay's icon and keep only a safe image data URL (see
 * `safeCommunityIconUrl`); a network URL from the relay renders the
 * placeholder instead of reaching an `<img src>`.
 */
export async function fetchSafeCommunityIcon(
  relayUrl: string,
  fetchIcon: (relayUrl: string) => Promise<string | null> = fetchCommunityIcon,
): Promise<string | null> {
  const icon = safeCommunityIconUrl(await fetchIcon(relayUrl));
  if (relayUrl) saveCachedCommunityIcon(relayUrl, icon);
  return icon;
}

async function fetchIconForCommunity(
  community: Community,
): Promise<string | null> {
  return fetchSafeCommunityIcon(community.relayUrl);
}

function iconQueryOptions(community: Community) {
  return {
    queryKey: communityIconQueryKey(community.relayUrl),
    queryFn: () => fetchIconForCommunity(community),
    // Cached icon renders immediately; the fetch still runs and replaces it.
    placeholderData: loadCachedCommunityIcon(community.relayUrl),
    staleTime: ICON_STALE_MS,
    retry: 1,
  };
}

/**
 * Community icons for the rail, keyed by community id. Each icon is read
 * from its relay's NIP-11 document over plain HTTP — active and inactive
 * communities alike. Falls back to the localStorage cache (then null →
 * initials) when a relay is unreachable.
 */
export function useCommunityIcons(
  communities: Community[],
): Record<string, string | null> {
  const results = useQueries({
    queries: communities.map((community) => iconQueryOptions(community)),
  });

  const icons: Record<string, string | null> = {};
  communities.forEach((community, index) => {
    icons[community.id] =
      results[index]?.data ?? loadCachedCommunityIcon(community.relayUrl);
  });
  return icons;
}

/** Icon of the ACTIVE community, for settings preview. */
export function useActiveCommunityIcon(relayUrl: string | undefined) {
  return useQuery({
    queryKey: communityIconQueryKey(relayUrl ?? ""),
    queryFn: () => fetchSafeCommunityIcon(relayUrl ?? ""),
    enabled: relayUrl !== undefined,
    staleTime: ICON_STALE_MS,
  });
}
