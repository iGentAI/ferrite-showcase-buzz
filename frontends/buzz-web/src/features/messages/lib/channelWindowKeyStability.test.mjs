// A sent message keeps one render key for its lifetime, across the head
// refetches every reconnect and auto-heal invalidation performs. Before the
// fix the acknowledged row's key moved from `optimistic-<uuid>` to the relay id
// on the first refetch, React remounted the row, and reminders 1's
// `scrollIntoViewIfNeeded()` found it detached.
import assert from "node:assert/strict";
import test from "node:test";

import {
  emptyChannelWindowStore,
  flattenChannelWindowEvents,
  mergeLiveChannelWindowEvent,
  replaceNewestChannelWindow,
} from "./channelWindowStore.ts";
import { reconcileChannelWindowMessages } from "./channelWindowReconciliation.ts";

// The on-the-wire shape a relay page row carries: no `localKey`.
function relayEvent(id, createdAt, content = id) {
  return {
    id: id.padEnd(64, "0"),
    pubkey: "a".repeat(64),
    created_at: createdAt,
    kind: 9,
    tags: [["h", "channel"]],
    content,
    sig: "b".repeat(128),
  };
}

function newestPage(events) {
  return {
    startCursor: null,
    rows: events.map((event) => ({ event, thread: null })),
    aux: [],
    nextCursor: null,
    hasMore: false,
  };
}

const OPTIMISTIC = "optimistic-2f9d";

function renderKeys(window, cache = []) {
  return reconcileChannelWindowMessages(window, cache).map((event) => event.localKey ?? event.id);
}

test("an acknowledged row keeps its optimistic key through a head refetch", () => {
  const older = relayEvent("older", 100);
  const sent = relayEvent("sent", 200, "remind me");
  let window = replaceNewestChannelWindow(emptyChannelWindowStore(), newestPage([older]));
  // The send is acknowledged: the relay event plus the optimistic id as localKey.
  window = mergeLiveChannelWindowEvent(window, { ...sent, localKey: OPTIMISTIC });
  assert.deepEqual(renderKeys(window).sort(), [older.id, OPTIMISTIC].sort());

  // A reconnect refetches page zero, which now contains the sent row.
  const refetched = replaceNewestChannelWindow(window, newestPage([sent, older]));
  assert.equal(refetched.liveOverlay.length, 0, "the page supersedes the overlay entry");
  const row = flattenChannelWindowEvents(refetched).find((event) => event.id === sent.id);
  assert.equal(row.localKey, OPTIMISTIC);
  assert.deepEqual(renderKeys(refetched).sort(), [older.id, OPTIMISTIC].sort());

  // And a SECOND refetch keeps it too (the key now lives on a page row).
  const again = replaceNewestChannelWindow(refetched, newestPage([sent, older]));
  assert.equal(
    flattenChannelWindowEvents(again).find((event) => event.id === sent.id).localKey,
    OPTIMISTIC,
  );
});

test("an acknowledgement that arrives after the refetch adopts the page row", () => {
  const sent = relayEvent("sent", 200);
  let window = replaceNewestChannelWindow(emptyChannelWindowStore(), newestPage([sent]));
  window = mergeLiveChannelWindowEvent(window, { ...sent, localKey: OPTIMISTIC });
  assert.equal(window.liveOverlay.length, 0);
  assert.equal(flattenChannelWindowEvents(window)[0].localKey, OPTIMISTIC);
});

test("rows nobody sent from this tab are untouched", () => {
  const theirs = relayEvent("theirs", 150);
  const first = replaceNewestChannelWindow(emptyChannelWindowStore(), newestPage([theirs]));
  const second = replaceNewestChannelWindow(first, newestPage([theirs]));
  assert.equal(flattenChannelWindowEvents(second)[0].localKey, undefined);
  // The same store object comes back for an unkeyed page, so nothing re-renders.
  const page = newestPage([theirs]);
  assert.equal(replaceNewestChannelWindow(first, page).pages[0], page);
});
