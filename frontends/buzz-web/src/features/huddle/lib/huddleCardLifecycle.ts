import type { RelayEvent } from "@/shared/api/types";
import {
  KIND_HUDDLE_ENDED,
  KIND_HUDDLE_PARTICIPANT_JOINED,
  KIND_HUDDLE_PARTICIPANT_LEFT,
  KIND_HUDDLE_STARTED,
} from "@/shared/constants/kinds";
import { normalizePubkey } from "@/shared/lib/pubkey";

export type HuddleLifecycleState = {
  ended: boolean;
  participants: Set<string>;
};

export function parseEphemeralChannelId(content: string): string | null {
  try {
    const parsed = JSON.parse(content) as { ephemeral_channel_id?: unknown };
    return typeof parsed.ephemeral_channel_id === "string"
      ? parsed.ephemeral_channel_id
      : null;
  } catch {
    return null;
  }
}

function lifecycleParticipant(event: RelayEvent): string | null {
  return (
    event.tags.find(
      (tag) => tag[0] === "p" && typeof tag[1] === "string",
    )?.[1] ??
    event.pubkey ??
    null
  );
}

/**
 * A huddle card's state from its lifecycle events, accepting each event only
 * from the key allowed to publish it (finding_64bc3bfe6d0f41ca):
 *
 * - the START is the card's own message, never another member's START;
 * - joins and leaves are the relay's (it signs admissions);
 * - an END is the relay's, or the creator's while no second author has
 *   published a START for the same room (a contested creator cannot end it).
 *
 * Signatures are checked where frames arrive; this applies provenance.
 */
export function reconstructHuddleCardLifecycle(
  events: Iterable<RelayEvent>,
  options: {
    ephemeralChannelId: string;
    startEventId: string;
    creatorPubkey: string | undefined;
    relaySelfPubkey: string | null | undefined;
  },
): HuddleLifecycleState {
  const relaySelf = options.relaySelfPubkey
    ? normalizePubkey(options.relaySelfPubkey)
    : null;
  const creator = options.creatorPubkey
    ? normalizePubkey(options.creatorPubkey)
    : null;
  const sorted = [...events]
    .filter(
      (event) =>
        parseEphemeralChannelId(event.content) === options.ephemeralChannelId,
    )
    .sort(
      (left, right) =>
        left.created_at - right.created_at ||
        left.kind - right.kind ||
        left.id.localeCompare(right.id),
    );
  const contested = sorted.some(
    (event) =>
      event.kind === KIND_HUDDLE_STARTED &&
      normalizePubkey(event.pubkey) !== creator,
  );
  const participants = new Set<string>();
  let ended = false;

  if (options.creatorPubkey) {
    participants.add(options.creatorPubkey);
  }

  for (const event of sorted) {
    const signer = normalizePubkey(event.pubkey ?? "");
    switch (event.kind) {
      case KIND_HUDDLE_STARTED:
        if (event.id !== options.startEventId) break;
        ended = false;
        if (event.pubkey) participants.add(event.pubkey);
        break;
      case KIND_HUDDLE_PARTICIPANT_JOINED: {
        if (ended || !relaySelf || signer !== relaySelf) break;
        const pubkey = lifecycleParticipant(event);
        if (pubkey) participants.add(pubkey);
        break;
      }
      case KIND_HUDDLE_PARTICIPANT_LEFT: {
        if (ended || !relaySelf || signer !== relaySelf) break;
        const pubkey = lifecycleParticipant(event);
        if (pubkey) participants.delete(pubkey);
        break;
      }
      case KIND_HUDDLE_ENDED:
        if (
          (relaySelf !== null && signer === relaySelf) ||
          (!contested && creator !== null && signer === creator)
        ) {
          ended = true;
        }
        break;
    }
  }

  return { ended, participants };
}
