// F169 / F429 / F430 / F431: link chips in rendered messages must not start
// relay lookups for every link on mount, must not reveal spoiler-hidden
// targets through their tooltips, and must not print author-chosen channel
// names on unresolved message chips.
import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";
import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});
Object.assign(globalThis, {
  IS_REACT_ACT_ENVIRONMENT: true,
  ResizeObserver: class {
    observe() {}
    unobserve() {}
    disconnect() {}
  },
  document: dom.window.document,
  localStorage: dom.window.localStorage,
  self: dom.window,
  window: dom.window,
});
Object.defineProperty(globalThis, "navigator", {
  configurable: true,
  value: dom.window.navigator,
});
dom.window.requestAnimationFrame = (callback) => setTimeout(callback, 0);
globalThis.requestAnimationFrame = dom.window.requestAnimationFrame;
globalThis.cancelAnimationFrame = (handle) => clearTimeout(handle);
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
  if (
    !(key in globalThis) &&
    (key.startsWith("HTML") ||
      key.startsWith("SVG") ||
      [
        "Node",
        "Event",
        "CustomEvent",
        "MouseEvent",
        "KeyboardEvent",
        "FocusEvent",
        "PointerEvent",
        "EventTarget",
        "Text",
        "DocumentFragment",
        "MutationObserver",
      ].includes(key))
  ) {
    const val = dom.window[key];
    if (val !== undefined) globalThis[key] = val;
  }
}
globalThis.getComputedStyle = dom.window.getComputedStyle.bind(dom.window);
const originalDispatch = dom.window.EventTarget.prototype.dispatchEvent;
dom.window.EventTarget.prototype.dispatchEvent = function (event) {
  if (!(event instanceof dom.window.Event)) return false;
  return originalDispatch.call(this, event);
};

let React;
let act;
let createRoot;
let MessageLinkPill;
let MessageLinkMetadataStoreContext;
let createMessageLinkMetadataStore;
let SpoilerHiddenContext;
let EntityLinkAnchor;
let renderEntityLinkAnchor;
let relayClient;
let QueryClient;
let QueryClientProvider;

before(async () => {
  ({ default: React, act } = await import("react"));
  ({ createRoot } = await import("react-dom/client"));
  ({ MessageLinkPill } = await import("./MessageLinkPill.tsx"));
  ({ MessageLinkMetadataStoreContext } = await import(
    "./useMessageLinkMetadata.ts"
  ));
  ({ createMessageLinkMetadataStore } = await import(
    "./messageLinkMetadataStore.ts"
  ));
  ({ SpoilerHiddenContext } = await import("./SpoilerInline.tsx"));
  ({ EntityLinkAnchor, renderEntityLinkAnchor } = await import(
    "./entityLinks.tsx"
  ));
  ({ relayClient } = await import("@/shared/api/relayClient"));
  ({ QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  ));
});

after(() => dom.window.close());

const CHANNEL = "f570339f-8f8a-4e08-a779-8d954aa44109";
const CHANNELS = [
  { id: CHANNEL, name: "general", channelType: "stream", visibility: "open" },
];

function eventId(n) {
  return n.toString(16).padStart(64, "0");
}

function countingStore(options) {
  const calls = [];
  const store = createMessageLinkMetadataStore(
    {
      getEvent: async (messageId) => {
        calls.push(messageId);
        return {
          content: `linked ${messageId.slice(-3)}`,
          created_at: Math.floor(Date.now() / 1000),
          kind: 9,
          pubkey: "a".repeat(64),
          tags: [["h", CHANNEL]],
        };
      },
      getProfile: async () => ({ displayName: "Ana" }),
      isDefinitiveNotFound: () => false,
      shortPubkey: (pubkey) => pubkey.slice(0, 8),
      summarize: (content) => content,
      wait: async () => {},
    },
    options,
  );
  return { calls, store };
}

