/**
 * F347 at the viewer: sending an unlocked, memory-bearing agent card must not
 * open a DM or upload anything until the user confirms a dialog that names
 * the recipients. Mounts the real AgentCardViewerDialog (recipient picker,
 * send controller and all) against a stubbed desktop bridge and watches the
 * commands it issues.
 */
import assert from "node:assert/strict";
import { after, afterEach, before, beforeEach, test } from "node:test";

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
const originalDispatch = dom.window.EventTarget.prototype.dispatchEvent;
dom.window.EventTarget.prototype.dispatchEvent = function (event) {
  if (!(event instanceof dom.window.Event)) return false;
  return originalDispatch.call(this, event);
};

const SELF = "1".repeat(64);
const ADA = "a".repeat(64);
const DM_ID = "dm-channel-1";

/** Commands that open a DM, upload the card or send a message. */
const WRITE_COMMANDS = new Set([
  "open_dm",
  "upload_media_bytes",
  "send_channel_message",
  "send_message",
]);

let commands = [];
// Test levers: while set, the matching command stays pending until released.
let holdSave = null;
let holdRelaySelf = false;

function invoke(cmd, payload) {
  commands.push({ cmd, payload });
  switch (cmd) {
    case "get_identity":
      return Promise.resolve({ pubkey: SELF, display_name: "Me" });
    case "get_relay_self":
      return holdRelaySelf ? new Promise(() => {}) : Promise.resolve(null);
    case "save_agent_card":
      return holdSave ? holdSave.promise : Promise.resolve(false);
    case "search_users":
      return Promise.resolve({
        users: [
          {
            pubkey: ADA,
            display_name: "Ada",
            avatar_url: null,
            nip05_handle: null,
            owner_pubkey: null,
            is_agent: false,
          },
        ],
        next_cursor: null,
      });
    case "open_dm":
      return Promise.resolve({
        id: DM_ID,
        name: "Ada",
        channel_type: "dm",
        visibility: "private",
        description: "",
        topic: null,
        purpose: null,
        member_count: 2,
        member_pubkeys: [SELF, ADA],
        last_message_at: null,
        archived_at: null,
        participants: [],
        participant_pubkeys: [SELF, ADA],
        is_member: true,
      });
    default:
      return Promise.reject(new Error(`unmocked: ${cmd}`));
  }
}

globalThis.__TAURI_INTERNALS__ = { invoke, transformCallback: () => 1 };
dom.window.__TAURI_INTERNALS__ = globalThis.__TAURI_INTERNALS__;

let act, render, screen, cleanup, fireEvent, createElement;
let AgentCardViewerDialog, openCardViewer, closeCardViewer;
let CommunitiesProvider, QueryClient, QueryClientProvider, ThemeProvider;
const clients = [];

before(async () => {
  ({ act, render, screen, cleanup, fireEvent } = await import(
    "@testing-library/react"
  ));
  ({ createElement } = await import("react"));
  ({ QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  ));
  ({ CommunitiesProvider } = await import(
    "@/features/communities/useCommunities.tsx"
  ));
  ({ ThemeProvider } = await import("@/shared/theme/ThemeProvider.tsx"));
  ({ openCardViewer, closeCardViewer } = await import(
    "@/features/agents/cardMintStore.ts"
  ));
  ({ AgentCardViewerDialog } = await import("./AgentCardViewerDialog.tsx"));
});

beforeEach(() => {
  commands = [];
  holdSave = null;
  holdRelaySelf = false;
});

afterEach(() => {
  act(() => closeCardViewer());
  cleanup?.();
  for (const client of clients.splice(0)) {
    client.cancelQueries();
    client.clear();
  }
});

after(() => dom.window.close());

async function settle(ms = 30) {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, ms));
  });
}

function writes() {
  return commands.filter(({ cmd }) => WRITE_COMMANDS.has(cmd));
}

