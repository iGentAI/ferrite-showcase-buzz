/**
 * LIVE end-to-end scenarios against the relay's EMBEDDED BOOTSTRAP UI.
 *
 * These do not mock anything: the app under test is the bundle the relay seeds
 * into its `web-assets` blob on first boot (`buzz/src/relay/assets/ui`), served
 * by the real gateway's content route, and every request it makes is a real
 * NIP-98-signed call to the deployed app over the real fleet.
 *
 *   BUZZ_EMBEDDED_UI=1 BUZZ_ORIGIN=http://<app-host>:28443 \
 *     npx playwright test e2e/live.spec.ts
 *
 * ── WHY THIS IS GATED ──────────────────────────────────────────────────
 *
 * The relay serves ONE UI at `/buzz/ui/`, and the two candidates are mutually
 * exclusive. `seed_ui_assets` seeds this embedded bootstrap only until a real
 * bundle is pushed through the operator asset routes; from then on the pushed
 * manifest is authoritative and the embedded bytes are inert. The standard
 * verification (`push-ui.mjs`) pushes the full buzz-web SPA, whose testids are
 * an entirely different set — so with the SPA deployed, every `getByTestId` in
 * this file resolves to nothing.
 *
 * That is not this file being stale: `app-header`, `channel-item`,
 * `composer-input`, `relay-health` and the rest are all present in
 * `buzz/src/relay/static/ui/`, and absent from the SPA; the passing specs'
 * `app-sidebar` / `message-author` are the reverse. The two suites test two
 * bundles.
 *
 * So the whole file skips unless `BUZZ_EMBEDDED_UI=1` declares that the
 * deployment under test is serving the bootstrap. Run it against a relay that
 * has had no bundle pushed; the SPA scenarios live in `company-day.spec.ts`,
 * `channel-lifecycle.spec.ts` and their siblings. Skipping loudly rather than
 * failing silently matters here because the file is serial — a single stale
 * first test would otherwise report the other nine as "did not run" and take
 * their coverage down without a word.
 *
 * Each scenario mimics a distinct real user. A NIP-07 signer is injected into
 * the page (an init script) exactly as a browser extension would provide it,
 * which is how a real user with Alby/nos2x signs — the app's own signer code
 * path is unchanged.
 */
import { test, expect, type Page } from "@playwright/test";
import { schnorr } from "@noble/curves/secp256k1.js";
import { sha256 } from "@noble/hashes/sha2.js";
import { bytesToHex, hexToBytes } from "@noble/hashes/utils.js";

import { tapCreations } from "./onboarding";
import { newBuzzContext } from "./sso";

const ORIGIN = process.env.BUZZ_ORIGIN ?? "http://127.0.0.1:28443";
const UI = `${ORIGIN}/buzz/ui/index.html`;

/** Console/network problems collected per test — every scenario asserts on them. */
type Problems = { console: string[]; failed: string[]; http: string[] };

function watch(page: Page): Problems {
  const p: Problems = { console: [], failed: [], http: [] };
  // What this page makes the relay create is deleted at global teardown.
  tapCreations(page);
  page.on("console", (m) => {
    if (m.type() === "error" || m.type() === "warning") {
      p.console.push(`${m.type()}: ${m.text()}`);
    }
  });
  page.on("pageerror", (e) => p.console.push(`pageerror: ${e.message}`));
  page.on("requestfailed", (r) =>
    p.failed.push(`${r.method()} ${r.url()} — ${r.failure()?.errorText}`),
  );
  page.on("response", (r) => {
    if (r.status() >= 400) p.http.push(`${r.status()} ${r.request().method()} ${r.url()}`);
  });
  return p;
}

/** Deterministic per-persona key, mirroring the Rust gates' `Actor::new`. */
function persona(label: string) {
  const sk = sha256(new TextEncoder().encode(label));
  return { skHex: bytesToHex(sk), pkHex: bytesToHex(schnorr.getPublicKey(sk)) };
}

