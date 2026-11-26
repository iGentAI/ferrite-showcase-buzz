import assert from "node:assert/strict";
import test from "node:test";

import { isVisualWorkflowSubmittable, stepIdsAreValid } from "./workflowFormTypes.ts";

test("stepIdsAreValid_rejectsEmptyMalformedAndDuplicateIds", () => {
  assert.equal(stepIdsAreValid([{ id: "a" }, { id: "b_2" }]), true);
  assert.equal(stepIdsAreValid([{ id: "" }]), false);
  assert.equal(stepIdsAreValid([{ id: "has space" }]), false);
  assert.equal(stepIdsAreValid([{ id: "a" }, { id: "a" }]), false);
});

test("isVisualWorkflowSubmittable_falseWhileStepIdsCollide", () => {
  const state = { trigger: { on: "manual" }, steps: [{ id: "a" }, { id: "a" }] };
  assert.equal(isVisualWorkflowSubmittable(state), false);
  assert.equal(
    isVisualWorkflowSubmittable({ ...state, steps: [{ id: "a" }, { id: "b" }] }),
    true,
  );
});
