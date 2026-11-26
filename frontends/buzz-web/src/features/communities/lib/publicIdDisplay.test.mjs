/**
 * Regression tests for F372 (WelcomeSetup encoded the pubkey with the
 * throwing encoder during render) and F373 (the public-ID copy had no
 * rejection handler).
 */
import assert from "node:assert/strict";
import test from "node:test";

import { npubEncode } from "nostr-tools/nip19";

import { copyPublicId, describePublicId } from "./publicIdDisplay.ts";

const KEY = "c".repeat(64);

test("F372: a malformed pubkey reads as unavailable instead of throwing", () => {
  for (const pubkey of [
    "npub1fromaquirkyextension",
    "zz",
    "c".repeat(63),
    "g".repeat(64),
  ]) {
    assert.deepEqual(describePublicId(pubkey), { kind: "unavailable" }, pubkey);
  }
});

test("F372: a missing pubkey is loading and a valid one is encoded", () => {
  assert.deepEqual(describePublicId(undefined), { kind: "loading" });
  assert.deepEqual(describePublicId(""), { kind: "loading" });
  assert.deepEqual(describePublicId(KEY), {
    kind: "ready",
    npub: npubEncode(KEY),
  });
});

test("F373: a denied clipboard resolves false instead of rejecting", async () => {
  const warn = console.warn;
  console.warn = () => {};
  try {
    assert.equal(
      await copyPublicId("npub1x", async () => {
        throw new Error("clipboard denied");
      }),
      false,
    );
  } finally {
    console.warn = warn;
  }
  let written = null;
  assert.equal(
    await copyPublicId("npub1x", async (text) => {
      written = text;
    }),
    true,
  );
  assert.equal(written, "npub1x");
});

test("describePublicId refuses hex that is not 32 bytes and normalizes case", () => {
  assert.deepEqual(describePublicId("ab".repeat(20)), { kind: "unavailable" });
  assert.deepEqual(describePublicId(` ${"C".repeat(64)} `), {
    kind: "ready",
    npub: npubEncode(KEY),
  });
});

test("an errored or empty identity reads as unavailable, not loading", async () => {
  const { describePublicId } = await import("./publicIdDisplay.ts");
  assert.deepEqual(describePublicId(undefined, false), { kind: "unavailable" });
  assert.deepEqual(describePublicId(undefined, true), { kind: "loading" });
  assert.equal(describePublicId(` ${"a".repeat(64)} `, false).kind, "ready");
});
