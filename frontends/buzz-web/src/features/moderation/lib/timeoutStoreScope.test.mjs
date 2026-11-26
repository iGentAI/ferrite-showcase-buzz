import assert from "node:assert/strict";
import test from "node:test";

import {
  getTimeoutSnapshot,
  recordTimeoutFromRejection,
  resetTimeoutScope,
  timeoutScopeEpoch,
} from "./timeoutStore.ts";
import { parseTimeoutRejection } from "./timeout.ts";

test("finding_2d613e60ee3ff2a0: a timeout does not survive a community switch, and a late one is not recorded", () => {
  const message = "restricted: you are timed out until 4102444800";
  assert.ok(parseTimeoutRejection(message), "a timeout rejection message parses");

  resetTimeoutScope();
  const epochA = timeoutScopeEpoch();
  assert.equal(recordTimeoutFromRejection(message, epochA), true);
  assert.equal(getTimeoutSnapshot().active, true);

  resetTimeoutScope();
  assert.equal(getTimeoutSnapshot().active, false);

  assert.equal(recordTimeoutFromRejection(message, epochA), true);
  assert.equal(getTimeoutSnapshot().active, false);
});
