/**
 * Identity and private conversation, driven through buzz's own client against a
 * LIVE relay.
 *
 * `company-day.spec.ts` covers the public-channel day. This file covers the two
 * things that day leaves untested: what one person can learn ABOUT another
 * (published profile, published status, presence) and the private rooms they
 * open together — including GROUP DMs, which the relay bounds at 2..9
 * participants (`channel.rs`) and which no existing scenario exercised.
 *
 * Same rules as the rest of the suite, for the same reasons:
 *   - every actor is their own browser context with their own identity key;
 *   - people and content are STAMPED, because the shared dev relay accumulates
 *     identities across runs and never forgets one;
 *   - colleagues are resolved by PUBKEY, never by display name — a name search
 *     with `.first()` picks whichever stale same-named identity the relay
 *     happens to return first.
 *
 *   BUZZ_UI=http://localhost:4173/buzz/ui/index.html \
 *   BUZZ_WS=ws://localhost:4173/buzz/ \
 *   npx playwright test e2e/identity-and-dms.spec.ts
 */
import { test, expect } from "@playwright/test";
import { messages, onboard, openChannel, say, type Person } from "./onboarding";

const stamp = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
const ANA = `Ana Ruiz ${stamp}`;
const BO = `Bo Nilsson ${stamp}`;
const CY = `Cy Okafor ${stamp}`;

/** Report what the page complained about, whether or not the scenario passed. */
function report(...people: Person[]) {
  for (const person of people) {
    if (person.problems.length) {
      console.log(`--- ${person.name} ---\n${[...new Set(person.problems)].join("\n")}`);
    }
  }
}

/** Add one recipient to the new-message picker, by pubkey. */
async function addRecipient(person: Person, recipient: Person) {
  // `#new-dm-search` is an id, not a testid — the picker clears and refocuses
  // it after each selection, so it is refilled per recipient rather than typed
  // once. Searching by pubkey is deliberate (see the file header).
  await person.page.locator("#new-dm-search").fill(recipient.pubkey);
  await person.page
    .getByTestId(`new-dm-result-${recipient.pubkey}`)
    .click({ timeout: 30_000 });
  // The chip is the picker's own confirmation that the recipient was accepted.
  // Without this the next fill can race the selection and silently drop one
  // participant — which would turn a group DM into a 1:1 and still "pass".
  await expect(
    person.page.getByTestId(`new-dm-selected-${recipient.pubkey}`),
  ).toBeVisible({ timeout: 30_000 });
}

/**
 * A DM room is keyed by a hash of its participant set, so a group DM is a
 * DIFFERENT room from either 1:1 — this proves the three-way room exists, that
 * both named recipients are actually in it, and that neither was silently
 * dropped by the picker.
 */
test("a group DM reaches every recipient it names", async ({ browser }) => {
  test.setTimeout(300_000);
  const ana = await onboard(browser, ANA);
  const bo = await onboard(browser, BO);
  const cy = await onboard(browser, CY);

  // Everyone speaks in General first: the picker resolves recipients from the
  // relay's kind-0 profile reads, and a just-created identity has to have
  // reached the relay before it can be found.
  await openChannel(ana, "General");
  await openChannel(bo, "General");
  await openChannel(cy, "General");
  await say(bo, `group dm setup ${stamp} bo`);
  await say(cy, `group dm setup ${stamp} cy`);
  await expect(
    messages(ana.page).filter({ hasText: `group dm setup ${stamp} cy` }),
  ).toBeAttached({ timeout: 60_000 });

  await ana.page.getByTestId("section-actions-dms-quick-create").click();
  await expect(ana.page.getByTestId("new-message-page")).toBeVisible({
    timeout: 45_000,
  });
  await addRecipient(ana, bo);
  await addRecipient(ana, cy);

  const line = `three-way ${stamp}: who is on call for the rollback?`;
  await say(ana, line);
  await expect(messages(ana.page).filter({ hasText: line })).toBeVisible({
    timeout: 45_000,
  });

  // Both recipients, independently. A message row only renders for the OPEN
  // conversation, so each has to open the room first — asserting the row
  // before opening it times out no matter what the relay did.
  for (const person of [bo, cy]) {
    const dmList = person.page.getByTestId("dm-list");
    await expect(dmList).toBeVisible({ timeout: 90_000 });
    await dmList.locator("li").first().click();
    await expect(messages(person.page).filter({ hasText: line })).toBeAttached({
      timeout: 90_000,
    });
  }

  report(ana, bo, cy);
  await ana.ctx.close();
  await bo.ctx.close();
  await cy.ctx.close();
});

/**
 * A published status (kind 30315) is meant to answer "what is this person doing"
 * for everyone else, so the receipt that matters is a DIFFERENT person reading
 * it off the profile — not the author seeing their own optimistic echo.
 */
test("a published status reaches the colleague who looks the author up", async ({
  browser,
}) => {
  test.setTimeout(300_000);
  const ana = await onboard(browser, ANA);
  const bo = await onboard(browser, BO);
  await openChannel(ana, "General");
  await openChannel(bo, "General");

  // Ana has to have SAID something for Bo to have an author to click. This is
  // also how a person actually reaches a colleague's profile in buzz.
  const line = `status setup ${stamp}: shipping the rollback runbook`;
  await say(ana, line);

  const statusText = `heads down on ${stamp}`;
  await ana.page.getByTestId("sidebar-profile-avatar-button").click();
  await expect(ana.page.getByTestId("profile-popover")).toBeVisible({
    timeout: 45_000,
  });
  await ana.page.getByTestId("profile-popover-set-status").click();
  await expect(ana.page.getByTestId("set-status-dialog")).toBeVisible({
    timeout: 45_000,
  });
  await ana.page.getByTestId("set-status-input").fill(statusText);
  await ana.page.getByTestId("set-status-save").click();
  // The dialog closing is the client's own acknowledgement that the publish was
  // accepted; asserting it keeps a failed publish from being read as a slow one.
  await expect(ana.page.getByTestId("set-status-dialog")).toBeHidden({
    timeout: 45_000,
  });
  await expect(ana.page.getByTestId("sidebar-profile-user-status")).toContainText(
    statusText,
    { timeout: 60_000 },
  );

  // Bo's side: find Ana's message, open its author, read the status. This can
  // only have come from Ana's 30315 crossing the relay.
  const anaRow = messages(bo.page).filter({ hasText: line }).first();
  await expect(anaRow).toBeAttached({ timeout: 90_000 });
  await anaRow.scrollIntoViewIfNeeded();
  await anaRow.getByTestId("message-author").first().click();
  // The synced client opens the author's PROFILE PANEL here (the older build
  // opened a popover with `user-profile-status`). Either surface is accepted, and
  // either can only show this text from Ana's 30315 read back off the relay.
  await expect(
    bo.page
      .getByTestId("user-profile-status")
      .or(bo.page.getByTestId("user-profile-scroll-body"))
      .filter({ hasText: statusText })
      .first(),
  ).toBeVisible({ timeout: 90_000 });

  report(ana, bo);
  await ana.ctx.close();
  await bo.ctx.close();
});
