// The browser seam's answers for the native deep-link queue, driven through
// upstream's own drain code (`shared/deep-link.ts`).
//
// A tab has no OS-delivered `buzz://` queue, so every take answers empty and
// every acknowledgement answers false. Before these commands were answered they
// fell to the generic refusal: both drains warned on every navigation, and a
// rejected `clear_pending_navigation_deep_links` made `resetNavigationDeepLinkDrain`
// reject, which `resetCommunityState` turns into "Could not safely switch
// communities" for an in-app community switch.
import assert from "node:assert/strict";
import test from "node:test";

import { handleAppCommand } from "./commands.ts";

const calls = [];
let nextCallbackId = 1;
const tauriInternals = {
  invoke: (cmd, args) => {
    calls.push(cmd);
    // The event plugin is the bridge's, not the seam's: stub the listen half so
    // the drains can mount.
    if (cmd === "plugin:event|listen") return Promise.resolve(nextCallbackId++);
    if (cmd === "plugin:event|unlisten") return Promise.resolve();
    const answer = handleAppCommand(cmd, args);
    if (answer === undefined) {
      return Promise.reject(new Error(`the seam does not answer ${cmd}`));
    }
    return answer;
  },
  transformCallback: () => nextCallbackId++,
};
globalThis.window = {
  __TAURI_INTERNALS__: tauriInternals,
  __TAURI_EVENT_PLUGIN_INTERNALS__: { unregisterListener: () => {} },
};
globalThis.__TAURI_INTERNALS__ = tauriInternals;

const { listenForEntityDeepLinks, listenForNavigationDeepLinks, resetNavigationDeepLinkDrain } =
  await import("@/shared/deep-link.ts");

async function settle() {
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
}

/** Collect console.warn output while `body` runs. */
async function warningsDuring(body) {
  const seen = [];
  const original = console.warn;
  console.warn = (...args) => seen.push(args.map(String).join(" "));
  try {
    await body();
    await settle();
  } finally {
    console.warn = original;
  }
  return seen;
}

test("every deep-link queue command is answered inertly", async () => {
  assert.equal(await handleAppCommand("take_pending_navigation_deep_link", {}), null);
  assert.equal(await handleAppCommand("take_pending_entity_deep_link", {}), null);
  assert.equal(await handleAppCommand("take_pending_community_deep_link", {}), null);
  for (const cmd of [
    "acknowledge_pending_navigation_deep_link",
    "acknowledge_pending_entity_deep_link",
    "acknowledge_pending_community_deep_link",
  ]) {
    assert.equal(await handleAppCommand(cmd, { id: "anything" }), false, cmd);
  }
  assert.equal(await handleAppCommand("clear_pending_navigation_deep_links", {}), null);
});

test("a community switch's drain reset resolves, and the drain stays enabled", async () => {
  // The latent defect: this rejected in the browser build.
  await resetNavigationDeepLinkDrain();
  calls.length = 0;
  const opened = [];
  const warnings = await warningsDuring(async () => {
    const unlisten = await listenForNavigationDeepLinks(
      (payload) => {
        opened.push(payload.channelId);
        return true;
      },
      () => true,
    );
    await settle();
    unlisten();
  });
  // The drain ran (so the reset re-enabled it) and found the queue empty.
  assert.ok(calls.includes("take_pending_navigation_deep_link"), calls.join(","));
  assert.deepEqual(opened, []);
  assert.deepEqual(warnings, []);
});

test("re-mounting both drains, as every navigation does, logs nothing", async () => {
  const warnings = await warningsDuring(async () => {
    for (let i = 0; i < 3; i += 1) {
      const navigation = await listenForNavigationDeepLinks(() => true, () => true);
      const entity = await listenForEntityDeepLinks(() => true);
      await settle();
      navigation();
      entity();
    }
  });
  assert.deepEqual(warnings, []);
});
