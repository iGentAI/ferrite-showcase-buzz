import assert from "node:assert/strict";
import { after, afterEach, before, beforeEach, test } from "node:test";

import { JSDOM } from "jsdom";

// F404 / F405: the workflow message picker, rendered for real with the relay
// commands it calls (`get_channel_window`, `get_event`) answered in-process.

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
  pretendToBeVisual: true,
});

const CHANNEL = "channel-1";
const PASTED = "d".repeat(64);
const HISTORY_MESSAGE = "1".repeat(64);
const AUTHOR = "e".repeat(64);

let act, cleanup, fireEvent, render, screen, waitFor;
let createElement, QueryClient, QueryClientProvider;
let CommunitiesProvider, WorkflowMessagePicker, windowBoundsKey;
let handlers, scrollCalls, clients;

before(async () => {
  Object.assign(globalThis, {
    document: dom.window.document,
    getComputedStyle: dom.window.getComputedStyle.bind(dom.window),
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    localStorage: dom.window.localStorage,
    window: dom.window,
  });
  for (const name of [
    "CustomEvent",
    "Element",
    "Event",
    "FocusEvent",
    "KeyboardEvent",
    "MouseEvent",
    "MutationObserver",
    "Node",
  ]) {
    globalThis[name] = dom.window[name];
  }
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: dom.window.navigator,
  });
  dom.window.HTMLElement.prototype.scrollIntoView = function scrollIntoView() {
    scrollCalls += 1;
  };
  dom.window.__TAURI_INTERNALS__ = {
    invoke: async (command, args) => {
      const handler = handlers.get(command);
      if (handler) return handler(args);
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
  ({ CommunitiesProvider } = await import(
    "../../communities/useCommunities.tsx"
  ));
  ({ WorkflowMessagePicker } = await import("./WorkflowMessagePicker.tsx"));
  ({ windowBoundsKey } = await import(
    "../../../shared/api/channelWindowKey.ts"
  ));
});

beforeEach(() => {
  handlers = new Map();
  scrollCalls = 0;
  clients = [];
});

afterEach(async () => {
  cleanup();
  // Some app queries set their own retry and gcTime; cancel in-flight
  // retries before clearing, or they re-arm timers that hold the process.
  for (const client of clients) {
    await client.cancelQueries();
    client.clear();
  }
});

after(() => dom.window.close());

function messageEvent(id, channelId, content = "deploy finished") {
  return {
    id,
    pubkey: AUTHOR,
    kind: 9,
    created_at: 100,
    content,
    tags: [["h", channelId]],
    sig: "",
  };
}

function historyPage(messages) {
  return [
    ...messages,
    {
      id: "f".repeat(64),
      pubkey: AUTHOR,
      kind: 39006,
      created_at: 1,
      content: JSON.stringify({ has_more: false, next_cursor: null }),
      tags: [["d", windowBoundsKey(CHANNEL, null)]],
      sig: "",
    },
  ];
}

function pickerElement(client, onChange) {
  return createElement(
    QueryClientProvider,
    { client },
    createElement(
      CommunitiesProvider,
      null,
      createElement(WorkflowMessagePicker, {
        channelId: CHANNEL,
        id: "picker",
        onChange,
        value: "",
      }),
    ),
  );
}

function renderPicker() {
  const changes = [];
  const onChange = (value) => changes.push(value);
  const client = new QueryClient({
    // No garbage-collection timers: they would hold the test process open
    // for five minutes after the last assertion.
    defaultOptions: { queries: { retry: false, gcTime: Infinity } },
  });
  clients.push(client);
  const view = render(pickerElement(client, onChange));
  const input = screen.getByRole("combobox", {
    name: "Search messages or paste a message ID",
  });
  return {
    changes,
    input,
    rerender: () => view.rerender(pickerElement(client, onChange)),
  };
}

async function pasteAndCommit(input) {
  await act(async () => {
    fireEvent.change(input, { target: { value: PASTED } });
  });
  const option = await screen.findByRole("option");
  await act(async () => {
    fireEvent.keyDown(input, { key: "Enter" });
  });
  await act(async () => {
    fireEvent.click(option);
  });
}

test("F404: a pasted message ID cannot be selected while its channel check is still pending", async () => {
  handlers.set("get_channel_window", () => historyPage([]));
  handlers.set("get_event", () => new Promise(() => {}));
  const { changes, input } = renderPicker();

  await pasteAndCommit(input);

  assert.deepEqual(
    changes,
    [],
    "an ID the relay had not yet confirmed was committed",
  );
  assert.ok(screen.getByText(/Checking that message is in this channel/));
});

test("F404: a pasted message ID cannot be selected when its channel check fails", async () => {
  handlers.set("get_channel_window", () => historyPage([]));
  handlers.set("get_event", () => {
    throw new Error(`No event ${PASTED} on this relay.`);
  });
  const { changes, input } = renderPicker();

  await act(async () => {
    fireEvent.change(input, { target: { value: PASTED } });
  });
  // Wait until the lookup has failed (the picker offers a retry).
  await screen.findByText(/Couldn’t load all messages/);
  const option = screen.getByRole("option");
  await act(async () => {
    fireEvent.keyDown(input, { key: "Enter" });
  });
  await act(async () => {
    fireEvent.click(option);
  });

  assert.deepEqual(
    changes,
    [],
    "an ID whose lookup failed was committed without confirmation",
  );
  assert.ok(screen.getByText(/Couldn’t confirm that message is in this channel/));
});

test("F404: a pasted message ID from another channel is still refused", async () => {
  handlers.set("get_channel_window", () => historyPage([]));
  handlers.set("get_event", () =>
    JSON.stringify(messageEvent(PASTED, "some-other-channel")),
  );
  const { changes, input } = renderPicker();

  await act(async () => {
    fireEvent.change(input, { target: { value: PASTED } });
  });
  await screen.findByText(/That message is not available in this channel/);
  await act(async () => {
    fireEvent.keyDown(input, { key: "Enter" });
  });

  assert.deepEqual(changes, []);
});

test("F404: a pasted message ID the relay confirms in this channel can be selected", async () => {
  handlers.set("get_channel_window", () => historyPage([]));
  handlers.set("get_event", () => JSON.stringify(messageEvent(PASTED, CHANNEL)));
  const { changes, input } = renderPicker();

  await act(async () => {
    fireEvent.change(input, { target: { value: PASTED } });
  });
  await screen.findByText("deploy finished");
  await act(async () => {
    fireEvent.keyDown(input, { key: "Enter" });
  });

  assert.deepEqual(changes, [PASTED]);
});

test("F405: an unrelated re-render does not scroll the active option again", async () => {
  handlers.set("get_channel_window", () =>
    historyPage([messageEvent(HISTORY_MESSAGE, CHANNEL)]),
  );
  const { input, rerender } = renderPicker();
  await screen.findByText("deploy finished");

  await act(async () => {
    fireEvent.keyDown(input, { key: "ArrowDown" });
  });
  // Let the profile lookup for the row's author settle.
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 50));
  });
  assert.ok(scrollCalls >= 1, "moving the active option scrolls it into view");

  const before = scrollCalls;
  for (let index = 0; index < 3; index += 1) {
    await act(async () => {
      rerender();
    });
  }

  assert.equal(
    scrollCalls - before,
    0,
    "re-rendering with nothing changed scrolled the list again",
  );
});
