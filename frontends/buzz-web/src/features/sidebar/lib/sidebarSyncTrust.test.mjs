// Regression tests for the trust boundary of the sidebar-preference sync
// managers (security review findings F159, F160, F161): a relay hands back
// `pubkey` and `created_at` as plain JSON, so nothing it says may move a
// watermark, win last-writer-wins or unlock seed-publishing unless the event
// verifies, and no timestamp may be persisted that makes publishing
// impossible.

import assert from "node:assert/strict";
import test, { mock } from "node:test";

import { relayClient } from "@/shared/api/relayClient";
import { ChannelMuteSyncManager } from "./channelMutesSync.ts";
import { ChannelSectionSyncManager } from "./channelSectionsSync.ts";
import { ChannelSortSyncManager } from "./channelSortSync.ts";
import { ChannelStarSyncManager } from "./channelStarsSync.ts";
import {
  isAuthenticOwnSyncEvent,
  MAX_SYNC_CLOCK_SKEW_SECS,
  syncPublishCreatedAt,
  syncPublishRetryDelayMs,
} from "./sidebarSyncEventTrust.ts";
import { advanceWatermark, readWatermark } from "./sidebarSyncWatermark.ts";
import { makeSyncSigner } from "./sidebarSyncSignedEvents.mjs";
import {
  installFakeWindow,
  installTauriMock,
  makeFakeWindow,
} from "./sidebarSyncTestHelpers.mjs";

const RELAY = "wss://r.test";
const RELAY_KEY = encodeURIComponent(RELAY);
const FAR_FUTURE = 9_999_999_999_999;
const now = () => Math.floor(Date.now() / 1_000);

const MANAGERS = [
  {
    name: "sections",
    blob: "channel-sections",
    make: (pubkey) => new ChannelSectionSyncManager(pubkey, RELAY),
    fetch: (m) => m.fetchRemoteSections(),
    subscribe: (m, cb) => m.subscribeToSections(cb),
    payload: { version: 1, sections: [], assignments: {} },
  },
  {
    name: "mutes",
    blob: "channel-mutes",
    make: (pubkey) => new ChannelMuteSyncManager(pubkey, RELAY),
    fetch: (m) => m.fetchRemoteMutes(),
    subscribe: (m, cb) => m.subscribeToMutes(cb),
    payload: { version: 1, channels: {} },
  },
  {
    name: "stars",
    blob: "channel-stars",
    make: (pubkey) => new ChannelStarSyncManager(pubkey, RELAY),
    fetch: (m) => m.fetchRemoteStars(),
    subscribe: (m, cb) => m.subscribeToStars(cb),
    payload: { version: 1, channels: {} },
  },
  {
    name: "sort",
    blob: "channel-sort",
    make: (pubkey) => new ChannelSortSyncManager(pubkey, RELAY),
    fetch: (m) => m.fetchRemoteSortPrefs(),
    subscribe: (m, cb) => m.subscribeToSortPrefs(cb),
    payload: { version: 1, groups: {} },
  },
];

function watermark(fw, blob, pubkey) {
  return fw.localStorage.getItem(
    `buzz-sync-watermark.v1:${blob}:${pubkey}:${RELAY_KEY}`,
  );
}

