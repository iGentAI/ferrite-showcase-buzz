import assert from "node:assert/strict";
import test from "node:test";

import { Schema } from "@tiptap/pm/model";

import {
  emacsCharacterStep,
  hardBreakLineBounds,
} from "./macEmacsTextShortcuts.ts";

const schema = new Schema({
  nodes: {
    doc: { content: "block+" },
    paragraph: { group: "block", content: "inline*" },
    codeBlock: { group: "block", content: "text*", code: true },
    hardBreak: { group: "inline", inline: true },
    text: { group: "inline" },
  },
});

const p = (...content) => schema.node("paragraph", null, content);
const t = (text) => schema.text(text);

test("line bounds inside a code block stop at the current line's newlines", () => {
  const doc = schema.node("doc", null, [
    schema.node("codeBlock", null, [t("one\ntwo\nthree")]),
  ]);
  // Caret inside "two": positions 1..14; "two" spans 5..8.
  const bounds = hardBreakLineBounds(doc.resolve(6));
  assert.deepEqual(bounds, { start: 5, end: 8 });
});

test("line bounds still honour hard breaks in paragraphs", () => {
  const doc = schema.node("doc", null, [
    p(t("ab"), schema.node("hardBreak"), t("cd")),
  ]);
  assert.deepEqual(hardBreakLineBounds(doc.resolve(5)), { start: 4, end: 6 });
});

test("Ctrl-b crosses into the previous block instead of snapping back", () => {
  const doc = schema.node("doc", null, [p(t("ab")), p(t("cd"))]);
  // Start of the second paragraph is 5; the end of the first is 3.
  assert.equal(emacsCharacterStep(doc, 5, -1), 3);
  assert.equal(emacsCharacterStep(doc, 3, 1), 5);
  assert.equal(emacsCharacterStep(doc, 1, -1), null);
});

test("character steps never split a surrogate pair", () => {
  const doc = schema.node("doc", null, [p(t("a😀b"))]);
  // a=1..2, emoji 2..4, b=4..5
  assert.equal(emacsCharacterStep(doc, 4, -1), 2);
  assert.equal(emacsCharacterStep(doc, 2, 1), 4);
  assert.equal(emacsCharacterStep(doc, 2, -1), 1);
});
