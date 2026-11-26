// Regression tests for community-wide huddle presence (security review
// findings F149, F150, F377): a KIND_HUDDLE_STARTED is the publisher's own
// claim, so a member who forges one for a live session must not be able to
// reset its presence, inherit the right to end it, redirect its liveness
// lookup, or grow the tracker without bound.

import assert from "node:assert/strict";
import test from "node:test";

import {
  finalizeEvent,
  generateSecretKey,
  getPublicKey,
} from "nostr-tools/pure";

import {
  HuddlePresenceTracker,
  HuddleSessionParentRouting,
  MAX_TRACKED_HUDDLE_SESSIONS,
  MAX_UNATTESTED_HUDDLE_PARENTS,
  reconstructHuddlePresence,
} from "./huddlePresence.ts";
import { startHuddlePresenceRuntime } from "./huddlePresenceRuntime.ts";

const ALICE = "a".repeat(64);
const BOB = "b".repeat(64);
const RELAY = "c".repeat(64);
const MALLORY = "e".repeat(64);

function event({
  id,
  kind,
  pubkey = ALICE,
  session = "room",
  parent = "general",
  tags = [],
  admissionId,
  rosterRevision,
  generation,
  createdAt,
}) {
  return {
    id,
    kind,
    pubkey,
    content: JSON.stringify({
      ephemeral_channel_id: session,
      admission_id: admissionId,
      roster_revision: rosterRevision,
      generation,
    }),
    tags: parent ? [["h", parent], ...tags] : tags,
    created_at: createdAt,
    sig: "",
  };
}

const start = (options) => event({ kind: 48100, ...options });
const end = (options) => event({ kind: 48103, ...options });
const join = (options) =>
  event({
    kind: 48101,
    pubkey: RELAY,
    tags: [["p", BOB]],
    admissionId: "bob-desktop",
    rosterRevision: 1,
    ...options,
  });

// ── F149: forged START ──────────────────────────────────────────────────────

test("a forged newer START from another member does not reset a live session", () => {
  const tracker = new HuddlePresenceTracker(RELAY);
  tracker.apply(start({ id: "genuine", createdAt: 100 }));
  tracker.apply(join({ id: "join", createdAt: 101 }));
  assert.equal(tracker.snapshot().has(BOB), true);

  assert.equal(
    tracker.apply(start({ id: "forged", pubkey: MALLORY, createdAt: 200 })),
    false,
  );
  assert.equal(tracker.snapshot().has(BOB), true, "admissions survive");
});

test("a forged START cannot hand its author END authority, in either order", () => {
  // Live order: the forged START arrives after the genuine one.
  const live = new HuddlePresenceTracker(RELAY);
  live.apply(start({ id: "genuine", createdAt: 100 }));
  live.apply(join({ id: "join", createdAt: 101 }));
  live.apply(start({ id: "forged", pubkey: MALLORY, createdAt: 200 }));
  assert.equal(
    live.apply(end({ id: "forged-end", pubkey: MALLORY, createdAt: 201 })),
    false,
  );
  assert.equal(live.snapshot().has(BOB), true);
  assert.equal(
    live.apply(join({ id: "join-2", createdAt: 202, admissionId: "bob-2" })),
    true,
    "relay-signed joins still apply",
  );

  // History order: a backdated forged START sorts before the genuine one.
  const history = reconstructHuddlePresence(
    [
      start({ id: "forged", pubkey: MALLORY, createdAt: 90 }),
      start({ id: "genuine", createdAt: 100 }),
      join({ id: "join", createdAt: 101 }),
      end({ id: "forged-end", pubkey: MALLORY, createdAt: 102 }),
    ],
    RELAY,
  );
  assert.equal(history.has(BOB), true);
});

test("once a session's START is contested only the relay may end it", () => {
  const tracker = new HuddlePresenceTracker(RELAY);
  tracker.apply(start({ id: "genuine", createdAt: 100 }));
  tracker.apply(join({ id: "join", createdAt: 101 }));
  tracker.apply(start({ id: "forged", pubkey: MALLORY, createdAt: 102 }));
  assert.equal(
    tracker.apply(end({ id: "creator-end", createdAt: 103 })),
    false,
  );
  assert.equal(
    tracker.apply(end({ id: "relay-end", pubkey: RELAY, createdAt: 104 })),
    true,
  );
  assert.equal(tracker.snapshot().has(BOB), false);
});

