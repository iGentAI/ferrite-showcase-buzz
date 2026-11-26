// Regression tests for the sidebar preference hooks' state handling:
// F397/F400 (impure mute/star updaters dropped remote updates when React ran
// them twice), F398 (createSection read its order from localStorage and
// reported success after a failed write) and F399 (a transiently empty
// section list wiped every per-section sort preference).

import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { JSDOM } from "jsdom";

import { makeSyncSigner } from "./sidebarSyncSignedEvents.mjs";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    window: dom.window,
  });
});

after(() => dom.window.close());

const RELAY_URL = "wss://relay.example";
const originals = {};

async function stubRelay({ fetchEvents = async () => [] } = {}) {
  const { relayClient } = await import("@/shared/api/relayClient");
  for (const key of [
    "fetchEvents",
    "subscribeLive",
    "subscribeToReconnects",
    "publishEvent",
  ]) {
    originals[key] ??= relayClient[key];
  }
  const live = { callback: null };
  relayClient.fetchEvents = fetchEvents;
  relayClient.subscribeLive = async (_filter, onEvent) => {
    live.callback = onEvent;
    return async () => {};
  };
  relayClient.subscribeToReconnects = () => () => {};
  relayClient.publishEvent = async () => {};
  return live;
}

afterEach(async () => {
  const { cleanup } = await import("@testing-library/react");
  cleanup();
  const { relayClient } = await import("@/shared/api/relayClient");
  Object.assign(relayClient, originals);
  delete window.__TAURI_INTERNALS__;
  window.localStorage.clear();
});

function decryptTo(payload) {
  const published = [];
  window.__TAURI_INTERNALS__ = {
    invoke: (cmd, args) => {
      if (cmd === "nip44_decrypt_from_self") {
        return Promise.resolve(JSON.stringify(payload));
      }
      if (cmd === "nip44_encrypt_to_self") {
        published.push(JSON.parse(args.plaintext));
        return Promise.resolve("ct");
      }
      if (cmd === "sign_event") {
        return Promise.resolve(
          JSON.stringify({
            id: `published-${published.length}`,
            pubkey: "self",
            content: "ct",
            created_at: args.createdAt,
            kind: args.kind,
            tags: args.tags,
            sig: "s",
          }),
        );
      }
      return Promise.reject(new Error(`unmocked: ${cmd}`));
    },
  };
  return published;
}

// The sync managers debounce publishes with window.setTimeout. Hold those
// timers so a test can run them on demand (React and the test runner use the
// Node global timers, which this leaves alone).
function holdWindowTimers() {
  const timers = new Map();
  let nextId = 1;
  const realSet = window.setTimeout;
  const realClear = window.clearTimeout;
  window.setTimeout = (fn) => {
    const id = nextId++;
    timers.set(id, fn);
    return id;
  };
  window.clearTimeout = (id) => {
    timers.delete(id);
  };
  return {
    runAll: () => {
      for (const [id, fn] of [...timers]) {
        timers.delete(id);
        fn();
      }
    },
    restore: () => {
      window.setTimeout = realSet;
      window.clearTimeout = realClear;
    },
  };
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 10));

