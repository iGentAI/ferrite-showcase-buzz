/**
 * Can a client FORGE a membership notice?
 *
 * 44100/44101 are relay-minted and were added to `is_relay_only_kind`, so a
 * client publishing one must be refused. This is the live check of that gate.
 *
 *   node e2e/probe-notice-forgery.mjs
 *
 * Not a Playwright test (`.mjs`, no `.spec.`), so the suite never collects it.
 */
import { finalizeEvent, generateSecretKey, getPublicKey } from "nostr-tools/pure";
const HOST = process.env.BUZZ_HOST || "3emul7w0sqvwfom6.localhost:28443";
const WS = `ws://${HOST}/buzz/`;
const stamp = Date.now().toString(36);
// A fresh random key: a key derived from a label is recomputable by anyone.
const sk = generateSecretKey();
const me = getPublicKey(sk);
const ws = new WebSocket(WS); const okFor = new Map(); let authed = false;
const sign = (k, t, c = "") => finalizeEvent({ kind: k, created_at: Math.floor(Date.now() / 1000), tags: t, content: c }, sk);
const pub = (ev, v = "EVENT") => new Promise((res) => { okFor.set(ev.id, res); ws.send(JSON.stringify([v, ev])); setTimeout(() => res(["timeout", "no OK"]), 20000); });
ws.onmessage = async (e) => {
  const m = JSON.parse(String(e.data));
  if (m[0] === "OK") { const r = okFor.get(m[1]); if (r) { okFor.delete(m[1]); r([m[2], m[3]]); } return; }
  if (m[0] === "AUTH" && !authed) {
    authed = true;
    await pub(sign(22242, [["relay", WS], ["challenge", m[1]]]), "AUTH");
    await pub(sign(9021, [["h", "general"]]));
    const problems = [];
    for (const kind of [44100, 44101]) {
      const r = await pub(sign(kind, [["h", "general"], ["p", me]]));
      console.log(`forged kind ${kind}:`, r);
      if (r[0] === true) problems.push(`kind ${kind} was ACCEPTED from a client`);
    }
    console.log(problems.length ? `FAIL: ${problems.join("; ")}` : "PROOF: both notice kinds are relay-only and refused from a client");
    process.exit(problems.length ? 1 : 0);
  }
};
setTimeout(() => { console.log("TIMEOUT"); process.exit(1); }, 90000);