test("an uncontested creator can still end their own huddle", () => {
  const tracker = new HuddlePresenceTracker(RELAY);
  tracker.apply(start({ id: "genuine", createdAt: 100 }));
  tracker.apply(join({ id: "join", createdAt: 101 }));
  assert.equal(tracker.apply(end({ id: "end", createdAt: 102 })), true);
  assert.equal(tracker.snapshot().has(BOB), false);
});

test("after a contested START, the first-seen author's newer START cannot reset admissions", () => {
  // History order: Mallory's backdated START is seen first, then Alice's
  // genuine one contests it, then the relay admits Bob.
  const tracker = new HuddlePresenceTracker(RELAY);
  tracker.apply(start({ id: "forged", pubkey: MALLORY, createdAt: 90 }));
  tracker.apply(start({ id: "genuine", createdAt: 100 }));
  tracker.apply(join({ id: "join", createdAt: 101 }));
  assert.equal(tracker.snapshot().has(BOB), true);

  for (const createdAt of [200, 300]) {
    assert.equal(
      tracker.apply(
        start({ id: `again-${createdAt}`, pubkey: MALLORY, createdAt }),
      ),
      false,
    );
  }
  assert.equal(tracker.snapshot().has(BOB), true, "admissions survive");
});

test("a creator's START naming a different parent channel is refused", () => {
  const tracker = new HuddlePresenceTracker(RELAY);
  tracker.apply(start({ id: "genuine", createdAt: 100 }));
  tracker.apply(join({ id: "join", createdAt: 101 }));
  assert.equal(
    tracker.apply(start({ id: "moved", parent: "design", createdAt: 150 })),
    false,
  );
  assert.equal(tracker.snapshot().has(BOB), true);
});

// ── F377: bounded session map ───────────────────────────────────────────────

test("the session map stays bounded when no session has ended", () => {
  const tracker = new HuddlePresenceTracker(RELAY);
  tracker.apply(start({ id: "real", session: "real", createdAt: 5_000 }));
  tracker.apply(join({ id: "real-join", session: "real", createdAt: 5_001 }));
  for (let index = 0; index < MAX_TRACKED_HUDDLE_SESSIONS + 500; index += 1) {
    tracker.apply(
      start({
        id: `spam-${index}`,
        pubkey: MALLORY,
        session: `spam-${index}`,
        createdAt: 1 + index,
      }),
    );
  }
  const participants = tracker.snapshot(new Set(["real"]));
  assert.ok(tracker.sessionCount <= MAX_TRACKED_HUDDLE_SESSIONS);
  assert.equal(participants.has(BOB), true, "the live session is kept");
});

test("eviction without liveness keeps sessions that have people in them", () => {
  const tracker = new HuddlePresenceTracker(RELAY);
  tracker.apply(start({ id: "real", session: "real", createdAt: 1 }));
  tracker.apply(join({ id: "real-join", session: "real", createdAt: 2 }));
  for (let index = 0; index < MAX_TRACKED_HUDDLE_SESSIONS + 10; index += 1) {
    tracker.apply(
      start({
        id: `spam-${index}`,
        pubkey: MALLORY,
        session: `spam-${index}`,
        createdAt: 10 + index,
      }),
    );
  }
  assert.equal(tracker.snapshot().has(BOB), true);
  assert.ok(tracker.sessionCount <= MAX_TRACKED_HUDDLE_SESSIONS);
});

// ── F150: parent-channel routing ────────────────────────────────────────────

test("a forged START adds a candidate parent but never replaces the first one", () => {
  const routing = new HuddleSessionParentRouting(RELAY, ["general", "design"]);
  routing.observe(start({ id: "genuine", createdAt: 100 }));
  routing.observe(
    start({ id: "forged", pubkey: MALLORY, parent: "design", createdAt: 200 }),
  );
  assert.deepEqual(routing.parentsOf("room"), ["general", "design"]);
});

test("a relay-signed lifecycle event fixes the parent and later STARTs cannot move it", () => {
  const routing = new HuddleSessionParentRouting(RELAY, ["general", "design"]);
  routing.observe(
    start({ id: "forged", pubkey: MALLORY, parent: "design", createdAt: 90 }),
  );
  routing.observe(join({ id: "join", createdAt: 101 }));
  routing.observe(
    start({ id: "forged-2", pubkey: MALLORY, parent: "design", createdAt: 300 }),
  );
  assert.deepEqual(routing.parentsOf("room"), ["general"]);
});

