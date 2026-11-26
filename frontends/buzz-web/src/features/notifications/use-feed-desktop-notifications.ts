import * as React from "react";

import { setLocalStorageItemWithRecovery } from "@/shared/lib/localStorageQuota";
import { truncateNpub } from "@/shared/lib/pubkey";
import { getStorageItem } from "@/shared/lib/safeStorage";
import {
  resolveUserLabel,
  type UserProfileLookup,
} from "@/features/profile/lib/identity";
import type { FeedItem, HomeFeedResponse } from "@/shared/api/types";
import {
  collectHomeAlertItems,
  eligibleFeedNotificationItems,
  formatFeedNotification,
  type NotificationChannel,
} from "./lib/feed";
import { buildFeedItemNotificationTarget } from "./lib/target";
import {
  getDesktopNotificationPermissionState,
  requestDesktopNotificationAccess,
  sendDesktopNotification,
} from "./lib/desktop";
import {
  playNotificationSound,
  resolveSlotSound,
  shouldPlayNotificationSound,
  slotForFeedKind,
} from "./lib/sound";
import type { NotificationSettings } from "./hooks";

// The home badge's seen set: mentions, needs-action and inbox rows the user has
// looked at on Home. Owned by useHomeFeedNotificationState (hooks.ts).
const HOME_FEED_SEEN_STORAGE_KEY = "buzz-home-feed-seen.v1";
// Desktop delivery's own record of alert items it has already considered, so a
// refresh does not notify twice. Kept apart from the badge's set: the two views
// differ (alerts are mentions + needs-action only), and when they shared one
// key every desktop write replaced the badge's inbox ids, so dismissed items
// counted as unread again on the next start.
const FEED_NOTIFIED_STORAGE_KEY = "buzz-feed-notified.v1";
const HOME_FEED_SEEN_MAX_ITEMS = 500;

function homeFeedSeenStorageKey(pubkey: string) {
  return `${HOME_FEED_SEEN_STORAGE_KEY}:${pubkey}`;
}

function feedNotifiedStorageKey(pubkey: string) {
  return `${FEED_NOTIFIED_STORAGE_KEY}:${pubkey}`;
}

function parseStoredIds(rawValue: string | null): string[] | null {
  if (!rawValue) {
    return null;
  }

  try {
    const parsed = JSON.parse(rawValue);
    if (!Array.isArray(parsed)) {
      return [];
    }

    return parsed
      .filter((value): value is string => typeof value === "string")
      .slice(-HOME_FEED_SEEN_MAX_ITEMS);
  } catch {
    return [];
  }
}

export function readStoredSeenFeedIds(pubkey: string): string[] {
  if (typeof window === "undefined" || pubkey.length === 0) {
    return [];
  }

  return (
    parseStoredIds(getStorageItem(homeFeedSeenStorageKey(pubkey))) ?? []
  );
}

export function writeStoredSeenFeedIds(pubkey: string, ids: string[]) {
  if (typeof window === "undefined" || pubkey.length === 0) {
    return;
  }

  setLocalStorageItemWithRecovery(
    homeFeedSeenStorageKey(pubkey),
    JSON.stringify(ids.slice(-HOME_FEED_SEEN_MAX_ITEMS)),
  );
}

/**
 * Alert ids desktop delivery has already considered. An older install holds
 * these ids under the badge's key, so a first read seeds from it: items the user
 * was already alerted to are not alerted again after upgrade.
 */
export function readStoredNotifiedFeedIds(pubkey: string): string[] {
  if (typeof window === "undefined" || pubkey.length === 0) {
    return [];
  }

  return (
    parseStoredIds(getStorageItem(feedNotifiedStorageKey(pubkey))) ??
    readStoredSeenFeedIds(pubkey)
  );
}

export function writeStoredNotifiedFeedIds(pubkey: string, ids: string[]) {
  if (typeof window === "undefined" || pubkey.length === 0) {
    return;
  }

  setLocalStorageItemWithRecovery(
    feedNotifiedStorageKey(pubkey),
    JSON.stringify(ids.slice(-HOME_FEED_SEEN_MAX_ITEMS)),
  );
}

/** Prevent unbounded growth — keep only the most recent entries. */
function trimSeenIds(ids: Set<string>): Set<string> {
  if (ids.size > HOME_FEED_SEEN_MAX_ITEMS) {
    const excess = ids.size - HOME_FEED_SEEN_MAX_ITEMS;
    let removed = 0;
    for (const id of ids) {
      if (removed >= excess) break;
      ids.delete(id);
      removed++;
    }
  }
  return ids;
}

