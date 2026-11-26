// Does a p-tagged kind 9 reach a SECOND socket on the concurrent relay?
// This is the relay half of scenario 5, with no client autocomplete involved.
//
// Env: BUZZ_WS (relay websocket URL), OPEN_TIMEOUT_MS (per-socket budget for
// the AUTH challenge, authentication and join; default 30000), OK_TIMEOUT_MS
// (per-event budget for the relay's OK; default 30000). A relay that never
// challenges, refuses authentication or the join, closes a socket (before or
// during the probe), or never answers fails the run with a FAIL line and
// exit 1 instead of hanging.
import { finalizeEvent, generateSecretKey, getPublicKey } from "nostr-tools/pure";
const WS = process.env.BUZZ_WS || "ws://3emul7w0sqvwfom6.localhost:28443/buzz/";
const OPEN_TIMEOUT_MS = Number(process.env.OPEN_TIMEOUT_MS || 30000);
const OK_TIMEOUT_MS = Number(process.env.OK_TIMEOUT_MS || 30000);
const stamp = Date.now().toString(36);
// Fresh CSPRNG keys each run; a key derived from the clock can be recomputed.
const mk = () => { const sk = generateSecretKey(); return { sk, pk: getPublicKey(sk) }; };
const ana = mk(), bo = mk();
function fail(message) {
  console.log(`FAIL: ${message}`);
  process.exit(1);
}
function open(who, label) {
  return new Promise((res, rej) => {
    const ws = new WebSocket(WS); const okFor = new Map(); let authed = false; let settled = false;
    const settle = (fn, value) => { if (!settled) { settled = true; clearTimeout(deadline); fn(value); } };
    // Before readiness a socket failure rejects open(); after it, the probe is
    // mid-run, so the same failure ends the run with its real cause instead of
    // leaving a publication to time out or bo's socket to look silent.
    const lost = (message) => { if (settled) fail(message); else settle(rej, new Error(message)); };
    const deadline = setTimeout(() => {
      settle(rej, new Error(`${label}: the relay did not challenge, authenticate and admit this socket within ${OPEN_TIMEOUT_MS} ms`));
      ws.close();
    }, OPEN_TIMEOUT_MS);
    const sign = (k, t, c) => finalizeEvent({ kind: k, created_at: Math.floor(Date.now()/1000), tags: t, content: c }, who.sk);
    const pub = (ev, verb="EVENT") => new Promise((r2, rj) => {
      const timer = setTimeout(() => { okFor.delete(ev.id); rj(new Error(`${label}: no OK for ${verb} kind ${ev.kind} within ${OK_TIMEOUT_MS} ms`)); }, OK_TIMEOUT_MS);
      okFor.set(ev.id, (verdict) => { clearTimeout(timer); r2(verdict); });
      ws.send(JSON.stringify([verb, ev]));
    });
    const api = { ws, sign, pub, seen: [], label };
    ws.onerror = (e) => lost(`${label}: websocket error${e?.message ? `: ${e.message}` : ""}`);
    ws.onclose = (e) => lost(`${label}: the relay closed the socket (code ${e.code})${settled ? " during the probe" : " before it was ready"}`);
    const handle = async (e) => {
      const m = JSON.parse(String(e.data));
      if (m[0] === "OK") { const r = okFor.get(m[1]); if (r) { okFor.delete(m[1]); r([m[2], m[3]]); } return; }
      if (m[0] === "EVENT") { api.seen.push(m[2]); return; }
      if (m[0] === "AUTH" && !authed) {
        authed = true;
        const [ok, why] = await pub(sign(22242, [["relay", WS], ["challenge", m[1]]], ""), "AUTH");
        if (ok !== true) throw new Error(`${label}: the relay refused authentication: ${why}`);
        const [joined, whyNot] = await pub(sign(9021, [["h","general"]], ""));
        if (joined !== true) throw new Error(`${label}: the relay refused the join to #general: ${whyNot}`);
        settle(res, api);
      }
    };
    // The handler's failures (a missing OK, a refusal) settle open() instead of
    // escaping as an unhandled rejection.
    ws.onmessage = (e) => { handle(e).catch((error) => settle(rej, error)); };
  });
}
let A, B;
try {
  A = await open(ana, "ana"); B = await open(bo, "bo");
} catch (error) {
  fail(error.message);
}
B.ws.send(JSON.stringify(["REQ","live",{ kinds:[9], "#h":["general"], since: Math.floor(Date.now()/1000) }]));
await new Promise(r => setTimeout(r, 2000));
const ask = A.sign(9, [["h","general"],["p",bo.pk]], `protocol mention ${stamp}`);
console.log("ana publish:", await A.pub(ask).catch((error) => fail(error.message)));
for (let i = 0; i < 30; i++) {
  await new Promise(r => setTimeout(r, 1000));
  if (B.seen.some(e => e.id === ask.id)) { console.log(`PASS: p-tagged kind 9 reached bo's socket in ~${i+1}s`); process.exit(0); }
}
console.log("FAIL: bo never received it; bo saw", B.seen.length, "events");
process.exit(1);
