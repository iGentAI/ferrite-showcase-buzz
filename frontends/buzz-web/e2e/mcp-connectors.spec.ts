/**
 * The MCP connector settings surface, in a real browser against the deployment.
 *
 * This exists because a vision-driven walk could not settle whether the section
 * was there. It spent fifty-eight turns in the app and reported "Connectors" as
 * absent, while the shipped bundle contained `mcp-connectors`, the settings nav
 * filter special-cased nothing relevant, and the built asset postdated the source.
 * It had missed an entry in a scrollable list.
 *
 * That is the same failure the model picker produced: repeated visual attempts,
 * three different mechanical dead ends, no verdict. A testid assertion separates
 * "the section is not rendered" from "I did not find it", and only the first is a
 * defect.
 *
 * The identity here is freshly onboarded, so this is the MEMBER path — the half
 * worth holding in a browser, because it is where an incorrect answer is actively
 * harmful: a Connect button that looks available and then refuses.
 */
import { test, expect } from "@playwright/test";

import { onboard, type Person } from "./onboarding";

/**
 * Open Settings and land on a named section.
 *
 * A CONTROL section is opened and asserted first, deliberately. If the settings
 * surface itself is broken, this fails on Appearance — which cannot be confused
 * with the connector card being broken. Without that step, every settings defect
 * in the app would present as a failure of the feature under test.
 */
async function openSettingsSection(person: Person, section: string) {
  const { page } = person;
  await page.getByTestId("open-settings").click({ timeout: 45_000 });
  await page.getByTestId("profile-popover-settings").click({ timeout: 30_000 });
  await expect(page.getByTestId("settings-view")).toBeVisible({ timeout: 45_000 });

  // The control: a section that has worked for a long time.
  await page.getByTestId("settings-nav-appearance").click();
  await expect(page.getByTestId("settings-theme")).toBeVisible({ timeout: 30_000 });

  const nav = page.getByTestId(`settings-nav-${section}`);
  await expect(
    nav,
    `the settings nav carries no ${section} entry — the section is registered in ` +
      `SettingsPanels.tsx, so its absence here means the registration did not reach ` +
      `the served bundle`,
  ).toBeVisible({ timeout: 30_000 });
  await nav.click();
}

test("a member reaches the connector catalog and is offered no control that would refuse", async ({
  browser,
}) => {
  const ana = await onboard(browser, "Connector Reader");

  await openSettingsSection(ana, "mcp-connectors");

  const card = ana.page.getByTestId("settings-mcp-connectors");
  await expect(card).toBeVisible({ timeout: 30_000 });

  // Rows come from the RELAY, so this also proves the signed GET works from a tab.
  // `apiGet` sends no Authorization header, and this route is members-only — a
  // client using the wrong helper gets a 401 with nothing at the call site to
  // explain it, which is a trap this codebase has already fallen into once.
  const linear = ana.page.getByTestId("mcp-connector-linear");
  await expect(
    linear,
    "the catalog rendered no Linear row, so the relay read failed or returned nothing",
  ).toBeVisible({ timeout: 45_000 });
  await expect(ana.page.getByTestId("mcp-status-linear")).toContainText(
    /connected|not connected|not granted/,
  );

  // A freshly onboarded person is not an operator. The banner says so...
  await expect(
    ana.page.getByTestId("mcp-not-operator"),
    "a member must be told why they cannot connect, not left to find out",
  ).toBeVisible();

  // ...and the control is genuinely disabled rather than merely styled that way.
  // A button that only LOOKS disabled would send a request the relay answers 403,
  // which is precisely what the catalog's `operator` field exists to prevent.
  const connect = ana.page.getByTestId("mcp-connect-linear");
  await expect(connect).toBeVisible();
  await expect(
    connect,
    "an enabled Connect button for a member is a control that discovers its own 403",
  ).toBeDisabled();

  // A provider that cannot work from a deployed app is listed WITH its reason
  // rather than dropped, so "where is Figma?" has an answer on the screen.
  const figma = ana.page.getByTestId("mcp-unavailable-figma");
  await expect(figma).toBeVisible();
  await expect(figma).toContainText(/127\.0\.0\.1|local/i);

  // Every page error, console warning or error, and failed relay request
  // counts. Nothing is excluded: the browser seam now answers the deep-link
  // queue commands inertly, the local-archive hooks do not run in a build
  // without an archive, and the relay serves the GIF picker's `/buzz/info`
  // capability probe, so a member's path through settings logs nothing.
  const unexpected = ana.problems;
  expect(unexpected, unexpected.join("\n")).toEqual([]);
});
