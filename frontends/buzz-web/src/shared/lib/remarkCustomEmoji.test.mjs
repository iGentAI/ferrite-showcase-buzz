import assert from "node:assert/strict";
import test from "node:test";

import remarkCustomEmoji from "./remarkCustomEmoji.ts";

const EMOJI = [
  { shortcode: "party_parrot", url: "https://relay/blob/parrot.gif" },
  { shortcode: "party", url: "https://relay/blob/party.png" },
];

function runPlugin(tree, emoji = EMOJI) {
  remarkCustomEmoji({ customEmoji: emoji })(tree);
  return tree;
}

function paragraph(...children) {
  return { type: "root", children: [{ type: "paragraph", children }] };
}
function text(value) {
  return { type: "text", value };
}
function kids(tree) {
  return tree.children[0].children;
}

test("known shortcode becomes an emoji node with src/alt", () => {
  const k = kids(runPlugin(paragraph(text(":party_parrot:"))));
  assert.equal(k.length, 1);
  assert.equal(k[0].type, "emoji");
  assert.equal(k[0].data.hName, "emoji");
  assert.equal(k[0].data.hProperties.src, "https://relay/blob/parrot.gif");
  assert.equal(k[0].data.hProperties.alt, ":party_parrot:");
  assert.equal(k[0].data.hProperties["data-shortcode"], "party_parrot");
});

test("unknown shortcode stays plain text", () => {
  const k = kids(runPlugin(paragraph(text(":thinking:"))));
  assert.equal(k.length, 1);
  assert.equal(k[0].type, "text");
  assert.equal(k[0].value, ":thinking:");
});

test("mid-sentence shortcode splits surrounding text", () => {
  const k = kids(runPlugin(paragraph(text("hi :party_parrot: there"))));
  assert.equal(k.length, 3);
  assert.equal(k[0].value, "hi ");
  assert.equal(k[1].type, "emoji");
  assert.equal(k[2].value, " there");
});

test("two custom emoji in one text node both replaced", () => {
  const k = kids(runPlugin(paragraph(text(":party: and :party_parrot:"))));
  const emoji = k.filter((c) => c.type === "emoji");
  assert.equal(emoji.length, 2);
  assert.equal(emoji[0].data.hProperties["data-shortcode"], "party");
  assert.equal(emoji[1].data.hProperties["data-shortcode"], "party_parrot");
});

test("longest-first: :party_parrot: is not shadowed by :party:", () => {
  const k = kids(runPlugin(paragraph(text(":party_parrot:"))));
  assert.equal(k.length, 1);
  assert.equal(k[0].data.hProperties["data-shortcode"], "party_parrot");
});

test("shortcodes inside inline code are left untouched", () => {
  const tree = {
    type: "root",
    children: [
      {
        type: "paragraph",
        children: [{ type: "inlineCode", value: ":party:" }],
      },
    ],
  };
  remarkCustomEmoji({ customEmoji: EMOJI })(tree);
  const codeNode = tree.children[0].children[0];
  assert.equal(codeNode.type, "inlineCode");
  assert.equal(codeNode.value, ":party:");
});

test("empty emoji map is a no-op (no matching, text preserved)", () => {
  const k = kids(runPlugin(paragraph(text(":party:")), []));
  assert.equal(k.length, 1);
  assert.equal(k[0].type, "text");
  assert.equal(k[0].value, ":party:");
});

test("plain text without shortcodes is unchanged", () => {
  const k = kids(runPlugin(paragraph(text("just a normal sentence"))));
  assert.equal(k.length, 1);
  assert.equal(k[0].type, "text");
  assert.equal(k[0].value, "just a normal sentence");
});

test("mixed-case :Party_Parrot: renders via the lowercase set key", () => {
  const k = kids(runPlugin(paragraph(text("yo :Party_Parrot: yo"))));
  const emoji = k.find((c) => c.type === "emoji");
  assert.ok(emoji, "expected an emoji node for mixed-case shortcode");
  assert.equal(emoji.data.hProperties["data-shortcode"], "party_parrot");
  assert.equal(emoji.data.hProperties.src, "https://relay/blob/parrot.gif");
});

// --- F427: emoji URLs reach the DOM only when renderable; F162: generic scan ---

import { isSafeCustomEmojiUrl } from "./remarkCustomEmoji.ts";

test("F427: an emoji with a non-http(s) URL renders as its literal shortcode", () => {
  for (const url of [
    "data:image/png;base64,AAAA",
    "file:///C:/secret.png",
    "blob:https://relay/uuid",
    "javascript:alert(1)",
    "//evil.example/x.png",
  ]) {
    const k = kids(
      runPlugin(paragraph(text("hi :bad: there")), [{ shortcode: "bad", url }]),
    );
    assert.ok(
      k.every((c) => c.type === "text"),
      `no emoji node for ${url}`,
    );
    assert.equal(k.map((c) => c.value).join(""), "hi :bad: there");
  }
});

test("F427: relay-path and http(s) emoji URLs are accepted", () => {
  assert.equal(isSafeCustomEmojiUrl("/buzz/blob/abc.png"), true);
  assert.equal(isSafeCustomEmojiUrl("https://relay/blob/parrot.gif"), true);
  assert.equal(isSafeCustomEmojiUrl("http://relay/blob/parrot.gif"), true);
  assert.equal(isSafeCustomEmojiUrl(""), false);
  assert.equal(isSafeCustomEmojiUrl(undefined), false);
  assert.equal(isSafeCustomEmojiUrl(`https://relay/${"a".repeat(3000)}`), false);
});

test("F162: an unknown :shortcode: does not hide a known one right after it", () => {
  const k = kids(runPlugin(paragraph(text("x :nope:party: y"))));
  const emoji = k.filter((c) => c.type === "emoji");
  assert.equal(emoji.length, 1);
  assert.equal(emoji[0].data.hProperties["data-shortcode"], "party");
  assert.equal(k.map((c) => c.value).join(""), "x :nope:party: y");
});

test("F162: a huge palette is matched by lookup, not a regex compiled from it", () => {
  const palette = Array.from({ length: 20_000 }, (_, i) => ({
    shortcode: `code_${i}`,
    url: `https://relay/${i}.png`,
  }));
  const body = Array.from({ length: 2_000 }, (_, i) => `:code_${i * 7}:`).join(" ");
  const started = performance.now();
  const k = kids(runPlugin(paragraph(text(body)), palette));
  const elapsed = performance.now() - started;
  assert.equal(k.filter((c) => c.type === "emoji").length, 2_000);
  assert.ok(elapsed < 1_000, `took ${elapsed.toFixed(0)} ms`);
});
