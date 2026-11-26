/**
 * Does the BROWSER's socket receive the relay's 44100, or is the gap in the
 * scenario's frame tap?
 *
 * Established already:
 *   * the relay mints and delivers a 44100 to a raw socket in ~1.5 s, for both
 *     a kind 9000 add and a kind 41010 DM open (`probe-membership-notice.mjs`);
 *   * the shipped client subscribes with exactly the right filter
 *     (`{kinds:[44100,44101],"#p":[me],limit:50,since:now-30}`).
 *
 * So this onboards ONE real client, then opens a DM to it from a throwaway CLI
 * key in this same process, and reports what the browser's socket actually saw.
 *
 *   node e2e/probe-notice-in-browser.mjs
 *
 * Not a Playwright test (`.mjs`, no `.spec.`), so the suite never collects it.
 */
import { chromium } from "@playwright/test";
import { finalizeEvent, generateSecretKey, getPublicKey } from "nostr-tools/pure";

const UI = process.env.BUZZ_UI ?? "http://localhost:4173/buzz/ui/index.html";
const RELAY_WS = process.env.BUZZ_WS ?? "ws://localhost:4173/buzz/";
const HOST = process.env.BUZZ_HOST || "3emul7w0sqvwfom6.localhost:28443";
const stamp = Date.now().toString(36);

const browser = await chromium.launch();
const ctx = await browser.newContext();
const page = await ctx.newPage();

let pubkey = "";
const received = [];
const allKinds = new Map();
page.on("websocket", (ws) => {
  ws.on("framesent", (frame) => {
    try {
      const m = JSON.parse(String(frame.payload));
      if ((m[0] === "AUTH" || m[0] === "EVENT") && !pubkey) pubkey = m[1]?.pubkey ?? "";
    } catch {
      /* not JSON */
    }
  });
  ws.on("framereceived", (frame) => {
    try {
      const m = JSON.parse(String(frame.payload));
      if (m[0] !== "EVENT") return;
      const kind = m[2]?.kind;
      allKinds.set(kind, (allKinds.get(kind) ?? 0) + 1);
      if (kind === 44100 || kind === 44101) {
        received.push(m[2]);
        console.log(`BROWSER SOCKET GOT kind=${kind} tags=${JSON.stringify(m[2].tags)}`);
      }
    } catch {
      /* not JSON */
    }
  });
});

await page.goto(UI, { waitUntil: "domcontentloaded" });
await page.getByRole("button", { name: /create a new identity key/i }).click({ timeout: 45_000 });
await page.getByRole("button", { name: /^Next$/ }).click({ timeout: 45_000 });
await page.getByRole("button", { name: /skip for now/i }).first().click({ timeout: 45_000 });
await page.getByRole("button", { name: /join a community/i }).first().click({ timeout: 45_000 });
await page.locator("input:visible").first().fill(RELAY_WS);
await page.getByRole("button", { name: /^Next$/ }).first().click({ timeout: 45_000 });
await page.getByPlaceholder(/username/i).fill(`Probe Recv ${stamp}`, { timeout: 45_000 });
await page.getByRole("button", { name: /^Next$/ }).first().click({ timeout: 45_000 });
await page.getByRole("button", { name: /take me to buzz|skip for now/i }).first().click({ timeout: 90_000 });
await page.getByTestId("app-sidebar").waitFor({ state: "visible", timeout: 90_000 });
await page
  .waitForFunction(() => !document.body.innerText.includes("Meet your starter team"), null, {
    timeout: 45_000,
  })
  .catch(() => {});
console.log(`onboarded browser identity ${pubkey}`);
const dmListBefore = await page.getByTestId("dm-list").isVisible().catch(() => false);
console.log(`dm-list visible before: ${dmListBefore}`);

// Now open a DM to that identity from a throwaway key over a raw socket.
// A fresh random key: a key derived from a label is recomputable by anyone.
const sk = generateSecretKey();
console.log(`sender ${getPublicKey(sk).slice(0, 16)}`);
const WS = `ws://${HOST}/buzz/`;
const ws = new WebSocket(WS);
const okFor = new Map();
let authed = false;
const sign = (kind, tags, content = "") =>
  finalizeEvent({ kind, created_at: Math.floor(Date.now() / 1000), tags, content }, sk);
const pub = (ev, verb = "EVENT") =>
  new Promise((res, rej) => {
    okFor.set(ev.id, res);
    ws.send(JSON.stringify([verb, ev]));
    setTimeout(() => rej(new Error("no OK")), 30_000);
  });
const opened = new Promise((resolve) => {
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
    if (m[0] === "AUTH" && !authed) {
      authed = true;
      await pub(sign(22242, [["relay", WS], ["challenge", m[1]]]), "AUTH");
      console.log("sender: open DM (41010)", await pub(sign(41010, [["p", pubkey]])));
      resolve();
    }
  };
});
await opened;

for (let i = 1; i <= 16; i += 1) {
  await page.waitForTimeout(5_000);
  const dmVisible = await page.getByTestId("dm-list").isVisible().catch(() => false);
  console.log(
    `t+${i * 5}s notices_on_browser_socket=${received.length} dm_list_visible=${dmVisible}`,
  );
  if (received.length && dmVisible) {
    console.log("PROOF: the notice reached the browser and the DM list rendered");
    break;
  }
}
console.log(
  "event kinds the browser socket received:",
  [...allKinds.entries()].sort((a, b) => b[1] - a[1]).slice(0, 20),
);
await browser.close();
