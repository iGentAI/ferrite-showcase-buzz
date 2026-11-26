/**
 * A room's LIFECYCLE and its MODERATION, driven through buzz's own client
 * against a LIVE relay.
 *
 * `company-day.spec.ts` covers a room being made, joined, talked in and
 * renamed. This file covers what happens to a room and its people afterwards:
 * membership and roles change, the room is archived and brought back, a message
 * is retracted, and a member is silenced by community moderators. Same rules as
 * that file — nothing is mocked, each person is their own browser context with
 * their own identity key, and the assertion is always the CONSEQUENCE the relay
 * produced rather than the click that asked for it.
 *
 *   npx playwright test e2e/channel-lifecycle.spec.ts --reporter=line --workers=1
 *
 * Two deliberate choices run through the whole file, both learned the hard way
 * in the suite this one joins:
 *
 *  1. Where a privilege is the subject, the negative and the positive are
 *     asserted on the SAME locator, minutes apart, with only the relay-side
 *     role changed in between. "The Edit button is missing" on its own is worth
 *     nothing — an unimplemented feature satisfies it. "The Edit button is
 *     missing, then present after a promotion, then missing again after a
 *     demotion" can only be satisfied by the relay's roster driving the UI.
 *  2. Where a refusal is the subject, it is bracketed by a success using the
 *     same helper. A `say()` that lands before the archive and again after the
 *     unarchive is what makes the silent `say()` in between a real refusal
 *     rather than a broken composer.
 *
 * What this file does NOT claim, and why (measured on this deployment, 2026-07-29):
 *
 *  * The composer's timeout banner (`composer-timeout-banner`) was not asserted
 *    before 2026-09-12, because the relay refused a timed-out write with
 *    `"restricted: restricted: you are banned or timed out"`, which the client's
 *    parse contract (`"restricted: you are timed out until"`,
 *    `features/moderation/lib/timeout.ts`) could not match. The deployed relay
 *    now answers `"restricted: you are timed out until <unix seconds>"`
 *    (measured on staging generation 28, 02:30Z), so scenario 4 asserts the
 *    banner as well as the write-block.
 *  * No scenario drives the moderator's own UI. The ban/timeout affordances are
 *    gated on `useMyRelayMembershipQuery` reporting owner/admin, and in the
 *    browser build that query can only ever answer `null`:
 *    `relay_requires_membership` returns `false` (`browser-backend/commands.ts`),
 *    which makes `loadRelayMembershipLookup` short-circuit before it reads
 *    anything, and nothing in this relay ever publishes the kind-13534 snapshot
 *    it would have read. Scenario 4 therefore drives the moderator half over
 *    the relay's own HTTP face and keeps the RECEIPT in the target's browser.
 *  * No scenario clicks `channel-management-add-members`. That testid lives in
 *    `ChannelMemberInviteCard.tsx`, which no screen renders in this build; the
 *    reachable add path is the members dialog's search row, which is what
 *    scenario 1 uses.
 *
 * Known environment noise, expected rather than diagnosed: onboarding
 * intermittently stalls on "The relay did not answer in time" (the shared
 * `onboard()` helper retries and records it), every run logs a `get_canvas`
 * DesktopUnavailableError and a `linkifyjs: already initialized` warning, and
 * `http 401 ... restricted: authorization event has already been used` appears
 * on writes that in fact landed — the deployed relay still predates its
 * idempotency fix, so the platform's write-duplication burns the NIP-98 auth
 * event and the second copy is refused. Because that 401 surfaces on operations
 * that SUCCEEDED, no scenario here asserts the absence of an error toast; each
 * asserts the durable outcome instead.
 *
 * That 401 is not always cosmetic, and this file will not pretend otherwise: on
 * the first 2026-07-29 run it hard-failed a scenario INSIDE `onboard()`, leaving
 * the starter-team card reading "restricted: authorization event has already
 * been used Try again." with `app-sidebar` never mounted, because the stall retry
 * matched only /relay did not answer in time/ and this is a different sentence on
 * the same card. `onboard()` now matches any refusal rendered there and records
 * the verbatim sentence, so it retries instead — which is why a passing scenario
 * here may still carry that 401 in `problems`. If a scenario ever fails inside
 * `onboarding.ts` again, the failure is the harness meeting this relay bug, not
 * an assertion below.
 */
import { test, expect, type Page } from "@playwright/test";
import { createHash } from "node:crypto";
// The operator key: BUZZ_OPERATOR_STATE, then the provisioning file.
import { identity, operatorSecret, PROVISION_FILE } from "./relayHttp";
import { finalizeEvent } from "nostr-tools/pure";

import {
  closeMembersDialog,
  createChannel,
  editor,
  messages,
  onboard,
  openChannel,
  openChannelId,
  openMembersDialog,
  say,
  type Person,
} from "./onboarding";

/**
 * Distinguishes this run's traffic from every earlier run on the same relay,
 * AND from the other workers of this run: module scope re-evaluates per worker
 * process, so each worker gets its own stamp. The random tail makes two
 * workers starting in the same millisecond collision-proof. Nothing may use
 * the stamp as a cross-scenario rendezvous — none exists today, and sharing
 * one would re-couple scenarios that are otherwise independent.
 */
const stamp = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;

/**
 * The PEOPLE are stamped, not just the text. Display names were the one real
 * cross-scenario coupling: scenarios resolve colleagues by name, and the
 * shared live relay already holds dozens of identical kind 0s from earlier
 * runs — a latent flake even serially, and ambiguous across parallel workers.
 * Unique names (plus the pubkey-keyed selectors below) remove it, so no
 * scenario needs to stay serial.
 */
const ANA = `Ana Ruiz ${stamp}`;
const BO = `Bo Nilsson ${stamp}`;

