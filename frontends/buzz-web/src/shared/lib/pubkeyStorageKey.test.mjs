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

const { pubkeyStorageKey, readPubkeyScopedItem } = await import(
  "./pubkeyStorageKey.ts"
);
const { makeRootIdStore } = await import(
  "../../features/channels/unreadRootIdStore.ts"
);
const { forcedUnreadStore } = await import(
  "../../features/channels/forcedUnreadStore.ts"
);

test("one key whatever the case of the pubkey", () => {
  assert.equal(pubkeyStorageKey("p", "ABcd"), "p:abcd");
});

test("a legacy mixed-case key is still read", () => {
  store.clear();
  store.set("p:ABcd", "legacy");
  assert.equal(readPubkeyScopedItem("p", "ABcd"), "legacy");
  store.set("p:abcd", "current");
  assert.equal(readPubkeyScopedItem("p", "ABcd"), "current");
});

test("root-id and forced-unread stores round-trip across pubkey case", () => {
  store.clear();
  const roots = makeRootIdStore("buzz-test-roots");
  roots.write("AAbb", new Set(["root-1"]));
  assert.deepEqual([...roots.read("aabb")], ["root-1"]);
  forcedUnreadStore.write("AAbb", { "channel-1": 5 });
  assert.deepEqual(forcedUnreadStore.read("aabb"), { "channel-1": 5 });
});
