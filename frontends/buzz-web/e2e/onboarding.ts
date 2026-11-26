/**
 * buzz's REAL onboarding, driven the way a person drives it.
 *
 * Every scenario starts here, so it lives in one place: identity → harnesses →
 * community → profile → starter team → workspace. Nothing is stubbed; each step
 * is the same button a user clicks, and the workspace only appears once the
 * relay has answered (`ensure_starter_channels` joins `general` and
 * `welcome-everyone` over the live socket before the sidebar renders).
 */
import { expect, type Browser, type BrowserContext, type Page } from "@playwright/test";
import { newBuzzContext } from "./sso";
import { createdFromEvent, recordCreated } from "./teardown";

export const UI =
  process.env.BUZZ_UI ?? "http://localhost:4173/buzz/ui/index.html";
export const RELAY_WS = process.env.BUZZ_WS ?? "ws://localhost:4173/buzz/";

/** One relay→client EVENT, as this person's socket received it. */
export type ReceivedEvent = {
  kind: number;
  pubkey: string;
  id: string;
  content: string;
  tags: string[][];
};

/** A colleague: their own browser context, their own identity key. */
export type Person = {
  name: string;
  ctx: BrowserContext;
  page: Page;
  /**
   * This person's identity key (64-hex), captured off the first signed frame
   * the client sends (the NIP-42 AUTH, or any published EVENT). Scenarios use
   * it for deterministic, pubkey-keyed selectors — the shared live relay holds
   * many identically-named kind 0s from earlier runs, so resolving a colleague
   * by display name alone is ambiguous.
   */
  pubkey: string;
  /**
   * Inbound relay EVENTs whose kind the scenario asked `onboard` to record.
   *
   * This is how a scenario proves WHICH relay output drove the UI, not merely
   * that the UI eventually agreed: a sidebar entry can appear because a 60 s
   * poll fired, so "the room showed up" alone cannot distinguish the live
   * membership-notice path from the poll. Recording is opt-in per kind because
   * taping every frame would copy a channel's whole timeline into the test
   * process for scenarios that never look at it.
   */
  received: ReceivedEvent[];
  /** Everything the page logged, for the report a failing scenario owes. */
  problems: string[];
};

function watch(page: Page, problems: string[]) {
  page.on("pageerror", (error) =>
    problems.push(`pageerror: ${error.message.slice(0, 200)}`),
  );
  page.on("console", (message) => {
    const text = message.text();
    if (message.type() !== "error" && message.type() !== "warning") return;
    if (/updater unavailable/.test(text)) return;
    problems.push(`${message.type()}: ${text.slice(0, 200)}`);
  });
  page.on("response", (response) => {
    if (!response.url().includes("/buzz/") || response.status() < 400) return;
    const where = `http ${response.status()} ${new URL(response.url()).pathname}`;
    // Record the relay's REASON, not just the status. Every 4xx here carries a
    // machine-readable `{error}` (`auth.rs` alone has a dozen distinct 401
    // strings — wrong method, wrong path, clock skew, a burned auth event), and
    // a bare "http 401 /buzz/api/feed" cannot tell those apart. Chasing one of
    // these without the body meant re-running a two-minute scenario to learn
    // something the first run already knew.
    void response
      .text()
      .then((body) => {
        let reason = body.trim().slice(0, 200);
        try {
          const parsed = JSON.parse(body) as { error?: unknown };
          if (typeof parsed.error === "string") reason = parsed.error;
        } catch {
          // Not the relay's JSON shape — a proxy or gateway answered instead.
        }
        problems.push(reason ? `${where}: ${reason}` : where);
      })
      .catch(() => problems.push(where));
  });
}

/**
 * Record every object this page makes the relay create, for `teardown.ts`.
 * A room or teammate is recorded on the relay's 2xx; a 9007 or a project event
 * is recorded when it is sent over the socket (the OK frame is not tapped), and
 * deleting one the relay refused is a harmless no-op at teardown.
 */
