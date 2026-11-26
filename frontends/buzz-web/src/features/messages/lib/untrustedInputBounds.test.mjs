import assert from "node:assert/strict";
import test from "node:test";

import { estimateRowHeight } from "./rowHeightEstimate.ts";
import {
  emojiAvatarDataUrl,
  parseEmojiAvatarDataUrl,
} from "../../profile/ui/ProfileAvatarEditor.utils.ts";

test("finding_fcafe06d799e814b: a long crafted line is estimated in linear time", () => {
  const body = `https://${"a".repeat(60_000)}.png?${"b".repeat(10_000)} x`;
  const started = performance.now();
  estimateRowHeight({ body });
  assert.ok(performance.now() - started < 500, "row estimate must not backtrack quadratically");
});

test("finding_4d6b8059573ecb13: an emoji avatar fill must be a hex colour", () => {
  assert.deepEqual(parseEmojiAvatarDataUrl(emojiAvatarDataUrl("🙂", "#22aa88")), {
    color: "#22aa88",
    emoji: "🙂",
  });
  assert.equal(
    parseEmojiAvatarDataUrl(emojiAvatarDataUrl("🙂", "url(https://evil.example/beacon)")),
    null,
    "a url() fill would be copied into a CSS background and fetched",
  );
});
