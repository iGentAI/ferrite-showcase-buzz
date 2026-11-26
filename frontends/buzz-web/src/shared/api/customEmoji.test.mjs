import assert from "node:assert/strict";
import path from "node:path";
import test from "node:test";

import {
  customEmojiFromEvent,
  customEmojiFromTags,
  normalizeShortcode,
  suggestShortcodeFromFilename,
} from "./customEmoji.ts";

function ev(tags, createdAt = 1) {
  return {
    id: "x",
    pubkey: "relay",
    created_at: createdAt,
    kind: 30030,
    tags,
    content: "",
    sig: "s",
  };
}

test("parses emoji tags into shortcode/url pairs", () => {
  const out = customEmojiFromEvent(
    ev([
      ["d", "buzz:custom-emoji"],
      ["emoji", "party_parrot", "https://relay/p.gif"],
      ["emoji", "shipit", "https://relay/s.png"],
    ]),
  );
  assert.deepEqual(out, [
    { shortcode: "party_parrot", url: "https://relay/p.gif" },
    { shortcode: "shipit", url: "https://relay/s.png" },
  ]);
});

test("null event yields empty list", () => {
  assert.deepEqual(customEmojiFromEvent(null), []);
});

test("skips malformed tags (missing url, bad shortcode chars)", () => {
  const out = customEmojiFromEvent(
    ev([
      ["emoji", "ok", "https://relay/ok.png"],
      ["emoji", "no_url"],
      ["emoji", "bad space", "https://relay/x.png"],
      ["emoji", "", "https://relay/empty.png"],
    ]),
  );
  assert.deepEqual(out, [{ shortcode: "ok", url: "https://relay/ok.png" }]);
});

test("first writer wins on duplicate shortcode", () => {
  const out = customEmojiFromEvent(
    ev([
      ["emoji", "dup", "https://relay/first.png"],
      ["emoji", "dup", "https://relay/second.png"],
    ]),
  );
  assert.deepEqual(out, [{ shortcode: "dup", url: "https://relay/first.png" }]);
});

test("ignores non-emoji tags", () => {
  const out = customEmojiFromEvent(
    ev([
      ["d", "x"],
      ["client", "buzz"],
      ["emoji", "yes", "https://relay/y.png"],
    ]),
  );
  assert.deepEqual(out, [{ shortcode: "yes", url: "https://relay/y.png" }]);
});

test("normalizeShortcode strips colons and lowercases", () => {
  assert.equal(normalizeShortcode(":PartyParrot:"), "partyparrot");
  assert.equal(normalizeShortcode("ship_it"), "ship_it");
  assert.equal(normalizeShortcode("  :Foo-Bar:  "), "foo-bar");
});

test("normalizeShortcode rejects invalid chars and empties", () => {
  assert.equal(normalizeShortcode("bad space"), null);
  assert.equal(normalizeShortcode("emoji!"), null);
  assert.equal(normalizeShortcode("::"), null);
  assert.equal(normalizeShortcode(""), null);
});

test("normalizeShortcode enforces the 64-character boundary", () => {
  assert.equal(normalizeShortcode("a".repeat(64)), "a".repeat(64));
  assert.equal(normalizeShortcode("a".repeat(65)), null);
});

test("suggestShortcodeFromFilename derives a valid name from common filenames", () => {
  assert.equal(
    suggestShortcodeFromFilename("Party Parrot.gif"),
    "party_parrot",
  );
  assert.equal(
    suggestShortcodeFromFilename("ship-it.final.PNG"),
    "ship-it_final",
  );
  assert.equal(
    suggestShortcodeFromFilename(path.join("tmp", "Narf! Zort.webp")),
    "narf_zort",
  );
  assert.equal(suggestShortcodeFromFilename("---.png"), null);
});

test("customEmojiFromTags normalizes shortcodes (case-fold)", () => {
  const out = customEmojiFromTags([["emoji", "ShipIt", "https://relay/s.png"]]);
  assert.deepEqual(out, [{ shortcode: "shipit", url: "https://relay/s.png" }]);
});

import { reactionEmojiUrl } from "./customEmoji.ts";

const SET = [{ shortcode: "shipit", url: "https://relay/s.png" }];

test("reactionEmojiUrl resolves :shortcode: against the set (case-insensitive)", () => {
  assert.equal(reactionEmojiUrl(":shipit:", SET), "https://relay/s.png");
  assert.equal(reactionEmojiUrl(":ShipIt:", SET), "https://relay/s.png");
});

test("reactionEmojiUrl returns undefined for unicode / unknown / no set", () => {
  assert.equal(reactionEmojiUrl("👍", SET), undefined);
  assert.equal(reactionEmojiUrl(":nope:", SET), undefined);
  assert.equal(reactionEmojiUrl(":shipit:", undefined), undefined);
});

