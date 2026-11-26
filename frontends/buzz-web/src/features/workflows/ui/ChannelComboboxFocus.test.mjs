import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { JSDOM } from "jsdom";

// F401: the channel picker's search box used an inline ref callback that
// called focus(). React re-attaches a new callback on every render, so any
// re-render while the popover was open pulled focus back to the search box.

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
  pretendToBeVisual: true,
});

let act, cleanup, fireEvent, render, screen, waitFor;
let createElement, QueryClient, QueryClientProvider, ChannelCombobox;
let CommunitiesProvider;

before(async () => {
  Object.assign(globalThis, {
    document: dom.window.document,
    getComputedStyle: dom.window.getComputedStyle.bind(dom.window),
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    localStorage: dom.window.localStorage,
    window: dom.window,
  });
  // Radix and Floating UI reach for these as globals, and dispatch events
  // they construct from them, so they must be jsdom's own classes.
  for (const name of [
    "CustomEvent",
    "DocumentFragment",
    "Element",
    "Event",
    "FocusEvent",
    "HTMLButtonElement",
    "HTMLInputElement",
    "KeyboardEvent",
    "MouseEvent",
    "MutationObserver",
    "Node",
    "SVGElement",
    "cancelAnimationFrame",
    "requestAnimationFrame",
  ]) {
    const value = dom.window[name];
    globalThis[name] =
      typeof value === "function" && /^[a-z]/.test(name)
        ? value.bind(dom.window)
        : value;
  }
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: dom.window.navigator,
  });
  class ResizeObserver {
    observe() {}
    unobserve() {}
    disconnect() {}
  }
  globalThis.ResizeObserver = ResizeObserver;
  dom.window.ResizeObserver = ResizeObserver;
  dom.window.HTMLElement.prototype.scrollIntoView = () => {};
  dom.window.__TAURI_INTERNALS__ = {
    invoke: async (command) => {
      throw new Error(`Unexpected IPC: ${command}`);
    },
    transformCallback: () => 1,
  };
  ({ act, cleanup, fireEvent, render, screen, waitFor } = await import(
    "@testing-library/react"
  ));
  ({ createElement } = await import("react"));
  ({ QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  ));
  ({ ChannelCombobox } = await import("./ChannelCombobox.tsx"));
  ({ CommunitiesProvider } = await import(
    "../../communities/useCommunities.tsx"
  ));
});

afterEach(() => cleanup());

after(() => dom.window.close());

function channel(id, name) {
  return {
    id,
    name,
    channelType: "stream",
    visibility: "open",
    participantPubkeys: [],
  };
}

test("F401: a re-render while open does not pull focus back to the search box", async () => {
  const client = new QueryClient({
    // No garbage-collection timers: they would hold the test process open
    // for five minutes after the last assertion.
    defaultOptions: { queries: { retry: false, gcTime: Infinity } },
  });
  const view = (channels) =>
    createElement(
      QueryClientProvider,
      { client },
      createElement(
        CommunitiesProvider,
        null,
        createElement(ChannelCombobox, {
          channels,
          onChange() {},
          value: "",
          variant: "field",
        }),
      ),
    );
  const { rerender } = render(
    view([channel("c1", "general"), channel("c2", "deploys")]),
  );
  // Compare elements by identity with a readable label: letting assert diff
  // two jsdom elements serialises the whole document and stalls the run.
  const focused = () => {
    const active = dom.window.document.activeElement;
    return active?.getAttribute("aria-label") ?? active?.textContent ?? "none";
  };

  try {
    await act(async () => {
      fireEvent.click(screen.getByRole("combobox"));
    });

    const search = await screen.findByRole("combobox", {
      name: "Search channels",
    });
    // Opening still puts the cursor in the search box.
    await waitFor(() =>
      assert.ok(
        dom.window.document.activeElement === search,
        `focus is on ${focused()}`,
      ),
    );

    // A keyboard user moves onto an option…
    const option = screen.getByRole("option", { name: /deploys/ });
    option.focus();
    assert.ok(dom.window.document.activeElement === option);

    // …and the component re-renders (a parent update, a query settling).
    await act(async () => {
      rerender(view([channel("c1", "general"), channel("c2", "deploys")]));
    });
    await act(async () => {
      rerender(view([channel("c1", "general"), channel("c2", "deploys")]));
    });

    assert.ok(
      dom.window.document.activeElement === option,
      `a re-render moved focus from the option to ${focused()}`,
    );
  } finally {
    cleanup();
    await client.cancelQueries();
    client.clear();
  }
});
