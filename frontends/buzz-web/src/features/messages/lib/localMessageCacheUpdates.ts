import type { QueryClient } from "@tanstack/react-query";

import type { RelayEvent } from "@/shared/api/types";
import {
  removeChannelWindowEvents,
  type ChannelWindowStore,
} from "./channelWindowStore";
import { channelMessagesKey, channelWindowKey } from "./messageQueryKeys";
import { projectChannelWindowMessages } from "./projectChannelWindow";

/** Prefix shared by every `threadRepliesKey(channelId, rootId)` of a channel. */
function channelThreadRepliesPrefix(channelId: string) {
  return ["thread-replies", channelId] as const;
}

/**
 * Remove events from every client cache that renders a channel: the window
 * store (the source of truth), the flattened timeline projected from it, and
 * the channel's open thread-reply caches.
 *
 * The store is edited first and the flattened cache is filtered too, because
 * `reconcileChannelWindowMessages` keeps pending rows and thread replies it
 * finds only in the flattened cache; leaving an id there would let the next
 * projection bring the row back.
 */
function removeEventsFromChannelCaches(
  queryClient: QueryClient,
  channelId: string,
  ids: ReadonlySet<string>,
) {
  queryClient.setQueryData<ChannelWindowStore>(
    channelWindowKey(channelId),
    (current) =>
      current ? removeChannelWindowEvents(current, ids) : current,
  );
  queryClient.setQueryData<RelayEvent[]>(
    channelMessagesKey(channelId),
    (current) => current?.filter((event) => !ids.has(event.id)),
  );
  queryClient.setQueriesData<RelayEvent[]>(
    { queryKey: channelThreadRepliesPrefix(channelId) },
    (current) => current?.filter((event) => !ids.has(event.id)),
  );
  projectChannelWindowMessages(queryClient, channelId);
}

/**
 * Apply a successful delete locally. Patching only the flattened timeline is
 * reverted by the next live event or refresh, which re-projects the window
 * store; the row would reappear until the relay's deletion marker arrived (and
 * for good while the live socket is down).
 */
export function applyLocalMessageDeletion(
  queryClient: QueryClient,
  channelId: string,
  eventId: string,
) {
  removeEventsFromChannelCaches(queryClient, channelId, new Set([eventId]));
}

/**
 * Roll back one failed optimistic send by removing only its row.
 *
 * Restoring the snapshot taken before the send would discard every live event
 * that arrived while the send was in flight, and when no window existed at
 * snapshot time the restore is a no-op (TanStack ignores `undefined`), leaving
 * a ghost "sending" row that reconciliation keeps forever.
 */
export function rollbackOptimisticSend(
  queryClient: QueryClient,
  channelId: string,
  optimisticId: string,
) {
  removeEventsFromChannelCaches(queryClient, channelId, new Set([optimisticId]));
}
