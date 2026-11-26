// Review finding 2 (PRs #51-#59): every by-id channel read shares one resolver,
// and that resolver carries a room this tab created until the relay projection
// serves it. Before, only the sidebar list carried it, so the sidebar showed the
// new room while `get_channel_details` for it answered "No channel <id>".
import assert from "node:assert/strict";
import test from "node:test";

import { channelProjectionFrom } from "./commands.ts";
import { forgetCreatedChannel, rememberCreatedChannel } from "./channels.ts";

const ME = "c".repeat(64);
const RELAY = "d".repeat(64);

function projection(kind, room, createdAt, tags) {
  return {
    id: `${kind}-${room}-${createdAt}`.padEnd(64, "0"),
    pubkey: RELAY,
    kind,
    created_at: createdAt,
    tags: [["d", room], ...tags],
    content: "",
    sig: "e".repeat(128),
  };
}

function reader(events) {
  return async (filter) =>
    events
      .filter((event) => filter.kinds.includes(event.kind))
      .filter((event) => filter.until === undefined || event.created_at <= filter.until);
}

const createdRoom = {
  id: "5f0c1d2e-0000-4000-8000-000000000001",
  name: "billing-race",
  channel_type: "stream",
  visibility: "open",
  description: "",
  topic: null,
  purpose: null,
  member_count: 1,
  member_pubkeys: [ME],
  last_message_at: null,
  archived_at: null,
  participants: [],
  participant_pubkeys: [ME],
  is_member: true,
  ttl_seconds: null,
  ttl_deadline: null,
};

test("a room created from this tab resolves by id before the projection serves it", async () => {
  const general = [
    projection(39000, "general", 100, [["name", "general"], ["t", "stream"]]),
    projection(39002, "general", 100, [["p", ME]]),
  ];
  rememberCreatedChannel(createdRoom);
  try {
    const rows = await channelProjectionFrom(reader(general), ME);
    const ids = rows.map((row) => row.id);
    assert.ok(ids.includes("general"));
    assert.ok(ids.includes(createdRoom.id), "the created room is carried by the shared resolver");

    // Once the relay serves it, the served row wins and the carried copy retires.
    const served = [
      ...general,
      projection(39000, createdRoom.id, 200, [["name", "billing-race"], ["t", "stream"]]),
      projection(39002, createdRoom.id, 200, [["p", ME]]),
    ];
    const after = await channelProjectionFrom(reader(served), ME);
    assert.equal(after.filter((row) => row.id === createdRoom.id).length, 1);
    const again = await channelProjectionFrom(reader(general), ME);
    assert.ok(!again.some((row) => row.id === createdRoom.id), "retired once served");
  } finally {
    forgetCreatedChannel(createdRoom.id);
  }
});
