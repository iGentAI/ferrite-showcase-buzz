// Signed-event helpers for the sidebar sync manager tests.
//
// The managers verify the id and Schnorr signature of every blob a relay
// hands back before trusting its `pubkey` or `created_at`, so tests that feed
// events to them sign those events with a real key. Pass the blob's `d` tag
// so signed fixtures carry the address the managers now require.

import {
  finalizeEvent,
  generateSecretKey,
  getPublicKey,
} from "nostr-tools/pure";

export function makeSyncSigner(dTag) {
  const secretKey = generateSecretKey();
  const pubkey = getPublicKey(secretKey);
  return {
    pubkey,
    sign: ({
      kind = 30078,
      content,
      created_at,
      tags = dTag ? [["d", dTag]] : [],
    }) => finalizeEvent({ kind, content, created_at, tags }, secretKey),
  };
}
