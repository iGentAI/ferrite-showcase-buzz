import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";

import type { MainTimelineEntry } from "./threadPanel";
import type { TimelineMessage } from "../types";
import type { Channel } from "@/shared/api/types";
import {
  collectRenderScopedReactionMessageIds,
  hydrateRenderScopedReactions,
} from "./renderScopedReactions";
import { channelMessagesKey } from "./messageQueryKeys";
import { mergeMessages } from "./messageMerge";
import { relayClient } from "@/shared/api/relayClient";
import type { RelayEvent } from "@/shared/api/types";

export function useRenderScopedReactionHydration(input: {
  activeChannel: Channel | null;
  mainTimelineEntries: MainTimelineEntry[];
  threadHeadMessage: TimelineMessage | null;
  threadMessages: MainTimelineEntry[];
}) {
  const queryClient = useQueryClient();
  const messageIds = collectRenderScopedReactionMessageIds({
    mainEntries: input.mainTimelineEntries,
    threadHeadMessage: input.threadHeadMessage,
    threadEntries: input.threadMessages,
  });
  const messageIdsKey = messageIds.join(":");

  React.useEffect(() => {
    const channelId = input.activeChannel?.id;
    if (!channelId || input.activeChannel?.channelType === "forum") {
      return;
    }

    if (messageIds.length === 0) return;

    let disposed = false;
    let unsubscribe: (() => Promise<void>) | undefined;
    const timeout = window.setTimeout(() => {
      void hydrateRenderScopedReactions({
        channelId,
        messageIds,
        queryClient,
      });
    }, 0);

    void relayClient
      .subscribeToMessageReactionsLive(messageIds, (event: RelayEvent) => {
        if (disposed) return;
        queryClient.setQueryData<RelayEvent[]>(
          channelMessagesKey(channelId),
          (current = []) => mergeMessages(current, event),
        );
      })
      .then((dispose) => {
        if (disposed) {
          void dispose();
        } else {
          unsubscribe = dispose;
        }
      })
      .catch((error) => {
        if (!disposed) {
          console.error(
            "Failed to subscribe to visible message reactions",
            channelId,
            error,
          );
        }
      });

    return () => {
      disposed = true;
      window.clearTimeout(timeout);
      if (unsubscribe) void unsubscribe();
    };
  }, [
    input.activeChannel,
    messageIdsKey,
    queryClient,
  ]);
}
