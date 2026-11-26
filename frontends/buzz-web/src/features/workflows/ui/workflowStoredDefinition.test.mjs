import assert from "node:assert/strict";
import test from "node:test";

import { parse as yamlParse } from "yaml";

import { getWorkflowActivationWarning } from "./workflowActivationWarning.ts";
import { yamlToFormState } from "./workflowFormTypes.ts";
import {
  readWorkflowDefinitionEnabled,
  stripWorkflowBookkeeping,
  workflowDefinitionYaml,
  yamlWithWorkflowEnabled,
} from "./workflowYamlDocument.ts";

// What the browser backend (shared/browser-backend/workflows.ts
// definitionFromYaml) and the relay hand back as `Workflow.definition`: the
// parsed document plus the author's text in `yaml` and a `created_at`.
function storedDefinition(yaml, createdAt = 1_790_000_000) {
  return { ...yamlParse(yaml), yaml, created_at: createdAt };
}

const UPSTREAM_DIALECT_YAML = `# posts a hello when someone says deploy
name: Deploy hello
trigger:
  on: message_posted
  filter: str_contains(trigger_text, "deploy")
steps:
  - id: hello
    action: send_message
    text: hello
`;

const RELAY_DIALECT_YAML = `name: Deploy watcher
status: paused
on:
  contains: deploy
steps:
  - id: step_1
    type: post
    text: Seen a deploy
`;

test("F402: the editor gets the author's own text back, without the backend's bookkeeping", () => {
  const yaml = workflowDefinitionYaml(storedDefinition(UPSTREAM_DIALECT_YAML));
  assert.equal(yaml, UPSTREAM_DIALECT_YAML);
  assert.doesNotMatch(yaml, /^yaml:/m);
  assert.doesNotMatch(yaml, /created_at/);
});

test("F402: an upstream-dialect workflow opens in Form mode again", () => {
  const result = yamlToFormState(
    workflowDefinitionYaml(storedDefinition(UPSTREAM_DIALECT_YAML)),
  );
  assert.equal(result.ok, true, result.ok ? "" : result.error);
});

test("F402: without trustworthy author text, the definition is serialized minus bookkeeping", () => {
  // No stored text at all.
  const bare = stripWorkflowBookkeeping(storedDefinition(RELAY_DIALECT_YAML));
  const fromBare = workflowDefinitionYaml({ ...bare, created_at: 5 });
  assert.deepEqual(yamlParse(fromBare), bare);

  // Stored text that disagrees with the definition that actually runs is not
  // what the editor shows.
  const tampered = {
    ...storedDefinition(RELAY_DIALECT_YAML),
    yaml: RELAY_DIALECT_YAML.replace("Seen a deploy", "Something harmless"),
  };
  const shown = workflowDefinitionYaml(tampered);
  assert.match(shown, /Seen a deploy/);
  assert.doesNotMatch(shown, /Something harmless|^yaml:|created_at/m);

  // Text written by an older client that serialized the whole stored object
  // (a nested `yaml:` block) is not shown verbatim either.
  const nested = storedDefinition(
    `${RELAY_DIALECT_YAML}yaml: |\n  name: old\ncreated_at: 3\n`,
  );
  assert.doesNotMatch(workflowDefinitionYaml(nested), /^yaml:|created_at/m);
});

test("F403: a paused relay workflow reads as paused", () => {
  const paused = storedDefinition(RELAY_DIALECT_YAML);
  assert.equal(readWorkflowDefinitionEnabled(paused), false);
  assert.equal(
    readWorkflowDefinitionEnabled({ ...paused, status: "active" }),
    true,
  );
  // A relay definition has no `enabled`; a stale one does not pause it.
  assert.equal(
    readWorkflowDefinitionEnabled({ ...paused, status: "active", enabled: false }),
    true,
  );
  // The legacy desktop dialect keeps its own flag.
  const legacy = storedDefinition(UPSTREAM_DIALECT_YAML);
  assert.equal(readWorkflowDefinitionEnabled(legacy), true);
  assert.equal(
    readWorkflowDefinitionEnabled({ ...legacy, enabled: false }),
    false,
  );
});

test("F403: toggling a relay workflow from the list writes status, not an ignored enabled flag", () => {
  const active = storedDefinition(
    RELAY_DIALECT_YAML.replace("status: paused", "status: active"),
  );
  const paused = yamlParse(
    yamlWithWorkflowEnabled(
      workflowDefinitionYaml(active),
      !readWorkflowDefinitionEnabled(active),
    ),
  );
  assert.equal(paused.status, "paused");
  assert.equal("enabled" in paused, false);
  assert.equal("yaml" in paused, false);
  assert.equal("created_at" in paused, false);
});

test("F403: turning on a relay workflow that fires on every message asks first", () => {
  for (const on of ['{ contains: "" }', '{ contains: "   " }', "{}"]) {
    const warning = getWorkflowActivationWarning(
      `name: Everything\nstatus: active\non: ${on}\nsteps:\n  - id: gate\n    type: approval\n    approver: moderator\n`,
    );
    assert.equal(warning?.title, "This workflow may run often", on);
  }
  // A phrase, or no trigger at all (manual only), needs no confirmation.
  assert.equal(getWorkflowActivationWarning(RELAY_DIALECT_YAML), null);
  assert.equal(
    getWorkflowActivationWarning(
      "name: By hand\nsteps:\n  - id: step_1\n    type: post\n    text: hi\n",
    ),
    null,
  );
  // The upstream dialect's warnings are unchanged.
  assert.equal(
    getWorkflowActivationWarning(
      "name: x\ntrigger:\n  on: message_posted\nsteps: []\n",
    )?.title,
    "This workflow may run often",
  );
});
