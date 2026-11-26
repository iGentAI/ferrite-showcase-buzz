// Run: node --import ./test-loader.mjs --test e2e/scriptRelay.test.mjs
//
// Runs the real hand-run scripts against a local fake relay.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { WebSocketServer } from "ws";

import { isLoopbackHost, relayWsUrl } from "./scriptRelay.mjs";

const WEB_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

test("F325/F327: the socket scheme follows the host, and BUZZ_WS wins outright", () => {
  assert.equal(relayWsUrl({ BUZZ_HOST: "buzz.example.com" }, "x"), "wss://buzz.example.com/buzz/");
  assert.equal(relayWsUrl({ BUZZ_HOST: "buzz.example.com:8443" }, "x"), "wss://buzz.example.com:8443/buzz/");
  assert.equal(relayWsUrl({}, "3emul7w0sqvwfom6.localhost:28443"), "ws://3emul7w0sqvwfom6.localhost:28443/buzz/");
  assert.equal(relayWsUrl({ BUZZ_HOST: "127.0.0.1:4173" }, "x"), "ws://127.0.0.1:4173/buzz/");
  assert.equal(relayWsUrl({ BUZZ_WS: "wss://relay.test/buzz/", BUZZ_HOST: "localhost" }, "x"), "wss://relay.test/buzz/");
  assert.equal(isLoopbackHost("localhost.attacker.tld"), false);
});

/** A fake relay: challenges on connect, then lets `onFrame` script its answers. */
async function fakeRelay(onFrame) {
  const server = new WebSocketServer({ host: "127.0.0.1", port: 0 });
  await new Promise((resolve) => server.once("listening", resolve));
  const frames = [];
  server.on("connection", (socket) => {
    socket.send(JSON.stringify(["AUTH", "challenge-1"]));
    socket.on("message", (raw) => {
      const frame = JSON.parse(String(raw));
      frames.push(frame);
      onFrame(frame, socket);
    });
  });
  const { port } = server.address();
  return { port, frames, close: () => new Promise((resolve) => server.close(resolve)) };
}

/** Run a script to completion (or the deadline) and collect what it said. */
function runScript(script, env, deadlineMs = 8_000) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [script], { cwd: WEB_ROOT, env: { ...process.env, ...env } });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    const timer = setTimeout(() => child.kill("SIGKILL"), deadlineMs);
    child.on("close", (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal, stdout, stderr });
    });
  });
}

test("F327: a missing OK mid-repair stops with a report and exit 1, not an unhandled rejection", async () => {
  const relay = await fakeRelay((frame, socket) => {
    const [verb, event] = frame;
    if (verb === "AUTH") socket.send(JSON.stringify(["OK", event.id, true, ""]));
    // The join is accepted; the topic edit is never answered.
    if (verb === "EVENT" && event.kind === 9021) socket.send(JSON.stringify(["OK", event.id, true, ""]));
  });
  const url = `ws://127.0.0.1:${relay.port}/buzz/`;
  const run = await runScript("e2e/repair-starter-rooms.mjs", {
    BUZZ_WS: url,
    BUZZ_HOST: `127.0.0.1:${relay.port}`,
    BUZZ_PROBE_OK_MS: "300",
  });
  await relay.close();

  assert.equal(run.signal, null, `the script finished on its own\n${run.stdout}\n${run.stderr}`);
  assert.equal(run.code, 1);
  assert.match(run.stdout, /accepted before the failure: general: join/);
  assert.match(run.stdout, /FAIL: the relay sent no OK for EVENT/);
  assert.doesNotMatch(run.stderr, /Unhandled|ERR_UNHANDLED_REJECTION/);
  const auth = relay.frames.find(([verb]) => verb === "AUTH")[1];
  assert.deepEqual(auth.tags.find((tag) => tag[0] === "relay"), ["relay", url], "the AUTH names the URL it connected to");
});

test("F325: a relay that drops the socket ends the probe with a diagnosis and exit 1", async () => {
  const relay = await fakeRelay((frame, socket) => {
    const [verb, event] = frame;
    if (verb === "AUTH") socket.send(JSON.stringify(["OK", event.id, true, ""]));
    if (verb === "REQ") socket.terminate();
  });
  const run = await runScript("e2e/probe-window.mjs", {
    BUZZ_WS: `ws://127.0.0.1:${relay.port}/buzz/`,
    BUZZ_HOST: `127.0.0.1:${relay.port}`,
    BUZZ_PROBE_OK_MS: "300",
  });
  await relay.close();

  assert.equal(run.signal, null, `the probe finished on its own\n${run.stdout}\n${run.stderr}`);
  assert.equal(run.code, 1);
  assert.match(run.stdout, /FAIL: the relay socket closed/);
  assert.doesNotMatch(run.stderr, /Unhandled|ERR_UNHANDLED_REJECTION/);
});

test("F325: an unanswered AUTH is a diagnosis and exit 1", async () => {
  const relay = await fakeRelay(() => {});
  const run = await runScript("e2e/probe-window.mjs", {
    BUZZ_WS: `ws://127.0.0.1:${relay.port}/buzz/`,
    BUZZ_HOST: `127.0.0.1:${relay.port}`,
    BUZZ_PROBE_OK_MS: "300",
  });
  await relay.close();

  assert.equal(run.signal, null, `the probe finished on its own\n${run.stdout}\n${run.stderr}`);
  assert.equal(run.code, 1);
  assert.match(run.stdout, /FAIL: the relay sent no OK for AUTH/);
  assert.doesNotMatch(run.stderr, /Unhandled|ERR_UNHANDLED_REJECTION/);
});

test("review: every served query is closed on the relay after EOSE", async () => {
  const relay = await fakeRelay((frame, socket) => {
    const [verb, a] = frame;
    if (verb === "AUTH") socket.send(JSON.stringify(["OK", a.id, true, ""]));
    if (verb === "REQ") socket.send(JSON.stringify(["EOSE", a]));
  });
  const run = await runScript("e2e/probe-window.mjs", {
    BUZZ_WS: `ws://127.0.0.1:${relay.port}/buzz/`,
    BUZZ_PROBE_OK_MS: "300",
  });
  await relay.close();

  assert.equal(run.code, 0, `${run.stdout}\n${run.stderr}`);
  const opened = relay.frames.filter(([verb]) => verb === "REQ").map(([, id]) => id);
  const closed = relay.frames.filter(([verb]) => verb === "CLOSE").map(([, id]) => id);
  assert.ok(opened.length > 0);
  assert.deepEqual(closed, opened, "one CLOSE per REQ");
});

test("review: a refused subscription (CLOSED) fails the probe instead of reading as an empty answer", async () => {
  const relay = await fakeRelay((frame, socket) => {
    const [verb, a] = frame;
    if (verb === "AUTH") socket.send(JSON.stringify(["OK", a.id, true, ""]));
    if (verb === "REQ") socket.send(JSON.stringify(["CLOSED", a, "restricted: not a member"]));
  });
  const run = await runScript("e2e/probe-window.mjs", {
    BUZZ_WS: `ws://127.0.0.1:${relay.port}/buzz/`,
    BUZZ_PROBE_OK_MS: "300",
  });
  await relay.close();

  assert.equal(run.signal, null, `the probe finished on its own\n${run.stdout}\n${run.stderr}`);
  assert.equal(run.code, 1);
  assert.match(run.stdout, /FAIL: the relay refused query q1: restricted: not a member/);
  assert.equal(relay.frames.filter(([verb]) => verb === "CLOSE").length, 0, "no CLOSE for a subscription the relay already dropped");
});
