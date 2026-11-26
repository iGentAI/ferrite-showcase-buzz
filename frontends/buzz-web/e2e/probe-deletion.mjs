/**
 * Live check of NIP-09 deletion, both shapes.
 *
 * The recorded open finding was: "NIP-09 deletion does not remove a replaceable
 * kind-10100 from reads (verified live)". The relay change claims two shapes now
 * work — an `e` tag erases one event id, and an `a` tag sets a deletion
 * WATERMARK on an addressable cell (`kind:pubkey:d`, empty `d` for 10100) so
 * even a version that arrives later retires itself.
 *
 * This proves both against the deployed relay, from the author's own key:
 *   1. a kind 9 in `general`, deleted by `e` tag, stops being served;
 *   2. a kind 10100, deleted by `a` coordinate, stops being served — and
 *      re-publishing a COVERED version afterwards does not bring it back.
 *
 *   node e2e/probe-deletion.mjs
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
console.log(`author ${me.slice(0, 16)} stamp ${stamp}`);

const ws = new WebSocket(WS);
const okFor = new Map();
const collectors = new Map();
let authed = false;
let n = 0;

const sign = (kind, tags, content = "") =>
  finalizeEvent(
    { kind, created_at: Math.floor(Date.now() / 1000), tags, content },
    sk,
  );
const pub = (ev, verb = "EVENT") =>
  new Promise((res) => {
    okFor.set(ev.id, res);
    ws.send(JSON.stringify([verb, ev]));
    setTimeout(() => res(["timeout", "no OK"]), 25_000);
  });
const query = (filter) =>
  new Promise((res) => {
    const id = `q${(n += 1)}`;
    const out = [];
    collectors.set(id, { out, res });
    ws.send(JSON.stringify(["REQ", id, filter]));
    setTimeout(() => {
      if (collectors.delete(id)) res(out);
    }, 15_000);
  });

/** Poll until the predicate holds, so a saga in flight is not read as a pass. */
async function until(label, filter, gone) {
  for (let attempt = 1; attempt <= 12; attempt += 1) {
    const got = await query(filter);
    const ok = gone ? got.length === 0 : got.length > 0;
    if (ok) {
      console.log(`${label}: ${gone ? "gone" : "present"} (attempt ${attempt})`);
      return true;
    }
    await new Promise((r) => setTimeout(r, 1_500));
  }
  console.log(`${label}: still ${gone ? "SERVED" : "MISSING"} after 12 attempts`);
  return false;
}

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
  if (m[0] === "EVENT") {
    collectors.get(m[1])?.out.push(m[2]);
    return;
  }
  if (m[0] === "EOSE") {
    const c = collectors.get(m[1]);
    if (c && collectors.delete(m[1])) c.res(c.out);
    return;
  }
  if (m[0] === "CLOSED" || m[0] === "NOTICE") {
    console.log("relay:", String(e.data).slice(0, 200));
    return;
  }
  if (m[0] !== "AUTH" || authed) return;
  authed = true;
  await pub(sign(22242, [["relay", WS], ["challenge", m[1]]]), "AUTH");
  await pub(sign(9021, [["h", "general"]]));
  const problems = [];

  // ---- shape 1: `e` tag on a plain event ----
  const msg = sign(9, [["h", "general"]], `deletion probe ${stamp}`);
  console.log("publish kind 9:", await pub(msg));
  if (!(await until("kind 9 before deletion", { ids: [msg.id] }, false))) {
    problems.push("the kind 9 was never served, so the deletion proves nothing");
  }
  console.log("publish kind 5 (e tag):", await pub(sign(5, [["e", msg.id]])));
  if (!(await until("kind 9 after deletion", { ids: [msg.id] }, true))) {
    problems.push("an `e`-tag deletion did not stop the event being served");
  }

  // ---- shape 2: `a` coordinate on a replaceable 10100 ----
  const profile = sign(
    10100,
    [["p", me], ["name", `probe-${stamp}`], ["respond_to", "anyone"], ["h", "general"]],
    JSON.stringify({ name: `probe-${stamp}`, respond_to: "anyone" }),
  );
  console.log("publish kind 10100:", await pub(profile));
  if (!(await until("10100 before deletion", { kinds: [10100], authors: [me] }, false))) {
    problems.push("the 10100 was never served, so its deletion proves nothing");
  }
  // NIP-01 coordinate for a plain replaceable kind: `kind:pubkey:` (empty d).
  console.log(
    "publish kind 5 (a coordinate):",
    await pub(sign(5, [["a", `10100:${me}:`]])),
  );
  if (!(await until("10100 after deletion", { kinds: [10100], authors: [me] }, true))) {
    problems.push("an `a`-coordinate deletion did not stop the 10100 being served");
  }

  // The watermark's whole point: a COVERED version published afterwards must
  // retire itself rather than reinstall the cell.
  const covered = finalizeEvent(
    {
      kind: 10100,
      created_at: Math.floor(Date.now() / 1000) - 5,
      tags: [["p", me], ["name", `probe-${stamp}`], ["h", "general"]],
      content: JSON.stringify({ name: `probe-${stamp}`, revived: true }),
    },
    sk,
  );
  console.log("re-publish a COVERED 10100:", await pub(covered));
  await new Promise((r) => setTimeout(r, 4_000));
  const revived = await query({ kinds: [10100], authors: [me] });
  console.log(`covered re-publish served=${revived.length}`);
  if (revived.length) problems.push("a covered re-publish resurrected the deleted cell");

  console.log(
    problems.length
      ? `FAIL: ${problems.join("; ")}`
      : "PROOF: both NIP-09 shapes stop the target being served, and the watermark holds",
  );
  process.exit(problems.length ? 1 : 0);
};

setTimeout(() => {
  console.log("TIMEOUT");
  process.exit(1);
}, 240_000);
