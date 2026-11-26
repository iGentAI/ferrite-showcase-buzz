import assert from "node:assert/strict";
import test from "node:test";

import { navigationStateChanges } from "./navigationState.ts";

test("navigationStateChanges_noUpdate_isNoChange", () => {
  assert.equal(navigationStateChanges(undefined, { a: 1 }), false);
});

test("navigationStateChanges_clearingAnAbsentHighlight_isNoChange", () => {
  const clear = (previous) => ({ ...previous, searchHighlight: null });
  assert.equal(navigationStateChanges(clear, {}), false);
  assert.equal(navigationStateChanges(clear, undefined), false);
  assert.equal(navigationStateChanges(clear, { searchHighlight: null }), false);
});

test("navigationStateChanges_settingOrClearingAHighlight_isChange", () => {
  const highlight = { messageId: "m1" };
  const set = (previous) => ({ ...previous, searchHighlight: highlight });
  const clear = (previous) => ({ ...previous, searchHighlight: null });
  assert.equal(navigationStateChanges(set, {}), true);
  assert.equal(navigationStateChanges(clear, { searchHighlight: highlight }), true);
});

test("navigationStateChanges_objectState_comparesItsKeys", () => {
  const origin = { workflowEditorHasOrigin: true };
  assert.equal(navigationStateChanges(origin, { workflowEditorHasOrigin: true }), false);
  assert.equal(navigationStateChanges(origin, {}), true);
});