/**
 * The per-scenario wall-clock ceiling, set for the WORST case rather than the
 * typical one.
 *
 * Every scenario in this file onboards two people, and `onboard()` is a chain of
 * waits individually allowed 45-90 s (the entry click, the sidebar, the curtain,
 * the pubkey poll) plus up to four stall retries — so two onboardings alone can
 * legitimately consume ~400 s before a scenario's first assertion. On top of
 * that these scenarios are poll-bounded in places (a member learns a room was
 * archived from a 60 s channel-list read) and reload whole workspaces.
 *
 * The config default is 240 s, which fits `company-day.spec.ts`'s lighter
 * two-person scenarios. It does not fit these: the 2026-07-29 run gave scenario
 * 1 a 420 s budget and it was still inside the SECOND onboarding when the axe
 * fell, on a host whose load average was 8.7 from unrelated processes (an
 * iCloud daemon and a crash reporter, each pinning a core). A generous ceiling
 * costs a passing run nothing — a test ends when its assertions do — and it
 * stops a starved HOST from being reported as a product failure, which is the
 * one thing this suite must never do.
 */
const SCENARIO_BUDGET_MS = 900_000;

/** Report what the page complained about, whether or not the scenario passed. */
function report(...people: Person[]) {
  for (const person of people) {
    if (person.problems.length) {
      console.log(`--- ${person.name} ---\n${[...new Set(person.problems)].join("\n")}`);
    }
  }
}

/* ------------------------------------------------------------------------- */
/* Local helpers                                                             */
/* ------------------------------------------------------------------------- */

/**
 * The channel-management panel, opened from the room's own header.
 *
 * `channel-management-trigger` is carried by TWO controls in
 * `ChannelMembersBar.tsx` — the inline icon button and the compact dropdown's
 * "Manage channel" item — and which one is mounted depends on the pane width,
 * so a bare `getByTestId` is strict-mode ambiguous. `.first()` is deliberate,
 * and the wait on `channel-management-sheet` is what proves the panel actually
 * opened rather than the click landing on a hidden twin.
 */
async function openManagementPanel(person: Person) {
  await person.page.getByTestId("channel-management-trigger").first().click();
  await expect(person.page.getByTestId("channel-management-sheet")).toBeVisible({
    timeout: 45_000,
  });
}

/**
 * The members dialog helpers (`openMembersDialog`, `closeMembersDialog`) live in
 * `onboarding.ts`, because company-day and agents-multi add personas to rooms the
 * same way scenario 1 adds a colleague.
 */

/**
 * Wait up to `budgetMs` for a relay-minted membership notice of `kind` naming
 * this person and the room `roomId`, and report whether it came.
 *
 * Returns a boolean rather than asserting, for the one case where a notice is
 * known NOT to arrive (see the removal step of scenario 1): a scenario that must
 * report a tracked defect without going red needs the observation, not a
 * failure. Requires `onboard(..., { watchKinds: [kind] })`.
 */
async function waitForNotice(
  person: Person,
  kind: number,
  budgetMs: number,
  roomId: string,
): Promise<boolean> {
  const deadline = Date.now() + budgetMs;
  for (;;) {
    const seen = person.received.some(
      (event) =>
        event.kind === kind &&
        event.tags.some((tag) => tag[0] === "p" && tag[1] === person.pubkey) &&
        event.tags.some((tag) => tag[0] === "h" && tag[1] === roomId),
    );
    if (seen) return true;
    if (Date.now() >= deadline) return false;
    await person.page.waitForTimeout(1_000);
  }
}

/**
 * Whether this page currently offers the privileged channel edit.
 *
 * `channel-management-edit` is rendered only when `canManageChannel` — an
 * owner or admin of the room, decided from the relay's roster read
 * (`useChannelModerationCapabilities` over `useChannelMembersQuery`) — and the
 * name field and Save button live inside the dialog it opens, so all three
 * appear and vanish together. Reading the count rather than visibility is
 * intentional: a member's panel does not render the control at all, and
 * `toBeHidden` would also pass for a control that exists but is off-screen.
 */
async function canEditChannel(person: Person): Promise<number> {
  return person.page.getByTestId("channel-management-edit").count();
}

/**
 * Re-read this person's roster from the relay by reloading their tab.
 *
 * A role change made by somebody else is not pushed: the client's live channel
 * subscription carries message kinds, not the 39002 roster projection
 * (`CHANNEL_EVENT_KINDS`), and `useChannelMembersQuery` holds its answer for
 * 30 s and only runs while the panel is open. So closing and reopening the
 * panel can legitimately serve the pre-change roster. A reload is the
 * deterministic re-read, and it strengthens rather than weakens the claim: a
 * capability that survives a full remount cannot be a local echo of anything
 * this tab did.
 */
async function reopenWorkspace(person: Person, channel: string | RegExp) {
  await person.page.reload();
  await expect(person.page.getByTestId("app-sidebar")).toBeVisible({
    timeout: 90_000,
  });
  await openChannel(person, channel);
}

/**
 * Pick an item out of a radix dropdown SUBMENU (Change role → Admin).
 *
 * Two things make this more than a click. The trigger row is only mounted while
 * its member card is hovered (`sidebar-member-menu-*` is `invisible` until
 * `group-hover/member`), so the card has to be hovered first; and the menu's
 * content portals to <body>, where the MODAL members dialog marks it
 * pointer-events:none — so the "Change role" `DropdownMenuSubTrigger` cannot be
 * opened by hover and its role items cannot be selected by click. The dropdown
 * is opened from its trigger inside the dialog (interactive), and everything
 * body-portaled below it is driven by the KEYBOARD, which fires on the focused
 * element regardless of pointer-events.
 */
