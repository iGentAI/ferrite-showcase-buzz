// Review finding 9 (PRs #51-#59): a live room projection must refresh the
// channel caches it changes, so a rename or archive reaches members without the
// 60 s poll.
import assert from "node:assert/strict";
import test from "node:test";

import { roomProjectionInvalidations } from "./useLiveRoomProjections.ts";

test("a 39000 or 39002 refreshes the list and that room's detail and members", () => {
  for (const kind of [39000, 39002]) {
    assert.deepEqual(roomProjectionInvalidations({ kind, tags: [["d", "ops"], ["name", "ops"]] }), [
      ["channels"],
      ["channels", "ops", "detail"],
      ["channels", "ops", "members"],
    ]);
  }
});

test("a projection without a room still refreshes the list", () => {
  assert.deepEqual(roomProjectionInvalidations({ kind: 39000, tags: [] }), [["channels"]]);
});

test("other kinds change nothing", () => {
  for (const kind of [9, 39001, 39005, 44100]) {
    assert.deepEqual(roomProjectionInvalidations({ kind, tags: [["d", "ops"]] }), []);
  }
});
