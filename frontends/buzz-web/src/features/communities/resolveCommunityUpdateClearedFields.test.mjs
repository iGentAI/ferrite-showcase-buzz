/**
 * Regression tests for F366: clearing the API token or repos directory in
 * the edit dialog sends the key with an `undefined` value, which must count
 * as a change (and re-init the active community) instead of being dropped.
 */
import assert from "node:assert/strict";
import test from "node:test";

import { resolveCommunityUpdateResult } from "./useCommunities.tsx";

const COMMUNITIES = [
  {
    id: "ws-1",
    name: "Community A",
    relayUrl: "wss://relay-a.example.com",
    token: "buzz_secret",
    reposDir: "/home/me/dev",
    addedAt: "2024-01-01",
  },
  {
    id: "ws-2",
    name: "Community B",
    relayUrl: "wss://relay-b.example.com",
    addedAt: "2024-01-02",
  },
];

test("F366: clearing only the token persists and re-inits the active community", () => {
  assert.deepEqual(
    resolveCommunityUpdateResult(COMMUNITIES, "ws-1", "ws-1", {
      token: undefined,
    }),
    { kind: "updated", requiresReinit: true },
  );
});

test("F366: clearing only the repos dir persists and re-inits the active community", () => {
  assert.deepEqual(
    resolveCommunityUpdateResult(COMMUNITIES, "ws-1", "ws-1", {
      reposDir: undefined,
    }),
    { kind: "updated", requiresReinit: true },
  );
});

test("F366: clearing the token together with a rename still re-inits", () => {
  assert.deepEqual(
    resolveCommunityUpdateResult(COMMUNITIES, "ws-1", "ws-1", {
      name: "Renamed",
      token: undefined,
    }),
    { kind: "updated", requiresReinit: true },
  );
});

test("F366: clearing the token of an inactive community persists without re-init", () => {
  assert.deepEqual(
    resolveCommunityUpdateResult(COMMUNITIES, "ws-2", "ws-1", {
      token: undefined,
    }),
    { kind: "updated", requiresReinit: false },
  );
});

test("F366: an absent key, or clearing an already-empty field, is unchanged", () => {
  assert.deepEqual(
    resolveCommunityUpdateResult(COMMUNITIES, "ws-1", "ws-1", {}),
    { kind: "unchanged" },
  );
  assert.deepEqual(
    resolveCommunityUpdateResult(COMMUNITIES, "ws-1", "ws-2", {
      token: undefined,
      reposDir: undefined,
    }),
    { kind: "unchanged" },
  );
});
