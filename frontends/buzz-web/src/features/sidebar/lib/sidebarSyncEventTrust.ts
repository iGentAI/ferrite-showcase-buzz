import type { RelayEvent } from "@/shared/api/types";
import { hasValidRelayEventSignature } from "@/shared/lib/relayEventSignature";

/**
 * Trust rules for the user's own encrypted sidebar-preference blobs
 * (sections, sort, stars, mutes) as they come back from a relay.
 *
 * NIP-44 self-encryption stops a relay from forging a blob's content, but the
 * metadata the sync managers act on — `pubkey`, `created_at`, `id` — is plain
 * JSON the relay hands back. Neither `relayClient.fetchEvents` nor
 * `subscribeLive` checks signatures, so a manager must do it before it lets an
 * event move its watermark, its last-writer-wins ordering, or its
 * found/absent/failed decision.
 */

/**
 * How far ahead of this device's clock a relay-supplied `created_at` may be
 * before it is treated as implausible.
 *
 * The buzz relay refuses events whose `created_at` is more than 900 seconds
 * from its own clock; 600 seconds leaves room for this device's clock to be
 * off in either direction while still keeping every publish we stamp inside
 * the relay's window.
 */
export const MAX_SYNC_CLOCK_SKEW_SECS = 600;

export function currentUnixSeconds(): number {
  return Math.floor(Date.now() / 1_000);
}

/** A positive integer timestamp that is not implausibly far in the future. */
export function isPlausibleSyncTimestamp(
  createdAt: unknown,
  now: number = currentUnixSeconds(),
): createdAt is number {
  return (
    typeof createdAt === "number" &&
    Number.isSafeInteger(createdAt) &&
    createdAt > 0 &&
    createdAt <= now + MAX_SYNC_CLOCK_SKEW_SECS
  );
}

/** The replaceable-event address a sync manager reads and writes. */
export type SyncBlobAddress = { kind: number; dTag: string };

/**
 * Whether `event` is a genuine event of `pubkey` for the blob at `address`:
 * the author, kind and `d` tag match, the timestamp is plausible, and the id
 * and Schnorr signature verify. The address matters because every sidebar
 * blob shares kind 30078: a relay could otherwise answer with another of the
 * user's genuinely signed events to move a watermark or swap in another blob.
 */
export function isAuthenticOwnSyncEvent(
  event: RelayEvent | undefined,
  pubkey: string,
  address: SyncBlobAddress,
  now: number = currentUnixSeconds(),
): event is RelayEvent {
  if (!event || typeof event !== "object") return false;
  if (event.pubkey !== pubkey) return false;
  if (event.kind !== address.kind) return false;
  const dTag = Array.isArray(event.tags)
    ? event.tags.find((tag) => Array.isArray(tag) && tag[0] === "d")?.[1]
    : undefined;
  if (dTag !== address.dTag) return false;
  if (!isPlausibleSyncTimestamp(event.created_at, now)) return false;
  return hasValidRelayEventSignature(event);
}

/**
 * The `created_at` to stamp on a publish: strictly after the newest head we
 * have seen, and never further ahead of the local clock than the skew bound.
 *
 * Returns null when no such timestamp exists yet (the head is already at the
 * bound, e.g. just after a poisoned watermark was capped). Replaceable events
 * with equal timestamps are ordered by event id, not by edit order, so the
 * publish must wait rather than reuse the head's timestamp.
 */
export function syncPublishCreatedAt(
  lastRemoteCreatedAt: number,
  now: number = currentUnixSeconds(),
): number | null {
  const createdAt = Math.max(now, lastRemoteCreatedAt + 1);
  return createdAt <= now + MAX_SYNC_CLOCK_SKEW_SECS ? createdAt : null;
}

/** How long to wait before a publish that `syncPublishCreatedAt` deferred. */
export function syncPublishRetryDelayMs(
  lastRemoteCreatedAt: number,
  now: number = currentUnixSeconds(),
): number {
  const waitSecs = lastRemoteCreatedAt + 1 - (now + MAX_SYNC_CLOCK_SKEW_SECS);
  return Math.max(1, waitSecs) * 1_000;
}
