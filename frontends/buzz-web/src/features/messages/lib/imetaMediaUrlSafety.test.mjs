import assert from "node:assert/strict";
import test from "node:test";

import {
  buildOutgoingMessage,
  findSpoileredImetaMediaUrls,
  formatImetaMediaLine,
  markdownDestinationUrl,
  restoreImetaMediaDisplayLabels,
  stripImetaMediaLines,
} from "./imetaMediaMarkdown.ts";

// F383: an imeta `url` value is free-form (everything after the first space of
// the tag part), and the edit path re-serializes it into the body on submit.
// A URL carrying a line break, a space or a parenthesis must not end the
// attachment line and publish the rest as the sender's own Markdown.

const SHA = "a".repeat(64);
const CRAFTED =
  "https://relay.example/media/a.png\n[Pay the invoice here](https://attacker.example)";

function media(url, type, filename) {
  return { url, type, sha256: SHA, size: 1, uploaded: 0, filename };
}

test("a line break in an attachment URL stays inside its one line (F383)", () => {
  for (const type of ["image/png", "video/webm", "application/pdf"]) {
    const line = formatImetaMediaLine(media(CRAFTED, type, "a.pdf"));
    assert.equal(line.split("\n").length, 2, JSON.stringify(line));
    assert.doesNotMatch(line, /\n\[Pay the invoice here\]/);
  }
  const spoilered = formatImetaMediaLine(media(CRAFTED, "image/png"), {
    spoiler: true,
  });
  assert.equal(spoilered.split("\n").length, 2);
});

test("a space or parenthesis cannot close the destination early (F383)", () => {
  assert.equal(
    formatImetaMediaLine(media("https://x.example/a b).png", "image/png")),
    "\n![image](https://x.example/a%20b%29.png)",
  );
  assert.equal(
    markdownDestinationUrl("https://x.example/(x)\u2028<y>\u0000"),
    "https://x.example/%28x%29%E2%80%A8%3Cy%3E%00",
  );
});

test("well-formed attachment URLs are written unchanged", () => {
  const url = `https://relay.example/media/${SHA}.png?x=1&y=%20`;
  assert.equal(markdownDestinationUrl(url), url);
  assert.equal(
    formatImetaMediaLine(media(url, "image/png")),
    `\n![image](${url})`,
  );
});

test("an encoded attachment line still round-trips through edit mode (F383)", () => {
  const file = media(CRAFTED, "application/pdf", "report.pdf");
  const { content } = buildOutgoingMessage("hello", [file]);
  assert.equal(stripImetaMediaLines(content, [file]), "hello");
  assert.equal(
    restoreImetaMediaDisplayLabels(content, [file])[0].displayLabel,
    "report.pdf",
  );

  const image = media(CRAFTED, "image/png");
  const spoilered = buildOutgoingMessage("hi", [image], new Set([CRAFTED]));
  assert.deepEqual(
    [...findSpoileredImetaMediaUrls(spoilered.content, [image])],
    [CRAFTED],
  );
  assert.equal(stripImetaMediaLines(spoilered.content, [image]), "hi");
});

test("a trailing backslash cannot escape the destination's closing parenthesis", () => {
  assert.equal(markdownDestinationUrl("https://x.example/a\\"), "https://x.example/a%5C");
});
