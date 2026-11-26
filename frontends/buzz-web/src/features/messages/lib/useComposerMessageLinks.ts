import type { Node as ProseMirrorNode } from "@tiptap/pm/model";
import type { Editor } from "@tiptap/react";
import * as React from "react";

import { parseChannelLink } from "./channelLink";
import {
  COMPOSER_MESSAGE_LINK_NODE_NAME,
  ComposerMessageLinkNode,
} from "./composerMessageLinkNode";
import { parseMessageLink } from "./messageLink";

export type ComposerMessageLinkChannel = { id: string; name: string };

/** The channel a chip's href names, for the two link families that name one. */
function linkedChannelId(href: string): string | null {
  const message = parseMessageLink(href);
  if (message.ok) return message.value.channelId;
  const channel = parseChannelLink(href);
  return channel.ok ? channel.value.channelId : null;
}

/**
 * The `channelName` attribute updates that bring a composer document in line
 * with the channel list.
 *
 * Mirrors `resolveComposerMessageLinkAttributes`: message links AND channel
 * links carry a channel name, entity links never do. Only a channel the list
 * actually knows changes a stored name. Blanking the rest cleared every
 * channel-link chip's name on each list change, and blanked every chip while
 * the list was still loading (F388); the chip's presentation already prefers
 * the live list and only falls back to the stored name. With no list at all
 * (`undefined`) there is nothing to reconcile against.
 */
export function composerChannelNameUpdates(
  doc: ProseMirrorNode,
  channels: readonly ComposerMessageLinkChannel[] | undefined,
): { position: number; channelName: string }[] {
  if (!channels || channels.length === 0) return [];
  const channelNamesById = new Map(
    channels.map((channel) => [channel.id, channel.name]),
  );
  const updates: { position: number; channelName: string }[] = [];
  doc.descendants((node, position) => {
    if (node.type.name !== COMPOSER_MESSAGE_LINK_NODE_NAME) return;
    const channelId = linkedChannelId(String(node.attrs.href ?? ""));
    if (channelId === null) return;
    const channelName = channelNamesById.get(channelId);
    if (channelName === undefined || channelName === node.attrs.channelName) {
      return;
    }
    updates.push({ position, channelName });
  });
  return updates;
}

export function useComposerMessageLinks(
  channels: readonly ComposerMessageLinkChannel[] | undefined,
) {
  const channelsRef = React.useRef(channels ?? []);
  channelsRef.current = channels ?? [];

  const resolveChannelName = React.useCallback(
    (channelId: string) =>
      channelsRef.current.find((channel) => channel.id === channelId)?.name,
    [],
  );

  const extension = React.useMemo(
    () => ComposerMessageLinkNode.configure({ resolveChannelName }),
    [resolveChannelName],
  );

  const syncChannelNames = React.useCallback(
    (editor: Editor) => {
      const updates = composerChannelNameUpdates(editor.state.doc, channels);
      if (updates.length === 0) return;
      let transaction = editor.state.tr;
      for (const { position, channelName } of updates) {
        transaction = transaction.setNodeAttribute(
          position,
          "channelName",
          channelName,
        );
      }
      // preventUpdate: a passive name refresh is not an authored edit.
      editor.view.dispatch(
        transaction.setMeta("addToHistory", false).setMeta("preventUpdate", true),
      );
    },
    [channels],
  );

  return { extension, resolveChannelName, syncChannelNames };
}
