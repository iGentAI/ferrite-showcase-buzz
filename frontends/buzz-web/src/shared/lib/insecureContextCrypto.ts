/**
 * SHA-256 and random UUIDs that also work in an insecure context.
 *
 * `crypto.subtle` and `crypto.randomUUID` are secure-context-only, so a bundle
 * served over plain http from a LAN host (or through a tunnel) sees both as
 * undefined, while `crypto.getRandomValues` is still there. Every NIP-98
 * header the tab signs carries a nonce, and every signed POST a payload
 * digest, so calling either API directly broke all authenticated relay HTTP
 * in that deployment while the socket kept working.
 *
 * The digest must be real SHA-256, not a stand-in: the relay recomputes the
 * NIP-98 payload hash and derives DM room ids from the same function. The
 * fallback is `@noble/hashes`, the implementation nostr-tools already signs
 * events with.
 */
import { sha256 as nobleSha256 } from "@noble/hashes/sha2.js";

type DigestApi = Pick<SubtleCrypto, "digest">;
type UuidApi = Pick<Crypto, "getRandomValues"> & { randomUUID?: () => string };

/** SHA-256 of `bytes`: `crypto.subtle` when present, `@noble/hashes` otherwise. */
export async function sha256Bytes(
  bytes: Uint8Array,
  subtle: DigestApi | undefined = globalThis.crypto?.subtle,
): Promise<Uint8Array> {
  if (!subtle) return nobleSha256(bytes);
  return new Uint8Array(await subtle.digest("SHA-256", bytes as BufferSource));
}

/** Lowercase hex SHA-256 of `bytes`. */
export async function sha256Hex(
  bytes: Uint8Array,
  subtle: DigestApi | undefined = globalThis.crypto?.subtle,
): Promise<string> {
  const digest = await sha256Bytes(bytes, subtle);
  return Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join(
    "",
  );
}

/**
 * An RFC 4122 v4 UUID: `crypto.randomUUID` when present, otherwise the same
 * form built from `crypto.getRandomValues`, which every context has.
 */
export function randomUuid(cryptoApi: UuidApi = globalThis.crypto): string {
  if (typeof cryptoApi.randomUUID === "function") return cryptoApi.randomUUID();
  const bytes = cryptoApi.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
