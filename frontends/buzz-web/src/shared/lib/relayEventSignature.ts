import { verifyEvent } from "nostr-tools/pure";

import type { RelayEvent } from "@/shared/api/types";

/**
 * Whether a relay-delivered event's id and Schnorr signature verify.
 *
 * `relayClient.fetchEvents` and `subscribeLive` hand back whatever the relay
 * framed without checking signatures, so a feature that trusts an event's
 * `pubkey` (its own blobs, relay-signed lifecycle events) must check here
 * first. A fresh object is verified so a cached verification marker on the
 * input can never stand in for the check.
 */
export function hasValidRelayEventSignature(event: RelayEvent): boolean {
  if (!event || typeof event !== "object") return false;
  try {
    return verifyEvent({
      id: event.id,
      pubkey: event.pubkey,
      created_at: event.created_at,
      kind: event.kind,
      tags: event.tags,
      content: event.content,
      sig: event.sig,
    });
  } catch {
    return false;
  }
}
