import assert from "node:assert/strict";
import test from "node:test";

import { requestHistoryGated, requestFirstEventGated } from "./relayGateBoundary.ts";

test("finding_0a06a7863fa0712b: a read whose session ended at the gate sends no REQ", async () => {
  for (const request of [requestHistoryGated, requestFirstEventGated]) {
    const subscriptions = new Map();
    const sent = [];
    await assert.rejects(
      request(
        subscriptions,
        async (payload) => sent.push(payload),
        async () => {},
        { kinds: [1] },
        1_000,
        () => false,
      ),
      /community switch/,
    );
    assert.equal(subscriptions.size, 0);
    assert.deepEqual(sent, []);
  }
});
