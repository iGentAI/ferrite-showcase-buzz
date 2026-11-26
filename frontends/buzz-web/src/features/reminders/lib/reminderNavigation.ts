import { getThreadReference } from "@/features/messages/lib/threading";
import type { ReminderTarget } from "@/features/reminders/lib/reminderTypes";
import { getEventById } from "@/shared/api/tauri";
import {
  KIND_FORUM_COMMENT,
  KIND_FORUM_POST,
} from "@/shared/constants/kinds";

/**
 * Where a reminder click should land, mirroring the search-hit destination: a
 * message inside a channel (optionally inside a thread), or a forum post,
 * optionally scrolled to one of its comments. A forum channel ignores a
 * message id, so a forum target routed as a channel message landed on the
 * forum index instead of the post.
 */
export type ReminderDestination =
  | {
      kind: "channel";
      channelId: string;
      messageId: string;
      threadRootId: string | null;
    }
  | {
      kind: "forum-post";
      channelId: string;
      postId: string;
      replyId?: string;
    };

/** Open a resolved reminder destination with the app's navigation. */
export function openReminderDestination(
  destination: ReminderDestination,
  navigation: {
    goChannel: (
      channelId: string,
      options: { messageId: string; threadRootId: string | null },
    ) => unknown;
    goForumPost: (
      channelId: string,
      postId: string,
      options?: { replyId?: string },
    ) => unknown;
  },
): void {
  if (destination.kind === "forum-post") {
    void navigation.goForumPost(
      destination.channelId,
      destination.postId,
      destination.replyId ? { replyId: destination.replyId } : undefined,
    );
    return;
  }
  void navigation.goChannel(destination.channelId, {
    messageId: destination.messageId,
    threadRootId: destination.threadRootId,
  });
}

/**
 * A target is navigable only when it carries a non-empty channelId, eventId,
 * and authorPubkey. The creation site stores `channelId ?? ""` /
 * `authorPubkey ?? ""`, so a *present* target can still hold empty strings —
 * those route to `/channels/` with an empty param or render a meaningless
 * author, and must be treated as non-navigable, same as note-only reminders
 * (no target at all).
 */
export function hasNavigableTarget(
  target: ReminderTarget | undefined,
): target is ReminderTarget {
  return (
    target !== undefined &&
    target.channelId !== "" &&
    target.eventId !== "" &&
    target.authorPubkey !== ""
  );
}

/**
 * Resolves the in-thread destination for a reminder target.
 *
 * Reminder targets store no thread context, so — like the forum-comment branch
 * of `resolveSearchHitDestination` — we fetch the target event and derive its
 * thread root from the tags. A top-level (non-reply) message yields a null root
 * (channel-level, no thread to enter); a fetch failure degrades the same way.
 *
 * Returns null when the target is non-navigable (absent or empty fields).
 */
export async function resolveReminderDestination(
  target: ReminderTarget | undefined,
  fetchEvent: typeof getEventById = getEventById,
): Promise<ReminderDestination | null> {
  if (!hasNavigableTarget(target)) {
    return null;
  }

  try {
    const event = await fetchEvent(target.eventId);
    if (event.kind === KIND_FORUM_POST) {
      return {
        kind: "forum-post",
        channelId: target.channelId,
        postId: target.eventId,
      };
    }
    if (event.kind === KIND_FORUM_COMMENT) {
      const thread = getThreadReference(event.tags);
      // A direct comment carries only a root-marked `e` tag, which the
      // thread reference (built for chat replies) does not report.
      const postId =
        thread.rootId ??
        thread.parentId ??
        event.tags.find((tag) => tag[0] === "e" && tag[3] === "root")?.[1] ??
        event.tags.find((tag) => tag[0] === "e")?.[1];
      if (postId) {
        return {
          kind: "forum-post",
          channelId: target.channelId,
          postId,
          replyId: target.eventId,
        };
      }
    }
    return {
      kind: "channel",
      channelId: target.channelId,
      messageId: target.eventId,
      threadRootId: getThreadReference(event.tags).rootId,
    };
  } catch (error) {
    console.error(
      "Failed to resolve reminder thread destination",
      target.eventId,
      error,
    );
    return {
      kind: "channel",
      channelId: target.channelId,
      messageId: target.eventId,
      threadRootId: null,
    };
  }
}
