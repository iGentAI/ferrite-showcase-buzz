/**
 * Encrypted identity backup (NIP-49), browser edition.
 *
 * The desktop app encrypts the keychain's nsec with a passphrase and hands
 * back an `ncryptsec1…` string. This tab holds its own key
 * (`browser-backend/identity`), and NIP-49 is pure crypto — scrypt +
 * XChaCha20-Poly1305, shipped by nostr-tools — so refusing these commands
 * would be a false reason. The functions here are PURE: the caller passes the
 * secret key in, which is what keeps them unit-testable without a DOM or a
 * localStorage stub.
 *
 * `saveNcryptsecCopy` is the one browser-shaped member: the desktop opens a
 * save-file dialog and returns the chosen path (null on cancel); a tab
 * triggers a download, which has no cancel signal, so the answer is the
 * suggested filename.
 */
import { decrypt, encrypt } from "nostr-tools/nip49";
import { getPublicKey } from "nostr-tools/pure";
import { npubEncode } from "nostr-tools/nip19";

/** The shape `verify_ncryptsec_backup` answers with (tauriIdentity.ts). */
export type BackupVerification = {
  pubkey: string;
  npub: string;
  matchesCurrentIdentity: boolean;
};

/**
 * Unambiguous lowercase alphabet: no l/o/0/1, 32 symbols, so each character
 * carries exactly 5 bits and a passphrase can be read back over the phone.
 */
const PASSPHRASE_ALPHABET = "abcdefghijkmnpqrstuvwxyz23456789";
const TOKEN_LENGTH = 4;
const DEFAULT_WORDS = 6;

/**
 * A random passphrase of word-like tokens: `words` × 4 characters × 5 bits =
 * 120 bits at the default 6. Uses rejection-free sampling — the alphabet's 32
 * symbols divide 256 evenly, so a byte mod 32 is uniform.
 */
export function generatePassphrase(options?: {
  words?: number;
  separator?: string;
}): string {
  const words = Math.min(Math.max(Math.floor(options?.words ?? DEFAULT_WORDS), 4), 12);
  const separator = options?.separator ?? "-";
  const bytes = new Uint8Array(words * TOKEN_LENGTH);
  crypto.getRandomValues(bytes);
  const tokens: string[] = [];
  for (let word = 0; word < words; word += 1) {
    let token = "";
    for (let at = 0; at < TOKEN_LENGTH; at += 1) {
      token += PASSPHRASE_ALPHABET[
        (bytes[word * TOKEN_LENGTH + at] ?? 0) % PASSPHRASE_ALPHABET.length
      ];
    }
    tokens.push(token);
  }
  return tokens.join(separator);
}

/** NIP-49 encrypt this tab's secret key under the given passphrase. */
export function createBackup(secretKey: Uint8Array, password: string): string {
  if (!password) {
    throw new Error("A backup needs a passphrase.");
  }
  return encrypt(secretKey, password);
}

/** NIP-49 decrypt a backup back into a validated 32-byte Nostr secret key. */
export function decryptBackup(ncryptsec: string, password: string): Uint8Array {
  const trimmed = ncryptsec.trim();
  if (!trimmed.startsWith("ncryptsec1")) {
    throw new Error("That does not look like an encrypted backup.");
  }
  if (!password) {
    throw new Error("Enter the passphrase for this encrypted backup.");
  }
  const secretKey = decrypt(trimmed, password);
  if (!(secretKey instanceof Uint8Array) || secretKey.length !== 32) {
    throw new Error("That backup did not decrypt to a valid Nostr secret key.");
  }
  getPublicKey(secretKey);
  return secretKey;
}

/**
 * Decrypt locally and report only the backup's PUBLIC identity and whether it
 * matches the current one — never the decrypted key itself, mirroring the
 * desktop's contract. A wrong passphrase throws, which the caller renders.
 */
export function verifyBackup(
  ncryptsec: string,
  password: string,
  currentPubkey: string | null,
): BackupVerification {
  const secretKey = decrypt(ncryptsec, password);
  const pubkey = getPublicKey(secretKey);
  return {
    pubkey,
    npub: npubEncode(pubkey),
    matchesCurrentIdentity:
      currentPubkey !== null && pubkey === currentPubkey.toLowerCase(),
  };
}

/**
 * How long the backup's blob URL outlives the click. Revoking it in the same
 * task can cancel the save (Firefox, and Safari for a slow blob) while this
 * function still reports success — for the only copy of the tab's key, that
 * is a lost account. A minute is far longer than any browser needs to start a
 * download of a few hundred bytes.
 */
export const BACKUP_URL_LIFETIME_MS = 60_000;

/**
 * `save_ncryptsec_copy`: hand the ncryptsec to the user as a file. The
 * download API has no cancel event, so the suggested filename is the answer
 * (the desktop returns the chosen path, null on a cancelled dialog).
 * `schedule` is injectable so the deferred revoke can be tested in node.
 */
export function saveNcryptsecCopy(
  ncryptsec: string,
  schedule: (callback: () => void, delayMs: number) => unknown = setTimeout,
): string | null {
  if (!ncryptsec.startsWith("ncryptsec1")) {
    throw new Error("That does not look like an encrypted backup.");
  }
  const filename = `buzz-identity-backup-${new Date().toISOString().slice(0, 10)}.txt`;
  const blob = new Blob([ncryptsec], { type: "text/plain" });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  schedule(() => URL.revokeObjectURL(url), BACKUP_URL_LIFETIME_MS);
  return filename;
}
