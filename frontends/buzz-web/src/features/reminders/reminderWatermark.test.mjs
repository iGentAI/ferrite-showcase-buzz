import assert from "node:assert/strict";
import test from "node:test";

const store = new Map();
globalThis.window = {
  localStorage: {
    getItem: (key) => (store.has(key) ? store.get(key) : null),
    setItem: (key, value) => store.set(key, String(value)),
    removeItem: (key) => store.delete(key),
  },
};

const { readWatermark, watermarkStorageKey } = await import(
  "./useReminderNotifications.ts"
);

test("each community keeps its own reminder watermark", () => {
  store.clear();
  const pubkey = "a".repeat(64);
  store.set(watermarkStorageKey(pubkey, "wss://a.example"), "500");
  store.set(watermarkStorageKey(pubkey, "wss://b.example"), "100");
  assert.equal(readWatermark(pubkey, "wss://a.example"), 500);
  assert.equal(readWatermark(pubkey, "wss://b.example"), 100);
});

test("a community's first read is seeded from the account-only watermark", () => {
  store.clear();
  const pubkey = "b".repeat(64);
  store.set(watermarkStorageKey(pubkey), "42");
  assert.equal(readWatermark(pubkey, "wss://c.example"), 42);
  assert.equal(store.get(watermarkStorageKey(pubkey, "wss://c.example")), "42");
});
