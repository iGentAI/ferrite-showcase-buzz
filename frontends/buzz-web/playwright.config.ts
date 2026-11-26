import { defineConfig, devices } from "@playwright/test";
// Installs the SSO session on the test process's own relay requests when
// BUZZ_SSO_ORIGIN is set; a no-op otherwise. Loaded here so every worker has it,
// whether or not its spec imports e2e/onboarding.ts.
import "./e2e/sso";

/** Live scenarios only — the app under test is the DEPLOYED relay's bundle. */
export default defineConfig({
  testDir: "./e2e",
  // A LIVE-relay budget: scenario 8's recipient-side DM materialization
  // measured 2.1 minutes end to end on 2026-07-28 (the latency itself is a
  // recorded product issue), and a 90s budget reported that as a hard
  // failure. Scenario 9 (the agent turn) sets its own 300s budget in-test.
  timeout: 240_000,
  expect: { timeout: 20_000 },
  // Scenarios are independent once display names are stamped per worker
  // (company-day.spec.ts): every person is a fresh identity key, so the
  // relay's per-author rate limits share no budget; starter-room creation
  // treats 409 as "someone else won the race"; and scenario 7's negative is
  // scoped to its own stamped private room and enforced relay-side by
  // access.rs::may_deliver — so nothing needs serial pinning for
  // CORRECTNESS. Wall-clock is another matter: 4 workers is up to 8 live
  // browsers plus the relay round trips on one machine, and a full 2026-07-28
  // run measured 7 of 9 scenarios timing out at 4 workers while the same
  // scenarios pass serially. Parallelism is therefore opt-in
  // (`BUZZ_WORKERS=4`), and the default is the mode that reports product
  // truth rather than machine load.
  fullyParallel: true,
  workers: Number(process.env.BUZZ_WORKERS ?? "1"),
  // Deliberately no retries: on a live relay a retry hides real flakes
  // (NIP-98 replay refusals, delivery timeouts) that must stay visible.
  retries: 0,
  reporter: [["list"]],
  // A run deletes what it created (rooms, projects, teammates), with the
  // operator key; see e2e/teardown.ts. BUZZ_KEEP_CREATED=1 keeps them.
  globalTeardown: "./e2e/teardown.ts",
  use: {
    baseURL: process.env.BUZZ_ORIGIN ?? "http://127.0.0.1:28443",
    // For specs that use the built-in `page`/`context` fixtures on a property
    // behind the Ferrite SSO broker: a storageState file carrying a redeemed
    // session (e2e/ssoSession.mjs `storageState`). Contexts made
    // through e2e/onboarding.ts mint their own per-person session instead.
    storageState: process.env.BUZZ_STORAGE_STATE || undefined,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "off",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
