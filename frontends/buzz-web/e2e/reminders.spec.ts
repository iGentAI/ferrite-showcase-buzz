/**
 * "Remind me later" — a private, author-only reminder set on a message, driven
 * through buzz's own client against a LIVE relay.
 *
 * A reminder is a NIP-ER kind 30300 parameterized-replaceable event whose
 * content is nip44-encrypted TO ITSELF (`reminderService.ts`:
 * `nip44EncryptToSelf` → `signRelayEvent` → `publishEvent`), keyed by
 * `(pubkey, 30300, <random 128-bit d tag>)`, with the schedule in a public
 * `not_before` tag so the relay can be asked for it without being able to read
 * it. Two properties make it worth its own file, and both are asserted here:
 *
 *   1. It is RELAY state, not local state. The panel has no local store at all
 *      (`useRemindersQuery` → `fetchReminders` → one authenticated query per
 *      read), so a full
 *      page reload is the receipt: after a reload nothing in the tab remembers
 *      the reminder, and what the panel renders can only have come back over
 *      the socket. Snoozing is asserted the same way — the panel shows the new
 *      schedule after a remount, and shows exactly ONE row for the reminder,
 *      which is the relay's NIP-33 replacement rather than two events.
 *   2. It is AUTHOR-ONLY at the relay (`kind.rs::AUTHOR_ONLY_KINDS`), enforced
 *      twice: `access::filter_authorized` refuses a query that could match
 *      another author's reminder, and `access::may_deliver` refuses the event
 *      itself. Scenario 2 asserts the filter layer directly, because no browser
 *      surface can: the shipped client only ever subscribes with
 *      `authors: [me]`, so driving the UI cannot tell "the relay refused" from
 *      "the client never asked".
 *
 *   BUZZ_UI=http://localhost:4173/buzz/ui/index.html \
 *   BUZZ_HTTP=http://localhost:4173 \
 *   npx playwright test e2e/reminders.spec.ts --reporter=line --workers=1
 *
 * WHAT IS NOT ASSERTED, and why:
 *
 *  * The `/reminders` ROUTE is not driven, because it is not a surface any
 *    more: `app/routes/reminders.tsx` is a `beforeLoad` redirect to `/`, kept
 *    for old history entries. Reminders live behind the inbox filter dropdown
 *    (`inbox-filter-trigger` → "Reminders" → `home-inbox-reminders`), which is
 *    the only reachable path and therefore the one used below.
 *  * Nothing asserts a reminder FIRING. `useReminderNotifications` toasts when
 *    `not_before` crosses now, and the shortest schedule this UI can express is
 *    the "In 30 minutes" preset — so watching one fire would mean a 30-minute
 *    test. The custom date/time surface is used instead, which is also what
 *    makes the schedule an exact number this test can assert on the wire.
 *  * The reminder's CONTENT is never asserted from a relay frame. It is
 *    nip44 ciphertext and the test holds no key; `not_before` and the `d` tag
 *    are public by design and are what the wire assertions use. The plaintext
 *    is asserted where it is legible: on screen, after a reload, which is the
 *    same evidence a user has.
 */
import { test, expect } from "@playwright/test";

import {
  messages,
  onboard,
  openChannel,
  relayEventId,
  say,
  type Person,
} from "./onboarding";
import {
  identity,
  publishAs,
  relayRequest,
  RELAY_HTTP,
  said,
} from "./relayHttp";

/**
 * Distinguishes this run's traffic from every earlier run on the same relay,
 * AND from the other workers of this run: module scope re-evaluates per worker
 * process, so each worker gets its own stamp.
 */
const stamp = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
const ANA = `Ana Ruiz ${stamp}`;

/**
 * Two onboardings' worth of headroom even though only one person onboards.
 * `onboard()` is a chain of 45-90 s waits plus up to four stall retries against
 * a relay whose starter-channel read is measured at ~3.9 s on its own, and this
 * scenario then reloads the workspace twice. A generous ceiling costs a passing
 * run nothing and stops a starved host from being reported as a product bug.
 */
const SCENARIO_BUDGET_MS = 600_000;

