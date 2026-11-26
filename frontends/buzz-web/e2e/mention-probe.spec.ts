/**
 * What does the mention picker actually offer?
 *
 * A diagnostic that also asserts: the picker must offer a registered persona, and a message
 * containing a mention must reach the relay. It exists because two observations disagreed about whether a registered AI persona can
 * be @-mentioned — `agents-multi` times out after 45 s waiting for a suggestion keyed on
 * the persona's pubkey, while driving the same workspace by hand offered it, labelled
 * "agent · not in channel". A vision-driven browser cannot break the tie, because a click
 * that misses the composer is indistinguishable from a picker with nothing to show.
 *
 * So this asks the DOM: at each prefix length, dump every suggestion rendered, with the
 * pubkey its testid carries. Run it with `--reporter=list` and read the console output.
 */
import { test, expect } from "@playwright/test";
import { createHash } from "node:crypto";
import { finalizeEvent, generateSecretKey } from "nostr-tools/pure";

import { addMemberByPubkey, createChannel, editor, onboard, openChannel, openChannelId, type Person } from "./onboarding";

/** Where the relay's HTTP face lives. The same default as `relayHttp.ts`. */
const RELAY_HTTP = process.env.BUZZ_HTTP ?? "http://localhost:4173";

/** How long one relay poll may take before it is abandoned. */
const POLL_REQUEST_TIMEOUT_MS = 5_000;

/**
 * Which room to probe in.
 *
 * Defaults to General, but the interesting room is one the agent is a MEMBER of: the
 * mention picker offers a relay agent only where it shares the channel, and that rule
 * is correct rather than an obstacle — an agent mentioned somewhere it does not belong
 * would be woken by dispatch and then refused when it tried to answer.
 */
const PROBE_CHANNEL = process.env.BUZZ_PROBE_CHANNEL ?? "General";

/**
 * Does the relay hold a message containing `needle` in `channel` yet?
 *
 * Signed with a throwaway key of this process's own — the browser holds the person's key
 * and will not hand it over, and the only question here is whether the CONTENT arrived,
 * which any authenticated reader can answer. Same route the ops scripts use.
 *
 * ONE url string is both signed and fetched. NIP-98 binds a token to the exact URL, so
 * building the `u` tag separately from the request — with a hardcoded scheme, say — makes
 * every poll fail on any deployment where the two spellings differ, and the helper would
 * report `relay never`: an answer indistinguishable from a client that never wrote.
 */
// One random reader key per run. It used to be sha256 of a label plus the
// relay URL, which anyone could recompute and sign with.
const READER_SECRET = generateSecretKey();

