import { useCallback, useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";

import { channelWindowKey } from "@/features/messages/lib/messageQueryKeys";
import {
  channelWindowHasMore,
  channelWindowHistoryExhausted,
  emptyChannelWindowStore,
  type ChannelWindowStore,
} from "@/features/messages/lib/channelWindowStore";
import { pageOlderMessagesUntilRowFloor } from "@/features/messages/lib/pageOlderMessages";
import type { Channel } from "@/shared/api/types";

export function useFetchOlderMessages(channel: Channel | null) {
  const queryClient = useQueryClient();
  const channelId = channel?.id ?? null;
  const [isFetchingOlder, setIsFetchingOlder] = useState(false);
  const isFetchingOlderRef = useRef(false);
  // The channel on screen now, for the paging guard below. `fetchOlder`
  // captures one render's `channelId`, and comparing it with that same
  // render's `channel?.id` was always true, so a page fetched for a channel
  // the user had already left was still appended and projected (F392).
  //
  // A generation, not the id: a page started on A must not land after
  // A -> B -> A either, because re-entering A replaced its window head. The
  // in-flight lock and loading flag belong to that generation too, so a slow
  // page on A neither shows as loading in B nor blocks B's own paging.
  const generationRef = useRef(0);
  const activeChannelIdRef = useRef<string | null>(channelId);
  useEffect(() => {
    generationRef.current += 1;
    activeChannelIdRef.current = channelId;
    isFetchingOlderRef.current = false;
    setIsFetchingOlder(false);
    return () => {
      generationRef.current += 1;
      activeChannelIdRef.current = null;
    };
  }, [channelId]);

  // Whether older history remains, derived reactively from the authoritative
  // window store rather than a private latch. A latch only reset on channelId
  // change went stale on reconnect: `refreshNewestWindow` replaces the newest
  // window with fresh `hasMore:true` rows, but the latch — flipped false when
  // the pre-reconnect window exhausted — kept the scroll observer uninstalled,
  // freezing paging at page one. Reading the store's tail `hasMore` self-heals:
  // the observer re-arms the moment the refreshed window reports more history.
  const windowKey = channelWindowKey(channelId ?? "none");
  const { data: hasOlderMessages = false } = useQuery({
    enabled: channelId !== null,
    queryKey: windowKey,
    select: channelWindowHasMore,
    // Passive subscription: the window store is written by the messages query
    // and the live subscription via setQueryData; this observer only reads.
    queryFn: () =>
      queryClient.getQueryData<ChannelWindowStore>(windowKey) ??
      emptyChannelWindowStore(),
  });

  // Distinct from `!hasOlderMessages`: an empty/unloaded window also reports
  // "no more", but exhaustion requires a RESOLVED tail page proving the
  // channel's beginning. Consumers gating UI on the history boundary (the
  // oldest day divider) must use this, not the paging signal.
  const { data: historyExhausted = false } = useQuery({
    enabled: channelId !== null,
    queryKey: windowKey,
    select: channelWindowHistoryExhausted,
    queryFn: () =>
      queryClient.getQueryData<ChannelWindowStore>(windowKey) ??
      emptyChannelWindowStore(),
  });

  const fetchOlder = useCallback(async () => {
    if (!channelId || isFetchingOlderRef.current) {
      return;
    }
    const store =
      queryClient.getQueryData<ChannelWindowStore>(
        channelWindowKey(channelId),
      ) ?? emptyChannelWindowStore();
    if (!channelWindowHasMore(store)) {
      return;
    }

    const requestGeneration = generationRef.current;
    const isCurrent = () =>
      generationRef.current === requestGeneration &&
      activeChannelIdRef.current === channelId;
    isFetchingOlderRef.current = true;
    setIsFetchingOlder(true);
    try {
      await pageOlderMessagesUntilRowFloor(queryClient, channelId, isCurrent);
    } catch (error) {
      console.error("Failed to fetch older messages", channelId, error);
    } finally {
      if (generationRef.current === requestGeneration) {
        isFetchingOlderRef.current = false;
        setIsFetchingOlder(false);
      }
    }
  }, [channelId, queryClient]);

  return { fetchOlder, isFetchingOlder, hasOlderMessages, historyExhausted };
}