/** Report what the page complained about, whether or not the scenario passed. */
function report(...people: Person[]) {
  for (const person of people) {
    if (person.problems.length) {
      console.log(`--- ${person.name} ---\n${[...new Set(person.problems)].join("\n")}`);
    }
  }
}

const pad = (value: number) => String(value).padStart(2, "0");

/**
 * A local `YYYY-MM-DD` `daysAhead` days from now, plus the epoch second that
 * `<date>T09:00` names.
 *
 * Computed exactly the way the app computes it (`timePresets.parseCustomDateTime`
 * → `new Date(\`${date}T${time}\`)`, which the ES spec parses as LOCAL time for
 * a date-time form with no offset), so the number asserted against the relay's
 * `not_before` tag is the same number the browser derived from the same two
 * strings. The browser and this process share the host's timezone, so there is
 * no offset to reconcile — if that ever stops being true this is the assertion
 * that will say so.
 */
function futureNineAm(daysAhead: number): { date: string; notBefore: number } {
  const target = new Date();
  target.setDate(target.getDate() + daysAhead);
  const date = `${target.getFullYear()}-${pad(target.getMonth() + 1)}-${pad(target.getDate())}`;
  return { date, notBefore: Math.floor(new Date(`${date}T09:00`).getTime() / 1000) };
}

/**
 * Open Inbox → filter → Reminders, and prove the panel mounted.
 *
 * The filter is component state (`HomeView`: `useState<InboxFilter>("all")`),
 * NOT a URL parameter — which is exactly why this has to be re-driven after
 * every reload, and exactly why a reload is a real receipt: the tab remembers
 * nothing about reminders, so what appears below came back from the relay.
 *
 * The dropdown is a radix menu that animates in, so a click issued while it is
 * still mounting lands on nothing (the failure `createChannel` documents at
 * length for the create dialog's permissions menu). Hence the retry, and
 * nothing inside the loop may throw.
 */
async function openRemindersPanel(person: Person) {
  const { page } = person;
  await page
    .getByTestId("app-sidebar")
    .getByRole("button", { name: "Inbox" })
    .first()
    .click({ timeout: 45_000 });
  const trigger = page.getByTestId("inbox-filter-trigger");
  await expect(trigger).toBeVisible({ timeout: 60_000 });
  const option = page.getByRole("menuitemradio", { name: "Reminders" });
  for (let attempt = 0; attempt < 4; attempt += 1) {
    if (await page.getByTestId("home-inbox-reminders").isVisible().catch(() => false)) {
      break;
    }
    await trigger.click({ timeout: 20_000 }).catch(() => undefined);
    if (await option.isVisible().catch(() => false)) {
      await option.click({ force: true }).catch(() => undefined);
    }
    await page.waitForTimeout(800);
  }
  const panel = page.getByTestId("home-inbox-reminders");
  await expect(panel).toBeVisible({ timeout: 45_000 });
  return panel;
}

/**
 * A reminder set on a message outlives the tab that set it, and snoozing it
 * changes what the panel serves.
 *
 * The sequence is deliberate: create → assert the WIRE (a 30300 carrying the
 * exact `not_before` the dialog was given) → RELOAD → assert the SCREEN →
 * snooze → assert the wire again (same `d` tag, new schedule) → RELOAD → assert
 * the screen again, including that there is still exactly one row. Each pair is
 * a mechanism claim followed by the product claim it caused; neither alone would
 * be worth much, because a client-side cache satisfies the screen and a
 * published event does not prove anything was stored.
 */