for (const surface of [
  {
    name: "mutes",
    blob: "channel-mutes",
    hook: async () => (await import("./useChannelMutes.ts")).useChannelMutes,
    storage: async () => (await import("./channelMutesStorage.ts")).storageKey,
    set: (api, id) => api.muteChannel(id),
    ids: (api) => api.mutedChannelIds,
    entry: (updatedAt) => ({ muted: true, updatedAt }),
  },
  {
    name: "stars",
    blob: "channel-stars",
    hook: async () => (await import("./useChannelStars.ts")).useChannelStars,
    // Stars are scoped to the community relay as well as the account.
    storage: async () => {
      const { storageKey } = await import("./channelStarsStorage.ts");
      return (pubkey) => storageKey(pubkey, RELAY_URL);
    },
    set: (api, id) => api.starChannel(id),
    ids: (api) => api.starredChannelIds,
    entry: (updatedAt) => ({ starred: true, updatedAt }),
  },
]) {
  test(`${surface.name}: a remote update that lands beside a local change survives StrictMode and the local change is still published`, async () => {
    const React = await import("react");
    const { act, renderHook } = await import("@testing-library/react");
    const useHook = await surface.hook();
    const signer = makeSyncSigner(surface.blob);
    const live = await stubRelay();
    const createdAt = Math.floor(Date.now() / 1_000);
    const published = decryptTo({
      version: 1,
      channels: { "remote-channel": surface.entry(createdAt) },
    });
    const timers = holdWindowTimers();
    try {
      const { result } = renderHook(
        () => useHook(signer.pubkey, RELAY_URL),
        { wrapper: React.StrictMode },
      );
      await act(settle);
      assert.equal(typeof live.callback, "function");

      await act(async () => {
        // A local change leaves an update queued on the hook, so the remote
        // update after it is processed during render, where StrictMode runs
        // the updater twice.
        surface.set(result.current, "local-channel");
        live.callback(
          signer.sign({ content: "cipher", created_at: createdAt }),
        );
        await settle();
      });

      const ids = surface.ids(result.current);
      assert.ok(ids.has("local-channel"), "local change kept");
      assert.ok(ids.has("remote-channel"), "remote update applied on screen");

      // The remote blob lacks the local change, so it must still be published.
      await act(async () => {
        timers.runAll();
        await settle();
      });
      assert.ok(published.length > 0, "the merged store is published");
      assert.ok(
        "local-channel" in published.at(-1).channels,
        "the published blob carries the local change",
      );
    } finally {
      timers.restore();
    }
  });

  test(`${surface.name}: a remote update whose local write failed is applied when it arrives again`, async () => {
    const { act, renderHook } = await import("@testing-library/react");
    const useHook = await surface.hook();
    const storageKey = await surface.storage();
    const signer = makeSyncSigner(surface.blob);
    const live = await stubRelay();
    const createdAt = Math.floor(Date.now() / 1_000);
    decryptTo({
      version: 1,
      channels: { "remote-channel": surface.entry(createdAt) },
    });
    const { result } = renderHook(() => useHook(signer.pubkey, RELAY_URL));
    await act(settle);
    const event = signer.sign({ content: "cipher", created_at: createdAt });

    const proto = Object.getPrototypeOf(window.localStorage);
    const originalSetItem = proto.setItem;
    const key = storageKey(signer.pubkey);
    proto.setItem = function (k, v) {
      if (k === key) throw new Error("QuotaExceededError");
      return originalSetItem.call(this, k, v);
    };
    try {
      await act(async () => {
        live.callback(event);
        await settle();
      });
    } finally {
      proto.setItem = originalSetItem;
    }
    assert.equal(surface.ids(result.current).has("remote-channel"), false);

    await act(async () => {
      live.callback(event);
      await settle();
    });
    assert.equal(
      surface.ids(result.current).has("remote-channel"),
      true,
      "the redelivered event is not skipped as already applied",
    );
  });
}

