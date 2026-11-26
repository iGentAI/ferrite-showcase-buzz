import assert from "node:assert/strict";
import test from "node:test";

import {
  consumePendingWelcomeChannel,
  rememberPendingWelcomeChannel,
} from "./welcome.ts";
import { enterWelcomeChannelDirectly } from "./welcomeDirectEntry.ts";

function installWindow() {
  const storage = new Map();
  const previousWindow = globalThis.window;
  const store = {
    getItem: (key) => storage.get(key) ?? null,
    setItem: (key, value) => storage.set(key, String(value)),
    removeItem: (key) => storage.delete(key),
  };
  globalThis.window = {
    sessionStorage: store,
    localStorage: store,
    location: { hash: "" },
  };
  return {
    restore() {
      if (previousWindow === undefined) delete globalThis.window;
      else globalThis.window = previousWindow;
    },
  };
}

test("direct entry routes to Welcome and leaves no pending entry for Home to replay", () => {
  const { restore } = installWindow();
  try {
    rememberPendingWelcomeChannel("welcome-30a1642d");
    enterWelcomeChannelDirectly("welcome-30a1642d");
    assert.equal(window.location.hash, "/channels/welcome-30a1642d");
    // The Home route's fallback: with the entry consumed, a later Inbox visit
    // must not redirect even though the channel is in the list.
    assert.equal(
      consumePendingWelcomeChannel(new Set(["general", "welcome-30a1642d"])),
      null,
    );
  } finally {
    restore();
  }
});

test("without direct entry the pending entry would still redirect Home (the regression)", () => {
  const { restore } = installWindow();
  try {
    rememberPendingWelcomeChannel("welcome-30a1642d");
    window.location.hash = "/channels/welcome-30a1642d";
    assert.equal(
      consumePendingWelcomeChannel(new Set(["general", "welcome-30a1642d"])),
      "welcome-30a1642d",
    );
  } finally {
    restore();
  }
});

test("channel ids are encoded into the route", () => {
  const { restore } = installWindow();
  try {
    enterWelcomeChannelDirectly("welcome room");
    assert.equal(window.location.hash, "/channels/welcome%20room");
  } finally {
    restore();
  }
});
