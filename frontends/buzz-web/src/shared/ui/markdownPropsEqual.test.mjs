import assert from "node:assert/strict";
import test from "node:test";

import { markdownPropsAreEqual } from "./markdownUtils.ts";

test("markdownPropsAreEqual sees the link preview props", () => {
  const base = { content: "see https://example.com", messageId: "m1" };
  assert.equal(markdownPropsAreEqual(base, { ...base }), true);
  assert.equal(markdownPropsAreEqual(base, { ...base, linkPreviewsSuppressed: true }), false);
  assert.equal(markdownPropsAreEqual(base, { ...base, messageId: "m2" }), false);
  assert.equal(markdownPropsAreEqual(base, { ...base, linkPreviewTags: [["r", "x"]] }), false);
});
