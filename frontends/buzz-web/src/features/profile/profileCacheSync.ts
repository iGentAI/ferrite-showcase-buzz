import type { Query, QueryClient } from "@tanstack/react-query";

import {
  evictUsersBatchEntries,
  profileQueryKey,
} from "@/features/profile/hooks";
import {
  fetchAvatarDataUrl,
  readSelfProfileCache,
  resolveAvatarDataUrl,
  writeSelfProfileCache,
} from "@/features/profile/lib/selfProfileStorage";
import type {
  Profile,
  UserProfileSummary,
  UsersBatchResponse,
} from "@/shared/api/types";
import { getAvatarSnapshotUrl } from "@/shared/lib/animatedAvatar";
import { rewriteRelayUrl } from "@/shared/lib/mediaUrl";

function queryContainsPubkey(query: Query, pubkey: string): boolean {
  return query.queryKey.includes(pubkey);
}

export async function refreshProfileCaches(
  queryClient: QueryClient,
  profile: Profile,
  relayUrl: string,
): Promise<void> {
  const pubkey = profile.pubkey.toLowerCase();
  await queryClient.cancelQueries({
    predicate: (query) =>
      query.queryKey[0] === profileQueryKey[0] ||
      (query.queryKey[0] === "user-profile" &&
        queryContainsPubkey(query, pubkey)) ||
      (query.queryKey[0] === "users-batch" &&
        queryContainsPubkey(query, pubkey)),
  });

  queryClient.setQueryData(profileQueryKey, profile);
  queryClient.setQueryData<Profile>(["user-profile", pubkey], profile);
  evictUsersBatchEntries(queryClient, [pubkey]);
  queryClient.setQueriesData<UsersBatchResponse>(
    {
      predicate: (query) =>
        query.queryKey[0] === "users-batch" &&
        queryContainsPubkey(query, pubkey),
    },
    (current) => {
      if (!current?.profiles[pubkey]) return current;
      return {
        ...current,
        profiles: {
          ...current.profiles,
          [pubkey]: {
            ...current.profiles[pubkey],
            avatarUrl: profile.avatarUrl,
          } satisfies UserProfileSummary,
        },
      };
    },
  );
  // A users-batch query that has no row for this pubkey was either cancelled
  // mid-fetch above (cancelQueries reverts it to idle and never restarts it)
  // or resolved before the profile existed and lists it as missing. Neither
  // can be patched in place, and their long staleTime means nothing else
  // refreshes them, so refetch them here.
  const unpatched = new Set(
    queryClient
      .getQueryCache()
      .findAll({
        predicate: (query) =>
          query.queryKey[0] === "users-batch" &&
          queryContainsPubkey(query, pubkey),
      })
      .filter(
        (query) =>
          !(query.state.data as UsersBatchResponse | undefined)?.profiles?.[
            pubkey
          ],
      ),
  );
  if (unpatched.size > 0) {
    void queryClient
      .invalidateQueries({ predicate: (query) => unpatched.has(query) })
      .catch(() => {});
  }
  // Search result pages also embed profile avatars, but their arbitrary query
  // text/page shape makes a safe targeted rewrite brittle. Mark every search
  // view stale; active searches refetch immediately and inactive ones refresh
  // when next opened.
  await queryClient.invalidateQueries({ queryKey: ["user-search"] });

  const existing = readSelfProfileCache(relayUrl, profile.pubkey);
  const baseCache = {
    version: 1 as const,
    displayName: profile.displayName,
    avatarUrl: profile.avatarUrl,
    about: profile.about,
    avatarDataUrl: resolveAvatarDataUrl(profile.avatarUrl, null, existing),
    updatedAt: Date.now(),
    ...(profile.hasProfileEvent && { hasProfileEvent: true as const }),
  };
  // Persist the canonical profile before attempting the optional image snapshot,
  // so quitting during that fetch cannot leave the durable fallback stale.
  writeSelfProfileCache(relayUrl, profile.pubkey, baseCache);

  const snapshotUrl = getAvatarSnapshotUrl(profile.avatarUrl);
  if (!snapshotUrl) return;
  const fetched = await fetchAvatarDataUrl(rewriteRelayUrl(snapshotUrl));
  if (!fetched) return;
  writeSelfProfileCache(relayUrl, profile.pubkey, {
    ...baseCache,
    avatarDataUrl: fetched,
  });
}