export function useFeedDesktopNotifications(
  feed: HomeFeedResponse | undefined,
  pubkey: string | undefined,
  settings: NotificationSettings,
  setDesktopEnabled: (enabled: boolean) => Promise<boolean>,
  enabled: boolean,
  profiles?: UserProfileLookup,
  mutedChannelIds?: ReadonlySet<string>,
  channels: readonly NotificationChannel[] = [],
  silentChannelIds?: ReadonlySet<string>,
) {
  const normalizedPubkey = pubkey?.trim().toLowerCase() ?? "";
  // useState runs the initializer once; a useRef argument would re-read and
  // parse storage on every render.
  const [initialSeenItemIds] = React.useState(
    () => new Set(readStoredNotifiedFeedIds(normalizedPubkey)),
  );
  const seenItemIdsRef = React.useRef<Set<string>>(initialSeenItemIds);
  const hasInitializedFeedRef = React.useRef(false);
  const hasAutoRequestedRef = React.useRef(false);
  // The feed can still hold the previous account’s data for a render after
  // the pubkey changes. That feed object is remembered and skipped, so it is
  // neither recorded under the new account nor taken as its backlog.
  const latestFeedRef = React.useRef(feed);
  latestFeedRef.current = feed;
  const previousPubkeyRef = React.useRef(normalizedPubkey);
  const staleFeedRef = React.useRef<HomeFeedResponse | undefined>(undefined);

  React.useEffect(() => {
    if (previousPubkeyRef.current !== normalizedPubkey) {
      staleFeedRef.current = latestFeedRef.current;
      previousPubkeyRef.current = normalizedPubkey;
    }
    seenItemIdsRef.current = new Set(
      readStoredNotifiedFeedIds(normalizedPubkey),
    );
    hasInitializedFeedRef.current = false;
    hasAutoRequestedRef.current = false;
  }, [normalizedPubkey]);

  const autoRequestPermissionIfNeeded = React.useEffectEvent(async () => {
    if (hasAutoRequestedRef.current) {
      return;
    }

    const currentPermission = await getDesktopNotificationPermissionState();
    if (currentPermission !== "default") {
      return;
    }

    hasAutoRequestedRef.current = true;
    const result = await requestDesktopNotificationAccess();
    if (result !== "granted") {
      void setDesktopEnabled(false);
    }
  });

  const deliverFeedNotification = React.useEffectEvent(
    async (item: FeedItem, senderName?: string) => {
      const { title, body } = formatFeedNotification(item, senderName);
      const didSend = await sendDesktopNotification({
        body,
        target: buildFeedItemNotificationTarget(item),
        title,
      });

      if (
        didSend &&
        shouldPlayNotificationSound(item.channelId, silentChannelIds)
      ) {
        const slot = slotForFeedKind(item.kind, item.category);
        playNotificationSound(resolveSlotSound(settings, slot));
      }
    },
  );

  React.useEffect(() => {
    if (!enabled || !feed) {
      return;
    }
    if (staleFeedRef.current !== undefined) {
      if (feed === staleFeedRef.current) return;
      staleFeedRef.current = undefined;
    }

    const currentFeedItems = collectHomeAlertItems(feed);

    // Wait for sender profiles to load so notification titles include names.
    // Empty feeds do not need profiles; marking them initialized here keeps the
    // first later live alert from being mistaken for initial-load backlog.
    if (profiles === undefined && currentFeedItems.length > 0) {
      return;
    }

    const nextSeenItemIds = new Set(seenItemIdsRef.current);

    if (!hasInitializedFeedRef.current) {
      hasInitializedFeedRef.current = true;
      // The first feed after mount is backlog, not news: record it without
      // alerting. Union it into the stored history rather than replacing it,
      // so older items that reappear in a later feed are not alerted again.
      if (currentFeedItems.length > 0) {
        for (const item of currentFeedItems) {
          nextSeenItemIds.add(item.id);
        }
        seenItemIdsRef.current = trimSeenIds(nextSeenItemIds);
        writeStoredNotifiedFeedIds(normalizedPubkey, [
          ...seenItemIdsRef.current,
        ]);
      }
      return;
    }

    const newItems = settings.desktopEnabled
      ? eligibleFeedNotificationItems(
          feed,
          {
            mentions: settings.slotAlertsEnabled.mention,
            needsAction: settings.slotAlertsEnabled.needs_action,
          },
          channels,
        )
          .filter((item) => !nextSeenItemIds.has(item.id))
          .filter(
            (item) =>
              !item.channelId ||
              !mutedChannelIds?.has(item.channelId) ||
              item.category === "mention",
          )
      : [];

    for (const item of currentFeedItems) {
      nextSeenItemIds.add(item.id);
    }

    seenItemIdsRef.current = trimSeenIds(nextSeenItemIds);
    writeStoredNotifiedFeedIds(normalizedPubkey, [...seenItemIdsRef.current]);

    if (newItems.length > 0) {
      void autoRequestPermissionIfNeeded();
    }

    for (const item of newItems) {
      const resolvedLabel = profiles
        ? resolveUserLabel({
            pubkey: item.pubkey,
            profiles,
            preferResolvedSelfLabel: true,
          })
        : undefined;
      // Only use real display names, not truncated pubkey fallbacks.
      const senderName =
        resolvedLabel && resolvedLabel !== truncateNpub(item.pubkey)
          ? resolvedLabel
          : undefined;
      void deliverFeedNotification(item, senderName);
    }
  }, [
    enabled,
    feed,
    channels,
    mutedChannelIds,
    normalizedPubkey,
    profiles,
    settings.desktopEnabled,
    settings.slotAlertsEnabled.mention,
    settings.slotAlertsEnabled.needs_action,
  ]);
}
