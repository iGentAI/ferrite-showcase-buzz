/**
 * INVITES: minting a code, and a newcomer redeeming one in their own browser.
 *
 * The relay's invite plane is three routes (`ingress.rs`):
 *
 *   POST /buzz/api/invites               mint. Gated on the operator-seeded
 *                                        community-moderator set, fail-closed —
 *                                        no moderators configured ⇒ nobody
 *                                        mints. Answers
 *                                        `{ok, code, expires_at}`.
 *   POST /buzz/api/invites/claim         claim, signed by the JOINING key and
 *                                        deliberately exempt from every
 *                                        membership gate (the caller is not a
 *                                        member yet). Idempotent: answers
 *                                        `{ok, status: "joined"|"already_member",
 *                                        community, role}`.
 *   POST /buzz/api/invites/accept-policy  unauthenticated exchange of an explicit
 *                                        policy acceptance for a code-bound
 *                                        receipt.
 *
 * TWO THINGS ABOUT THIS DEPLOYMENT SHAPE THE FILE, and both were measured
 * against the live relay on 2026-07-29 rather than assumed:
 *
 *  1. `GET /buzz/api/join-policy` answers `{}` — no policy document is
 *     configured. So `getJoinPolicy()` resolves to null, the redeem form goes
 *     straight to the claim, and `/invites/accept-policy` is unreachable from
 *     any client here: `claim_invite` only demands a receipt when
 *     `join_policy_doc` is Some. Scenario 1 asserts that emptiness up front
 *     rather than carrying a policy-acceptance branch no run has ever executed;
 *     if a policy is ever configured, that assertion fails and tells the next
 *     author to grow the step.
 *  2. The invite UI is UNREACHABLE in the browser build, so the mint below is
 *     driven over the relay's own HTTP face. That is not a shortcut — it is the
 *     finding scenario 3 pins. `CommunityMembersSettingsCard` renders nothing
 *     unless `useMyRelayMembershipLookupQuery` reports owner/admin, and in this
 *     build that lookup can only ever answer `membership: null`:
 *     `relay_requires_membership` returns `false`
 *     (`browser-backend/commands.ts`), which makes `loadRelayMembershipLookup`
 *     short-circuit to a null snapshot before it reads anything, and nothing on
 *     this relay ever publishes the kind-13534 membership list it would have
 *     read. So `community-invite-dialog-trigger`,
 *     `community-invite-link-section` and `copy-invite-link` exist in the tree
 *     and cannot be reached by any user of this build.
 *
 * A THIRD finding is recorded rather than asserted, because a test that pinned
 * it would be a test demanding a bug: `mintInvite()` (`shared/api/invites.ts`)
 * reads `url` off the mint reply and hands it to the clipboard, and the relay's
 * reply has no `url` field at all — so the reachable-in-principle "Copy link"
 * button would put `undefined` on the clipboard. Scenario 1 logs which way that
 * went, the way `channel-lifecycle.spec.ts` logs the private-room notice gap.
 * Related and separate: even a correct `https://<relay>/invite/<code>` landing
 * URL could not address THIS relay, because `parseInviteInput` rebuilds the ws
 * URL from `url.host` alone and drops the path — and every route here lives
 * under `/buzz/`. That is why the invite pasted below uses the
 * `buzz://join?relay=<full ws url>&code=<code>` form, which is the only one of
 * the three accepted forms that can carry a path-namespaced relay.
 *
 * THE STRONGEST BRACKET AVAILABLE IS DELIBERATELY NOT TAKEN, and it should stay
 * that way unless someone is watching the run. `enforce_community_membership`
 * only refuses an unclaimed key while the operator flag
 * `buzz-community-closed` is the literal string `true`, so closing the community
 * for the length of a scenario would turn "the claim worked" into "this key
 * could not write before the claim and can after" — a perfect same-key,
 * same-route pair. It would also lock every OTHER user of this shared relay out
 * of every write for that window, and a crash between the flag and its restore
 * would leave the community closed with no test still running to reopen it.
 * The idempotence receipt below (`joined` then `already_member`) is weaker and
 * safe, and safe wins on shared infrastructure.
 *
 * NOT THE SAME THING AS `live.spec.ts` scenario 9, which is also about invites:
 * that one drives the relay-served hash-routed bundle (`#/invite/<code>`, with
 * `invite-code` and `join-button` testids) and only exercises an INVALID code.
 * This file drives the buzz-web port and a real minted one.
 *
 *   BUZZ_UI=http://localhost:4173/buzz/ui/index.html \
 *   BUZZ_WS=ws://localhost:4173/buzz/ \
 *   npx playwright test e2e/invites.spec.ts --reporter=line --workers=1
 */
