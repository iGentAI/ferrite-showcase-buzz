import assert from "node:assert/strict";
import test from "node:test";

import {
  profileMatchesCommitAuthor,
  searchedProfileForCommitAuthor,
} from "./ProjectRepositoryPanel.tsx";

const commit = { authorName: "Ada Lovelace", authorEmail: "" };

test("a blank profile field never matches a blank commit field", () => {
  assert.equal(
    profileMatchesCommitAuthor(commit, { displayName: "Someone Else", nip05Handle: null }),
    false,
  );
  assert.equal(
    profileMatchesCommitAuthor(commit, { displayName: "Ada Lovelace", nip05Handle: null }),
    true,
  );
});

test("a search hit that does not match exactly is not the author", () => {
  assert.equal(
    searchedProfileForCommitAuthor(commit, [
      { pubkey: "p1", displayName: "Ada L", nip05Handle: null, avatarUrl: null },
    ]),
    null,
  );
});
