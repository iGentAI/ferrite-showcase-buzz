/**
 * Browser-port contract for `rewriteRelayUrl` — ours-only; upstream never
 * sees this file.
 *
 * This build has no localhost media proxy: the browser backend answers
 * `get_media_proxy_port` with null, always (shared/browser-backend/commands.ts).
 * Upstream's desktop app falls back to its `buzz-media://` custom protocol
 * here, but only the Tauri shell registers that scheme — in a tab nothing
 * resolves it, so every relay-hosted image would render broken. The port
 * returns the relay URL unchanged instead: this client already talks to the
 * relay directly, so the direct URL is fetchable wherever the relay is.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";

const HASH = "b".repeat(64);

test("rewriteRelayUrl: with no media proxy (the live browser answer) relay URLs pass through unchanged", async () => {
  const previousWindow = globalThis.window;
  globalThis.window = {};
  // Answer exactly as the live browser backend does: a real relay origin,
  // null for the proxy port.
  mockIPC((command) => {
    if (command === "get_media_proxy_port") return null;
    if (command === "get_relay_http_url") return "https://relay.example";
    throw new Error(`Unexpected command: ${command}`);
  });

  let mediaUrl;
  try {
    mediaUrl = await import("./mediaUrl.ts?no-proxy");
    await new Promise((resolve) => setTimeout(resolve, 0));

    const relayMediaUrl = `https://relay.example/media/${HASH}.png`;
    assert.equal(mediaUrl.rewriteRelayUrl(relayMediaUrl), relayMediaUrl);
  } finally {
    // Bump the generation so the module's port poll loop (still retrying the
    // null port) exits at its next tick instead of holding the process for
    // its full 5s budget.
    mediaUrl?.resetMediaCaches();
    clearMocks();
    globalThis.window = previousWindow;
  }
});
