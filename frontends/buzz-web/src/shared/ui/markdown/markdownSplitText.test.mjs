// F426 follow-up: search highlighting (or any other rehype step) may split the
// text of a synthetic inline element around a <mark>. The renderers must read
// that text as text; String(children) turned it into "@,[object Object]" and
// dropped the chip.
import assert from "node:assert/strict";
import test from "node:test";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { createMarkdownComponents } from "../markdown.tsx";
import { MarkdownRuntimeContext } from "./runtimeContext.ts";

const KEY = "b".repeat(64);
const CHANNEL = "f570339f-8f8a-4e08-a779-8d954aa44109";
const EVENT = "c".repeat(64);
const OWNER = "a".repeat(64);

const mark = (text) => React.createElement("mark", { key: "m" }, text);

function render(tag, children, runtime = {}) {
  const components = createMarkdownComponents(false, false);
  return renderToStaticMarkup(
    React.createElement(
      QueryClientProvider,
      { client: new QueryClient() },
      React.createElement(
        MarkdownRuntimeContext.Provider,
        { value: { channels: [], ...runtime } },
        React.createElement(components[tag], null, ...children),
      ),
    ),
  );
}

test("a highlighted mention keeps its chip and identity", () => {
  const html = render("mention", ["@", mark("alice")], {
    mentionPubkeysByName: { alice: KEY },
  });
  assert.doesNotMatch(html, /object Object/);
  assert.match(html, new RegExp(`data-mention-pubkey="${KEY}"`));
  assert.match(html, /data-mention-label="alice"/);
});

test("a highlighted message link keeps its pill", () => {
  const html = render("message-link", [
    "buzz://message?channel=",
    mark(CHANNEL.slice(0, 8)),
    `${CHANNEL.slice(8)}&id=${EVENT}`,
  ]);
  assert.doesNotMatch(html, /object/);
  assert.match(html, /data-buzz-link=""/);
  assert.match(html, /Open message in channel f570339f/);
});

test("a highlighted entity link keeps its chip", () => {
  const html = render("entity-link", [
    `buzz://pr?id=${EVENT}&owner=${OWNER}&d=`,
    mark("buzz"),
    "-world",
  ]);
  assert.doesNotMatch(html, /object Object/);
  assert.match(html, /data-buzz-link-kind="pr"/);
  assert.match(html, /buzz-world/);
});

test("a highlighted channel reference keeps its name", () => {
  const html = render("channel-link", ["#", mark("gen"), "eral"]);
  assert.doesNotMatch(html, /object Object/);
  assert.match(html, /general/);
});

test("a highlighted channel deep link keeps its chip", () => {
  const html = render("channel-deep-link", [
    "buzz://channel/",
    mark(CHANNEL.slice(0, 8)),
    CHANNEL.slice(8),
  ]);
  assert.doesNotMatch(html, /object Object/);
  assert.match(html, /data-buzz-link=""/);
});