/** Install a NIP-07 provider before any app code runs.
 *
 * The page gets a faithful `window.nostr`; the actual BIP-340 signing happens
 * in Node (same curve the relay verifies) and is reached through an exposed
 * binding, so the shim needs no network and cannot drift from the relay's
 * crypto. This is exactly the shape a real extension presents to the app, so
 * the app's own signer path runs unmodified.
 */
async function installSigner(page: Page, label: string) {
  const { skHex, pkHex } = persona(label);
  await page.exposeFunction(
    "__signNostr",
    (ev: { kind: number; created_at: number; tags: string[][]; content: string }) => {
      const canonical = JSON.stringify([0, pkHex, ev.created_at, ev.kind, ev.tags, ev.content]);
      const id = sha256(new TextEncoder().encode(canonical));
      const sig = schnorr.sign(id, hexToBytes(skHex));
      return { ...ev, id: bytesToHex(id), pubkey: pkHex, sig: bytesToHex(sig) };
    },
  );
  await page.addInitScript((pk: string) => {
    (window as unknown as { nostr: unknown }).nostr = {
      async getPublicKey() {
        return pk;
      },
      async signEvent(ev: unknown) {
        return (window as unknown as { __signNostr: (e: unknown) => Promise<unknown> }).__signNostr(
          ev,
        );
      },
    };
  }, pkHex);
  return { skHex, pkHex };
}

/** Fail the scenario on any console error, request failure, or 5xx. */
function assertClean(p: Problems, allowHttp: RegExp[] = [], allowConsole: RegExp[] = []) {
  // Report HTTP context first: a bare "500" console line is useless without
  // knowing which call produced it.
  const unexpectedFirst = p.http.filter((h) => !allowHttp.some((re) => re.test(h)));
  expect(unexpectedFirst, `HTTP errors: ${unexpectedFirst.join(" | ")}`).toEqual([]);
  const consoleLeft = p.console.filter((c) => !allowConsole.some((re) => re.test(c)));
  expect(consoleLeft, `console errors: ${consoleLeft.join(" | ")}`).toEqual([]);
  expect(p.failed, `failed requests: ${p.failed.join(" | ")}`).toEqual([]);
  const unexpected = p.http.filter((h) => !allowHttp.some((re) => re.test(h)));
  expect(unexpected, `unexpected HTTP errors: ${unexpected.join(" | ")}`).toEqual([]);
}

test.describe.configure({ mode: "serial" });

/// This file targets the relay's EMBEDDED bootstrap UI, which the pushed SPA
/// supersedes — see the header. Opt in only against a deployment that serves it.
const RUN_EMBEDDED_UI = process.env.BUZZ_EMBEDDED_UI === "1";
test.beforeEach(() => {
  test.skip(
    !RUN_EMBEDDED_UI,
    "live.spec tests the relay's embedded bootstrap UI; the verification profile " +
      "serves the pushed buzz-web SPA instead. Set BUZZ_EMBEDDED_UI=1 to run it " +
      "against a relay with no bundle pushed.",
  );
});

/**
 * The channel every scenario works in. It is created once (scenario 2) and
 * re-added by id in later scenarios because the relay exposes NO
 * list-channels route: the sidebar is this browser's localStorage, so a fresh
 * context genuinely starts with an empty list — exactly what a real user on a
 * new device sees, and what the UI says in place.
 */
const CHANNEL = `room-${Date.now().toString(36)}`;

/** Add CHANNEL to this browser's list (if absent) and select it. */
async function openChannel(page: Page) {
  const item = page.getByTestId("channel-item").filter({ hasText: CHANNEL });
  if ((await item.count()) === 0) {
    await page.getByTestId("channel-join-id").fill(CHANNEL);
    await page.getByTestId("channel-join-submit").click();
  }
  await item.first().click();
}

test("1. a first-time visitor lands on the app and sees the live relay", async ({ page }) => {
  const p = watch(page);
  await installSigner(page, "browser-ana");
  await page.goto(UI);
  await expect(page.getByTestId("app-header")).toBeVisible();
  // The landing page reads GET /relay-info off the deployed relay.
  await expect(page.getByTestId("relay-info")).toContainText("buzz", { timeout: 15_000 });
  // Assert the semantic state, not the copy.
  await expect(page.getByTestId("relay-health")).toHaveAttribute("data-state", "ok");
  assertClean(p);
});

