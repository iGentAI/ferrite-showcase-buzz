import type { InboxItem } from "@/features/home/lib/inbox";
import { canManageMessageForCurrentUser } from "@/features/messages/lib/canManageMessage";
import type { TimelineMessage } from "@/features/messages/types";
import type { UserProfileLookup } from "@/features/profile/lib/identity";
import type { FeedItem } from "@/shared/api/types";

type DeleteTargetCandidate = Pick<
  FeedItem,
  "channelId" | "id" | "kind" | "pubkey"
>;

export function getHomeMessageCapabilities(
  item: InboxItem | null,
  currentPubkey: string | undefined,
  availableChannelIds: ReadonlySet<string>,
  options?: {
    channelArchivedAt?: number | string | null;
    profiles?: UserProfileLookup;
    targetMessage?: DeleteTargetCandidate | null;
  },
) {
  const isArchived = (options?.channelArchivedAt ?? null) !== null;
  const canReact = Boolean(
    item?.item.channelId &&
      availableChannelIds.has(item.item.channelId) &&
      !isArchived,
  );
  const canReply =
    canReact && item?.item.kind !== 45001 && item?.item.kind !== 45003;
  const disabledReplyReason =
    canReply || !item
      ? null
      : isArchived
        ? "This channel is archived."
        : item.item.channelId
          ? availableChannelIds.has(item.item.channelId)
            ? "This item does not support inline replies yet."
            : "Open the linked channel to reply."
          : "This inbox item does not have a reply target.";

  const deleteCandidate =
    options?.targetMessage !== undefined
      ? options.targetMessage
      : (item?.item ?? null);
  const canDelete = Boolean(
    item !== null &&
      canReact &&
      deleteCandidate !== null &&
      deleteCandidate.channelId === item.item.channelId &&
      canManageMessageForCurrentUser(
        {
          kind: deleteCandidate.kind,
          pubkey: deleteCandidate.pubkey,
        } as TimelineMessage,
        currentPubkey,
        options?.profiles,
      ),
  );

  return {
    canDelete,
    deleteTargetId: canDelete && deleteCandidate ? deleteCandidate.id : null,
    canReact,
    canReply,
    disabledReplyReason,
  };
}
