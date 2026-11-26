import { applyEditTagOverlay } from "@/features/messages/lib/applyEditTagOverlay.mjs";
import type { RelayEvent } from "@/shared/api/types";
import { KIND_STREAM_MESSAGE_EDIT } from "@/shared/constants/kinds";

export type LocalMessageEdit = {
  eventId: string;
  content: string;
  mediaTags?: string[][];
  mentionPubkeys?: string[];
};

/** The last `e` tag, which names an edit's target. */
function editTargetId(tags: string[][]): string | null {
  for (let index = tags.length - 1; index >= 0; index -= 1) {
    const tag = tags[index];
    if (tag?.[0] === "e" && typeof tag[1] === "string") return tag[1];
  }
  return null;
}

/**
 * Reflect a just-published edit in a cached event.
 *
 * The target message gets the new content and tag overlay. An EARLIER edit
 * event (kind 40003) aimed at the same message is rewritten too: the timeline
 * renders the newest edit overlay in preference to the message's own content,
 * so a second edit that patched only the message would keep showing the
 * first edit's text until the relay echoed the new one.
 */
export function applyLocalMessageEdit(
  message: RelayEvent,
  edit: LocalMessageEdit,
): RelayEvent {
  const editTags = [
    ...(edit.mediaTags ?? []),
    ...(edit.mentionPubkeys ?? []).map((pubkey) => ["p", pubkey]),
    ["buzz:mention-snapshot"],
  ];
  if (message.id === edit.eventId) {
    return {
      ...message,
      content: edit.content,
      tags: applyEditTagOverlay(message.tags, editTags),
    };
  }
  if (
    message.kind === KIND_STREAM_MESSAGE_EDIT &&
    editTargetId(message.tags) === edit.eventId
  ) {
    const structural = message.tags.filter(
      (tag) => tag[0] === "e" || tag[0] === "h",
    );
    return { ...message, content: edit.content, tags: [...editTags, ...structural] };
  }
  return message;
}