async function pickFromMemberMenu(
  page: Page,
  pubkey: string,
  path: { submenu?: string; item: string },
) {
  const card = page.getByTestId(`sidebar-member-${pubkey}`);
  await expect(card).toBeVisible({ timeout: 45_000 });
  const item = page.getByTestId(path.item);
  // Nothing inside the loop may THROW, or the first mistimed open aborts the
  // retry it exists to perform; the assertion below is what fails the test.
  for (let attempt = 0; attempt < 5; attempt += 1) {
    // Bounded: the config sets no actionTimeout, so a bare hover on a card the
    // closing menu momentarily covers waits out the whole 15-minute scenario
    // budget instead of failing this one attempt.
    await card.hover({ timeout: 10_000 }).catch(() => undefined);
    await page
      .getByTestId(`sidebar-member-menu-${pubkey}`)
      .click({ timeout: 10_000 })
      .catch(() => undefined);
    if (path.submenu) {
      const submenu = page.getByTestId(path.submenu);
      if (await submenu.isVisible().catch(() => false)) {
        // The dropdown CONTENT portals to <body>, and the members dialog is a
        // MODAL Radix dialog that marks everything outside itself
        // pointer-events:none — so HOVERING the sub-trigger never fires and its
        // SubContent never opens. Keyboard events reach the focused element
        // regardless, so focus the sub-trigger and open it with ArrowRight.
        await submenu.focus().catch(() => undefined);
        await submenu.press("ArrowRight").catch(() => undefined);
      }
    }
    // A just-hovered sub-content mounts after Radix's open delay, so WAIT for
    // the item rather than reading its visibility in the same tick the hover
    // was issued.
    if (
      await item
        .waitFor({ state: "visible", timeout: 3_000 })
        .then(() => true)
        .catch(() => false)
    ) {
      break;
    }
    // Both levels of the menu have to go, or the next attempt's click toggles
    // the parent shut instead of reopening it.
    await page.keyboard.press("Escape");
    await page.keyboard.press("Escape");
    await page.waitForTimeout(1_000);
  }
  await expect(
    item,
    `the member menu never offered ${path.item}`,
  ).toBeVisible({ timeout: 20_000 });
  // A role item is DISABLED when it is the member's current role
  // (`disabled={disabled || member.role === role}`), and activating a disabled
  // radix item is a silent no-op — the command is simply never sent. Left
  // unchecked that surfaces minutes later as the next capability assertion
  // failing for the wrong reason, so it is caught here: a disabled item means
  // this page's roster has not caught up with the change before it.
  await expect(
    item,
    `${path.item} was disabled — this tab's roster still shows the old role`,
  ).not.toHaveAttribute("data-disabled", /.*/, { timeout: 20_000 });
  // Activate with the keyboard, for the same reason the submenu was opened with
  // it: a click on a body-portaled menu item is swallowed by the modal dialog's
  // pointer-events guard, while Enter on the focused item fires its onSelect.
  await item.focus().catch(() => undefined);
  await item.press("Enter");
}

/* --- The relay's own HTTP face, for the moderator half of scenario 4 ------ */

/**
 * The operator secret this deployment was provisioned with.
 *
 * The community moderator set (`buzz-community-moderators`) is reachable only
 * from the operator plane, and that plane is rooted in a pubkey set no HTTP
 * request can widen — deliberately, so a community member cannot escalate into
 * it (`ingress.rs::operator_caller`). The local dev fleet's operator key was
 * written here when the workspace agents were provisioned; a run without it
 * SKIPS scenario 4 loudly rather than quietly asserting less.
 */
/** Same origin the browser uses, so the vite preview proxy fronts both. */
const RELAY_HTTP = process.env.BUZZ_HTTP ?? "http://localhost:4173";

/**
 * One NIP-98 `Authorization` header.
 *
 * The relay binds the signature to the exact method and full URL and, for a
 * POST, to a SHA-256 of the body via the `payload` tag — and it burns the auth
 * event on use. So the URL is finalized before signing and never touched
 * after, and every request gets its own fresh event.
 */
function nip98(
  secret: Uint8Array,
  method: string,
  url: string,
  body?: string,
): string {
  const tags: string[][] = [
    ["u", url],
    ["method", method],
    ["nonce", `${Math.random().toString(36).slice(2)}${Date.now()}`],
  ];
  if (body !== undefined) {
    tags.push(["payload", createHash("sha256").update(body).digest("hex")]);
  }
  const event = finalizeEvent(
    {
      kind: 27235,
      created_at: Math.floor(Date.now() / 1000),
      tags,
      content: "",
    },
    secret,
  );
  return `Nostr ${Buffer.from(JSON.stringify(event)).toString("base64")}`;
}

async function relayRequest(
  secret: Uint8Array,
  method: "GET" | "POST",
  path: string,
  payload?: unknown,
): Promise<{ status: number; body: string }> {
  const url = `${RELAY_HTTP}${path}`;
  const body = payload === undefined ? undefined : JSON.stringify(payload);
  const response = await fetch(url, {
    method,
    headers: {
      "x-buzz-authorization": nip98(secret, method, url, body),
      "content-type": "application/json",
    },
    body,
  });
  // The body is returned WHOLE. An earlier revision truncated it for readable
  // failure messages, which silently broke `JSON.parse` on a moderator set long
  // enough to exceed the cut — the callers slice for display instead.
  return { status: response.status, body: await response.text() };
}

/** The relay's answer, trimmed for a failure message. */
function said(reply: { status: number; body: string }): string {
  return `${reply.status} ${reply.body.slice(0, 200)}`;
}

/** Publish a signed event through the relay's HTTP ingest, returning its answer. */
async function publishAs(
  who: { secret: Uint8Array },
  kind: number,
  tags: string[][],
  content = "",
): Promise<{ status: number; body: string }> {
  const event = finalizeEvent(
    { kind, created_at: Math.floor(Date.now() / 1000), tags, content },
    who.secret,
  );
  return relayRequest(who.secret, "POST", "/buzz/api/events", event);
}

/* ------------------------------------------------------------------------- */
/* Scenarios                                                                 */
/* ------------------------------------------------------------------------- */

