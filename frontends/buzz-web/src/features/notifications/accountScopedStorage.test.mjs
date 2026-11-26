import assert from "node:assert/strict";
import { after, afterEach, before, beforeEach, test } from "node:test";

import { JSDOM } from "jsdom";

// F394 / F395: notification state is stored per account in localStorage.
// These drive the real hooks in a DOM and read the storage writes back.

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

const ACCOUNT_A = "a".repeat(64);
const ACCOUNT_B = "b".repeat(64);
const SETTINGS_KEY = (pubkey) => `buzz-notification-settings.v2:${pubkey}`;
const SEEN_KEY = (pubkey) => `buzz-home-feed-seen.v1:${pubkey}`;
const NOTIFIED_KEY = (pubkey) => `buzz-feed-notified.v1:${pubkey}`;

let writes = [];

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    window: dom.window,
  });
  // Wrap the prototype method: assigning `localStorage.setItem = …` on a
  // Storage object stores an item named "setItem" instead of replacing it.
  const proto = dom.window.Storage.prototype;
  const setItem = proto.setItem;
  proto.setItem = function recordingSetItem(key, value) {
    writes.push([key, String(value)]);
    return setItem.call(this, key, value);
  };
});

beforeEach(() => {
  dom.window.localStorage.clear();
  writes = [];
});

afterEach(async () => {
  const { cleanup } = await import("@testing-library/react");
  cleanup();
});

after(() => dom.window.close());

function feedItem(id, category) {
  return {
    id,
    kind: 9,
    pubkey: "c".repeat(64),
    content: id,
    createdAt: 1,
    channelId: null,
    channelName: "",
    tags: [],
    category,
  };
}

function homeFeed(feed) {
  return {
    feed: {
      mentions: [],
      needsAction: [],
      activity: [],
      agentActivity: [],
      ...feed,
    },
    meta: { since: 0, total: 0, generatedAt: 0 },
  };
}

const QUIET_SETTINGS = {
  desktopEnabled: false,
  homeBadgeEnabled: true,
  notifyWhileViewing: false,
  sounds: {},
  slotAlertsEnabled: { mention: true, needs_action: true },
  slotAlertsSnapshot: null,
};

test("F394: switching accounts never writes the previous account's settings under the new account's key", async () => {
  dom.window.localStorage.setItem(
    SETTINGS_KEY(ACCOUNT_A),
    JSON.stringify({ homeBadgeEnabled: false, notifyWhileViewing: true }),
  );
  dom.window.localStorage.setItem(
    SETTINGS_KEY(ACCOUNT_B),
    JSON.stringify({ homeBadgeEnabled: true, notifyWhileViewing: false }),
  );
  writes = [];

  const { renderHook, act } = await import("@testing-library/react");
  const { useNotificationSettings } = await import("./hooks.ts");
  const { result, rerender } = renderHook(
    ({ pubkey }) => useNotificationSettings(pubkey),
    { initialProps: { pubkey: ACCOUNT_A } },
  );
  assert.equal(result.current.settings.notifyWhileViewing, true);

  await act(async () => {
    rerender({ pubkey: ACCOUNT_B });
  });

  for (const [key, value] of writes) {
    if (key !== SETTINGS_KEY(ACCOUNT_B)) continue;
    const written = JSON.parse(value);
    assert.equal(
      written.notifyWhileViewing,
      false,
      `account A's settings were written under account B's key: ${value}`,
    );
    assert.equal(written.homeBadgeEnabled, true);
  }
  assert.equal(result.current.settings.notifyWhileViewing, false);
  assert.equal(result.current.settings.homeBadgeEnabled, true);
  const storedB = JSON.parse(
    dom.window.localStorage.getItem(SETTINGS_KEY(ACCOUNT_B)),
  );
  assert.equal(storedB.notifyWhileViewing, false);
  assert.equal(storedB.homeBadgeEnabled, true);
});