export function tapCreations(page: Page) {
  page.on("response", (response) => {
    const request = response.request();
    if (request.method() !== "POST" || response.status() < 200 || response.status() >= 300) return;
    const pathname = new URL(response.url()).pathname;
    if (pathname === "/buzz/api/channels") {
      void response
        .json()
        .then((body: { channel?: { id?: unknown } }) => {
          const id = body.channel?.id ?? (request.postDataJSON() as { id?: unknown } | null)?.id;
          if (typeof id === "string" && id) recordCreated({ type: "room", id });
        })
        .catch(() => undefined);
    } else if (pathname === "/buzz/api/agents") {
      void response
        .json()
        .then((body: { agent?: unknown }) => {
          if (typeof body.agent === "string" && body.agent) recordCreated({ type: "agent", agent: body.agent });
        })
        .catch(() => undefined);
    } else if (pathname === "/buzz/api/events") {
      try {
        const entry = createdFromEvent(request.postDataJSON() as { kind?: unknown; pubkey?: unknown; tags?: unknown });
        if (entry) recordCreated(entry);
      } catch {
        // Not a JSON event body.
      }
    }
  });
  page.on("websocket", (ws) => {
    ws.on("framesent", (frame) => {
      try {
        const parsed = JSON.parse(String(frame.payload)) as unknown[];
        if (parsed[0] !== "EVENT") return;
        const entry = createdFromEvent(parsed[1] as { kind?: unknown; pubkey?: unknown; tags?: unknown });
        if (entry) recordCreated(entry);
      } catch {
        // Not a JSON protocol frame.
      }
    });
  });
}