/**
 * A colleague is added to a private room, promoted, demoted, and removed.
 *
 * The membership commands are NIP-29 kinds 9000 (put-user, which is also the
 * role-assignment verb — there is no separate one) and 9001 (remove-user), and
 * the owner issues every one of them from the members dialog. None of that is
 * the assertion. What is asserted is what the OTHER person's client can then
 * do, in this order:
 *
 *   added     → the room appears in Bo's sidebar and Bo can read its history,
 *               and the relay minted a 44100 membership notice naming Bo
 *   member    → Bo's management panel offers no privileged edit
 *   promoted  → the SAME panel now offers it, Bo renames the room, and ANA's
 *               sidebar carries the new name
 *   demoted   → the privileged edit is gone again
 *   removed   → a 44101 names Bo and the room leaves Bo's sidebar
 *
 * The room is PRIVATE on purpose. A public room is writable by any
 * authenticated pubkey whether or not they are a member (measured: a removed
 * member's kind 9 into an open room is still accepted), so a public room would
 * make the removal step assert almost nothing. In a private room the relay
 * refuses a non-member with `restricted: not a member of this channel`, which
 * is what makes losing membership mean something.
 */
test("1. a colleague is added, promoted, demoted, and removed", async ({
  browser,
}) => {
  test.setTimeout(SCENARIO_BUDGET_MS);
  // 44100/44101 are the relay-minted membership notices, `#p`-gated to their
  // subject, which the shipped client already subscribes to
  // (`useMembershipNotifications`). Taping them is how this scenario knows the
  // relay did the work, rather than a 60 s channel-list poll having eventually
  // agreed.
  const ana = await onboard(browser, ANA);
  const bo = await onboard(browser, BO, { watchKinds: [44100, 44101] });
  const room = `crew-${stamp}`;

  await createChannel(ana, room, { private: true });
  await openChannel(ana, room);
  // The room's relay id, off the route Ana's client opened. Rooms are keyed by a
  // random UUID (upstream's contract), so the name's stamp is not in the id.
  const roomId = openChannelId(ana);
  const history = `crew note ${stamp}: rotation starts Monday`;
  await say(ana, history);
  await expect(messages(ana.page).filter({ hasText: history })).toBeVisible({
    timeout: 45_000,
  });

  // Bo has not been added yet, and the room is private — so neither the room
  // nor its traffic may be reachable. Without this the "added" step below could
  // be satisfied by a room Bo could already see.
  await expect(bo.page.getByTestId("app-sidebar")).not.toContainText(room);

  // --- added ---------------------------------------------------------------
  // The reachable add path is the members dialog's search row. Search by
  // PUBKEY, not by name: the shared live relay holds same-named kind 0s from
  // earlier runs and the result row's testid carries the pubkey, so a name
  // search with `.first()` can add a stale dead identity — the same trap that
  // broke the DM and mention scenarios in `company-day.spec.ts`.
  await openMembersDialog(ana);
  await ana.page.getByTestId("channel-management-search-users").fill(bo.pubkey);
  const addRow = ana.page.getByTestId(`channel-user-search-result-${bo.pubkey}`);
  // 45 s, not 20: the row needs Bo's kind 0 to propagate through the relay's
  // search/profile reads, which under real traffic exceeds a 20 s budget.
  await expect(addRow).toBeVisible({ timeout: 45_000 });
  await addRow.click();
  await closeMembersDialog(ana);

  // The relay's own receipt for the add, on Bo's already-open socket: a 44100
  // whose `p` names Bo and whose `h` names this room. A sidebar entry alone
  // cannot distinguish this from the channel-list poll firing.
  await expect
    .poll(
      () =>
        bo.received.some(
          (event) =>
            event.kind === 44100 &&
            event.tags.some((tag) => tag[0] === "p" && tag[1] === bo.pubkey) &&
            event.tags.some((tag) => tag[0] === "h" && tag[1] === roomId),
        ),
      {
        timeout: 90_000,
        message: "no kind 44100 membership notice for this room reached Bo",
      },
    )
    .toBe(true);

  // And the UI that notice drove: the private room is now Bo's, with its
  // history — which the relay only serves to a member.
  await expect(bo.page.getByTestId("app-sidebar")).toContainText(room, {
    timeout: 90_000,
  });
  await openChannel(bo, room);
  await expect(messages(bo.page).filter({ hasText: history })).toBeAttached({
    timeout: 60_000,
  });

  // --- a plain member holds no privileged edit ------------------------------
  await openManagementPanel(bo);
  expect(
    await canEditChannel(bo),
    "a plain member was offered the privileged channel edit",
  ).toBe(0);
  // The panel itself IS there — so what is missing is the capability, not the
  // screen. Without this the assertion above would also pass if the panel had
  // failed to open at all.
  await expect(
    bo.page.getByTestId("channel-management-member-count"),
  ).toBeVisible({ timeout: 45_000 });
  // The panel is left OPEN rather than closed. There is no close affordance to
  // click in summary view (`channel-management-back` only mounts in the canvas
  // view, and the overlay layout suppresses Escape), and the next step reloads
  // this tab anyway — which unmounts it. Clicking a nonexistent button here is
  // how an earlier revision of this scenario failed.

  // --- promoted ------------------------------------------------------------
  await openMembersDialog(ana);
  await pickFromMemberMenu(ana.page, bo.pubkey, {
    submenu: `sidebar-change-role-${bo.pubkey}`,
    item: `sidebar-role-admin-${bo.pubkey}`,
  });
  await closeMembersDialog(ana);

  await reopenWorkspace(bo, room);
  await openManagementPanel(bo);
  // Same locator that was absent a moment ago. Only the relay's roster changed.
  await expect(bo.page.getByTestId("channel-management-edit")).toBeVisible({
    timeout: 90_000,
  });

  // An admin may rename: `name` is a privileged 9002 tag, and the relay refuses
  // one from a member with "only owners/admins may change this channel setting".
  const renamed = `crew-handbook-${stamp}`;
  await bo.page.getByTestId("channel-management-edit").click();
  await bo.page.getByTestId("channel-management-name").fill(renamed);
  await bo.page.getByTestId("channel-management-save-changes").click();

  // THE receipt for the promotion: Ana's sidebar carries a name Bo chose. That
  // label can only come from the relay re-projecting the room's 39000, which
  // can only have happened if the relay accepted a privileged edit from Bo.
  await expect(ana.page.getByTestId("app-sidebar")).toContainText(renamed, {
    timeout: 90_000,
  });

  // --- demoted -------------------------------------------------------------
  await openMembersDialog(ana);
  await pickFromMemberMenu(ana.page, bo.pubkey, {
    submenu: `sidebar-change-role-${bo.pubkey}`,
    item: `sidebar-role-member-${bo.pubkey}`,
  });
  await closeMembersDialog(ana);

  await reopenWorkspace(bo, renamed);
  await openManagementPanel(bo);
  expect(
    await canEditChannel(bo),
    "the privileged channel edit survived a demotion to member",
  ).toBe(0);

  // --- removed -------------------------------------------------------------
  await openMembersDialog(ana);
  await pickFromMemberMenu(ana.page, bo.pubkey, {
    item: `sidebar-remove-member-${bo.pubkey}`,
  });
  await closeMembersDialog(ana);

  // The removal's notice is RECORDED, not asserted — because on this relay it
  // never arrives for a private room, and that is a live product bug rather
  // than a flake.
  //
  // Measured three times outside the browser entirely: a 9001 against a PRIVATE
  // channel is accepted, and no 44101 reaches the subject's `#p` subscription,
  // while the identical removal in an OPEN channel delivers one. The cause is
  // delivery-time access control, not minting — this room's 44100 WAS delivered
  // live at add time (asserted above), and after the removal that same 44100 is
  // no longer readable by that same subscription. The relay refuses a non-member
  // any event carrying an `h` tag for a private channel, the notices carry
  // exactly that tag, and the revocation commits before the notice is pushed
  // (`effects::bump_revocation`, deliberately ordered that way). So the
  // revocation censors the notice announcing the revocation, and the one case
  // where losing access matters is the one case nobody is told about.
  //
  // Making this room OPEN would turn the assertion green and make the scenario
  // weaker: an open room is writable by any authenticated pubkey whether or not
  // they are a member (measured — a removed member's kind 9 into an open room is
  // still accepted), so the removal would stop meaning anything. Keeping the
  // room private and logging the gap keeps both the strong removal receipt below
  // and a per-run report of the defect.
  const sawRemovalNotice = await waitForNotice(bo, 44101, 30_000, roomId);
  console.log(
    sawRemovalNotice
      ? "kind 44101 removal notice reached the removed member — the private-room notice gap appears FIXED, restore the assertion"
      : "KNOWN BUG (private-room removal notice): no kind 44101 reached the removed member in 30s; their client can only discover the removal on its 60s channel-list poll",
  );

  // The receipt that does hold, and it is a relay one: the room is gone from
  // Bo's workspace after a full remount, so it is the relay's membership read
  // that dropped it, not a stale client filter or a local echo.
  await bo.page.reload();
  await expect(bo.page.getByTestId("app-sidebar")).toBeVisible({
    timeout: 90_000,
  });
  await expect(bo.page.getByTestId("app-sidebar")).not.toContainText(renamed, {
    timeout: 90_000,
  });
  report(ana, bo);
  await ana.ctx.close();
  await bo.ctx.close();
});

