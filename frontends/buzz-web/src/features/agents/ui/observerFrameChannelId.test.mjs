import assert from "node:assert/strict";
import test from "node:test";

import { observerFrameChannelId } from "./useObserverEvents.ts";

test("finding_2752de31dd86bece: a batch frame is indexed under its inner events channel", () => {
  assert.equal(
    observerFrameChannelId({
      kind: "batch",
      channelId: null,
      payload: {
        events: [
          { kind: "message", channelId: "chan-x" },
          { kind: "lifecycle", channelId: null },
        ],
      },
    }),
    "chan-x",
  );
  assert.equal(observerFrameChannelId({ kind: "message", channelId: "chan-y" }), "chan-y");
  assert.equal(observerFrameChannelId({ kind: "lifecycle", channelId: null }), null);
  assert.equal(observerFrameChannelId(null), null);
});
