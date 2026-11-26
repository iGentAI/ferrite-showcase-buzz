/**
 * Regression tests for F146 (legacy community list keeps the plaintext nsec)
 * and F364 (raw removeItem calls escape the throw-safe storage contract).
 */
import assert from "node:assert/strict";
import test from "node:test";

import {
  clearCommunityStorage,
  loadCommunities,
  migrateLegacyCommunityStorage,
  saveCommunities,
  stripCommunityKeyMaterial,
} from "./communityStorage.ts";
import { applyLegacyCommunityStorage } from "./legacyCommunityStorage.ts";

const NSEC = `nsec1${"q".repeat(58)}`;

function createMemoryStorage(initial = {}) {
  const values = new Map(Object.entries(initial));
  return {
    values,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, String(value)),
    removeItem: (key) => values.delete(key),
    clear: () => values.clear(),
    key: (index) => Array.from(values.keys())[index] ?? null,
    get length() {
      return values.size;
    },
  };
}

function install(storage) {
  globalThis.localStorage = storage;
  globalThis.window = { localStorage: storage };
}

function assertNoStoredKey(storage) {
  for (const [key, value] of storage.values) {
    assert.ok(!String(value).includes(NSEC), `${key} still holds the nsec`);
  }
}

test("F146: migrating a legacy community list never copies the stored nsec", () => {
  const storage = createMemoryStorage({
    "buzz-workspaces": JSON.stringify([
      { id: "legacy", relayUrl: "wss://relay.example.com", nsec: NSEC },
    ]),
    "buzz-active-workspace-id": "legacy",
  });

  migrateLegacyCommunityStorage(storage);

  assert.deepEqual(JSON.parse(storage.getItem("buzz-communities")), [
    { id: "legacy", relayUrl: "wss://relay.example.com" },
  ]);
  assert.equal(storage.getItem("buzz-active-community-id"), "legacy");
  // The legacy source that held the plaintext key is removed after the copy.
  assert.equal(storage.getItem("buzz-workspaces"), null);
  assert.equal(storage.getItem("buzz-active-workspace-id"), null);
  assertNoStoredKey(storage);
});

test("F146: a legacy entry left beside current state is swept on the next boot", () => {
  const storage = createMemoryStorage({
    "buzz-communities": '[{"id":"current"}]',
    "buzz-active-community-id": "current",
    "buzz-workspaces": JSON.stringify([{ id: "old", nsec: NSEC }]),
    "buzz-active-workspace-id": "old",
  });

  migrateLegacyCommunityStorage(storage);

  assert.equal(storage.getItem("buzz-communities"), '[{"id":"current"}]');
  assert.equal(storage.getItem("buzz-active-community-id"), "current");
  assert.equal(storage.getItem("buzz-workspaces"), null);
  assert.equal(storage.getItem("buzz-active-workspace-id"), null);
  assertNoStoredKey(storage);
});

test("F146: the Sprout snapshot path strips the nsec before writing Buzz storage", () => {
  const storage = createMemoryStorage();

  applyLegacyCommunityStorage(
    {
      workspaces: JSON.stringify([
        { id: "sprout", relayUrl: "wss://relay.example.com", nsec: NSEC },
      ]),
      activeWorkspaceId: "sprout",
      onboardingCompletions: [],
    },
    storage,
  );

  assert.deepEqual(JSON.parse(storage.getItem("buzz-communities")), [
    { id: "sprout", relayUrl: "wss://relay.example.com" },
  ]);
  assertNoStoredKey(storage);
});

test("F146: stripCommunityKeyMaterial leaves key-free lists byte-identical", () => {
  const raw = '[{"id":"a","relayUrl":"wss://relay.example.com"}]';
  assert.equal(stripCommunityKeyMaterial(raw), raw);
  assert.equal(stripCommunityKeyMaterial("not json"), null);
  assert.equal(stripCommunityKeyMaterial('{"id":"a"}'), null);
});

test("F146: a failed strip-back write is retried so the nsec does not stay stored", () => {
  const storage = createMemoryStorage({
    "buzz-communities": JSON.stringify([{ id: "c1", nsec: NSEC }]),
  });
  const realSetItem = storage.setItem;
  let failures = 0;
  storage.setItem = (key, value) => {
    if (key === "buzz-communities" && failures === 0) {
      failures += 1;
      throw new Error("QuotaExceededError");
    }
    realSetItem(key, value);
  };
  install(storage);

  assert.deepEqual(loadCommunities(), [{ id: "c1" }]);
  assert.equal(failures, 1);
  assert.deepEqual(JSON.parse(storage.getItem("buzz-communities")), [
    { id: "c1" },
  ]);
  assertNoStoredKey(storage);
});

