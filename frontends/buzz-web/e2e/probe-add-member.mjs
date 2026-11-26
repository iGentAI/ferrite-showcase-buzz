/**
 * Why does the members sidebar's "Add" leave its button stuck disabled?
 *
 * The relay path is proven: `probe-membership-notice.mjs` shows a kind 9000 add
 * accepted and a relay-signed 44100 on the subject's socket in ~1.5 s. So when
 * scenario 11 sees no notice, the question is what the CLIENT sent (or failed to
 * send) when the moderator clicked Add. This drives the same UI and prints every
 * console error, every published event, and every refusal.
 *
 *   node e2e/probe-add-member.mjs
 *
 * Not a Playwright test (`.mjs`, no `.spec.`), so the suite never collects it.
 */
import { chromium } from "@playwright/test";

const UI = process.env.BUZZ_UI ?? "http://localhost:4173/buzz/ui/index.html";
const RELAY_WS = process.env.BUZZ_WS ?? "ws://localhost:4173/buzz/";
const stamp = Date.now().toString(36);

const browser = await chromium.launch();

async function onboard(label, name) {
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  let pubkey = "";
  page.on("websocket", (ws) => {
    ws.on("framesent", (frame) => {
      try {
        const m = JSON.parse(String(frame.payload));
        if ((m[0] === "AUTH" || m[0] === "EVENT") && !pubkey) {
          pubkey = m[1]?.pubkey ?? "";
        }
        if (m[0] === "EVENT" && m[1]?.kind !== 22242) {
          console.log(
            `[${label}] -> EVENT kind=${m[1]?.kind} tags=${JSON.stringify(m[1]?.tags ?? []).slice(0, 160)}`,
          );
        }
      } catch {
        /* not JSON */
      }
    });
    ws.on("framereceived", (frame) => {
      try {
        const m = JSON.parse(String(frame.payload));
        if (m[0] === "OK" && m[2] === false) {
          console.log(`[${label}] <= OK:false ${m[3]}`);
        }
        if (m[0] === "CLOSED") console.log(`[${label}] <= CLOSED ${m[2]}`);
        if (m[0] === "NOTICE") {
          console.log(`[${label}] <= NOTICE ${JSON.stringify(m[1]).slice(0, 200)}`);
        }
        if (m[0] === "EVENT" && (m[2]?.kind === 44100 || m[2]?.kind === 44101)) {
          console.log(
            `[${label}] <= NOTICE-KIND ${m[2].kind} tags=${JSON.stringify(m[2].tags).slice(0, 200)}`,
          );
        }
      } catch {
        /* not JSON */
      }
    });
  });
  page.on("console", (m) => {
    if (m.type() === "error" || m.type() === "warning") {
      console.log(`[${label}.console] ${m.text().slice(0, 260)}`);
    }
  });
  page.on("pageerror", (e) => console.log(`[${label}.pageerror] ${e.message.slice(0, 260)}`));
  page.on("response", async (r) => {
    if (r.url().includes("/buzz/") && r.status() >= 400) {
      console.log(
        `[${label}.http] ${r.status()} ${new URL(r.url()).pathname} :: ${(await r.text().catch(() => "")).slice(0, 200)}`,
      );
    }
  });

  await page.goto(UI, { waitUntil: "domcontentloaded" });
  await page.getByRole("button", { name: /create a new identity key/i }).click({ timeout: 45_000 });
  await page.getByRole("button", { name: /^Next$/ }).click({ timeout: 45_000 });
  await page.getByRole("button", { name: /skip for now/i }).first().click({ timeout: 45_000 });
  await page.getByRole("button", { name: /join a community/i }).first().click({ timeout: 45_000 });
  await page.locator("input:visible").first().fill(RELAY_WS);
  await page.getByRole("button", { name: /^Next$/ }).first().click({ timeout: 45_000 });
  await page.getByPlaceholder(/username/i).fill(name, { timeout: 45_000 });
  await page.getByRole("button", { name: /^Next$/ }).first().click({ timeout: 45_000 });
  await page.getByRole("button", { name: /take me to buzz|skip for now/i }).first().click({ timeout: 90_000 });
  await page.getByTestId("app-sidebar").waitFor({ state: "visible", timeout: 90_000 });
  await page.waitForFunction(
    () => !document.body.innerText.includes("Meet your starter team"),
    null,
    { timeout: 45_000 },
  ).catch(() => {});
  console.log(`[${label}] onboarded pubkey=${pubkey}`);
  return { page, pubkey };
}

const ana = await onboard("ana", `Probe Ana ${stamp}`);
const bo = await onboard("bo", `Probe Bo ${stamp}`);

// Ana creates a private room.
const room = `probe-add-${stamp}`;
await ana.page.getByTestId("section-actions-channels-quick-create").click();
await ana.page.getByRole("button", { name: /create a new channel/i }).click();
await ana.page.getByRole("textbox").first().fill(room);
const trigger = ana.page.getByTestId("create-channel-permissions");
const option = ana.page.getByTestId("create-channel-permissions-option-private");
for (let attempt = 0; attempt < 3; attempt += 1) {
  await trigger.click();
  if (await option.isVisible().catch(() => false)) break;
  await ana.page.waitForTimeout(500);
}
await option.click({ timeout: 20_000, force: true });
await ana.page.getByTestId("create-channel-submit").click();
await ana.page.getByTestId("app-sidebar").getByText(room).first().waitFor({ timeout: 45_000 });
console.log(`--- room ${room} created ---`);

await ana.page.getByTestId("app-sidebar").getByText(room).first().click();
await ana.page.getByTestId("message-composer").waitFor({ timeout: 45_000 });

// Open the members sidebar and add Bo.
await ana.page.getByTestId("channel-members-trigger").first().click();
await ana.page.getByTestId("members-sidebar").waitFor({ timeout: 30_000 });
await ana.page.getByTestId("channel-management-search-users").fill(bo.pubkey);
const row = ana.page.getByTestId(`channel-user-search-result-${bo.pubkey}`);
await row.waitFor({ timeout: 45_000 });
console.log("--- clicking Add ---");
await row.getByRole("button", { name: "Add" }).click();

for (let i = 1; i <= 12; i += 1) {
  await ana.page.waitForTimeout(5_000);
  const addDisabled = await row
    .getByRole("button", { name: "Add" })
    .isDisabled()
    .catch(() => "gone");
  const boSidebar = await bo.page
    .getByTestId("app-sidebar")
    .innerText()
    .catch(() => "");
  console.log(
    `t+${i * 5}s add_button_disabled=${addDisabled} bo_sees_room=${boSidebar.includes(room)}`,
  );
  if (boSidebar.includes(room)) {
    console.log("PROOF: the room reached Bo");
    break;
  }
}
console.log(
  "ana members sidebar:",
  (await ana.page.getByTestId("members-sidebar").innerText().catch(() => "")).replace(/\s+/g, " ").slice(0, 400),
);
await browser.close();
