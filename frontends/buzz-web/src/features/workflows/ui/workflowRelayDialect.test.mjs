import assert from "node:assert/strict";
import test from "node:test";

const {
  getWorkflowActionTiles,
  getWorkflowCardLabel,
  getWorkflowEnabled,
  getWorkflowPrimaryAction,
  getWorkflowTriggerSummary,
  getWorkflowTriggerType,
} = await import("./workflowDefinition.ts");
const { formStateToYaml, isVisualWorkflowSubmittable, yamlToFormState } =
  await import("./workflowFormTypes.ts");

const relay = {
  name: "Deploy notes",
  status: "active",
  enabled: false,
  on: { contains: "deploy" },
  steps: [
    { id: "s1", type: "post", text: "Shipping" },
    { id: "s2", type: "approval", approver: "moderator" },
  ],
};

test("relay definitions are read in their own dialect", () => {
  assert.equal(getWorkflowTriggerType(relay), "message_posted");
  assert.equal(getWorkflowPrimaryAction(relay), "post");
  assert.deepEqual(
    getWorkflowActionTiles(relay).map((tile) => tile.action),
    ["post", "approval"],
  );
  assert.equal(getWorkflowTriggerSummary(relay), "Message containing “deploy”");
  const label = getWorkflowCardLabel(relay);
  assert.ok(label.startsWith("When a message containing “deploy” is posted, post"), label);
  assert.ok(label.endsWith("then 1 more step"), label);
  // status is authoritative over a leftover enabled flag.
  assert.equal(getWorkflowEnabled(relay), true);
  assert.equal(getWorkflowEnabled({ ...relay, status: "paused", enabled: true }), false);
  assert.equal(getWorkflowTriggerType({ name: "x", status: "active", steps: [] }), "manual");
});

test("an upstream document round-trips in the upstream dialect", () => {
  const yaml = [
    "name: Notify",
    "trigger:",
    "  on: message_posted",
    "steps:",
    "  - id: s1",
    "    action: post",
    "    text: hello",
    "",
  ].join("\n");
  const parsed = yamlToFormState(yaml);
  assert.equal(parsed.ok, true);
  assert.equal(parsed.state.dialect, "upstream");
  // An unfiltered upstream message trigger is valid in Form mode.
  assert.equal(isVisualWorkflowSubmittable(parsed.state), true);
  const written = formStateToYaml(parsed.state);
  assert.ok(written.includes("trigger:"), written);
  assert.ok(!written.includes("status:"), written);
});

test("Form mode leaves a padded relay phrase to the YAML editor", () => {
  const parsed = yamlToFormState('name: x\non:\n  contains: " deploy "\nsteps: []\n');
  assert.equal(parsed.ok, false);
});
