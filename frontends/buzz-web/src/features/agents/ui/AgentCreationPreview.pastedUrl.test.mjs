import assert from "node:assert/strict";
import test from "node:test";

import { resolvePastedAvatarUrl } from "./AgentCreationPreview.utils.ts";
import { isValidRemoteAvatarUrl } from "@/features/profile/ui/ProfileAvatarEditor.utils.ts";

// F140: the agent avatar / community icon editor committed any pasted string.

const remote = { inlineImagesOnly: false };
const inline = { inlineImagesOnly: true };

test("agent avatars accept a plain http(s) image link", () => {
  assert.equal(
    resolvePastedAvatarUrl(
      "  https://example.com/a.png ",
      remote,
      isValidRemoteAvatarUrl,
    ),
    "https://example.com/a.png",
  );
});

test("agent avatars refuse non-http schemes, credentials and oversized links", () => {
  for (const pasted of [
    "javascript:alert(1)",
    "data:text/html,<script>alert(1)</script>",
    "file:///etc/passwd",
    "ftp://example.com/a.png",
    "https://user:secret@example.com/a.png",
    `https://example.com/${"a".repeat(2_100)}.png`,
    "not a url",
    "   ",
  ]) {
    assert.equal(
      resolvePastedAvatarUrl(pasted, remote, isValidRemoteAvatarUrl),
      null,
      `must refuse ${pasted.slice(0, 40)}`,
    );
  }
});

test("the community icon accepts no pasted link, even a valid one", () => {
  assert.equal(
    resolvePastedAvatarUrl(
      "https://tracker.example/pixel.png",
      inline,
      isValidRemoteAvatarUrl,
    ),
    null,
  );
});