import { unionCustomEmoji } from "./customEmoji.ts";

test("unionCustomEmoji merges members and sorts by shortcode", () => {
  const out = unionCustomEmoji([
    ev([["emoji", "shipit", "https://relay/s.png"]]),
    ev([["emoji", "ahoy", "https://relay/a.png"]]),
  ]);
  assert.deepEqual(out, [
    { shortcode: "ahoy", url: "https://relay/a.png" },
    { shortcode: "shipit", url: "https://relay/s.png" },
  ]);
});

test("unionCustomEmoji collapses a shortcode to ONE deterministic winner", () => {
  // Two members claim :party_parrot: at the same created_at. The palette must
  // expose exactly one (tie-break: lexicographically-smallest URL), since
  // downstream identity is shortcode-only and cannot disambiguate two URLs.
  const out = unionCustomEmoji([
    ev([["emoji", "party_parrot", "https://relay/zebra.gif"]]),
    ev([["emoji", "party_parrot", "https://relay/alpha.gif"]]),
  ]);
  assert.deepEqual(out, [
    { shortcode: "party_parrot", url: "https://relay/alpha.gif" },
  ]);
});

test("unionCustomEmoji prefers the most recently published set", () => {
  // Newer event wins regardless of URL sort order or input position.
  const older = ev([["emoji", "dup", "https://relay/alpha.gif"]], 100);
  const newer = ev([["emoji", "dup", "https://relay/zebra.gif"]], 200);
  const expected = [{ shortcode: "dup", url: "https://relay/zebra.gif" }];
  assert.deepEqual(unionCustomEmoji([older, newer]), expected);
  assert.deepEqual(unionCustomEmoji([newer, older]), expected);
});

test("unionCustomEmoji winner is independent of member order", () => {
  const a = ev([["emoji", "dup", "https://relay/alpha.gif"]]);
  const b = ev([["emoji", "dup", "https://relay/zebra.gif"]]);
  assert.deepEqual(unionCustomEmoji([a, b]), unionCustomEmoji([b, a]));
});

// --- F162: bounded palettes / F427: renderable emoji URLs only ---

import {
  MAX_CUSTOM_EMOJI_PALETTE,
  MAX_CUSTOM_EMOJI_PER_EVENT,
} from "./customEmoji.ts";

function manyEmojiTags(count, prefix = "e", url = (i) => `https://relay/${i}.png`) {
  return Array.from({ length: count }, (_, i) => ["emoji", `${prefix}${i}`, url(i)]);
}

test("F162: one event yields at most MAX_CUSTOM_EMOJI_PER_EVENT emoji, first tags first", () => {
  // ~10,000 emoji tags fit in one 240 KiB event; readers must not take them all.
  const out = customEmojiFromTags(manyEmojiTags(10_000));
  assert.equal(out.length, MAX_CUSTOM_EMOJI_PER_EVENT);
  assert.equal(out[0].shortcode, "e0");
  assert.equal(out.at(-1).shortcode, `e${MAX_CUSTOM_EMOJI_PER_EVENT - 1}`);
});

test("F162: the unioned community palette is capped and stays deterministic", () => {
  const members = Array.from({ length: 20 }, (_, m) =>
    ev(manyEmojiTags(MAX_CUSTOM_EMOJI_PER_EVENT, `m${m}_`), m + 1),
  );
  const palette = unionCustomEmoji(members);
  assert.equal(palette.length, MAX_CUSTOM_EMOJI_PALETTE);
  assert.deepEqual(unionCustomEmoji([...members].reverse()), palette);
  const sorted = [...palette].sort((a, b) =>
    a.shortcode.localeCompare(b.shortcode),
  );
  assert.deepEqual(palette, sorted);
});

test("F427: emoji whose URL is not an http(s) or relay-path image are dropped", () => {
  const out = customEmojiFromTags([
    ["emoji", "ok_https", "https://relay/ok.png"],
    ["emoji", "ok_http", "http://relay/ok.png"],
    ["emoji", "ok_relay_path", "/buzz/blob/abc.png"],
    ["emoji", "data_url", "data:image/svg+xml,<svg/>"],
    ["emoji", "file_url", "file:///etc/passwd"],
    ["emoji", "blob_url", "blob:https://relay/uuid"],
    ["emoji", "js_url", "javascript:alert(1)"],
    ["emoji", "tauri_url", "tauri://localhost/x.png"],
    ["emoji", "scheme_relative", "//evil.example/x.png"],
    ["emoji", "backslash", "/\\evil.example/x.png"],
    ["emoji", "spaced", " https://relay/x.png"],
    ["emoji", "newline", "https://relay/x\n.png"],
  ]);
  assert.deepEqual(
    out.map((e) => e.shortcode),
    ["ok_https", "ok_http", "ok_relay_path"],
  );
});
