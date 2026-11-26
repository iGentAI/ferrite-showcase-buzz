import assert from "node:assert/strict";
import test from "node:test";

import {
  buildDirectReplyIdsByParentId,
  collectReplyDescendantIds,
} from "./subtreeCreatedAt.ts";

test("a cyclic or self-referential reply graph yields each descendant once", () => {
  const messages = [
    { id: "a", parentId: "b", createdAt: 1 },
    { id: "b", parentId: "a", createdAt: 2 },
    { id: "c", parentId: "c", createdAt: 3 },
  ];
  const map = buildDirectReplyIdsByParentId(messages);
  assert.equal(map.has("c"), false);
  assert.deepEqual(collectReplyDescendantIds("a", map), ["b"]);
});
