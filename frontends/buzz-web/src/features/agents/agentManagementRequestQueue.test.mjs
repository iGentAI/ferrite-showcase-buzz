/**
 * Agent-initiated management requests: which one is on screen, and when.
 *
 * Before this queue, useAgentManagement recorded a request id as seen before
 * checking whether a dialog was already open, so a request arriving while
 * another was shown was dropped for good (a retry with the same id was ignored
 * too), the seen set grew all session, and an agent could re-open the dialog
 * as soon as the user dismissed it. The dialog also never said which agent
 * asked. These tests pin the replacement behaviour.
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  AgentManagementRequestQueue,
  describeAgentRequester,
} from "./agentManagementRequestQueue.ts";

const AGENT_A = "A".repeat(64);
const AGENT_B = "b".repeat(64);

function req(requestId, action = "update") {
  return { requestId, action, request: { channelId: "chan-1", agentName: "Ada" } };
}

function clock(start = 1_000) {
  const c = { t: start, now: () => c.t };
  return c;
}

describe("AgentManagementRequestQueue", () => {
  it("queues a request that arrives while another is shown, and shows it next", () => {
    const q = new AgentManagementRequestQueue();
    assert.equal(q.offer(AGENT_A, req("r1")), "shown");
    assert.equal(q.offer(AGENT_B, req("r2")), "queued");
    assert.equal(q.hasSeen("r2"), false, "not recorded until it is shown");

    const next = q.close("saved");
    assert.equal(next?.request.requestId, "r2");
    assert.equal(q.current?.request.requestId, "r2");
    assert.equal(q.hasSeen("r2"), true);
  });

  it("a request dropped while busy can still be shown when the agent retries", () => {
    const c = clock();
    const q = new AgentManagementRequestQueue({ now: c.now });
    q.offer(AGENT_A, req("r1"));
    // Agent A's second request is refused while its first is on screen...
    assert.equal(q.offer(AGENT_A, req("r2")), "rate_limited");
    q.close("saved");
    // ...but it was not recorded, so the retry with the same id is shown.
    assert.equal(q.offer(AGENT_A, req("r2")), "shown");
  });

  it("ignores exact duplicates of a shown or queued request", () => {
    const q = new AgentManagementRequestQueue();
    q.offer(AGENT_A, req("r1"));
    q.offer(AGENT_B, req("r2"));
    assert.equal(q.offer(AGENT_A, req("r1")), "duplicate");
    assert.equal(q.offer(AGENT_B, req("r2")), "duplicate");
    q.close("saved");
    q.close("saved");
    assert.equal(q.offer(AGENT_A, req("r1")), "duplicate", "already shown once");
  });

  it("after a dismissal the agent is ignored for the cooldown, then may ask again", () => {
    const c = clock();
    const q = new AgentManagementRequestQueue({ now: c.now, cooldownMs: 60_000 });
    q.offer(AGENT_A, req("r1"));
    q.close("dismissed");

    c.t += 30_000;
    assert.equal(q.offer(AGENT_A, req("r2")), "rate_limited");
    assert.equal(q.hasSeen("r2"), false);
    // Another agent is unaffected.
    assert.equal(q.offer(AGENT_B, req("r3")), "shown");
    q.close("saved");

    c.t += 30_001;
    assert.equal(q.offer(AGENT_A, req("r2")), "shown");
  });

  it("a save starts no cooldown", () => {
    const q = new AgentManagementRequestQueue({ now: clock().now });
    q.offer(AGENT_A, req("r1"));
    q.close("saved");
    assert.equal(q.offer(AGENT_A, req("r2")), "shown");
  });

  it("matches agents case-insensitively", () => {
    const q = new AgentManagementRequestQueue();
    q.offer(AGENT_A, req("r1"));
    assert.equal(q.offer(AGENT_A.toLowerCase(), req("r2")), "rate_limited");
  });

  it("bounds the waiting queue", () => {
    const q = new AgentManagementRequestQueue({ maxQueued: 2 });
    q.offer("1".repeat(64), req("r1"));
    assert.equal(q.offer("2".repeat(64), req("r2")), "queued");
    assert.equal(q.offer("3".repeat(64), req("r3")), "queued");
    assert.equal(q.offer("4".repeat(64), req("r4")), "queue_full");
    assert.equal(q.queuedCount, 2);
  });

  it("bounds the seen set, forgetting the oldest ids first", () => {
    const q = new AgentManagementRequestQueue({ maxSeen: 3 });
    for (let i = 1; i <= 5; i++) {
      q.offer(AGENT_A, req(`r${i}`));
      q.close("saved");
    }
    assert.equal(q.hasSeen("r1"), false);
    assert.equal(q.hasSeen("r2"), false);
    assert.equal(q.hasSeen("r5"), true);
  });

  it("close with nothing shown is a no-op", () => {
    const q = new AgentManagementRequestQueue();
    assert.equal(q.close("dismissed"), null);
    assert.equal(q.offer(AGENT_A, req("r1")), "shown");
  });
});

describe("describeAgentRequester", () => {
  it("names the agent and channel for an update", () => {
    const text = describeAgentRequester({
      action: "update",
      agentPubkey: AGENT_B,
      agentName: "Scout",
      channelName: "ops",
    });
    assert.match(text, /^Scout in #ops is asking to change this agent/);
  });

  it("falls back to a shortened pubkey and omits an unknown channel", () => {
    const text = describeAgentRequester({
      action: "create",
      agentPubkey: AGENT_B,
    });
    assert.match(text, /^Agent bbbbbbbb…bbbb is asking to create this agent/);
  });
});
