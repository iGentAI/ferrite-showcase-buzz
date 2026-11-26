/**
 * CUSTOM EMOJI (NIP-30, kind 30030) and USER STATUS (NIP-38, kind 30315),
 * driven through buzz's own client against a LIVE relay.
 *
 * Both are per-member addressable state the relay stores globally and every
 * other member reads: a custom-emoji set is `(pubkey, 30030,
 * "buzz:custom-emoji")` and the "community palette" is the client-side UNION of
 * every member's set (`shared/api/customEmoji.ts::unionCustomEmoji`); a status
 * is `(pubkey, 30315, "general")` with the text in `content` and the glyph in an
 * `emoji` tag. Neither has any local store, so every assertion below is
 * necessarily about what the relay served.
 *
 *   BUZZ_UI=http://localhost:4173/buzz/ui/index.html \
 *   BUZZ_WS=ws://localhost:4173/buzz/ \
 *   npx playwright test e2e/emoji-and-status.spec.ts --reporter=line --workers=1
 *
 * ── WHAT CHANGED HERE, stated before anything else ─────────────────────────
 *
 * ADDING a custom emoji USED TO BE unreachable in the browser build, and this
 * file was built around that. The add form's submit is gated on
 * `pendingUpload !== null` (`CustomEmojiSettingsCard`: `canSubmit`), the only
 * thing that can set it is `pickAndUploadMedia()` → `invoke
 * ("pick_and_upload_media")`, and that command had no case in
 * `shared/browser-backend/commands.ts` — the whole media family fell through to
 * `unavailableCommandMessage`.
 *
 * It is implemented now (`commands.ts`, `pick_and_upload_media` →
 * `browser-backend/media.ts`, against the relay's own `/buzz/api/media/*`
 * routes). The one refusal left in that family is `upload_media`, which takes a
 * file PATH and reads it — the thing a tab genuinely cannot do.
 *
 * So scenario 1's second half now asserts the CAPABILITY rather than its
 * absence: the click opens a file chooser, a real PNG is hashed and declared to
 * the relay, and `custom-emoji-add` ARMS. That is the path the old assertion
 * existed to say was impossible.
 *
 * It still stops short of publishing the 30030. Doing so would leave a shortcode
 * in a SHARED community palette, which is exactly what this file's `finally`
 * blocks exist to prevent; the armed button is the last point where the evidence
 * is unambiguous and the cleanup stays honest.
 *
 * The uploaded PNG needs no cleanup and gets none: its bytes are FIXED, so
 * content-addressing gives every run the same object rather than one each. See
 * the comment at `setFiles` — that is the whole reason it is the one value in
 * this file that does not carry the run stamp.
 *
 * Everything DOWNSTREAM of the upload is real and is tested, with the palette
 * seeded by a second member publishing their own 30030 over the relay's HTTP
 * ingest — byte-for-byte the event `publishOwnSet()` would have signed. That
 * substitution is about keeping the SHARED palette clean rather than about a
 * missing capability: it lets the run retract the set in a `finally` (a
 * replaceable event with no `emoji` tags) instead of leaving a shortcode in the
 * community's palette forever.
 *
 * ── OVERLAP WITH AN EXISTING SPEC, declared rather than discovered ──────────
 *
 * `identity-and-dms.spec.ts` already covers "a published status reaches the
 * colleague who looks the author up", and scenario 3 below deliberately repeats
 * that shape as its SETUP. What it adds is the three things that file does not
 * assert: the 30315 frame arriving on the second person's own socket (so the
 * live subscription is exercised, not just an eventual read), an `emoji` tag on
 * the event (taken from a preset, which that scenario skips), and CLEARING —
 * which is a different write and had no coverage at all. If the two ever need to
 * be one file, this scenario is the superset.
 *
 * `live.spec.ts` scenario 9 also mentions invites and status, but it drives a
 * DIFFERENT surface: the relay-served hash-routed bundle (`#/invite/<code>`,
 * `invite-code`, `join-button`), not this port. Nothing here relates to it.
 *
 * Also not asserted, and why: nothing clicks a custom emoji inside the
 * emoji-mart picker. Its emoji buttons take their accessible name from
 * `skins[0].native`, which a custom emoji does not have (`aria-label` is
 * literally undefined), and its category rows render lazily — so a click there
 * would depend on shadow-DOM internals and scroll position rather than on
 * anything buzz owns. The reaction pill is the same mutation (`onSelect` →
 * `useToggleReaction` → `add_reaction`) through an affordance with a real name,
 * so scenario 2 uses that.
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
import { identity, publishAs, said } from "./relayHttp";

const stamp = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
const ANA = `Ana Ruiz ${stamp}`;
const BO = `Bo Nilsson ${stamp}`;

/** Two onboardings plus a settings walk on a live relay — see channel-lifecycle. */
const SCENARIO_BUDGET_MS = 900_000;

