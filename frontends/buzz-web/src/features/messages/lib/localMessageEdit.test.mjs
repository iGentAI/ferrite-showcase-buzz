import assert from "node:assert/strict";
import test from "node:test";

import { applyLocalMessageEdit } from "./localMessageEdit.ts";

const target = "a".repeat(64);

test("a re-edit rewrites the earlier edit overlay as well as the message", () => {
  const message = { id: target, kind: 9, content: "v1", tags: [["h", "c"]] };
  const firstEdit = {
    id: "b".repeat(64),
    kind: 40003,
    content: "v2",
    tags: [["h", "c"], ["e", target]],
  };
  const edit = { eventId: target, content: "v3" };
  assert.equal(applyLocalMessageEdit(message, edit).content, "v3");
  const overlay = applyLocalMessageEdit(firstEdit, edit);
  assert.equal(overlay.content, "v3");
  assert.deepEqual(
    overlay.tags.filter((tag) => tag[0] === "e"),
    [["e", target]],
    "the overlay still targets the message",
  );
});

test("unrelated events are returned unchanged", () => {
  const other = { id: "c".repeat(64), kind: 9, content: "x", tags: [] };
  assert.equal(applyLocalMessageEdit(other, { eventId: target, content: "v" }), other);
});
