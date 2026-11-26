/**
 * Diagnostic for onboarding's last gate, in the REAL client.
 *
 * The protocol-level probe (`probe-join.mjs`) shows a 9021 join landing in the
 * roster within a second, so when the browser still reports "Starter channels
 * were not available after setup" the failure is in what the client's own
 * subscriptions get back. This drives onboarding and prints every relay frame
 * that carries a refusal (CLOSED / NOTICE / OK:false) plus the console errors,
 * which is what a Playwright failure screenshot cannot show.
 *
 *   node e2e/probe-onboard.mjs
 *
 * Not a Playwright test (`.mjs`, no `.spec.`), so the suite never collects it.
 */
import { chromium } from "@playwright/test";

const UI = process.env.BUZZ_UI ?? "http://localhost:4173/buzz/ui/index.html";
const RELAY_WS = process.env.BUZZ_WS ?? "ws://localhost:4173/buzz/";
const name = `Probe ${Date.now().toString(36)}`;

const browser = await chromium.launch();
const ctx = await browser.newContext();
const page = await ctx.newPage();

const reqCounts = new Map();
const eventCounts = new Map();
const pendingHttp = new Map();
page.on("request", (request) => {
  if (!request.url().includes("/buzz/api/")) return;
  pendingHttp.set(request, performance.now());
  console.log(`=> HTTP ${request.method()} ${new URL(request.url()).pathname}`);
});
page.on("response", (response) => {
  const request = response.request();
  const startedAt = pendingHttp.get(request);
  if (startedAt === undefined) return;
  pendingHttp.delete(request);
  console.log(
    `<= HTTP ${response.status()} ${request.method()} ${new URL(request.url()).pathname} ${Math.round(performance.now() - startedAt)}ms`,
  );
});
page.on("requestfailed", (request) => {
  const startedAt = pendingHttp.get(request);
  if (startedAt === undefined) return;
  pendingHttp.delete(request);
  console.log(
    `xx HTTP ${request.method()} ${new URL(request.url()).pathname} ${request.failure()?.errorText ?? "failed"}`,
  );
});
page.on("websocket", (ws) => {
  ws.on("framesent", (frame) => {
    try {
      const m = JSON.parse(String(frame.payload));
      if (m[0] === "REQ") {
        reqCounts.set(m[1], JSON.stringify(m.slice(2)).slice(0, 200));
        console.log(`-> REQ ${m[1]} ${JSON.stringify(m.slice(2)).slice(0, 200)}`);
      }
      if (m[0] === "CLOSE") console.log(`-> CLOSE ${m[1]}`);
    } catch {
      /* not JSON */
    }
  });
  ws.on("framereceived", (frame) => {
    try {
      const m = JSON.parse(String(frame.payload));
      if (m[0] === "CLOSED") {
        console.log(
          `<= CLOSED ${m[1]} :: ${m[2]} :: for filter ${reqCounts.get(m[1]) ?? "?"}`,
        );
      }
      if (m[0] === "NOTICE") console.log(`<= NOTICE ${JSON.stringify(m[1]).slice(0, 240)}`);
      if (m[0] === "OK" && m[2] === false) {
        console.log(`<= OK:false ${m[1].slice(0, 12)} :: ${m[3]}`);
      }
      if (m[0] === "EVENT") {
        const seen = eventCounts.get(m[1]) ?? { n: 0, ds: [] };
        seen.n += 1;
        const d = m[2]?.tags?.find((t) => t[0] === "d")?.[1];
        if (d) seen.ds.push(d);
        eventCounts.set(m[1], seen);
      }
      if (m[0] === "EOSE") {
        const seen = eventCounts.get(m[1]) ?? { n: 0, ds: [] };
        const starters = ["general", "welcome-everyone"].filter((s) =>
          seen.ds.includes(s),
        );
        console.log(
          `<= EOSE ${m[1]} events=${seen.n} starters_present=[${starters.join(",")}]`,
        );
      }
    } catch {
      /* not JSON */
    }
  });
});
page.on("console", (m) => {
  if (
    m.type() === "error" ||
    m.type() === "warning" ||
    m.text().startsWith("[invoke:")
  ) {
    console.log(`[console.${m.type()}] ${m.text().slice(0, 300)}`);
  }
});
page.on("pageerror", (e) => console.log(`[pageerror] ${e.message.slice(0, 300)}`));

await page.goto(UI, { waitUntil: "domcontentloaded" });
await page.evaluate(() => {
  const internals = window.__TAURI_INTERNALS__;
  if (!internals?.invoke) return;
  const original = internals.invoke.bind(internals);
  internals.invoke = async (command, args, options) => {
    const startedAt = performance.now();
    console.log(`[invoke:start] ${command}`);
    try {
      const result = await original(command, args, options);
      console.log(
        `[invoke:done] ${command} ${Math.round(performance.now() - startedAt)}ms`,
      );
      return result;
    } catch (error) {
      console.warn(
        `[invoke:error] ${command} ${Math.round(performance.now() - startedAt)}ms ${String(error)}`,
      );
      throw error;
    }
  };
});
await page.getByRole("button", { name: /create a new identity key/i }).click({ timeout: 45_000 });
await page.getByRole("button", { name: /^Next$/ }).click({ timeout: 45_000 });
await page.getByRole("button", { name: /skip for now/i }).first().click({ timeout: 45_000 });
await page.getByRole("button", { name: /join a community/i }).first().click({ timeout: 45_000 });
await page.locator("input:visible").first().fill(RELAY_WS);
await page.getByRole("button", { name: /^Next$/ }).first().click({ timeout: 45_000 });
await page.getByPlaceholder(/username/i).fill(name, { timeout: 45_000 });
await page.getByRole("button", { name: /^Next$/ }).first().click({ timeout: 45_000 });
console.log("--- at the starter-team card ---");
await page
  .getByRole("button", { name: /take me to buzz|skip for now/i })
  .first()
  .click({ timeout: 90_000 });
console.log("--- clicked enter; waiting on the sidebar ---");
const ok = await page
  .getByTestId("app-sidebar")
  .waitFor({ state: "visible", timeout: 60_000 })
  .then(() => true)
  .catch(() => false);
console.log(`sidebar visible: ${ok}`);
console.log(
  "page text:",
  (await page.locator("body").innerText()).replace(/\s+/g, " ").slice(0, 400),
);
console.log(
  "onboarding transaction:",
  await page.evaluate(() =>
    localStorage.getItem("buzz-community-onboarding-transaction.v1"),
  ),
);
console.log(
  "onboarding marks:",
  await page.evaluate(() =>
    performance
      .getEntriesByType("mark")
      .filter((entry) => entry.name.startsWith("buzz:onboarding:"))
      .map((entry) => `${entry.name}@${Math.round(entry.startTime)}ms`),
  ),
);
for (const [request, startedAt] of pendingHttp) {
  console.log(
    `!! HTTP STILL PENDING ${request.method()} ${new URL(request.url()).pathname} ${Math.round(performance.now() - startedAt)}ms`,
  );
}
await browser.close();
process.exit(ok ? 0 : 1);