/** A NIP-30 shortcode: `[a-z0-9_-]+`, which the run stamp already satisfies. */
const SHORTCODE = `parrot-${stamp}`;
/**
 * The image URL the seeded set points at.
 *
 * It carries the run stamp so every later locator can match on it, and it is
 * NOT expected to resolve: no scenario asserts the image loads. What the URL is
 * for is identity — it travels from the seeded 30030 into the palette, into the
 * kind-7's NIP-30 `emoji` tag, and back out into an `<img src>`, and matching
 * the stamp along that path is what proves the same URL made the whole trip.
 */
const EMOJI_URL = `http://localhost:4173/buzz/api/assets/${SHORTCODE}.png`;
const CUSTOM_EMOJI_D_TAG = "buzz:custom-emoji";

/** Report what the page complained about, whether or not the scenario passed. */
function report(...people: Person[]) {
  for (const person of people) {
    if (person.problems.length) {
      console.log(`--- ${person.name} ---\n${[...new Set(person.problems)].join("\n")}`);
    }
  }
}

/** Open Settings the way a person does — it is a route, reached from the profile card. */
async function openSettings(person: Person, section: string) {
  await person.page.getByTestId("open-settings").click({ timeout: 45_000 });
  await person.page
    .getByTestId("profile-popover-settings")
    .click({ timeout: 45_000 });
  await expect(person.page.getByTestId("settings-view")).toBeVisible({
    timeout: 60_000,
  });
  await person.page
    .getByTestId(`settings-nav-${section}`)
    .click({ timeout: 45_000 });
}

/**
 * Another member's custom emoji reaches the palette; the local add path works.
 *
 * The two halves belong together because each is what makes the other
 * meaningful: the community section proving a relay-published shortcode is
 * present shows the palette machinery works end to end, and the upload half
 * shows the local add path reaching the same relay through its media routes.
 * Reading either alone — a palette nobody can add to, or an armed button on a
 * screen that never loaded anything — would prove much less.
 */
