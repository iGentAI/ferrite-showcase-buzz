import assert from "node:assert/strict";
import test from "node:test";

import {
  AUTO_RESTART_QUIESCENCE_MS,
  decideAutoRestart,
  executeAutoRestart,
  nextEdgeState,
  stillFiresBeforeRestart,
} from "./autoRestartPolicy.ts";

// ── Chunk F policy matrix ────────────────────────────────────────────────────
//
// SAFETY-CRITICAL: stop is SIGTERM → ≤1s → SIGKILL with no in-process drain,
// so a wrong "fire" here kills a mid-turn agent. The never-fire rows below
// are exhaustive over every gate; each row flips exactly one input away from
// the all-green baseline to prove that gate alone holds the line.

/** All-green inputs: every gate open, window satisfied — the ONLY fire case. */
function greenInputs(overrides = {}) {
  return {
    autoRestartEnabled: true,
    needsRestart: true,
    working: false,
    workingSource: "none",
    connected: true,
    isLocalBackend: true,
    isRunning: true,
    edgeConsumed: false,
    quiescentForMs: AUTO_RESTART_QUIESCENCE_MS,
    ...overrides,
  };
}

test("fires only when every gate is green and the window has elapsed", () => {
  assert.equal(decideAutoRestart(greenInputs()), "fire");
});

// ── never-fire rows: one gate red at a time ─────────────────────────────────

const NEVER_FIRE_ROWS = [
  ["opt-out toggle off", { autoRestartEnabled: false }],
  ["no config drift", { needsRestart: false }],
  [
    "agent mid-turn (working, observer)",
    { working: true, workingSource: "observer" },
  ],
  [
    "typing source counts as working",
    { working: true, workingSource: "typing" },
  ],
  ["working flag alone defers (defensive)", { working: true }],
  [
    "source alone defers even if working flag lies (defensive)",
    { workingSource: "observer" },
  ],
  ["typing source alone defers", { workingSource: "typing" }],
  ["observer relay not connected", { connected: false }],
  ["remote backend", { isLocalBackend: false }],
  ["agent not running", { isRunning: false }],
  [
    "edge already consumed (one attempt per rising edge)",
    { edgeConsumed: true },
  ],
];

for (const [label, overrides] of NEVER_FIRE_ROWS) {
  test(`never fires: ${label}`, () => {
    assert.equal(
      decideAutoRestart(greenInputs(overrides)),
      "hold",
      `${label} must hold — a fire here is a kill`,
    );
  });
}

// ── the quiescence window ───────────────────────────────────────────────────

test("arms (does not fire) before the window elapses", () => {
  assert.equal(decideAutoRestart(greenInputs({ quiescentForMs: 0 })), "arm");
  assert.equal(
    decideAutoRestart(
      greenInputs({ quiescentForMs: AUTO_RESTART_QUIESCENCE_MS - 1 }),
    ),
    "arm",
  );
});

test("window is minutes-scale — far beyond the 25s turn-store prune", () => {
  // A relay hiccup makes a mid-turn agent look idle after 25s; the window
  // must dwarf that so the flicker resets it long before firing.
  assert.ok(AUTO_RESTART_QUIESCENCE_MS >= 2 * 60 * 1000);
});

// ── edge-trigger state machine ───────────────────────────────────────────────

test("falling needsRestart edge re-arms a consumed edge", () => {
  const consumed = { consumed: true, armedAt: null };
  const next = nextEdgeState(consumed, {
    needsRestart: false,
    isRunning: true,
  });
  assert.deepEqual(next, { consumed: false, armedAt: null });
});

test("agent stop re-arms a consumed edge (manual stop/start cycle can auto-fire again)", () => {
  const consumed = { consumed: true, armedAt: null };
  const next = nextEdgeState(consumed, {
    needsRestart: true,
    isRunning: false,
  });
  assert.deepEqual(next, { consumed: false, armedAt: null });
});

test("a held rising edge preserves consumed state (failed attempt badges only)", () => {
  const consumed = { consumed: true, armedAt: null };
  const next = nextEdgeState(consumed, { needsRestart: true, isRunning: true });
  assert.equal(next.consumed, true, "no retry until the edge cycles");
});

test("undefined prior state initializes un-consumed and un-armed", () => {
  assert.deepEqual(
    nextEdgeState(undefined, { needsRestart: true, isRunning: true }),
    { consumed: false, armedAt: null },
  );
});

