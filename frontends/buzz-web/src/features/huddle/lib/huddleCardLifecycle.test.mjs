import assert from "node:assert/strict";
import test from "node:test";

import { reconstructHuddleCardLifecycle } from "./huddleCardLifecycle.ts";

const CREATOR = "a".repeat(64);
const RELAY = "c".repeat(64);
const MALLORY = "e".repeat(64);
const BOB = "b".repeat(64);

const ev = (id, kind, pubkey, createdAt, tags = []) => ({
  id,
  kind,
  pubkey,
  created_at: createdAt,
  content: JSON.stringify({ ephemeral_channel_id: "room" }),
  tags,
  sig: "",
});
const options = {
  ephemeralChannelId: "room",
  startEventId: "start",
  creatorPubkey: CREATOR,
  relaySelfPubkey: RELAY,
};
const start = ev("start", 48100, CREATOR, 1);

test("finding_64bc3bfe6d0f41ca: another member cannot end, resurrect or populate the card", () => {
  const state = reconstructHuddleCardLifecycle(
    [
      start,
      ev("forged-join", 48101, MALLORY, 2, [["p", MALLORY]]),
      ev("forged-end", 48103, MALLORY, 3),
    ],
    options,
  );
  assert.equal(state.ended, false);
  assert.equal(state.participants.has(MALLORY), false);

  const resurrected = reconstructHuddleCardLifecycle(
    [start, ev("end", 48103, RELAY, 2), ev("forged-start", 48100, MALLORY, 3)],
    options,
  );
  assert.equal(resurrected.ended, true);
});

test("finding_64bc3bfe6d0f41ca: relay joins count, and a contested creator cannot end", () => {
  const live = reconstructHuddleCardLifecycle(
    [start, ev("join", 48101, RELAY, 2, [["p", BOB]])],
    options,
  );
  assert.equal(live.participants.has(BOB), true);

  assert.equal(
    reconstructHuddleCardLifecycle([start, ev("end", 48103, CREATOR, 2)], options).ended,
    true,
  );
  assert.equal(
    reconstructHuddleCardLifecycle(
      [start, ev("other", 48100, MALLORY, 2), ev("end", 48103, CREATOR, 3)],
      options,
    ).ended,
    false,
  );
});
