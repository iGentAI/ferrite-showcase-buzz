// Run: node --import ./test-loader.mjs --test e2e/relayHttp.test.mjs
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { getPublicKey } from "nostr-tools/pure";

import { identity } from "./relayHttp.ts";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const hex = (bytes) => Buffer.from(bytes).toString("hex");

test("F134: a scenario identity's secret is not derivable from its label", () => {
  const label = `invite-minter-${Date.now().toString(36)}-abcd`;
  const who = identity(label);
  assert.notEqual(hex(who.secret), createHash("sha256").update(label).digest("hex"));
  assert.equal(who.pubkey, getPublicKey(who.secret));
  assert.equal(who.secret.length, 32);
});

test("F134: one label names one identity for the whole process, two labels never share one", () => {
  const a = identity("reminder-owner-run1");
  assert.equal(identity("reminder-owner-run1"), a, "a repeated label is the same identity");
  assert.notEqual(identity("reminder-stranger-run1").pubkey, a.pubkey);
});

test("F134: no harness script derives a signing key from sha256 of a label", () => {
  const pattern = /new Uint8Array\(\s*createHash\(\s*["']sha256["']\s*\)\s*\.update\(/;
  const offenders = fs
    .readdirSync(HERE)
    .filter((name) => /\.(ts|mjs)$/.test(name) && !name.endsWith(".test.mjs"))
    .filter((name) => pattern.test(fs.readFileSync(path.join(HERE, name), "utf8")));
  assert.deepEqual(offenders, []);
});
