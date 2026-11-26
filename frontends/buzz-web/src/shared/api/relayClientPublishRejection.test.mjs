// A relay rejection addressed to one event must settle that event's pending
// publish *and* arm the rate-limit gate.
//
// History: the relay rejected an over-quota EVENT with a bare
// `["NOTICE", "rate-limited: ..."]`. A NOTICE carries no event id, and
// `pendingEvents` is keyed by event id, so nothing settled — the publish sat
// until PUBLISH_TIMEOUT_MS (25s) and surfaced as a message stuck on
// "Sending…". Startup quota exhaustion made that routine in the first seconds
// after launch. The relay now rejects on the OK channel instead, so the gate
// arming that used to live in the NOTICE branch has to happen here too.
import assert from "node:assert/strict";
import test from "node:test";
import { registerHooks } from "node:module";

// Match the browser build's default Vite environment while exercising real
// URL construction, signing and fetch admission under Node.
registerHooks({
  load(url, context, nextLoad) {
    const loaded = nextLoad(url, context);
    if (url.endsWith("/shared/lib/relay-url.ts") || url.endsWith("/shared/api/relayClientSession.ts")) {
      const viteEnv = process.env.BUZZ_TEST_HTTP_ONLY === "1"
        ? { VITE_RELAY_TRANSPORT: "http" } : {};
      return { ...loaded, source: String(loaded.source).replaceAll("import.meta.env", `(${JSON.stringify(viteEnv)})`) };
    }
    return loaded;
  },
});

const fakeNow = 0;
const pendingTimers = new Map();
let nextTimerId = 1;
const sendAttempts = [];
const deliveredFrames = [];
let invokeError = null;
let signTransport = async (args) => JSON.stringify({ id: "auth", ...args });
const httpRequests = [];
globalThis.fetch = async (url, options) => {
  httpRequests.push({ url, options });
  return new Response("{}", { status: 200 });
};
let sendTransport = async (args) => {
  deliveredFrames.push(args);
};

globalThis.window = {
  location: { origin: "https://buzz.test" },
  setTimeout: (fn, ms) => {
    const id = nextTimerId++;
    pendingTimers.set(id, { fn, fireAt: fakeNow + ms });
    return id;
  },
  clearTimeout: (id) => pendingTimers.delete(id),
  __TAURI_INTERNALS__: {
    invoke: async (command, args) => {
      if (command === "sign_event") return signTransport(args);
      // The relay the socket uses; the HTTP fallback posts to it.
      if (command === "get_relay_http_url") return "https://buzz.test/buzz/api";
      if (command === "get_channels" && invokeError) throw invokeError;
      if (command === "plugin:websocket|send") {
        sendAttempts.push(args);
        return sendTransport(args);
      }
    },
  },
};
Date.now = () => fakeNow;

const { RelayClient } = await import("./relayClientSession.ts");
const { invokeTauri } = await import("./tauri.ts");
const { activateRateLimit, isRateLimited, resetRateLimitGate } = await import(
  "./relayRateLimitGate.ts"
);

function reset() {
  resetRateLimitGate();
  invokeError = null;
  httpRequests.length = 0;
  signTransport = async (args) => JSON.stringify({ id: "auth", ...args });
  pendingTimers.clear();
  nextTimerId = 1;
  sendAttempts.length = 0;
  deliveredFrames.length = 0;
  sendTransport = async (args) => {
    deliveredFrames.push(args);
  };
}

function connectedClient() {
  const client = new RelayClient();
  client.wsId = 7;
  return client;
}

function eventFrames() {
  return deliveredFrames.filter(
    ({ message }) => JSON.parse(message.data)[0] === "EVENT",
  );
}

async function flushUntil(predicate, attempts = 20) {
  for (let attempt = 0; attempt < attempts; attempt++) {
    if (predicate()) return;
    await Promise.resolve();
  }
  assert.fail("condition did not become true before the microtask limit");
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((promiseResolve, promiseReject) => {
    resolve = promiseResolve;
    reject = promiseReject;
  });
  return { promise, resolve, reject };
}

/**
 * Registers a pending publish the way `publishEvent` does, without needing a
 * socket: the OK dispatch under test only reads `pendingEvents`.
 */
function armPendingPublish(client, eventId) {
  const event = { id: eventId };
  const settled = new Promise((resolve, reject) => {
    client.pendingEvents.set(eventId, {
      event,
      resolve,
      reject,
      timeout: window.setTimeout(() => {}, 25_000),
    });
  });
  // Keep the rejection from surfacing as an unhandled rejection.
  return settled.then(
    (value) => ({ status: "resolved", value }),
    (error) => ({ status: "rejected", error }),
  );
}

/** Feeds a raw relay frame through the real inbound dispatch path. */
function deliver(client, frame) {
  return client.handleWsMessage(
    { type: "Text", data: JSON.stringify(frame) },
    client.connectionGeneration,
  );
}

