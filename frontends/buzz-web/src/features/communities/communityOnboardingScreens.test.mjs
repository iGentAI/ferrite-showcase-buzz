/**
 * Component-level regression tests for the hosted-community and first-run
 * screens: F367/F369 (identity gate failed open while this device's key was
 * unknown), F371 (pending Builderlab sign-in not cancelled on unmount),
 * F372 (malformed pubkey crashed WelcomeSetup) and F373 (clipboard failure
 * was an unhandled rejection with no feedback).
 *
 * The real components are mounted in jsdom; Tauri commands are answered by
 * a scripted `__TAURI_INTERNALS__.invoke`, the seam the desktop bridge
 * routes mocked commands through.
 */
import assert from "node:assert/strict";
import { afterEach, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

Object.assign(globalThis, {
  HTMLElement: dom.window.HTMLElement,
  IS_REACT_ACT_ENVIRONMENT: true,
  MutationObserver: dom.window.MutationObserver,
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
dom.window.cancelAnimationFrame = (id) => clearTimeout(id);
globalThis.requestAnimationFrame = dom.window.requestAnimationFrame;
globalThis.cancelAnimationFrame = dom.window.cancelAnimationFrame;
dom.window.ResizeObserver = globalThis.ResizeObserver;
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
      key.startsWith("CSS") ||
      [
        "Node",
        "NodeFilter",
        "NodeList",
        "NamedNodeMap",
        "Event",
        "CustomEvent",
        "MouseEvent",
        "KeyboardEvent",
        "FocusEvent",
        "InputEvent",
        "PointerEvent",
        "TouchEvent",
        "WheelEvent",
        "EventTarget",
        "Text",
        "Comment",
        "DocumentFragment",
        "Range",
        "Selection",
        "getComputedStyle",
        "IntersectionObserver",
      ].includes(key))
  ) {
    const value = dom.window[key];
    if (value !== undefined) globalThis[key] = value;
  }
}
globalThis.getComputedStyle = dom.window.getComputedStyle.bind(dom.window);
const originalDispatch = dom.window.EventTarget.prototype.dispatchEvent;
dom.window.EventTarget.prototype.dispatchEvent = function (event) {
  if (!(event instanceof dom.window.Event)) return false;
  return originalDispatch.call(this, event);
};
globalThis.EventTarget = dom.window.EventTarget;

/** Scripted Tauri commands; each test installs its own table. */
let commands = {};
const invoked = [];
dom.window.__TAURI_INTERNALS__ = {
  invoke: (command, args) => {
    invoked.push(command);
    const handler = commands[command];
    if (!handler) return Promise.reject(new Error(`unmocked: ${command}`));
    return handler(args);
  },
};

const React = await import("react");
const { createRoot } = await import("react-dom/client");
const { QueryClient, QueryClientProvider } = await import(
  "@tanstack/react-query"
);
const { CommunityOnboardingProvider } = await import(
  "../onboarding/communityOnboarding.tsx"
);
const { HostedCommunityOnboarding } = await import(
  "./ui/HostedCommunityOnboarding.tsx"
);
const { HostedCommunityCreateFlow } = await import(
  "./ui/HostedCommunityCreateFlow.tsx"
);
const { WelcomeSetup } = await import("./ui/WelcomeSetup.tsx");
const { ThemeProvider } = await import("@/shared/theme/ThemeProvider.tsx");

const KEY_A = "a".repeat(64);
const KEY_B = "b".repeat(64);
const AUTH = { email: "owner@example.com", expiresAt: "2099-01-01T00:00:00Z" };
const never = () => new Promise(() => {});

let mounted = [];
let clients = [];

async function flush(rounds = 5) {
  for (let index = 0; index < rounds; index += 1) {
    await React.act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
  }
}

async function render(element, { identity } = {}) {
  const client = new QueryClient({
    // gcTime Infinity: no garbage-collection timers left holding the process.
    defaultOptions: { queries: { retry: false, gcTime: Number.POSITIVE_INFINITY } },
  });
  clients.push(client);
  if (identity !== undefined) client.setQueryData(["identity"], identity);
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  await React.act(async () => {
    root.render(
      React.createElement(
        ThemeProvider,
        null,
        React.createElement(
          QueryClientProvider,
          { client },
          React.createElement(CommunityOnboardingProvider, null, element),
        ),
      ),
    );
  });
  await flush();
  const handle = {
    unmount: async () => {
      await React.act(async () => root.unmount());
      mounted = mounted.filter((entry) => entry !== handle);
    },
  };
  mounted.push(handle);
  return handle;
}

