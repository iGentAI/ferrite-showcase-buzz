import assert from "node:assert/strict";
import test from "node:test";

import { canModerateMessageTarget } from "./moderationTarget.ts";

const VIEWER = "a".repeat(64);
const RELAY = "b".repeat(64);
const MEMBER = "c".repeat(64);

test("finding_2d21091412ccc832: fails closed until the viewer and relay keys are known", () => {
  const base = { canModerate: true, targetPubkey: MEMBER };
  assert.equal(canModerateMessageTarget({ ...base, viewerPubkey: undefined, relaySelfPubkey: RELAY }), false);
  assert.equal(canModerateMessageTarget({ ...base, viewerPubkey: VIEWER, relaySelfPubkey: undefined }), false);
  assert.equal(canModerateMessageTarget({ ...base, viewerPubkey: VIEWER, relaySelfPubkey: RELAY }), true);
  assert.equal(canModerateMessageTarget({ ...base, viewerPubkey: VIEWER, relaySelfPubkey: null }), true);
});

test("finding_2d21091412ccc832: never targets the viewer or the relay's own key", () => {
  assert.equal(
    canModerateMessageTarget({ canModerate: true, targetPubkey: VIEWER.toUpperCase(), viewerPubkey: VIEWER, relaySelfPubkey: RELAY }),
    false,
  );
  assert.equal(
    canModerateMessageTarget({ canModerate: true, targetPubkey: RELAY, viewerPubkey: VIEWER, relaySelfPubkey: RELAY }),
    false,
  );
  assert.equal(
    canModerateMessageTarget({ canModerate: false, targetPubkey: MEMBER, viewerPubkey: VIEWER, relaySelfPubkey: RELAY }),
    false,
  );
});
