/**
 * Regression tests for F147 (relay URLs with userinfo or fragments were
 * accepted) and F365 (probeRelayReachable().cancel() never settled).
 */
import assert from "node:assert/strict";
import test from "node:test";

import { normalizeRelayUrl, probeRelayReachable } from "./relayProbe.ts";

test("F147: userinfo is refused for every accepted input form", () => {
  for (const input of [
    "https://relay.trusted.example@evil.example/",
    "wss://relay.trusted.example@evil.example",
    "ws://user:pass@evil.example",
    "http://user@evil.example",
    "https://:secret@evil.example",
    "relay.trusted.example@evil.example",
    "wss://@evil.example",
  ]) {
    assert.equal(normalizeRelayUrl(input), null, input);
  }
});

test("F147: fragments are refused", () => {
  for (const input of [
    "wss://relay.example.com#frag",
    "https://relay.example.com/#x",
    "wss://relay.example.com#",
    "relay.example.com#x",
  ]) {
    assert.equal(normalizeRelayUrl(input), null, input);
  }
});

test("F147: ordinary relay URLs, ports, paths and queries still normalize", () => {
  assert.equal(
    normalizeRelayUrl("https://relay.example.com:8443"),
    "wss://relay.example.com:8443",
  );
  assert.equal(
    normalizeRelayUrl("wss://relay.example.com/custom?room=a"),
    "wss://relay.example.com/custom?room=a",
  );
  // An @ in the path is not userinfo.
  assert.equal(
    normalizeRelayUrl("wss://relay.example.com/u@team"),
    "wss://relay.example.com/u@team",
  );
  assert.equal(normalizeRelayUrl("relay.example.com"), "wss://relay.example.com");
});

class FakeWebSocket {
  static instances = [];
  constructor(url) {
    this.url = url;
    this.closeCalls = 0;
    FakeWebSocket.instances.push(this);
  }
  close() {
    this.closeCalls += 1;
    this.onclose?.();
  }
}

async function settledWithin(promise, ms) {
  let timer;
  const pending = new Promise((resolve) => {
    timer = setTimeout(() => resolve("still pending"), ms);
  });
  try {
    return await Promise.race([promise, pending]);
  } finally {
    clearTimeout(timer);
  }
}

test("F365: cancel resolves an in-flight probe with false and closes the socket", async () => {
  const original = globalThis.WebSocket;
  FakeWebSocket.instances = [];
  globalThis.WebSocket = FakeWebSocket;
  try {
    const probe = probeRelayReachable("wss://relay.example.com", 60_000);
    probe.cancel();
    assert.equal(await settledWithin(probe.promise, 200), false);
    assert.equal(FakeWebSocket.instances[0].closeCalls, 1);
    // A second cancel is harmless.
    probe.cancel();
    assert.equal(FakeWebSocket.instances[0].closeCalls, 1);
  } finally {
    globalThis.WebSocket = original;
  }
});

test("F365: cancel after the probe settled keeps the original result", async () => {
  const original = globalThis.WebSocket;
  FakeWebSocket.instances = [];
  globalThis.WebSocket = FakeWebSocket;
  try {
    const probe = probeRelayReachable("wss://relay.example.com", 60_000);
    FakeWebSocket.instances[0].onopen();
    probe.cancel();
    assert.equal(await settledWithin(probe.promise, 200), true);
  } finally {
    globalThis.WebSocket = original;
  }
});

test("finding_b24a79d54b005219: parser-differential relay URLs are refused and accepted ones are canonical", () => {
  for (const hostile of [
    "wss:///@evil.example",
    "https:///@evil.example",
    "/@evil.example",
    "wss://evil.example\\@relay.trusted.example",
    "wss://relay.example/\u0000x",
    "wss://relay.example/a b",
  ]) {
    assert.equal(normalizeRelayUrl(hostile), null, hostile);
  }
  assert.equal(normalizeRelayUrl("WSS://Relay.Example/buzz/"), "wss://relay.example/buzz");
  assert.equal(normalizeRelayUrl("https://relay.example"), "wss://relay.example");
});
