// Run: node --import ./test-loader.mjs --test e2e/relayWs.test.mjs
import assert from "node:assert/strict";
import test from "node:test";
import { WebSocketServer } from "ws";

// relayWs.ts reads the socket URL from onboarding.ts at import time.
const server = new WebSocketServer({ host: "127.0.0.1", port: 0 });
await new Promise((resolve) => server.once("listening", resolve));
process.env.BUZZ_WS = `ws://127.0.0.1:${server.address().port}/buzz/`;

let onReq = (socket, id) => socket.send(JSON.stringify(["EOSE", id]));
server.on("connection", (socket) => {
  socket.send(JSON.stringify(["AUTH", "challenge-1"]));
  socket.on("message", (raw) => {
    const [verb, a] = JSON.parse(String(raw));
    if (verb === "AUTH") socket.send(JSON.stringify(["OK", a.id, true, ""]));
    if (verb === "REQ") onReq(socket, a);
  });
});

const { openRelaySocket, FRAME_LIMIT } = await import("./relayWs.ts");
const { identity } = await import("./relayHttp.ts");

const timers = () => process.getActiveResourcesInfo().filter((kind) => kind === "Timeout").length;

test("F326: a finished AUTH and REQ leave no budget timer behind", async () => {
  const before = timers();
  const socket = await openRelaySocket(identity("relayws-timers"), 45_000);
  const outcome = await socket.req({ kinds: [1] }, 30_000);
  assert.equal(outcome.eose, true);
  socket.close();
  assert.equal(timers(), before, "neither the 45 s AUTH budget nor the 30 s REQ budget is still armed");
});

test("F326: close settles a REQ that is still waiting", async () => {
  onReq = () => {};
  const socket = await openRelaySocket(identity("relayws-close"), 45_000);
  const pending = socket.req({ kinds: [1] }, 30_000);
  socket.close();
  let timer;
  const outcome = await Promise.race([
    pending,
    new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error("the REQ outlived close()")), 2_000);
    }),
  ]).finally(() => clearTimeout(timer));
  assert.equal(outcome.eose, false);
  onReq = (s, id) => s.send(JSON.stringify(["EOSE", id]));
});

test("F326: the frame log is bounded", async () => {
  onReq = (s, id) => {
    for (let i = 0; i < FRAME_LIMIT + 100; i += 1) s.send(JSON.stringify(["NOTICE", `n${i}`]));
    s.send(JSON.stringify(["EOSE", id]));
  };
  const socket = await openRelaySocket(identity("relayws-frames"), 45_000);
  await socket.req({ kinds: [1] }, 30_000);
  assert.ok(socket.frames.length <= FRAME_LIMIT, `kept ${socket.frames.length} frames`);
  assert.match(socket.frames.at(-1), /EOSE/, "the newest frames are the ones kept");
  socket.close();
});

test.after(() => new Promise((resolve) => server.close(resolve)));