function bodyText() {
  return document.body.textContent ?? "";
}

/** Boolean presence check: a failing assert must not try to print a DOM node. */
function present(testId) {
  return document.querySelector(`[data-testid="${testId}"]`) !== null;
}

function button(label) {
  const match = [...document.querySelectorAll("button")].find(
    (candidate) =>
      candidate.textContent?.includes(label) ||
      candidate.getAttribute("aria-label") === label,
  );
  assert.equal(match !== undefined, true, `button "${label}" is rendered`);
  return match;
}

async function click(element) {
  await React.act(async () => {
    element.dispatchEvent(
      new dom.window.MouseEvent("click", { bubbles: true, cancelable: true }),
    );
  });
  await flush();
}

afterEach(async () => {
  for (const handle of [...mounted]) await handle.unmount();
  for (const client of clients) client.clear();
  clients = [];
  document.body.innerHTML = "";
  dom.window.localStorage.clear();
  commands = {};
  invoked.length = 0;
});

const signedInAccount = {
  get_builderlab_auth: async () => AUTH,
  get_builderlab_nostr_identity: async () => ({
    identity: { pubkey_hex: KEY_A },
  }),
  list_builderlab_communities: async () => ({ communities: [] }),
};

test("F371: unmounting during a pending Builderlab sign-in cancels it", async () => {
  let cancels = 0;
  commands = {
    get_builderlab_auth: async () => null,
    start_builderlab_login: never,
    cancel_builderlab_login: async () => {
      cancels += 1;
      return null;
    },
  };
  const handle = await render(
    React.createElement(HostedCommunityOnboarding, {
      onBack: () => {},
      stageHidden: true,
    }),
    { identity: { pubkey: KEY_A } },
  );

  await click(button("Sign in to continue"));
  assert.ok(invoked.includes("start_builderlab_login"));
  assert.equal(cancels, 0);

  await handle.unmount();
  await flush(1);

  assert.equal(cancels, 1);
});

test("F369: onboarding stays closed while this device's key is still loading", async () => {
  let readyCalls = 0;
  commands = { ...signedInAccount, get_identity: never };
  await render(
    React.createElement(HostedCommunityOnboarding, {
      onBack: () => {},
      onReady: () => {
        readyCalls += 1;
      },
    }),
  );

  assert.equal(readyCalls, 0);
  assert.match(bodyText(), /Checking this device’s identity/);
  assert.equal(present("hosted-community-create-surface"), false);
});

test("F369: onboarding refuses to proceed when this device's key cannot be read", async () => {
  let readyCalls = 0;
  commands = {
    ...signedInAccount,
    get_identity: async () => {
      throw new Error("keyring unavailable");
    },
  };
  await render(
    React.createElement(HostedCommunityOnboarding, {
      onBack: () => {},
      onReady: () => {
        readyCalls += 1;
      },
    }),
  );

  assert.equal(readyCalls, 0);
  assert.match(bodyText(), /Couldn’t read this device’s identity/);
});

test("F369: onboarding is ready once this device's key matches the account", async () => {
  let readyCalls = 0;
  commands = { ...signedInAccount };
  await render(
    React.createElement(HostedCommunityOnboarding, {
      onBack: () => {},
      onReady: () => {
        readyCalls += 1;
      },
    }),
    { identity: { pubkey: KEY_A } },
  );

  assert.equal(readyCalls, 1);
  assert.equal(present("hosted-community-create-surface"), true);
});

test("F370: the switch is not offered when the account's bound key is unreadable", async () => {
  commands = {
    ...signedInAccount,
    get_builderlab_nostr_identity: async () => ({
      identity: { pubkey_hex: "npub1storedinthehexfield" },
    }),
  };
  await render(
    React.createElement(HostedCommunityOnboarding, { onBack: () => {} }),
    { identity: { pubkey: KEY_B } },
  );

  assert.equal(button("Use this device's identity").disabled, true);
});

test("F367: the create flow shows no create form while this device's key is unknown", async () => {
  commands = { ...signedInAccount, get_identity: never };
  await render(
    React.createElement(HostedCommunityCreateFlow, { onComplete: () => {} }),
  );

  assert.equal(present("hosted-community-create-name"), false);
  assert.match(bodyText(), /Checking this device’s identity/);
});