test("2. a user creates a channel and posts the first message", async ({ page }) => {
  const p = watch(page);
  await installSigner(page, "browser-ana");
  await page.goto(`${UI}#/w`);
  await expect(page.getByTestId("workspace")).toBeVisible({ timeout: 15_000 });

  await page.getByTestId("channel-create-id").fill(CHANNEL);
  // Name it after the id so the list row is unambiguous for the assertion.
  await page.getByTestId("channel-create-name").fill(CHANNEL);
  await page.getByTestId("channel-create-submit").click();
  await expect(page.getByTestId("channel-item").filter({ hasText: CHANNEL })).toBeVisible({
    timeout: 15_000,
  });

  await page.getByTestId("composer-input").fill("hello from a real browser");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("message-content").first()).toContainText(
    "hello from a real browser",
    { timeout: 20_000 },
  );
  assertClean(p);
});

test("3. a user replies in-thread and the thread summary appears", async ({ page }) => {
  const p = watch(page);
  await installSigner(page, "browser-ana");
  await page.goto(`${UI}#/w`);
  await openChannel(page);
  await expect(page.getByTestId("message-row").first()).toBeVisible({ timeout: 20_000 });

  await page.getByTestId("message-reply").first().click();
  await expect(page.getByTestId("reply-banner")).toBeVisible();
  await page.getByTestId("composer-input").fill("threaded reply from the browser");
  await page.getByTestId("composer-send").click();
  await expect(page.getByTestId("thread-summary").first()).toBeVisible({ timeout: 20_000 });
  assertClean(p);
});

test("4. a user reacts to a message and the tally updates", async ({ page }) => {
  const p = watch(page);
  await installSigner(page, "browser-ana");
  await page.goto(`${UI}#/w`);
  await openChannel(page);
  await expect(page.getByTestId("message-row").first()).toBeVisible({ timeout: 20_000 });
  await page.getByTestId("react-button").first().click();
  await expect(page.getByTestId("reaction-tally").first()).toBeVisible({ timeout: 20_000 });
  assertClean(p);
});

test("5. a second user sees the first user's message live over SSE", async ({ browser }) => {
  // Signed in at the SSO broker when the property sits behind it (a no-op
  // otherwise): each person their own broker subject.
  const ana = await newBuzzContext(browser, "live-ana");
  const bo = await newBuzzContext(browser, "live-bo");
  const anaPage = await ana.newPage();
  const boPage = await bo.newPage();
  const p = watch(boPage);
  await installSigner(anaPage, "browser-ana");
  await installSigner(boPage, "browser-bo");

  await boPage.goto(`${UI}#/w`);
  await openChannel(boPage);
  await expect(boPage.getByTestId("live-indicator")).toBeVisible({ timeout: 25_000 });

  const marker = `live-${Date.now().toString(36)}`;
  await anaPage.goto(`${UI}#/w`);
  await openChannel(anaPage);
  await anaPage.getByTestId("composer-input").fill(marker);
  await anaPage.getByTestId("composer-send").click();

  // bo never reloads: the message must arrive through the relay's SSE pump.
  await expect(boPage.getByTestId("message-content").filter({ hasText: marker })).toBeVisible({
    timeout: 45_000,
  });
  assertClean(p);
  await ana.close();
  await bo.close();
});

test("6. the home feed shows activity and switches categories", async ({ page }) => {
  const p = watch(page);
  await installSigner(page, "browser-ana");
  await page.goto(`${UI}#/w`);
  await openChannel(page);

  await page.getByTestId("feed-tab-activity").click();
  await expect(page.getByTestId("feed-row").first()).toBeVisible({ timeout: 25_000 });
  // mentions/needs_action are legitimately empty for this persona — an empty
  // state is a pass, a crash or an error banner is not.
  await page.getByTestId("feed-tab-mentions").click();
  await page.waitForTimeout(1500);
  await page.getByTestId("feed-tab-needs_action").click();
  await page.waitForTimeout(1500);
  assertClean(p);
});