/**
 * A room is archived, a member's client goes read-only, and it comes back.
 *
 * Archiving is a 9002 metadata edit carrying `archived` alone (nothing else may
 * ride along — a 9002 is authorized per tag, and an archived room refuses every
 * edit except the unarchive, so a bundled write could never be undone). The
 * relay's `archived_at` is real state: `Channel::is_writable` goes false and
 * every write is refused with "channel is archived or deleted" while reads keep
 * working, so the history does not vanish.
 *
 * `company-day.spec.ts` scenario 12 deliberately does not assert any of this,
 * and its comment says why: at the time `archive_channel` existed only in the
 * e2e MOCK backend, so the menu item threw and the room stayed writable. It is
 * answered now (`browser-backend/channelAdmin.ts`), which is what makes this
 * scenario possible — and the reason the writes are asserted from BOTH sides is
 * that the old failure looked exactly like a success from the archiver's tab.
 *
 * An archived room does NOT disappear for its members in this build, and the
 * scenario asserts what happens instead. `get_channels` maps the projection's
 * `archived` tag to `archived_at` and returns the room like any other
 * (`browser-backend/commands.ts`), and no sidebar code filters on it — so the
 * room keeps its place in the list and turns READ-ONLY. Asserting "it left the
 * sidebar" would fail against a product that is behaving as built.
 *
 * The archived 39000 is taped off Bo's socket. Bo's client subscribes to room
 * projections live (`features/channels/useLiveRoomProjections.ts`) and refreshes
 * its channel list when one arrives, so the read-only composer and the restored
 * one are budgeted well under one 60 s channel-list poll interval: neither can
 * pass on the poll alone.
 */