async function mount(element) {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  await act(async () => {
    root.render(element);
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
  return {
    container,
    async unmount() {
      await act(async () => root.unmount());
      container.remove();
    },
  };
}

async function settle() {
  await act(async () => {
    for (let n = 0; n < 5; n += 1) {
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
  });
}

async function hover(element) {
  await act(async () => {
    element.dispatchEvent(
      new dom.window.MouseEvent("pointerover", { bubbles: true }),
    );
    element.dispatchEvent(
      new dom.window.MouseEvent("mouseover", { bubbles: true }),
    );
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

async function focus(element) {
  await act(async () => {
    element.focus();
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

function pill(n, props = {}) {
  return React.createElement(MessageLinkPill, {
    key: n,
    channels: CHANNELS,
    interactive: true,
    link: { channelId: CHANNEL, messageId: eventId(n), threadRootId: null },
    onOpenChannel: () => {},
    onOpenMessageLink: () => {},
    ...props,
  });
}

function chips(container) {
  return [...container.querySelectorAll("[data-message-link]")];
}

describe("message-link pills", () => {
  it("a message full of links resolves only a bounded number on mount (F169)", async () => {
    const { calls, store } = countingStore({ maxEager: 3 });
    const view = await mount(
      React.createElement(
        MessageLinkMetadataStoreContext.Provider,
        { value: store },
        Array.from({ length: 12 }, (_, n) => pill(n)),
      ),
    );
    await settle();
    assert.equal(calls.length, 3, "only the eager budget reached the relay");

    // The reader hovers a pill that was not resolved on mount.
    const last = chips(view.container).at(-1);
    await hover(last);
    await settle();
    assert.equal(calls.length, 4);
    assert.ok(calls.includes(eventId(11)));

    // Focus works the same way for keyboard readers.
    await focus(chips(view.container).at(-2));
    await settle();
    assert.equal(calls.length, 5);
    await view.unmount();
  });

  it("a pill inside an unrevealed spoiler never resolves or shows a tooltip (F429)", async () => {
    const { calls, store } = countingStore();
    const hidden = await mount(
      React.createElement(
        MessageLinkMetadataStoreContext.Provider,
        { value: store },
        React.createElement(
          SpoilerHiddenContext.Provider,
          { value: true },
          pill(1),
        ),
      ),
    );
    await settle();
    const chip = chips(hidden.container)[0];
    await hover(chip);
    await focus(chip);
    await settle();
    assert.equal(calls.length, 0, "no lookup for a hidden spoiler's link");
    assert.equal(
      chips(hidden.container)[0].hasAttribute("data-state"),
      false,
      "no tooltip trigger wraps the hidden chip",
    );
    await hidden.unmount();

    // The same pill outside a spoiler resolves and gets its tooltip.
    const shown = await mount(
      React.createElement(
        MessageLinkMetadataStoreContext.Provider,
        { value: store },
        pill(1),
      ),
    );
    await settle();
    assert.equal(calls.length, 1);
    assert.equal(chips(shown.container)[0].getAttribute("data-state"), "closed");
    await shown.unmount();
  });

  it("an unresolved slug channel shows a neutral label, not the author's words (F430)", async () => {
    const { store } = countingStore();
    const view = await mount(
      React.createElement(
        MessageLinkMetadataStoreContext.Provider,
        { value: store },
        pill(1, {
          channels: [],
          link: {
            channelId: "announcements",
            messageId: eventId(1),
            threadRootId: null,
          },
        }),
        pill(2, {
          channels: [],
          link: { channelId: CHANNEL, messageId: eventId(2), threadRootId: null },
        }),
      ),
    );
    const [slug, uuid] = chips(view.container);
    assert.doesNotMatch(slug.textContent, /announce/i);
    assert.match(slug.textContent, /unknown channel/);
    assert.match(uuid.textContent, /f570339f/);
    await view.unmount();
  });
});

const OWNER = "a".repeat(64);

function issueHref(n) {
  return `buzz://issue?id=${eventId(n)}&owner=${OWNER}&d=repo-${n}`;
}

describe("entity-link chips (F431)", () => {
  it("do not query the relay on mount, only when a reader shows intent", async () => {
    const original = relayClient.fetchEvents;
    const filters = [];
    relayClient.fetchEvents = async (filter) => {
      filters.push(filter);
      return [];
    };
    try {
      const view = await mount(
        React.createElement(
          "div",
          null,
          Array.from({ length: 6 }, (_, n) =>
            React.createElement(
              React.Fragment,
              { key: n },
              renderEntityLinkAnchor({
                children: issueHref(n),
                href: issueHref(n),
                interactive: true,
                onOpenEntityLink: () => {},
                relayOrigin: null,
              }),
            ),
          ),
        ),
      );
      await settle();
      assert.equal(filters.length, 0, "mounting six entity links sent nothing");

      const chip = view.container.querySelectorAll("[data-buzz-link-kind]")[2];
      await hover(chip);
      await settle();
      assert.ok(filters.length >= 1, "hovering resolves that one chip");
      const afterHover = filters.length;
      await settle();
      assert.equal(filters.length, afterHover, "no other chip resolved");
      await view.unmount();
    } finally {
      relayClient.fetchEvents = original;
    }
  });

  it("inside an unrevealed spoiler, never resolve or show a tooltip", async () => {
    const original = relayClient.fetchEvents;
    let queries = 0;
    relayClient.fetchEvents = async () => {
      queries += 1;
      return [];
    };
    try {
      const view = await mount(
        React.createElement(
          SpoilerHiddenContext.Provider,
          { value: true },
          renderEntityLinkAnchor({
            children: issueHref(40),
            href: issueHref(40),
            interactive: true,
            onOpenEntityLink: () => {},
            relayOrigin: null,
          }),
        ),
      );
      const chip = view.container.querySelector("[data-buzz-link-kind]");
      await hover(chip);
      await focus(chip);
      await settle();
      assert.equal(queries, 0);
      assert.equal(chip.hasAttribute("data-state"), false);
      await view.unmount();
    } finally {
      relayClient.fetchEvents = original;
    }
  });

  it("start the project scan only after a tooltip is requested", async () => {
    const original = relayClient.fetchEvents;
    relayClient.fetchEvents = async () => [];
    const queryClient = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    try {
      const view = await mount(
        React.createElement(
          QueryClientProvider,
          { client: queryClient },
          React.createElement(
            EntityLinkAnchor,
            {
              href: issueHref(50),
              interactive: true,
              onOpenEntityLink: () => {},
              relayOrigin: null,
            },
            issueHref(50),
          ),
        ),
      );
      await settle();
      const projects = () =>
        queryClient.getQueryCache().find({ queryKey: ["projects"], exact: true });
      const untouched = (query) =>
        !query ||
        (query.state.fetchStatus === "idle" &&
          query.state.dataUpdateCount === 0 &&
          query.state.errorUpdateCount === 0);
      assert.ok(untouched(projects()), "rendering an entity link did not scan projects");

      await hover(view.container.querySelector("[data-buzz-link-kind]"));
      await settle();
      assert.equal(untouched(projects()), false, "opening the tooltip may scan projects");
      await view.unmount();
    } finally {
      relayClient.fetchEvents = original;
      queryClient.clear();
    }
  });
});
