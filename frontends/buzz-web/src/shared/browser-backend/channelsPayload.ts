/**
 * The `get_channels` answer, in upstream's shape.
 *
 * block/buzz's desktop command (`desktop/src-tauri/src/commands/channels.rs`
 * `get_channels`) answers `{ hash, channels, last_messages }`: `channels` is
 * `null` when the caller's `knownHash` equals the list's stable hash, and
 * `last_messages` (channel id -> ISO timestamp of its latest message) always
 * ships. The vendored client reads exactly that (`shared/api/tauriChannels.ts`
 * `getChannels`), so a bare row array made it throw
 * `Cannot read properties of undefined (reading 'map')`. That broke the sidebar
 * channel list and starter-channel onboarding for every new member of a relay-served
 * bundle.
 *
 * The hash excludes `last_message_at`, as upstream's `compute_channels_hash`
 * does, so a new message refreshes timestamps without re-sending the list.
 */
export type ChannelsPayload<Row> = {
  hash: string;
  channels: Row[] | null;
  last_messages: Record<string, string>;
};

/** The row fields this answer reads. */
export type ChannelsPayloadRow = {
  id: string;
  last_message_at: string | null;
};

const HASH_HEX_CHARS = 16;

/** Build the payload for `rows`, honouring a matching `knownHash`. */
export async function channelsPayload<Row extends ChannelsPayloadRow>(
  rows: Row[],
  knownHash: unknown,
): Promise<ChannelsPayload<Row>> {
  const lastMessages: Record<string, string> = {};
  for (const row of rows) {
    if (row.last_message_at) lastMessages[row.id] = row.last_message_at;
  }
  const hash = await stableChannelsHash(rows);
  return {
    hash,
    channels: typeof knownHash === "string" && knownHash === hash ? null : rows,
    last_messages: lastMessages,
  };
}

/**
 * SHA-256 over the rows with `last_message_at` removed, first 16 hex chars.
 *
 * `crypto.subtle` exists only in a secure context. Served over plain http from
 * a LAN host it is undefined, and throwing here emptied the sidebar. The hash
 * only lets the client skip re-sending an unchanged list to ITSELF, so in that
 * case a deterministic non-cryptographic digest ({@link fallbackChannelsDigest})
 * does the same job; the 16-hex shape is kept.
 */
export async function stableChannelsHash(
  rows: ChannelsPayloadRow[],
  subtle: Pick<SubtleCrypto, "digest"> | undefined = globalThis.crypto?.subtle,
): Promise<string> {
  const stable = rows.map(({ last_message_at: _ignored, ...rest }) => rest);
  const text = JSON.stringify(stable);
  if (!subtle) return fallbackChannelsDigest(text);
  const bytes = new TextEncoder().encode(text);
  const digest = new Uint8Array(await subtle.digest("SHA-256", bytes));
  return Array.from(digest, (byte) => byte.toString(16).padStart(2, "0"))
    .join("")
    .slice(0, HASH_HEX_CHARS);
}

/** Two independent 32-bit FNV-1a passes over the UTF-8 bytes, as 16 hex chars. */
export function fallbackChannelsDigest(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let left = 0x811c9dc5;
  let right = 0x01000193 ^ bytes.length;
  for (const byte of bytes) {
    left = Math.imul(left ^ byte, 0x01000193) >>> 0;
    right = Math.imul(right ^ ((byte + 0x5b) & 0xff), 0x01000193) >>> 0;
  }
  return left.toString(16).padStart(8, "0") + right.toString(16).padStart(8, "0");
}