import { test, expect } from "@playwright/test";

import { onboard, RELAY_WS, type Person } from "./onboarding";
import {
  identity,
  operatorSecret,
  PROVISION_FILE,
  RELAY_HTTP,
  relayRequest,
  said,
} from "./relayHttp";

const stamp = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
const BO = `Bo Nilsson ${stamp}`;
const CY = `Cy Okafor ${stamp}`;

/**
 * Onboarding two people plus a settings walk, on a relay whose starter-channel
 * read alone is measured at ~3.9 s and whose onboarding card legitimately needs
 * up to four retries.
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

/** One claim the newcomer's own tab made, as the relay answered it. */
type ClaimAnswer = { status: number; body: string };

/**
 * Redeem an invite the way a person already inside the relay-served client does.
 *
 * A client served BY its relay onboards straight into its own community (name,
 * avatar, workspace), so there is no "Join a community" step to paste an invite
 * into. The redeem form a person can reach is the one behind the profile menu:
 * Community actions → Add a community → Join an existing community, which
 * renders upstream's `InviteRedeemForm` (`AddCommunityDialog.tsx`). The tap on
 * `/buzz/api/invites/claim` is installed BEFORE the submit, so every claim the
 * newcomer's own tab makes is recorded with the relay's answer; a successful
 * claim is invisible to `onboard()`'s watcher, which records only 4xx/5xx.
 */
async function redeemInviteFromAddCommunity(
  person: Person,
  inviteUrl: string,
): Promise<ClaimAnswer[]> {
  const { page } = person;
  const claims: ClaimAnswer[] = [];
  page.on("response", (response) => {
    if (!/\/buzz\/api\/invites\/claim$/.test(new URL(response.url()).pathname)) return;
    const status = response.status();
    void response
      .text()
      .then((body) => claims.push({ status, body: body.slice(0, 400) }))
      .catch(() => claims.push({ status, body: "<unreadable>" }));
  });

  await page.getByTestId("open-settings").click({ timeout: 45_000 });
  await page.getByTestId("community-switcher").first().click({ timeout: 45_000 });
  await page
    .getByTestId("profile-community-actions")
    .getByRole("menuitem", { name: /add a community/i })
    .click({ timeout: 45_000 });
  await expect(page.getByTestId("add-community-dialog")).toBeVisible({ timeout: 45_000 });
  await page.getByTestId("add-community-join").click({ timeout: 45_000 });
  // The invite goes in the field a community URL would. `InviteRedeemForm`
  // decides which it is (`parseInviteInput`), and only the invite branch reaches
  // `claimInvite`, so a recorded claim proves this input was taken as an invite.
  await page.getByTestId("invite-redeem-input").first().fill(inviteUrl);
  await page.getByTestId("invite-redeem-submit").click({ timeout: 45_000 });
  return claims;
}

/**
 * A moderator mints an invite; a stranger redeems it and lands in the
 * community.
 *
 * The mint window is kept as SHORT as possible on purpose. The moderator set is
 * community-wide state on a shared relay, so this run's throwaway key holds
 * moderation authority over everybody's community for exactly as long as it
 * takes to mint two codes and prove a non-moderator cannot — then the prior
 * value goes back, before the browser work starts. An invite code is a
 * stateless HMAC (`invites::mint_invite`), so it stays valid after its minter
 * stands down, which is what makes that ordering possible.
 */
