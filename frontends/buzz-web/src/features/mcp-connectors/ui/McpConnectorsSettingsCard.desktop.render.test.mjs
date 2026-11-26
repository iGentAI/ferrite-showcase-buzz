import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";
import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "https://buzz.example/buzz/ui/",
});

const openerUrls = [];
let windowOpenCalled = false;

dom.window.open = () => {
  windowOpenCalled = true;
  return null;
};
dom.window.__BUZZ_DESKTOP__ = {
  version: 1,
  commands: ["plugin:opener|open_url"],
};
dom.window.__TAURI_INTERNALS__ = {
  transformCallback: () => 1,
  invoke: async (cmd, args) => {
    if (cmd === "plugin:opener|open_url") {
      openerUrls.push(args.url);
      return null;
    }
    throw new Error(`Unexpected Tauri invoke: ${cmd}`);
  },
};

Object.assign(globalThis, {
  IS_REACT_ACT_ENVIRONMENT: true,
  document: dom.window.document,
  window: dom.window,
  self: dom.window,
  localStorage: dom.window.localStorage,
  sessionStorage: dom.window.sessionStorage,
});
Object.defineProperty(globalThis, "navigator", {
  configurable: true,
  value: dom.window.navigator,
});
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

let React, act, createRoot, McpConnectorsSettingsCard;

before(async () => {
  ({ default: React, act } = await import("react"));
  ({ createRoot } = await import("react-dom/client"));
  ({ McpConnectorsSettingsCard } = await import("./McpConnectorsSettingsCard.tsx"));
});

after(() => dom.window.close());

describe("McpConnectorsSettingsCard in Tauri desktop shell", () => {
  it("opens validated https authorization URL via plugin:opener|open_url without calling window.open", async () => {
    const originalFetch = globalThis.fetch;
    globalThis.fetch = async (url) => {
      const href = String(url);
      if (href.includes("/mcp/connectors")) {
        return new Response(
          JSON.stringify({
            operator: true,
            egress_available: true,
            connectors: [
              {
                id: "linear",
                name: "Linear",
                endpoint: "https://mcp.linear.app/mcp",
                summary: "Issue tracking",
                provenance: "documented",
                egress: "mcp-egress-partners",
                reachable: true,
                connected: false,
              },
            ],
            unavailable: [],
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      if (href.includes("/mcp/authorize")) {
        return new Response(
          JSON.stringify({
            authorization_url: "https://linear.app/oauth/authorize?state=desktop1",
            issuer: "https://linear.app",
            scopes: ["read"],
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      throw new Error(`Unexpected fetch: ${href}`);
    };

    try {
      const container = document.createElement("div");
      document.body.appendChild(container);
      const root = createRoot(container);
      await act(async () => {
        root.render(React.createElement(McpConnectorsSettingsCard));
      });
      await act(async () => {
        await new Promise((resolve) => setTimeout(resolve, 20));
      });

      await act(async () => {
        container
          .querySelector('[data-testid="mcp-connect-linear"]')
          .dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
      });
      await act(async () => {
        await new Promise((resolve) => setTimeout(resolve, 20));
      });

      assert.equal(windowOpenCalled, false, "desktop shell must not call window.open");
      assert.deepEqual(openerUrls, [
        "https://linear.app/oauth/authorize?state=desktop1",
      ]);

      await act(async () => root.unmount());
      container.remove();
    } finally {
      globalThis.fetch = originalFetch;
    }
  });
});
