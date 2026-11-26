import type { RelayEvent } from "@/shared/api/types";
import {
  KIND_HUDDLE_ENDED,
  KIND_HUDDLE_PARTICIPANT_JOINED,
  KIND_HUDDLE_PARTICIPANT_LEFT,
  KIND_HUDDLE_STARTED,
} from "@/shared/constants/kinds";
import { normalizePubkey } from "@/shared/lib/pubkey";

export type ActiveHuddleState = {
  ephemeralChannelId: string;
  huddleThreadEventId: string | null;
  participants: Set<string>;
};

/**
 * The channel header's active huddle, reconstructed from lifecycle events
 * sorted by time, kind and id, and accepting each event only from the key
 * allowed to publish it (finding_a3b8dc61cbfebee2):
 *
 * - joins and leaves are the relay's; a leave never invents a huddle;
 * - a START opens a huddle only when none is live, so a later START cannot
 *   take over a live session, its thread or its creator's authority, and a
 *   second author's START for the same room revokes the creator's END;
 * - an END is the relay's, or the uncontested creator's.
 *
 * Missing starts are still tolerated: relay joins for a room imply it exists.
 * Signatures are checked where frames arrive; this applies provenance.
 */
export function reconstructActiveHuddle(
  events: Iterable<RelayEvent>,
  relaySelfPubkey: string | null | undefined,
): ActiveHuddleState | null {
  const relaySelf = relaySelfPubkey ? normalizePubkey(relaySelfPubkey) : null;
  const sorted = [...events].sort(
    (a, b) =>
      a.created_at - b.created_at || a.kind - b.kind || a.id.localeCompare(b.id),
  );

  let huddle: ActiveHuddleState | null = null;
  const endedChannels = new Set<string>();
  const creators = new Map<string, string>();
  const contested = new Set<string>();

  for (const ev of sorted) {
    let ephId: string | null = null;
    try {
      const content = JSON.parse(ev.content);
      ephId =
        typeof content?.ephemeral_channel_id === "string"
          ? content.ephemeral_channel_id
          : null;
    } catch {
      continue;
    }
    if (!ephId) continue;
    const signer = normalizePubkey(ev.pubkey ?? "");
    const fromRelay = relaySelf !== null && signer === relaySelf;

    switch (ev.kind) {
      case KIND_HUDDLE_STARTED: {
        const creator = creators.get(ephId);
        if (creator !== undefined && creator !== signer) contested.add(ephId);
        if (creator === undefined && signer) creators.set(ephId, signer);
        const live =
          huddle !== null &&
          huddle.huddleThreadEventId !== null &&
          !endedChannels.has(huddle.ephemeralChannelId);
        if (live) break;
        if (huddle && huddle.ephemeralChannelId === ephId) {
          // A room inferred from relay joins gains its thread.
          if (creators.get(ephId) === signer) huddle.huddleThreadEventId = ev.id;
          break;
        }
        if (creators.get(ephId) !== signer) break;
        endedChannels.delete(ephId);
        huddle = {
          ephemeralChannelId: ephId,
          huddleThreadEventId: ev.id,
          participants: new Set([ev.pubkey]),
        };
        break;
      }
      case KIND_HUDDLE_PARTICIPANT_JOINED: {
        if (!fromRelay || endedChannels.has(ephId)) break;
        const joined = ev.tags.find((t) => t[0] === "p")?.[1] ?? ev.pubkey;
        if (!huddle || ephId !== huddle.ephemeralChannelId) {
          huddle = {
            ephemeralChannelId: ephId,
            huddleThreadEventId: null,
            participants: new Set(),
          };
        }
        huddle.participants.add(joined);
        break;
      }
      case KIND_HUDDLE_PARTICIPANT_LEFT: {
        if (!fromRelay || endedChannels.has(ephId)) break;
        if (!huddle || ephId !== huddle.ephemeralChannelId) break;
        const left = ev.tags.find((t) => t[0] === "p")?.[1] ?? ev.pubkey;
        huddle.participants.delete(left);
        break;
      }
      case KIND_HUDDLE_ENDED: {
        const creatorMayEnd =
          !contested.has(ephId) &&
          signer !== null &&
          creators.get(ephId) === signer;
        if (!fromRelay && !creatorMayEnd) break;
        endedChannels.add(ephId);
        if (huddle && ephId === huddle.ephemeralChannelId) huddle = null;
        break;
      }
    }
  }
  return huddle;
}
