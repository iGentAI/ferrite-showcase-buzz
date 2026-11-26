/**
 * Local cache updates for a deleted message (F378) and a failed optimistic
 * send (F379), exercised against a real QueryClient and the real projection.
 *
 * The window store is the source of truth: every live merge re-projects it
 * over the flattened `channelMessagesKey` array. So a delete patched only into
 * the flattened array came back with the next live event, and a failed send
 * rolled back by restoring pre-send snapshots dropped every event that had
 * arrived meanwhile (or, with no window at snapshot time, left a ghost
 * "sending" row forever).
 */
import assert from "node:assert/strict";
import test from "node:test";

import { QueryClient } from "@tanstack/react-query";

import {
  emptyChannelWindowStore,
  mergeLiveChannelWindowEvent,
  removeChannelWindowEvents,
} from "./channelWindowStore.ts";
import {
  applyLocalMessageDeletion,
  rollbackOptimisticSend,
} from "./localMessageCacheUpdates.ts";
import {
  channelMessagesKey,
  channelWindowKey,
  threadRepliesKey,
} from "./messageQueryKeys.ts";
import { projectChannelWindowMessages } from "./projectChannelWindow.ts";

const CHANNEL_ID = "36411e44-0e2d-4cfe-bd6e-567eb169db9f";
const ME = "1".repeat(64);
const OTHER = "2".repeat(64);

function event(id, createdAt, overrides = {}) {
  return {
    id,
    pubkey: ME,
    kind: 9,
    created_at: createdAt,
    content: `message ${id.slice(0, 4)}`,
    tags: [["h", CHANNEL_ID]],
    sig: "sig",
    ...overrides,
  };
}

function pageOf(...events) {
  return {
    startCursor: null,
    rows: events.map((row) => ({ event: row, thread: null })),
    aux: [],
    nextCursor: null,
    hasMore: false,
  };
}

function seededClient(store) {
  const queryClient = new QueryClient();
  if (store) {
    queryClient.setQueryData(channelWindowKey(CHANNEL_ID), store);
    projectChannelWindowMessages(queryClient, CHANNEL_ID);
  }
  return queryClient;
}

/** What `useChannelSubscription.appendMessage` does with a live row. */
function deliverLive(queryClient, live) {
  const key = channelWindowKey(CHANNEL_ID);
  const current =
    queryClient.getQueryData(key) ?? emptyChannelWindowStore();
  queryClient.setQueryData(key, mergeLiveChannelWindowEvent(current, live));
  projectChannelWindowMessages(queryClient, CHANNEL_ID);
}

function renderedIds(queryClient) {
  return (queryClient.getQueryData(channelMessagesKey(CHANNEL_ID)) ?? []).map(
    (row) => row.id,
  );
}

test("removeChannelWindowEvents drops ids from pages, aux and live lists", () => {
  const a = event("a".repeat(64), 100);
  const b = event("b".repeat(64), 101);
  const c = event("c".repeat(64), 102);
  const aux = event("d".repeat(64), 103, { kind: 7 });
  const store = {
    ...emptyChannelWindowStore(),
    pages: [{ ...pageOf(b, a), aux: [aux] }],
    liveOverlay: [c],
    liveAux: [aux],
  };
  const next = removeChannelWindowEvents(store, new Set([a.id, c.id, aux.id]));
  assert.deepEqual(
    next.pages[0].rows.map((row) => row.event.id),
    [b.id],
  );
  assert.deepEqual(next.pages[0].aux, []);
  assert.deepEqual(next.liveOverlay, []);
  assert.deepEqual(next.liveAux, []);
  assert.equal(
    removeChannelWindowEvents(store, new Set(["f".repeat(64)])),
    store,
    "unchanged store keeps its identity",
  );
});

test("a deleted message stays gone after the next live event (F378)", () => {
  const kept = event("a".repeat(64), 100);
  const deleted = event("b".repeat(64), 101);
  const queryClient = seededClient({
    ...emptyChannelWindowStore(),
    pages: [pageOf(deleted, kept)],
  });
  const reply = event("c".repeat(64), 102, {
    tags: [
      ["h", CHANNEL_ID],
      ["e", kept.id, "", "root"],
    ],
  });
  queryClient.setQueryData(threadRepliesKey(CHANNEL_ID, kept.id), [
    reply,
    { ...deleted },
  ]);

  applyLocalMessageDeletion(queryClient, CHANNEL_ID, deleted.id);
  assert.deepEqual(renderedIds(queryClient), [kept.id]);

  deliverLive(queryClient, event("e".repeat(64), 200, { pubkey: OTHER }));
  assert.equal(
    renderedIds(queryClient).includes(deleted.id),
    false,
    "the next live merge must not re-project the deleted row",
  );
  assert.deepEqual(
    queryClient
      .getQueryData(threadRepliesKey(CHANNEL_ID, kept.id))
      .map((row) => row.id),
    [reply.id],
    "an open thread panel drops the deleted row too",
  );
});

test("a failed send removes only its optimistic row and keeps live events that arrived meanwhile (F379)", () => {
  const history = event("a".repeat(64), 100);
  const queryClient = seededClient({
    ...emptyChannelWindowStore(),
    pages: [pageOf(history)],
  });

  // onMutate: the optimistic row joins the live overlay.
  const optimistic = event("f".repeat(64), 150, { pending: true });
  deliverLive(queryClient, optimistic);
  // While the send is in flight, somebody else's message arrives.
  const concurrent = event("b".repeat(64), 160, { pubkey: OTHER });
  deliverLive(queryClient, concurrent);

  rollbackOptimisticSend(queryClient, CHANNEL_ID, optimistic.id);

  assert.deepEqual(renderedIds(queryClient).sort(), [history.id, concurrent.id].sort());
  const store = queryClient.getQueryData(channelWindowKey(CHANNEL_ID));
  assert.deepEqual(
    store.liveOverlay.map((row) => row.id),
    [concurrent.id],
  );
});

test("a failed send with no window at snapshot time leaves no ghost row (F379)", () => {
  const queryClient = seededClient(null);
  const optimistic = event("f".repeat(64), 150, { pending: true });
  deliverLive(queryClient, optimistic);
  assert.deepEqual(renderedIds(queryClient), [optimistic.id]);

  rollbackOptimisticSend(queryClient, CHANNEL_ID, optimistic.id);
  assert.deepEqual(renderedIds(queryClient), []);

  deliverLive(queryClient, event("b".repeat(64), 160, { pubkey: OTHER }));
  assert.deepEqual(renderedIds(queryClient), ["b".repeat(64)]);
});
