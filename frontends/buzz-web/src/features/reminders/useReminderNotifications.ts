import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";

import {
  remindersQueryKey,
  useRemindersQuery,
} from "@/features/reminders/hooks";
import { dueSince } from "@/features/reminders/lib/reminderFilters";
import { normalizeRelayUrl } from "@/shared/lib/normalizeRelayUrl";
import type { Reminder } from "@/features/reminders/lib/reminderTypes";
import {
  requestDockBounce,
  sendDesktopNotification,
} from "@/features/notifications/lib/desktop";
import type { NotificationSettings } from "@/features/notifications/hooks";
import { startReminderNotificationPoll } from "@/features/reminders/lib/reminderNotificationPoll";
import {
  formatNotificationTitle,
  resolveNotificationChannelLabel,
  truncateNotificationBody,
} from "@/features/notifications/lib/notificationFormat";
import {
  playNotificationSound,
  resolveSlotSound,
} from "@/features/notifications/lib/sound";

const WATERMARK_STORAGE_PREFIX = "buzz:lastReminderCheck:";

/**
 * The watermark is per account AND per community: reminders live on, and are
 * polled from, the active community's relay only, so one shared watermark
 * advanced while viewing community A skipped reminders that came due on B.
 * A pre-scoping key (account only) seeds a community's first read.
 */
export function watermarkStorageKey(pubkey: string, relayUrl?: string): string {
  const account = `${WATERMARK_STORAGE_PREFIX}${pubkey.trim().toLowerCase()}`;
  return relayUrl
    ? `${account}:${encodeURIComponent(normalizeRelayUrl(relayUrl))}`
    : account;
}

/**
 * Read the persisted watermark, seeding it to `now` on first-ever launch.
 * Seeding to `now` (not 0) is deliberate: a 0 seed would replay the user's
 * entire reminder history as toasts. A reminder already due at first launch
 * fails the strict `notBefore > watermark` test and surfaces only in the
 * panel/badge, never as a toast — see the plan's behavioral note.
 */
export function readWatermark(pubkey: string, relayUrl?: string): number {
  const key = watermarkStorageKey(pubkey, relayUrl);
  const stored =
    window.localStorage.getItem(key) ??
    (relayUrl ? window.localStorage.getItem(watermarkStorageKey(pubkey)) : null);
  if (stored !== null) {
    const parsed = Number(stored);
    if (Number.isFinite(parsed)) {
      window.localStorage.setItem(key, String(parsed));
      return parsed;
    }
  }
  const now = Math.floor(Date.now() / 1_000);
  window.localStorage.setItem(key, String(now));
  return now;
}

/**
 * App-level fire-on-due detection. On launch and every {@link POLL_INTERVAL_MS}
 * it fires reminders newly crossing their `not_before` since the persisted
 * watermark, coalescing multiple due reminders into one toast, then advances
 * the watermark. The toast respects `desktopEnabled` + the `needs_action`
 * alert slot. This hook is the sole detector — mount it once at app level.
 */
export function useReminderNotifications(
  pubkey: string | undefined,
  settings: NotificationSettings,
  channels: ReadonlyArray<{ id: string; name?: string | null }>,
  relayUrl?: string,
): void {
  const reminders = useRemindersQuery(pubkey).data;
  const queryClient = useQueryClient();
  const remindersRef = React.useRef<Reminder[]>([]);
  remindersRef.current = reminders ?? [];
  const settingsRef = React.useRef(settings);
  settingsRef.current = settings;
  const channelsRef = React.useRef(channels);
  channelsRef.current = channels;

  // Track whether the query has resolved at least once. On mount,
  // useRemindersQuery is still loading (data === undefined), so
  // remindersRef.current is []. Without this guard, check() would advance
  // the watermark past any reminders that came due while the app was closed.
  // It follows the current data rather than latching: after an account or
  // community switch the new query starts unresolved again.
  const queryResolvedRef = React.useRef(false);
  queryResolvedRef.current = reminders !== undefined;

  const fire = React.useEffectEvent((due: Reminder[]) => {
    const current = settingsRef.current;
    if (
      !current.desktopEnabled ||
      !current.slotAlertsEnabled.needs_action ||
      due.length === 0
    ) {
      return;
    }

    // For a single reminder, try to resolve the channel name from its target.
    // Multiple reminders may span different channels, so we omit channel context
    // in that case and let the body count speak for itself.
    const channelLabel =
      due.length === 1
        ? resolveNotificationChannelLabel(
            due[0].content.target?.channelId ?? null,
            channelsRef.current,
          )
        : null;

    const body =
      due.length === 1
        ? truncateNotificationBody(
            due[0].content.target?.preview ?? due[0].content.note ?? "",
            "A reminder is waiting",
          )
        : `${due.length} reminders are due`;

    void sendDesktopNotification({
      title: formatNotificationTitle({ prefix: "Reminder due", channelLabel }),
      body,
    }).then((didSend) => {
      if (!didSend) return;
      playNotificationSound(resolveSlotSound(current, "needs_action"));
      void requestDockBounce();
    });
  });

  React.useEffect(() => {
    if (!pubkey) return;

    const check = () => {
      // Defer until the query has resolved at least once — an empty array from
      // an unresolved query must not advance the watermark past reminders that
      // came due while the app was closed (the "missed-while-asleep" window).
      if (remindersRef.current.length === 0 && !queryResolvedRef.current)
        return;

      const watermark = readWatermark(pubkey, relayUrl);
      const now = Math.floor(Date.now() / 1_000);
      const due = dueSince(remindersRef.current, watermark, now);
      fire(due);
      // Advance unconditionally, even when fire() suppressed the toast
      // (notifications off or needs_action slot muted). Re-enabling later must
      // not backlog-replay reminders that came due while muted — same no-replay
      // rationale as seed-to-now. Suppressed reminders still show in panel/badge.
      window.localStorage.setItem(
        watermarkStorageKey(pubkey, relayUrl),
        String(now),
      );
      // Liveness tick: re-render every countDue consumer (inbox nav badge,
      // HomeView filter, panel) so a reminder that crossed notBefore while the
      // app sat idle surfaces within the poll interval. Safe to run after the
      // watermark advance — the toast check() fires on this hook's own
      // setInterval, not on query-data change, so the refetch cannot re-fire it.
      void queryClient.invalidateQueries({
        queryKey: remindersQueryKey(pubkey),
      });
    };

    return startReminderNotificationPoll(check);
  }, [pubkey, queryClient, relayUrl]);
}
