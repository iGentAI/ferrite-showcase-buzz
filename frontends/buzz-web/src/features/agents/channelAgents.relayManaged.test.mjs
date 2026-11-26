import assert from "node:assert/strict";
import test from "node:test";

import { provisionChannelManagedAgent } from "./channelAgents.ts";

test("provisioning a relay-managed or command-less runtime fails cleanly before any backend call", async (t) => {
  const calls = [];
  const prior = globalThis.window;
  globalThis.window ??= {};
  window.__TAURI_INTERNALS__ = {
    invoke: (command) => {
      calls.push(command);
      return Promise.resolve([]);
    },
  };
  t.after(() => {
    globalThis.window = prior;
  });

  for (const runtime of [
    { id: "ferrite-cloud", command: null },
    { id: "goose", command: null },
  ]) {
    await assert.rejects(
      provisionChannelManagedAgent(
        { name: "Cloud", runtime, systemPrompt: "" },
        { managedAgents: [], channelMemberPubkeys: new Set(), personas: [] },
      ),
      /runs on the relay/,
    );
  }
  assert.deepEqual(calls, []);
});
