import assert from "node:assert/strict";
import test from "node:test";

import {
  chunkHttpRelaySubscriptions,
  dispatchHttpRelayEvents,
  groupHttpRelaySubscriptions,
  nextHttpRelayPollDelay,
} from "./relayHttpPoller.ts";

test("HTTP relay subscriptions are capped to the server's eight-filter query", () => {
  const chunks = chunkHttpRelaySubscriptions(Array.from({ length: 19 }, (_, i) => i));
  assert.deepEqual(chunks.map((chunk) => chunk.length), [8, 8, 3]);
  assert.deepEqual(chunks.flat(), Array.from({ length: 19 }, (_, i) => i));
});

test("one batched response is routed to matching subscriptions exactly once", () => {
  const received = { alpha: [], beta: [] };
  const subscriptions = [
    {
      filter: "alpha",
      seen: new Set(),
      onEvent: (event) => received.alpha.push(event.id),
    },
    {
      filter: "beta",
      seen: new Set(),
      onEvent: (event) => received.beta.push(event.id),
    },
  ];
  const events = [
    { id: "a", room: "alpha" },
    { id: "b", room: "beta" },
    { id: "shared", room: "both" },
  ];
  const matches = (filter, event) =>
    event.room === filter || event.room === "both";

  assert.equal(dispatchHttpRelayEvents(subscriptions, events, matches), 4);
  assert.equal(dispatchHttpRelayEvents(subscriptions, events, matches), 0);
  assert.deepEqual(received, {
    alpha: ["a", "shared"],
    beta: ["b", "shared"],
  });
});

test("equivalent filters share one server query without sharing delivery state", () => {
  const received = [[], [], []];
  const subscriptions = [
    { filter: { kinds: [1] }, seen: new Set(), onEvent: (event) => received[0].push(event.id) },
    { filter: { kinds: [1] }, seen: new Set(), onEvent: (event) => received[1].push(event.id) },
    { filter: { kinds: [7] }, seen: new Set(), onEvent: (event) => received[2].push(event.id) },
  ];
  const groups = groupHttpRelaySubscriptions(
    subscriptions,
    (filter) => JSON.stringify(filter),
  );
  assert.equal(groups.length, 2);
  assert.deepEqual(groups.map((group) => group.subscriptions.length), [2, 1]);
  assert.notEqual(groups[0].subscriptions[0].seen, groups[0].subscriptions[1].seen);
});

test("invalid poll batch sizes fail before network fan-out", () => {
  assert.throws(() => chunkHttpRelaySubscriptions([1], 0), /positive integer/);
  assert.throws(() => chunkHttpRelaySubscriptions([1], 1.5), /positive integer/);
});

test("idle HTTP relay polling backs off and activity wakes it", () => {
  assert.equal(nextHttpRelayPollDelay(1_000, 0), 2_000);
  assert.equal(nextHttpRelayPollDelay(2_000, 0), 4_000);
  assert.equal(nextHttpRelayPollDelay(4_000, 0), 8_000);
  assert.equal(nextHttpRelayPollDelay(8_000, 0), 8_000);
  assert.equal(nextHttpRelayPollDelay(8_000, 1), 1_000);
});