test("F394: switching accounts never writes the previous account's seen feed ids under the new account's key", async () => {
  dom.window.localStorage.setItem(
    SEEN_KEY(ACCOUNT_A),
    JSON.stringify(["a-read-1", "a-read-2"]),
  );
  dom.window.localStorage.setItem(
    SEEN_KEY(ACCOUNT_B),
    JSON.stringify(["b-read-1"]),
  );
  writes = [];

  const { renderHook, act } = await import("@testing-library/react");
  const { useHomeFeedNotificationState } = await import("./hooks.ts");
  const { rerender } = renderHook(
    ({ pubkey }) =>
      useHomeFeedNotificationState(
        undefined,
        pubkey,
        QUIET_SETTINGS,
        async () => true,
        false,
        false,
        () => null,
        0,
        new Set(),
      ),
    { initialProps: { pubkey: ACCOUNT_A } },
  );

  await act(async () => {
    rerender({ pubkey: ACCOUNT_B });
  });

  for (const [key, value] of writes) {
    if (key !== SEEN_KEY(ACCOUNT_B)) continue;
    assert.ok(
      !value.includes("a-read"),
      `account A's seen ids were written under account B's key: ${value}`,
    );
  }
  assert.deepEqual(
    JSON.parse(dom.window.localStorage.getItem(SEEN_KEY(ACCOUNT_B))),
    ["b-read-1"],
  );
  assert.deepEqual(
    JSON.parse(dom.window.localStorage.getItem(SEEN_KEY(ACCOUNT_A))),
    ["a-read-1", "a-read-2"],
  );
});

test("F395: desktop delivery keeps its own record and leaves the home badge's seen ids alone", async () => {
  // The badge had recorded an inbox row and an older mention as seen.
  dom.window.localStorage.setItem(
    SEEN_KEY(ACCOUNT_A),
    JSON.stringify(["inbox-dismissed", "mention-old"]),
  );
  writes = [];

  const { renderHook, act } = await import("@testing-library/react");
  const { useFeedDesktopNotifications } = await import(
    "./use-feed-desktop-notifications.ts"
  );
  const firstFeed = homeFeed({ mentions: [feedItem("mention-1", "mention")] });
  const { rerender } = renderHook(
    ({ feed }) =>
      useFeedDesktopNotifications(
        feed,
        ACCOUNT_A,
        QUIET_SETTINGS,
        async () => true,
        true,
        {},
      ),
    { initialProps: { feed: firstFeed } },
  );
  await act(async () => {
    rerender({
      feed: homeFeed({
        mentions: [feedItem("mention-1", "mention")],
        needsAction: [feedItem("approval-1", "needs_action")],
      }),
    });
  });

  assert.deepEqual(
    writes.filter(([key]) => key === SEEN_KEY(ACCOUNT_A)),
    [],
    "desktop delivery must not write the home badge's seen key",
  );
  assert.deepEqual(
    JSON.parse(dom.window.localStorage.getItem(SEEN_KEY(ACCOUNT_A))),
    ["inbox-dismissed", "mention-old"],
  );

  // Its own record was seeded from the old shared key and then unioned with
  // the first feed instead of replaced by it.
  const notified = JSON.parse(
    dom.window.localStorage.getItem(NOTIFIED_KEY(ACCOUNT_A)),
  );
  for (const id of ["inbox-dismissed", "mention-old", "mention-1", "approval-1"]) {
    assert.ok(notified.includes(id), `${id} missing from ${notified}`);
  }
});

test("F395: the first feed after mount is unioned into the stored delivery history", async () => {
  dom.window.localStorage.setItem(
    NOTIFIED_KEY(ACCOUNT_A),
    JSON.stringify(["mention-from-last-week"]),
  );

  const { renderHook } = await import("@testing-library/react");
  const { useFeedDesktopNotifications } = await import(
    "./use-feed-desktop-notifications.ts"
  );
  renderHook(() =>
    useFeedDesktopNotifications(
      homeFeed({ mentions: [feedItem("mention-today", "mention")] }),
      ACCOUNT_A,
      QUIET_SETTINGS,
      async () => true,
      true,
      {},
    ),
  );

  assert.deepEqual(
    JSON.parse(dom.window.localStorage.getItem(NOTIFIED_KEY(ACCOUNT_A))),
    ["mention-from-last-week", "mention-today"],
  );
});