test("2. a room is archived, a member goes read-only, then it is restored", async ({
  browser,
}) => {
  test.setTimeout(SCENARIO_BUDGET_MS);
  const ana = await onboard(browser, ANA);
  const bo = await onboard(browser, BO, { watchKinds: [39000] });
  const room = `ops-room-${stamp}`;

  await createChannel(ana, room);
  await openChannel(ana, room);
  const roomId = openChannelId(ana);

  // Bo joins the OPEN room the way a person does: browse, preview, Join. The
  // row click only previews (`ChannelBrowserDialog.handleSelect` navigates
  // read-only); the separate Join is what makes `is_member` true, and without
  // it the member-filtered sidebar never lists the room.
  await bo.page.getByTestId("section-actions-channels-quick-create").click();
  const entry = bo.page.getByRole("button", { name: new RegExp(room) }).first();
  await expect(entry).toBeVisible({ timeout: 45_000 });
  await entry.click();
  await bo.page.keyboard.press("Escape");
  await bo.page.getByRole("button", { name: /^Join$/ }).first().click();
  await openChannel(bo, room);

  // Bo can write BEFORE the archive. This is what makes the silence after it a
  // refusal rather than a composer that never worked — same helper, same room.
  const before = `ops ${stamp}: pager rotation swapped`;
  await say(bo, before);
  await expect(messages(ana.page).filter({ hasText: before })).toBeAttached({
    timeout: 60_000,
  });

  // --- archived ------------------------------------------------------------
  await openManagementPanel(ana);
  await ana.page.getByTestId("channel-management-archive").click();
  // Ana's own panel flips to the archived state. This is NOT the receipt, and
  // saying so matters: `useArchiveChannelMutation.onSuccess` writes
  // `archivedAt` into the cache itself (`setChannelArchivedState`), so these two
  // assertions are satisfied by the optimistic local write before the relay has
  // answered anything. They are here only to confirm the click reached the
  // mutation and to reach the Unarchive control below. Everything that proves
  // the relay did the work is asserted on BO's tab, which ran no mutation.
  // (The synced client has no separate "archived" badge any more; the sheet's
  // Archive row turns into Unarchive, and that swap is the archived state.)
  await expect(
    ana.page.getByTestId("channel-management-unarchive"),
  ).toBeVisible({ timeout: 90_000 });
  await expect(ana.page.getByTestId("channel-management-archive")).toHaveCount(0);

  // The relay's re-projected room document, as Bo's client read it: same room,
  // now carrying `archived`.
  await expect
    .poll(
      () =>
        bo.received.some(
          (event) =>
            event.kind === 39000 &&
            event.tags.some((tag) => tag[0] === "d" && tag[1] === roomId) &&
            event.tags.some((tag) => tag[0] === "archived" && tag[1] === "true"),
        ),
      {
        timeout: 30_000,
        message:
          "Bo's client never received a 39000 for this room carrying archived=true",
      },
    )
    .toBe(true);

  // Bo's composer goes read-only without a reload. `editor()` selects the
  // contenteditable, and Tiptap renders `contenteditable="false"` when the
  // composer is disabled, so the writable field is not merely greyed, it is
  // absent. Budgeted under one poll interval: the live projection drives it.
  await expect(editor(bo.page)).toHaveCount(0, { timeout: 30_000 });
  await expect(bo.page.getByTestId("message-composer")).toBeVisible();

  // The honest consequence: the write does not happen. Typing is attempted
  // anyway — a disabled composer swallows it — and neither side ever renders
  // the line. The relay refuses it too ("channel is archived or deleted"), so
  // there is no path by which this text could land.
  const during = `ops ${stamp}: this must never post`;
  await editor(bo.page)
    .click({ timeout: 5_000 })
    .catch(() => undefined);
  await bo.page.keyboard.type(during);
  await bo.page.keyboard.press("Enter");
  await bo.page.waitForTimeout(8_000);
  await expect(messages(bo.page).filter({ hasText: during })).toHaveCount(0);
  await expect(messages(ana.page).filter({ hasText: during })).toHaveCount(0);

  // --- restored ------------------------------------------------------------
  await ana.page.getByTestId("channel-management-unarchive").click();
  await expect(ana.page.getByTestId("channel-management-archive")).toBeVisible({
    timeout: 90_000,
  });

  // The unarchive's receipt is that the room is WRITABLE again, from the other
  // person's tab, and that their line reaches Ana. A restored room that nobody
  // can post in would satisfy any "the badge went away" assertion. Live-driven
  // like the archive half, so budgeted under one poll interval.
  await expect(editor(bo.page)).toHaveCount(1, { timeout: 30_000 });
  const after = `ops ${stamp}: back on the air`;
  await say(bo, after);
  await expect(messages(ana.page).filter({ hasText: after })).toBeAttached({
    timeout: 90_000,
  });
  report(ana, bo);
  await ana.ctx.close();
  await bo.ctx.close();
});

/**
 * A message is retracted and the relay stops serving it.
 *
 * NIP-09 kind 5. The weak version of this test — "the author's own row went
 * away" — proves nothing: every buzz client hides any message named by any
 * kind:5 it sees, so a deletion the relay never applied looks identical from
 * the deleter's tab. (That is not hypothetical; it is why `channelAdmin.ts`
 * refuses to publish a deletion for somebody else's message, which this relay
 * accepts and then ignores.)
 *
 * So the proof is in three parts, escalating:
 *   1. the kind 5 reaches the SECOND person's socket with an `e` tag naming the
 *      root's real relay id — the wire receipt,
 *   2. their timeline drops the row live,
 *   3. and after a full reload it is still gone — which is the only one of the
 *      three that cannot be produced by a client-side hide, because a reloaded
 *      tab re-asks the relay for the room's history.
 */