test("7. a mention reaches the mentioned user's feed", async ({ browser }) => {
  const ana = await newBuzzContext(browser, "live-ana");
  const boPage = await (await newBuzzContext(browser, "live-bo")).newPage();
  const anaPage = await ana.newPage();
  const p = watch(boPage);
  await installSigner(anaPage, "browser-ana");
  const bo = await installSigner(boPage, "browser-bo");

  await anaPage.goto(`${UI}#/w`);
  await openChannel(anaPage);
  // Add bo to the room, then mention bo by pubkey.
  await anaPage.getByTestId("member-add-input").fill(bo.pkHex);
  await anaPage.getByTestId("member-add-submit").click();
  const marker = `mention-${Date.now().toString(36)}`;
  await anaPage.getByTestId("composer-input").fill(`@${bo.pkHex} ${marker}`);
  await anaPage.getByTestId("composer-send").click();
  await anaPage.waitForTimeout(3000);

  await boPage.goto(`${UI}#/w`);
  await openChannel(boPage);
  await boPage.getByTestId("feed-tab-mentions").click();
  await expect(boPage.getByTestId("feed-row").filter({ hasText: marker })).toBeVisible({
    timeout: 30_000,
  });
  assertClean(p);
});

test("8. the admin console reads the live relay and refuses moderation to a non-moderator", async ({
  page,
}) => {
  const p = watch(page);
  await installSigner(page, "browser-ana");
  await page.goto(`${UI}#/admin`);
  await expect(page.getByTestId("admin-relay-info")).toBeVisible({ timeout: 20_000 });
  await expect(page.getByTestId("admin-metrics")).toBeVisible({ timeout: 20_000 });
  await page.getByTestId("admin-metrics-refresh").click();

  // The relay is fail-closed: only an operator-seeded moderator may ban, so
  // this MUST be refused, and the UI must explain rather than crash.
  await page.getByTestId("admin-mod-pubkey").fill(persona("browser-bo").pkHex);
  await page.getByTestId("admin-mod-ban").click();
  await expect(page.getByTestId("admin-mod-result")).toContainText(/moderator|restricted|refus/i, {
    timeout: 20_000,
  });
  // A 403 from the ban attempt is the EXPECTED outcome here: the caller
  // authenticated and is not permitted, which the relay's `http_status` maps
  // to 403 (`error.rs`, since 2026-08-08; this line said 401 until then). The
  // browser logs a resource error for that 403; it is the refusal this
  // scenario exists to prove, not a defect.
  assertClean(p, [/403 POST .*\/buzz\/api\/events/], [/status of 403/]);
});

test("9. the invite page renders a real code and reports the relay's verdict", async ({ page }) => {
  const p = watch(page);
  await installSigner(page, "browser-cy");
  // An unminted (invalid) code must be refused CLEANLY by the relay and the
  // failure explained in the UI — the coarse `invite_invalid` path.
  await page.goto(`${UI}#/invite/not-a-real-code`);
  await expect(page.getByTestId("invite-code")).toContainText("not-a-real-code", {
    timeout: 20_000,
  });
  const join = page.getByTestId("join-button");
  if (await join.isEnabled().catch(() => false)) {
    await join.click();
    await expect(page.getByTestId("join-error")).toBeVisible({ timeout: 25_000 });
  }
  assertClean(
    p,
    [/40[13] POST .*\/buzz\/api\/invites\/claim/, /429 POST .*claim/],
    [/status of 40[13]/, /status of 429/],
  );
});

test("10. repos view states the no-git-byte-path limit instead of faking a tree", async ({
  page,
}) => {
  const p = watch(page);
  await installSigner(page, "browser-ana");
  await page.goto(`${UI}#/repos`);
  // No NIP-34 repos are announced on this deployment, so the honest empty
  // state is the pass condition.
  await expect(page.getByTestId("repos-list").or(page.getByTestId("repo-empty"))).toBeVisible({
    timeout: 20_000,
  });
  assertClean(p);
});