test("1. a member's emoji set reaches the community palette; adding one works", async ({
  browser,
}) => {
  test.setTimeout(SCENARIO_BUDGET_MS);
  const painter = identity(`emoji-painter-${stamp}`);

  // Seeded BEFORE onboarding so Ana's first palette read already contains it —
  // the live 30030 subscription would also invalidate into it
  // (`useCommunityEmojiLiveUpdates`), but a scenario should not depend on a race
  // it does not need.
  const seeded = await publishAs(painter, 30030, [
    ["d", CUSTOM_EMOJI_D_TAG],
    ["emoji", SHORTCODE, EMOJI_URL],
  ]);
  expect(
    seeded.status,
    `the relay refused a member's kind 30030 set: ${said(seeded)}`,
  ).toBe(200);

  // Onboarding is INSIDE the try because it is the step most likely to fail on
  // this relay (see `onboarding.ts`'s retry loop), and a failure there must
  // still retract the seeded set — otherwise a stalled run leaves a dead
  // shortcode in the shared community's palette permanently.
  let ana: Person | null = null;
  try {
    ana = await onboard(browser, ANA);
    await openSettings(ana, "custom-emoji");
    const card = ana.page.getByTestId("settings-custom-emoji");
    await expect(card).toBeVisible({ timeout: 45_000 });

    // THE PALETTE RECEIPT. `custom-emoji-community` renders the union MINUS the
    // caller's own shortcodes, so a shortcode appearing here is necessarily
    // another member's 30030 read back off the relay — Ana published nothing.
    await expect(
      ana.page.getByTestId("custom-emoji-community"),
      "another member's custom emoji never reached the community palette",
    ).toContainText(`:${SHORTCODE}:`, { timeout: 90_000 });
    // And the split is real rather than a single list rendered twice: Ana's own
    // section shows its empty state, because she owns no set.
    await expect(ana.page.getByTestId("custom-emoji-mine")).toContainText(
      /haven.t added any emoji yet/i,
      { timeout: 45_000 },
    );
    await expect(ana.page.getByTestId("custom-emoji-mine")).not.toContainText(
      `:${SHORTCODE}:`,
    );

    // --- and the add path WORKS, end to end ---------------------------------
    // A name alone does not arm the form: `canSubmit` also needs an upload. This
    // half is unchanged, and it is what makes the next step meaningful — an
    // armed button afterwards can only have come from the upload.
    await ana.page.getByTestId("custom-emoji-name-input").fill(`mine-${stamp}`);
    await expect(
      ana.page.getByTestId("custom-emoji-add"),
      "the add button armed before any upload, so arming proves nothing",
    ).toBeDisabled();

    // The picker OPENS — the step that used to be a refusal. Awaited as an event
    // rather than probed for afterwards, because a file chooser is modal state
    // rather than DOM.
    const chooser = ana.page.waitForEvent("filechooser", { timeout: 30_000 });
    await ana.page.getByTestId("custom-emoji-upload").click({ timeout: 20_000 });
    const picker = await chooser;

    // A REAL png: the 8-byte signature plus filler, which is what the relay's
    // magic-number check reads. Bytes rather than a fixture file so the spec
    // carries its own input and cannot drift from one.
    //
    // DELIBERATELY NOT STAMPED, unlike everything else in this file. Media is
    // content-addressed (`media/{sha256}.{ext}`) and there is no delete route,
    // so unique bytes would mean one permanent object per run on a shared relay
    // — and the `finally` below can retract a 30030 but not a blob. Stable bytes
    // make every run resolve to the same digest instead: the first upload
    // anywhere creates the object and every later run is answered `have: true`
    // by `/media/begin` and transfers nothing.
    //
    // The cost, stated rather than hidden: after the first run this exercises
    // the DEDUP branch rather than the transfer. Both arm the button, which is
    // what is asserted here, and the transfer path is covered where it is the
    // subject rather than a means — the media gates, and
    // the HTTP end-to-end driver's media search.
    await picker.setFiles({
      // The stamp rides the FILENAME, which is not part of the address, so a
      // trace still says which run produced it.
      name: `mine-${stamp}.png`,
      mimeType: "image/png",
      buffer: Buffer.concat([
        Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
        Buffer.from("buzz-e2e-custom-emoji-fixture".padEnd(512, ".")),
      ]),
    });

    // THE RECEIPT: the button arms — which the old assertion said was impossible.
    //
    // Precisely what that establishes, in EITHER branch: `pickAndUploadMedia`
    // hashed the file, `/buzz/api/media/begin` accepted the declaration, and a
    // usable `pendingUpload` came back that the form took. What it does NOT
    // distinguish is whether the bytes travelled (`/buzz/api/media/chunk`
    // assembling and verifying them) or were recognised as already held — with
    // the fixed fixture above, the first run anywhere does the former and every
    // run after it does the latter.
    //
    // A generous budget either way: the first run is a real round trip to the
    // relay's blob plane, not a local state flip.
    await expect(
      ana.page.getByTestId("custom-emoji-add"),
      "the upload completed but the add form never armed",
    ).toBeEnabled({ timeout: 90_000 });
  } finally {
    // Hand the community's palette back. A 30030 with no `emoji` tags is a
    // replacement, so the shortcode leaves the union rather than accumulating
    // one dead entry per run on a shared relay.
    const cleared = await publishAs(painter, 30030, [["d", CUSTOM_EMOJI_D_TAG]]);
    if (cleared.status !== 200) {
      console.log(
        `WARNING: could not retract the seeded emoji set (${said(cleared)}) — :${SHORTCODE}: may be left in the shared community palette`,
      );
    }
    if (ana) {
      report(ana);
      await ana.ctx.close();
    }
  }
});

