import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { JSDOM } from "jsdom";

import { composerLinkPasteYields } from "./composerLinkPasteYields.ts";

const dom = new JSDOM("<!doctype html><html><body></body></html>");
before(() => {
  globalThis.DOMParser = dom.window.DOMParser;
});
after(() => dom.window.close());

function clipboard({ html = "", text = "", files = 0 }) {
  return {
    items: Array.from({ length: files }, () => ({ kind: "file" })),
    getData: (type) => (type === "text/html" ? html : type === "text/plain" ? text : ""),
  };
}

test("a copied image that also carries its URL yields to the file upload", () => {
  assert.equal(
    composerLinkPasteYields(clipboard({ text: "https://example.com/a.png", files: 1 })),
    true,
  );
});

test("a Buzz code block holding a URL yields to the code-block paste", () => {
  assert.equal(
    composerLinkPasteYields(
      clipboard({
        html: '<pre data-buzz-code-block="true"><code>https://example.com</code></pre>',
        text: "https://example.com",
      }),
    ),
    true,
  );
});

test("a plain URL paste is the link handler's", () => {
  assert.equal(composerLinkPasteYields(clipboard({ text: "https://example.com" })), false);
});