test("routing ignores channels the viewer is not in, forged joins, and caps candidates", () => {
  const channels = ["general", "c1", "c2", "c3", "c4", "c5"];
  const routing = new HuddleSessionParentRouting(RELAY, channels);
  routing.observe(start({ id: "outside", parent: "elsewhere", createdAt: 1 }));
  assert.deepEqual(routing.parentsOf("room"), []);
  routing.observe(
    join({ id: "forged-join", pubkey: MALLORY, parent: "c1", createdAt: 2 }),
  );
  assert.deepEqual(routing.parentsOf("room"), []);
  channels.forEach((parent, index) =>
    routing.observe(
      start({ id: `s-${index}`, pubkey: MALLORY, parent, createdAt: 3 + index }),
    ),
  );
  assert.equal(routing.parentsOf("room").length, MAX_UNATTESTED_HUDDLE_PARENTS);
});

async function settle() {
  await new Promise((resolve) => setImmediate(resolve));
  await new Promise((resolve) => setImmediate(resolve));
}

test("a forged START with the wrong parent does not hide a live huddle", async () => {
  const livenessQueries = [];
  const snapshots = [];
  const history = [
    start({ id: "genuine", createdAt: 100 }),
    join({ id: "join", createdAt: 101 }),
    // Mallory, who is in #design too, names the live session under #design.
    start({ id: "forged", pubkey: MALLORY, parent: "design", createdAt: 150 }),
  ];
  const dispose = startHuddlePresenceRuntime({
    relaySelfPubkey: RELAY,
    channelIds: ["general", "design"],
    subscribeLive: async () => () => {},
    fetchEvents: async (filter) => {
      if (!filter.kinds?.includes(48104)) return history;
      livenessQueries.push(filter);
      // The relay mints liveness only under the session's real parent.
      return filter["#h"].includes("general") && filter["#d"].includes("room")
        ? [
            {
              id: "live",
              kind: 48104,
              pubkey: RELAY,
              content: JSON.stringify({
                ephemeral_channel_id: "room",
                generation: "1",
              }),
              tags: [
                ["h", "general"],
                ["d", "room"],
              ],
              created_at: 160,
              sig: "",
            },
          ]
        : [];
    },
    subscribeToReconnects: () => () => {},
    onPresence: (participants) => snapshots.push(new Set(participants)),
    setRetryTimer: () => null,
    clearRetryTimer: () => {},
    setLivenessTimer: () => null,
    clearLivenessTimer: () => {},
    nowSeconds: () => 1_000,
    // Unsigned fixtures; the signature check has its own test below.
    verifyLifecycleEvent: () => true,
  });
  await settle();
  assert.ok(livenessQueries.length > 0);
  assert.equal(snapshots.at(-1)?.has(BOB), true);
  dispose();
});

test("an unsigned event claiming the relay's key can neither redirect liveness nor admit anyone", async () => {
  const relayKey = generateSecretKey();
  const relay = getPublicKey(relayKey);
  const aliceKey = generateSecretKey();
  const mallory = "f".repeat(64);
  const signed = (key, { kind, parent, tags = [], createdAt }) =>
    finalizeEvent(
      {
        kind,
        created_at: createdAt,
        content: JSON.stringify({
          ephemeral_channel_id: "room",
          admission_id: `adm-${createdAt}`,
          roster_revision: createdAt,
        }),
        tags: [["h", parent], ...tags],
      },
      key,
    );
  const genuineJoin = signed(relayKey, {
    kind: 48101,
    parent: "general",
    tags: [["p", BOB]],
    createdAt: 101,
  });
  // Same shape as a relay join, claiming the relay's key, but not signed by it.
  const forgedJoin = {
    ...signed(aliceKey, {
      kind: 48101,
      parent: "design",
      tags: [["p", mallory]],
      createdAt: 150,
    }),
    pubkey: relay,
  };
  const history = [
    signed(aliceKey, { kind: 48100, parent: "general", createdAt: 100 }),
    genuineJoin,
    forgedJoin,
  ];
  const livenessQueries = [];
  const snapshots = [];
  const dispose = startHuddlePresenceRuntime({
    relaySelfPubkey: relay,
    channelIds: ["general", "design"],
    subscribeLive: async () => () => {},
    fetchEvents: async (filter) => {
      if (!filter.kinds?.includes(48104)) return history;
      livenessQueries.push(filter);
      // Liveness is relay-signed like every authoritative relay event.
      return filter["#h"].includes("general")
        ? [
            finalizeEvent(
              {
                kind: 48104,
                content: JSON.stringify({
                  ephemeral_channel_id: "room",
                  generation: "1",
                }),
                tags: [
                  ["h", "general"],
                  ["d", "room"],
                ],
                created_at: 160,
              },
              relayKey,
            ),
          ]
        : [];
    },
    subscribeToReconnects: () => () => {},
    onPresence: (participants) => snapshots.push(new Set(participants)),
    setRetryTimer: () => null,
    clearRetryTimer: () => {},
    setLivenessTimer: () => null,
    clearLivenessTimer: () => {},
    nowSeconds: () => 1_000,
  });
  await settle();
  assert.ok(
    livenessQueries.every((filter) => !filter["#h"].includes("design")),
    "liveness is never asked under the forged parent",
  );
  assert.equal(snapshots.at(-1)?.has(BOB), true, "the live huddle is shown");
  assert.equal(snapshots.at(-1)?.has(mallory), false, "the forgery admits no one");
  dispose();
});

