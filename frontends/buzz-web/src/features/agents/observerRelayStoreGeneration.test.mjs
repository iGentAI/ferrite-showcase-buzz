/**
 * F346: frames buffered while the trusted-agent set was still unknown are
 * replayed when agents register. That replay must carry the generation of the
 * moment it was scheduled, so a community/identity reset that lands before it
 * runs drops the frames instead of feeding them into the fresh store.
 */
import assert from "node:assert/strict";
import { beforeEach, it } from "node:test";

import {
  getAgentObserverSnapshot,
  ingestArchivedObserverEvents,
  resetAgentObserverStore,
  _testDeliverRelayObserverEvent,
  _testObserverProcessingQueue,
  _testPendingUnknownAgentFrameCount,
  _testRegisterKnownAgents,
} from "@/features/agents/observerRelayStore.ts";

const OLD_AGENT = "a".repeat(64);
const OWNER = "b".repeat(64);

function rawFrame(agent = OLD_AGENT) {
  return {
    id: "e".repeat(64),
    pubkey: agent,
    created_at: 1000,
    kind: 24200,
    tags: [
      ["p", OWNER],
      ["agent", agent],
      ["frame", "telemetry"],
    ],
    content: "encrypted",
    sig: "s".repeat(128),
  };
}

beforeEach(() => {
  resetAgentObserverStore();
});

it("a replay scheduled before a reset is dropped, not re-buffered into the new store", async () => {
  // No trusted agents yet: the frame is buffered for replay.
  await _testDeliverRelayObserverEvent(rawFrame());
  assert.equal(_testPendingUnknownAgentFrameCount(), 1);

  // Registering an unrelated agent schedules the replay...
  _testRegisterKnownAgents("old-community", ["c".repeat(64)]);
  const scheduledReplay = _testObserverProcessingQueue();
  // ...and the community switch lands before it runs.
  resetAgentObserverStore();
  await scheduledReplay;

  // With a lazily read generation the replay ran as if current, found no
  // trusted agents in the reset store and buffered the old frame there, to be
  // replayed into the next community's agents.
  assert.equal(_testPendingUnknownAgentFrameCount(), 0);
});

it("a frame from the previous subscription is not buffered after a reset", async () => {
  // Frames captured with the old generation keep arriving until the old
  // subscription's unsubscribe completes.
  const staleDelivery = _testDeliverRelayObserverEvent(rawFrame());
  resetAgentObserverStore();
  await staleDelivery;
  assert.equal(_testPendingUnknownAgentFrameCount(), 0);
});

it("frames in the current generation are still buffered for replay", async () => {
  await _testDeliverRelayObserverEvent(rawFrame());
  await _testDeliverRelayObserverEvent(rawFrame());
  assert.equal(_testPendingUnknownAgentFrameCount(), 2);
});

it("finding_5a2d6dd1b4c4e4d6: one frame that fails to decrypt does not mark the subscription failed", async () => {
  _testRegisterKnownAgents("community", [OLD_AGENT]);
  await _testObserverProcessingQueue();
  const before = getAgentObserverSnapshot(OLD_AGENT).connectionState;
  // No native decryptor in tests: the frame fails to decrypt.
  await _testDeliverRelayObserverEvent(rawFrame());
  const after = getAgentObserverSnapshot(OLD_AGENT);
  assert.notEqual(after.connectionState, "error");
  assert.equal(after.connectionState, before);
});

it("finding_26010c083b8e6f23: archived frames decrypted across a reset stay out of the new store", async () => {
  _testRegisterKnownAgents("community", [OLD_AGENT]);
  await _testObserverProcessingQueue();
  let decrypted = 0;
  await ingestArchivedObserverEvents([rawFrame(), rawFrame()], async () => {
    decrypted += 1;
    resetAgentObserverStore();
    return { kind: "status", timestamp: "2026-01-01T00:00:00Z", payload: {} };
  });
  assert.equal(decrypted, 1);
  assert.equal(getAgentObserverSnapshot(OLD_AGENT).events.length, 0);
});
