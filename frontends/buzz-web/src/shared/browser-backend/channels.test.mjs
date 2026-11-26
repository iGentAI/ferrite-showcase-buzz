import assert from "node:assert/strict";
import test from "node:test";

import { channelCreateBody, RECENTLY_CREATED_TTL_MS, withRecentlyCreated } from "./channels.ts";
import { isValidProjectChannelId } from "../../features/projects/projectModels.ts";

test("an unpinned room is created under a UUID the synced client accepts as a project channel", () => {
  const body = channelCreateBody({ name: "runbooks-mtxs6seu-3wl8" });
  assert.equal(body.name, "runbooks-mtxs6seu-3wl8");
  assert.equal(body.type, "stream");
  assert.equal(body.visibility, "open");
  assert.ok(isValidProjectChannelId(body.id), `not a UUID: ${body.id}`);
  // The regression: the relay's slug for the same name is refused.
  assert.equal(isValidProjectChannelId("runbooks-mtxs6seu-3wl8"), false);
});

test("two rooms with the same label never share an id", () => {
  const a = channelCreateBody({ name: "Welcome", visibility: "private" });
  const b = channelCreateBody({ name: "Welcome", visibility: "private" });
  assert.notEqual(a.id, b.id);
});

test("a pinned starter room keeps its shared id, type and ttl pass through", () => {
  const body = channelCreateBody(
    { name: "general", id: "general", channelType: "forum", ttlSeconds: 3600, description: "d" },
    () => "never-used",
  );
  assert.deepEqual(body, {
    name: "general",
    type: "forum",
    visibility: "open",
    id: "general",
    description: "d",
    ttl_seconds: "3600",
  });
});

function created(id, name = id) {
  return {
    id,
    name,
    channel_type: "stream",
    visibility: "private",
    description: "",
    topic: null,
    purpose: null,
    member_count: 1,
    member_pubkeys: ["me"],
    last_message_at: null,
    archived_at: null,
    participants: [],
    participant_pubkeys: ["me"],
    is_member: true,
    ttl_seconds: null,
    ttl_deadline: null,
  };
}

test("a room the relay accepted stays listed while the projection has not served it yet", () => {
  const store = new Map([["incident-room", { row: created("incident-room"), createdAtMs: 1_000 }]]);
  // The racing refetch: the projection answers without the new room.
  const racing = withRecentlyCreated([{ id: "general" }], 2_000, store);
  assert.deepEqual(racing.map((row) => row.id), ["general", "incident-room"]);
  assert.equal(store.size, 1, "still carried until the projection serves it");
  // The projection catches up: its own row wins and the carried entry retires.
  const caughtUp = withRecentlyCreated([{ id: "general" }, { id: "incident-room", name: "from relay" }], 3_000, store);
  assert.deepEqual(caughtUp.map((row) => row.name ?? row.id), ["general", "from relay"]);
  assert.equal(store.size, 0);
});

test("a carried room retires after the TTL even if the projection never serves it", () => {
  const store = new Map([["ghost", { row: created("ghost"), createdAtMs: 0 }]]);
  const rows = withRecentlyCreated([], RECENTLY_CREATED_TTL_MS + 1, store);
  assert.deepEqual(rows, []);
  assert.equal(store.size, 0);
});