test("finding_45b3cf9efb0b6a4d: unsigned liveness claiming the relay's key keeps no generation", async () => {
  const relayKey = generateSecretKey();
  const relay = getPublicKey(relayKey);
  const sign = (event) => finalizeEvent(event, relayKey);
  const content = JSON.stringify({ ephemeral_channel_id: "room", generation: "1" });
  const tags = [["h", "general"], ["d", "room"]];
  const run = async (liveness) => {
    const snapshots = [];
    const dispose = startHuddlePresenceRuntime({
      relaySelfPubkey: relay,
      channelIds: ["general"],
      subscribeLive: async () => () => {},
      fetchEvents: async (filter) =>
        filter.kinds?.includes(48104)
          ? [liveness]
          : [
              finalizeEvent(
                {
                  kind: 48100,
                  created_at: 100,
                  content: JSON.stringify({ ephemeral_channel_id: "room" }),
                  tags: [["h", "general"]],
                },
                generateSecretKey(),
              ),
              sign({
                kind: 48101,
                created_at: 101,
                content: JSON.stringify({
                  ephemeral_channel_id: "room",
                  admission_id: "adm",
                  roster_revision: 1,
                }),
                tags: [["h", "general"], ["p", BOB]],
              }),
            ],
      subscribeToReconnects: () => () => {},
      onPresence: (participants) => snapshots.push(new Set(participants)),
      setRetryTimer: () => null,
      clearRetryTimer: () => {},
      setLivenessTimer: () => null,
      clearLivenessTimer: () => {},
      nowSeconds: () => 1_000,
    });
    await settle();
    dispose();
    return snapshots.at(-1)?.has(BOB) ?? false;
  };

  const genuine = sign({ kind: 48104, created_at: 160, content, tags });
  const forged = { ...genuine, id: "f".repeat(64), sig: "" };
  assert.equal(await run(genuine), true, "a relay-signed liveness keeps the room");
  assert.equal(await run(forged), false, "an unsigned one does not");
});

test("finding_80ac99a47c8ad44b: a backdated START and END pair cannot keep a live huddle hidden", () => {
  const tracker = new HuddlePresenceTracker(RELAY);
  tracker.apply(start({ id: "forged-start", pubkey: MALLORY, createdAt: 50 }));
  tracker.apply(end({ id: "forged-end", pubkey: MALLORY, createdAt: 51 }));
  tracker.apply(start({ id: "genuine", createdAt: 100 }));
  tracker.apply(join({ id: "join", createdAt: 101 }));
  assert.equal(tracker.snapshot().has(BOB), true);
});

test("finding_80ac99a47c8ad44b: a relay-signed END still stands after a contested START", () => {
  const tracker = new HuddlePresenceTracker(RELAY);
  tracker.apply(start({ id: "genuine", createdAt: 100 }));
  tracker.apply(end({ id: "relay-end", pubkey: RELAY, createdAt: 110 }));
  tracker.apply(start({ id: "forged", pubkey: MALLORY, createdAt: 120 }));
  tracker.apply(join({ id: "join", createdAt: 121 }));
  assert.equal(tracker.snapshot().has(BOB), false);
});
