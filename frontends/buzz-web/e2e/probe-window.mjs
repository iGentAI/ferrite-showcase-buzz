/**
 * Is `welcome-everyone`'s 39000 GONE, or merely outside the page the relay
 * chose to return?
 *
 * The client asks for `{kinds:[39000], limit:500}` and gets ~131 events back
 * without that room's metadata, while the same room's 39002 roster is present.
 * Two very different causes: a truncated page (the room's metadata is older
 * than the slice the relay returned) or an erased projection. This walks the
 * window backwards with `until` and also tries the tag filters, so the answer
 * is evidence rather than inference.
 *
 *   node e2e/probe-window.mjs
 *
 * BUZZ_WS names the socket URL outright; otherwise it is built from BUZZ_HOST,
 * with wss:// for anything that is not loopback. A missing OK, a closed socket
 * or a refused AUTH prints `FAIL: <why>` and exits 1 (`scriptRelay.mjs`).
 *
 * Not a Playwright test (`.mjs`, no `.spec.`), so the suite never collects it.
 */
import { generateSecretKey, getPublicKey } from "nostr-tools/pure";

import { connectRelay, relayWsUrl, runScript } from "./scriptRelay.mjs";

const WS = relayWsUrl(process.env, "3emul7w0sqvwfom6.localhost:28443");
const TARGETS = ["general", "welcome-everyone"];

// A fresh random key: a key derived from a label is recomputable by anyone.
const sk = generateSecretKey();
console.log(`probe ${getPublicKey(sk).slice(0, 12)} against ${WS}`);

const dOf = (ev) => ev.tags.find((t) => t[0] === "d")?.[1];

runScript(async () => {
  const { query } = await connectRelay(WS, sk);

  // 1. The plain page the client uses, at several limits.
  for (const limit of [50, 132, 200, 500]) {
    const page = await query({ kinds: [39000], limit });
    const ts = page.map((ev) => ev.created_at);
    console.log(
      `limit=${limit} returned=${page.length}` +
        ` created_at=[${Math.min(...ts)}..${Math.max(...ts)}]` +
        TARGETS.map((t) => ` ${t}=${page.some((ev) => dOf(ev) === t)}`).join(""),
    );
  }

  // 2. Tag filters, which NIP-01 says must work for single-letter tags.
  for (const target of TARGETS) {
    const byD = await query({ kinds: [39000], "#d": [target], limit: 10 });
    const byH = await query({ kinds: [39000], "#h": [target], limit: 10 });
    console.log(`${target}: #d=${byD.length} #h=${byH.length}`);
  }

  // 3. Walk backwards through the window. If the metadata merely fell out of a
  //    truncated newest-first page, an older slice must contain it.
  let until = Math.floor(Date.now() / 1000) + 60;
  for (let page = 1; page <= 8; page += 1) {
    const got = await query({ kinds: [39000], limit: 500, until });
    if (!got.length) {
      console.log(`page ${page}: empty, window exhausted`);
      break;
    }
    const oldest = Math.min(...got.map((ev) => ev.created_at));
    const found = TARGETS.filter((t) => got.some((ev) => dOf(ev) === t));
    console.log(
      `page ${page}: until=${until} returned=${got.length} oldest=${oldest} found=[${found.join(",")}]`,
    );
    if (found.length === TARGETS.length) {
      console.log("both starter rooms are reachable by paging backwards");
      break;
    }
    if (until === oldest - 1) break;
    until = oldest - 1;
  }
  return 0;
}, { deadlineMs: 240_000 });
