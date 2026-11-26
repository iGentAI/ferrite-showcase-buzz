/**
 * Does a `#d` filter work for USER-AUTHORED addressable kinds?
 *
 * The register records that `{kinds:[39000],"#d":[…]}` returns nothing on this
 * relay while the same kinds-only page returns that exact event — a NIP-01
 * conformance bug in the tag-index write path for RELAY-MINTED projections. Two
 * client features depend on the same filter shape for events a MEMBER signs, so
 * whether the bug reaches them decides whether either can work at all:
 *
 *   custom emoji  `fetchCommunityEmojiEvents` asks for
 *                 `{kinds:[30030], "#d":["buzz:custom-emoji"], limit:500}`. If
 *                 that returns nothing, the community palette is permanently
 *                 empty and `:shortcode:` can never render anywhere.
 *   user status   `useUserStatusQuery` asks for
 *                 `{kinds:[30315], authors:[…], "#d":["general"]}`, and the live
 *                 subscription uses the same `#d`.
 *
 * Read-only: it opens one socket, authenticates a throwaway key, and issues
 * three REQs. It publishes nothing and mutates nothing.
 *
 *   node e2e/probe-tag-index.mjs
 *
 * Not a Playwright test (`.mjs`, no `.spec.`), so the suite never collects it.
 */
import { finalizeEvent, generateSecretKey, getPublicKey } from "nostr-tools/pure";

const WS = process.env.BUZZ_WS || "ws://localhost:4173/buzz/";
// A fresh random key: a key derived from a label is recomputable by anyone.
const sk = generateSecretKey();
console.log(`probe ${getPublicKey(sk).slice(0, 12)} against ${WS}`);

const ws = new WebSocket(WS);
const subs = new Map();
let counter = 0;
const req = (filter) =>
  new Promise((resolve) => {
    const id = `q${(counter += 1)}`;
    const out = { events: [], closed: null };
    subs.set(id, { out, resolve });
    ws.send(JSON.stringify(["REQ", id, filter]));
    setTimeout(() => {
      if (subs.delete(id)) resolve(out);
    }, 15_000);
  });

let started = false;
ws.onmessage = (event) => {
  const message = JSON.parse(String(event.data));
  if (message[0] === "AUTH") {
    ws.send(
      JSON.stringify([
        "AUTH",
        finalizeEvent(
          {
            kind: 22242,
            created_at: Math.floor(Date.now() / 1000),
            tags: [
              ["relay", WS],
              ["challenge", message[1]],
            ],
            content: "",
          },
          sk,
        ),
      ]),
    );
    return;
  }
  if (message[0] === "OK") {
    console.log("AUTH", message[2], message[3]);
    if (message[2]) void run();
    return;
  }
  const sub = subs.get(message[1]);
  if (!sub) {
    console.log("frame", String(event.data).slice(0, 160));
    return;
  }
  if (message[0] === "EVENT") sub.out.events.push(message[2]);
  if (message[0] === "EOSE") {
    subs.delete(message[1]);
    sub.resolve(sub.out);
  }
  if (message[0] === "CLOSED") {
    sub.out.closed = message[2];
    subs.delete(message[1]);
    sub.resolve(sub.out);
  }
};

async function run() {
  if (started) return;
  started = true;
  const emojiByTag = await req({
    kinds: [30030],
    "#d": ["buzz:custom-emoji"],
    limit: 5,
  });
  const emojiByKind = await req({ kinds: [30030], limit: 5 });
  const statusByTag = await req({ kinds: [30315], "#d": ["general"], limit: 5 });
  const statusByKind = await req({ kinds: [30315], limit: 5 });
  console.log(
    `30030 #d=buzz:custom-emoji : ${emojiByTag.events.length} events (closed=${emojiByTag.closed})`,
  );
  console.log(
    `30030 kinds only          : ${emojiByKind.events.length} events (closed=${emojiByKind.closed})`,
  );
  console.log(
    `30315 #d=general          : ${statusByTag.events.length} events (closed=${statusByTag.closed})`,
  );
  console.log(
    `30315 kinds only          : ${statusByKind.events.length} events (closed=${statusByKind.closed})`,
  );
  // The DIAGNOSTIC comparison, not the counts on their own: a tag filter that
  // returns zero where the kinds-only page returned rows carrying that exact
  // tag is the index bug; both zero just means nobody has published one yet.
  for (const [label, byTag, byKind, tag, value] of [
    ["30030", emojiByTag, emojiByKind, "d", "buzz:custom-emoji"],
    ["30315", statusByTag, statusByKind, "d", "general"],
  ]) {
    const present = byKind.events.filter((event) =>
      event.tags.some((t) => t[0] === tag && t[1] === value),
    ).length;
    if (present > 0 && byTag.events.length === 0) {
      console.log(
        `BUG ${label}: ${present} of the kinds-only page carry ["${tag}","${value}"] and the #${tag} filter returned none`,
      );
    } else if (present === 0) {
      console.log(
        `INCONCLUSIVE ${label}: nothing on the kinds-only page carries ["${tag}","${value}"]`,
      );
    } else {
      console.log(`OK ${label}: the #${tag} filter agrees with the kinds-only page`);
    }
  }
  process.exit(0);
}

setTimeout(() => {
  console.log("TIMEOUT");
  process.exit(1);
}, 60_000);