test("F367: the create flow shows the form once this device's key matches", async () => {
  commands = { ...signedInAccount };
  await render(
    React.createElement(HostedCommunityCreateFlow, { onComplete: () => {} }),
    { identity: { pubkey: KEY_A } },
  );

  assert.equal(present("hosted-community-create-name"), true);
});

test("F368: a device key owned by another account never deletes the binding", async () => {
  commands = {
    ...signedInAccount,
    bind_builderlab_nostr_identity: async () => ({
      error: { code: "pubkey_already_bound" },
    }),
    delete_builderlab_nostr_identity: async () => ({}),
  };
  await render(
    React.createElement(HostedCommunityCreateFlow, { onComplete: () => {} }),
    { identity: { pubkey: KEY_B } },
  );

  await click(button("Use this device"));

  assert.ok(invoked.includes("bind_builderlab_nostr_identity"));
  assert.ok(!invoked.includes("delete_builderlab_nostr_identity"));
  assert.match(bodyText(), /Nothing was changed/);
});

test("F368 review: an unbound account is reported even when the refresh fails", async () => {
  let identityLoads = 0;
  let binds = 0;
  commands = {
    ...signedInAccount,
    get_builderlab_nostr_identity: async () => {
      identityLoads += 1;
      if (identityLoads > 1) throw new Error("refresh failed");
      return { identity: { pubkey_hex: KEY_A } };
    },
    bind_builderlab_nostr_identity: async () => {
      binds += 1;
      return binds === 1
        ? { error: { code: "identity_already_bound" } }
        : { error: { code: "pubkey_already_bound" } };
    },
    delete_builderlab_nostr_identity: async () => ({}),
  };
  const warn = console.warn;
  console.warn = () => {};
  try {
    await render(
      React.createElement(HostedCommunityCreateFlow, { onComplete: () => {} }),
      { identity: { pubkey: KEY_B } },
    );
    await click(button("Use this device"));
  } finally {
    console.warn = warn;
  }

  assert.ok(invoked.includes("delete_builderlab_nostr_identity"));
  assert.match(bodyText(), /previous Buzz identity was disconnected/);
  // The removed binding is no longer shown; the screen offers to connect.
  assert.match(bodyText(), /Connect and continue/);
});

test("F372: a malformed pubkey renders as unavailable instead of crashing", async () => {
  await render(React.createElement(WelcomeSetup, { initialPage: "join" }), {
    identity: { pubkey: "npub1fromaquirkyextension" },
  });

  const npub = document.querySelector('[data-testid="welcome-join-npub"]');
  assert.equal(npub !== null, true, "the join page rendered");
  assert.equal(npub.textContent, "Unavailable");
  assert.equal(button("Copy public ID").disabled, true);
});

test("F373: a clipboard failure is shown to the user", async () => {
  commands = {
    copy_text_to_clipboard: async () => {
      throw new Error("clipboard denied");
    },
  };
  const warn = console.warn;
  console.warn = () => {};
  try {
    await render(React.createElement(WelcomeSetup, { initialPage: "join" }), {
      identity: { pubkey: KEY_A },
    });
    await click(button("Copy public ID"));
  } finally {
    console.warn = warn;
  }

  assert.ok(invoked.includes("copy_text_to_clipboard"));
  assert.match(bodyText(), /Couldn’t copy your public ID/);
});

test("a failed account load offers a retry, not an identity bind", async () => {
  commands = {
    ...signedInAccount,
    list_builderlab_communities: async () => ({ error: { code: "internal" } }),
  };
  await render(
    React.createElement(HostedCommunityCreateFlow, { onComplete: () => {} }),
    { identity: { pubkey: KEY_A } },
  );
  assert.match(bodyText(), /could not load your Builderlab account/);
  assert.doesNotMatch(bodyText(), /Connect this device/);
  assert.ok(button("Try again"));
});

test("an expired session on account load signs out", async () => {
  commands = {
    ...signedInAccount,
    list_builderlab_communities: async () => ({ error: { code: "unauthorized" } }),
    clear_builderlab_auth: async () => null,
  };
  await render(
    React.createElement(HostedCommunityCreateFlow, { onComplete: () => {} }),
    { identity: { pubkey: KEY_A } },
  );
  assert.match(bodyText(), /Sign in with Builderlab/);
});
