import assert from "node:assert/strict";
import test from "node:test";

import { groupedReactionRemovalTargets } from "./groupedReactionRemoval.ts";

const message = { id: "m1", reactions: [] };
const other = { id: "m2", reactions: [{ emoji: "👍", reactedByCurrentUser: true }] };

test("undoing an un-echoed add removes it from the row's message", () => {
  assert.deepEqual(
    groupedReactionRemovalTargets([message], message, "👍", true).map((m) => m.id),
    ["m1"],
  );
});

test("an echoed reaction is removed from the sources that carry it", () => {
  assert.deepEqual(
    groupedReactionRemovalTargets([message, other], message, "👍", false).map((m) => m.id),
    ["m2"],
  );
});
