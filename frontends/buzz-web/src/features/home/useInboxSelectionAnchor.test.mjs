import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { JSDOM } from "jsdom";

import { useInboxSelectionAnchor } from "./useInboxSelectionAnchor.ts";

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

afterEach(async () => {
  const { cleanup } = await import("@testing-library/react");
  cleanup();
  delete window.__TAURI_INTERNALS__;
});

after(() => dom.window.close());

const CHANNELS = new Set(["chan-1"]);
const A = "a".repeat(64);
const B = "b".repeat(64);

function relayEvent(id) {
  return { id, pubkey: "c".repeat(64), kind: 9, created_at: 1, content: id, tags: [["h", "chan-1"]], sig: "" };
}

function feedWith(items) {
  return { feed: { mentions: items, needsAction: [], activity: [], agentActivity: [] } };
}

function feedItem(id, channelId) {
  return { id, kind: 1618, pubkey: "c".repeat(64), content: "", createdAt: 1, tags: [], channelId, channelName: "", category: "activity" };
}

async function render(initialProps) {
  const { renderHook } = await import("@testing-library/react");
  return renderHook((props) => useInboxSelectionAnchor(props), { initialProps });
}

test("going back to an earlier cold anchor still resolves it", async () => {
  const { waitFor } = await import("@testing-library/react");
  window.__TAURI_INTERNALS__ = {
    invoke: async (command, args) =>
      command === "get_event" ? JSON.stringify(relayEvent(args.eventId)) : null,
  };
  const view = await render({ feed: feedWith([]), selectedEventId: A, availableChannelIds: CHANNELS });
  await waitFor(() => assert.equal(view.result.current.activeLatchedItem?.id, A));
  view.rerender({ feed: feedWith([]), selectedEventId: B, availableChannelIds: CHANNELS });
  await waitFor(() => assert.equal(view.result.current.activeLatchedItem?.id, B));
  view.rerender({ feed: feedWith([]), selectedEventId: A, availableChannelIds: CHANNELS });
  await waitFor(() => assert.equal(view.result.current.activeLatchedItem?.id, A));
});

test("a channel-less Project Inbox item survives eviction from the feed", async () => {
  const id = "d".repeat(64);
  const view = await render({
    feed: feedWith([feedItem(id, null)]),
    selectedEventId: id,
    availableChannelIds: CHANNELS,
  });
  assert.equal(view.result.current.activeLatchedItem?.id, id);
  view.rerender({ feed: feedWith([]), selectedEventId: id, availableChannelIds: CHANNELS });
  assert.equal(view.result.current.activeLatchedItem?.id, id);
});
