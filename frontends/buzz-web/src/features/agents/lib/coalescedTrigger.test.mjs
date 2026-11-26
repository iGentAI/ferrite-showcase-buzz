import assert from "node:assert/strict";
import test from "node:test";

import { createCoalescedTrigger } from "./coalescedTrigger.ts";

// F345: relay events from any community member must not be able to force a
// full catalog refetch per event.

function fakeClock() {
  let now = 0;
  let nextId = 1;
  const timers = new Map();
  return {
    clock: {
      now: () => now,
      setTimer: (callback, ms) => {
        const id = nextId++;
        timers.set(id, { at: now + ms, callback });
        return id;
      },
      clearTimer: (id) => timers.delete(id),
    },
    advance(ms) {
      now += ms;
      for (const [id, timer] of [...timers]) {
        if (timer.at <= now) {
          timers.delete(id);
          timer.callback();
        }
      }
    },
    pendingTimers: () => timers.size,
  };
}

test("a burst of events costs one immediate run and one trailing run", () => {
  const { clock, advance } = fakeClock();
  let runs = 0;
  const trigger = createCoalescedTrigger(() => runs++, 10_000, clock);

  for (let i = 0; i < 100; i++) trigger.trigger();
  assert.equal(runs, 1, "the first event refreshes at once");

  advance(9_999);
  assert.equal(runs, 1, "no refetch inside the interval");
  advance(1);
  assert.equal(runs, 2, "the rest of the burst coalesces into one trailing run");

  advance(60_000);
  assert.equal(runs, 2, "nothing further without new events");
});

test("sustained events are held to one run per interval", () => {
  const { clock, advance } = fakeClock();
  let runs = 0;
  const trigger = createCoalescedTrigger(() => runs++, 10_000, clock);

  // One event per second for a minute (the relay's per-author rate cap).
  for (let second = 0; second < 60; second++) {
    trigger.trigger();
    advance(1_000);
  }
  assert.ok(runs <= 7, `expected at most 7 refetches, got ${runs}`);
});

test("an event after a quiet interval refreshes immediately", () => {
  const { clock, advance } = fakeClock();
  let runs = 0;
  const trigger = createCoalescedTrigger(() => runs++, 10_000, clock);
  trigger.trigger();
  advance(30_000);
  trigger.trigger();
  assert.equal(runs, 2);
});

test("dispose cancels a pending trailing run", () => {
  const { clock, advance, pendingTimers } = fakeClock();
  let runs = 0;
  const trigger = createCoalescedTrigger(() => runs++, 10_000, clock);
  trigger.trigger();
  trigger.trigger();
  assert.equal(pendingTimers(), 1);
  trigger.dispose();
  assert.equal(pendingTimers(), 0);
  advance(20_000);
  trigger.trigger();
  assert.equal(runs, 1);
});