test("1. a reminder survives a reload, and snoozing re-schedules it", async ({
  browser,
}) => {
  test.setTimeout(SCENARIO_BUDGET_MS);
  // kind 9 for the message's durable relay id (see `relayEventId`), kind 30300
  // for the reminder events the relay serves back to the panel's own read.
  const ana = await onboard(browser, ANA, { watchKinds: [9, 30300] });
  await openChannel(ana, "General");

  const subject = `remind me ${stamp}: rotate the signing keys`;
  const note = `note-${stamp} ask ops first`;
  await say(ana, subject);
  const row = messages(ana.page).filter({ hasText: subject }).first();
  await expect(row).toBeVisible({ timeout: 45_000 });

  // Wait for the relay's echo BEFORE setting the reminder. Until it arrives the
  // row carries a client-minted `optimistic-<uuid>` id, and `handleRemindLater`
  // copies `msg.id` straight into the reminder's target — so a reminder set too
  // early would durably point at an id that exists nowhere on the relay and
  // could never navigate anywhere. The id itself is not asserted on: it lives
  // inside the reminder's nip44 ciphertext, which no relay frame exposes.
  await relayEventId(ana, subject);

  // --- set it ---------------------------------------------------------------
  await row.scrollIntoViewIfNeeded();
  await row.hover();
  // The testid carries the message id, and by now that is the relay's id — but
  // matching the PREFIX inside this row is what keeps the locator honest if the
  // echo swap ever lands between the hover and the click.
  await row
    .locator('[data-testid^="more-actions-"]')
    .first()
    .click({ timeout: 20_000 });
  await ana.page
    .getByRole("menuitem", { name: /remind me later/i })
    .click({ timeout: 20_000 });

  // `RemindMeLaterDialog` carries no testid; its note field does, and it is
  // inside the dialog, so it is the anchor that proves the dialog opened.
  const noteField = ana.page.locator("#reminder-note");
  await expect(noteField).toBeVisible({ timeout: 45_000 });
  await noteField.fill(note);

  // The custom date/time surface, not a preset: it is the only schedule this UI
  // can express as an exact number, which is what makes the `not_before`
  // assertion below an equality rather than a range. Twelve days out also gives
  // a deterministic day-scale relative-time receipt without coupling this
  // journey to the panel's current visual grouping.
  const { date, notBefore } = futureNineAm(12);
  await ana.page.getByLabel("Reminder date").fill(date);
  await ana.page.getByLabel("Reminder time").fill("09:00");
  await ana.page
    .getByRole("button", { name: /^Set reminder$/ })
    .click({ timeout: 20_000 });
  // The dialog closes only from `create.onSuccess` — so its disappearance is
  // the mutation resolving, not the click landing.
  await expect(noteField).toBeHidden({ timeout: 90_000 });

  // THE WIRE RECEIPT. The panel is not even open yet: `RemindMeLaterProvider`
  // holds the same `useRemindersQuery`, so `create.onSuccess` invalidates it and
  // the refetch is a REQ whose answer frames this tap records. What is asserted
  // is the relay's own copy of the event — a `d` tag with the 128 bits of
  // entropy NIP-ER requires (32 hex chars, not a UUIDv4's 122 bits), the exact
  // schedule the dialog was given, and Ana's key as the author.
  let reminderDTag = "";
  await expect
    .poll(
      () => {
        reminderDTag =
          ana.received.find(
            (event) =>
              event.kind === 30300 &&
              event.pubkey === ana.pubkey &&
              event.tags.some(
                (tag) => tag[0] === "not_before" && tag[1] === String(notBefore),
              ),
          )?.tags.find((tag) => tag[0] === "d")?.[1] ?? "";
        return reminderDTag;
      },
      {
        timeout: 90_000,
        message: `no kind 30300 authored by Ana carrying not_before=${notBefore} came back from the relay`,
      },
    )
    .toMatch(/^[0-9a-f]{32}$/);

  // --- it survives a reload -------------------------------------------------
  // Everything the tab knew is gone here: the query cache, the inbox filter,
  // the message list. The panel below is rendering one REQ's answer.
  await ana.page.reload();
  await expect(ana.page.getByTestId("app-sidebar")).toBeVisible({
    timeout: 90_000,
  });
  const panel = await openRemindersPanel(ana);

  // The message's own text is the reminder's preview (`msg.body.slice(0, 100)`),
  // and the note rides alongside it — so both halves of what was typed came
  // back through the relay, decrypted with Ana's own key.
  await expect(panel).toContainText(subject, { timeout: 90_000 });
  await expect(panel).toContainText(note, { timeout: 45_000 });
  // Day-scale, because the schedule is 12 days out. The contrast with the
  // minute-scale text after the snooze is what makes the snooze assertion mean
  // something: `formatRelativeTime` is a pure function of `not_before`, so this
  // string changing IS the schedule changing.
  await expect(panel.getByText(/^in \d+d$/)).toBeVisible({ timeout: 45_000 });
  // Exactly one row, and it is this run's. Ana is a fresh identity per run and
  // reminders are author-scoped, so anything else here would mean the relay
  // served another author's event — which scenario 2 asserts it refuses.
  await expect(panel.locator("p").filter({ hasText: subject })).toHaveCount(1);

  // --- snooze it ------------------------------------------------------------
  // One reminder ⇒ one Snooze control (its accessible name comes from
  // `title="Snooze"`), so the page-level role locator is unambiguous. The
  // control lives in the detail pane, which is a sibling of the list pane
  // carrying `home-inbox-reminders`.
  const snoozedAtMs = Date.now();
  const snoozeMenu = ana.page.getByRole("button", { name: /snooze/i });
  const inThirty = ana.page.getByRole("menuitem", { name: "In 30 minutes" });
  for (let attempt = 0; attempt < 4; attempt += 1) {
    await snoozeMenu.click({ timeout: 20_000 }).catch(() => undefined);
    if (await inThirty.isVisible().catch(() => false)) break;
    await ana.page.keyboard.press("Escape");
    await ana.page.waitForTimeout(700);
  }
  await expect(
    inThirty,
    "the snooze dropdown never offered its presets",
  ).toBeVisible({ timeout: 20_000 });
  await inThirty.click({ force: true });

  // The wire again: the SAME `d` tag — so this is a replacement of that
  // reminder and not a second one — carrying a schedule half an hour out
  // instead of twelve days. The window is wide enough to absorb the round trip
  // and narrow enough that only the 30-minute preset can satisfy it.
  const lowerBound = Math.floor(snoozedAtMs / 1000) + 1500;
  const upperBound = Math.floor(snoozedAtMs / 1000) + 2100;
  await expect
    .poll(
      () =>
        ana.received.some((event) => {
          if (event.kind !== 30300 || event.pubkey !== ana.pubkey) return false;
          if (!event.tags.some((tag) => tag[0] === "d" && tag[1] === reminderDTag)) {
            return false;
          }
          const scheduled = Number(
            event.tags.find((tag) => tag[0] === "not_before")?.[1] ?? "0",
          );
          return scheduled >= lowerBound && scheduled <= upperBound;
        }),
      {
        timeout: 90_000,
        message: `no replacement 30300 under d=${reminderDTag} arrived with a not_before inside [${lowerBound}, ${upperBound}]`,
      },
    )
    .toBe(true);

  // --- and the reschedule is what the relay now serves ----------------------
  // The second reload is the one that proves the relay REPLACED the reminder
  // rather than storing two: a fresh tab reads the whole set back, and there is
  // one row, on the new schedule. A client-side cache cannot produce this, and
  // two stored events would show two rows.
  await ana.page.reload();
  await expect(ana.page.getByTestId("app-sidebar")).toBeVisible({
    timeout: 90_000,
  });
  const afterSnooze = await openRemindersPanel(ana);
  await expect(afterSnooze).toContainText(subject, { timeout: 90_000 });
  await expect(
    afterSnooze.locator("p").filter({ hasText: subject }),
    "the snooze stored a second reminder instead of replacing the first",
  ).toHaveCount(1);
  await expect(
    afterSnooze.getByText(/^in \d+m$|^in less than a minute$/),
    "the panel still shows the pre-snooze schedule",
  ).toBeVisible({ timeout: 90_000 });
  await expect(afterSnooze.getByText(/^in \d+d$/)).toHaveCount(0);
  // The GROUP label is deliberately not asserted here. `groupReminders` buckets
  // on end-of-today, so "30 minutes from now" lands in Today for most of the
  // day and in Upcoming for the last half hour of it — a run after 23:30 would
  // fail an assertion about the product being fine. The relative-time text
  // above is a pure function of `not_before` and carries the same fact without
  // the wall-clock dependency.

  report(ana);
  await ana.ctx.close();
});

