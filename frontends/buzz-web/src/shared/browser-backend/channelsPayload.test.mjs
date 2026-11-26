import assert from "node:assert/strict";
import test from "node:test";

import { channelsPayload, stableChannelsHash } from "./channelsPayload.ts";
import { getChannels } from "../api/tauriChannels.ts";

const row = (id, lastMessageAt) => ({
  id,
  name: id,
  channel_type: "stream",
  visibility: "open",
  description: "",
  topic: null,
  purpose: null,
  member_count: 1,
  last_message_at: lastMessageAt,
  archived_at: null,
  participants: [],
  participant_pubkeys: [],
  is_member: true,
  ttl_seconds: null,
  ttl_deadline: null,
});

test("get_channels answers upstream's payload, not a bare row array", async () => {
  const rows = [row("general", "2026-09-11T20:00:00Z"), row("random", null)];
  const payload = await channelsPayload(rows, null);
  assert.equal(Array.isArray(payload), false);
  assert.equal(payload.channels, rows);
  assert.match(payload.hash, /^[0-9a-f]{16}$/);
  assert.deepEqual(payload.last_messages, { general: "2026-09-11T20:00:00Z" });
});

test("a matching knownHash answers not-modified, and last_messages still ships", async () => {
  const rows = [row("general", "2026-09-11T20:00:00Z")];
  const first = await channelsPayload(rows, null);
  const second = await channelsPayload(rows, first.hash);
  assert.equal(second.channels, null);
  assert.deepEqual(second.last_messages, first.last_messages);
  assert.notEqual((await channelsPayload(rows, "0000000000000000")).channels, null);
});

test("a new message changes timestamps without changing the stable hash", async () => {
  const before = await stableChannelsHash([row("general", "2026-09-11T20:00:00Z")]);
  const after = await stableChannelsHash([row("general", "2026-09-11T21:00:00Z")]);
  const renamed = await stableChannelsHash([{ ...row("general", null), name: "lobby" }]);
  assert.equal(before, after);
  assert.notEqual(before, renamed);
});

test("the vendored client consumes the payload without throwing", async () => {
  const rows = [row("general", "2026-09-11T20:00:00Z")];
  const payload = await channelsPayload(rows, null);
  globalThis.window ??= globalThis;
  const previous = globalThis.__TAURI_INTERNALS__;
  globalThis.__TAURI_INTERNALS__ = {
    invoke: async (cmd) => {
      assert.equal(cmd, "get_channels");
      return payload;
    },
    transformCallback: () => 0,
  };
  try {
    const answer = await getChannels(null);
    assert.equal(answer.channels?.[0]?.id, "general");
    assert.equal(answer.hash, payload.hash);
  } finally {
    globalThis.__TAURI_INTERNALS__ = previous;
  }
});

test("the browser seam's get_channels dispatch answers that payload", async () => {
  // The production dispatch, not the helper alone: reverting the case to a bare
  // row array fails here. The relay read and the signer are the only stubs.
  globalThis.window ??= globalThis;
  const previousNostr = globalThis.window.nostr;
  globalThis.window.nostr = { getPublicKey: async () => "ab".repeat(32) };
  const { relayClient } = await import("../api/relayClient.ts");
  const previousFetch = relayClient.fetchEvents;
  relayClient.fetchEvents = async (filter) =>
    filter.kinds[0] === 39000
      ? [{ id: "1", pubkey: "cd".repeat(32), kind: 39000, created_at: 10, content: "", sig: "", tags: [["d", "general"], ["name", "general"]] }]
      : [{ id: "2", pubkey: "cd".repeat(32), kind: 39002, created_at: 10, content: "", sig: "", tags: [["d", "general"], ["p", "ab".repeat(32)]] }];
  try {
    const { handleAppCommand } = await import("./commands.ts");
    const answer = await handleAppCommand("get_channels", { knownHash: null });
    assert.equal(Array.isArray(answer), false);
    assert.match(answer.hash, /^[0-9a-f]{16}$/);
    assert.equal(answer.channels[0].id, "general");
    assert.equal(answer.channels[0].is_member, true);
    const repeat = await handleAppCommand("get_channels", { knownHash: answer.hash });
    assert.equal(repeat.channels, null);
  } finally {
    relayClient.fetchEvents = previousFetch;
    globalThis.window.nostr = previousNostr;
  }
});