export async function onboard(
  browser: Browser,
  name: string,
  options: { watchKinds?: number[] } = {},
): Promise<Person> {
  const ctx = await newBuzzContext(browser, name);
  const page = await ctx.newPage();
  const problems: string[] = [];
  const received: ReceivedEvent[] = [];
  const watchKinds = options.watchKinds ?? [];
  watch(page, problems);
  tapCreations(page);

  // The identity key never appears in the DOM, but every signed relay write
  // carries it. Tap both transports BEFORE navigation: desktop builds use the
  // WebSocket protocol, while the served edge build deliberately uses signed
  // HTTP polling and POST /events.
  let pubkey = "";
  const receivedIds = new Set<string>();
  page.on("request", (request) => {
    if (!request.url().includes("/buzz/api/events")) return;
    try {
      const event = request.postDataJSON() as ReceivedEvent;
      if (!pubkey && /^[0-9a-f]{64}$/.test(event.pubkey)) {
        pubkey = event.pubkey;
      }
    } catch {
      // A proxy or malformed diagnostic request; the ordinary response checks
      // will report a real product failure.
    }
  });
  page.on("response", (response) => {
    if (
      !watchKinds.length ||
      !response.url().includes("/buzz/api/filter-query") ||
      !response.ok()
    ) {
      return;
    }
    void response
      .json()
      .then((events: unknown) => {
        if (!Array.isArray(events)) return;
        for (const event of events as ReceivedEvent[]) {
          if (
            watchKinds.includes(event.kind) &&
            typeof event.id === "string" &&
            !receivedIds.has(event.id)
          ) {
            receivedIds.add(event.id);
            received.push(event);
          }
        }
      })
      .catch(() => undefined);
  });
  page.on("websocket", (ws) => {
    ws.on("framesent", (frame) => {
      if (pubkey) return;
      try {
        const parsed = JSON.parse(String(frame.payload)) as unknown[];
        const event =
          parsed[0] === "AUTH" || parsed[0] === "EVENT"
            ? (parsed[1] as { pubkey?: unknown })
            : null;
        if (typeof event?.pubkey === "string") pubkey = event.pubkey;
      } catch {
        // Not a JSON protocol frame; signed events always are.
      }
    });
    if (!watchKinds.length) return;
    // A relay→client event frame is `["EVENT", <subscription id>, <event>]`
    // (NIP-01), so the event is at index 2 — the client→relay publish frame
    // above puts it at index 1, which is why these two taps cannot share code.
    ws.on("framereceived", (frame) => {
      try {
        const parsed = JSON.parse(String(frame.payload)) as unknown[];
        if (parsed[0] !== "EVENT") return;
        const event = parsed[2] as ReceivedEvent | undefined;
        if (
          event &&
          watchKinds.includes(event.kind) &&
          !receivedIds.has(event.id)
        ) {
          receivedIds.add(event.id);
          received.push(event);
        }
      } catch {
        // Not a JSON protocol frame.
      }
    });
  });

  await page.goto(UI, { waitUntil: "domcontentloaded" });
  await page
    .getByRole("button", { name: /create a new identity key/i })
    .click({ timeout: 45_000 });
  await page.getByRole("button", { name: /^Next$/ }).click({ timeout: 45_000 });
  await page
    .getByRole("button", { name: /skip for now/i })
    .first()
    .click({ timeout: 45_000 });
  // Two onboarding shapes exist, and the harness must drive whichever the
  // served client renders. A client served BY its relay (the bundle
  // `push-ui.mjs` publishes, as on staging) already knows its community: it asks
  // for a display name, then offers an avatar, then enters the workspace. A
  // client that has to be pointed at a relay asks to join a community first.
  const displayName = page.getByTestId("onboarding-display-name");
  const joinCommunity = page.getByRole("button", { name: /join a community/i }).first();
  await expect(displayName.or(joinCommunity)).toBeVisible({ timeout: 45_000 });
  let enter = page.getByRole("button", {
    name: /take me to buzz|skip for now/i,
  });
  if (await displayName.isVisible()) {
    await page
      .getByRole("textbox", { name: /name/i })
      .first()
      .fill(name, { timeout: 45_000 });
    await page.getByTestId("onboarding-next").click({ timeout: 45_000 });
    // The avatar card is the last one; skipping it enters the workspace.
    enter = page.getByTestId("onboarding-skip");
    await enter.click({ timeout: 90_000 });
    // On this path a failed starter-room setup does not change the button: it
    // raises a toast (`features/onboarding/hooks.ts`, "Couldn't set up starter
    // channels"). Watch for it while the workspace mounts and record it, the
    // same finding the join-community branch records as "Skip for now".
    void page
      .getByText(/couldn't set up starter channels/i)
      .first()
      .waitFor({ state: "visible", timeout: 60_000 })
      .then(() => problems.push("starter channels failed: the setup toast appeared"))
      .catch(() => undefined);
  } else {
    await joinCommunity.click({ timeout: 45_000 });
    await page.locator("input:visible").first().fill(RELAY_WS);
    await page
      .getByRole("button", { name: /^Next$/ })
      .first()
      .click({ timeout: 45_000 });
    await page.getByPlaceholder(/username/i).fill(name, { timeout: 45_000 });
    await page
      .getByRole("button", { name: /^Next$/ })
      .first()
      .click({ timeout: 45_000 });
    // The last card offers "Take me to Buzz", or "Skip for now" when the starter
    // rooms failed twice. Either one enters the workspace; which one appeared is
    // itself a finding, so record it rather than only accepting the happy label.
    await enter.first().click({ timeout: 90_000 });
    if (await page.getByRole("button", { name: /skip for now/i }).count()) {
      problems.push("starter channels failed twice: entered via Skip for now");
    }
  }
  // The card can REFUSE to advance: when the starter-channel setup outruns the
  // client's 15 s HTTP budget, it stays put and shows "The relay did not answer
  // in time. Try again." A real person clicks the button again, and it then
  // usually works — so the harness does too, rather than reporting a relay
  // latency problem as a missing sidebar 90 s later. Every attempt past the
  // first is recorded, because the retry is the FINDING: measured on the live
  // dev relay, the kind-39000 read that `ensure_starter_channels` performs
  // costs ~3.9 s on its own, and onboarding crosses 15 s under any concurrency.
  //
  // The refusal is NOT always the timeout wording. The same card, with the same
  // live button, also renders "restricted: authorization event has already been
  // used" — the platform duplicating a write (the gateway hedges after 500 ms)
  // against a relay whose NIP-98 burn is not yet idempotent. Matching only the
  // timeout sentence made that variant fall through to the do-nothing branch and
  // take the whole scenario down 90 s later, so the predicate matches ANY refusal
  // on the card and records the sentence it actually saw.
  const sidebar = page.getByTestId("app-sidebar");
  const REFUSAL = /relay did not answer in time|already been used|restricted:|invalid:|unavailable:/i;
  for (let attempt = 1; attempt <= 4; attempt += 1) {
    if (await sidebar.isVisible().catch(() => false)) break;
    // `count()` is the non-blocking primitive and it MUST come first. An earlier
    // version of this loop called `innerText()` straight away, which WAITS for
    // its element to exist — so in the ordinary case (onboarding merely slow, no
    // refusal on the card) it parked until the test timeout, and `.catch()` never
    // fired because it never rejected. A trace showed 421 s of a 420 s budget
    // spent inside that single read while every other action took about a second.
    // Any `innerText` on an element that may legitimately be absent needs its own
    // timeout too.
    const refusal = page.getByText(REFUSAL).first();
    if (!(await refusal.count())) {
      // No refusal on the card — give the workspace time to mount on its own.
      await page.waitForTimeout(2_000);
      continue;
    }
    const card = await refusal.innerText({ timeout: 2_000 }).catch(() => "");
    // Record the VERBATIM sentence: which refusal appeared is the finding, and
    // the two have entirely different causes (relay read latency versus a
    // duplicated write meeting a non-idempotent auth burn).
    problems.push(
      `onboarding stalled on "${card.replace(/\s+/g, " ").trim().slice(0, 120)}" (retry ${attempt})`,
    );
    await enter
      .first()
      .click({ timeout: 45_000 })
      .catch(() => undefined);
    await page.waitForTimeout(3_000);
  }
  await expect(sidebar).toBeVisible({ timeout: 90_000 });
  // The entry curtain covers the app while Welcome mounts; it lifts on its own.
  await page
    .waitForFunction(
      () => !document.body.innerText.includes("Meet your starter team"),
      null,
      { timeout: 45_000 },
    )
    .catch(() => problems.push("the entry curtain never lifted"));
  // Entering the workspace required an authenticated socket (the starter-room
  // joins ride it), so the key must have crossed the tap by now.
  await expect
    .poll(() => pubkey, { timeout: 30_000 })
    .toMatch(/^[0-9a-f]{64}$/);
  return { name, ctx, page, pubkey, received, problems };
}

/**
 * Create a channel the way a person does: sidebar → Browse channels → Create a
 * new channel. There is no "New channel" button in the sidebar itself; the
 * browser dialog is the only entry point.
 */
export async function createChannel(
  person: Person,
  name: string,
  options: { private?: boolean } = {},
) {
  const { page } = person;
  // The sidebar's quick action, by test id: the Welcome room's empty state
  // renders a second "Browse channels" button with the same accessible name.
  await page.getByTestId("section-actions-channels-quick-create").click();
  await page.getByRole("button", { name: /create a new channel/i }).click();
  await page.getByRole("textbox").first().fill(name);
  if (options.private) {
    // Two shapes of the same control exist upstream (`ChannelPermissionsSettings`):
    // a SEGMENTED toggle, where both options are always rendered and the chosen
    // one carries aria-pressed, and a radix DROPDOWN, whose options only mount
    // while its menu is open. The create dialog renders the segmented one since
    // sync 003; the dropdown path is kept for the management sheet's variant.
    const trigger = page.getByTestId("create-channel-permissions");
    const option = page.getByTestId("create-channel-permissions-option-private");
    if (await option.isVisible().catch(() => false)) {
      await option.click({ timeout: 20_000 });
      await expect(option).toHaveAttribute("aria-pressed", "true", {
        timeout: 10_000,
      });
    } else {
      // The radix menu opens on pointerdown and animates in; a click issued
      // while it is still mounting lands on nothing.
      for (let attempt = 0; attempt < 3; attempt += 1) {
        await trigger.click();
        if (await option.isVisible().catch(() => false)) break;
        await page.waitForTimeout(500);
      }
      // `force` skips the stability wait: the radix menu keeps a transform
      // animation running long enough that Playwright's "element is stable"
      // check can starve for the full timeout while the option is genuinely
      // visible and clickable (measured in the 2026-07-28 serial run).
      await option.click({ timeout: 20_000, force: true });
      await expect(option).toBeHidden({ timeout: 10_000 });
      await expect(trigger).toContainText("Private", { timeout: 10_000 });
    }
  }
  await page.getByTestId("create-channel-submit").click();
  await expect(page.getByTestId("app-sidebar")).toContainText(name, {
    timeout: 45_000,
  });
  // The dialog must actually CLOSE, and that is a separate claim from "the
  // channel exists". When the relay answers an error the create still lands but
  // the dialog stays open, and its Radix overlay then intercepts pointer events
  // for the entire app — so the real cause surfaces as some unrelated click
  // timing out minutes later in a different step. (Measured: a spurious
  // `restricted: authorization event has already been used` on a duplicated
  // request left this dialog open, and scenario 12 then burned its full 300 s
  // budget retrying one sidebar click 435 times.) Fail here instead, quoting
  // the relay.
  const overlay = page.getByTestId("dialog-overlay");
  try {
    await expect(overlay).toBeHidden({ timeout: 20_000 });
  } catch {
    const shown = await page
      .getByRole("dialog")
      .innerText()
      .catch(() => "");
    const said = shown
      .split("\n")
      .map((line) => line.trim())
      .find((line) => /^(restricted|invalid|blocked|unauthorized|error)\b/i.test(line));
    throw new Error(
      `the create-channel dialog stayed open after "${name}" was created` +
        (said ? `; the relay said: ${said}` : "; it showed no error text") +
        " — every later click in this page is blocked by its overlay",
    );
  }
}

/** Open a channel from the sidebar and wait for its composer. */
export async function openChannel(person: Person, channel: string | RegExp) {
  const sidebar = person.page.getByTestId("app-sidebar");
  const entry = sidebar.getByText(channel, { exact: false }).first();
  await expect(entry).toBeAttached({ timeout: 45_000 });
  await entry.scrollIntoViewIfNeeded();
  await entry.click();
  await expect(person.page.getByTestId("message-composer")).toBeVisible({
    timeout: 45_000,
  });
}

/**
 * The members dialog of the OPEN room, opened from the room's header.
 *
 * The search box is cleared on every open. `searchQuery` lives in
 * `MembersSidebar` ABOVE the dialog it renders, so it survives a close: leaving
 * the pubkey from an earlier add in place filters the roster, and a member card
 * that is filtered out cannot be hovered.
 */
export async function openMembersDialog(person: Person) {
  const dialog = person.page.getByTestId("members-sidebar");
  if (!(await dialog.isVisible().catch(() => false))) {
    await person.page.getByTestId("channel-members-trigger").first().click();
  }
  await expect(dialog).toBeVisible({ timeout: 45_000 });
  await person.page.getByTestId("channel-management-search-users").fill("");
}

/**
 * Close the members dialog, and prove it closed.
 *
 * A dialog that stays open keeps its Radix overlay intercepting pointer events
 * for the whole app, so the real cause surfaces minutes later as some unrelated
 * click timing out — the failure mode `createChannel` documents at length. Fail
 * here instead.
 *
 * Escape alone does NOT do it after using a member's action menu. The menu sets
 * `onCloseAutoFocus={(event) => event.preventDefault()}`
 * (`MembersSidebarMemberCard.tsx`), so when the menu closes focus is restored to
 * nothing, the keypress never reaches the dialog, and it sits there open. So the
 * Escape here is only to dismiss a lingering menu, and the dialog is closed by
 * its own Close control.
 */
export async function closeMembersDialog(person: Person) {
  const dialog = person.page.getByTestId("members-sidebar");
  for (let attempt = 0; attempt < 3; attempt += 1) {
    if (await dialog.isHidden().catch(() => false)) break;
    await person.page.keyboard.press("Escape");
    await dialog
      .getByRole("button", { name: /close/i })
      .click({ timeout: 10_000 })
      .catch(() => undefined);
    await person.page.waitForTimeout(700);
  }
  await expect(dialog).toBeHidden({ timeout: 20_000 });
}

/**
 * Add someone to the OPEN room from its members dialog, by PUBKEY, and wait for
 * the roster to list them.
 *
 * This is how a workspace persona joins a room. The synced client authorizes an
 * @mention of a relay agent only where that agent is on the room's signed roster
 * (`relayAgentCanRespondInChannel`), and its "outside this channel" Invite
 * refuses to add an agent the person does not own — which is also why the relay
 * only lets the persona post where it is a member. So a scenario that asks an
 * agent something first makes it a member, the way the headless `multi-agent`
 * check does, rather than answering a dialog that must refuse.
 *
 * Search is by pubkey for the reason `channel-lifecycle.spec.ts` gives: the
 * shared relay holds same-named kind 0s from earlier runs.
 */
export async function addMemberByPubkey(person: Person, pubkey: string) {
  const { page } = person;
  await openMembersDialog(person);
  await page.getByTestId("channel-management-search-users").fill(pubkey);
  const addRow = page.getByTestId(`channel-user-search-result-${pubkey}`);
  await expect(addRow).toBeVisible({ timeout: 45_000 });
  await addRow.click();
  await page.getByTestId("channel-management-search-users").fill("");
  await expect(
    page.getByTestId("members-sidebar").getByTestId(`sidebar-member-${pubkey}`),
    "the added member never appeared on the room's roster",
  ).toBeVisible({ timeout: 90_000 });
  await closeMembersDialog(person);
}

/** The id of the room this person has open, from the client's route. */
export function openChannelId(person: Person): string {
  const match = /#\/channels\/([^/?#]+)/.exec(person.page.url());
  if (!match) throw new Error(`no channel is open at ${person.page.url()}`);
  return decodeURIComponent(match[1]);
}

/**
 * Type into buzz's composer and send.
 *
 * The composer is a Tiptap editor, so the thing that takes keystrokes is the
 * contenteditable inside it — clicking the `message-composer` wrapper focuses
 * nothing and every keystroke goes to the document.
 */
export function editor(page: Page) {
  return page
    .getByTestId("message-composer")
    .locator('[contenteditable="true"]')
    .first();
}

export async function say(person: Person, text: string) {
  const field = editor(person.page);
  await field.click();
  await person.page.keyboard.type(text);
  await person.page.keyboard.press("Enter");
  // A send clears the draft at once (the row is optimistic), so a draft still
  // in the composer means Enter was not taken, and that is a finding, not
  // something to press through. An earlier revision pressed Enter again here:
  // the ignored Enter in a just-created room was the seam answering
  // `get_channel_details` "No channel <id>" during the projection race (review
  // finding 2, fixed in `channelProjectionFrom`), and a second Enter could
  // double-post if the composer cleared late.
  await expect
    .poll(async () => ((await field.innerText().catch(() => "")) || "").includes(text), {
      timeout: 10_000,
      message: `the composer kept "${text.slice(0, 60)}" after Enter: the send was not taken`,
    })
    .toBe(false);
}

/**
 * The THREAD panel's composer, which is a different composer from the channel's.
 *
 * Both carry `data-testid="message-composer"` (there is one `MessageComposer`
 * component and it tags itself), so `editor()`'s `.first()` resolves to the
 * CHANNEL composer even when a thread is open. That is not a hypothetical: it is
 * why the old thread scenario passed for weeks while threaded replies were
 * completely broken in the browser build — the reply text was typed into the
 * main composer, posted as an ordinary channel message, and the assertion
 * ("the text is visible") was satisfied by that ordinary message. Scoping to
 * `thread-composer-overlay` (`MessageThreadPanel.tsx`) is what makes a thread
 * test actually about threads.
 */
export function threadEditor(page: Page) {
  return page
    .getByTestId("thread-composer-overlay")
    .getByTestId("message-composer")
    .locator('[contenteditable="true"]')
    .first();
}

/** Type into the open thread panel's composer and send. */
export async function sayInThread(person: Person, text: string) {
  const field = threadEditor(person.page);
  await field.click();
  await person.page.keyboard.type(text);
  await person.page.keyboard.press("Enter");
}

/**
 * The RELAY's id for a message this person sent, taken off their own socket.
 *
 * A just-sent message lives in the cache under a client-minted
 * `optimistic-<uuid>` id until the relay echoes it back. Anything that needs the
 * durable id — asserting an `e` tag names it, or opening its thread — has to
 * wait for that echo, because the optimistic id exists nowhere on the relay.
 * Requires `onboard(..., { watchKinds: [9] })`.
 */
export async function relayEventId(person: Person, contains: string) {
  let id = "";
  await expect
    .poll(
      () => {
        id =
          person.received.find(
            (event) => event.kind === 9 && event.content.includes(contains),
          )?.id ?? "";
        return id;
      },
      { timeout: 90_000 },
    )
    .toMatch(/^[0-9a-f]{64}$/);
  return id;
}

export function messages(page: Page) {
  return page.getByTestId("message-row");
}

/** The relay's HTTP face. One default, shared — three specs had three copies of it. */
export const RELAY_HTTP = process.env.BUZZ_HTTP ?? "http://localhost:4173";

/** One registered persona, as the relay's public roster describes it. */
export type RelayPersona = { agent: string; pubkey: string; harness: string };

/**
 * The personas this deployment actually runs.
 *
 * `GET /buzz/api/agents` is public — a persona's pubkey is already on every message it posts — so
 * this needs no identity, and it is the same list a client renders mentions from.
 *
 * Both fields are validated and the result is deduplicated by pubkey. The agent id is used to
 * type a mention prefix AND to build a selector, so it is held to what the relay's own
 * `clean_agent_id` guarantees (lowercase alphanumerics, `-`, `_`); an entry outside that is
 * skipped rather than trusted. Two entries sharing a pubkey would let ONE agent satisfy two
 * per-pubkey assertions, which is the single way a fan-out test could pass while proving nothing.
 */
export async function relayPersonas(): Promise<RelayPersona[]> {
  const response = await fetch(`${RELAY_HTTP}/buzz/api/agents`);
  if (!response.ok) throw new Error(`the roster answered http ${response.status}`);
  const body = (await response.json()) as { agents?: RelayPersona[] };
  const seen = new Set<string>();
  const personas: RelayPersona[] = [];
  for (const entry of body.agents ?? []) {
    if (typeof entry?.pubkey !== "string" || !/^[0-9a-f]{64}$/.test(entry.pubkey)) continue;
    if (typeof entry?.agent !== "string" || !/^[a-z0-9_-]{1,64}$/.test(entry.agent)) continue;
    if (seen.has(entry.pubkey)) continue;
    seen.add(entry.pubkey);
    // The harness matters to one scenario — the Fabric arm is a different path end to end — and
    // the roster route already reports it, so it travels with the rest rather than being fetched
    // again.
    personas.push({
      agent: entry.agent,
      pubkey: entry.pubkey,
      harness: typeof entry.harness === "string" ? entry.harness : "",
    });
  }
  return personas;
}

/** The open mention autocomplete, if any. */
export function mentionPicker(person: Person) {
  return person.page.getByTestId("mention-autocomplete");
}

/**
 * Pick a persona out of the mention autocomplete, matching its row EXACTLY.
 *
 * A suggestion's testid is `pubkey ?? persona-{personaId} ?? team-{teamId} ?? displayName`
 * (`MentionAutocomplete`'s own `suggestionKey`), and which form a persona carries depends on
 * whether the client could resolve it to a relay agent — a cloud teammate resolves to a pubkey,
 * a purely local persona does not. Both are accepted, as two EXACT testids or'd together, never
 * a substring match over the row: a row also renders "agent", "not in channel" and an owner
 * label, so matching its text could select a row that merely mentions the id.
 *
 * `typed` is what a person types, prefix included.
 */
export async function mentionPersona(
  person: Person,
  persona: RelayPersona,
  typed: string,
) {
  await editor(person.page).click();
  await person.page.keyboard.type(typed);
  const suggestion = mentionPicker(person)
    .getByTestId(`mention-suggestion-${persona.pubkey}`)
    .or(mentionPicker(person).getByTestId(`mention-suggestion-persona-${persona.agent}`));
  // 45 s, not 20: the suggestion needs the persona's kind 0 back through the relay's profile
  // read, which under real traffic exceeds a 20 s budget.
  await expect(suggestion).toBeVisible({ timeout: 45_000 });
  // Exactly one row may match. Two would mean the roster holds one agent id twice, or that a
  // candidate carries both keys — either way the click is a coin toss, and a coin toss is not a
  // receipt.
  await expect(suggestion).toHaveCount(1);
  await suggestion.click();
  // The picker must be closed before anything else is typed. Not because it swallows Enter — that
  // was a hypothesis and measurement disproved it — but because an open picker is an ambiguity
  // for one visibility read.
  if (await mentionPicker(person).isVisible().catch(() => false)) {
    await person.page.keyboard.press("Escape");
  }
  await expect(mentionPicker(person)).toBeHidden({ timeout: 20_000 });
}

/**
 * Assert that NO "mention people outside this channel?" confirmation appears.
 *
 * Where a scenario has already made the persona a member of the room, the
 * prompt would mean the add silently failed (the client still sees the persona
 * as an outsider). Answering it with Invite (what an earlier helper did)
 * would paper over that and let the scenario pass for the wrong reason.
 */
export async function expectNoOutsiderPrompt(person: Person) {
  const dialog = person.page
    .locator("[role='alertdialog'], [role='dialog']")
    .filter({ hasText: /outside this channel|not in this channel/i })
    .first();
  const shown = await dialog
    .waitFor({ state: "visible", timeout: 10_000 })
    .then(() => true)
    .catch(() => false);
  if (shown) {
    const said = ((await dialog.innerText().catch(() => "")) || "").replace(/\s+/g, " ").trim();
    throw new Error(
      `the outside-channel prompt appeared for a persona this scenario had added to the room, so the add did not take: "${said.slice(0, 200)}"`,
    );
  }
}