test("a rate-limited OK rejection settles the pending publish", async () => {
  resetRateLimitGate();
  pendingTimers.clear();
  const client = new RelayClient();
  const eventId = "a".repeat(64);
  const settled = armPendingPublish(client, eventId);

  await deliver(client, [
    "OK",
    eventId,
    false,
    "rate-limited: quota exceeded; retry in 4s",
  ]);

  const outcome = await settled;
  assert.equal(
    outcome.status,
    "rejected",
    "an over-quota publish must fail fast, not hang until the 25s publish timeout",
  );
  assert.match(outcome.error.message, /rate-limited/);
  assert.equal(
    client.pendingEvents.has(eventId),
    false,
    "the pending entry must be cleared",
  );
});

test("a rate-limited OK rejection arms the rate-limit gate", async () => {
  resetRateLimitGate();
  pendingTimers.clear();
  const client = new RelayClient();
  const eventId = "b".repeat(64);
  const settled = armPendingPublish(client, eventId);

  assert.equal(isRateLimited(), false, "gate starts closed");

  await deliver(client, [
    "OK",
    eventId,
    false,
    "rate-limited: quota exceeded; retry in 4s",
  ]);
  await settled;

  assert.equal(
    isRateLimited(),
    true,
    "back-pressure now arrives on the OK channel — without arming here the " +
      "client fails the send and immediately retries into the same quota",
  );
});

test("an ordinary OK rejection does not arm the gate", async () => {
  resetRateLimitGate();
  pendingTimers.clear();
  const client = new RelayClient();
  const eventId = "c".repeat(64);
  const settled = armPendingPublish(client, eventId);

  await deliver(client, ["OK", eventId, false, "invalid: bad signature"]);
  const outcome = await settled;

  assert.equal(outcome.status, "rejected");
  assert.equal(
    isRateLimited(),
    false,
    "only `rate-limited:` rejections signal back-pressure",
  );
});

test("an accepted OK still resolves the pending publish", async () => {
  reset();
  const client = new RelayClient();
  const eventId = "d".repeat(64);
  const settled = armPendingPublish(client, eventId);

  await deliver(client, ["OK", eventId, true, ""]);
  const outcome = await settled;

  assert.equal(outcome.status, "resolved");
  assert.equal(outcome.value.id, eventId);
});

test("a publish started during an ordinary outage reconnects once and settles", async () => {
  reset();
  const client = new RelayClient();
  const event = { id: "0".repeat(64), kind: 1 };
  let reconnects = 0;
  client.ensureConnected = async () => {
    reconnects++;
    client.connectionGeneration++;
    client.wsId = 8;
    return client.connectionGeneration;
  };

  const published = client.publishEvent(event, "timed out", "send failed");
  await flushUntil(() => eventFrames().length === 1);

  assert.equal(reconnects, 1);
  assert.equal(sendAttempts.length, 1);
  assert.equal(client.pendingEvents.has(event.id), true);

  await deliver(client, ["OK", event.id, true, ""]);
  assert.equal(await published, event);
  assert.equal(client.pendingEvents.size, 0);
});

test("a community switch while gated cannot publish through its replacement socket", async () => {
  reset();
  activateRateLimit(4);
  const client = connectedClient();
  const event = { id: "e".repeat(64), kind: 1 };

  const published = client.publishEvent(event, "timed out", "send failed");
  await Promise.resolve();
  assert.equal(client.pendingEvents.size, 0);

  client.disconnect();
  resetRateLimitGate();
  client.wsId = 8;

  await assert.rejects(published, /community switch/);
  assert.equal(client.pendingEvents.size, 0);
  assert.equal(eventFrames().length, 0);
  assert.equal(httpRequests.length, 0, "cancelled publish cannot fall back to HTTP");
});

test("a community switch after send failure cannot retry through its replacement socket", async () => {
  reset();
  const client = connectedClient();
  const event = { id: "f".repeat(64), kind: 1 };
  const reconnect = deferred();
  let connectionAttempts = 0;
  client.ensureConnected = async () => {
    if (++connectionAttempts > 1) await reconnect.promise;
    return client.connectionGeneration;
  };
  sendTransport = async () => {
    throw new Error("old socket failed");
  };

  const published = client.publishEvent(event, "timed out", "send failed");
  const outcome = published.then(
    () => ({ status: "resolved" }),
    (error) => ({ status: "rejected", error }),
  );
  await flushUntil(() => client.connectionGeneration === 1);
  assert.equal(sendAttempts.length, 1);
  assert.equal(eventFrames().length, 0);
  assert.equal(
    client.connectionGeneration,
    1,
    "the failed send reset its socket",
  );
  assert.equal(
    client.pendingEvents.has(event.id),
    true,
    "the original publish remains owned while reconnect is pending",
  );

  client.disconnect();
  client.wsId = 8;
  const settledBeforeReconnect = await outcome;
  assert.equal(
    settledBeforeReconnect.status,
    "rejected",
    "community switch must settle the publish without waiting for reconnect",
  );
  assert.match(settledBeforeReconnect.error.message, /community switch/);

  reconnect.resolve();
  await published.catch(() => {});
  await Promise.resolve();
  assert.equal(client.pendingEvents.size, 0);
  assert.equal(
    sendAttempts.length,
    1,
    "the replacement socket must not be used",
  );
  assert.equal(eventFrames().length, 0);
});

