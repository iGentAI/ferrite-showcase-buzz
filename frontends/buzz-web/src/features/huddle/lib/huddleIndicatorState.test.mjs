import assert from "node:assert/strict";
import test from "node:test";

import { reconstructActiveHuddle } from "./huddleIndicatorState.ts";

const ALICE = "a".repeat(64);
const BOB = "b".repeat(64);
const RELAY = "c".repeat(64);
const MALLORY = "e".repeat(64);

const ev = (id, kind, pubkey, createdAt, room = "room", tags = []) => ({
  id,
  kind,
  pubkey,
  created_at: createdAt,
  content: JSON.stringify({ ephemeral_channel_id: room }),
  tags,
  sig: "",
});

test("a genuine huddle with relay joins is shown and its creator can end it", () => {
  const events = [ev("s", 48100, ALICE, 1), ev("j", 48101, RELAY, 2, "room", [["p", BOB]])];
  const huddle = reconstructActiveHuddle(events, RELAY);
  assert.equal(huddle?.huddleThreadEventId, "s");
  assert.equal(huddle?.participants.has(BOB), true);
  assert.equal(reconstructActiveHuddle([...events, ev("e", 48103, ALICE, 3)], RELAY), null);
});

test("finding_a3b8dc61cbfebee2: another member cannot end, hijack or spoof the huddle", () => {
  const base = [ev("s", 48100, ALICE, 1), ev("j", 48101, RELAY, 2, "room", [["p", BOB]])];

  const ended = reconstructActiveHuddle([...base, ev("x", 48103, MALLORY, 3)], RELAY);
  assert.equal(ended?.huddleThreadEventId, "s");

  const hijacked = reconstructActiveHuddle(
    [...base, ev("h", 48100, MALLORY, 3, "other")],
    RELAY,
  );
  assert.equal(hijacked?.ephemeralChannelId, "room");
  assert.equal(hijacked?.huddleThreadEventId, "s");

  const spoofed = reconstructActiveHuddle(
    [...base, ev("f", 48101, MALLORY, 3, "room", [["p", MALLORY]])],
    RELAY,
  );
  assert.equal(spoofed?.participants.has(MALLORY), false);

  assert.equal(
    reconstructActiveHuddle([ev("l", 48102, RELAY, 1, "ghost", [["p", BOB]])], RELAY),
    null,
  );
});

test("finding_a3b8dc61cbfebee2: a second START for the room revokes the creator's END", () => {
  const huddle = reconstructActiveHuddle(
    [
      ev("s", 48100, ALICE, 1),
      ev("m", 48100, MALLORY, 2),
      ev("e", 48103, ALICE, 3),
    ],
    RELAY,
  );
  assert.equal(huddle?.huddleThreadEventId, "s");
  assert.equal(
    reconstructActiveHuddle(
      [ev("s", 48100, ALICE, 1), ev("m", 48100, MALLORY, 2), ev("e", 48103, RELAY, 3)],
      RELAY,
    ),
    null,
  );
});
