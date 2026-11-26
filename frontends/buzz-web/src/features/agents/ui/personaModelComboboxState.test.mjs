import assert from "node:assert/strict";
import test from "node:test";

import {
  firstEnabledIndex,
  nextEnabledIndex,
  optionsSignature,
} from "./personaModelComboboxState.ts";

const MODELS = [
  { value: "claude-fable-5" },
  { value: "claude-opus-5" },
  { value: "claude-sonnet-4-6" },
  { value: "claude-haiku-4-5" },
];

// THE REGRESSION TEST. The highlight reset is keyed on this signature rather
// than on the array, because `AgentDefinitionDialog` rebuilds its options inline
// on every render and model discovery makes it re-render asynchronously. Keyed
// on identity, the reset fired on every render and dragged the highlight back to
// the first row, so a user could not move to the model they wanted and Enter
// selected the wrong one.
test("an equal-but-new array has an unchanged signature", () => {
  const rebuilt = MODELS.map((model) => ({ ...model }));
  assert.notEqual(rebuilt, MODELS, "the fixture must be a different array");
  assert.equal(optionsSignature(rebuilt), optionsSignature(MODELS));
});

test("the signature tracks every change that matters", () => {
  const base = optionsSignature(MODELS);
  // A model appearing (discovery landing) or disappearing.
  assert.notEqual(optionsSignature([...MODELS, { value: "gpt-5.5" }]), base);
  assert.notEqual(optionsSignature(MODELS.slice(1)), base);
  // A model becoming unselectable, with the same set of values.
  assert.notEqual(
    optionsSignature([{ ...MODELS[0], disabled: true }, ...MODELS.slice(1)]),
    base,
  );
  // Reordering, since the highlight is an index into this list.
  assert.notEqual(optionsSignature([...MODELS].reverse()), base);
  // And a value cannot forge the structure around it. The first encoding here
  // joined values with a separator, and this assertion failed against it: one
  // option containing the separator produced the same bytes as two options,
  // which would have meant a real change in the list going unnoticed.
  assert.notEqual(
    optionsSignature([{ value: "a:0\u001fb" }]),
    optionsSignature([{ value: "a" }, { value: "b" }]),
  );
});

test("arrow keys walk the list and wrap", () => {
  assert.equal(nextEnabledIndex(MODELS, 0, 1), 1);
  assert.equal(nextEnabledIndex(MODELS, 2, 1), 3);
  // Forwards off the end wraps to the top...
  assert.equal(nextEnabledIndex(MODELS, 3, 1), 0);
  // ...and backwards off the top wraps to the end. This is the case the old
  // arithmetic obscured: `%` keeps the sign of its left operand in JavaScript.
  assert.equal(nextEnabledIndex(MODELS, 0, -1), 3);
  assert.equal(nextEnabledIndex(MODELS, 2, -1), 1);
});

test("arrow keys skip rows the user may not choose", () => {
  const withGaps = [
    { value: "a" },
    { disabled: true, value: "loading" },
    { disabled: true, value: "divider" },
    { value: "d" },
  ];
  assert.equal(nextEnabledIndex(withGaps, 0, 1), 3, "skips a run of disabled");
  assert.equal(nextEnabledIndex(withGaps, 3, 1), 0, "wraps past disabled");
  assert.equal(nextEnabledIndex(withGaps, 3, -1), 0, "skips backwards too");
});

test("a list with nothing selectable leaves the highlight alone", () => {
  // The real case: discovery is still running and the only row is its sentinel.
  const allDisabled = [
    { disabled: true, value: "loading" },
    { disabled: true, value: "none" },
  ];
  assert.equal(nextEnabledIndex(allDisabled, 1, 1), 1);
  assert.equal(nextEnabledIndex(allDisabled, 1, -1), 1);
  assert.equal(nextEnabledIndex([], 0, 1), 0, "an empty list is not an error");
});

test("the highlight lands on the first selectable row", () => {
  assert.equal(firstEnabledIndex(MODELS), 0);
  assert.equal(
    firstEnabledIndex([{ disabled: true, value: "loading" }, ...MODELS]),
    1,
    "a leading sentinel is skipped",
  );
  assert.equal(
    firstEnabledIndex([{ disabled: true, value: "loading" }]),
    0,
    "an all-disabled list still yields a valid index",
  );
  assert.equal(firstEnabledIndex([]), 0);
});
