import assert from "node:assert/strict";
import test from "node:test";

import { mergeProfileUpdate } from "./profileUpdate.ts";

test("a partial profile edit preserves every omitted kind-0 field", () => {
  assert.deepEqual(
    mergeProfileUpdate(
      {
        name: "Release QA",
        display_name: "Release QA",
        picture: "https://example.test/avatar.png",
        about: "Old bio",
        nip05: "release@example.test",
        lud16: "release@example.test",
      },
      { about: "New bio" },
    ),
    {
      name: "Release QA",
      display_name: "Release QA",
      picture: "https://example.test/avatar.png",
      about: "New bio",
      nip05: "release@example.test",
      lud16: "release@example.test",
    },
  );
});

test("explicit clears replace only the requested fields", () => {
  assert.deepEqual(
    mergeProfileUpdate(
      { name: "Before", display_name: "Before", picture: "avatar", about: "bio" },
      { displayName: null, avatarUrl: null },
    ),
    { name: "", display_name: "", picture: "", about: "bio" },
  );
});