// ── F344: the pre-fire re-check is the full predicate ───────────────────────

function liveInputs(overrides = {}) {
  const { edgeConsumed: _edge, quiescentForMs: _window, ...live } =
    greenInputs();
  return { ...live, ...overrides };
}

test("pre-fire re-check passes only when every live gate is still green", () => {
  assert.equal(stillFiresBeforeRestart(liveInputs()), true);
});

for (const [name, overrides] of [
  ["observer disconnected", { connected: false }],
  ["backend no longer local", { isLocalBackend: false }],
  ["opt-out toggled off", { autoRestartEnabled: false }],
  ["drift cleared", { needsRestart: false }],
  ["agent not running", { isRunning: false }],
  ["agent working", { working: true, workingSource: "observer" }],
  ["typing fallback", { workingSource: "typing" }],
]) {
  test(`pre-fire re-check holds when ${name}`, () => {
    assert.equal(stillFiresBeforeRestart(liveInputs(overrides)), false);
  });
}

// ── F342: a stop that lands must not leave the agent silently stopped ───────

function recorder({
  stopFails = false,
  startFailures = 0,
  stillOwnsRetry = async () => true,
} = {}) {
  const calls = [];
  let failuresLeft = startFailures;
  return {
    calls,
    deps: {
      stop: async () => {
        calls.push("stop");
        if (stopFails) throw new Error("stop refused");
      },
      afterStop: () => calls.push("clear-turns"),
      start: async () => {
        calls.push("start");
        if (failuresLeft > 0) {
          failuresLeft -= 1;
          throw new Error("relay mesh preflight failed");
        }
      },
      sleep: async (ms) => {
        calls.push(`sleep ${ms}`);
      },
      stillOwnsRetry: async () => {
        calls.push("owns?");
        return stillOwnsRetry();
      },
      retryDelaysMs: [10, 20],
    },
  };
}

test("a clean restart stops, clears turns, and starts once", async () => {
  const { calls, deps } = recorder();
  assert.deepEqual(await executeAutoRestart(deps), { kind: "restarted" });
  assert.deepEqual(calls, ["stop", "clear-turns", "start"]);
});

test("a start that fails after a successful stop is retried", async () => {
  const { calls, deps } = recorder({ startFailures: 2 });
  assert.deepEqual(await executeAutoRestart(deps), { kind: "restarted" });
  assert.deepEqual(calls, [
    "stop",
    "clear-turns",
    "start",
    "sleep 10",
    "owns?",
    "start",
    "sleep 20",
    "owns?",
    "start",
  ]);
});

test("a start that keeps failing is reported, not swallowed", async () => {
  const { calls, deps } = recorder({ startFailures: 5 });
  const outcome = await executeAutoRestart(deps);
  assert.equal(outcome.kind, "start-failed");
  assert.equal(outcome.attempts, 3);
  assert.match(String(outcome.error), /relay mesh preflight failed/);
  assert.equal(calls.filter((call) => call === "start").length, 3);
});

test("a failed stop never starts and reports that the agent was not stopped", async () => {
  const { calls, deps } = recorder({ stopFails: true });
  const outcome = await executeAutoRestart(deps);
  assert.equal(outcome.kind, "stop-failed");
  assert.deepEqual(calls, ["stop"]);
});

test("a scope switch during backoff cancels the retry before it starts anything", async () => {
  // The community (or identity) changes while the first backoff sleeps.
  let scope = "community-a";
  const { calls, deps } = recorder({
    startFailures: 5,
    stillOwnsRetry: async () => scope === "community-a",
  });
  const realSleep = deps.sleep;
  deps.sleep = async (ms) => {
    await realSleep(ms);
    scope = "community-b";
  };
  const outcome = await executeAutoRestart(deps);
  assert.equal(outcome.kind, "retry-cancelled");
  assert.equal(outcome.attempts, 1);
  assert.deepEqual(calls, ["stop", "clear-turns", "start", "sleep 10", "owns?"]);
});

test("a retry is cancelled when its ownership check fails or throws", async () => {
  const { calls, deps } = recorder({
    startFailures: 5,
    stillOwnsRetry: async () => {
      throw new Error("listManagedAgents failed");
    },
  });
  const outcome = await executeAutoRestart(deps);
  assert.equal(outcome.kind, "retry-cancelled");
  assert.equal(calls.filter((call) => call === "start").length, 1);
});