// Drive the actual invoke rejection and publisher, not a classifier helper.
// Restoring HTTP -> WS gate propagation must fail before any timer advances.
for (const message of [
  "relay rate-limited: retry in 50s",
  "relay rate-limited: quota exceeded",
  "relay rate-limited: retry in 1000000s",
]) {
  test(`HTTP backoff does not withhold a WS publish: ${message}`, async () => {
    reset();
    invokeError = message;
    await assert.rejects(invokeTauri("get_channels"), { message });
    const client = connectedClient();
    const event = { id: "9".repeat(64), kind: 9 };
    const published = client.publishEvent(event, "timed out", "send failed");
    try {
      await flushUntil(() => eventFrames().length === 1);
      assert.equal(isRateLimited(), false);
      await deliver(client, ["OK", event.id, true, ""]);
      assert.equal(await published, event);
      assert.equal(client.pendingEvents.size, 0);
    } finally {
      // Also drain safely when the pre-fix gate coupling is restored.
      resetRateLimitGate();
      await flushUntil(() => eventFrames().length === 1);
      await deliver(client, ["OK", event.id, true, ""]);
      await published;
    }
  });
}

test("HTTP failure does not clear an existing WS backoff", async () => {
  reset();
  const client = connectedClient();
  await deliver(client, [
    "NOTICE",
    "rate-limited: quota exceeded; retry in 4s",
  ]);
  invokeError = "relay rate-limited: retry in 50s";
  await assert.rejects(invokeTauri("get_channels"), { message: invokeError });
  const event = { id: "8".repeat(64), kind: 9 };
  const published = client.publishEvent(event, "timed out", "send failed");
  await Promise.resolve();
  assert.equal(eventFrames().length, 0);
  assert.equal(isRateLimited(), true);
  assert.ok([...pendingTimers.values()].some(({ fireAt }) => fireAt === 4_000));
  assert.ok(
    ![...pendingTimers.values()].some(({ fireAt }) => fireAt === 50_000),
  );
  resetRateLimitGate();
  await flushUntil(() => eventFrames().length === 1);
  await deliver(client, ["OK", event.id, true, ""]);
  assert.equal(await published, event);
});


test("connection failure falls back to HTTP once for the same signed event", async () => {
  reset();
  const client = connectedClient();
  client.ensureConnected = async () => { throw new Error("ordinary outage"); };
  const event = { id: "a".repeat(64), kind: 9, content: "unchanged" };
  assert.equal(await client.publishEvent(event, "timed out", "send failed"), event);
  assert.equal(httpRequests.length, 1);
  assert.equal(httpRequests[0].url, "https://buzz.test/buzz/api/events");
  assert.deepEqual(JSON.parse(httpRequests[0].options.body), event);
});

test("community switch during initial connection never signs or sends HTTP", async () => {
  reset();
  const client = connectedClient();
  const connection = deferred();
  client.ensureConnected = () => connection.promise;
  let signatures = 0;
  signTransport = async () => { signatures++; return "{}"; };
  const published = client.publishEvent({ id: "b".repeat(64), kind: 9 }, "timed out", "send failed");
  client.disconnect();
  connection.reject(new Error("old connection failed"));
  await assert.rejects(published, /community switch/);
  assert.equal(signatures, 0);
  assert.equal(httpRequests.length, 0);
});

test("community switch during HTTP signing never sends the event", async () => {
  reset();
  const client = connectedClient();
  client.ensureConnected = async () => { throw new Error("ordinary outage"); };
  const signing = deferred();
  let started = false;
  signTransport = async () => { started = true; return signing.promise; };
  const published = client.publishEvent({ id: "c".repeat(64), kind: 9 }, "timed out", "send failed");
  for (let i = 0; i < 100 && !started; i++) await new Promise((r) => setImmediate(r));
  assert.equal(started, true, "actual fallback reached signing");
  client.disconnect();
  signing.resolve("{}");
  await assert.rejects(published, /community switch/);
  assert.equal(httpRequests.length, 0);
});

test("community switch during HTTP digest never signs for the replacement identity", async () => {
  reset();
  const client = connectedClient();
  client.ensureConnected = async () => { throw new Error("ordinary outage"); };
  const digest = deferred();
  const originalDigest = crypto.subtle.digest;
  let started = false;
  let signatures = 0;
  crypto.subtle.digest = async () => { started = true; return digest.promise; };
  signTransport = async () => { signatures++; return "{}"; };
  try {
    const published = client.publishEvent({ id: "d".repeat(64), kind: 9 }, "timed out", "send failed");
    await flushUntil(() => started);
    client.disconnect();
    digest.resolve(new ArrayBuffer(32));
    await assert.rejects(published, /community switch/);
    assert.equal(signatures, 0);
    assert.equal(httpRequests.length, 0);
  } finally {
    crypto.subtle.digest = originalDigest;
  }
});
