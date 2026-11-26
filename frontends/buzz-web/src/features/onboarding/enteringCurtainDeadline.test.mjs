import assert from "node:assert/strict";
import test from "node:test";

import { enteringCurtainRemainingMs } from "./enteringCurtainDeadline.ts";

test("preserves the original entering deadline across remounts", () => {
  const updatedAt = "2026-08-18T09:00:00.000Z";

  assert.equal(
    enteringCurtainRemainingMs(updatedAt, 8_000, Date.parse(updatedAt) + 2_000),
    6_000,
  );
  assert.equal(
    enteringCurtainRemainingMs(updatedAt, 8_000, Date.parse(updatedAt) + 7_500),
    500,
  );
});

test("expires an entering curtain whose deadline already elapsed", () => {
  const updatedAt = "2026-08-18T09:00:00.000Z";
  assert.equal(
    enteringCurtainRemainingMs(
      updatedAt,
      8_000,
      Date.parse(updatedAt) + 20_000,
    ),
    0,
  );
});

test("uses the full safety interval for malformed persisted timestamps", () => {
  assert.equal(enteringCurtainRemainingMs("not-a-date", 8_000, 42), 8_000);
});
