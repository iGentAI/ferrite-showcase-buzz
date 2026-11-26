import * as React from "react";

import type { ParsedMessageLink } from "@/features/messages/lib/messageLink";
import { summarizeMessageLinkContent } from "@/features/messages/lib/messageLinkMetadata";
import { getEventById } from "@/shared/api/tauri";
import { getUserProfile } from "@/shared/api/tauriProfiles";
import { truncateNpub } from "@/shared/lib/pubkey";
import { isDefinitiveEventNotFound } from "@/shared/lib/eventLookupError";

import {
  type CachedMessageLinkMetadata,
  createMessageLinkMetadataStore,
  type MessageLinkMetadata,
  type MessageLinkMetadataStore,
  messageLinkMetadataKey,
} from "./messageLinkMetadataStore";

type MessageLinkMetadataState =
  | { kind: "idle" }
  | { kind: "loading" }
  | ({ kind: "ready" } & MessageLinkMetadata)
  | { kind: "deleted" }
  | { kind: "unavailable" };

const defaultMessageLinkMetadataStore = createMessageLinkMetadataStore({
  getEvent: (messageId) => getEventById(messageId),
  getProfile: (pubkey) => getUserProfile(pubkey),
  isDefinitiveNotFound: isDefinitiveEventNotFound,
  shortPubkey: truncateNpub,
  summarize: summarizeMessageLinkContent,
});

/**
 * The store message-link pills read from. Production uses the module store;
 * tests (and any isolated surface) can provide their own.
 */
export const MessageLinkMetadataStoreContext =
  React.createContext<MessageLinkMetadataStore>(
    defaultMessageLinkMetadataStore,
  );

export function resetMessageLinkMetadataCache() {
  defaultMessageLinkMetadataStore.reset();
}

/**
 * Drop cached previews of any message a live deletion, edit or moderation
 * tombstone refers to, so a pill never keeps showing removed or superseded
 * text.
 */
export function invalidateMessageLinkMetadataForEvent(event: {
  kind: number;
  tags: string[][];
}) {
  defaultMessageLinkMetadataStore.invalidateForEvent(event);
}

/**
 * Metadata for one message-link pill.
 *
 * A readable pill resolves itself on mount only while the shared store admits
 * another eager lookup; otherwise, and whenever its cached answer has gone
 * stale or been invalidated, it resolves on `requestLoad` (hover or focus).
 */
export function useMessageLinkMetadata(
  link: ParsedMessageLink,
  channelReadable: boolean,
): { requestLoad: () => void; state: MessageLinkMetadataState } {
  const store = React.useContext(MessageLinkMetadataStoreContext);
  const key = messageLinkMetadataKey(link);
  const [resolved, setResolved] = React.useState<{
    key: string;
    state: CachedMessageLinkMetadata;
  } | null>(() => {
    const cached = channelReadable ? store.peek(link) : undefined;
    return cached ? { key, state: cached.value } : null;
  });
  const resolvedRef = React.useRef(resolved);
  resolvedRef.current = resolved;
  const requestId = React.useRef(0);
  const { channelId, messageId } = link;

  const run = React.useCallback(
    (eager: boolean) => {
      if (!channelReadable) return;
      const request = store.load({ channelId, messageId }, { eager });
      if (!request) return;
      const currentRequest = requestId.current;
      void request.then((metadata) => {
        if (requestId.current !== currentRequest) return;
        if (
          resolvedRef.current?.key === key &&
          resolvedRef.current.state === metadata
        ) {
          return;
        }
        setResolved({ key, state: metadata });
      });
    },
    [channelId, channelReadable, key, messageId, store],
  );

  React.useEffect(() => {
    requestId.current += 1;
    if (!channelReadable) return;
    const linkRef = { channelId, messageId };
    const cached = store.peek(linkRef);
    if (
      cached &&
      !(
        resolvedRef.current?.key === key &&
        resolvedRef.current.state === cached.value
      )
    ) {
      setResolved({ key, state: cached.value });
    }
    if (!cached?.fresh) run(true);
    const unsubscribe = store.subscribe(linkRef, () => {
      requestId.current += 1;
      setResolved(null);
    });
    return () => {
      requestId.current += 1;
      unsubscribe();
    };
  }, [channelId, channelReadable, key, messageId, run, store]);

  const requestLoad = React.useCallback(() => {
    if (!channelReadable) return;
    const cached = store.peek({ channelId, messageId });
    if (cached?.fresh && resolvedRef.current?.key === key) return;
    run(false);
  }, [channelId, channelReadable, key, messageId, run, store]);

  if (!channelReadable) return { requestLoad, state: { kind: "idle" } };
  return {
    requestLoad,
    state: resolved?.key === key ? resolved.state : { kind: "loading" },
  };
}
