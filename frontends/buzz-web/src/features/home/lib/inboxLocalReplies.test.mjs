import assert from "node:assert/strict";
import test from "node:test";

import {
  withEditedLocalReply,
  withoutDeliveredLocalReplies,
  withoutLocalReply,
} from "./inboxLocalReplies.ts";

const reply = (id, content = "hi") => ({ id, content });

test("a deleted local reply leaves the list", () => {
  const next = withoutLocalReply({ item: [reply("a"), reply("b")] }, "a");
  assert.deepEqual(next.item.map((r) => r.id), ["b"]);
});

test("a delivered reply is dropped for good, so a later deletion cannot revive it", () => {
  const next = withoutDeliveredLocalReplies({ item: [reply("a"), reply("b")] }, new Set(["a"]));
  assert.deepEqual(next.item.map((r) => r.id), ["b"]);
});

test("an edited local reply shows its new content", () => {
  const next = withEditedLocalReply({ item: [reply("a", "old")] }, "a", "new");
  assert.equal(next.item[0].content, "new");
});

test("unchanged lists keep their identity", () => {
  const replies = { item: [reply("a")] };
  assert.equal(withoutLocalReply(replies, "z"), replies);
  assert.equal(withoutDeliveredLocalReplies(replies, new Set()), replies);
  assert.equal(withEditedLocalReply(replies, "z", "x"), replies);
});
