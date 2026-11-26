import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";

import {
  emptyChannelWindowStore,
  mergeLiveChannelWindowEvent,
  type ChannelWindowStore,
} from "@/features/messages/lib/channelWindowStore";
import { channelWindowKey } from "@/features/messages/lib/messageQueryKeys";
import { projectChannelWindowMessages } from "@/features/messages/lib/projectChannelWindow";
import {
  getChannelIdFromTags,
  getThreadReference,
} from "@/features/messages/lib/threading";
import { fetchStructuralAuxForMessages } from "@/features/messages/lib/auxBackfill";
import { getEventById } from "@/shared/api/tauri";
import type { Channel, RelayEvent } from "@/shared/api/types";
import { isDefinitiveEventNotFound } from "@/shared/lib/eventLookupError";

const EVENT_ID_RE = /^[0-9a-f]{64}$/i;
/** Ancestor lookups in flight at once; the rest wait their turn. */
const ANCESTOR_LOOKUP_CONCURRENCY = 4;

export function useLoadMissingAncestors(
  activeChannel: Channel | null,
  resolvedMessages: RelayEvent[],
  fetchAux: (
    channelId: string,
    messageIds: string[],
  ) => Promise<RelayEvent[]> = fetchStructuralAuxForMessages,
) {
  const queryClient = useQueryClient();
  const requestedAncestorIdsRef = React.useRef<Set<string>>(new Set());
  const previousChannelIdRef = React.useRef<string | null>(null);

  // An answer that arrives after unmount belongs to no screen.
  React.useEffect(
    () => () => {
      previousChannelIdRef.current = null;
    },
    [],
  );

  React.useEffect(() => {
    const activeChannelId = activeChannel?.id ?? null;
    if (previousChannelIdRef.current === activeChannelId) {
      return;
    }
    previousChannelIdRef.current = activeChannelId;
    requestedAncestorIdsRef.current.clear();
  }, [activeChannel?.id]);

  React.useEffect(() => {
    if (!activeChannel || activeChannel.channelType === "forum") {
      return;
    }

    const knownEvents = new Map(
      resolvedMessages.map((message) => [message.id, message]),
    );
    const missingAncestorIds = new Set<string>();

    for (const message of resolvedMessages) {
      const thread = getThreadReference(message.tags);

      for (const eventId of [thread.parentId, thread.rootId]) {
        // Only a real relay event id can be looked up: an optimistic id or a
        // malformed tag would be asked for, fail, and be asked for again.
        if (
          !eventId ||
          !EVENT_ID_RE.test(eventId) ||
          knownEvents.has(eventId) ||
          requestedAncestorIdsRef.current.has(eventId)
        ) {
          continue;
        }

        missingAncestorIds.add(eventId);
      }
    }

    if (missingAncestorIds.size === 0) {
      return;
    }

    for (const eventId of missingAncestorIds) {
      requestedAncestorIdsRef.current.add(eventId);
    }

    const maxRequestedAncestors = 500;
    if (requestedAncestorIdsRef.current.size > maxRequestedAncestors) {
      const excess =
        requestedAncestorIdsRef.current.size - maxRequestedAncestors;
      let removed = 0;
      for (const id of requestedAncestorIdsRef.current) {
        if (removed >= excess) {
          break;
        }
        requestedAncestorIdsRef.current.delete(id);
        removed++;
      }
    }

    const channelId = activeChannel.id;

    const queue = [...missingAncestorIds];
    const lookUp = async (eventId: string) => {
        try {
          const event = await getEventById(eventId);

          // Only a channel switch makes the answer stale. This effect re-runs
          // on every new message, and dropping an in-flight answer then left
          // the id marked as requested, so it was never fetched again.
          if (previousChannelIdRef.current !== channelId) return;
          if (getChannelIdFromTags(event.tags) !== channelId) return;

          // The window store is the timeline's source of truth: every live
          // event, head refresh and page append re-projects the flattened
          // cache from it, so an ancestor patched only into that cache was
          // gone after the next projection and, already marked requested,
          // was never fetched again (F393). It is kept with the store's
          // retained out-of-page events (`liveAux`), which survive live
          // merges, page appends and head refetches, then projected.
          //
          // Its edits and deletions come with it: the event alone renders its
          // original text, and no window page will carry them (best effort).
          const aux = await fetchAux(channelId, [event.id]).catch(() => []);
          if (previousChannelIdRef.current !== channelId) return;
          queryClient.setQueryData<ChannelWindowStore>(
            channelWindowKey(channelId),
            (current) => {
              let next = mergeLiveChannelWindowEvent(
                current ?? emptyChannelWindowStore(),
                event,
                false,
              );
              for (const auxEvent of aux) {
                next = mergeLiveChannelWindowEvent(next, auxEvent, false);
              }
              return next;
            },
          );
          projectChannelWindowMessages(queryClient, channelId);
        } catch (error) {
          // A transient failure resolved nothing, so a later pass may retry
          // it. A definitive "not found" (the ancestor was deleted, or never
          // existed) stays marked as requested: asking again on every new
          // message would be a lookup storm with the same answer.
          if (
            previousChannelIdRef.current === channelId &&
            !isDefinitiveEventNotFound(error)
          ) {
            requestedAncestorIdsRef.current.delete(eventId);
          }
          console.error("Failed to load ancestor event", eventId, error);
        }
    };
    const worker = async () => {
      for (let next = queue.shift(); next !== undefined; next = queue.shift()) {
        await lookUp(next);
      }
    };
    void Promise.all(
      Array.from(
        { length: Math.min(ANCESTOR_LOOKUP_CONCURRENCY, queue.length) },
        worker,
      ),
    );
  }, [activeChannel, fetchAux, queryClient, resolvedMessages]);
}