async function relayHas(channel: string, needle: string) {
  const url = new URL("/buzz/api/query", RELAY_HTTP).toString();
  const sk = READER_SECRET;
  const body = JSON.stringify({ channel, limit: 60 });
  const payload = createHash("sha256").update(body).digest("hex");
  const auth = finalizeEvent(
    {
      kind: 27235,
      created_at: Math.floor(Date.now() / 1000),
      tags: [
        ["u", url],
        ["method", "POST"],
        ["nonce", `${Math.random()}`],
        ["payload", payload],
      ],
      content: "",
    },
    sk,
  );
  const response = await fetch(url, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Nostr ${Buffer.from(JSON.stringify(auth)).toString("base64")}`,
    },
    body,
    // Bounded, so one stalled request cannot hold the loop past its budget and starve
    // the DOM half of the measurement — which is the comparison this exists to make.
    signal: AbortSignal.timeout(POLL_REQUEST_TIMEOUT_MS),
  }).catch(() => null);
  if (!response?.ok) return false;
  const page = (await response.json().catch(() => null)) as {
    events?: { content?: string }[];
  } | null;
  return (page?.events ?? []).some((event) => (event.content ?? "").includes(needle));
}

/**
 * What the composer TOLD the user, if anything.
 *
 * Both abort paths in `useMentionSendFlow` call `toast.error(...)` with a reason and one also
 * sets an inline prompt error — so a refused send is never silent to a person, only to a test
 * that looks solely at the relay and the timeline. Two fix attempts were spent on absences
 * that could not distinguish one gate from another; this reads the reason instead.
 */
async function complaints(page: import("@playwright/test").Page) {
  const found: string[] = [];
  // Toasts (sonner), live regions, AND dialogs. The last one matters: a mention of someone who
  // is not a channel member may open a CONFIRMATION rather than fail, which is correct
  // behaviour and matches none of the error selectors — so a probe reading only errors would
  // report "nothing happened" about an app that is politely waiting for an answer.
  for (const selector of [
    "[data-sonner-toast]",
    "[role='status']",
    "[role='alert']",
    "[role='dialog']",
    "[role='alertdialog']",
    "[data-testid*='prompt']",
  ]) {
    const rows = page.locator(selector);
    const count = await rows.count().catch(() => 0);
    for (let i = 0; i < count; i += 1) {
      const visible = await rows.nth(i).isVisible().catch(() => false);
      if (!visible) continue;
      const text = ((await rows.nth(i).innerText().catch(() => "")) || "")
        .replace(/\s+/g, " ")
        .trim();
      if (text && !found.some((seen) => seen.includes(text))) {
        found.push(`${selector}: ${text.slice(0, 220)}`);
      }
    }
  }
  return found;
}

/**
 * Answer the "mention people outside this channel?" confirmation, if it appears.
 *
 * This dialog is why three rounds of investigation found an absence: it asks whether to invite
 * the mentioned party or send without inviting them, and until it is answered the message
 * correctly stays in the composer. Nothing was refusing the send.
 *
 * INVITE is the answer chosen when offered, because it is also the one that lets an AI teammate
 * reply: the relay authorizes a persona's own post against the channel, so a persona that was
 * never added to a private room could be woken and then unable to answer.
 *
 * Returns what it did, so a caller can report the dialog's presence as the ordinary path and
 * its ABSENCE as meaning the mentioned party was already a member.
 */
async function answerOutsideChannelPrompt(page: import("@playwright/test").Page) {
  const dialog = page.locator("[role='alertdialog'], [role='dialog']").filter({
    hasText: /outside this channel|not in this channel/i,
  });
  if (!(await dialog.first().isVisible().catch(() => false))) return "no dialog";
  for (const label of [/^invite$/i, /send anyway/i, /send without/i]) {
    const button = dialog.first().getByRole("button", { name: label });
    if (await button.count()) {
      const text = ((await button.first().innerText().catch(() => "")) || "").trim();
      await button.first().click({ timeout: 20_000 });
      await expect(dialog.first()).toBeHidden({ timeout: 20_000 });
      return `answered "${text}"`;
    }
  }
  return "dialog with no answerable button";
}

/**
 * Time a send at BOTH edges: when the relay holds it, and when the timeline shows it.
 *
 * Reported separately on purpose. A late relay timestamp means the cost is between the
 * client and the relay; a prompt relay timestamp beside a late row means the write was
 * fine and the RENDER is what lagged. The evidence that prompted this measurement — one
 * stored message and one missing row — could not distinguish them.
 */
async function timeSend(
  page: import("@playwright/test").Page,
  needle: string,
  budgetMs = 180_000,
  channelId = PROBE_CHANNEL.toLowerCase(),
) {
  const startedAt = Date.now();
  let onRelay: number | null = null;
  let inDom: number | null = null;
  const row = page.getByTestId("message-row").filter({ hasText: needle });
  while (Date.now() - startedAt < budgetMs && (onRelay === null || inDom === null)) {
    if (onRelay === null && (await relayHas(channelId, needle))) {
      onRelay = Date.now() - startedAt;
    }
    if (inDom === null && (await row.isVisible().catch(() => false))) inDom = Date.now() - startedAt;
    if (onRelay !== null && inDom !== null) break;
    await page.waitForTimeout(2_000);
  }
  const ms = (value: number | null) => (value === null ? "never" : `${(value / 1000).toFixed(1)}s`);
  return { onRelay, inDom, describe: `relay ${ms(onRelay)}, timeline ${ms(inDom)}` };
}

/**
 * Open the probe's room.
 *
 * By URL when a specific channel was named, because the SIDEBAR only lists rooms the person
 * has joined — a freshly onboarded identity cannot see an open room somebody else created,
 * and looking for it there fails for a reason unrelated to anything being measured. The
 * client routes channels by hash, so this is the same view a person reaches by browsing to
 * it. The default (General) still goes through the sidebar, like every other scenario.
 */
async function openProbeChannel(person: Person, persona?: { pubkey: string }) {
  if (!process.env.BUZZ_PROBE_CHANNEL) {
    if (persona) {
      // A mention of a relay agent is authorized only where the agent is on the room's
      // roster, so the send probe makes a room and adds the persona first; General would
      // measure the authorization refusal instead of the send.
      const name = `mention-probe-${Date.now().toString(36)}`;
      await createChannel(person, name);
      await openChannel(person, name);
      await addMemberByPubkey(person, persona.pubkey);
      return openChannelId(person);
    }
    await openChannel(person, PROBE_CHANNEL);
    return PROBE_CHANNEL.toLowerCase();
  }
  // The page's CURRENT url, not `BUZZ_UI`: that variable may be unset even on this path (the
  // onboarding helper has its own default), and falling back to an empty string produced a
  // relative `#/channels/…` that Playwright rejects outright. The loaded url is absolute by
  // definition.
  const base = person.page.url().split("#")[0];
  await person.page.goto(`${base}#/channels/${encodeURIComponent(PROBE_CHANNEL)}`, {
    waitUntil: "domcontentloaded",
  });
  await expect(person.page.getByTestId("message-composer")).toBeVisible({
    timeout: 45_000,
  });
  return PROBE_CHANNEL;
}

/** Everything the picker is showing right now, as `pubkey  label` lines. */
async function offered(page: import("@playwright/test").Page) {
  const rows = page.getByTestId("mention-autocomplete").locator("[data-testid^='mention-suggestion-']");
  const count = await rows.count();
  const out: string[] = [];
  for (let i = 0; i < count; i += 1) {
    const row = rows.nth(i);
    const id = (await row.getAttribute("data-testid")) ?? "";
    const text = ((await row.innerText().catch(() => "")) || "").replace(/\s+/g, " ").trim();
    out.push(`${id.replace("mention-suggestion-", "").slice(0, 12)}…  ${text.slice(0, 70)}`);
  }
  return out;
}

test("what the mention picker offers as a prefix grows", async ({ browser }) => {
  test.setTimeout(420_000);

  // The deployment's own personas, so the probe reports about what is really there.
  const roster = (await (await fetch(`${RELAY_HTTP}/buzz/api/agents`)).json()) as {
    agents?: { agent: string; pubkey: string }[];
  };
  const personas = (roster.agents ?? []).filter((a) => /^[0-9a-f]{64}$/.test(a.pubkey));
  console.log("\n=== personas this deployment registers ===");
  for (const p of personas) console.log(`  ${p.pubkey.slice(0, 12)}…  ${p.agent}`);
  expect(personas.length, "deployment must register at least one persona").toBeGreaterThan(0);

  const ana = await onboard(browser, `Probe ${Date.now().toString(36)}`);
  let offeredTarget = false;
  try {
    await openProbeChannel(ana);
    await editor(ana.page).click();

    // Grow the prefix one character at a time over the first persona's id, dumping the
    // picker after each. Where the persona first appears is the answer: at one character
    // the picker is doing something broad, and at the full name it is doing something
    // exact — which of those it needs is exactly what is in dispute.
    const target = personas[0];
    await ana.page.keyboard.type("@");
    for (let n = 1; n <= target.agent.length; n += 1) {
      await ana.page.keyboard.type(target.agent[n - 1] ?? "");
      // Generous: the global path pages kind-0 profiles and is debounced.
      await ana.page.waitForTimeout(6_000);
      const rows = await offered(ana.page);
      const found = rows.some((row) => row.startsWith(target.pubkey.slice(0, 12)));
      console.log(
        `\n@${target.agent.slice(0, n)}  → ${rows.length} suggestion(s)` +
          `${found ? `  [${target.agent} IS offered]` : ""}`,
      );
      // Every row, uncapped: this is a diagnostic, and a truncated dump can hide the
      // very entry it was run to look for.
      for (const row of rows) console.log(`    ${row}`);
      if (found) {
        offeredTarget = true;
        break;
      }
    }
    expect(
      offeredTarget,
      `mention picker never offered ${target.agent} (${target.pubkey.slice(0, 12)}…)`,
    ).toBe(true);
  } finally {
    console.log("\nPROBLEMS:", JSON.stringify(ana.problems, null, 1));
    await ana.ctx.close();
  }
});

/**
 * Can a message that CONTAINS a mention be sent?
 *
 * Three trials in one channel, because the interesting answer is which of them differs:
 *
 * 1. plain text + Enter — the control. Every passing scenario in the suite does this.
 * 2. a mention insert, then text, then Enter — the case that failed in `agents-multi`,
 *    where the composer demonstrably held the right content and no message was posted.
 * 3. the same content, sent with the composer's own send BUTTON rather than the key.
 *
 * If 1 works and 2 does not, Enter is being swallowed after a mention. If 3 then works,
 * the send path is fine and only the key binding is at fault — which is a real defect for
 * anyone who types rather than clicks, and it is also exactly what a person would do.
 */
test("whether a message containing a mention can be sent", async ({ browser }) => {
  test.setTimeout(420_000);

  const roster = (await (await fetch(`${RELAY_HTTP}/buzz/api/agents`)).json()) as {
    agents?: { agent: string; pubkey: string }[];
  };
  const persona = (roster.agents ?? []).find((a) => /^[0-9a-f]{64}$/.test(a.pubkey));
  expect(persona, "deployment must register at least one persona").toBeDefined();
  if (!persona) return;

  const ana = await onboard(browser, `Send ${Date.now().toString(36)}`);
  try {
    const channelId = await openProbeChannel(ana, persona);

    // 1. THE CONTROL: plain text, Enter.
    const plain = `probe plain ${Date.now().toString(36)}`;
    await editor(ana.page).click();
    await ana.page.keyboard.type(plain);
    await ana.page.keyboard.press("Enter");
    const plainTiming = await timeSend(ana.page, plain, 180_000, channelId);
    console.log(`\n1. plain text + Enter        → ${plainTiming.describe}`);
    expect(plainTiming.onRelay, "plain text message must reach the relay").not.toBeNull();
    expect(plainTiming.inDom, "plain text message must render in the timeline").not.toBeNull();

    // 2. A MENTION, then text, then Enter.
    const mentioned = `probe mention ${Date.now().toString(36)}`;
    await editor(ana.page).click();
    await ana.page.keyboard.type(`@${persona.agent.slice(0, 4)}`);
    const suggestion = ana.page
      .getByTestId("mention-autocomplete")
      .getByTestId(`mention-suggestion-${persona.pubkey}`)
      .or(
        ana.page
          .getByTestId("mention-autocomplete")
          .getByTestId(`mention-suggestion-persona-${persona.agent}`),
      );
    await expect(suggestion).toBeVisible({ timeout: 45_000 });
    await suggestion.click();
    await ana.page.keyboard.type(` ${mentioned}`);
    console.log(
      "   composer holds:",
      JSON.stringify((await editor(ana.page).innerText().catch(() => "?")).slice(0, 120)),
    );
    await ana.page.keyboard.press("Enter");
    // The confirmation appears immediately and blocks the send until answered — which is what
    // every earlier round mistook for a failure.
    await ana.page.waitForTimeout(3_000);
    const answered = await answerOutsideChannelPrompt(ana.page);
    console.log(`   outside-channel prompt: ${answered}`);
    const said = await complaints(ana.page);
    console.log(
      said.length ? `   on screen: ${JSON.stringify(said)}` : "   on screen: nothing",
    );
    const mentionTiming = await timeSend(ana.page, mentioned, 90_000, channelId);
    console.log(`2. mention + text + Enter    → ${mentionTiming.describe}`);

    // Only if it STILL did not send is there anything left to explain.
    if (mentionTiming.onRelay === null) {
      console.log("   → pressing Escape, then Enter again");
      await editor(ana.page).click();
      await ana.page.keyboard.press("Escape");
      await ana.page.waitForTimeout(500);
      await ana.page.keyboard.press("Enter");
      await ana.page.waitForTimeout(2_000);
      console.log(`   outside-channel prompt: ${await answerOutsideChannelPrompt(ana.page)}`);
      const afterEscape = await timeSend(ana.page, mentioned, 60_000, channelId);
      console.log(`3. Escape, then Enter        → ${afterEscape.describe}`);
      const saidAfter = await complaints(ana.page);
      if (saidAfter.length) console.log("   on screen:", JSON.stringify(saidAfter));
      console.log(
        "   composer now holds:",
        JSON.stringify((await editor(ana.page).innerText().catch(() => "?")).slice(0, 120)),
      );
    }

    expect(mentionTiming.onRelay, "message containing a mention must reach the relay on Enter").not.toBeNull();
    expect(mentionTiming.inDom, "message containing a mention must render in the timeline").not.toBeNull();
  } finally {
    console.log("\nPROBLEMS:", JSON.stringify(ana.problems, null, 1));
    await ana.ctx.close();
  }
});