test("1. a minted invite is redeemed by a newcomer in their own browser", async ({
  browser,
}) => {
  test.setTimeout(SCENARIO_BUDGET_MS);

  const operator = operatorSecret();
  test.skip(
    operator === null,
    `no operator key in ${PROVISION_FILE}: minting an invite needs the operator-seeded moderator set, and this run cannot reach it`,
  );
  if (!operator) return;

  // The policy plane, measured rather than assumed — see the file header. If
  // this ever stops being empty, the redeem form gains a two-step
  // accept-then-claim flow and this spec has to grow it.
  const policy = await fetch(`${RELAY_HTTP}/buzz/api/join-policy`);
  const policyBody = await policy.text();
  // Truthiness, not key presence: `getJoinPolicy` itself reads `raw.policy ? …
  // : null`, so a `{"policy":null}` body means "no policy" to the client and
  // must mean the same here. Measured today: the body is `{}`.
  expect(
    (JSON.parse(policyBody) as { policy?: unknown }).policy ?? null,
    `this deployment now HAS a join policy (${policyBody.slice(0, 200)}); the claim below needs an accept-policy receipt and this scenario does not mint one`,
  ).toBeNull();

  const minter = identity(`invite-minter-${stamp}`);
  const outsider = identity(`invite-outsider-${stamp}`);
  const claimer = identity(`invite-claimer-${stamp}`);

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

  let browserCode = "";
  let httpCode = "";
  try {
    const seeded = await relayRequest(
      operator,
      "POST",
      "/buzz/api/operator/config/buzz-community-moderators",
      { value: [minter.pubkey] },
    );
    expect(
      seeded.status,
      `seeding the moderator set failed: ${said(seeded)}`,
    ).toBe(200);

    // --- a non-moderator may not mint ---------------------------------------
    // Asserted BEFORE the successful mint so the pair reads as one claim about
    // authority: the same route, the same body, two keys. Quoting the relay's
    // sentence is the point — "not 200" alone would also be satisfied by a
    // missing route, a malformed body or a rate limit, none of which would show
    // that a moderator gate exists at all. 403, not 401: the outsider
    // authenticated and is not permitted (the relay's `http_status` maps
    // `Unauthorized` to 403, pinned by its own unit test).
    const refused = await relayRequest(outsider, "POST", "/buzz/api/invites", {
      ttl_secs: 3600,
    });
    expect(refused.status, `an outsider's mint was not refused: ${said(refused)}`).toBe(
      403,
    );
    expect(refused.body).toContain(
      "only a community moderator may create invites",
    );

    // --- and a moderator may ------------------------------------------------
    const mintOne = await relayRequest(minter, "POST", "/buzz/api/invites", {
      ttl_secs: 3600,
    });
    if (/invite key is not configured/.test(mintOne.body)) {
      // Deployment-level: with no HMAC key there are no invites at all, and
      // seeding one would change this community's security posture for every
      // other user of the shared relay. Skip loudly instead of asserting less.
      test.skip(true, `invites are unavailable on this deployment: ${said(mintOne)}`);
    }
    expect(mintOne.status, `the moderator's mint failed: ${said(mintOne)}`).toBe(200);
    const first = JSON.parse(mintOne.body) as {
      code?: string;
      expires_at?: number;
      url?: string;
    };
    expect(
      first.code,
      `the mint reply carried no code: ${said(mintOne)}`,
    ).toMatch(/^\S{8,}$/);
    expect(first.expires_at ?? 0).toBeGreaterThan(Math.floor(Date.now() / 1000));
    // RECORDED, not asserted: the client reads `url` off this reply and puts it
    // on the clipboard. Pinning its absence would be a test demanding a bug, so
    // this only reports which way it went — the same treatment
    // `channel-lifecycle.spec.ts` gives the private-room notice gap.
    console.log(
      first.url
        ? `the mint reply now carries url=${first.url} — the copy-invite-link path may be whole again, re-read shared/api/invites.ts`
        : "KNOWN GAP (mint reply has no `url`): shared/api/invites.ts reads raw.url and hands it to writeTextToClipboard, so the Copy link button would copy `undefined`",
    );

    const mintTwo = await relayRequest(minter, "POST", "/buzz/api/invites", {
      ttl_secs: 3600,
    });
    expect(mintTwo.status, `the second mint failed: ${said(mintTwo)}`).toBe(200);
    const second = JSON.parse(mintTwo.body) as { code?: string };
    expect(second.code, "the second mint carried no code").toMatch(/^\S{8,}$/);
    // Two mints with identical parameters must not be the same code: the nonce
    // seed folds in the instant and the gateway's per-request id
    // (`ingress.rs::mint_invite`), and a deterministic code would make one
    // leaked invite equal to every invite.
    expect(
      second.code,
      "two identically-parameterised mints returned the same code",
    ).not.toBe(first.code);

    browserCode = first.code ?? "";
    httpCode = second.code ?? "";
  } finally {
    // Hand the community back its own moderator set — `value: null` deletes the
    // key, restoring the documented absent-default. Done before any browser
    // work so the window in which a throwaway key can moderate is seconds long.
    const restore = await relayRequest(
      operator,
      "POST",
      "/buzz/api/operator/config/buzz-community-moderators",
      { value: priorValue ?? null },
    );
    if (restore.status !== 200) {
      console.log(
        `WARNING: could not restore buzz-community-moderators (${said(restore)}) — this run may have left its minter key holding authority`,
      );
    }
  }

  // --- the claim is durable, and a forged code is not ------------------------
  // Three answers on one route from one key, in this order, so each licenses the
  // next: a first claim MATERIALIZES a membership document
  // (`put_if_version(..., None)` — create-if-absent), a second claim of the same
  // code by the same key finds it there (`already_member`, which is the only
  // observable proof the first claim wrote durable state rather than answering
  // and forgetting), and a code with one character of its MAC changed is
  // refused. The refusal cannot be vacuous: the same key, on the same route, was
  // accepted twice moments earlier.
  const joined = await relayRequest(claimer, "POST", "/buzz/api/invites/claim", {
    code: httpCode,
  });
  expect(joined.status, `a valid claim was refused: ${said(joined)}`).toBe(200);
  expect(JSON.parse(joined.body) as { status?: string }).toMatchObject({
    status: "joined",
  });

  const again = await relayRequest(claimer, "POST", "/buzz/api/invites/claim", {
    code: httpCode,
  });
  expect(again.status, `a re-claim was refused: ${said(again)}`).toBe(200);
  expect(
    JSON.parse(again.body) as { status?: string },
    "a re-claim reported `joined` again — the first claim stored no membership",
  ).toMatchObject({ status: "already_member" });

  // Flip the LAST character of the code, which is inside its MAC. Anything the
  // relay accepts here would mean the code is not authenticated.
  const tampered = `${httpCode.slice(0, -1)}${httpCode.slice(-1) === "a" ? "b" : "a"}`;
  const forged = await relayRequest(claimer, "POST", "/buzz/api/invites/claim", {
    code: tampered,
  });
  expect(
    forged.status,
    `a tampered invite code was ACCEPTED (${said(forged)}) — the MAC is not being verified`,
  ).toBeGreaterThanOrEqual(400);

  // --- and a newcomer redeems one in their own browser -----------------------
  // `buzz://join?relay=<full ws url>&code=<code>` is the only accepted invite
  // form that can carry a path-namespaced relay (see the file header), and the
  // mint reply carries a code, not a URL, so the minter's code is wrapped in it.
  const inviteUrl = `buzz://join?relay=${encodeURIComponent(RELAY_WS)}&code=${encodeURIComponent(browserCode)}`;
  const bo = await onboard(browser, BO);
  const claims = await redeemInviteFromAddCommunity(bo, inviteUrl);

  // Bo's OWN tab made the claim, with Bo's own key, and the relay said `joined`.
  // Reaching the workspace alone would not distinguish an invite redeemed from
  // an invite ignored, because this community is open.
  await expect
    .poll(() => claims.length, {
      timeout: 90_000,
      message: `Bo's client never called /buzz/api/invites/claim; page problems: ${bo.problems.join(" | ").slice(0, 400)}`,
    })
    .toBeGreaterThan(0);
  const boClaim = claims[claims.length - 1];
  expect(boClaim.status, `Bo's claim was refused: ${boClaim.body}`).toBe(200);
  const boAnswer = JSON.parse(boClaim.body) as {
    status?: string;
    role?: string;
    community?: string;
  };
  expect(boAnswer.status, `Bo's claim answered ${boClaim.body}`).toBe("joined");
  // The role rides inside the signed code (`payload.r`), so the relay reporting
  // one is evidence it verified and decoded the code rather than waving Bo
  // through.
  expect(boAnswer.role, "the claim named no role").toBeTruthy();
  expect(boAnswer.community, "the claim named no community").toBeTruthy();

  // Bo's side: after redeeming, Bo is in the workspace and sees its room.
  await expect(bo.page.getByTestId("app-sidebar")).toBeVisible({ timeout: 90_000 });
  await expect(bo.page.getByTestId("app-sidebar")).toContainText(/general/i, {
    timeout: 90_000,
  });

  // The relay's side: the community roster lists Bo's key, read by the
  // operator (a member of this community, so the members-only route answers).
  // The roster is paged by an opaque cursor; walk it.
  let listed = false;
  let cursor: string | null = null;
  for (let page = 0; page < 50 && !listed; page += 1) {
    const path = cursor
      ? `/buzz/api/members?cursor=${encodeURIComponent(cursor)}`
      : "/buzz/api/members";
    const roster = await relayRequest(operator, "GET", path);
    expect(roster.status, `the community roster refused a read: ${said(roster)}`).toBe(200);
    const body = JSON.parse(roster.body) as {
      members?: Array<{ pubkey?: string }>;
      next_cursor?: string | null;
    };
    listed = (body.members ?? []).some((member) => member.pubkey === bo.pubkey);
    cursor = body.next_cursor ?? null;
    if (!cursor) break;
  }
  expect(listed, `the relay's community roster does not list Bo (${bo.pubkey})`).toBe(true);

  report(bo);
  await bo.ctx.close();
});

