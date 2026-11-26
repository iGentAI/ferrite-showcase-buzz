import assert from "node:assert/strict";
import test from "node:test";

import { readAllPages } from "./channelDirectory.ts";

/** A relay that serves newest-first pages of at most `pageSize` events. */
function pagedRelay(events, pageSize) {
  const calls = [];
  const fetchPage = async (filter) => {
    calls.push(filter);
    return events
      .filter((event) => filter.until === undefined || event.created_at <= filter.until)
      .sort((a, b) => b.created_at - a.created_at)
      .slice(0, pageSize);
  };
  return { calls, fetchPage };
}

const event = (id, created_at) => ({ id, created_at, tags: [["d", id]] });

test("an old room beyond the first page is still read", async () => {
  // The staging shape: `general` written long ago, newer rooms fill page one.
  const events = [event("general", 10), ...Array.from({ length: 5 }, (_, i) => event(`room-${i}`, 100 + i))];
  const relay = pagedRelay(events, 3);
  const all = await readAllPages({ kinds: [39000], limit: 500 }, relay.fetchPage);
  assert.deepEqual(all.map((e) => e.id).sort(), ["general", "room-0", "room-1", "room-2", "room-3", "room-4"]);
  assert.equal(relay.calls[0].until, undefined);
  assert.deepEqual(relay.calls[0].kinds, [39000]);
});

test("events sharing the boundary second are not skipped, and the walk moves past them", async () => {
  const events = [event("x", 60), event("a", 50), event("b", 50), event("c", 40), event("d", 30)];
  const relay = pagedRelay(events, 2);
  const all = await readAllPages({ kinds: [39000] }, relay.fetchPage);
  assert.deepEqual(all.map((e) => e.id).sort(), ["a", "b", "c", "d", "x"]);
});

test("the walk is bounded", async () => {
  let n = 0;
  const endless = async () => [event(`e-${n++}`, 1_000 - n)];
  const all = await readAllPages({ kinds: [39000] }, endless, 4);
  assert.equal(all.length, 4);
});

test("readAllPages steps past a one-second page and never sends a negative until", async () => {
  const seen = [];
  const fetchPage = async (filter) => {
    seen.push(filter.until);
    if (filter.until === undefined) {
      return [
        { id: "a", created_at: 5 },
        { id: "b", created_at: 5 },
      ];
    }
    if (filter.until === 4) return [{ id: "c", created_at: 0 }];
    return [];
  };
  const events = await readAllPages({ kinds: [1] }, fetchPage, 10);
  assert.deepEqual(events.map((event) => event.id).sort(), ["a", "b", "c"]);
  assert.deepEqual(seen, [undefined, 4]);
});
