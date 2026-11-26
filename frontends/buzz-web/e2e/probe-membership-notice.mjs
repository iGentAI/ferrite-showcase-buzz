/**
 * Does the relay actually mint and DELIVER a kind 44100 membership notice?
 *
 * The relay-side change (`effects::emit_membership_notices`) is gated by unit
 * tests over the store rows; live delivery to the subject's socket was left
 * unproven. Scenario 11 asserts it through the browser and did not see one, so
 * this isolates the two halves: two identities on two sockets, an owner adding
 * a subject with a real kind 9000, and the subject watching exactly the filter
 * the shipped client uses (`useMembershipNotifications`).
 *
 *   node e2e/probe-membership-notice.mjs
 *
 * Not a Playwright test (`.mjs`, no `.spec.`), so the suite never collects it.
 */
import { finalizeEvent, generateSecretKey, getPublicKey } from "nostr-tools/pure";

const HOST = process.env.BUZZ_HOST || "3emul7w0sqvwfom6.localhost:28443";
const WS = `ws://${HOST}/buzz/`;
const stamp = Date.now().toString(36);
const room = `notice-${stamp}`;

// A fresh random key per role: a key derived from a label is recomputable by
// anyone who reads the label here and the stamp on the relay.
const mk = () => {
  const sk = generateSecretKey();
  return { sk, pk: getPublicKey(sk) };
};
const owner = mk("owner");
const subject = mk("subject");
console.log(
  `room ${room}\nowner   ${owner.pk.slice(0, 16)}\nsubject ${subject.pk.slice(0, 16)}`,
);

function connect(who, onAuth) {
  const ws = new WebSocket(WS);
  const okFor = new Map();
  const state = { ws, authed: false, notices: [] };
  const sign = (kind, tags, content = "") =>
    finalizeEvent(
      { kind, created_at: Math.floor(Date.now() / 1000), tags, content },
      who.sk,
    );
  state.sign = sign;
  state.pub = (ev, verb = "EVENT") =>
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
    if (m[0] === "EVENT") {
      state.notices.push(m[2]);
      console.log(
        `[${who.label ?? "sub"}] EVENT kind=${m[2].kind} tags=${JSON.stringify(m[2].tags).slice(0, 180)}`,
      );
      return;
    }
    if (m[0] === "CLOSED" || m[0] === "NOTICE") {
      console.log(`[relay] ${String(e.data).slice(0, 240)}`);
      return;
    }
    if (m[0] === "AUTH" && !state.authed) {
      state.authed = true;
      await state.pub(
        sign(22242, [
          ["relay", WS],
          ["challenge", m[1]],
        ]),
        "AUTH",
      );
      await onAuth(state);
    }
  };
  return state;
}

const subjectReady = new Promise((resolve) => {
  connect(subject, async (s) => {
    // EXACTLY the filter the shipped client subscribes with.
    s.ws.send(
      JSON.stringify([
        "REQ",
        "membership",
        {
          kinds: [44100, 44101],
          "#p": [subject.pk],
          limit: 50,
          since: Math.floor(Date.now() / 1000) - 30,
        },
      ]),
    );
    console.log("subject: watching {kinds:[44100,44101], #p:[me]}");
    resolve(s);
  });
});
const sub = await subjectReady;

/** NIP-98 Authorization header, as `shared/lib/api.ts` builds it. */
async function nip98(url, method, body, sk) {
  const tags = [
    ["u", url],
    ["method", method],
  ];
  if (body !== undefined) {
    const digest = await crypto.subtle.digest(
      "SHA-256",
      new TextEncoder().encode(body),
    );
    tags.push([
      "payload",
      Array.from(new Uint8Array(digest))
        .map((b) => b.toString(16).padStart(2, "0"))
        .join(""),
    ]);
    tags.push(["nonce", crypto.randomUUID()]);
  }
  const ev = finalizeEvent(
    { kind: 27235, created_at: Math.floor(Date.now() / 1000), tags, content: "" },
    sk,
  );
  return `Nostr ${Buffer.from(JSON.stringify(ev)).toString("base64")}`;
}

connect(owner, async (o) => {
  // MODE=dm exercises the OTHER trigger the relay is supposed to notify on: a
  // DM open is a kind 41010 with `p` tags (`openDmViaRelay`), and the relay's
  // gate claims the other participant is notified.
  if (process.env.MODE === "dm") {
    console.log(
      "owner: open DM (41010)",
      await o.pub(o.sign(41010, [["p", subject.pk]])),
    );
    for (let i = 1; i <= 20; i += 1) {
      await new Promise((r) => setTimeout(r, 1_500));
      const hit = sub.notices.find(
        (ev) =>
          (ev.kind === 44100 || ev.kind === 44101) &&
          ev.tags.some((t) => t[0] === "p" && t[1] === subject.pk),
      );
      if (hit) {
        console.log(
          `PROOF: kind ${hit.kind} reached the DM recipient after ~${i * 1.5}s`,
          JSON.stringify(hit.tags),
        );
        process.exit(0);
      }
    }
    console.log(
      `FAIL: a DM open minted no membership notice for the recipient; frames seen: ${sub.notices.length}`,
    );
    process.exit(1);
  }

  // A room is created over the HTTP face (`ingress.rs: create_channel`), not by
  // a WS event — the same call `createChannelViaRelay` makes.
  const url = `http://${HOST}/buzz/api/channels`;
  const body = JSON.stringify({
    id: room,
    name: room,
    type: "stream",
    visibility: "private",
  });
  const created = await fetch(url, {
    method: "POST",
    headers: {
      "x-buzz-authorization": await nip98(url, "POST", body, owner.sk),
      "content-type": "application/json",
    },
    body,
  });
  console.log(
    "owner: create private room",
    created.status,
    (await created.text()).slice(0, 200),
  );
  console.log("owner: join own room", await o.pub(o.sign(9021, [["h", room]])));
  // NIP-29 add-user: the shape the members sidebar sends.
  console.log(
    "owner: add subject (9000)",
    await o.pub(
      o.sign(9000, [
        ["h", room],
        ["p", subject.pk],
      ]),
    ),
  );

  for (let i = 1; i <= 20; i += 1) {
    await new Promise((r) => setTimeout(r, 1_500));
    const hit = sub.notices.find(
      (ev) =>
        ev.kind === 44100 &&
        ev.tags.some((t) => t[0] === "p" && t[1] === subject.pk),
    );
    if (hit) {
      console.log(
        `PROOF: 44100 reached the subject's socket after ~${i * 1.5}s`,
        JSON.stringify(hit).slice(0, 300),
      );
      process.exit(0);
    }
  }
  console.log(
    `FAIL: no 44100 on the subject's socket; frames seen: ${sub.notices.length}`,
  );
  process.exit(1);
});

setTimeout(() => {
  console.log("TIMEOUT");
  process.exit(1);
}, 120_000);
