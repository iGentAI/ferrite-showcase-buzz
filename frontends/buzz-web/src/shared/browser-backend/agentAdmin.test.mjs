// `add_relay_member` with a moderator role is two relay writes with no atomic
// route. When the second is refused, the first has already landed, and the
// error must say so rather than read as "nothing happened" (F413).
import test from "node:test";
import assert from "node:assert/strict";

import { addRelayMember } from "./agentAdmin.ts";
import { getAgentConfigSurface } from "../api/tauri.ts";

const TARGET = "b".repeat(64);

function relay({ refuseRole }) {
  const calls = [];
  const post = async (path, body) => {
    calls.push([path, body]);
    if (path === "/members/role" && refuseRole) {
      throw new Error("only an operator may change community moderator status");
    }
    return { ok: true };
  };
  return { calls, post };
}

test("a refused moderator grant reports that the member was still added", async () => {
  const { calls, post } = relay({ refuseRole: true });
  await assert.rejects(
    addRelayMember({ targetPubkey: TARGET, role: "admin" }, post),
    (error) => {
      assert.match(error.message, /added bbbbbbbb… as a member/i);
      assert.match(error.message, /moderator status was refused/);
      assert.match(error.message, /only an operator may/);
      return true;
    },
  );
  assert.deepEqual(
    calls.map(([path]) => path),
    ["/members/add", "/members/role"],
  );
});

test("a granted moderator role and a plain add both succeed quietly", async () => {
  const granted = relay({ refuseRole: false });
  assert.equal(
    await addRelayMember({ targetPubkey: TARGET, role: "owner" }, granted.post),
    null,
  );
  assert.deepEqual(granted.calls, [
    ["/members/add", { pubkey: TARGET }],
    ["/members/role", { pubkey: TARGET, role: "admin" }],
  ]);
  const plain = relay({ refuseRole: true });
  await addRelayMember({ targetPubkey: TARGET }, plain.post);
  assert.deepEqual(plain.calls, [["/members/add", { pubkey: TARGET }]]);
});

test("a failed add is not reworded as a partial success", async () => {
  const post = async () => {
    throw new Error("not a moderator");
  };
  await assert.rejects(
    addRelayMember({ targetPubkey: TARGET, role: "admin" }, post),
    /^Error: not a moderator$/,
  );
});

test("getAgentConfigSurface returns a valid RuntimeConfigSurface shape for workspace agents", async () => {
  const originalWindow = globalThis.window;
  const originalFetch = globalThis.fetch;
  const store = new Map();
  globalThis.window = {
    location: { origin: "https://relay.example" },
    localStorage: {
      getItem: (k) => store.get(k) ?? null,
      setItem: (k, v) => store.set(k, String(v)),
      removeItem: (k) => store.delete(k),
    },
  };
  globalThis.fetch = async (input) => {
    const url = String(input);
    let body = {};
    if (url.endsWith("/buzz/api/agents")) {
      body = { agents: [{ agent: "helper", pubkey: TARGET }] };
    } else if (url.endsWith("/buzz/api/agents/config")) {
      body = {
        agent: "helper",
        instructions: "Stay concise.",
        model: "claude-sonnet-4",
        harness: "",
      };
    } else if (url.endsWith("/buzz/api/agent-models")) {
      body = { models: [{ id: "claude-sonnet-4" }, { id: "gpt-5" }] };
    }
    return new Response(JSON.stringify(body), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    });
  };
  try {
    const surface = await getAgentConfigSurface(TARGET);
    assert.equal(surface.runtimeId, "ferrite-cloud");
    assert.equal(surface.runtimeLabel, "ferrite-cloud");
    assert.equal(surface.isPreSpawn, false);
    assert.equal(surface.normalized.model?.value, "claude-sonnet-4");
    assert.equal(surface.normalized.model?.origin, "buzzExplicit");
    assert.equal(surface.normalized.systemPrompt?.value, "Stay concise.");
    assert.equal(surface.normalized.thinkingEffort, null);
    assert.deepEqual(surface.advanced, []);
    assert.deepEqual(surface.extensions, []);
    assert.equal(surface.sources.mcpConfigFilePath, null);
    assert.equal(surface.sources.configFilePath, null);
    assert.doesNotThrow(() => Object.entries(surface.normalized));
  } finally {
    if (originalWindow === undefined) delete globalThis.window;
    else globalThis.window = originalWindow;
    globalThis.fetch = originalFetch;
  }
});

test("putAgentSessionConfig writes against the generation it read and rebuilds on conflict", async () => {
  const { putAgentSessionConfig } = await import("./agentAdmin.ts");
  const writes = [];
  let generation = 4;
  let instructions = "old";
  const post = async (_path, body) => {
    if (!body.set) return { instructions, model: "m", generation };
    writes.push(body);
    if (body.if_generation !== generation) {
      throw Object.assign(new Error("conflict"), { status: 409 });
    }
    generation += 1;
    return { ok: true };
  };
  // A concurrent edit lands after the first read.
  let first = true;
  const racing = async (path, body) => {
    const reply = await post(path, body);
    if (first && !body.set) {
      first = false;
      instructions = "concurrent";
      generation += 1;
    }
    return reply;
  };
  await putAgentSessionConfig({ agent: "agent-1", payload: { model: "m2" } }, racing);
  assert.equal(writes.length, 2, "the stale write is refused, then rebuilt");
  assert.equal(writes[0].if_generation, 4);
  assert.equal(writes[1].set.instructions, "concurrent", "the concurrent edit survives");
  assert.equal(writes[1].set.default_model, "m2");
});
