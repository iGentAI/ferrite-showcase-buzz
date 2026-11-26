/**
 * Repair the LIVE fixture so onboarding works again.
 *
 * ## Why this is needed
 *
 * `query::MAX_PAGE_BYTES` caps one query page at 512 KiB, newest-first, and the
 * paging contract is "follow the cursor" (`until`). buzz-web's channel list does
 * NOT page: `channelsFromRelay()` issues a single `{kinds:[39000],limit:500}`
 * REQ and treats the first page as the complete set — its own comment only
 * accounts for the 500-EVENT clamp, not the byte cap.
 *
 * This deployment has accumulated ~133 rooms from earlier test runs, so the
 * 39000 set no longer fits one page. The rooms that fall off are the OLDEST,
 * and the starter rooms (`general`, `welcome-everyone`) are the oldest of all.
 * `ensureStarterChannels()` then cannot satisfy its readiness gate and every
 * onboarding fails with "Starter channels were not available after setup" —
 * which blocks all twelve scenarios before they assert anything.
 *
 * ## What this does
 *
 * Sets a `topic` on each starter room over the real product path. A 9002 edit
 * carrying only `topic` needs nothing but active membership
 * (`effects::authorize_channel_edit`: `MEMBER_EDIT_TAGS`), and the relay
 * re-publishes the room's 39000 from it — which moves that metadata to the
 * newest end of the window, back inside the first page.
 *
 * This repairs DATA, not code. It does not hide the defect: the client's
 * missing pagination is reported as a source bug, and this deployment will trip
 * it again once ~130 more rooms accumulate.
 *
 *   node e2e/repair-starter-rooms.mjs
 *
 * BUZZ_WS names the socket URL outright; otherwise it is built from BUZZ_HOST,
 * with wss:// for anything that is not loopback. Any missing OK, refusal or
 * lost socket stops the run with the steps already accepted listed, then
 * `FAIL: <why>` and exit code 1, so a half-done repair is never silent.
 *
 * Not a Playwright test (`.mjs`, no `.spec.`), so the suite never collects it.
 */
import { finalizeEvent, generateSecretKey, getPublicKey } from "nostr-tools/pure";

import { connectRelay, relayWsUrl, runScript } from "./scriptRelay.mjs";

const WS = relayWsUrl(process.env, "3emul7w0sqvwfom6.localhost:28443");
const ROOMS = ["general", "welcome-everyone"];

const stamp = Date.now().toString(36);
// A fresh random key: a key derived from a label is recomputable by anyone.
const sk = generateSecretKey();
console.log(`repair identity ${getPublicKey(sk).slice(0, 12)} stamp ${stamp} against ${WS}`);

const sign = (kind, tags, content) =>
  finalizeEvent(
    { kind, created_at: Math.floor(Date.now() / 1000), tags, content },
    sk,
  );
const dOf = (ev) => ev.tags.find((t) => t[0] === "d")?.[1];

runScript(async () => {
  const { publish, query } = await connectRelay(WS, sk);
  const accepted = [];
  /** Publish one step; a refusal or a missing OK stops the repair. */
  const step = async (label, event) => {
    const [ok, reason] = await publish(event);
    console.log(`${label}:`, [ok, reason]);
    if (!ok) throw new Error(`${label} refused: ${reason}`);
    accepted.push(label);
  };

  try {
    for (const room of ROOMS) {
      // Membership first: a `topic` edit requires an ACTIVE membership.
      await step(`${room}: join`, sign(9021, [["h", room]], ""));
      const edit = sign(
        9002,
        [
          ["h", room],
          ["d", room],
          ["topic", `Shared workspace room — kept current ${stamp}`],
        ],
        "",
      );
      await step(`${room}: 9002 topic edit`, edit);
    }
  } catch (error) {
    console.log(`repair stopped; accepted before the failure: ${accepted.join(", ") || "nothing"}`);
    throw error;
  }

  // Prove the repair: the client's exact read must now carry both rooms.
  for (let attempt = 1; attempt <= 10; attempt += 1) {
    await new Promise((r) => setTimeout(r, 1_000));
    const page = await query({ kinds: [39000], limit: 500 });
    const present = ROOMS.filter((room) => page.some((ev) => dOf(ev) === room));
    console.log(
      `attempt ${attempt}: 39000 page=${page.length} starters_in_page=[${present.join(",")}]`,
    );
    if (present.length === ROOMS.length) {
      console.log("PROOF: both starter rooms are inside the client's first page");
      return 0;
    }
  }
  console.log("FAIL: the starter rooms are still outside the first page");
  return 1;
}, { deadlineMs: 180_000 });