test("3. a message is retracted and stops being served", async ({ browser }) => {
  test.setTimeout(SCENARIO_BUDGET_MS);
  // Ana taps kind 9 for the root's durable relay id; Bo taps kind 5, which the
  // live channel subscription genuinely carries (`CHANNEL_EVENT_KINDS`).
  const ana = await onboard(browser, ANA, { watchKinds: [9] });
  const bo = await onboard(browser, BO, { watchKinds: [5] });
  const room = `retract-${stamp}`;

  await createChannel(ana, room);
  await openChannel(ana, room);

  await bo.page.getByTestId("section-actions-channels-quick-create").click();
  const entry = bo.page.getByRole("button", { name: new RegExp(room) }).first();
  await expect(entry).toBeVisible({ timeout: 45_000 });
  await entry.click();
  await bo.page.keyboard.press("Escape");
  await bo.page.getByRole("button", { name: /^Join$/ }).first().click();
  await openChannel(bo, room);

  const doomed = `misfire ${stamp}: wrong channel, sorry`;
  await say(ana, doomed);
  const row = messages(ana.page).filter({ hasText: doomed }).first();
  await expect(row).toBeVisible({ timeout: 45_000 });
  await expect(messages(bo.page).filter({ hasText: doomed })).toBeAttached({
    timeout: 60_000,
  });

  // The relay's id for the message, taken off Ana's socket. Until the echo
  // arrives the row carries a client-minted `optimistic-<uuid>` id that exists
  // nowhere on the relay, so an `e`-tag assertion against it could never pass.
  let rootId = "";
  await expect
    .poll(
      () => {
        rootId =
          ana.received.find(
            (event) => event.kind === 9 && event.content.includes(doomed),
          )?.id ?? "";
        return rootId;
      },
      { timeout: 90_000 },
    )
    .toMatch(/^[0-9a-f]{64}$/);

  // Delete from the row's own action bar. The testids are keyed by message id,
  // and the DOM may still hold the optimistic one, so the locators are scoped
  // to the hovered ROW by testid PREFIX — the same reason `company-day`'s
  // thread scenario clicks `[data-testid^="reply-message-"]`.
  await row.scrollIntoViewIfNeeded();
  await row.hover();
  await row
    .locator('[data-testid^="more-actions-"]')
    .first()
    .click({ timeout: 20_000 });
  await ana.page
    .locator('[data-testid^="delete-message-"]')
    .first()
    .click({ timeout: 20_000 });
  // The confirmation carries no testid of its own; scope to the dialog so the
  // exact-name match cannot pick up the "Delete message?" title.
  await ana.page
    .getByRole("alertdialog")
    .getByRole("button", { name: /^Delete$/ })
    .click({ timeout: 20_000 });

  // 1. The wire receipt on Bo's socket: a kind 5 naming the root's relay id.
  await expect
    .poll(
      () =>
        bo.received.some(
          (event) =>
            event.kind === 5 &&
            event.tags.some((tag) => tag[0] === "e" && tag[1] === rootId),
        ),
      {
        timeout: 90_000,
        message: "no kind 5 naming the message reached Bo's socket",
      },
    )
    .toBe(true);

  // 2. Bo's live timeline drops it.
  await expect(messages(bo.page).filter({ hasText: doomed })).toHaveCount(0, {
    timeout: 90_000,
  });

  // 3. The claim that matters: the relay no longer SERVES it. A reloaded tab
  //    re-reads the room's history from the relay, so this can only pass if the
  //    deletion was applied rather than merely stored.
  await bo.page.reload();
  await expect(bo.page.getByTestId("app-sidebar")).toBeVisible({
    timeout: 90_000,
  });
  await openChannel(bo, room);
  // Bo's own line proves the reloaded timeline really did load this room's
  // history — without it, "the deleted text is absent" would also be true of a
  // timeline that rendered nothing at all.
  const survivor = `still here ${stamp}: unaffected by the retraction`;
  await say(bo, survivor);
  await expect(messages(bo.page).filter({ hasText: survivor })).toBeVisible({
    timeout: 60_000,
  });
  await expect(messages(bo.page).filter({ hasText: doomed })).toHaveCount(0);
  report(ana, bo);
  await ana.ctx.close();
  await bo.ctx.close();
});

/**
 * A community timeout silences a member in their own live session, and a
 * non-moderator cannot issue one.
 *
 * Moderation commands are kinds 9040-9044 and the relay enforces its moderator
 * set FAIL-CLOSED: an absent, unreadable or non-matching set means "not a
 * moderator", and the command is refused before any work
 * (`ingress.rs::ingest_event`). So the negative half of this scenario is the
 * relay's own answer, quoted, and it is checked twice over — the refusal
 * sentence, and the fact that the target can still write afterwards.
 *
 * The moderator half is driven over the relay's HTTP face rather than through
 * the UI, and that is a REPORTED GAP, not a shortcut: the browser build can
 * never render the moderator affordances, because they are gated on a relay
 * owner/admin role that `relay_requires_membership: false` makes permanently
 * unknowable (see the file header). The RECEIPT is still Bo's real browser
 * session — a live, authenticated socket that has been posting successfully all
 * scenario, which then stops being able to.
 *
 * `composer-timeout-banner` is asserted during the timeout (the relay's refusal
 * now carries the parseable expiry). The banner lives in an in-memory store that
 * only a send rejection sets (`features/moderation/lib/timeoutStore.ts`, upstream's
 * reactive ruling: no proactive self-restriction read), so a live session cannot
 * learn of an EARLY lift — after the 9043 the member reloads, as a person looking
 * at a stale banner would, and the write must then land.
 */
