import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";

import { relayClient } from "@/shared/api/relayClient";
import type { RelayEvent } from "@/shared/api/types";

/** NIP-29 relay-signed room projections: metadata and members. */
const KIND_GROUP_METADATA = 39000;
const KIND_GROUP_MEMBERS = 39002;

/** Coalesce a burst of projections (a rename re-publishes both) into one refetch. */
const INVALIDATE_DEBOUNCE_MS = 750;

/**
 * The query keys one live room projection makes stale.
 *
 * The browser seam answers `get_channels` and `get_channel_details` from the
 * relay's 39000/39002 projection, so a fresh projection means those answers
 * changed: the list (a rename, an archive, a new member) and the room's detail
 * and member reads. Anything else is not a room projection and changes nothing.
 */
export function roomProjectionInvalidations(
  event: Pick<RelayEvent, "kind" | "tags">,
): Array<readonly unknown[]> {
  if (event.kind !== KIND_GROUP_METADATA && event.kind !== KIND_GROUP_MEMBERS) {
    return [];
  }
  const room = event.tags.find((tag) => tag[0] === "d")?.[1];
  const keys: Array<readonly unknown[]> = [["channels"]];
  if (room) {
    keys.push(["channels", room, "detail"], ["channels", room, "members"]);
  }
  return keys;
}

/**
 * Apply live room projections to the channel caches.
 *
 * Without this a member learned of a rename or an archive only on the next
 * 60 s channel-list poll, while the relay had already re-published the room's
 * 39000. Subscribes for projections newer than now; the relay's access gate
 * decides which rooms this identity receives.
 */
export function useLiveRoomProjections(currentPubkey?: string) {
  const queryClient = useQueryClient();
  const enabled = Boolean(currentPubkey?.trim());

  React.useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    let dispose: (() => Promise<void>) | undefined;
    const pending = new Map<string, readonly unknown[]>();
    let timer: number | undefined;

    const flush = () => {
      timer = undefined;
      const keys = [...pending.values()];
      pending.clear();
      for (const queryKey of keys) {
        void queryClient.invalidateQueries({ queryKey });
      }
    };

    void relayClient
      .subscribeLive(
        {
          kinds: [KIND_GROUP_METADATA, KIND_GROUP_MEMBERS],
          limit: 50,
          since: Math.floor(Date.now() / 1_000),
        },
        (event) => {
          if (cancelled) return;
          for (const key of roomProjectionInvalidations(event)) {
            pending.set(JSON.stringify(key), key);
          }
          if (pending.size > 0 && timer === undefined) {
            timer = window.setTimeout(flush, INVALIDATE_DEBOUNCE_MS);
          }
        },
      )
      .then((next) => {
        if (cancelled) {
          void next().catch(() => {});
        } else {
          dispose = next;
        }
      })
      .catch((error: unknown) => {
        // The 60 s poll still converges; a failed live subscription only costs
        // latency, so it is reported rather than retried in a loop.
        console.error("Failed to subscribe to live room projections", error);
      });

    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
      if (dispose) void dispose().catch(() => {});
    };
  }, [enabled, queryClient]);
}