/**
 * The invite UI cannot be reached in this build, and the check is bracketed so
 * that "settings is broken" cannot satisfy it.
 *
 * Order matters: settings is opened, a section that DOES work is asserted to
 * render (`settings-custom-emoji`, reached through its own nav item), and only
 * then is the invite surface asserted absent. Without the positive half this
 * would pass against a settings screen that failed to mount, a mistyped testid,
 * or a build where the whole route was gone.
 *
 * What this pins is a product gap, not a preference: the relay HAS an invite
 * plane (scenario 1 uses it), and the client HAS the screen — the two are
 * simply not connected in a browser, because the capability check reads a
 * kind-13534 snapshot nothing publishes.
 */
test("2. no member can reach the invite dialog in the browser build", async ({
  browser,
}) => {
  test.setTimeout(SCENARIO_BUDGET_MS);
  const cy = await onboard(browser, CY);

  // Settings is a route, opened the way a person opens it: the sidebar profile
  // card's popover. Navigating to the URL directly would depend on the preview
  // server's SPA fallback for a path the bundle is not served at.
  await cy.page.getByTestId("open-settings").click({ timeout: 45_000 });
  await cy.page.getByTestId("profile-popover-settings").click({ timeout: 30_000 });
  await expect(cy.page.getByTestId("settings-view")).toBeVisible({
    timeout: 60_000,
  });

  // --- the positive half: a section that works ------------------------------
  await cy.page
    .getByTestId("settings-nav-custom-emoji")
    .click({ timeout: 45_000 });
  await expect(cy.page.getByTestId("settings-custom-emoji")).toBeVisible({
    timeout: 45_000,
  });

  // --- the invite surface, absent -------------------------------------------
  // `SettingsView.visibleSections` drops `community-members` unless
  // `canManageCommunityMembers` sees an owner/admin role, so the nav item is
  // gone too — not merely the card. Both are asserted, because a present nav
  // item leading to an empty panel would be a different (and worse) bug than a
  // hidden one.
  await expect(cy.page.getByTestId("settings-nav-community-members")).toHaveCount(
    0,
  );
  await expect(cy.page.getByTestId("settings-community-members")).toHaveCount(0);
  await expect(cy.page.getByTestId("community-invite-dialog-trigger")).toHaveCount(
    0,
  );
  await expect(cy.page.getByTestId("community-invite-link-section")).toHaveCount(
    0,
  );
  await expect(cy.page.getByTestId("copy-invite-link")).toHaveCount(0);

  // The relay agrees about the capability, which is what makes the missing UI a
  // wiring gap rather than a UI that is correctly hiding an authority this
  // person does not have: a fresh member's mint IS refused, with the moderator
  // sentence. (The set was restored to its prior value by scenario 1; this
  // asserts nothing about what that value is.)
  const nobody = identity(`invite-nonmoderator-${stamp}`);
  const refused = await relayRequest(nobody, "POST", "/buzz/api/invites", {
    ttl_secs: 3600,
  });
  expect(
    refused.status,
    `a non-moderator minted an invite: ${said(refused)}`,
  ).toBeGreaterThanOrEqual(400);

  report(cy);
  await cy.ctx.close();
});