/**
 * A reaction carrying a CUSTOM shortcode, written by the browser client.
 *
 * NIP-25 kind 7 with the shortcode in `content`, plus the NIP-30
 * `["emoji", shortcode, url]` tag that tells every other client which image to
 * render (`browser-backend/commands.ts::add_reaction`). The scenario is built so
 * that the interesting step is the one the CLIENT does:
 *
 *   1. a second member reacts with the custom shortcode over HTTP, which gets a
 *      pill onto Ana's timeline that Ana's client did not create — and pins the
 *      image URL that arrived with it,
 *   2. Ana clicks that pill, which is `add_reaction` with the same shortcode,
 *   3. the receipt is Ana's OWN kind 7 off her socket, carrying both the
 *      shortcode content and the `emoji` tag, and the pill's count reaching two.
 *
 * Step 1 is also what makes step 3 unfakeable: the tally is the relay's
 * (`effects` reaction rows, re-read per window), so a count of two requires two
 * accepted events from two different keys.
 */
test("2. a reaction can carry a custom shortcode, and its tally is the relay's", async ({
  browser,
}) => {
  test.setTimeout(SCENARIO_BUDGET_MS);
  const painter = identity(`emoji-reactor-${stamp}`);

  const seeded = await publishAs(painter, 30030, [
    ["d", CUSTOM_EMOJI_D_TAG],
    ["emoji", SHORTCODE, EMOJI_URL],
  ]);
  expect(
    seeded.status,
    `the relay refused a member's kind 30030 set: ${said(seeded)}`,
  ).toBe(200);

  // kind 9 for the message's durable id, kind 7 for the reaction events — both
  // ride Ana's live channel subscription (`CHANNEL_EVENT_KINDS`). Onboarding is
  // inside the try for the same reason as scenario 1: a stall must still retract
  // the seeded set.
  let ana: Person | null = null;
  try {
    // Bound to a const as well: the polls below read `ana.received` inside a
    // closure, where the assignment above no longer narrows away the null.
    const her = (ana = await onboard(browser, ANA, { watchKinds: [7, 9] }));
    await openChannel(her, "General");
    const line = `emoji check ${stamp}: does the palette resolve`;
    await say(her, line);
    const row = messages(her.page).filter({ hasText: line }).first();
    await expect(row).toBeVisible({ timeout: 45_000 });
    // The relay's own id for the message: a reaction's `e` tag has to name it,
    // and until the echo arrives the row carries an `optimistic-<uuid>`.
    const messageId = await relayEventId(her, line);

    // --- somebody else reacts with the custom shortcode ---------------------
    const reacted = await publishAs(
      painter,
      7,
      [
        ["e", messageId],
        ["emoji", SHORTCODE, EMOJI_URL],
      ],
      `:${SHORTCODE}:`,
    );
    expect(
      reacted.status,
      `the relay refused a custom-shortcode reaction: ${said(reacted)}`,
    ).toBe(200);

    // It arrives on Ana's open socket and renders as a pill. The pill's
    // accessible name is `Toggle <emoji> reaction` (`ReactionPill`), so this
    // locator is exact rather than positional.
    const pill = row
      .getByTestId("message-reactions")
      .getByRole("button", { name: `Toggle :${SHORTCODE}: reaction` });
    await expect(
      pill,
      "a custom-shortcode reaction from another member never rendered",
    ).toBeVisible({ timeout: 90_000 });
    // The IMAGE proves the NIP-30 tag round-tripped: `formatTimelineMessages`
    // takes the pill's `emojiUrl` from the reaction event's own `emoji` tag, so
    // this src is the URL the other member published, carried by the relay.
    await expect(pill.locator("img")).toHaveAttribute(
      "src",
      new RegExp(SHORTCODE),
      { timeout: 45_000 },
    );
    // Ana has not reacted yet — the state the toggle below changes.
    await expect(pill).toHaveAttribute("aria-pressed", "false");

    // --- and Ana joins it -----------------------------------------------------
    await pill.click({ timeout: 20_000 });

    // THE WIRE RECEIPT: Ana's own kind 7, signed by her key, with the shortcode
    // as content and the NIP-30 tag alongside it. Without the tag every other
    // client would render a bare `:shortcode:` string, so the tag is the
    // interoperability half of the feature and is asserted explicitly.
    await expect
      .poll(
        () =>
          her.received.some(
            (event) =>
              event.kind === 7 &&
              event.pubkey === her.pubkey &&
              event.content === `:${SHORTCODE}:` &&
              event.tags.some((tag) => tag[0] === "e" && tag[1] === messageId) &&
              event.tags.some(
                (tag) =>
                  tag[0] === "emoji" &&
                  tag[1] === SHORTCODE &&
                  tag[2] === EMOJI_URL,
              ),
          ),
        {
          timeout: 90_000,
          message:
            "no kind 7 authored by Ana carried the custom shortcode and its NIP-30 emoji tag",
        },
      )
      .toBe(true);

    // The relay's tally, as the UI reads it back: two distinct keys reacted with
    // the same shortcode, so the pill counts two and reports Ana as one of them.
    await expect(pill).toContainText("2", { timeout: 90_000 });
    await expect(pill).toHaveAttribute("aria-pressed", "true", {
      timeout: 45_000,
    });
  } finally {
    const cleared = await publishAs(painter, 30030, [["d", CUSTOM_EMOJI_D_TAG]]);
    if (cleared.status !== 200) {
      console.log(
        `WARNING: could not retract the seeded emoji set (${said(cleared)}) — :${SHORTCODE}: may be left in the shared community palette`,
      );
    }
    if (ana) {
      report(ana);
      await ana.ctx.close();
    }
  }
});

