import assert from "node:assert/strict";
import test from "node:test";

import {
  AGENT_MANAGEMENT_REQUEST,
  createInputFromRequest,
  requestTargetsEditablePersona,
  parseAgentManagementRequest,
  updateInputFromRequest,
} from "./agentManagement.ts";
import {
  behaviorForSubmit,
  draftFromBehavior,
} from "./ui/personaBehaviorDraft.ts";

const CHANNEL_ID = "7c07e659-3610-42f4-9a5e-1e9973c09da9";

function createPayload(overrides = {}) {
  return {
    type: AGENT_MANAGEMENT_REQUEST,
    action: "create",
    requestId: "request-1",
    request: {
      channelId: CHANNEL_ID,
      displayName: "Research helper",
      systemPrompt: "Find reliable sources and summarize them.",
    },
    ...overrides,
  };
}

test("parses the narrow no-secret create request", () => {
  assert.deepEqual(
    parseAgentManagementRequest(createPayload()),
    createPayload(),
  );
});

test("rejects an agent-management request with extra secret-shaped fields", () => {
  const payload = createPayload();
  payload.request.apiKey = "should-not-be-accepted";

  assert.equal(parseAgentManagementRequest(payload), null);
});

test("chat creation cannot choose runtime, provider, model, or access", () => {
  for (const [field, value] of [
    ["runtime", "claude"],
    ["provider", "anthropic"],
    ["model", "claude-opus"],
    ["respondTo", "anyone"],
  ]) {
    const payload = createPayload();
    payload.request[field] = value;
    assert.equal(parseAgentManagementRequest(payload), null);
  }
});

test("chat creation leaves advanced behavior unset so the form stays collapsed", () => {
  const parsed = parseAgentManagementRequest(createPayload());
  assert.ok(parsed && parsed.action === "create");

  assert.deepEqual(createInputFromRequest(parsed), {
    displayName: "Research helper",
    systemPrompt: "Find reliable sources and summarize them.",
  });
});

test("requires the originating channel for profile updates", () => {
  const payload = {
    type: AGENT_MANAGEMENT_REQUEST,
    action: "update",
    requestId: "request-2",
    request: {
      agentName: "Review helper",
      systemPrompt: "Review changes concisely.",
    },
  };

  assert.equal(parseAgentManagementRequest(payload), null);
});

test("uses an agent's current name, never an internal profile ID", () => {
  const payload = {
    type: AGENT_MANAGEMENT_REQUEST,
    action: "update",
    requestId: "request-3",
    request: {
      channelId: CHANNEL_ID,
      agentName: "Review helper",
      systemPrompt: "Review changes concisely.",
    },
  };

  assert.deepEqual(parseAgentManagementRequest(payload), payload);
});

test("allows agents to update only personal, editable profiles", () => {
  assert.equal(
    requestTargetsEditablePersona({ isBuiltIn: false, sourceTeam: null }),
    true,
  );
  assert.equal(
    requestTargetsEditablePersona({ isBuiltIn: true, sourceTeam: null }),
    true,
  );
  assert.equal(
    requestTargetsEditablePersona({ isBuiltIn: false, sourceTeam: "team" }),
    false,
  );
});

// F340: the result of updateInputFromRequest seeds both the dialog draft and
// the baseline behaviorForSubmit diffs against, so an agent-requested access
// mode must not reach it. Previously this test pinned the opposite: touching
// parallelism submitted the agent's "anyone" as if the user had chosen it.
test("agent-requested access edits keep the stored access policy and conversation context", () => {
  const request = {
    type: AGENT_MANAGEMENT_REQUEST,
    action: "update",
    requestId: "request-4",
    request: {
      channelId: CHANNEL_ID,
      agentName: "Review helper",
      respondTo: "anyone",
    },
  };
  const updated = updateInputFromRequest(request, {
    id: "review-helper",
    displayName: "Review helper",
    systemPrompt: "Review changes concisely.",
    behavior: {
      respondTo: "owner-only",
      respondToAllowlist: [],
      parallelism: 2,
      sessionPolicy: "thread",
    },
  });

  assert.equal(updated.behavior?.sessionPolicy, "thread");
  assert.equal(updated.behavior?.respondTo, "owner-only");

  const seed = draftFromBehavior(updated.behavior);
  const edited = { ...seed, parallelism: "3" };
  assert.deepEqual(behaviorForSubmit(edited, seed, true), {
    respondTo: "owner-only",
    respondToAllowlist: undefined,
    parallelism: 3,
    sessionPolicy: "thread",
  });
});