test("4. a timed-out member cannot write, and a non-moderator cannot time anyone out", async ({
  browser,
}) => {
  test.setTimeout(SCENARIO_BUDGET_MS);
  const secret = operatorSecret();
  test.skip(
    secret === null,
    `no operator key at ${PROVISION_FILE}; the community moderator set is only reachable from the operator plane, so this scenario cannot run`,
  );
  const operator = secret as Uint8Array;

  const ana = await onboard(browser, ANA);
  const bo = await onboard(browser, BO);
  const room = `moderated-${stamp}`;

  await createChannel(ana, room);
  await openChannel(ana, room);
  await bo.page.getByTestId("section-actions-channels-quick-create").click();
  const entry = bo.page.getByRole("button", { name: new RegExp(room) }).first();
  await expect(entry).toBeVisible({ timeout: 45_000 });
  await entry.click();
  await bo.page.keyboard.press("Escape");
  await bo.page.getByRole("button", { name: /^Join$/ }).first().click();
  await openChannel(bo, room);

  // Bo can write. Everything below is measured against this.
  const allowed = `moderated ${stamp}: first post, before any timeout`;
  await say(bo, allowed);
  await expect(messages(ana.page).filter({ hasText: allowed })).toBeAttached({
    timeout: 60_000,
  });

  // Whatever the deployment already had, so the scenario can put it back. A
  // moderator set is community-wide state on a SHARED relay: leaving this
  // run's moderator behind would silently grant a stranger's key moderation
  // authority over every later run.
  const before = await relayRequest(
    operator,
    "GET",
    "/buzz/api/operator/config/buzz-community-moderators",
  );
  expect(
    before.status,
    `the operator plane refused a read (${said(before)}) — the key in ${PROVISION_FILE} is not an operator of this deployment`,
  ).toBe(200);
  const priorValue = (
    JSON.parse(before.body) as { present?: boolean; value?: unknown }
  ).value;

  const moderator = identity(`channel-lifecycle-moderator-${stamp}`);
  const bystander = identity(`channel-lifecycle-bystander-${stamp}`);
  const expiresAt = Math.floor(Date.now() / 1000) + 3600;
  const timeoutTags = (target: string) => [
    ["p", target],
    ["expiration", String(expiresAt)],
  ];

  try {
    // --- fail-closed: nobody is a moderator yet ---------------------------
    const refused = await publishAs(bystander, 9042, timeoutTags(bo.pubkey));
    // Quoting the relay is the point. A test that only checked "not 200" would
    // also pass on a malformed event, a rate limit, or a route that does not
    // exist — none of which would prove the moderator gate exists at all. 403,
    // not 401: the caller authenticated and is not permitted (the relay's
    // `http_status` maps `Unauthorized` to 403, pinned by its own unit test).
    expect(refused.status).toBe(403);
    expect(refused.body).toContain(
      "only a community moderator may issue this command",
    );

    // And the refusal was total: no restriction was written, so Bo can still
    // write. This is the half that makes the check above more than a string
    // match on an error page.
    const stillAllowed = `moderated ${stamp}: the refused timeout changed nothing`;
    await say(bo, stillAllowed);
    await expect(
      messages(ana.page).filter({ hasText: stillAllowed }),
    ).toBeAttached({ timeout: 60_000 });

    // --- a real moderator times Bo out -----------------------------------
    const seeded = await relayRequest(
      operator,
      "POST",
      "/buzz/api/operator/config/buzz-community-moderators",
      { value: [moderator.pubkey] },
    );
    expect(seeded.status, `seeding the moderator set failed: ${said(seeded)}`).toBe(
      200,
    );

    const applied = await publishAs(moderator, 9042, timeoutTags(bo.pubkey));
    expect(applied.status, `the 9042 timeout was refused: ${said(applied)}`).toBe(
      200,
    );

    // Bo's live session — the same tab, the same socket, the same composer that
    // has posted twice already — can no longer put a message in the room. The
    // relay refuses the write at the auth seam
    // ("restricted: you are timed out until <unix seconds>"), so the send is rolled back
    // and the row never survives. Polled rather than asserted once, because
    // the client learns the block only by ATTEMPTING a send: there is no
    // proactive self-restriction read in this build.
    const blocked = `moderated ${stamp}: must never reach the room`;
    await expect
      .poll(
        async () => {
          const field = editor(bo.page);
          if ((await field.count()) === 0) return "composer-gone";
          await field.click().catch(() => undefined);
          await bo.page.keyboard.type(blocked);
          await bo.page.keyboard.press("Enter");
          await bo.page.waitForTimeout(4_000);
          return (await messages(bo.page).filter({ hasText: blocked }).count()) ===
            0
            ? "refused"
            : "posted";
        },
        {
          timeout: 120_000,
          message: "a timed-out member's message stayed in their own timeline",
        },
      )
      .not.toBe("posted");

    // ANA is the stronger witness: her tab can only show what the relay served
    // her, so its absence there is the relay's refusal rather than Bo's client
    // rolling back an optimistic row.
    await expect(messages(ana.page).filter({ hasText: blocked })).toHaveCount(0);
    // And Bo's client parsed the relay's refusal into its timeout state.
    await expect(bo.page.getByTestId("composer-timeout-banner")).toBeVisible({
      timeout: 30_000,
    });

    // --- lifted ------------------------------------------------------------
    // Which proves the block above was the timeout and nothing else: the only
    // thing that changes here is the relay's restriction state.
    const lifted = await publishAs(moderator, 9043, [["p", bo.pubkey]]);
    expect(lifted.status, `the 9043 untimeout was refused: ${said(lifted)}`).toBe(
      200,
    );
    // The client holds the recorded expiry until a reload (see the scenario's
    // doc comment), so Bo reloads before trying again. The route keeps the room
    // open across the reload, so the composer is the readiness signal.
    await bo.page.reload();
    await expect(bo.page.getByTestId("app-sidebar")).toBeVisible({
      timeout: 90_000,
    });
    await expect(bo.page.getByTestId("message-composer")).toBeVisible({
      timeout: 90_000,
    });
    await expect(bo.page.getByTestId("composer-timeout-banner")).toHaveCount(0);
    const restored = `moderated ${stamp}: writing again after the lift`;
    await expect
      .poll(
        async () => {
          const field = editor(bo.page);
          if ((await field.count()) === 0) return false;
          await field.click().catch(() => undefined);
          await bo.page.keyboard.type(restored);
          await bo.page.keyboard.press("Enter");
          await bo.page.waitForTimeout(4_000);
          return (
            (await messages(ana.page).filter({ hasText: restored }).count()) > 0
          );
        },
        {
          timeout: 120_000,
          message: "the member never regained the ability to write",
        },
      )
      .toBe(true);
  } finally {
    // Lift the timeout BEFORE standing the moderator down, in that order: the
    // 9043 is only authorized while this run's key is still in the set. A
    // timeout is community-wide state, and while the target is a throwaway
    // identity, a failure part-way through the block above would otherwise
    // leave a restriction behind on a shared relay for an hour.
    await publishAs(moderator, 9043, [["p", bo.pubkey]]).catch(() => undefined);
    // Then hand the community back its own moderator set — `value: null`
    // deletes the key, restoring the documented absent-default.
    const restore = await relayRequest(
      operator,
      "POST",
      "/buzz/api/operator/config/buzz-community-moderators",
      { value: priorValue ?? null },
    );
    if (restore.status !== 200) {
      console.log(
        `WARNING: could not restore buzz-community-moderators (${said(restore)}) — this run may have left its moderator key holding authority`,
      );
    }
  }
  report(ana, bo);
  await ana.ctx.close();
  await bo.ctx.close();
});
