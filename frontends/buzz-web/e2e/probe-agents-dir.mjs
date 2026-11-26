// Dump every kind 10100 agent-directory entry, to compare what makes an agent
// mentionable (`relayAgentIsSharedWithUser` needs channel ids + respond_to).
import { finalizeEvent, generateSecretKey } from "nostr-tools/pure";
const HOST = "3emul7w0sqvwfom6.localhost:28443", WS = `ws://${HOST}/buzz/`;
// A fresh random key: a key derived from a label is recomputable by anyone.
const sk = generateSecretKey();
const ws = new WebSocket(WS); const okFor = new Map(); let authed = false;
const sign = (k, t, c = "") => finalizeEvent({ kind: k, created_at: Math.floor(Date.now() / 1000), tags: t, content: c }, sk);
const pub = (ev, v = "EVENT") => new Promise(r => { okFor.set(ev.id, r); ws.send(JSON.stringify([v, ev])); });
ws.onmessage = async e => {
  const m = JSON.parse(String(e.data));
  if (m[0] === "OK") { const r = okFor.get(m[1]); if (r) { okFor.delete(m[1]); r(); } return; }
  if (m[0] === "EVENT") { console.log(JSON.stringify({ pk: m[2].tags.find(t=>t[0]==="p")?.[1]?.slice(0,12), content: m[2].content, tags: m[2].tags })); return; }
  if (m[0] === "EOSE") { process.exit(0); }
  if (m[0] === "AUTH" && !authed) { authed = true; await pub(sign(22242, [["relay", WS], ["challenge", m[1]]]), "AUTH");
    ws.send(JSON.stringify(["REQ", "d", { kinds: [10100], limit: 50 }])); }
};
setTimeout(() => process.exit(0), 20000);