test("an agent-requested access mode never replaces a stored allowlist", () => {
  const request = {
    type: AGENT_MANAGEMENT_REQUEST,
    action: "update",
    requestId: "request-5",
    request: {
      channelId: CHANNEL_ID,
      agentName: "Review helper",
      systemPrompt: "Review changes thoroughly.",
      respondTo: "anyone",
    },
  };
  const stored = {
    respondTo: "allowlist",
    respondToAllowlist: ["c".repeat(64)],
    parallelism: 1,
    sessionPolicy: "channel",
  };
  const updated = updateInputFromRequest(request, {
    id: "review-helper",
    displayName: "Review helper",
    systemPrompt: "Review changes concisely.",
    behavior: stored,
  });

  // The requested prompt change applies; the access policy is the stored one.
  assert.equal(updated.systemPrompt, "Review changes thoroughly.");
  assert.deepEqual(updated.behavior, stored);
  // With no behavior edit, Save leaves the stored group untouched.
  const seed = draftFromBehavior(updated.behavior);
  assert.equal(behaviorForSubmit(seed, seed, true), undefined);
});

test("an update request that only asks to change access is not a request", () => {
  const payload = {
    type: AGENT_MANAGEMENT_REQUEST,
    action: "update",
    requestId: "request-6",
    request: {
      channelId: CHANNEL_ID,
      agentName: "Review helper",
      respondTo: "anyone",
    },
  };
  assert.equal(parseAgentManagementRequest(payload), null);

  payload.request.model = "claude-opus";
  const parsed = parseAgentManagementRequest(payload);
  assert.equal(parsed?.request.model, "claude-opus");
  assert.equal("respondTo" in (parsed?.request ?? {}), false);
});

test("updateInputFromRequest_runtimeSwitch_clearsModelEffortAndLockedProviderKey", () => {
  const current = {
    id: "p1",
    displayName: "Ada",
    systemPrompt: "x",
    runtime: "buzz-agent",
    provider: "openai",
    model: "gpt-5",
    envVars: {
      BUZZ_AGENT_THINKING_EFFORT: "high",
      OPENAI_API_KEY: "sk-test",
      KEEP_ME: "1",
    },
  };
  const next = updateInputFromRequest(
    {
      type: "agent_management_request",
      action: "update",
      requestId: "r1",
      request: { runtime: "claude" },
    },
    current,
  );
  assert.equal(next.runtime, "claude");
  assert.equal(next.model, "", "the previous runtime's model is cleared");
  assert.equal(next.provider, "", "claude has no provider selection");
  assert.equal(next.envVars.BUZZ_AGENT_THINKING_EFFORT, undefined);
  assert.equal(next.envVars.KEEP_ME, "1");
});

test("updateInputFromRequest_explicitModel_isKept", () => {
  const next = updateInputFromRequest(
    {
      type: "agent_management_request",
      action: "update",
      requestId: "r2",
      request: { runtime: "claude", model: "claude-sonnet" },
    },
    {
      id: "p1",
      displayName: "Ada",
      systemPrompt: "x",
      runtime: "buzz-agent",
      provider: "openai",
      model: "gpt-5",
    },
  );
  assert.equal(next.model, "claude-sonnet");
});

test("updateInputFromRequest_noRuntimeOrProviderChange_keepsEverything", () => {
  const current = {
    id: "p1",
    displayName: "Ada",
    systemPrompt: "x",
    runtime: "buzz-agent",
    provider: "openai",
    model: "gpt-5",
    envVars: { OPENAI_API_KEY: "sk-test" },
  };
  const next = updateInputFromRequest(
    {
      type: "agent_management_request",
      action: "update",
      requestId: "r3",
      request: { displayName: "Ada 2" },
    },
    current,
  );
  assert.equal(next.model, "gpt-5");
  assert.equal(next.provider, "openai");
  assert.deepEqual(next.envVars, { OPENAI_API_KEY: "sk-test" });
});
