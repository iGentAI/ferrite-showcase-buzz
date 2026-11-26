import assert from "node:assert/strict";
import test from "node:test";

import { getPublicKey } from "nostr-tools/pure";
import { nsecEncode } from "nostr-tools/nip19";
import { importLocalIdentity, localSecretKey } from "./identity.ts";

function storage() {
  const values = new Map();
  return {
    getItem(key) {
      return values.get(key) ?? null;
    },
    setItem(key, value) {
      values.set(key, String(value));
    },
  };
}

test("browser identity import replaces the key used for signing", () => {
  globalThis.window = { localStorage: storage() };
  const secretKey = Uint8Array.from({ length: 32 }, (_, index) => index + 1);

  const imported = importLocalIdentity(nsecEncode(secretKey));

  assert.deepEqual(imported, secretKey);
  assert.deepEqual(localSecretKey(), secretKey);
  assert.equal(getPublicKey(localSecretKey()), getPublicKey(secretKey));
});

test("browser identity import rejects non-nsec input without replacing the key", () => {
  globalThis.window = { localStorage: storage() };
  const original = localSecretKey();

  assert.throws(() => importLocalIdentity("not-an-nsec"));
  assert.deepEqual(localSecretKey(), original);
});
