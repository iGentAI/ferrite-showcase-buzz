/**
 * The model picker in the create-agent dialog.
 *
 * This exists because the picker's keyboard path shipped BROKEN and three attempts
 * to check it by eye produced three different mechanical failures and no verdict.
 * Every assertion here is about what a user observes, not about component state, so
 * each one fails against the broken build for the right reason:
 *
 * * the highlight was reset on every render, so ArrowDown appeared dead and Enter
 *   always chose the FIRST model rather than the one the user moved to;
 * * the search box re-applied focus on every render, and the symptom people actually
 *   hit was their typing landing in the instructions field and corrupting the prompt.
 *
 * See `src/features/agents/ui/personaModelComboboxState.ts` for the mechanism and its
 * unit tests; this file is the part those cannot cover — that the component is wired
 * to them correctly in a real browser against the deployed bundle.
 */
import { test, expect, type Page } from "@playwright/test";

import { onboard, type Person } from "./onboarding";

const RELAY_HTTP = process.env.BUZZ_HTTP ?? "http://localhost:4173";

/**
 * The open popover's content.
 *
 * Radix renders popover content through a portal, so it is NOT inside the dialog's
 * subtree and cannot be scoped to it. The popper wrapper is the portal's own root and
 * is the stable handle on "whatever panel is currently open".
 */
function openPanel(page: Page) {
  return page.locator("[data-radix-popper-content-wrapper]");
}

/** Walk from the workspace to a create-agent dialog with the model field showing. */
async function openAgentDialog(person: Person, name: string, instructions: string) {
  const { page } = person;

  // The agents surface. By TESTID: the sidebar's entry carries
  // `open-agents-view`, and matching on the word "Agents" instead resolves to
  // whatever else happens to render it. Then wait for the button this function is
  // about to click, rather than for some other element on the same screen — the
  // readiness signal and the next action should be the same thing.
  await page.getByTestId("open-agents-view").click();
  const newAgent = page.getByRole("button", { name: /new agent/i }).first();
  await expect(newAgent).toBeVisible({ timeout: 45_000 });
  await newAgent.click();
  const scratch = page.getByRole("menuitem", { name: /^create agent$/i });
  if (await scratch.count()) await scratch.first().click();

  const dialog = page.getByTestId("persona-dialog");
  await expect(dialog).toBeVisible({ timeout: 45_000 });

  // The name and the prompt. `textbox` order is stable: name first, instructions
  // second — and the instructions box is asserted by name below, so a reordering
  // would surface as a failed assertion rather than as a silently wrong prompt.
  await dialog.getByRole("textbox").first().fill(name);
  const prompt = dialog.locator("textarea").first();
  await prompt.fill(instructions);
  await expect(prompt).toHaveValue(instructions);

  // The model field only exists in the per-agent configuration mode.
  await dialog.getByText(/customize for this agent/i).first().click();
  await expect(page.locator("#persona-model")).toBeVisible({ timeout: 20_000 });
  return { dialog, prompt };
}

test("arrow keys choose the model they land on, not the first one", async ({
  browser,
}) => {
  const mia = await onboard(browser, "Mia Chen");
  const { page } = mia;
  await openAgentDialog(
    mia,
    `Picker Probe ${Date.now().toString(36)}`,
    "You are a probe. Post your answer and end your turn.",
  );

  const trigger = page.locator("#persona-model");
  await trigger.click();
  const panel = openPanel(page);
  await expect(panel).toBeVisible({ timeout: 20_000 });

  // The models this deployment offers, in the order the panel lists them. Read from
  // the page rather than hard-coded: the catalog is the relay's to change, and a test
  // that pins it would fail for a reason that is not this bug.
  const rows = panel.locator("button:not([disabled])");
  await expect.poll(() => rows.count(), { timeout: 20_000 }).toBeGreaterThan(2);
  const labels = (await rows.allInnerTexts()).map((text) => text.trim());

  // THE REGRESSION. Two steps down, then take it. Against the old build the
  // highlight was dragged back to the top on every render, so this selected
  // labels[0]; the whole point of the fix is that it now selects labels[2].
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Enter");

  await expect(panel).toBeHidden({ timeout: 20_000 });
  await expect(trigger).toHaveText(new RegExp(escapeForRegExp(labels[2] ?? "")), {
    timeout: 20_000,
  });
  // Stated as its own assertion so a failure reads "it chose the first row again"
  // rather than merely "the text did not match".
  expect(
    await trigger.innerText(),
    "the picker selected the first row instead of the highlighted one",
  ).not.toContain(labels[0] ?? "\u0000");

  console.log("PROBLEMS:", JSON.stringify(mia.problems, null, 1));
  await mia.ctx.close();
});