test("F146: when the rewrite keeps failing the key-bearing value is removed", () => {
  const storage = createMemoryStorage({
    "buzz-communities": JSON.stringify([{ id: "c1", nsec: NSEC }]),
  });
  const realSetItem = storage.setItem;
  storage.setItem = (key, value) => {
    if (key === "buzz-communities") throw new Error("QuotaExceededError");
    realSetItem(key, value);
  };
  install(storage);

  assert.deepEqual(loadCommunities(), [{ id: "c1" }]);
  assert.equal(storage.getItem("buzz-communities"), null);
  assertNoStoredKey(storage);
});

test("F364: saveCommunities survives an origin that denies removeItem", () => {
  const storage = createMemoryStorage({
    "buzz-community-discovery-after-leave": "1",
  });
  storage.removeItem = () => {
    throw new DOMException("storage denied", "SecurityError");
  };
  install(storage);

  assert.doesNotThrow(() => saveCommunities([{ id: "joined" }]));
});

test("F364: clearCommunityStorage never throws and still clears the other keys", () => {
  const storage = createMemoryStorage({
    "buzz-communities": "[]",
    "buzz-active-community-id": "a",
    "buzz-workspaces": "[]",
    "buzz-active-workspace-id": "a",
  });
  const realRemoveItem = storage.removeItem;
  storage.removeItem = (key) => {
    if (key === "buzz-communities") {
      throw new DOMException("storage denied", "SecurityError");
    }
    realRemoveItem(key);
  };

  assert.doesNotThrow(() => clearCommunityStorage(storage));
  assert.equal(storage.getItem("buzz-active-community-id"), null);
  assert.equal(storage.getItem("buzz-workspaces"), null);
  assert.equal(storage.getItem("buzz-active-workspace-id"), null);
});

test("F364: clearCommunityStorage with the default storage survives a denied origin", () => {
  const storage = createMemoryStorage({ "buzz-communities": "[]" });
  storage.removeItem = () => {
    throw new DOMException("storage denied", "SecurityError");
  };
  install(storage);

  assert.doesNotThrow(() => clearCommunityStorage());
});

test("F146 review: a failed copy still removes the nsec from the legacy entry", () => {
  const storage = createMemoryStorage({
    "buzz-workspaces": JSON.stringify([
      { id: "legacy", relayUrl: "wss://relay.example.com", nsec: NSEC },
    ]),
  });
  const realSetItem = storage.setItem;
  storage.setItem = (key, value) => {
    if (key === "buzz-communities") throw new Error("QuotaExceededError");
    realSetItem(key, value);
  };

  assert.doesNotThrow(() => migrateLegacyCommunityStorage(storage));

  assert.equal(storage.getItem("buzz-communities"), null);
  // The list survives for the next attempt, without the key.
  assert.deepEqual(JSON.parse(storage.getItem("buzz-workspaces")), [
    { id: "legacy", relayUrl: "wss://relay.example.com" },
  ]);
  assertNoStoredKey(storage);
});

test("F146 review: when every write fails the key-bearing legacy entry is removed", () => {
  const storage = createMemoryStorage({
    "buzz-workspaces": JSON.stringify([{ id: "legacy", nsec: NSEC }]),
    "buzz-active-workspace-id": "legacy",
  });
  storage.setItem = () => {
    throw new Error("QuotaExceededError");
  };

  assert.doesNotThrow(() => migrateLegacyCommunityStorage(storage));

  assert.equal(storage.getItem("buzz-workspaces"), null);
  assertNoStoredKey(storage);
});

test("F146 review: a failed active-id copy does not skip the legacy cleanup", () => {
  const storage = createMemoryStorage({
    "buzz-workspaces": JSON.stringify([{ id: "legacy", nsec: NSEC }]),
    "buzz-active-workspace-id": "legacy",
  });
  const realSetItem = storage.setItem;
  storage.setItem = (key, value) => {
    if (key === "buzz-active-community-id") {
      throw new Error("QuotaExceededError");
    }
    realSetItem(key, value);
  };

  migrateLegacyCommunityStorage(storage);

  assert.deepEqual(JSON.parse(storage.getItem("buzz-communities")), [
    { id: "legacy" },
  ]);
  assert.equal(storage.getItem("buzz-workspaces"), null);
  // The active id is kept in its legacy slot for the next attempt.
  assert.equal(storage.getItem("buzz-active-workspace-id"), "legacy");
  assertNoStoredKey(storage);
});