async function mountViewerWithRecipient(card) {
  const client = new QueryClient({
    // gcTime 0: otherwise unmounting schedules five-minute GC timers that
    // keep the test process alive.
    defaultOptions: {
      queries: { retry: false, gcTime: 0 },
      mutations: { retry: false, gcTime: 0 },
    },
  });
  clients.push(client);
  render(
    createElement(
      QueryClientProvider,
      { client },
      createElement(
        ThemeProvider,
        null,
        createElement(
          CommunitiesProvider,
          null,
          createElement(AgentCardViewerDialog),
        ),
      ),
    ),
  );
  await act(async () => {
    openCardViewer({
      agentName: "Eva",
      card: {
        cardPngBase64: "AAAA",
        fileName: "eva.agent.png",
        designerNotes: "",
        locked: false,
        memoryLevel: "everything",
        ...card,
      },
      remint: null,
    });
  });
  await settle(50);

  const search = screen.getByTestId("agent-card-share-recipient-search");
  await act(async () => {
    fireEvent.focus(search);
    fireEvent.change(search, { target: { value: "Ada" } });
  });
  await settle(80);
  await act(async () => {
    fireEvent.click(
      screen.getByTestId(`agent-card-share-recipient-option-${ADA}`),
    );
  });
  await settle(30);
  const send = screen.getByTestId("agent-card-send");
  assert.equal(send.disabled, false, "Send is enabled once DM safety is ready");
  return send;
}

test("a memory-bearing card asks first, and cancel sends nothing", async () => {
  const send = await mountViewerWithRecipient({});
  await act(async () => {
    fireEvent.click(send);
  });
  await settle();

  const confirmation = screen.getByTestId(
    "agent-card-share-memory-confirmation",
  );
  assert.match(confirmation.textContent ?? "", /plaintext all memories/);
  assert.match(confirmation.textContent ?? "", /Ada/);
  assert.deepEqual(writes(), [], "nothing opened or uploaded before confirm");

  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  });
  await settle();
  assert.equal(
    screen.queryByTestId("agent-card-share-memory-confirmation") !== null,
    false,
  );
  assert.deepEqual(writes(), [], "cancel sends nothing");
});

test("confirming sends to the captured recipients", async () => {
  const send = await mountViewerWithRecipient({ memoryLevel: "core" });
  await act(async () => {
    fireEvent.click(send);
  });
  await settle();
  assert.deepEqual(writes(), []);

  await act(async () => {
    fireEvent.click(screen.getByTestId("agent-card-share-memory-confirm"));
  });
  await settle(80);

  const openDm = commands.find(({ cmd }) => cmd === "open_dm");
  assert.ok(openDm, "confirming opens the DM");
  assert.deepEqual(openDm.payload?.pubkeys ?? openDm.payload?.input?.pubkeys, [
    ADA,
  ]);
});

test("a config-only card sends without a memory confirmation", async () => {
  const send = await mountViewerWithRecipient({ memoryLevel: "none" });
  await act(async () => {
    fireEvent.click(send);
  });
  await settle(80);
  assert.equal(
    screen.queryByTestId("agent-card-share-memory-confirmation") !== null,
    false,
  );
  assert.ok(commands.some(({ cmd }) => cmd === "open_dm"));
});

// The confirmation dialog calls the send path directly, so it must refuse on
// its own when the viewer turned busy or lost DM safety while it was open.
test("confirming while a save is in flight sends nothing", async () => {
  const send = await mountViewerWithRecipient({});
  await act(async () => {
    fireEvent.click(send);
  });
  await settle();
  assert.ok(screen.getByTestId("agent-card-share-memory-confirmation"));

  let release;
  holdSave = { promise: new Promise((resolve) => (release = resolve)) };
  await act(async () => {
    fireEvent.click(screen.getByTestId("agent-card-save"));
  });
  await settle();
  assert.ok(commands.some(({ cmd }) => cmd === "save_agent_card"));

  await act(async () => {
    fireEvent.click(screen.getByTestId("agent-card-share-memory-confirm"));
  });
  await settle(80);
  assert.deepEqual(writes(), [], "no DM opened or upload while busy");

  release(false);
  await settle();
});

test("confirming after DM safety is lost sends nothing", async () => {
  const send = await mountViewerWithRecipient({});
  await act(async () => {
    fireEvent.click(send);
  });
  await settle();
  assert.ok(screen.getByTestId("agent-card-share-memory-confirmation"));

  // Relay identity goes unknown again (refetch pending): not DM-safe.
  holdRelaySelf = true;
  const client = clients[clients.length - 1];
  await act(async () => {
    void client.resetQueries({ queryKey: ["relaySelf"] });
  });
  await settle();
  assert.equal(screen.getByTestId("agent-card-send").disabled, true);

  await act(async () => {
    fireEvent.click(screen.getByTestId("agent-card-share-memory-confirm"));
  });
  await settle(80);
  assert.deepEqual(writes(), [], "no DM opened while not DM-safe");
});
