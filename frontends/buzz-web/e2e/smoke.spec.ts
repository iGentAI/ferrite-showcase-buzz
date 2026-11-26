import { test, expect } from "@playwright/test";
import { messages, onboard, openChannel, say } from "./onboarding";

test("one person posts in general and sees it", async ({ browser }) => {
  const ana = await onboard(browser, "Ana Ruiz");
  await openChannel(ana, "General");
  const line = `smoke ${Date.now().toString(36)}: hello from the browser`;
  await say(ana, line);
  await expect(messages(ana.page).filter({ hasText: line })).toBeVisible({
    timeout: 45_000,
  });
  console.log("PROBLEMS:", JSON.stringify(ana.problems, null, 1));
  await ana.ctx.close();
});
