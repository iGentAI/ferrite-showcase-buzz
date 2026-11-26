/**
 * The channel WRITE path, browser edition.
 *
 * Reading is in `commands.ts` (`channelsFromRelay`), which builds the list from
 * the relay's own NIP-29 projections. Creating is here because it is the one
 * channel operation that is not a Nostr event: the relay owns room creation on
 * its HTTP face and then publishes the 39000/39002 the read path sees.
 */

import type { RawChannel } from "@/shared/api/tauriChannels";
import { randomUuid } from "@/shared/lib/insecureContextCrypto";

/**
 * The pinned id for a SHARED room (the starter channels): everyone in the
 * workspace derives the same slug, so a taken id means "someone else made it"
 * and the caller joins instead of creating a near-duplicate.
 */
export function channelIdFor(name: string): string {
  return (
    name
      .toLowerCase()
      .replace(/[^a-z0-9-_]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 64) || `room-${Date.now().toString(36)}`
  );
}

export type ChannelCreateInput = {
  name: string;
  channelType?: string;
  visibility?: string;
  description?: string;
  ttlSeconds?: number;
  /**
   * Pin the room's id. Only the starter rooms do this: they are shared by
   * everyone in the workspace, so a taken id means "someone else made it",
   * and the caller joins instead of creating a near-duplicate.
   */
  id?: string;
};

export function mintChannelId(
  cryptoApi: Pick<Crypto, "getRandomValues"> & { randomUUID?: () => string } = globalThis.crypto,
): string {
  return randomUuid(cryptoApi);
}

/**
 * The `POST /buzz/api/channels` body for one create.
 *
 * The relay keys a room by id, but buzz treats the NAME as a label — every new
 * member's private "Welcome" is called the same thing — so a per-user room must
 * never take its id from its name. Pinned ids (the shared starter rooms) pass
 * through unchanged, where a 409 stays meaningful ("someone else made it").
 *
 * Every other room gets a random UUID, which is upstream's own contract: the
 * desktop backend mints a v4 UUID for every channel it creates, and
 * `features/projects/projectModels.ts` `isValidProjectChannelId` accepts only a
 * UUID. A UUID also cannot collide, so there is no 409 to hide and no probe
 * round trip.
 */
export function channelCreateBody(
  input: ChannelCreateInput,
  mintId: () => string = () => mintChannelId(),
): Record<string, string> {
  const body: Record<string, string> = {
    name: input.name,
    // The route reads `type`, not `channel_type` (`ingress.rs: create_channel`);
    // the client's own field name would create every room as a stream.
    type: input.channelType ?? "stream",
    visibility: input.visibility ?? "open",
    id: input.id ?? mintId(),
  };
  if (input.description) body.description = input.description;
  if (input.ttlSeconds !== undefined) {
    body.ttl_seconds = String(input.ttlSeconds);
  }
  return body;
}

/**
 * Create a channel through the relay's HTTP face (`POST /buzz/api/channels`),
 * then answer with the row the UI expects. The relay publishes the 39000/39002
 * for it, so the next `get_channels` sees it from the same source as the rest.
 */
export async function createChannelViaRelay(
  input: ChannelCreateInput,
  mePubkey: string,
): Promise<RawChannel> {
  const { apiPost } = await import("@/shared/lib/api");
  const body = channelCreateBody(input);
  const reply = await apiPost<{
    channel?: {
      id?: string;
      description?: string;
      ttl_seconds?: number;
      ttl_deadline?: number;
    };
  }>("/channels", body);
  const id = reply.channel?.id ?? body.id;
  if (!id) {
    throw new Error("The relay did not return an id for the new channel.");
  }
  const row: RawChannel = {
    id,
    name: input.name,
    channel_type:
      input.channelType === "forum" ? "forum" : ("stream" as RawChannel["channel_type"]),
    visibility: (input.visibility as "open" | "private") ?? "open",
    description: reply.channel?.description ?? input.description ?? "",
    topic: null,
    purpose: null,
    member_count: 1,
    member_pubkeys: [mePubkey],
    last_message_at: null,
    archived_at: null,
    participants: [],
    participant_pubkeys: [mePubkey],
    is_member: true,
    ttl_seconds: reply.channel?.ttl_seconds ?? input.ttlSeconds ?? null,
    ttl_deadline:
      reply.channel?.ttl_deadline === undefined
        ? null
        : String(reply.channel.ttl_deadline),
  };
  rememberCreatedChannel(row);
  return row;
}

/** How long a created room is carried before the projection must have it. */
export const RECENTLY_CREATED_TTL_MS = 120_000;

const recentlyCreated = new Map<string, { row: RawChannel; createdAtMs: number }>();

/** Record a room the relay accepted (201) from this tab. */
export function rememberCreatedChannel(row: RawChannel, nowMs = Date.now()) {
  recentlyCreated.set(row.id, { row, createdAtMs: nowMs });
}

/** Stop carrying a room, e.g. once this tab deletes it. */
export function forgetCreatedChannel(id: string) {
  recentlyCreated.delete(id);
}

/**
 * The relay's channel projection, plus rooms this tab created that the
 * projection does not serve yet.
 *
 * `get_channels` is built from the relay's 39000/39002 projection, while the
 * desktop answers it from a local database its own create already wrote. The
 * create mutation invalidates the channel list at once, so under load its
 * refetch can reach the relay before the new room's projection is readable,
 * and the answer then REPLACES the cached list without the room. The relay had
 * accepted the create (201), so listing that room is the truth; the entry
 * retires as soon as the projection serves it, or after
 * `RECENTLY_CREATED_TTL_MS`.
 */
export function withRecentlyCreated<Row extends { id: string }>(
  rows: Row[],
  nowMs = Date.now(),
  store: Map<string, { row: RawChannel; createdAtMs: number }> = recentlyCreated,
): Array<Row | RawChannel> {
  const served = new Set(rows.map((row) => row.id));
  const carried: RawChannel[] = [];
  for (const [id, entry] of store) {
    if (served.has(id) || nowMs - entry.createdAtMs > RECENTLY_CREATED_TTL_MS) {
      store.delete(id);
      continue;
    }
    carried.push(entry.row);
  }
  return [...rows, ...carried];
}
