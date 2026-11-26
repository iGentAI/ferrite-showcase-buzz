/**
 * Rendered EnvVarsEditor behaviour that the pure-helper tests in
 * EnvVarsEditor.test.mjs cannot reach.
 *
 * 1. Insecure context: `crypto.randomUUID` is secure-context only. The editor
 *    used it for row ids, so a bundle served over plain http from a LAN host
 *    threw while rendering any existing env var and on "Add variable".
 * 2. Deep-link focus: the one-shot focus guard used to be consumed before the
 *    scheduled frame ran. A re-render in between (requiredKeys is often a new
 *    array) cancelled the frame, and the re-run returned early, so the
 *    requested credential input was never focused.
 */

import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";
import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

// Frames run only when a test flushes them, so a re-render can land between
// the effect and its frame exactly as it does in the app.
let frameQueue = new Map();
let nextFrameId = 1;
function requestFrame(callback) {
  const id = nextFrameId++;
  frameQueue.set(id, callback);
  return id;
}
function cancelFrame(id) {
  frameQueue.delete(id);
}
function flushFrames() {
  const pending = [...frameQueue.values()];
  frameQueue = new Map();
  for (const callback of pending) callback(0);
}

Object.assign(globalThis, {
  IS_REACT_ACT_ENVIRONMENT: true,
  document: dom.window.document,
  window: dom.window,
  self: dom.window,
  requestAnimationFrame: requestFrame,
  cancelAnimationFrame: cancelFrame,
});
Object.defineProperty(globalThis, "navigator", {
  configurable: true,
  value: dom.window.navigator,
});
dom.window.requestAnimationFrame = requestFrame;
dom.window.cancelAnimationFrame = cancelFrame;
dom.window.HTMLElement.prototype.scrollIntoView = function () {};
for (const key of Object.getOwnPropertyNames(dom.window)) {
  if (
    !(key in globalThis) &&
    (key.startsWith("HTML") ||
      ["Node", "Event", "MouseEvent", "FocusEvent", "EventTarget", "Text"].includes(key))
  ) {
    globalThis[key] = dom.window[key];
  }
}

let React, act, createRoot, EnvVarsEditor;

before(async () => {
  ({ default: React, act } = await import("react"));
  ({ createRoot } = await import("react-dom/client"));
  ({ EnvVarsEditor } = await import("./EnvVarsEditor.tsx"));
});

after(() => dom.window.close());

async function mount(props) {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  await act(async () => {
    root.render(React.createElement(EnvVarsEditor, props));
  });
  return {
    container,
    rerender: (next) =>
      act(async () => {
        root.render(React.createElement(EnvVarsEditor, next));
      }),
    unmount: async () => {
      await act(async () => root.unmount());
      container.remove();
    },
  };
}

describe("EnvVarsEditor in an insecure context", () => {
  it("renders existing rows and adds a row without crypto.randomUUID", async () => {
    Object.defineProperty(globalThis.crypto, "randomUUID", {
      configurable: true,
      value: undefined,
    });
    try {
      let emitted = null;
      const view = await mount({
        value: { FOO: "bar", BAZ: "qux" },
        onChange: (next) => {
          emitted = next;
        },
      });
      const keys = view.container.querySelectorAll('[data-testid="env-vars-key"]');
      assert.equal(keys.length, 2, "both existing variables render");

      await act(async () => {
        view.container
          .querySelector('[data-testid="env-vars-add"]')
          .dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
      });
      assert.equal(
        view.container.querySelectorAll('[data-testid="env-vars-key"]').length,
        3,
        "Add variable appends a row",
      );
      assert.deepEqual(emitted, { FOO: "bar", BAZ: "qux" });
      await view.unmount();
    } finally {
      delete globalThis.crypto.randomUUID;
    }
  });
});

describe("EnvVarsEditor deep-link focus", () => {
  it("still focuses the requested key when a re-render lands before the frame", async () => {
    frameQueue = new Map();
    const props = {
      value: {},
      onChange: () => {},
      requiredKeys: ["ANTHROPIC_API_KEY"],
      focusKey: "ANTHROPIC_API_KEY",
    };
    const view = await mount(props);
    // Same content, new array identity: re-runs the focus effect and cancels
    // the frame the first run scheduled.
    await view.rerender({ ...props, requiredKeys: ["ANTHROPIC_API_KEY"] });
    await act(async () => flushFrames());

    const input = view.container.querySelector(
      '[data-testid="env-vars-required-value"]',
    );
    assert.ok(input, "the required row renders");
    // Compare identities as a boolean: a failing assert.equal on two JSDOM
    // nodes inspects the whole DOM graph and exhausts memory.
    assert.equal(
      document.activeElement === input,
      true,
      "the deep-linked required input is focused",
    );
    await view.unmount();
  });

  it("focuses once: a later requiredKeys change does not steal focus back", async () => {
    frameQueue = new Map();
    const props = {
      value: {},
      onChange: () => {},
      requiredKeys: ["ANTHROPIC_API_KEY"],
      focusKey: "ANTHROPIC_API_KEY",
    };
    const view = await mount(props);
    await act(async () => flushFrames());
    const other = document.createElement("input");
    document.body.appendChild(other);
    other.focus();

    await view.rerender({
      ...props,
      requiredKeys: ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"],
    });
    await act(async () => flushFrames());
    assert.equal(
      document.activeElement === other,
      true,
      "focus stays where the user moved it",
    );
    other.remove();
    await view.unmount();
  });
});
