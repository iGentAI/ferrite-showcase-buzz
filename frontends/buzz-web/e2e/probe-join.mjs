/**
 * Root-cause probe for onboarding's last gate.
 *
 * The client's `ensure_starter_channels` fails when, after a 9021 join, the
 * room's 39002 roster does not list the joiner (`is_member`). This reproduces
 * exactly that at the protocol level, which is seconds instead of a browser
 * scenario's minutes.
 *
 *   node e2e/probe-join.mjs
 *
 * Not a Playwright test (`.mjs`, no `.spec.`), so the suite never collects it.
 */
import { finalizeEvent, generateSecretKey, getPublicKey } from "nostr-tools/pure";

const HOST = process.env.BUZZ_HOST || "3emul7w0sqvwfom6.localhost:28443";
const WS = `ws://${HOST}/buzz/`;
const ROOM = process.env.ROOM || "general";

const stamp = Date.now().toString(36);
// A fresh random key: a key derived from a label is recomputable by anyone.
const sk = generateSecretKey();
const me = getPublicKey(sk);
console.log(`probe identity ${me} stamp ${stamp} room ${ROOM}`);

const ws = new WebSocket(WS);
const okFor = new Map();
let authed = false;

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

/** One-shot query: collect EVENTs for a sub id until EOSE. */
const query = (id, filter) =>
  new Promise((res) => {
    const out = [];
    collectors.set(id, { out, res });
    ws.send(JSON.stringify(["REQ", id, filter]));
    setTimeout(() => {
      collectors.delete(id);
      res(out);
    }, 15_000);
  });
const collectors = new Map();

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
    const c = collectors.get(m[1]);
    if (c) c.out.push(m[2]);
    return;
  }
  if (m[0] === "EOSE") {
    const c = collectors.get(m[1]);
    if (c) {
      collectors.delete(m[1]);
      c.res(c.out);
    }
    return;
  }
  if (m[0] === "CLOSED" || m[0] === "NOTICE") {
    console.log("relay:", String(e.data).slice(0, 300));
    return;
  }
  if (m[0] === "AUTH" && !authed) {
    authed = true;
    console.log(
      "auth:",
      await pub(sign(22242, [["relay", WS], ["challenge", m[1]]], ""), "AUTH"),
    );

    // What rooms does the relay advertise, and what is the room's id?
    // Exactly the two reads the client's channel list performs
    // (`channelsFromRelay`: RELAY_PAGE_LIMIT = 500, kinds-only, no tag filter).
    const metas = await query("meta", { kinds: [39000], limit: 500 });
    const rosters0 = await query("ros0", { kinds: [39002], limit: 500 });
    const dOf = (ev) => ev.tags.find((t) => t[0] === "d")?.[1];
    console.log(
      `client-shaped reads: 39000=${metas.length} (has ${ROOM}: ${metas.some((e) => dOf(e) === ROOM)})` +
        ` 39002=${rosters0.length} (has ${ROOM}: ${rosters0.some((e) => dOf(e) === ROOM)})`,
    );
    const room = metas.find((ev) => dOf(ev) === ROOM);
    const roomId =
      room?.tags.find((t) => t[0] === "d")?.[1] ??
      room?.tags.find((t) => t[0] === "h")?.[1];
    console.log(`room 39000 found=${Boolean(room)} id=${roomId}`);
    if (!roomId) {
      console.log("FAIL: no 39000 for that room name");
      process.exit(1);
    }

    console.log("join 9021:", await pub(sign(9021, [["h", roomId]], "")));

    // Poll the roster the way the client does: a kinds-only page, then pick
    // the room out of it.
    for (let attempt = 1; attempt <= 8; attempt += 1) {
      await new Promise((r) => setTimeout(r, 700));
      const page = await query(`ros${attempt}`, {
        kinds: [39002],
        limit: 500,
      });
      const roster = page.find(
        (ev) => ev.tags.find((t) => t[0] === "d")?.[1] === roomId,
      );
      const members = (roster?.tags ?? []).filter((t) => t[0] === "p").length;
      const mine = (roster?.tags ?? []).some(
        (t) => t[0] === "p" && t[1] === me,
      );
      console.log(
        `attempt ${attempt}: 39002 present=${Boolean(roster)} members=${members} includes_me=${mine}`,
      );
      if (mine) {
        console.log("PROOF: the join landed in the roster projection");
        process.exit(0);
      }
    }
    console.log("FAIL: the 9021 was accepted but the roster never listed me");
    process.exit(1);
  }
};

setTimeout(() => {
  console.log("TIMEOUT");
  process.exit(1);
}, 120_000);
