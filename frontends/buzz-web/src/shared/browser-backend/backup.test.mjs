import assert from "node:assert/strict";
import test from "node:test";

import {
  BACKUP_URL_LIFETIME_MS,
  createBackup,
  generatePassphrase,
  saveNcryptsecCopy,
  verifyBackup,
} from "./backup.ts";
import { generateSecretKey, getPublicKey } from "nostr-tools/pure";
import { importIdentity } from "../api/tauriIdentity.ts";
import { localSecretKey } from "./identity.ts";

test("a passphrase has the asked-for shape and never repeats", () => {
  const one = generatePassphrase();
  assert.equal(one.split("-").length, 6, "default is six words");
  for (const token of one.split("-")) {
    assert.match(token, /^[abcdefghijkmnpqrstuvwxyz23456789]{4}$/);
  }
  const custom = generatePassphrase({ words: 8, separator: "." });
  assert.equal(custom.split(".").length, 8);
  // 120 bits of entropy cannot collide in two draws unless the sampler is broken.
  assert.notEqual(one, generatePassphrase());
});

test("a backup round-trips and names its own identity", () => {
  const secretKey = generateSecretKey();
  const pubkey = getPublicKey(secretKey);
  const ncryptsec = createBackup(secretKey, "correct horse");
  assert.ok(ncryptsec.startsWith("ncryptsec1"));

  const match = verifyBackup(ncryptsec, "correct horse", pubkey);
  assert.equal(match.pubkey, pubkey);
  assert.ok(match.npub.startsWith("npub1"));
  assert.equal(match.matchesCurrentIdentity, true);

  // The same backup verified against a DIFFERENT current identity must say so
  // — that mismatch is the whole point of the verify step.
  const other = getPublicKey(generateSecretKey());
  assert.equal(
    verifyBackup(ncryptsec, "correct horse", other).matchesCurrentIdentity,
    false,
  );
  assert.equal(
    verifyBackup(ncryptsec, "correct horse", null).matchesCurrentIdentity,
    false,
  );
});

test("a wrong passphrase is an error, not a wrong identity", () => {
  const ncryptsec = createBackup(generateSecretKey(), "right");
  assert.throws(() => verifyBackup(ncryptsec, "wrong", null));
  assert.throws(() => createBackup(generateSecretKey(), ""));
});

test("the backup file's blob URL outlives the click that starts the download", () => {
  // Revoking in the same task can cancel the save while success is reported,
  // for the only copy of the tab's key.
  const events = [];
  const scheduled = [];
  const originalDocument = globalThis.document;
  const originalCreate = URL.createObjectURL;
  const originalRevoke = URL.revokeObjectURL;
  globalThis.document = {
    createElement: () => ({
      click: () => events.push("click"),
      remove: () => events.push("remove"),
    }),
    body: { appendChild: () => events.push("append") },
  };
  URL.createObjectURL = () => "blob:backup";
  URL.revokeObjectURL = (url) => events.push(`revoke ${url}`);
  try {
    const filename = saveNcryptsecCopy("ncryptsec1qqqq", (callback, delayMs) =>
      scheduled.push({ callback, delayMs }),
    );
    assert.match(filename, /^buzz-identity-backup-\d{4}-\d{2}-\d{2}\.txt$/);
    assert.deepEqual(events, ["append", "click", "remove"], "not revoked yet");
    assert.equal(scheduled.length, 1);
    assert.ok(scheduled[0].delayMs >= 30_000);
    assert.equal(scheduled[0].delayMs, BACKUP_URL_LIFETIME_MS);
    scheduled[0].callback();
    assert.deepEqual(events.at(-1), "revoke blob:backup");
  } finally {
    globalThis.document = originalDocument;
    URL.createObjectURL = originalCreate;
    URL.revokeObjectURL = originalRevoke;
  }
});

test("importIdentity restores a NIP-49 ncryptsec1 backup with its passphrase and rejects bad passphrases", async () => {
  const originalWindow = globalThis.window;
  const store = new Map();
  globalThis.window = {
    localStorage: {
      getItem: (k) => store.get(k) ?? null,
      setItem: (k, v) => store.set(k, String(v)),
      removeItem: (k) => store.delete(k),
    },
  };
  try {
    const initialPubkey = getPublicKey(localSecretKey());
    const backedUpKey = generateSecretKey();
    const backedUpPubkey = getPublicKey(backedUpKey);
    const ncryptsec = createBackup(backedUpKey, "correct horse");

    await assert.rejects(importIdentity(ncryptsec), /passphrase|password/i);
    assert.equal(getPublicKey(localSecretKey()), initialPubkey);

    await assert.rejects(importIdentity(ncryptsec, "wrong passphrase"));
    assert.equal(getPublicKey(localSecretKey()), initialPubkey);

    const restored = await importIdentity(`  ${ncryptsec}  `, "correct horse");
    assert.equal(restored.pubkey, backedUpPubkey);
    assert.equal(getPublicKey(localSecretKey()), backedUpPubkey);
  } finally {
    if (originalWindow === undefined) delete globalThis.window;
    else globalThis.window = originalWindow;
  }
});
