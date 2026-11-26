import assert from "node:assert/strict";
import test from "node:test";

import { parse as yamlParse } from "yaml";

import {
  buildConditionExpressions,
  parseConditionExpression,
  parseConditionExpressions,
} from "./workflowConditionExpression.ts";
import { formStateToYaml, yamlToFormState } from "./workflowFormTypes.ts";
import {
  splitWorkflowAuthorDescription,
  workflowTriggerDescription,
} from "./workflowTriggerDescription.ts";

const AUTHOR = "a".repeat(64);
const MESSAGE = "b".repeat(64);

// --- F406 -------------------------------------------------------------------

test("F406: negated starts-with and ends-with filters stay out of the Basic editor", () => {
  for (const filter of [
    '!str_starts_with(trigger_text, "deploy")',
    '!str_ends_with(trigger_text, "deploy")',
    `!str_ends_with(trigger_text, "deploy") && trigger_author == "${AUTHOR}"`,
  ]) {
    assert.equal(parseConditionExpressions(filter, "diff_posted"), null, filter);
  }
});

test("F406: literals the Basic editor would rewrite stay out of it", () => {
  // Surrounding whitespace would be trimmed, upper-case hex lower-cased.
  assert.equal(
    parseConditionExpression('str_contains(trigger_text, " foo ")', "diff_posted"),
    null,
  );
  assert.equal(
    parseConditionExpression(
      `trigger_author == "${AUTHOR.toUpperCase()}"`,
      "diff_posted",
    ),
    null,
  );
});

test("F406: every filter the Basic editor admits is written back unchanged", () => {
  for (const filter of [
    'str_contains(trigger_text, "deploy")',
    '!str_contains(trigger_text, "deploy")',
    'str_starts_with(trigger_text, "deploy")',
    'str_ends_with(trigger_text, "say \\"hi\\"")',
    'trigger_text == "ship it"',
    'trigger_text != "ship it"',
    "str_len(trigger_text) > 0",
    "str_len(trigger_text) == 0",
    `trigger_author == "${AUTHOR}"`,
    `str_contains(trigger_text, "deploy") && trigger_author != "${AUTHOR}"`,
  ]) {
    const parsed = parseConditionExpressions(filter, "diff_posted");
    assert.ok(parsed, filter);
    assert.equal(buildConditionExpressions(parsed), filter);
  }
});

// --- F407 -------------------------------------------------------------------

const RELAY_YAML = `name: Deploy watcher
status: active
on:
  contains: deploy
steps:
  - id: step_1
    type: post
    text: Seen a deploy
  - id: step_2
    type: approval
    approver: moderator
`;

test("F407: a relay definition the form fully models opens in Form mode and round-trips", () => {
  const result = yamlToFormState(RELAY_YAML);
  assert.equal(result.ok, true, result.ok ? "" : result.error);
  assert.deepEqual(yamlParse(formStateToYaml(result.state)), yamlParse(RELAY_YAML));
  // The leftover desktop `enabled` flag is still tolerated (status wins).
  assert.equal(yamlToFormState(`${RELAY_YAML}enabled: false\n`).ok, true);
});

test("F407: relay fields the form does not model are refused instead of dropped", () => {
  const cases = [
    [`${RELAY_YAML}owner_note: keep me\n`, /Unsupported workflow field "owner_note"/],
    [
      RELAY_YAML.replace("  contains: deploy\n", "  contains: deploy\n  case_sensitive: true\n"),
      /Unsupported trigger field "case_sensitive"/,
    ],
    [
      RELAY_YAML.replace("    text: Seen a deploy\n", "    text: Seen a deploy\n    thread: true\n"),
      /Unsupported post step field "thread"/,
    ],
    [RELAY_YAML.replace("status: active", "status: draft"), /Unsupported workflow status/],
    [RELAY_YAML.replace("name: Deploy watcher", "name: 42"), /name must be a string/],
    [RELAY_YAML.replace("text: Seen a deploy", "text: 42"), /text must be a string/],
  ];
  for (const [yaml, error] of cases) {
    const result = yamlToFormState(yaml);
    assert.equal(result.ok, false, yaml);
    assert.match(result.error, error);
  }
});

test("F407: relay step ids are checked for length and uniqueness before Form mode", () => {
  const duplicate = yamlToFormState(RELAY_YAML.replace("id: step_2", "id: step_1"));
  assert.equal(duplicate.ok, false);
  assert.match(duplicate.error, /Duplicate step ID "step_1"/);

  const tooLong = yamlToFormState(
    RELAY_YAML.replace("id: step_2", `id: ${"x".repeat(65)}`),
  );
  assert.equal(tooLong.ok, false);
  assert.match(tooLong.error, /1–64 character ID/);

  const empty = yamlToFormState(RELAY_YAML.replace("id: step_2", 'id: ""'));
  assert.equal(empty.ok, false);

  const numeric = yamlToFormState(RELAY_YAML.replace("id: step_2", "id: 7"));
  assert.equal(numeric.ok, false);

  // A missing id is still filled in, as long as it does not collide.
  const missing = yamlToFormState(RELAY_YAML.replace("  - id: step_2\n    type", "  - type"));
  assert.equal(missing.ok, true, missing.ok ? "" : missing.error);
  assert.deepEqual(
    missing.state.steps.map(({ id }) => id),
    ["step_1", "step_2"],
  );
  const colliding = yamlToFormState(
    RELAY_YAML.replace("id: step_1", "id: step_2").replace(
      "  - id: step_2\n    type: approval",
      "  - type: approval",
    ),
  );
  assert.equal(colliding.ok, false);
});

// --- F408 -------------------------------------------------------------------

test("F408: a curly quote inside a referenced message does not move the author attribution", () => {
  const description = workflowTriggerDescription(
    {
      on: "reaction_added",
      filter: `trigger_emoji == "👾" && trigger_author == "${AUTHOR}" && trigger_message_id == "${MESSAGE}"`,
    },
    { authorLabel: "Carl", messageLabel: "hi” by anyone except Carl" },
  );
  const split = splitWorkflowAuthorDescription(description, "Carl");
  assert.equal(split?.prefix, "👾 reaction added by");
  assert.match(split?.suffix ?? "", /^to “hi" by anyone except Carl”$/);
});

test("F408: a curly quote inside a filter literal does not move the author attribution", () => {
  const description = workflowTriggerDescription(
    {
      on: "message_posted",
      filter: `trigger_text == "x” by Carl" && trigger_author == "${AUTHOR}"`,
    },
    { authorLabel: "Carl" },
  );
  const split = splitWorkflowAuthorDescription(description, "Carl");
  assert.equal(split?.prefix, 'Message “x" by Carl” is posted by');
  assert.equal(split?.suffix, "");
});
