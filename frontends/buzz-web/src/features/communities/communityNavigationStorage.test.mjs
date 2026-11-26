import assert from "node:assert/strict";
import test from "node:test";

import {
  clearCommunityDestinations,
  loadCommunityDestination,
  removeCommunityDestination,
  saveCommunityDestination,
} from "./communityNavigationStorage.ts";

function createMemoryStorage(initial = {}) {
  const values = new Map(Object.entries(initial));
  return {
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

test("saves independent Home and channel destinations by community", () => {
  const storage = createMemoryStorage();

  saveCommunityDestination(
    "alpha",
    { kind: "channel", channelId: "general" },
    storage,
  );
  saveCommunityDestination("bravo", { kind: "home" }, storage);

  assert.deepEqual(loadCommunityDestination("alpha", storage), {
    kind: "channel",
    channelId: "general",
  });
  assert.deepEqual(loadCommunityDestination("bravo", storage), {
    kind: "home",
  });
});

test("ignores malformed stored destinations", () => {
  const storage = createMemoryStorage({
    "buzz-community-destinations": JSON.stringify({
      valid: { kind: "channel", channelId: "general" },
      emptyChannel: { kind: "channel", channelId: "" },
      unknown: { kind: "settings" },
      primitive: "home",
    }),
  });

  assert.deepEqual(loadCommunityDestination("valid", storage), {
    kind: "channel",
    channelId: "general",
  });
  assert.equal(loadCommunityDestination("emptyChannel", storage), null);
  assert.equal(loadCommunityDestination("unknown", storage), null);
  assert.equal(loadCommunityDestination("primitive", storage), null);
});

test("recovers from invalid JSON", () => {
  const storage = createMemoryStorage({
    "buzz-community-destinations": "not-json",
  });

  assert.equal(loadCommunityDestination("alpha", storage), null);
  saveCommunityDestination("alpha", { kind: "home" }, storage);
  assert.deepEqual(loadCommunityDestination("alpha", storage), {
    kind: "home",
  });
});

test("removes one destination without disturbing another", () => {
  const storage = createMemoryStorage();
  saveCommunityDestination("alpha", { kind: "home" }, storage);
  saveCommunityDestination(
    "bravo",
    { kind: "channel", channelId: "random" },
    storage,
  );

  removeCommunityDestination("alpha", storage);

  assert.equal(loadCommunityDestination("alpha", storage), null);
  assert.deepEqual(loadCommunityDestination("bravo", storage), {
    kind: "channel",
    channelId: "random",
  });
});

test("clears all destinations", () => {
  const storage = createMemoryStorage();
  saveCommunityDestination("alpha", { kind: "home" }, storage);

  clearCommunityDestinations(storage);

  assert.equal(storage.length, 0);
});

function withDeniedLocalStorage(run) {
  const hadWindow = "window" in globalThis;
  const previous = globalThis.window;
  globalThis.window = {};
  Object.defineProperty(globalThis.window, "localStorage", {
    get() {
      throw new DOMException("denied", "SecurityError");
    },
  });
  try {
    run();
  } finally {
    if (hadWindow) globalThis.window = previous;
    else delete globalThis.window;
  }
}

test("destination helpers degrade instead of throwing when storage is denied", () => {
  withDeniedLocalStorage(() => {
    assert.equal(loadCommunityDestination("alpha"), null);
    saveCommunityDestination("alpha", { kind: "home" });
    removeCommunityDestination("alpha");
    clearCommunityDestinations();
  });
});

test("a removeItem that throws does not abort clearing destinations", () => {
  const storage = createMemoryStorage();
  storage.removeItem = () => {
    throw new DOMException("denied", "SecurityError");
  };
  clearCommunityDestinations(storage);
});

test("prototype member names are not destinations", () => {
  const storage = createMemoryStorage();
  assert.equal(loadCommunityDestination("toString", storage), null);
  assert.equal(loadCommunityDestination("__proto__", storage), null);
});
