/**
 * Regression tests for F374: clearCommunities() skipped the relay-scoped
 * cache purge that removeCommunity() performs, leaving a backed-out
 * community's snapshots to be rehydrated if the relay was added again.
 */
import assert from "node:assert/strict";
import test from "node:test";

import { channelSnapshotKey } from "../channels/channelSnapshot.ts";
import { projectSnapshotKey } from "../projects/projectSnapshot.ts";
import { userLabelCacheKey } from "../profile/lib/userLabelStorage.ts";
import {
  clearAllCommunityLocalState,
  purgeCommunityLocalState,
} from "./useCommunities.tsx";

const PURGE_STEPS = [
  "removeSelfProfileCachesForRelay",
  "removeUserLabelCacheForRelay",
  "removeChannelSnapshotForRelay",
  "removeProjectSnapshotForRelay",
  "clearPersistedChannelHeads",
  "clearSavedCommunitySnapshot",
  "removeCommunityDestination",
];

function recordingPurge() {
  const calls = [];
  const purge = Object.fromEntries(
    PURGE_STEPS.map((step) => [step, (arg) => calls.push([step, arg])]),
  );
  return { calls, purge };
}

function expectedCalls(community) {
  return PURGE_STEPS.map((step) => [
    step,
    step === "clearSavedCommunitySnapshot" ||
    step === "removeCommunityDestination"
      ? community.id
      : community.relayUrl,
  ]);
}

const A = { id: "c-a", relayUrl: "wss://a.example" };
const B = { id: "c-b", relayUrl: "wss://b.example" };

test("F374: clearing all communities runs the full per-community purge first", () => {
  const { calls, purge } = recordingPurge();

  clearAllCommunityLocalState([A, B], purge, () => calls.push(["clear"]));

  assert.deepEqual(calls, [
    ...expectedCalls(A),
    ...expectedCalls(B),
    ["clear"],
  ]);
});

test("F374: single removal uses the same purge set", () => {
  const { calls, purge } = recordingPurge();
  purgeCommunityLocalState(A, purge);
  assert.deepEqual(calls, expectedCalls(A));
});

test("F374: the default purge removes the relay's real snapshot caches", () => {
  const values = new Map();
  const storage = {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, String(value)),
    removeItem: (key) => values.delete(key),
    key: (index) => Array.from(values.keys())[index] ?? null,
    get length() {
      return values.size;
    },
  };
  globalThis.localStorage = storage;
  globalThis.window = { localStorage: storage };
  const owner = "d".repeat(64);
  const cleared = [
    channelSnapshotKey(A.relayUrl, owner),
    projectSnapshotKey(A.relayUrl, owner),
    userLabelCacheKey(A.relayUrl),
  ];
  const kept = [
    channelSnapshotKey("wss://other.example", owner),
    userLabelCacheKey("wss://other.example"),
  ];
  for (const key of [...cleared, ...kept]) values.set(key, "{}");

  const warn = console.warn;
  console.warn = () => {};
  try {
    clearAllCommunityLocalState([A], undefined, () => {});
  } finally {
    console.warn = warn;
  }

  for (const key of cleared) assert.equal(values.has(key), false, key);
  for (const key of kept) assert.equal(values.has(key), true, key);
});
