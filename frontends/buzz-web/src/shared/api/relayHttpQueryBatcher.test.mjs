import assert from "node:assert/strict";
import test from "node:test";

import { HttpRelayQueryBatcher } from "./relayHttpQueryBatcher.ts";

test("coalesces, deduplicates and preserves each caller's result", async () => {
  const runs = [];
  const batcher = new HttpRelayQueryBatcher(
    async (filters) => {
      runs.push(filters);
      return [
        { id: "a", kind: 1 },
        { id: "b", kind: 2 },
      ];
    },
    (filter, event) => filter.kind === event.kind,
    (filter) => JSON.stringify(filter),
    (event) => event.id,
    8,
    0,
  );

  const [ones, duplicateOnes, twos] = await Promise.all([
    batcher.query({ kind: 1 }),
    batcher.query({ kind: 1 }),
    batcher.query({ kind: 2 }),
  ]);

  assert.deepEqual(runs, [[{ kind: 1 }, { kind: 2 }]]);
  assert.deepEqual(ones.map((event) => event.id), ["a"]);
  assert.deepEqual(duplicateOnes.map((event) => event.id), ["a"]);
  assert.deepEqual(twos.map((event) => event.id), ["b"]);
});

test("chunks at the relay limit and rejects every caller on failure", async () => {
  const runs = [];
  const batcher = new HttpRelayQueryBatcher(
    async (filters) => {
      runs.push(filters);
      if (runs.length === 2) throw new Error("relay unavailable");
      return [];
    },
    () => true,
    (filter) => String(filter.id),
    (event) => String(event.id),
    2,
    0,
  );

  const settled = await Promise.allSettled([
    batcher.query({ id: 1 }),
    batcher.query({ id: 2 }),
    batcher.query({ id: 3 }),
  ]);

  assert.deepEqual(runs, [
    [{ id: 1 }, { id: 2 }],
    [{ id: 3 }],
  ]);
  assert.ok(settled.every((result) => result.status === "rejected"));
});

test("deduplicates events returned by overlapping chunks", async () => {
  const batcher = new HttpRelayQueryBatcher(
    async () => [{ id: "shared", kind: 1 }],
    (filter, event) => filter.kind === event.kind,
    (filter) => String(filter.id),
    (event) => event.id,
    1,
    0,
  );

  const [first] = await Promise.all([
    batcher.query({ id: 1, kind: 1 }),
    batcher.query({ id: 2, kind: 1 }),
  ]);

  assert.deepEqual(first, [{ id: "shared", kind: 1 }]);
});
