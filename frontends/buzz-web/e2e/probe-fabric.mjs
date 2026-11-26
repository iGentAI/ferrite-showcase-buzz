/**
 * A cheap CLI de-risk for the Fabric-backed persona, run BEFORE spending a
 * browser scenario on it: does `fabrizio` (harness `fabric`) answer an
 * ARITHMETIC ask in-channel?
 *
 * The arithmetic shape matters for the e2e scenario, not for the persona: the
 * answer must not be a substring of the question, or the scenario's assertion
 * could be satisfied by the ask itself instead of by a reply. This script tells
 * us whether fabric is reliable at that shape before scenario 10 depends on it.
 *
 *   node e2e/probe-fabric.mjs
 *
 * Not a Playwright test (`.mjs`, no `.spec.`), so the suite never collects it.
 */
import { finalizeEvent, generateSecretKey, getPublicKey } from "nostr-tools/pure";

const HOST = process.env.BUZZ_HOST || "3emul7w0sqvwfom6.localhost:28443";
const WS = `ws://${HOST}/buzz/`;
const AGENT =
  process.env.AGENT_PK ||
  "9a024c1e6266af6b292759589615f49111a77aa3b49e0dc2def82483e6830e70";
const BUDGET = Number(process.env.BUDGET_MS || 420_000);
const EXPECT = process.env.EXPECT || "3087";

const stamp = Date.now().toString(36);
// A fresh random key: a key derived from a label is recomputable by anyone.
const sk = generateSecretKey();
const me = getPublicKey(sk);
console.log(`probe identity ${me.slice(0, 12)} stamp ${stamp}`);

const ws = new WebSocket(WS);
const okFor = new Map();
let authed = false;
let askId = null;

const sign = (kind, tags, content) =>
  finalizeEvent(
    { kind, created_at: Math.floor(Date.now() / 1000), tags, content },
    sk,
  );

const pub = (ev, verb = "EVENT") =>
  new Promise((res, rej) => {
    okFor.set(ev.id, res);
    ws.send(JSON.stringify([verb, ev]));
    setTimeout(() => rej(new Error("no OK")), 30_000);
  });

ws.onmessage = async (e) => {
  const m = JSON.parse(String(e.data));
  if (m[0] === "OK") {
    const r = okFor.get(m[1]);
    if (r) {
      okFor.delete(m[1]);
      r([m[2], m[3]]);
    }
    return;
  }
  if (m[0] === "CLOSED" || m[0] === "NOTICE") {
    console.log("relay:", String(e.data).slice(0, 200));
    return;
  }
  if (m[0] === "AUTH" && !authed) {
    authed = true;
    console.log(
      "auth:",
      await pub(sign(22242, [["relay", WS], ["challenge", m[1]]], ""), "AUTH"),
    );
    console.log("profile:", await pub(sign(0, [], JSON.stringify({ name: `Probe ${stamp}` }))));
    console.log("join:", await pub(sign(9021, [["h", "general"]], "")));
    // `since` is load-bearing: without it the relay replays this persona's
    // HISTORY and the watcher matches an OLD answer.
    ws.send(
      JSON.stringify([
        "REQ",
        "fab",
        { authors: [AGENT], since: Math.floor(Date.now() / 1000) },
      ]),
    );
    const ask = sign(
      9,
      [
        ["h", "general"],
        ["p", AGENT],
      ],
      `@fabrizio compute 4321 - 1234 for fabric check ${stamp} and reply with just the number`,
    );
    askId = ask.id;
    console.log("ask:", await pub(ask), ask.id.slice(0, 12));
  }
  if (m[0] === "EVENT" && m[2]?.pubkey === AGENT) {
    const threaded = askId && JSON.stringify(m[2].tags).includes(askId);
    console.log(
      `>>> FABRIZIO kind=${m[2].kind} threaded=${threaded}:`,
      JSON.stringify(m[2].content).slice(0, 300),
    );
    if (m[2].kind === 9 && m[2].content.includes(EXPECT)) {
      console.log(`PROOF: fabric answered arithmetic correctly (${EXPECT})`);
      process.exit(0);
    }
  }
};

setTimeout(() => {
  console.log("TIMEOUT: no correct fabric answer in", BUDGET / 1000, "s");
  process.exit(1);
}, BUDGET);