for (const surface of MANAGERS) {
  test(`${surface.name}: a relay-tampered created_at is refused and never persisted`, async () => {
    const signer = makeSyncSigner(surface.blob);
    const genuine = signer.sign({ content: "good-cipher", created_at: now() });
    // The relay keeps the user's real signature but rewrites the timestamp.
    const tampered = { ...genuine, created_at: FAR_FUTURE };
    mock.method(relayClient, "fetchEvents", () => Promise.resolve([tampered]));
    const fw = makeFakeWindow();
    const restore = installFakeWindow(fw);
    const tauri = installTauriMock(JSON.stringify(surface.payload));
    try {
      const manager = surface.make(signer.pubkey);
      const result = await surface.fetch(manager);
      assert.equal(result.status, "failed");
      assert.equal(result.createdAt, undefined);
      assert.equal(watermark(fw, surface.blob, signer.pubkey), null);
    } finally {
      tauri.restore();
      restore();
      mock.reset();
    }
  });

  test(`${surface.name}: an event from another author is a failure, not absence`, async () => {
    const signer = makeSyncSigner(surface.blob);
    const other = makeSyncSigner(surface.blob);
    mock.method(relayClient, "fetchEvents", () =>
      Promise.resolve([
        other.sign({ content: "good-cipher", created_at: now() }),
      ]),
    );
    const fw = makeFakeWindow();
    const restore = installFakeWindow(fw);
    try {
      const manager = surface.make(signer.pubkey);
      const result = await surface.fetch(manager);
      // `absent` would send a fresh profile down the seed-publish path.
      assert.equal(result.status, "failed");
      assert.equal(watermark(fw, surface.blob, signer.pubkey), null);
    } finally {
      restore();
      mock.reset();
    }
  });

  test(`${surface.name}: a forged live event neither advances the watermark nor reaches the hook`, async () => {
    const signer = makeSyncSigner(surface.blob);
    let liveCallback = null;
    mock.method(relayClient, "subscribeLive", (_filter, onEvent) => {
      liveCallback = onEvent;
      return Promise.resolve(async () => {});
    });
    const fw = makeFakeWindow();
    const restore = installFakeWindow(fw);
    const tauri = installTauriMock(JSON.stringify(surface.payload));
    try {
      const manager = surface.make(signer.pubkey);
      const updates = [];
      await surface.subscribe(manager, (remote) => updates.push(remote));
      const genuine = signer.sign({ content: "good-cipher", created_at: now() });
      liveCallback({ ...genuine, sig: "0".repeat(128) });
      liveCallback({ ...genuine, created_at: FAR_FUTURE });
      await new Promise((r) => setTimeout(r, 10));
      assert.equal(watermark(fw, surface.blob, signer.pubkey), null);
      assert.equal(updates.length, 0);

      // The genuine event still goes through.
      liveCallback(genuine);
      await new Promise((r) => setTimeout(r, 10));
      assert.equal(
        Number(watermark(fw, surface.blob, signer.pubkey)),
        genuine.created_at,
      );
      assert.equal(updates.length, 1);
    } finally {
      tauri.restore();
      restore();
      mock.reset();
    }
  });

  test(`${surface.name}: the user's genuine event for another blob is refused`, async () => {
    const signer = makeSyncSigner(surface.blob);
    const t = now();
    const wrongNamespace = [
      // Same kind, another sidebar blob's d tag.
      signer.sign({
        content: "good-cipher",
        created_at: t,
        tags: [["d", "channel-other"]],
      }),
      // No d tag at all.
      signer.sign({ content: "good-cipher", created_at: t, tags: [] }),
      // Right d tag, another kind.
      signer.sign({ kind: 1, content: "good-cipher", created_at: t }),
    ];
    let liveCallback = null;
    const fw = makeFakeWindow();
    const restore = installFakeWindow(fw);
    const tauri = installTauriMock(JSON.stringify(surface.payload));
    try {
      for (const event of wrongNamespace) {
        mock.method(relayClient, "fetchEvents", () => Promise.resolve([event]));
        const result = await surface.fetch(surface.make(signer.pubkey));
        assert.equal(result.status, "failed");
        mock.reset();
      }
      assert.equal(watermark(fw, surface.blob, signer.pubkey), null);

      mock.method(relayClient, "subscribeLive", (_filter, onEvent) => {
        liveCallback = onEvent;
        return Promise.resolve(async () => {});
      });
      const updates = [];
      await surface.subscribe(surface.make(signer.pubkey), (remote) =>
        updates.push(remote),
      );
      for (const event of wrongNamespace) liveCallback(event);
      await new Promise((r) => setTimeout(r, 10));
      assert.equal(updates.length, 0);
      assert.equal(watermark(fw, surface.blob, signer.pubkey), null);
    } finally {
      tauri.restore();
      restore();
      mock.reset();
    }
  });
}

test("a genuinely signed event stamped implausibly far ahead is refused", () => {
  const address = { kind: 30078, dTag: "channel-sections" };
  const signer = makeSyncSigner(address.dTag);
  const skewed = signer.sign({
    content: "x",
    created_at: now() + MAX_SYNC_CLOCK_SKEW_SECS + 60,
  });
  assert.equal(isAuthenticOwnSyncEvent(skewed, signer.pubkey, address), false);
  const fine = signer.sign({ content: "x", created_at: now() + 30 });
  assert.equal(isAuthenticOwnSyncEvent(fine, signer.pubkey, address), true);
});

test("advanceWatermark refuses an implausible timestamp and replaces a poisoned one", () => {
  const fw = makeFakeWindow();
  const restore = installFakeWindow(fw);
  try {
    advanceWatermark("pk", "channel-sections", RELAY, FAR_FUTURE);
    assert.equal(readWatermark("pk", "channel-sections", RELAY), 0);
    advanceWatermark("pk", "channel-sections", RELAY, Number.NaN);
    advanceWatermark("pk", "channel-sections", RELAY, -5);
    assert.equal(readWatermark("pk", "channel-sections", RELAY), 0);

    // A value persisted before validation existed.
    fw.localStorage.setItem(
      `buzz-sync-watermark.v1:channel-sections:pk:${RELAY_KEY}`,
      String(FAR_FUTURE),
    );
    const capped = readWatermark("pk", "channel-sections", RELAY);
    assert.ok(capped > 0, "a seen head still blocks seed-publishing");
    assert.ok(capped <= now() + MAX_SYNC_CLOCK_SKEW_SECS);

    const genuine = now();
    advanceWatermark("pk", "channel-sections", RELAY, genuine);
    assert.equal(
      fw.localStorage.getItem(
        `buzz-sync-watermark.v1:channel-sections:pk:${RELAY_KEY}`,
      ),
      String(genuine),
    );
  } finally {
    restore();
  }
});

