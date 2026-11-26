import assert from "node:assert/strict";
import test from "node:test";

import { buildCustomEmojiTags } from "./customEmojiTags.ts";

const EMOJI = [
  { shortcode: "party_parrot", url: "https://relay/p.gif" },
  { shortcode: "shipit", url: "https://relay/s.png" },
];

test("emits emoji tags only for known shortcodes present in content", () => {
  const tags = buildCustomEmojiTags("ship it :shipit: now", EMOJI);
  assert.deepEqual(tags, [["emoji", "shipit", "https://relay/s.png"]]);
});

test("ignores unknown shortcodes", () => {
  assert.deepEqual(buildCustomEmojiTags("hello :thinking: world", EMOJI), []);
});

test("dedupes repeated shortcodes, preserves first-appearance order", () => {
  const tags = buildCustomEmojiTags(":shipit: :party_parrot: :shipit:", EMOJI);
  assert.deepEqual(tags, [
    ["emoji", "shipit", "https://relay/s.png"],
    ["emoji", "party_parrot", "https://relay/p.gif"],
  ]);
});

test("empty emoji list yields no tags", () => {
  assert.deepEqual(buildCustomEmojiTags(":shipit:", []), []);
});

test("no shortcodes in content yields no tags", () => {
  assert.deepEqual(buildCustomEmojiTags("just plain text", EMOJI), []);
});

test("a known shortcode right after an unknown one or a timestamp is still tagged", () => {
  assert.deepEqual(buildCustomEmojiTags(":nope:party_parrot:", EMOJI), [
    ["emoji", "party_parrot", "https://relay/p.gif"],
  ]);
  assert.deepEqual(buildCustomEmojiTags("at 12:30:shipit:", EMOJI), [
    ["emoji", "shipit", "https://relay/s.png"],
  ]);
});

test("unsafe emoji URLs are not tagged and keys are normalized", () => {
  const tags = buildCustomEmojiTags(":bad: :Spaced:", [
    { shortcode: "bad", url: "javascript:alert(1)" },
    { shortcode: " Spaced ", url: "https://relay/x.png" },
  ]);
  assert.deepEqual(tags, [["emoji", "spaced", "https://relay/x.png"]]);
});
