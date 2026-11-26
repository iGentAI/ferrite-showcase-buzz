import assert from "node:assert/strict";
import test from "node:test";

const { withKnownAgentMetadata, admitsRecipientCandidate } = await import(
  "./newMessageRecipientAgents.ts"
);

const AGENT = "a".repeat(64);
const PERSON = "b".repeat(64);

test("a relay agent whose profile omits the bot flag is still an agent", () => {
  const classified = withKnownAgentMetadata(
    { pubkey: AGENT, displayName: "Helper", isAgent: false },
    { managedAgentsByPubkey: new Map(), relayAgentPubkeys: new Set([AGENT]) },
  );
  assert.equal(classified.isAgent, true);
  assert.equal(
    admitsRecipientCandidate({
      isAgent: classified.isAgent,
      pubkey: AGENT,
      eligibleAgentPubkeys: new Set(),
      directoryReady: true,
    }),
    false,
  );
});

test("agents are hidden while a directory refresh has failed; people are not", () => {
  const eligible = new Set([AGENT]);
  assert.equal(
    admitsRecipientCandidate({ isAgent: true, pubkey: AGENT, eligibleAgentPubkeys: eligible, directoryReady: false }),
    false,
  );
  assert.equal(
    admitsRecipientCandidate({ isAgent: true, pubkey: AGENT, eligibleAgentPubkeys: eligible, directoryReady: true }),
    true,
  );
  assert.equal(
    admitsRecipientCandidate({ isAgent: false, pubkey: PERSON, eligibleAgentPubkeys: new Set(), directoryReady: false }),
    true,
  );
});
