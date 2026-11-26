/**
 * F140 wiring: the real editor must not commit an invalid pasted link, and the
 * community-icon variant (processImage set) offers no link field at all.
 */
import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});
Object.assign(globalThis, {
  document: dom.window.document,
  window: dom.window,
  IS_REACT_ACT_ENVIRONMENT: true,
  localStorage: dom.window.localStorage,
  self: dom.window,
  ResizeObserver: class {
    observe() {}
    unobserve() {}
    disconnect() {}
  },
});
Object.defineProperty(globalThis, "navigator", {
  configurable: true,
  value: dom.window.navigator,
  writable: true,
});
dom.window.requestAnimationFrame = (cb) => setTimeout(cb, 0);
globalThis.requestAnimationFrame = dom.window.requestAnimationFrame;
dom.window.matchMedia ??= (query) => ({
  matches: false,
  media: query,
  onchange: null,
  addListener: () => {},
  removeListener: () => {},
  addEventListener: () => {},
  removeEventListener: () => {},
  dispatchEvent: () => false,
});
globalThis.matchMedia = dom.window.matchMedia;
for (const key of Object.getOwnPropertyNames(dom.window)) {
  if (key === "window" || key === "document" || key === "globalThis") continue;
  const value = dom.window[key];
  if (
    typeof value === "function" &&
    /^(HTML|SVG)|Element$|Event$|EventTarget$|^Node|^Document|Observer$/.test(
      key,
    )
  ) {
    globalThis[key] = value;
  }
}
globalThis.getComputedStyle = dom.window.getComputedStyle.bind(dom.window);
globalThis.__TAURI_INTERNALS__ = {
  invoke: (cmd) => Promise.reject(new Error(`unmocked: ${cmd}`)),
  transformCallback: () => 1,
};
dom.window.__TAURI_INTERNALS__ = globalThis.__TAURI_INTERNALS__;

let act, render, screen, cleanup, fireEvent, createElement;
let AgentCreationPreview;
let QueryClient, QueryClientProvider;

before(async () => {
  ({ act, render, screen, cleanup, fireEvent } = await import(
    "@testing-library/react"
  ));
  ({ createElement } = await import("react"));
  ({ QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  ));
  ({ AgentCreationPreview } = await import("./AgentCreationPreview.tsx"));
});

afterEach(() => cleanup?.());
after(() => dom.window.close());

async function openEditor(props) {
  const selected = [];
  const committed = [];
  const client = new QueryClient();
  render(
    createElement(
      QueryClientProvider,
      { client },
      createElement(AgentCreationPreview, {
        avatarUrl: null,
        label: "Helper",
        onSelectAvatar: (url) => selected.push(url),
        onCommitAvatar: (url) => committed.push(url),
        ...props,
      }),
    ),
  );
  await act(async () => {});
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: /^Add / }));
  });
  await act(async () => {});
  return { selected, committed, client };
}

test("an invalid pasted link is flagged and never committed", async () => {
  const { selected, committed, client } = await openEditor({});
  const input = screen.getByPlaceholderText("Paste a URL");
  await act(async () => {
    fireEvent.change(input, {
      target: { value: "https://user:secret@example.com/a.png" },
    });
  });
  assert.equal(
    screen.queryByTestId("agent-avatar-url-error") !== null,
    true,
    "the inline error explains the refusal",
  );
  await act(async () => {
    fireEvent.keyDown(input, { key: "Enter" });
  });
  assert.deepEqual(selected, []);
  assert.deepEqual(committed, []);

  await act(async () => {
    fireEvent.change(input, { target: { value: "https://example.com/a.png" } });
  });
  await act(async () => {
    fireEvent.keyDown(input, { key: "Enter" });
  });
  assert.deepEqual(committed, ["https://example.com/a.png"]);
  client.clear();
});

test("the community icon editor offers no pasted-link field", async () => {
  const { client } = await openEditor({
    assetLabel: "icon",
    processImage: async () => "data:image/png;base64,AAAA",
    shape: "rounded-square",
  });
  assert.equal(screen.queryByPlaceholderText("Paste a URL") !== null, false);
  client.clear();
});
