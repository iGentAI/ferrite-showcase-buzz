import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";
import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "https://buzz.example/buzz/ui/",
});

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

let React, act, createRoot, toast, McpConnectorsSettingsCard, authorizeConnector;

before(async () => {
  ({ default: React, act } = await import("react"));
  ({ createRoot } = await import("react-dom/client"));
  ({ toast } = await import("sonner"));
  ({ McpConnectorsSettingsCard } = await import("./McpConnectorsSettingsCard.tsx"));
  ({ authorizeConnector } = await import("../lib/mcpConnectors.ts"));
});

after(() => dom.window.close());

function catalogResponse() {
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

async function mountCard() {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  await act(async () => {
    root.render(React.createElement(McpConnectorsSettingsCard));
  });
  return {
    container,
    unmount: async () => {
      await act(async () => root.unmount());
      container.remove();
    },
  };
}

async function flushAsync() {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 20));
  });
}

describe("McpConnectorsSettingsCard authorization URL security and desktop/browser opening", () => {
  it("rejects javascript:, non-https:, and credentialed authorization_url values without navigating same-origin about:blank", async () => {
    const originalFetch = globalThis.fetch;
    const originalOpen = dom.window.open;

    for (const hostileUrl of [
      "javascript:window.__stolen=localStorage.getItem('buzz-web:sk')",
      "http://evil.example/oauth/authorize",
      "file:///etc/passwd",
      "https://user:pass@linear.app/oauth/authorize",
    ]) {
      let claimedPopup = null;
      dom.window.open = () => {
        claimedPopup = {
          opener: {},
          closed: false,
          closeCalled: false,
          close() {
            this.closed = true;
            this.closeCalled = true;
          },
          location: {
            replacedWith: null,
            replace(target) {
              this.replacedWith = target;
            },
          },
        };
        return claimedPopup;
      };
      globalThis.window.open = dom.window.open;

      globalThis.fetch = async (url) => {
        const href = String(url);
        if (href.includes("/mcp/connectors")) return catalogResponse();
        if (href.includes("/mcp/authorize")) {
          return new Response(
            JSON.stringify({
              authorization_url: hostileUrl,
              issuer: "https://linear.app",
              scopes: ["read"],
            }),
            { status: 200, headers: { "Content-Type": "application/json" } },
          );
        }
        throw new Error(`Unexpected fetch: ${href}`);
      };

      await assert.rejects(
        () => authorizeConnector("linear"),
        /authorization URL/i,
        `authorizeConnector must reject ${hostileUrl}`,
      );

      const view = await mountCard();
      await flushAsync();

      const connectBtn = view.container.querySelector(
        '[data-testid="mcp-connect-linear"]',
      );
      assert.ok(connectBtn, "Connect button rendered");

      await act(async () => {
        connectBtn.dispatchEvent(
          new dom.window.MouseEvent("click", { bubbles: true }),
        );
      });
      await flushAsync();

      assert.ok(claimedPopup, "popup was claimed synchronously");
      assert.equal(
        claimedPopup.location.replacedWith,
        null,
        `must never navigate same-origin about:blank to ${hostileUrl}`,
      );
      assert.equal(
        claimedPopup.closeCalled,
        true,
        `must close claimed blank popup when ${hostileUrl} is rejected`,
      );
      assert.equal(
        view.container.querySelector('[data-testid="mcp-pending-authorization"]'),
        null,
        "must not store hostile URL in pending authorization state",
      );

      await view.unmount();
    }

    globalThis.fetch = originalFetch;
    dom.window.open = originalOpen;
    globalThis.window.open = originalOpen;
  });

  it("does not raise a false popup-blocked error when browser window.open with noopener returns null", async () => {
    const originalFetch = globalThis.fetch;
    const originalOpen = dom.window.open;
    const originalToastError = toast.error;
    const errorToasts = [];
    const openedCalls = [];

    toast.error = (msg) => {
      errorToasts.push(String(msg));
    };

    dom.window.open = (url, target, features) => {
      openedCalls.push({ url, target, features });
      // Initial connect("about:blank") blocked, forcing manual button;
      // manual openAuthorizationPage(..., "noopener,noreferrer") returns null per HTML spec.
      return null;
    };
    globalThis.window.open = dom.window.open;

    globalThis.fetch = async (url) => {
      const href = String(url);
      if (href.includes("/mcp/connectors")) return catalogResponse();
      if (href.includes("/mcp/authorize")) {
        return new Response(
          JSON.stringify({
            authorization_url: "https://linear.app/oauth/authorize?state=ok123",
            issuer: "https://linear.app",
            scopes: ["read"],
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      throw new Error(`Unexpected fetch: ${href}`);
    };

    try {
      const view = await mountCard();
      await flushAsync();

      await act(async () => {
        view.container
          .querySelector('[data-testid="mcp-connect-linear"]')
          .dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
      });
      await flushAsync();

      const openAuthBtn = view.container.querySelector(
        '[data-testid="mcp-open-authorization"]',
      );
      assert.ok(openAuthBtn, "manual Open authorization page button rendered");

      await act(async () => {
        openAuthBtn.dispatchEvent(
          new dom.window.MouseEvent("click", { bubbles: true }),
        );
      });
      await flushAsync();

      assert.equal(
        openedCalls.some(
          (call) =>
            call.url === "https://linear.app/oauth/authorize?state=ok123" &&
            typeof call.features === "string" &&
            call.features.includes("noopener"),
        ),
        true,
        "manual open button calls window.open with noopener",
      );
      assert.deepEqual(
        errorToasts,
        [],
        "noopener null return value must not trigger false popup-blocked toast",
      );

      await view.unmount();
    } finally {
      toast.error = originalToastError;
      globalThis.fetch = originalFetch;
      dom.window.open = originalOpen;
      globalThis.window.open = originalOpen;
    }
  });
});
