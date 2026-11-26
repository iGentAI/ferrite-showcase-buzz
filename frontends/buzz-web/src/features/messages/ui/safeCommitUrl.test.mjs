import assert from "node:assert/strict";
import test from "node:test";

import { safeCommitUrl } from "./DiffMessage.tsx";

test("finding_0283a7ce559e6e0d: diff commit links accept only a plain repo URL and a hex sha", () => {
  assert.equal(
    safeCommitUrl("https://github.com/org/repo/", "ABCDEF1"),
    "https://github.com/org/repo/commit/abcdef1",
  );
  assert.equal(safeCommitUrl("https://github.com/org/repo", "../../evil"), undefined);
  assert.equal(safeCommitUrl("https://github.com/org/repo", "abc123?x=1"), undefined);
  assert.equal(safeCommitUrl("https://github.com@evil.example/repo", "abcdef1"), undefined);
  assert.equal(safeCommitUrl("https://github.com/org/repo?next=//evil", "abcdef1"), undefined);
  assert.equal(safeCommitUrl("javascript:alert(1)//", "abcdef1"), undefined);
});
