import { useEffect, useEffectEvent } from "react";
import {
  type QueryClient,
  isCancelledError,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { toast } from "sonner";

import {
  channelMessagesKey,
  channelWindowKey,
  threadRepliesKey,
} from "@/features/messages/lib/messageQueryKeys";
import {
  buildReplyTags,
  getThreadReference,
  isBroadcastReply,
  normalizeMentionPubkeys,
  resolveReplyRootId,
} from "@/features/messages/lib/threading";
import {
  projectChannelWindowMessages,
  refreshChannelWindowMessages,
} from "@/features/messages/lib/projectChannelWindow";
import { reconcileChannelWindowMessages } from "@/features/messages/lib/channelWindowReconciliation";
import {
  applyLocalMessageDeletion,
  rollbackOptimisticSend,
} from "@/features/messages/lib/localMessageCacheUpdates";
import {
  channelHeadCacheScope,
  channelHeadHydration,
  consumeHydratedChannel,
} from "@/features/messages/lib/channelHeadCache";
import { storeChannelHeadCache } from "@/shared/api/tauriChannelHeadCache";
import {
  mergeMessages,
  mergeTimelineCacheMessages,
} from "@/features/messages/lib/messageMerge";

export { mergeMessages, mergeTimelineCacheMessages };
import { splitOutgoingTags } from "@/features/messages/lib/imetaMediaMarkdown";
import { messageMentionPubkeys } from "@/features/messages/lib/messageMentionPubkeys";
import { buildSentFromThreadTag } from "@/features/messages/lib/sentFromThread";
import {
  clearTimeoutState,
  recordTimeoutFromRejection,
  timeoutScopeEpoch,
} from "@/features/moderation/lib/timeoutStore";
import { relayClient, setVisibleChannel } from "@/shared/api/relayClient";
import { customEmojiQueryKey } from "@/features/custom-emoji/hooks";
import { channelsQueryKey } from "@/features/channels/hooks";
import { reactionEmojiUrl } from "@/shared/api/customEmoji";
import type { CustomEmoji } from "@/shared/lib/remarkCustomEmoji";
import {
  addReaction,
  deleteMessage,
  editMessage,
  removeReaction,
  sendChannelMessage,
} from "@/shared/api/tauri";
import { getChannelWindowEvents } from "@/shared/api/channelWindow";
import type { Channel, Identity, RelayEvent } from "@/shared/api/types";
import { invalidateMessageLinkMetadataForEvent } from "@/shared/ui/markdown/useMessageLinkMetadata";
// Same .mjs the renderer uses, so the cache-update projection can't drift
// from the on-render overlay.
import { applyLocalMessageEdit } from "@/features/messages/lib/localMessageEdit";
import {
  emptyChannelWindowStore,
  mapChannelWindowEvents,
  mergeLiveChannelWindowEvent,
  mergeLiveThreadSummary,
  replaceNewestChannelWindow,
  type ChannelWindowStore,
} from "@/features/messages/lib/channelWindowStore";
import {
  parseChannelWindowResponse,
  parseLiveThreadSummary,
} from "@/features/messages/lib/channelWindowResponse";
import {
  CHANNEL_AUX_EVENT_KINDS,
  CHANNEL_TIMELINE_CONTENT_KINDS,
  KIND_CHANNEL_THREAD_SUMMARY,
  KIND_STREAM_MESSAGE,
  KIND_SYSTEM_MESSAGE,
} from "@/shared/constants/kinds";

type MessageQueryContext = {
  optimisticId: string;
  channelId: string;
  /** Timeout scope the send started in (finding_2d613e60ee3ff2a0). */
  timeoutEpoch?: number;
};

const CHANNEL_TIMELINE_KINDS = new Set<number>(CHANNEL_TIMELINE_CONTENT_KINDS);
const CHANNEL_AUX_KINDS = new Set<number>(CHANNEL_AUX_EVENT_KINDS);

export function resolveCachedReplyRootId(
  parentEventId: string,
  messageCaches: readonly RelayEvent[][],
): string | null {
  for (const messages of messageCaches) {
    if (messages.some((event) => event.id === parentEventId)) {
      return resolveReplyRootId(parentEventId, messages);
    }
  }
  return null;
}

export function createOptimisticMessage(
  channelId: string,
  content: string,
  identity: Identity,
  currentMessages: RelayEvent[],
  mentionPubkeys: string[] = [],
  parentEventId: string | null = null,
  mediaTags: string[][] = [],
  sentFromThreadRootId: string | null = null,
  sentFromThreadRootExcerpt: string | null = null,
): RelayEvent {
  const localKey = `optimistic-${crypto.randomUUID()}`;
  const tags: string[][] = [];

  if (parentEventId) {
    tags.push(
      ...buildReplyTags(
        channelId,
        identity.pubkey,
        parentEventId,
        resolveReplyRootId(parentEventId, currentMessages),
        mentionPubkeys,
      ),
    );
  } else {
    tags.push(["h", channelId]);
    tags.push(["p", identity.pubkey]);
    for (const pubkey of normalizeMentionPubkeys(
      mentionPubkeys,
      identity.pubkey,
    )) {
      tags.push(["p", pubkey]);
    }
  }

  for (const tag of mediaTags) {
    tags.push(tag);
  }
  if (sentFromThreadRootId) {
    tags.push(
      buildSentFromThreadTag(sentFromThreadRootId, sentFromThreadRootExcerpt),
    );
  }

  return {
    id: localKey,
    localKey,
    pubkey: identity.pubkey,
    created_at: Math.floor(Date.now() / 1_000),
    kind: KIND_STREAM_MESSAGE,
    tags,
    content,
    sig: "",
    pending: true,
  };
}

/**
 * Resolves the effective target channel for a send operation.
 *
 * When `capturedChannelId` is supplied (non-null), the target is looked up from
 * `channelsCache` — this pins the send to the compose-time channel regardless
 * of any subsequent navigation. If the id is supplied but resolves to nothing,
 * returns `null` (caller should throw — don't silently fall back to the live
 * channel). When `capturedChannelId` is null, the caller didn't capture one and
 * the closed-over `fallbackChannel` is the intended target.
 *
 * Exported for unit testing.
 */
export function resolveEffectiveChannel(
  capturedChannelId: string | null | undefined,
  channelsCache: Channel[] | undefined,
  fallbackChannel: Channel | null,
): Channel | null {
  if (capturedChannelId == null) {
    return fallbackChannel;
  }
  return channelsCache?.find((c) => c.id === capturedChannelId) ?? null;
}

/**
 * Resolves a send target captured as either the channel object itself or its id.
 * A relay-returned channel remains authoritative even when the shared channel
 * list is temporarily stale and does not contain it.
 *
 * Exported for unit testing.
 */
export function resolveSendChannel(
  targetChannel: Channel | undefined,
  capturedChannelId: string | null | undefined,
  channelsCache: Channel[] | undefined,
  fallbackChannel: Channel | null,
): Channel | null {
  return (
    targetChannel ??
    resolveEffectiveChannel(capturedChannelId, channelsCache, fallbackChannel)
  );
}

/**
 * Resolves the thread reply target from a submit-time captured context or,
 * for callers that predate the capture pattern, from live refs.
 *
 * When `threadContext` is supplied (non-null), its values are used exclusively
 * — no live-ref reads occur. This is the race-free path: the context was
 * captured synchronously at submit time before any async awaits.
 *
 * When `threadContext` is null/undefined (legacy callers), falls back to
 * `liveReplyTargetId ?? liveThreadHeadId`.
 *
 * Returns null when no parentEventId can be resolved (caller should bail).
 */
export function resolveThreadReplyTarget(
  threadContext:
    | { parentEventId: string | null; threadHeadId: string | null }
    | null
    | undefined,
  liveReplyTargetId: string | null | undefined,
  liveThreadHeadId: string | null | undefined,
): { parentEventId: string; threadHeadId: string | null } | null {
  if (threadContext != null) {
    // Captured context: use exclusively — no ?? fallback to live refs.
    if (!threadContext.parentEventId) {
      return null;
    }
    return {
      parentEventId: threadContext.parentEventId,
      threadHeadId: threadContext.threadHeadId,
    };
  }
  // Legacy path: read from live refs.
  const parentEventId = liveReplyTargetId ?? liveThreadHeadId ?? null;
  if (!parentEventId) {
    return null;
  }
  return {
    parentEventId,
    threadHeadId: liveThreadHeadId ?? null,
  };
}

export function useChannelWindowQuery(channel: Channel | null) {
  const queryClient = useQueryClient();
  const queryKey = channelWindowKey(channel?.id ?? "none");
  return useQuery({
    enabled: channel !== null && channel.channelType !== "forum",
    queryKey,
    queryFn: () =>
      queryClient.getQueryData<ChannelWindowStore>(queryKey) ??
      emptyChannelWindowStore(),
    staleTime: Number.POSITIVE_INFINITY,
  });
}

export function reconcileFetchedChannelWindow(
  queryClient: QueryClient,
  channelId: string,
  events: Awaited<ReturnType<typeof getChannelWindowEvents>>,
  previousMessages: RelayEvent[],
  signal: AbortSignal,
): RelayEvent[] {
  // Tauri invokes cannot be canceled after dispatch. A replacement refetch can
  // therefore win while this older request is still in flight. Never let that
  // canceled request commit its stale page into the authoritative window.
  signal.throwIfAborted();
  // The rows kept from before the fetch are read now, after it: a send
  // rolled back or a reply deleted while the fetch was in flight must not
  // come back from a snapshot taken before it started.
  const latestMessages =
    queryClient.getQueryData<RelayEvent[]>(channelMessagesKey(channelId)) ??
    previousMessages;
  const windowKey = channelWindowKey(channelId);
  const page = parseChannelWindowResponse(events, channelId, null);
  const current =
    queryClient.getQueryData<ChannelWindowStore>(windowKey) ??
    emptyChannelWindowStore();
  const next = replaceNewestChannelWindow(current, page);
  queryClient.setQueryData(windowKey, next);
  const scope = channelHeadCacheScope(queryClient);
  if (scope) {
    void storeChannelHeadCache(scope, channelId, events).catch((error) => {
      console.warn("Failed to persist channel head", channelId, error);
    });
  }
  return reconcileChannelWindowMessages(next, latestMessages);
}

export function useChannelMessagesQuery(channel: Channel | null) {
  const queryClient = useQueryClient();
  const queryKey = channelMessagesKey(channel?.id ?? "none");
  return useQuery({
    enabled: channel !== null && channel.channelType !== "forum",
    queryKey,
    queryFn: async ({ signal }) => {
      if (!channel) throw new Error("No channel selected.");
      // Persisted heads seed asynchronously; wait for that seed so a channel
      // opened during boot takes the hydrated path instead of racing it with
      // a cold relay fetch.
      await channelHeadHydration(queryClient);
      if (consumeHydratedChannel(queryClient, channel.id)) {
        return queryClient.getQueryData<RelayEvent[]>(queryKey) ?? [];
      }
      const events = await getChannelWindowEvents(channel.id);
      return reconcileFetchedChannelWindow(
        queryClient,
        channel.id,
        events,
        [],
        signal,
      );
    },
    staleTime: 5 * 60 * 1_000,
    gcTime: 60 * 60 * 1_000,
  });
}

export function useChannelSubscription(channel: Channel | null) {
  const queryClient = useQueryClient();
  const channelId = channel?.id ?? null;
  const channelType = channel?.channelType ?? null;
  const refreshNewestWindow = useEffectEvent(async () => {
    if (!channelId) return;
    await refreshChannelWindowMessages(queryClient, channelId);
  });

  const appendMessage = useEffectEvent((event: RelayEvent) => {
    if (!channelId) return;
    if (event.kind === KIND_CHANNEL_THREAD_SUMMARY) {
      // Relay-pushed live badge recount — window-store overlay only, never a
      // timeline row (mirrors the page path, where 39005 is metadata).
      const parsed = parseLiveThreadSummary(event);
      if (!parsed) return;
      const windowKey = channelWindowKey(channelId);
      const current =
        queryClient.getQueryData<ChannelWindowStore>(windowKey) ??
        emptyChannelWindowStore();
      const next = mergeLiveThreadSummary(current, parsed.rootId, parsed.live);
      if (next !== current) queryClient.setQueryData(windowKey, next);
      return;
    }
    const isTimelineRow = CHANNEL_TIMELINE_KINDS.has(event.kind);
    // A live deletion, edit or moderation tombstone makes any cached
    // message-link preview of its target stale.
    invalidateMessageLinkMetadataForEvent(event);
    const threadReference = isTimelineRow
      ? getThreadReference(event.tags)
      : null;
    if (threadReference?.parentId != null) {
      const rootId = threadReference?.rootId;
      if (rootId) {
        queryClient.setQueryData<RelayEvent[]>(
          threadRepliesKey(channelId, rootId),
          (current = []) => mergeMessages(current, event),
        );
      }
      if (!isBroadcastReply(event.tags)) return;
    }
    if (!isTimelineRow && !CHANNEL_AUX_KINDS.has(event.kind)) return;
    if (!isTimelineRow) {
      queryClient.setQueriesData<RelayEvent[]>(
        { queryKey: ["thread-replies", channelId] },
        (current = []) => mergeMessages(current, event),
      );
    }

    const windowKey = channelWindowKey(channelId);
    const current =
      queryClient.getQueryData<ChannelWindowStore>(windowKey) ??
      emptyChannelWindowStore();
    const next = mergeLiveChannelWindowEvent(current, event, isTimelineRow);
    if (next !== current) {
      queryClient.setQueryData(windowKey, next);
      projectChannelWindowMessages(queryClient, channelId);
    }

    if (event.kind === KIND_SYSTEM_MESSAGE) {
      try {
        const payload = JSON.parse(event.content) as { type?: string };
        if (
          payload.type === "member_joined" ||
          payload.type === "member_left" ||
          payload.type === "member_removed"
        ) {
          void queryClient.invalidateQueries({
            queryKey: ["channels", channelId, "members"],
          });
          void queryClient.invalidateQueries({
            queryKey: ["channels"],
            exact: true,
          });
        }
      } catch {
        // Non-JSON system message — ignore.
      }
    }
  });

  // Notify the relay client which channel is currently visible so its live
  // subscriptions are replayed first on reconnect, reducing latency on
  // degraded networks.
  useEffect(() => {
    if (!channelId || channelType === "forum") return;
    setVisibleChannel(channelId);
    return () => {
      setVisibleChannel(null);
    };
  }, [channelId, channelType]);

  useEffect(() => {
    if (!channelId || channelType === "forum") {
      return;
    }

    let isDisposed = false;
    let cleanup: (() => Promise<void>) | undefined;
    const disposeReconnectListener = relayClient.subscribeToReconnects(() => {
      void refreshNewestWindow().catch((error) => {
        if (!isDisposed && !isCancelledError(error)) {
          console.error(
            "Failed to refresh channel window after reconnecting",
            channelId,
            error,
          );
        }
      });
    });

    // The live subscription starts at "now", so it cannot close the gap
    // between the last page snapshot and subscription establishment. Always
    // refresh once subscription setup settles — on success because freshness
    // alone is not proof that no relay events landed in that interval, and on
    // failure because a hydrated channel has no other authoritative fetch:
    // the relay window endpoint may be healthy even when the live socket is
    // not, and the reconnect listener above re-syncs when it recovers.
    const refreshAfterSubscribe = (outcome: string) => {
      if (isDisposed) return;
      void refreshNewestWindow().catch((error) => {
        // An optimistic send deliberately cancels the refresh before updating
        // the timeline; that cancellation is not a relay failure.
        if (!isDisposed && !isCancelledError(error)) {
          console.error(
            `Failed to refresh channel window after ${outcome}`,
            channelId,
            error,
          );
        }
      });
    };
    relayClient
      .subscribeToChannelLive(channelId, (event) => {
        if (!isDisposed) {
          appendMessage(event);
        }
      })
      .then(
        (dispose) => {
          if (isDisposed) {
            void dispose();
            return;
          }
          cleanup = dispose;
          refreshAfterSubscribe("subscribing");
        },
        (error) => {
          console.error("Failed to subscribe to channel", channelId, error);
          refreshAfterSubscribe("subscription failure");
        },
      );

    return () => {
      isDisposed = true;
      disposeReconnectListener();
      if (cleanup) {
        void cleanup();
      }
    };
  }, [channelId, channelType]);
}

export function useSendMessageMutation(
  channel: Channel | null,
  identity: Identity | undefined,
) {
  const queryClient = useQueryClient();

  return useMutation<
    RelayEvent,
    Error,
    {
      channelId?: string;
      targetChannel?: Channel;
      content: string;
      mentionPubkeys?: string[];
      parentEventId?: string | null;
      mediaTags?: string[][];
      forceRest?: boolean;
      sentFromThreadRootId?: string | null;
      sentFromThreadRootExcerpt?: string | null;
      transport?: "auto" | "http";
    },
    MessageQueryContext | undefined
  >({
    mutationFn: async ({
      channelId: capturedChannelId,
      targetChannel,
      content,
      mentionPubkeys,
      parentEventId,
      mediaTags,
      forceRest,
      sentFromThreadRootId,
      sentFromThreadRootExcerpt,
      transport = "auto",
    }) => {
      // Prefer a channel captured by the caller at compose time. Otherwise,
      // resolve a captured id from the shared channel cache so navigation
      // cannot redirect the message. Legacy callers without either value use
      // the closed-over `channel`.
      const effectiveChannel = resolveSendChannel(
        targetChannel,
        capturedChannelId,
        queryClient.getQueryData<Channel[]>(channelsQueryKey),
        channel,
      );

      if (effectiveChannel == null) {
        if (capturedChannelId != null) {
          throw new Error("Channel is no longer available.");
        }
        throw new Error("This channel does not support message sending yet.");
      }

      if (effectiveChannel.channelType === "forum") {
        throw new Error("This channel does not support message sending yet.");
      }

      if (!identity) {
        throw new Error("No identity available for sending messages.");
      }

      // `mediaTags` arrives as the merged outgoing tag set (imeta + NIP-30
      // emoji). Split it so each kind goes to its own validated Tauri arg —
      // emoji tags must NOT ride the imeta-only `media` channel (that gate
      // rejects any non-imeta prefix, which silently dropped emoji sends).
      const {
        mediaTags: imetaTags,
        emojiTags,
        mentionTags,
        linkPreviewTags,
      } = splitOutgoingTags(mediaTags);
      const recipientPubkeys = messageMentionPubkeys(
        effectiveChannel,
        identity.pubkey,
        mentionPubkeys,
      );
      if (sentFromThreadRootId && parentEventId) {
        throw new Error(
          "A thread message can only be sent as a top-level message.",
        );
      }

      const sentFromThreadTag = sentFromThreadRootId
        ? buildSentFromThreadTag(
            sentFromThreadRootId,
            sentFromThreadRootExcerpt,
          )
        : undefined;

      // Messages carrying media OR custom-emoji tags MUST go through REST so
      // the relay's tag validation runs. The WebSocket path emits no extra
      // tags, so emoji-only messages would otherwise lose their emoji tag.
      if (
        forceRest ||
        transport === "http" ||
        parentEventId ||
        imetaTags.length > 0 ||
        emojiTags.length > 0 ||
        linkPreviewTags.length > 0
      ) {
        const cachedMessages =
          queryClient.getQueryData<RelayEvent[]>(
            channelMessagesKey(effectiveChannel.id),
          ) ?? [];
        const threadCaches = queryClient
          .getQueriesData<RelayEvent[]>({
            queryKey: ["thread-replies", effectiveChannel.id],
          })
          .flatMap(([, events]) => (events ? [events] : []));
        const suppliedRootEventId = parentEventId
          ? resolveCachedReplyRootId(parentEventId, [
              cachedMessages,
              ...threadCaches,
            ])
          : null;
        const result = await sendChannelMessage(
          effectiveChannel.id,
          content,
          parentEventId ?? null,
          imetaTags,
          recipientPubkeys,
          undefined,
          emojiTags,
          mentionTags,
          linkPreviewTags,
          sentFromThreadTag,
          undefined,
          // The identity this composer rendered as: the backend refuses to
          // sign as anyone else if the signer changed meanwhile
          // (finding_7165e3b84de54cdc).
          identity.pubkey,
          suppliedRootEventId,
        );

        // Build tags matching relay-emitted shape: h, author p, mention ps, reply es, imeta, emoji.
        // For replies, buildReplyTags already includes ["p", author] and ["h", channel].
        // For non-replies (media-only), we add them ourselves.
        const replyTags = parentEventId
          ? buildReplyTags(
              effectiveChannel.id,
              identity.pubkey,
              parentEventId,
              result.rootEventId ?? parentEventId,
              recipientPubkeys,
            )
          : [];
        const baseTags = parentEventId
          ? replyTags // buildReplyTags includes h + author p + mention ps
          : [
              ["h", effectiveChannel.id],
              ["p", identity.pubkey],
            ]; // non-reply: add ourselves

        return {
          id: result.eventId,
          pubkey: identity.pubkey,
          created_at: result.createdAt,
          kind: KIND_STREAM_MESSAGE,
          tags: [
            ...baseTags,
            // For non-replies, add mention p-tags here (replies get them via buildReplyTags)
            ...(!parentEventId
              ? normalizeMentionPubkeys(recipientPubkeys, identity.pubkey).map(
                  (pk) => ["p", pk],
                )
              : []),
            ...imetaTags,
            ...emojiTags,
            ...mentionTags,
            ...linkPreviewTags,
            ...(sentFromThreadTag ? [sentFromThreadTag] : []),
          ],
          content: content.trim(),
          sig: "",
        };
      }

      return relayClient.sendMessage(
        effectiveChannel.id,
        content,
        recipientPubkeys,
        [...mentionTags, ...(sentFromThreadTag ? [sentFromThreadTag] : [])],
      );
    },
    onMutate: async ({
      channelId: capturedChannelId,
      targetChannel,
      content,
      mentionPubkeys,
      parentEventId,
      mediaTags,
      sentFromThreadRootId,
      sentFromThreadRootExcerpt,
    }) => {
      // Mirror mutationFn's target resolution so the optimistic message lands
      // in the cache for the same channel as the real send. A caller-supplied
      // channel remains valid even when a stale channel-list read omitted it.
      const effectiveChannel = resolveSendChannel(
        targetChannel,
        capturedChannelId,
        queryClient.getQueryData<Channel[]>(channelsQueryKey),
        channel,
      );

      if (
        !effectiveChannel ||
        !identity ||
        effectiveChannel.channelType === "forum"
      ) {
        return undefined;
      }

      const queryKey = channelMessagesKey(effectiveChannel.id);
      const windowKey = channelWindowKey(effectiveChannel.id);
      // The rendered timeline is projected from the channel-window cache. Cancel
      // both reads before snapshotting either cache so an older window response
      // cannot replace the optimistic row between onMutate and onSuccess.
      await Promise.all([
        queryClient.cancelQueries({ queryKey }),
        queryClient.cancelQueries({ queryKey: windowKey }),
      ]);

      const previousMessages =
        queryClient.getQueryData<RelayEvent[]>(queryKey) ?? [];
      const previousWindow =
        queryClient.getQueryData<ChannelWindowStore>(windowKey);
      const optimisticMessage = createOptimisticMessage(
        effectiveChannel.id,
        content.trim(),
        identity,
        previousMessages,
        mentionPubkeys ?? [],
        parentEventId ?? null,
        mediaTags ?? [],
        sentFromThreadRootId ?? null,
        sentFromThreadRootExcerpt ?? null,
      );

      const nextWindow = mergeLiveChannelWindowEvent(
        previousWindow ?? emptyChannelWindowStore(),
        optimisticMessage,
      );
      queryClient.setQueryData(windowKey, nextWindow);
      projectChannelWindowMessages(queryClient, effectiveChannel.id);

      return {
        optimisticId: optimisticMessage.id,
        channelId: effectiveChannel.id,
        timeoutEpoch: timeoutScopeEpoch(),
      };
    },
    onError: (error, _variables, context) => {
      // A community timeout surfaces here as the relay's `OK false` reason.
      // Record it so the composer can show the timeout chip and block further
      // sends until it expires; other errors fall through to the caller.
      // Only in the community the send started in.
      recordTimeoutFromRejection(error?.message, context?.timeoutEpoch);
      if (!context) {
        return;
      }

      // Remove only the failed optimistic row. Restoring the pre-send
      // snapshots would drop every live event that landed while the send was
      // in flight, and is a no-op when no window existed yet.
      rollbackOptimisticSend(
        queryClient,
        context.channelId,
        context.optimisticId,
      );
    },
    onSuccess: (message, _variables, context) => {
      // An accepted send proves the write-block is lifted; clear any recorded
      // timeout so the chip and disable state fall away immediately.
      clearTimeoutState(context?.timeoutEpoch);
      if (!context) {
        return;
      }

      const windowKey = channelWindowKey(context.channelId);
      const current =
        queryClient.getQueryData<ChannelWindowStore>(windowKey) ??
        emptyChannelWindowStore();
      const withoutPending: ChannelWindowStore = {
        ...current,
        liveOverlay: current.liveOverlay.filter(
          (event) => event.id !== context.optimisticId,
        ),
      };
      const next = mergeLiveChannelWindowEvent(withoutPending, {
        ...message,
        localKey: context.optimisticId,
      });
      queryClient.setQueryData(windowKey, next);
      projectChannelWindowMessages(queryClient, context.channelId);
    },
  });
}

export function useToggleReactionMutation() {
  const queryClient = useQueryClient();
  return useMutation<
    void,
    Error,
    {
      eventId: string;
      emoji: string;
      remove: boolean;
    }
  >({
    mutationFn: async ({ eventId, emoji, remove }) => {
      if (remove) {
        await removeReaction(eventId, emoji);
        return;
      }

      // Custom-emoji reaction: emoji is `:shortcode:`. Resolve its image URL
      // from the cached community palette so the kind:7 carries the NIP-30
      // `["emoji", shortcode, url]` tag. Unicode reactions resolve to no URL.
      const emojiUrl = reactionEmojiUrl(
        emoji,
        queryClient.getQueryData<CustomEmoji[]>(customEmojiQueryKey),
      );
      await addReaction(eventId, emoji, emojiUrl);
    },
  });
}

/**
 * Pin the channel a mutation was started in into its variables. TanStack
 * hands an in-flight mutation the latest render's options, so a `channel`
 * read from the closure would be whatever channel is open when the request
 * settles, not the one the user acted in.
 */
function withChannelPinned<TVariables extends { channelId?: string }, TResult>(
  mutation: TResult & {
    mutate: (variables: TVariables, ...rest: never[]) => void;
    mutateAsync: (variables: TVariables, ...rest: never[]) => Promise<void>;
  },
  channel: Channel | null,
): TResult {
  const pin = (variables: TVariables): TVariables =>
    variables.channelId || !channel
      ? variables
      : { ...variables, channelId: channel.id };
  return {
    ...mutation,
    mutate: (variables: TVariables, ...rest: never[]) =>
      mutation.mutate(pin(variables), ...rest),
    mutateAsync: (variables: TVariables, ...rest: never[]) =>
      mutation.mutateAsync(pin(variables), ...rest),
  };
}

export function useDeleteMessageMutation(channel: Channel | null) {
  const queryClient = useQueryClient();

  const mutation = useMutation<
    void,
    Error,
    { eventId: string; channelId?: string }
  >({
    mutationFn: async ({ eventId, channelId }) => {
      const id = channelId ?? channel?.id;
      if (!id) {
        throw new Error("No channel selected.");
      }
      await deleteMessage(id, eventId);
    },
    onSuccess: (_data, { eventId, channelId }) => {
      const id = channelId ?? channel?.id;
      if (!id) return;
      // Through the window store (the source of truth) and the thread caches,
      // not just the flattened timeline, which the next live event re-projects.
      applyLocalMessageDeletion(queryClient, id, eventId);
    },
    onError: (error) => {
      toast.error(`Failed to delete message: ${error.message}`);
    },
  });
  return withChannelPinned(mutation, channel);
}

export function useEditMessageMutation(channel: Channel | null) {
  const queryClient = useQueryClient();

  const mutation = useMutation<
    void,
    Error,
    {
      eventId: string;
      content: string;
      mediaTags?: string[][];
      // Pubkeys of mentions *newly added* by this edit, diffed at the composer.
      // Only these receive a `p` tag so a typo-fix edit re-wakes nobody.
      mentionPubkeys?: string[];
      channelId?: string;
    }
  >({
    mutationFn: async ({
      eventId,
      content,
      mediaTags,
      mentionPubkeys,
      channelId,
    }) => {
      const id = channelId ?? channel?.id;
      if (!id) {
        throw new Error("No channel selected.");
      }

      // `mediaTags` arrives as the merged outgoing set (imeta + NIP-30 emoji).
      // Split so each rides its own validated Tauri arg — emoji tags must NOT
      // go through the imeta-only `mediaTags` channel (the Rust `imeta_tags`
      // guard rejects any non-imeta prefix), mirroring the send path.
      const {
        mediaTags: imetaTags,
        emojiTags,
        mentionTags,
      } = splitOutgoingTags(mediaTags);

      await editMessage(
        id,
        eventId,
        content,
        imetaTags,
        emojiTags,
        mentionPubkeys,
        false,
        mentionTags,
      );
    },
    onSuccess: (
      _data,
      { eventId, content, mediaTags, mentionPubkeys, channelId },
    ) => {
      const id = channelId ?? channel?.id;
      if (!id) {
        return;
      }

      // Apply-on-success cache update: reflect the edit's new content and
      // imeta tag set immediately, so the local cache matches what the
      // receiver overlay (formatTimelineMessages) will produce when the
      // edit event arrives back from the relay. (Not a true optimistic
      // update — runs in onSuccess, not onMutate. Worth bearing the cost
      // only because the edit event round-trip can lag perceptibly.) Earlier
      // edit overlays of the same message are rewritten too, and so are the
      // thread caches a reply is rendered from.
      const applyEdit = (message: RelayEvent): RelayEvent =>
        applyLocalMessageEdit(message, {
          eventId,
          content,
          mediaTags,
          mentionPubkeys,
        });

      // The WINDOW STORE is the source of truth: every live merge
      // re-flattens it over `channelMessagesKey`, so patching only the
      // flattened array gets reverted by the next live event (see
      // mapChannelWindowEvents). Update the store first, then keep the
      // flattened cache in step for immediate paint.
      queryClient.setQueryData<ChannelWindowStore>(
        channelWindowKey(id),
        (current) =>
          current ? mapChannelWindowEvents(current, applyEdit) : current,
      );
      queryClient.setQueryData<RelayEvent[]>(
        channelMessagesKey(id),
        (current = []) => current.map(applyEdit),
      );
      queryClient.setQueriesData<RelayEvent[]>(
        { queryKey: ["thread-replies", id] },
        (current) => (Array.isArray(current) ? current.map(applyEdit) : current),
      );
    },
  });
  return withChannelPinned(mutation, channel);
}