/**
 * A second key may not even ASK the relay for someone else's reminders.
 *
 * This is the one claim in the file no browser can carry, so it is asserted on
 * the wire, and it is bracketed three ways so that nothing here can pass
 * vacuously:
 *
 *   positive  X's own reminder is stored and served back to X's own request —
 *             so kind 30300 is a queryable, deliverable kind on this relay and
 *             the refusals below are about WHO is asking.
 *   negative  the same signed request, one filter field different
 *             (`authors: [Y]`), is
 *             refused at the filter layer with the relay's own sentence.
 *   negative  an unconstrained `{kinds:[30300]}` is refused too, which is the
 *             case that would otherwise leak every member's schedule.
 *   negative  an ANONYMOUS HTTP query is rejected rather than handed an empty
 *             page, which proves the edge requires per-request NIP-98.
 *
 * No browser is opened: onboarding costs 30-90 s and would prove nothing extra,
 * because the shipped client cannot construct any of these filters.
 */
test("2. one member's reminders are unreadable to another key", async () => {
  test.setTimeout(240_000);
  const owner = identity(`reminder-owner-${stamp}`);
  const stranger = identity(`reminder-stranger-${stamp}`);

  // A reminder of X's own, published over the relay's HTTP ingest — the same
  // saga the browser's `publishEvent` runs. The content is opaque on purpose:
  // the real client stores nip44 ciphertext here and the relay never reads it,
  // so a literal marker is a faithful stand-in and keeps the test independent
  // of the encryption.
  // 32 hex characters, the shape `randomDTag()` writes. Derived from this
  // run's own key so two runs can never collide on it.
  const dTag = owner.pubkey.slice(0, 32);
  const notBefore = Math.floor(Date.now() / 1000) + 86_400;
  const written = await publishAs(
    owner,
    30300,
    [
      ["d", dTag],
      ["not_before", String(notBefore)],
    ],
    `ciphertext-stand-in-${stamp}`,
  );
  expect(
    written.status,
    `the relay refused a kind 30300 from a fresh key: ${said(written)} — if this says "invite-only", this deployment has been closed and the whole premise of the scenario changed`,
  ).toBe(200);

  const filterQuery = (who: typeof owner, filter: Record<string, unknown>) =>
    relayRequest(who, "POST", "/buzz/api/filter-query", [filter]);

  // --- the positive control -----------------------------------------------
  const own = await filterQuery(owner, {
    kinds: [30300],
    authors: [owner.pubkey],
    limit: 50,
  });
  expect(own.status, `the relay refused the author's filter: ${said(own)}`).toBe(200);
  const ownEvents = JSON.parse(own.body) as Array<{ tags: string[][] }>;
  expect(
    ownEvents.some((event) =>
      event.tags.some((tag) => tag[0] === "d" && tag[1] === dTag),
    ),
    "the author's own reminder did not come back",
  ).toBe(true);

  // --- naming somebody else is refused, not merely empty ------------------
  const notMine = await filterQuery(stranger, {
    kinds: [30300],
    authors: [owner.pubkey],
    limit: 50,
  });
  expect(notMine.status, `another key's filter was served: ${said(notMine)}`).toBe(
    403,
  );
  expect(notMine.body).toContain(
    "restricted: this filter asks for events you may not read",
  );

  // --- and an unconstrained read is refused too ---------------------------
  const unscoped = await filterQuery(stranger, { kinds: [30300], limit: 50 });
  expect(unscoped.status, "an author-unconstrained reminder filter was served").toBe(
    403,
  );
  expect(unscoped.body).toContain(
    "restricted: this filter asks for events you may not read",
  );

  // --- anonymous HTTP has no per-request NIP-98 authority -----------------
  const unauthenticated = await fetch(`${RELAY_HTTP}/buzz/api/filter-query`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify([
      { kinds: [30300], authors: [owner.pubkey], limit: 50 },
    ]),
  });
  expect(unauthenticated.status).toBe(401);
  expect(await unauthenticated.text()).toMatch(/auth|authorization/i);
});
