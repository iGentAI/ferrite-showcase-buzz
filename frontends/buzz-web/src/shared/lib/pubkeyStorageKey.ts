/**
 * localStorage keys scoped to one account, `${prefix}:${pubkey}`.
 *
 * The pubkey is lowercased, so every reader and writer agrees on one key
 * whatever case the caller holds (hex pubkeys are case-insensitive). A value
 * written before this normalisation under a mixed-case key is still read
 * when the lowercased key holds nothing.
 */
export function pubkeyStorageKey(prefix: string, pubkey: string): string {
  return `${prefix}:${pubkey.toLowerCase()}`;
}

export function readPubkeyScopedItem(
  prefix: string,
  pubkey: string,
): string | null {
  const raw = window.localStorage.getItem(pubkeyStorageKey(prefix, pubkey));
  if (raw !== null || pubkey === pubkey.toLowerCase()) return raw;
  return window.localStorage.getItem(`${prefix}:${pubkey}`);
}
