import type { InboxReply } from "@/features/home/lib/inbox";

export type LocalRepliesByItemId = Record<string, InboxReply[]>;

/**
 * Replies sent from the Inbox this session, shown until the relay's own copy
 * arrives. These keep that list honest: a deleted reply leaves it (otherwise
 * it reappeared once the timeline filtered the deleted event out), a reply
 * the relay has delivered leaves it for good, and an edit updates it.
 */
export function withoutLocalReply(
  replies: LocalRepliesByItemId,
  eventId: string,
): LocalRepliesByItemId {
  let changed = false;
  const next: LocalRepliesByItemId = {};
  for (const [itemId, list] of Object.entries(replies)) {
    const kept = list.filter((reply) => reply.id !== eventId);
    if (kept.length !== list.length) changed = true;
    next[itemId] = kept;
  }
  return changed ? next : replies;
}

export function withoutDeliveredLocalReplies(
  replies: LocalRepliesByItemId,
  deliveredIds: ReadonlySet<string>,
): LocalRepliesByItemId {
  let changed = false;
  const next: LocalRepliesByItemId = {};
  for (const [itemId, list] of Object.entries(replies)) {
    const kept = list.filter((reply) => !deliveredIds.has(reply.id));
    if (kept.length !== list.length) changed = true;
    next[itemId] = kept;
  }
  return changed ? next : replies;
}

export function withEditedLocalReply(
  replies: LocalRepliesByItemId,
  eventId: string,
  content: string,
): LocalRepliesByItemId {
  let changed = false;
  const next: LocalRepliesByItemId = {};
  for (const [itemId, list] of Object.entries(replies)) {
    next[itemId] = list.map((reply) => {
      if (reply.id !== eventId) return reply;
      changed = true;
      return { ...reply, content };
    });
  }
  return changed ? next : replies;
}