/**
 * A status one person sets is read by another person's client, and clearing it
 * is read too.
 *
 * kind 30315, `d = "general"`, text in `content` and the glyph in an `emoji`
 * tag (`relayClientSession.publishUserStatus`). Every receipt here is on BO's
 * side, which is the only side that cannot be satisfied by a local echo: Ana's
 * own mutation writes her status straight into her own cache
 * (`useSetUserStatusMutation.onSuccess`), so asserting anything on Ana's tab
 * would prove nothing about the relay.
 *
 * The pair is what gives it teeth. A status that merely APPEARS could be a
 * stale read of anything; a status that appears and then goes away again, on the
 * same locator in the same tab, with nothing changing in between except two
 * events on the relay, cannot.
 */
test("3. a status set by one person is read by another, and cleared the same way", async ({
  browser,
}) => {
  test.setTimeout(SCENARIO_BUDGET_MS);
  // Bo taps 30315: the shipped client holds a LIVE subscription for it
  // (`useUserStatusSubscription` → `{kinds:[30315], "#d":["general"], limit:0}`),
  // so the frame is a genuine push to a second person rather than the answer to
  // a read Bo asked for.
  const ana = await onboard(browser, ANA);
  const bo = await onboard(browser, BO, { watchKinds: [30315] });
  await openChannel(ana, "General");
  await openChannel(bo, "General");

  // Bo needs a message row authored by Ana to open her profile from.
  const line = `status check ${stamp}: heads down this morning`;
  await say(ana, line);
  const anaRow = messages(bo.page).filter({ hasText: line }).first();
  await expect(anaRow).toBeAttached({ timeout: 90_000 });

  // --- Ana sets a status ----------------------------------------------------
  const statusText = `reviewing ${stamp}`;
  await ana.page.getByTestId("open-settings").click({ timeout: 45_000 });
  await ana.page
    .getByTestId("profile-popover-set-status")
    .click({ timeout: 45_000 });
  await expect(ana.page.getByTestId("set-status-dialog")).toBeVisible({
    timeout: 45_000,
  });
  // The preset FIRST, then the text: `handlePresetClick` overwrites both fields,
  // so filling the input first would lose the stamped text. Taking the emoji
  // from a preset is what puts an `emoji` tag on the event.
  await ana.page
    .getByTestId("set-status-preset-in-a-meeting")
    .click({ timeout: 20_000 });
  await ana.page.getByTestId("set-status-input").fill(statusText);
  await ana.page.getByTestId("set-status-save").click({ timeout: 20_000 });
  await expect(ana.page.getByTestId("set-status-dialog")).toBeHidden({
    timeout: 45_000,
  });

  // 1. THE MECHANISM, on Bo's socket: Ana's 30315, scoped to the `general`
  //    `d` tag the client subscribes with, carrying the text and a glyph.
  await expect
    .poll(
      () =>
        bo.received.some(
          (event) =>
            event.kind === 30315 &&
            event.pubkey === ana.pubkey &&
            event.content === statusText &&
            event.tags.some((tag) => tag[0] === "d" && tag[1] === "general") &&
            event.tags.some((tag) => tag[0] === "emoji" && Boolean(tag[1])),
        ),
      {
        timeout: 120_000,
        message: "no kind 30315 authored by Ana reached Bo's socket",
      },
    )
    .toBe(true);

  // 2. THE UI that frame drove, in Bo's tab: Ana's profile popover carries her
  //    status line. It is a HOVER preview — `UserProfilePopover` opens on
  //    `mouseenter` after a 500ms delay (`handleTriggerMouseEnter`), while a
  //    CLICK instead cancels that timer and opens the full profile panel. So this
  //    HOVERS the author name; clicking would deterministically keep the popover
  //    shut. The cursor stays on the trigger through the reads below, which is
  //    what keeps the preview open.
  //
  //    The author header only renders on a NON-continuation row (`MessageRow`
  //    collapses consecutive messages from one author), which is why Ana posts
  //    exactly one line in this scenario and posts it before anything else she
  //    does. If this ever fails to find the header, that is the reason.
  await anaRow.scrollIntoViewIfNeeded();
  const authorLink = anaRow.getByTestId("message-author").first();
  await expect(
    authorLink,
    "Ana's row rendered no author header — it was grouped as a continuation, so her profile cannot be opened from it",
  ).toBeVisible({ timeout: 45_000 });
  await authorLink.hover({ timeout: 20_000 });
  const popover = bo.page.getByTestId("user-profile-popover");
  await expect(popover).toBeVisible({ timeout: 45_000 });
  await expect(
    popover.getByTestId("user-profile-status"),
    "Ana's status never appeared in Bo's copy of her profile",
  ).toContainText(statusText, { timeout: 90_000 });
  await bo.page.keyboard.press("Escape");

  // --- and Ana clears it ----------------------------------------------------
  // `onClearUserStatus` publishes an empty 30315 (text "" and no emoji tag),
  // which the relay stores as the replacement for the one above.
  await ana.page.getByTestId("open-settings").click({ timeout: 45_000 });
  await ana.page
    .getByTestId("profile-popover-set-status")
    .click({ timeout: 45_000 });
  await expect(ana.page.getByTestId("set-status-dialog")).toBeVisible({
    timeout: 45_000,
  });
  await ana.page.getByTestId("set-status-clear").click({ timeout: 20_000 });
  await expect(ana.page.getByTestId("set-status-dialog")).toBeHidden({
    timeout: 45_000,
  });

  // The clearing event is on the wire too — same author, same `d`, empty
  // content. Asserted before the UI so a failure says which half broke.
  await expect
    .poll(
      () =>
        bo.received.some(
          (event) =>
            event.kind === 30315 &&
            event.pubkey === ana.pubkey &&
            event.content === "" &&
            event.tags.some((tag) => tag[0] === "d" && tag[1] === "general"),
        ),
      {
        timeout: 120_000,
        message: "no cleared kind 30315 reached Bo's socket",
      },
    )
    .toBe(true);

  // And Bo's UI agrees after a full reload. The reload is deliberate: the
  // popover's own query has a 60 s `staleTime`, so re-opening it inside that
  // window can legitimately serve the status it already had — which would make
  // a bare "it went away" assertion a test of react-query's cache rather than
  // of the relay. A remounted tab re-reads kind 30315 from the relay and gets
  // the replacement.
  await bo.page.reload();
  await expect(bo.page.getByTestId("app-sidebar")).toBeVisible({
    timeout: 90_000,
  });
  await openChannel(bo, "General");
  const reloadedRow = messages(bo.page).filter({ hasText: line }).first();
  await expect(reloadedRow).toBeAttached({ timeout: 90_000 });
  await reloadedRow.scrollIntoViewIfNeeded();
  const reloadedAuthor = reloadedRow.getByTestId("message-author").first();
  await expect(reloadedAuthor).toBeVisible({ timeout: 45_000 });
  await reloadedAuthor.hover({ timeout: 20_000 });
  const reopened = bo.page.getByTestId("user-profile-popover");
  await expect(reopened).toBeVisible({ timeout: 45_000 });
  // The status LINE is gone, not merely empty: `hasUserStatus` is false, so
  // `StatusLine` is not rendered at all. Asserting the count rather than the
  // text is what distinguishes "no status" from "a status whose text happens not
  // to contain the stamp".
  await expect(
    reopened.getByTestId("user-profile-status"),
    "Ana's cleared status was still being served to Bo",
  ).toHaveCount(0);
  // The popover itself IS there, which is what stops the assertion above from
  // passing because nothing rendered.
  await expect(reopened).toContainText(ANA, { timeout: 45_000 });

  report(ana, bo);
  await ana.ctx.close();
  await bo.ctx.close();
});