test("typing after opening filters the list and never reaches the prompt", async ({
  browser,
}) => {
  const rui = await onboard(browser, "Rui Alves");
  const { page } = rui;
  const instructions = "You are a probe. Post your answer and end your turn.";
  const { prompt } = await openAgentDialog(
    rui,
    `Focus Probe ${Date.now().toString(36)}`,
    instructions,
  );

  await page.locator("#persona-model").click();
  const panel = openPanel(page);
  await expect(panel).toBeVisible({ timeout: 20_000 });
  const before = await panel.locator("button:not([disabled])").count();
  expect(before, "the catalog should offer several models to filter").toBeGreaterThan(1);

  // Deliberately NO click first: the search box is supposed to be focused already,
  // and this is the assertion that says so. "haiku" matches exactly one model id.
  await page.keyboard.type("haiku");

  await expect
    .poll(() => panel.locator("button:not([disabled])").count(), { timeout: 20_000 })
    .toBeLessThan(before);
  await expect(panel.getByText(/haiku/i).first()).toBeVisible({ timeout: 20_000 });

  // ...and the prompt is untouched. This is the exact corruption the broken build
  // produced: the keystrokes went to the instructions textarea instead.
  await expect(prompt).toHaveValue(instructions);

  console.log("PROBLEMS:", JSON.stringify(rui.problems, null, 1));
  await rui.ctx.close();
});

test("an agent is created with the model that was chosen", async ({ browser }) => {
  // Onboarding, a dialog, a relay write and a roster read. The default budget is
  // 240 s and this path legitimately approaches it.
  test.setTimeout(420_000);
  const ada = await onboard(browser, "Ada Okoro");
  const { page } = ada;
  const stamp = Date.now().toString(36);
  const display = `Reviewer ${stamp}`;
  // `clean_agent_id` in the relay lowercases and replaces runs of anything outside
  // [a-z0-9_-] with a single '-', so this is the id the display name yields.
  const agentId = `reviewer-${stamp}`;
  const instructions =
    "You are Code Reviewer. You review Rust for correctness and clarity, then post your review and end your turn.";
  await openAgentDialog(ada, display, instructions);

  // Pick a specific model BY NAME, which is what a person does. "haiku" is the
  // discriminator: it matches exactly one model in this catalog.
  const discriminator = "haiku";
  await page.locator("#persona-model").click();
  const panel = openPanel(page);
  await expect(panel).toBeVisible({ timeout: 20_000 });
  await page.keyboard.type(discriminator);
  const haiku = panel.locator("button:not([disabled])").filter({ hasText: /haiku/i });
  await expect(haiku.first()).toBeVisible({ timeout: 20_000 });
  // The option's visible LABEL, which is not necessarily its stored id: the option
  // type allows a friendly name over a different value. So this string is asserted
  // where it belongs — on the trigger the user reads — and the relay is checked
  // against the discriminator instead.
  const chosenLabel = (await haiku.first().innerText()).trim();
  await haiku.first().click();
  await expect(panel).toBeHidden({ timeout: 20_000 });
  await expect(page.locator("#persona-model")).toHaveText(
    new RegExp(escapeForRegExp(chosenLabel)),
    { timeout: 20_000 },
  );

  // A BOUNDED click. Playwright waits for an element to be visible, stable and
  // unobstructed, and names whatever is intercepting pointer events when it is
  // not — but only if the wait ends before the test's own budget does. Unbounded,
  // this reported "Test ended" after four minutes and said nothing.
  await page.getByTestId("persona-dialog-submit").click({ timeout: 20_000 });
  await expect(page.getByTestId("persona-dialog")).toBeHidden({ timeout: 60_000 });

  // Asserted against the RELAY, not the screen. The roster is what every other
  // client reads, and a dialog that closes is not evidence that anything was stored.
  // Matched on the discriminator rather than the option's label, because the two are
  // allowed to differ and only the stored id is the relay's business.
  await expect
    .poll(
      async () => {
        const response = await fetch(`${RELAY_HTTP}/buzz/api/agents`);
        if (!response.ok) return `http ${response.status}`;
        const body = (await response.json()) as {
          agents?: { agent: string; model?: string | null }[];
        };
        const row = body.agents?.find((entry) => entry.agent === agentId);
        return row ? (row.model ?? "no model") : "absent";
      },
      { timeout: 90_000 },
    )
    .toMatch(new RegExp(discriminator, "i"));

  console.log("PROBLEMS:", JSON.stringify(ada.problems, null, 1));
  await ada.ctx.close();
});

/** Quote a model id for use inside a RegExp (ids carry dots and dashes). */
function escapeForRegExp(text: string) {
  return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
