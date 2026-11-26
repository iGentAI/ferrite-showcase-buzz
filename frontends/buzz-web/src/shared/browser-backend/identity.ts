/**
 * The ONE key this tab signs with.
 *
 * There are two signing paths in the client — the Tauri-command path
 * (`browser-backend/commands.ts`, which signs everything that rides the
 * WebSocket) and `shared/lib/nostr-signer.ts` (which signs NIP-98 headers for
 * the relay's HTTP routes). They must resolve to the SAME identity: otherwise a
 * room created over HTTP is owned by a key the socket never authenticated as,
 * so the tab is not a member of the channel it just created and the UI offers
 * it a "Join" button.
 *
 * A NIP-07 extension, when installed, owns the identity outright and this key
 * is never used. Otherwise the key is persisted in `localStorage` so a reload
 * keeps the same person — an in-memory key would orphan every membership the
 * user created before refreshing.
 */
import { generateSecretKey, getPublicKey } from "nostr-tools/pure";
import { decode } from "nostr-tools/nip19";

const STORAGE_KEY = "buzz-web:sk";

export function hexToBytes(hex: string): Uint8Array {
  return new Uint8Array((hex.match(/.{2}/g) ?? []).map((b) => parseInt(b, 16)));
}

export function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

export function localSecretKey(): Uint8Array {
  const stored = window.localStorage.getItem(STORAGE_KEY);
  if (stored && /^[0-9a-f]{64}$/i.test(stored)) {
    return hexToBytes(stored);
  }
  const fresh = generateSecretKey();
  window.localStorage.setItem(STORAGE_KEY, bytesToHex(fresh));
  return fresh;
}

/** Persist a validated 32-byte secret key as the browser-held identity. */
export function setLocalSecretKey(secretKey: Uint8Array): Uint8Array {
  if (!(secretKey instanceof Uint8Array) || secretKey.length !== 32) {
    throw new Error("Enter a valid 32-byte Nostr secret key.");
  }
  getPublicKey(secretKey);
  window.localStorage.setItem(STORAGE_KEY, bytesToHex(secretKey));
  return secretKey;
}

/** Replace the browser-held identity with an explicitly imported nsec. */
export function importLocalIdentity(nsec: string): Uint8Array {
  const decoded = decode(nsec.trim());
  if (
    decoded.type !== "nsec" ||
    !(decoded.data instanceof Uint8Array) ||
    decoded.data.length !== 32
  ) {
    throw new Error("Enter a valid Nostr secret key beginning with nsec1.");
  }
  return setLocalSecretKey(decoded.data);
}

/**
 * Forget the browser-held key.
 *
 * The whole of signing out in this build: there is no session on a server to
 * end, no token to revoke and no OS keychain to lock — the key IS the account,
 * and a tab that no longer holds one is signed out.
 *
 * Irreversible without the nsec, which is why the backup screen exists. Callers
 * are expected to have warned first; this function's job is to actually forget,
 * not to ask.
 */
export function clearLocalIdentity(): void {
  window.localStorage.removeItem(STORAGE_KEY);
}
