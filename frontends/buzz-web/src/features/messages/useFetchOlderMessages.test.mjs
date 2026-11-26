import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { JSDOM } from "jsdom";
import React from "react";

import { windowBoundsKey } from "@/shared/api/channelWindowKey";
import { channelWindowKey } from "./lib/messageQueryKeys.ts";
import { useFetchOlderMessages } from "./useFetchOlderMessages.ts";

// F392: the paging guard compared two values captured from the same render,
// so it was always true and a page fetched for a channel the user had left
// was still appended to that channel's window.

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

const CURSOR = { createdAt: 100, eventId: "e".repeat(64) };

function channel(id) {
  return {
    id,
    name: id,
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
}

function relayEvent(id, kind, createdAt, content = "", tags = []) {
  return {
    id: id.padEnd(64, "0"),
    pubkey: "a".repeat(64),
    created_at: createdAt,
    kind,
    tags: [["h", "channel-a"], ...tags],
    content,
    sig: "b".repeat(128),
  };
}

/** A window with one retained page whose tail still has older history. */
function seededClient() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const newest = relayEvent("f", 9, 200, "newest");
  client.setQueryData(channelWindowKey("channel-a"), {
    pages: [
      {
        startCursor: null,
        rows: [{ event: newest, thread: null }],
        aux: [],
        nextCursor: CURSOR,
        hasMore: true,
      },
    ],
    liveOverlay: [],
    liveAux: [],
    liveSummaries: {},
  });
  return client;
}

/** The relay's answer for the page after CURSOR: one older row, then the start. */
const OLDER_PAGE = [
  relayEvent("1", 9, 50, "older"),
  relayEvent(
    "9",
    39006,
    300,
    JSON.stringify({ has_more: false, next_cursor: null }),
    [["d", windowBoundsKey("channel-a", CURSOR)]],
  ),
];

function installBackend() {
  const pending = [];
  window.__TAURI_INTERNALS__ = {
    invoke: (command) => {
      if (command === "get_channel_window") {
        return new Promise((resolve) => pending.push(resolve));
      }
      return null;
    },
  };
  return pending;
}

async function renderOlderPaging(client, initial) {
  const { renderHook } = await import("@testing-library/react");
  return renderHook(({ current }) => useFetchOlderMessages(current), {
    initialProps: { current: initial },
    wrapper: ({ children }) =>
      React.createElement(QueryClientProvider, { client }, children),
  });
}

test("an older page for the channel on screen is appended (control)", async () => {
  const { act, waitFor } = await import("@testing-library/react");
  const pending = installBackend();
  const client = seededClient();
  const view = await renderOlderPaging(client, channel("channel-a"));

  let fetching;
  act(() => {
    fetching = view.result.current.fetchOlder();
  });
  await waitFor(() => assert.equal(pending.length, 1));
  await act(async () => {
    pending[0](OLDER_PAGE);
    await fetching;
  });
  assert.equal(client.getQueryData(channelWindowKey("channel-a")).pages.length, 2);
});

test("an older page that lands after a channel switch is discarded (F392)", async () => {
  const { act, waitFor } = await import("@testing-library/react");
  const pending = installBackend();
  const client = seededClient();
  const before = client.getQueryData(channelWindowKey("channel-a"));
  const view = await renderOlderPaging(client, channel("channel-a"));

  let fetching;
  act(() => {
    fetching = view.result.current.fetchOlder();
  });
  await waitFor(() => assert.equal(pending.length, 1));

  // The user moves to another channel while the request is in flight.
  view.rerender({ current: channel("channel-b") });
  await act(async () => {
    pending[0](OLDER_PAGE);
    await fetching;
  });

  assert.equal(
    client.getQueryData(channelWindowKey("channel-a")),
    before,
    "the left channel's window is not touched",
  );
});

test("a page started before A -> B -> A is discarded, and its lock never reaches B", async () => {
  const { act, waitFor } = await import("@testing-library/react");
  const pending = installBackend();
  const client = seededClient();
  const before = client.getQueryData(channelWindowKey("channel-a"));
  const view = await renderOlderPaging(client, channel("channel-a"));

  let fetching;
  act(() => {
    fetching = view.result.current.fetchOlder();
  });
  await waitFor(() => assert.equal(pending.length, 1));

  view.rerender({ current: channel("channel-b") });
  assert.equal(view.result.current.isFetchingOlder, false, "B is not shown loading");
  view.rerender({ current: channel("channel-a") });
  await act(async () => {
    pending[0](OLDER_PAGE);
    await fetching;
  });
  assert.equal(
    client.getQueryData(channelWindowKey("channel-a")),
    before,
    "the stale page does not land after re-entering A",
  );
});
