import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { JSDOM } from "jsdom";
import React from "react";

import { mergeLiveChannelWindowEvent } from "./lib/channelWindowStore.ts";
import { channelMessagesKey, channelWindowKey } from "./lib/messageQueryKeys.ts";
import { projectChannelWindowMessages } from "./lib/projectChannelWindow.ts";
import { useLoadMissingAncestors } from "./useLoadMissingAncestors.ts";

// F393: a fetched ancestor was written only to the flattened timeline cache,
// which every live event re-projects from the window store, so it vanished at
// the next projection; and the id stayed marked as requested, so it was never
// fetched again (nor after a failed fetch).

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

const CHANNEL = {
  id: "channel-a",
  name: "general",
  channelType: "stream",
  visibility: "open",
  description: "",
  topic: null,
  purpose: null,
  memberCount: 1,
  memberPubkeys: [],
  lastMessageAt: null,
  archivedAt: null,
  participants: [],
  participantPubkeys: [],
  isMember: true,
  ttlSeconds: null,
  ttlDeadline: null,
};

function relayEvent(id, createdAt, tags = []) {
  return {
    id: id.padEnd(64, "0"),
    pubkey: "a".repeat(64),
    created_at: createdAt,
    kind: 9,
    tags: [["h", "channel-a"], ...tags],
    content: id,
    sig: "b".repeat(128),
  };
}

// A thread root older than the loaded window, and a broadcast reply to it
// that the window does hold.
const ROOT = relayEvent("0d", 10);
const REPLY = relayEvent("c", 200, [
  ["e", ROOT.id, "", "root"],
  ["e", ROOT.id, "", "reply"],
  ["broadcast", "1"],
]);

function seededClient() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  client.setQueryData(channelWindowKey(CHANNEL.id), {
    pages: [
      {
        startCursor: null,
        rows: [{ event: REPLY, thread: null }],
        aux: [],
        nextCursor: null,
        hasMore: false,
      },
    ],
    liveOverlay: [],
    liveAux: [],
    liveSummaries: {},
  });
  client.setQueryData(channelMessagesKey(CHANNEL.id), [REPLY]);
  return client;
}

function installBackend(answer) {
  const requested = [];
  window.__TAURI_INTERNALS__ = {
    invoke: (command, args) => {
      if (command !== "get_event") return null;
      requested.push(args.eventId);
      return answer(args.eventId, requested.length);
    },
  };
  return requested;
}

async function renderAncestors(client, messages) {
  const { renderHook } = await import("@testing-library/react");
  return renderHook(
    ({ current }) => useLoadMissingAncestors(CHANNEL, current, async () => []),
    {
      initialProps: { current: messages },
      wrapper: ({ children }) =>
        React.createElement(QueryClientProvider, { client }, children),
    },
  );
}

const timelineIds = (client) =>
  (client.getQueryData(channelMessagesKey(CHANNEL.id)) ?? []).map(
    (event) => event.id,
  );

test("a loaded ancestor survives the next live event's projection (F393)", async () => {
  const { waitFor } = await import("@testing-library/react");
  const requested = installBackend(() => JSON.stringify(ROOT));
  const client = seededClient();
  await renderAncestors(client, [REPLY]);

  await waitFor(() => assert.ok(timelineIds(client).includes(ROOT.id)));
  assert.deepEqual(requested, [ROOT.id]);

  // A live message arrives: the store is merged and the timeline re-projected,
  // exactly as the channel subscription does it.
  const live = relayEvent("d", 300);
  client.setQueryData(channelWindowKey(CHANNEL.id), (current) =>
    mergeLiveChannelWindowEvent(current, live),
  );
  projectChannelWindowMessages(client, CHANNEL.id);

  // Oldest first, as the timeline cache is ordered.
  assert.deepEqual(timelineIds(client), [ROOT.id, REPLY.id, live.id]);
});

test("a failed ancestor fetch is retried on a later pass (F393)", async () => {
  const { waitFor } = await import("@testing-library/react");
  const requested = installBackend((_id, attempt) => {
    if (attempt === 1) throw new Error("relay unavailable");
    return JSON.stringify(ROOT);
  });
  const client = seededClient();
  const originalError = console.error;
  console.error = () => {};
  try {
    const view = await renderAncestors(client, [REPLY]);
    await waitFor(() => assert.equal(requested.length, 1));
    // Give the rejection time to settle before the next pass.
    await new Promise((resolve) => setTimeout(resolve, 10));

    view.rerender({ current: [REPLY] });
    await waitFor(() => assert.ok(timelineIds(client).includes(ROOT.id)));
    assert.deepEqual(requested, [ROOT.id, ROOT.id]);
  } finally {
    console.error = originalError;
  }
});

test("an ancestor still lands when new messages arrive mid-fetch (F393)", async () => {
  const { act, waitFor } = await import("@testing-library/react");
  let release;
  installBackend(
    () =>
      new Promise((resolve) => {
        release = () => resolve(JSON.stringify(ROOT));
      }),
  );
  const client = seededClient();
  const view = await renderAncestors(client, [REPLY]);
  await waitFor(() => assert.equal(typeof release, "function"));

  // Another message renders while the ancestor request is in flight.
  view.rerender({ current: [relayEvent("e", 250), REPLY] });
  await act(async () => release());

  await waitFor(() => assert.ok(timelineIds(client).includes(ROOT.id)));
});

test("a deleted ancestor is not asked for again on every new message", async () => {
  const { waitFor } = await import("@testing-library/react");
  const requested = installBackend(() => {
    throw new Error("event not found");
  });
  const client = seededClient();
  const originalError = console.error;
  console.error = () => {};
  try {
    const view = await renderAncestors(client, [REPLY]);
    await waitFor(() => assert.equal(requested.length, 1));
    await new Promise((resolve) => setTimeout(resolve, 10));
    view.rerender({ current: [REPLY] });
    await new Promise((resolve) => setTimeout(resolve, 10));
    assert.deepEqual(requested, [ROOT.id], "a definitive miss is remembered");
  } finally {
    console.error = originalError;
  }
});

test("an optimistic or malformed ancestor id is never looked up", async () => {
  const requested = installBackend(() => JSON.stringify(ROOT));
  const client = seededClient();
  const reply = relayEvent("e", 300, [["e", "optimistic-123", "", "reply"]]);
  await renderAncestors(client, [reply]);
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.deepEqual(requested, []);
});
