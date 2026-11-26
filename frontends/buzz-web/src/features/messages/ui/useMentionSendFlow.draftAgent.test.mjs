import assert from "node:assert/strict";
import test from "node:test";

import { draftAgentPredicate } from "./useMentionSendFlow.helpers.ts";

test("an agent the composer captured stays an agent when the live view forgets it", () => {
  const agent = "a".repeat(64);
  const human = "b".repeat(64);
  const isAgent = draftAgentPredicate(
    { inlineAgentMentionPubkeys: [agent.toUpperCase()], addressedAgentPubkeys: [] },
    () => false,
  );
  assert.equal(isAgent(agent), true);
  assert.equal(isAgent(human), false);
});
