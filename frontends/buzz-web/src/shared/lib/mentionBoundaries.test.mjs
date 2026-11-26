import assert from "node:assert/strict";
import test from "node:test";

import { getMentionOffsets } from "./mentionBoundaries.ts";

// F168: code masking must stay linear on untrusted message bodies, and must
// find exactly the code spans CommonMark does.

test("mentions inside matched code spans are masked, outside ones are found", () => {
  assert.deepEqual(getMentionOffsets("`@alice` @alice", "alice"), [9]);
  assert.deepEqual(getMentionOffsets("``@alice`` @alice", "alice"), [11]);
  // A closer must have the opener's exact length.
  assert.deepEqual(getMentionOffsets("``@alice` @alice``", "alice"), []);
  // An unmatched opener is literal text.
  assert.deepEqual(getMentionOffsets("``` x ` @alice", "alice"), [8]);
  assert.deepEqual(getMentionOffsets("`` x ` @alice", "alice"), [7]);
  // An escaped backtick does not open a span.
  assert.deepEqual(getMentionOffsets("\\` @alice `", "alice"), [3]);
  // Fenced and indented code are masked.
  assert.deepEqual(getMentionOffsets("```\n@alice\n```\n@alice", "alice"), [15]);
  assert.deepEqual(getMentionOffsets("    @alice\n@alice", "alice"), [11]);
});

test("F168: unbalanced backtick runs of growing length do not go quadratic", () => {
  let body = "";
  let length = 1;
  while (body.length < 96 * 1024) body += `${"`".repeat(length++)}x`;
  body += " @alice";
  const started = performance.now();
  for (let i = 0; i < 20; i += 1) {
    // A fresh string each time, so the one-entry memo cannot help.
    getMentionOffsets(`${body}${i}`, "alice");
  }
  const elapsed = performance.now() - started;
  assert.ok(elapsed < 1_000, `took ${elapsed.toFixed(0)} ms for 20 bodies`);
});

test("F168: repeated scans of the same body reuse one masking pass", () => {
  const body = `${"`a` ".repeat(20_000)}@alice`;
  const started = performance.now();
  for (let i = 0; i < 500; i += 1) getMentionOffsets(body, `name${i}`);
  const elapsed = performance.now() - started;
  assert.ok(elapsed < 2_000, `took ${elapsed.toFixed(0)} ms for 500 scans`);
});