test("createSection derives its order from state and reports a failed write", async () => {
  const { act, renderHook } = await import("@testing-library/react");
  const { useChannelSections } = await import("./useChannelSections.ts");
  await stubRelay();
  const pubkey = makeSyncSigner().pubkey;

  const { result } = renderHook(() => useChannelSections(pubkey, RELAY_URL));
  await act(settle);

  let first;
  act(() => {
    first = result.current.createSection("First");
  });
  assert.equal(first?.order, 0);

  // Storage refuses the write (quota exhausted, private mode).
  const setItem = window.localStorage.setItem;
  const originalSetItem = Object.getPrototypeOf(window.localStorage).setItem;
  Object.getPrototypeOf(window.localStorage).setItem = () => {
    throw new Error("QuotaExceededError");
  };
  let refused;
  try {
    act(() => {
      refused = result.current.createSection("Refused");
    });
  } finally {
    Object.getPrototypeOf(window.localStorage).setItem = originalSetItem;
  }
  assert.equal(refused, null, "a section that was not stored is not reported");
  assert.deepEqual(
    result.current.sections.map((s) => s.name),
    ["First"],
  );
  assert.equal(window.localStorage.setItem, setItem);

  // localStorage now disagrees with what is on screen.
  window.localStorage.clear();
  let second;
  act(() => {
    second = result.current.createSection("Second");
  });
  assert.equal(second?.order, 1, "order continues from the sections shown");
  assert.deepEqual(
    result.current.sections.map((s) => [s.name, s.order]),
    [
      ["First", 0],
      ["Second", 1],
    ],
  );
});

test("createSection at the section limit returns a section that is really stored", async () => {
  const { act, renderHook } = await import("@testing-library/react");
  const { MAX_CHANNEL_SECTIONS, readChannelSectionsStore, writeChannelSectionsStore } =
    await import("./channelSectionsStorage.ts");
  const { useChannelSections } = await import("./useChannelSections.ts");
  await stubRelay();
  const pubkey = makeSyncSigner().pubkey;
  writeChannelSectionsStore(
    pubkey,
    {
      version: 1,
      sections: Array.from({ length: MAX_CHANNEL_SECTIONS }, (_, index) => ({
        id: `s-${index}`,
        name: `S${index}`,
        order: index,
      })),
      assignments: {},
    },
    RELAY_URL,
  );
  const { result } = renderHook(() => useChannelSections(pubkey, RELAY_URL));
  await act(settle);
  let created;
  act(() => {
    created = result.current.createSection("Newest");
  });
  assert.equal(created?.name, "Newest", "the highest order survives the bound");
  assert.ok(result.current.sections.some((s) => s.id === created.id));
  assert.ok(
    readChannelSectionsStore(pubkey, RELAY_URL).sections.some(
      (s) => s.id === created.id,
    ),
  );
  assert.equal(result.current.sections.length, MAX_CHANNEL_SECTIONS);
  assert.equal(
    result.current.sections.some((s) => s.id === "s-0"),
    false,
    "the oldest section made room",
  );
});

test("an empty live-section list does not wipe per-section sort preferences", async () => {
  const { act, renderHook } = await import("@testing-library/react");
  const { readChannelSortStore, storageKey, writeChannelSortStore } =
    await import("./channelSortPreference.ts");
  const { useChannelSortPreference } = await import(
    "./useChannelSortPreference.ts"
  );
  await stubRelay();
  const pubkey = makeSyncSigner().pubkey;
  writeChannelSortStore(
    pubkey,
    { version: 1, groups: { "section:abc": "recent", dms: "recent" } },
    RELAY_URL,
  );
  assert.ok(window.localStorage.getItem(storageKey(pubkey, RELAY_URL)));

  // Sections have not loaded yet, so the sidebar passes an empty list.
  const { result, rerender } = renderHook(
    ({ ids }) => useChannelSortPreference(pubkey, RELAY_URL, ids),
    { initialProps: { ids: [] } },
  );
  await act(settle);
  act(() => result.current.setSortModeFor("channels", "recent"));
  assert.deepEqual(readChannelSortStore(pubkey, RELAY_URL).groups, {
    "section:abc": "recent",
    dms: "recent",
    channels: "recent",
  });

  // Once sections are known, orphaned section keys are still pruned.
  rerender({ ids: ["other"] });
  act(() => result.current.setSortModeFor("forums", "recent"));
  assert.deepEqual(readChannelSortStore(pubkey, RELAY_URL).groups, {
    dms: "recent",
    channels: "recent",
    forums: "recent",
  });
});