test("syncPublishCreatedAt is strictly newer than the head or defers", () => {
  const t = 1_700_000_000;
  assert.equal(syncPublishCreatedAt(0, t), t);
  assert.equal(syncPublishCreatedAt(t + 5, t), t + 6);
  assert.equal(
    syncPublishCreatedAt(t + MAX_SYNC_CLOCK_SKEW_SECS - 1, t),
    t + MAX_SYNC_CLOCK_SKEW_SECS,
  );
  // At the bound no newer timestamp fits: never reuse the head's.
  assert.equal(syncPublishCreatedAt(t + MAX_SYNC_CLOCK_SKEW_SECS, t), null);
  assert.equal(syncPublishCreatedAt(FAR_FUTURE, t), null);
  assert.equal(syncPublishRetryDelayMs(t + MAX_SYNC_CLOCK_SKEW_SECS, t), 1_000);
});

async function publishHarness(signer) {
  const signed = [];
  mock.method(relayClient, "fetchEvents", () => Promise.resolve([]));
  mock.method(relayClient, "publishEvent", () => Promise.resolve());
  const fw = makeFakeWindow();
  const restore = installFakeWindow(fw);
  const tauri = installTauriMock("{}");
  const originalInvoke = globalThis.window.__TAURI_INTERNALS__.invoke;
  globalThis.window.__TAURI_INTERNALS__.invoke = (cmd, args) => {
    if (cmd === "sign_event") signed.push(args?.createdAt);
    return originalInvoke(cmd, args);
  };
  const realNow = Date.now;
  let offsetMs = 0;
  Date.now = () => realNow() + offsetMs;
  return {
    fw,
    signed,
    advance: (ms) => {
      offsetMs += ms;
    },
    settle: () => new Promise((r) => setTimeout(r, 20)),
    done: () => {
      Date.now = realNow;
      tauri.restore();
      restore();
      mock.reset();
    },
  };
}

test("a poisoned stored watermark no longer pushes publishes out of the relay's window", async () => {
  const signer = makeSyncSigner("channel-sections");
  const h = await publishHarness(signer);
  try {
    h.fw.localStorage.setItem(
      `buzz-sync-watermark.v1:channel-sections:${signer.pubkey}:${RELAY_KEY}`,
      String(FAR_FUTURE),
    );
    const manager = new ChannelSectionSyncManager(signer.pubkey, RELAY);
    const head = readWatermark(signer.pubkey, "channel-sections", RELAY);
    manager.publishSections({
      version: 1,
      sections: [{ id: "s1", name: "Work", order: 0 }],
      assignments: {},
    });
    h.fw._fireTimer();
    await h.settle();
    // The capped head sits at the bound, so the publish waits for a second
    // rather than reuse the head's timestamp.
    assert.equal(h.signed.length, 0);
    assert.ok(h.fw._hasTimer(), "the publish is retried, not dropped");
    h.advance(1_000);
    h.fw._fireTimer();
    await h.settle();
    assert.equal(h.signed.length, 1);
    assert.ok(h.signed[0] > head, "strictly newer than the head");
    assert.ok(h.signed[0] <= now() + 1 + MAX_SYNC_CLOCK_SKEW_SECS);
  } finally {
    h.done();
  }
});

test("consecutive publishes at the skew bound get strictly increasing timestamps", async () => {
  const signer = makeSyncSigner("channel-mutes");
  const h = await publishHarness(signer);
  try {
    h.fw.localStorage.setItem(
      `buzz-sync-watermark.v1:channel-mutes:${signer.pubkey}:${RELAY_KEY}`,
      String(FAR_FUTURE),
    );
    const manager = new ChannelMuteSyncManager(signer.pubkey, RELAY);
    for (let edit = 0; edit < 3; edit += 1) {
      manager.publishMutes({
        version: 1,
        channels: { [`c${edit}`]: { muted: true, updatedAt: now() } },
      });
      h.fw._fireTimer();
      await h.settle();
      for (let tries = 0; h.signed.length === edit && tries < 5; tries += 1) {
        h.advance(1_000);
        h.fw._fireTimer();
        await h.settle();
      }
    }
    assert.equal(h.signed.length, 3);
    assert.ok(h.signed[0] < h.signed[1] && h.signed[1] < h.signed[2]);
  } finally {
    h.done();
  }
});
