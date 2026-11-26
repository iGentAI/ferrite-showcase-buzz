/**
 * A company's day in buzz — driven through buzz's OWN client against a LIVE
 * relay.
 *
 * Nothing here is mocked: the page is buzz's real desktop UI served as a web
 * bundle, it speaks NIP-01 over a real WebSocket to the deployed relay, and
 * every assertion reads what the UI actually renders after the relay answered.
 * Each person is their own browser context with their own identity key, so
 * "Bo sees what Ana posted" is two clients and one relay, not one page talking
 * to itself.
 *
 *   BUZZ_UI=http://localhost:4173/buzz/ui/index.html \
 *   BUZZ_WS=ws://localhost:4173/buzz/ \
 *   npx playwright test e2e/company-day.spec.ts
 */
import { test, expect } from "@playwright/test";
import {
  addMemberByPubkey,
  createChannel,
  editor,
  expectNoOutsiderPrompt,
  mentionPersona,
  messages,
  onboard,
  openChannel,
  relayEventId,
  relayPersonas,
  say,
  sayInThread,
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
 * shared live relay already holds dozens of identical "Bo Nilsson" kind 0s
 * from earlier runs — a latent flake even serially, and ambiguous across
 * parallel workers. Unique names (plus the pubkey-keyed selectors below)
 * remove it, so no scenario needs to stay serial.
 */
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

test("1. a new hire lands in the workspace with the starter channels", async ({
  browser,
}) => {
  const ana = await onboard(browser, ANA);
  const sidebar = ana.page.getByTestId("app-sidebar");
  // Both starter channels are named in lowercase (`canonical_channel_name`
  // lowercases every channel on ingest), so the sidebar renders `general`, not
  // `General` — and `toContainText` is case-sensitive. `openChannel("General")`
  // in later scenarios still works because that helper matches case-insensitively.
  await expect(sidebar).toContainText("general");
  await expect(sidebar).toContainText("welcome-everyone");
  // The name they typed during onboarding is a real kind 0 on the relay, read
  // back by `get_profile` — not a local echo. The stamped name proves it is
  // THIS run's profile, not a same-named residue from an earlier one.
  await expect(sidebar).toContainText(ANA);
  report(ana);
  await ana.ctx.close();
});

test("2. the morning standup: two colleagues post and see each other live", async ({
  browser,
}) => {
  const ana = await onboard(browser, ANA);
  const bo = await onboard(browser, BO);
  await openChannel(ana, "General");
  await openChannel(bo, "General");

  const anaLine = `standup ${stamp}: shipping the billing migration today`;
  await say(ana, anaLine);
  await expect(messages(ana.page).filter({ hasText: anaLine })).toBeVisible({
    timeout: 45_000,
  });
  // Bo never reloads: it has to arrive over the live socket. Assert it is
  // ATTACHED, not visible — the timeline now renders history, so a new row
  // lands below the fold behind a "new messages" pill until Bo scrolls.
  await expect(messages(bo.page).filter({ hasText: anaLine })).toBeAttached({
    timeout: 60_000,
  });

  const boLine = `standup ${stamp}: reviewing the migration, no blockers`;
  await say(bo, boLine);
  await expect(messages(ana.page).filter({ hasText: boLine })).toBeAttached({
    timeout: 60_000,
  });
  report(ana, bo);
  await ana.ctx.close();
  await bo.ctx.close();
});

/**
 * A REPLY, not merely a second message.
 *
 * This scenario used to assert only that the reply text was "visible", and it
 * passed for weeks while threaded replies were entirely broken in the browser
 * build (`send_channel_message` had no browser implementation at all, so every
 * reply threw). Two mistakes let that through: it typed into `editor()`, whose
 * `.first()` resolves to the CHANNEL composer even with a thread open, and
 * "the text is visible" is equally true of an ordinary channel message. So it
 * proved the one thing that was never in doubt.
 *
 * What it asserts now is the thread RELATIONSHIP, on the wire and in the UI:
 * the reply is a kind 9 whose `e` tag names the root's real relay id, and the
 * thread panel renders it as a reply rather than the timeline rendering it as a
 * peer.
 */
test("3. a design review turns into a thread", async ({ browser }) => {
  test.setTimeout(300_000);
  // watchKinds records the relay's own echo, which is where the root's durable
  // id comes from — see `relayEventId`.
  const ana = await onboard(browser, ANA, { watchKinds: [9] });
  await openChannel(ana, "General");
  const topic = `design review ${stamp}: the new empty state`;
  await say(ana, topic);
  const row = messages(ana.page).filter({ hasText: topic }).first();
  await expect(row).toBeVisible({ timeout: 45_000 });

  // Wait for the relay to echo the root before opening its thread. Until then
  // the row carries a client-minted `optimistic-<uuid>` id, and opening a thread
  // on that id makes the client fetch an event the relay has never seen.
  const rootId = await relayEventId(ana, topic);

  await row.scrollIntoViewIfNeeded();
  await row.hover();
  await row.locator('[data-testid^="reply-message-"]').first().click({ timeout: 20_000 });
  await expect(ana.page.getByTestId("thread-composer-overlay")).toBeVisible({
    timeout: 45_000,
  });

  const replyText = `thread ${stamp}: keep the illustration, cut the copy`;
  await sayInThread(ana, replyText);

  // The receipt that matters: the relay accepted a kind 9 that POINTS AT the
  // root. An ordinary channel message carries no `e` tag, so this assertion is
  // the one the old scenario was missing, and it is what a broken reply path
  // cannot fake.
  await expect
    .poll(
      () =>
        ana.received.some(
          (event) =>
            event.kind === 9 &&
            event.content.includes(replyText) &&
            event.tags.some((tag) => tag[0] === "e" && tag[1] === rootId),
        ),
      { timeout: 90_000 },
    )
    .toBe(true);

  // And the UI nests it: the reply renders inside the thread panel's reply list,
  // not merely somewhere on the page.
  await expect(ana.page.getByTestId("message-thread-replies")).toContainText(
    replyText,
    { timeout: 60_000 },
  );
  report(ana);
  await ana.ctx.close();
});

test("4. a teammate reacts instead of replying", async ({ browser }) => {
  const ana = await onboard(browser, ANA);
  const bo = await onboard(browser, BO);
  await openChannel(ana, "General");
  await openChannel(bo, "General");

  const line = `ship note ${stamp}: staging is green`;
  await say(ana, line);
  const row = messages(bo.page).filter({ hasText: line }).first();
  await expect(row).toBeAttached({ timeout: 60_000 });

  // The action bar belongs to the hovered ROW, so scope the click to it — the
  // timeline now renders history, so a page-wide match hits the first row.
  await row.scrollIntoViewIfNeeded();
  await row.hover();
  await row.getByRole("button", { name: "React with :+1:" }).click({ timeout: 20_000 });

  // The reaction is a kind 7 on the relay, so Ana sees it without reloading.
  await expect(
    messages(ana.page)
      .filter({ hasText: line })
      .getByTestId("message-reactions"),
  ).toBeVisible({ timeout: 60_000 });
  report(ana, bo);
  await ana.ctx.close();
  await bo.ctx.close();
});

test("5. an @mention reaches the person it names", async ({ browser }) => {
  const ana = await onboard(browser, ANA);
  const bo = await onboard(browser, BO);
  await openChannel(ana, "General");
  await openChannel(bo, "General");

  // Bo has to have said something before Ana's client can resolve the name.
  await say(bo, `checking in ${stamp}`);
  await expect(messages(ana.page).filter({ hasText: `checking in ${stamp}` })).toBeAttached({
    timeout: 60_000,
  });

  await editor(ana.page).click();
  await ana.page.keyboard.type("@Bo");
  // Select by PUBKEY, not by name: the suggestion testid carries the pubkey
  // (`MentionAutocomplete.tsx`), and a name filter with `.first()` picked a
  // stale same-named identity off the shared relay — the mention's p tag then
  // named a dead key and this run's Bo never got it.
  const suggestion = ana.page
    .getByTestId("mention-autocomplete")
    .getByTestId(`mention-suggestion-${bo.pubkey}`);
  // 45s, not 20: the suggestion needs the colleague's kind 0 to propagate
  // through the live relay's search/profile reads, which under real traffic
  // can exceed a 20s budget (observed flake, 2026-07-28 final run).
  await expect(suggestion).toBeVisible({ timeout: 45_000 });
  await suggestion.click();
  await ana.page.keyboard.type(` can you take the ${stamp} rollback runbook?`);
  await ana.page.keyboard.press("Enter");

  await expect(
    messages(bo.page).filter({ hasText: `${stamp} rollback runbook` }),
  ).toBeAttached({ timeout: 60_000 });
  // A mention is a `p` tag, which is what puts it in Bo's Inbox. Click the
  // BUTTON role: the label renders twice inside it (an invisible bold twin
  // reserves the active-state width), so a text locator is ambiguous exactly
  // when the item is highlighted.
  await bo.page
    .getByTestId("app-sidebar")
    .getByRole("button", { name: "Inbox" })
    .first()
    .click();
  await expect(
    bo.page.getByText(`${stamp} rollback runbook`).first(),
  ).toBeVisible({ timeout: 60_000 });
  report(ana, bo);
  await ana.ctx.close();
  await bo.ctx.close();
});

test("6. a project channel is created and a colleague joins it", async ({
  browser,
}) => {
  const ana = await onboard(browser, ANA);
  const bo = await onboard(browser, BO);
  const room = `billing-${stamp}`;

  await createChannel(ana, room);
  await expect(ana.page.getByTestId("app-sidebar")).toContainText(room, {
    timeout: 45_000,
  });

  const line = `kickoff ${stamp}: scope is invoices only`;
  await openChannel(ana, room);
  await say(ana, line);
  await expect(messages(ana.page).filter({ hasText: line })).toBeVisible({
    timeout: 45_000,
  });

  // Bo finds the room in the browser and joins it — the relay published its
  // 39000 the moment it was created, so it is listed for everyone. The row
  // click only PREVIEWS the room (`ChannelBrowserDialog.handleSelect` closes
  // the dialog and navigates read-only); membership is the separate Join
  // affordance, so without the Join click below `is_member` stays false and
  // the member-filtered sidebar never lists the room for Bo.
  await bo.page.getByTestId("section-actions-channels-quick-create").click();
  const entry = bo.page.getByRole("button", { name: new RegExp(room) }).first();
  await expect(entry).toBeVisible({ timeout: 45_000 });
  await bo.page
    .getByRole("button", { name: new RegExp(`${room}[\\s\\S]*`) })
    .first()
    .click();
  await bo.page.keyboard.press("Escape");
  await bo.page.getByRole("button", { name: /^Join$/ }).first().click();
  await openChannel(bo, room);
  await expect(messages(bo.page).filter({ hasText: line })).toBeAttached({
    timeout: 60_000,
  });
  report(ana, bo);
  await ana.ctx.close();
  await bo.ctx.close();
});

test("7. an incident gets a private channel a non-member cannot see", async ({
  browser,
}) => {
  const ana = await onboard(browser, ANA);
  const cy = await onboard(browser, CY);
  const incident = `incident-${stamp}`;

  await createChannel(ana, incident, { private: true });
  await openChannel(ana, incident);

  const secret = `sev1 ${stamp}: checkout latency, war room here`;
  await say(ana, secret);
  await expect(messages(ana.page).filter({ hasText: secret })).toBeVisible({
    timeout: 45_000,
  });

  // Cy was never added. Neither the room nor its traffic may reach them.
  await cy.page.reload();
  await expect(cy.page.getByTestId("app-sidebar")).toBeVisible({ timeout: 60_000 });
  await cy.page.waitForTimeout(6_000);
  await expect(cy.page.getByTestId("app-sidebar")).not.toContainText(incident);
  await expect(messages(cy.page).filter({ hasText: secret })).toHaveCount(0);
  report(ana, cy);
  await ana.ctx.close();
  await cy.ctx.close();
});

test("8. two colleagues take it to a DM", async ({ browser }) => {
  const ana = await onboard(browser, ANA);
  const bo = await onboard(browser, BO);
  await openChannel(ana, "General");
  await openChannel(bo, "General");
  await say(bo, `dm setup ${stamp}`);
  await expect(messages(ana.page).filter({ hasText: `dm setup ${stamp}` })).toBeAttached({
    timeout: 60_000,
  });

  // Pick the recipient by PUBKEY, not by name: the picker's search haystack
  // includes the pubkey (`commands.ts` search_users) and each result row's
  // testid carries it (`NewMessageResultRow.tsx`). A name search with
  // `.first()` picked a stale same-named identity off the shared relay, the
  // DM room hashed to that dead key's pair, and the real Bo was correctly
  // denied the room by the relay's fail-closed access gate.
  await ana.page.getByTestId("section-actions-dms-quick-create").click();
  await ana.page.locator("#new-dm-search").fill(bo.pubkey);
  await ana.page
    .getByTestId(`new-dm-result-${bo.pubkey}`)
    .click({ timeout: 30_000 });

  const dmLine = `quick one ${stamp}: are you ok to own the rollback?`;
  await say(ana, dmLine);
  await expect(messages(ana.page).filter({ hasText: dmLine })).toBeVisible({
    timeout: 45_000,
  });
  // Drive Bo's side the way a person receives a DM: the conversation arrives
  // via the 39000/39002 refetch and shows in the DM list with an unread badge,
  // and a message-row only renders for the OPEN conversation — asserting the
  // row before opening it always timed out, whatever the relay did.
  const boDmList = bo.page.getByTestId("dm-list");
  await expect(boDmList).toBeVisible({ timeout: 60_000 });
  await boDmList.locator("li").first().click();
  await expect(messages(bo.page).filter({ hasText: dmLine })).toBeAttached({
    timeout: 60_000,
  });
  report(ana, bo);
  await ana.ctx.close();
  await bo.ctx.close();
});

/**
 * The workspace agent, registered relay-side as the `nova` persona
 * (`buzz-agents` registry → agents-cloud sibling → LLM gateway), is asked a
 * question in #general and answers INTO the channel. Nothing in this test
 * talks to the agent directly: the whole path is mention → relay dispatch →
 * persona turn → `buzz_post` back through the relay's ingest saga.
 */
test("9. the team asks the workspace agent and gets a real answer", async ({
  browser,
}) => {
  // A real LLM turn (gateway round trip + tool posts) runs 30-120 s on top of
  // onboarding; the default 90 s budget is for human-to-human scenarios.
  test.setTimeout(300_000);

  // WHICH agent is DISCOVERED, not named. This scenario used to hardcode a pubkey belonging to
  // one particular dev relay's roster, so anywhere else it ran a full onboarding, spent a real
  // model budget, and then reported that an agent which had never been on the deployment did not
  // answer. An agent is still workspace infrastructure — provisioned operator-side, not minted
  // here — this only stops assuming WHICH one.
  const personas = await relayPersonas();
  test.skip(
    personas.length === 0,
    "this deployment registers no personas, so there is no workspace agent to ask",
  );
  const nova = personas[0];

  const ana = await onboard(browser, ANA, { watchKinds: [9] });
  // The ask happens in a room the agent belongs to. The synced client authorizes
  // an agent mention only where the agent is on the room's roster, and the relay
  // only lets a persona post where it is a member — so Ana makes a room and adds
  // the agent first, exactly as a person setting up a team room would.
  const room = `ask-agent-${stamp}`;
  await createChannel(ana, room);
  await openChannel(ana, room);
  await addMemberByPubkey(ana, nova.pubkey);

  // Picked by whichever testid form the candidate carries — a cloud teammate resolves to a
  // pubkey, a purely local persona does not — and the picker is left closed.
  await mentionPersona(ana, nova, `@${nova.agent.slice(0, 4)}`);
  // The answer (5555) is not a substring of the question, so the assertion
  // below can only be satisfied by a REPLY — never by this message itself.
  const ask = ` compute 1234 + 4321 for check ${stamp} and reply with just the number`;
  await ana.page.keyboard.type(ask);
  await ana.page.keyboard.press("Enter");
  // Mentioning a non-member raises a confirmation, and until it is answered the message
  // correctly never leaves the composer. The agent is a member here, so the prompt must not
  // appear; if it does, the add did not take.
  await expectNoOutsiderPrompt(ana);
  await expect(
    messages(ana.page).filter({ hasText: `check ${stamp}` }),
  ).toBeVisible({ timeout: 45_000 });

  // The agent's answer arrives as an ordinary channel message signed by the
  // persona's relay-held key — same saga, same live subscription, no bypass.
  // WHERE it lands is the model's choice on this harness: a ferrite-cloud turn
  // posts with `buzz_post`, whose `reply_to` is optional, so the same persona
  // has answered threaded (headless `persona`, 2026-09-12 02:21Z) and top-level
  // (this scenario, 02:20Z). Only the Fabric arm threads by construction
  // (`post_a2a_reply`). So the receipt is the persona's KEY on the answer, off
  // Ana's socket, and then the UI rendering it wherever it was posted.
  let answerId = "";
  await expect
    .poll(
      () => {
        answerId =
          ana.received.find(
            (event) =>
              event.kind === 9 &&
              event.pubkey === nova.pubkey &&
              event.content.includes("5555"),
          )?.id ?? "";
        return answerId;
      },
      { timeout: 240_000, message: `no kind 9 authored by ${nova.agent}'s key carried 5555` },
    )
    .toMatch(/^[0-9a-f]{64}$/);
  // And the UI renders THAT event, wherever the model posted it: as a timeline
  // row carrying the answer's own relay id, or inside the thread the ask
  // opened. Either way the rendered row is the persona's event, not any row
  // that happens to contain the digits.
  const answerRow = ana.page.locator(`[data-testid="message-row"][data-message-id="${answerId}"]`);
  const askRow = messages(ana.page).filter({ hasText: `check ${stamp}` }).first();
  const summary = askRow.locator(
    "xpath=following::button[@data-testid='message-thread-summary'][1]",
  );
  await expect(answerRow.first().or(summary)).toBeVisible({ timeout: 60_000 });
  if (!(await answerRow.first().isVisible())) {
    await summary.click();
  }
  await expect(answerRow.first()).toBeVisible({ timeout: 30_000 });
  await expect(answerRow.first()).toContainText("5555");
  report(ana);
  await ana.ctx.close();
});

/**
 * The OTHER agent backend.
 *
 * Scenario 9 covers the `ferrite-cloud` harness, whose turn runs inside
 * agents-cloud itself. `fabrizio` is registered against the `fabric` harness,
 * which is a different path end to end: the dispatch leaves agents-cloud for a
 * remote A2A peer, runs on the peer's wall clock, and comes back through the
 * relay's command seam. Nothing in the suite exercised it, so the whole A2A
 * arm — including its turn lease and the lease's heartbeat — had no scenario.
 *
 * The ask is arithmetic for the same reason scenario 9's is: `3087` does not
 * occur anywhere in the question, so the assertion cannot be satisfied by the
 * ask itself. The socket tap is what makes it FABRIC's answer rather than any
 * agent's — it pins the author to fabrizio's key.
 */
test("10. the Fabric-backed agent answers in the same channel", async ({
  browser,
}) => {
  // A Fabric turn owns a 300 s peer budget before the post budget on top, so
  // this is the longest scenario in the suite by construction.
  test.setTimeout(480_000);

  // Found by HARNESS, not by a hardcoded key. This scenario is about the fabric/A2A path
  // specifically, so a deployment that wires no such backend has nothing here to exercise — a
  // fact about the deployment rather than a defect, and worth skipping on rather than failing on.
  const fabricPersonas = (await relayPersonas()).filter((persona) =>
    /fabric|a2a/i.test(persona.harness),
  );
  test.skip(
    fabricPersonas.length === 0,
    "this deployment registers no persona on the fabric/A2A harness, so there is no remote-peer arm to exercise",
  );
  const fabrizio = fabricPersonas[0];
  const FABRIZIO = fabrizio.pubkey;
  const ana = await onboard(browser, ANA, { watchKinds: [9] });
  // Same room rule as scenario 9: the agent is added to the room before it is asked.
  const room = `fabric-ask-${stamp}`;
  await createChannel(ana, room);
  await openChannel(ana, room);
  await addMemberByPubkey(ana, fabrizio.pubkey);

  // The picker only offers a relay-directory agent the reader may address
  // (`agentAutocompleteEligibility`), and the row is matched by whichever testid form it carries.
  await mentionPersona(ana, fabrizio, `@${fabrizio.agent.slice(0, 4)}`);
  await ana.page.keyboard.type(
    ` compute 4321 - 1234 for fabric check ${stamp} and reply with just the number`,
  );
  await ana.page.keyboard.press("Enter");
  await expectNoOutsiderPrompt(ana);
  await expect(
    messages(ana.page).filter({ hasText: `fabric check ${stamp}` }),
  ).toBeVisible({ timeout: 45_000 });

  // Same shape as scenario 9: the peer replies IN-THREAD, so the timeline shows
  // the ask's thread-summary badge and the answer text renders once opened.
  const askRow = messages(ana.page)
    .filter({ hasText: `fabric check ${stamp}` })
    .first();
  const summary = askRow.locator(
    "xpath=following::button[@data-testid='message-thread-summary'][1]",
  );
  await expect(summary).toBeVisible({ timeout: 420_000 });
  await summary.click();
  await expect(ana.page.getByText("3087").first()).toBeVisible({
    timeout: 30_000,
  });
  // The UI showed an answer; this pins WHOSE. Only fabrizio's key proves the
  // fabric harness answered rather than the workspace's other persona.
  expect(
    ana.received.some(
      (event) => event.pubkey === FABRIZIO && event.content.includes("3087"),
    ),
    "no kind 9 authored by fabrizio's key carried the answer",
  ).toBe(true);
  report(ana);
  await ana.ctx.close();
});

/**
 * A DM reaches the recipient in SECONDS, and by the live path.
 *
 * This is the one product-latency bug the suite had on record: the recipient's
 * side of a DM took ~2 minutes (measured 126 s on 2026-07-28). The cause was
 * never the projections — it was discovery. A client's live subscriptions are
 * per KNOWN channel (`{"#h":[id]}`), so a room the recipient has never heard of
 * matches nothing, and the only thing that could find it was the channel-list
 * poll (`staleTime`/`refetchInterval` 60 s, and not at all while backgrounded).
 *
 * The fix is a relay-minted membership notice: kinds 44100/44101, `#p`-gated to
 * the subject, which the shipped client has always subscribed to
 * (`useMembershipNotifications`) while nothing ever minted one.
 *
 * Both halves are asserted, and the order matters. "Bo's DM list eventually
 * showed it" proves nothing about the mechanism — a slow poll produces the same
 * final UI. So the notice is asserted off Bo's OWN socket first, then the UI it
 * drove, then the wall clock as a guard against the old two-minute behaviour.
 *
 * (Scenario 8 also opens a DM, but only ever asserted that the conversation
 * arrives — never how, nor how fast, and it waits a full 60 s to do it.)
 */
test("11. a DM reaches the recipient in seconds, over the live notice", async ({
  browser,
}) => {
  test.setTimeout(300_000);
  const ana = await onboard(browser, ANA);
  const bo = await onboard(browser, BO, { watchKinds: [44100] });
  await openChannel(ana, "General");
  await openChannel(bo, "General");

  // Bo has to be resolvable in the picker before Ana can name them.
  await say(bo, `dm ping ${stamp}`);
  await expect(
    messages(ana.page).filter({ hasText: `dm ping ${stamp}` }),
  ).toBeAttached({ timeout: 60_000 });
  // Bo has no conversations yet, so the DM section itself is the receipt.
  await expect(bo.page.getByTestId("dm-list")).toBeHidden();

  // Pick by PUBKEY, not name: the shared live relay holds same-named
  // identities from earlier runs, and the wrong one would hash the DM room to
  // a dead key's pair — which the relay then correctly denies to the real Bo.
  await ana.page.getByTestId("section-actions-dms-quick-create").click();
  await ana.page.locator("#new-dm-search").fill(bo.pubkey);
  const openedAt = Date.now();
  await ana.page
    .getByTestId(`new-dm-result-${bo.pubkey}`)
    .click({ timeout: 30_000 });

  const dmLine = `heads up ${stamp}: the rollback is yours`;
  await say(ana, dmLine);
  await expect(messages(ana.page).filter({ hasText: dmLine })).toBeVisible({
    timeout: 45_000,
  });

  // 1. The mechanism: the relay minted a 44100 naming Bo — and only Bo, since
  //    the filter the client subscribes with is `#p`-gated to the reader — and
  //    delivered it on Bo's already-open socket.
  await expect
    .poll(
      () =>
        bo.received.filter(
          (event) =>
            event.kind === 44100 &&
            event.tags.some((tag) => tag[0] === "p" && tag[1] === bo.pubkey),
        ).length,
      {
        timeout: 60_000,
        message: "no kind 44100 membership notice reached Bo's socket",
      },
    )
    .toBeGreaterThan(0);

  // 2. The UI that notice drove: Bo's DM section appears without a reload.
  await expect(bo.page.getByTestId("dm-list")).toBeVisible({ timeout: 45_000 });
  await bo.page.getByTestId("dm-list").locator("li").first().click();
  await expect(messages(bo.page).filter({ hasText: dmLine })).toBeAttached({
    timeout: 60_000,
  });

  // 3. The guard on the bug itself. The old path could only discover the room
  //    on a 60 s poll boundary (two, with a backgrounded tab), which is the
  //    ~126 s that was measured. Anything inside one poll interval cannot be
  //    explained by the poll alone.
  const elapsed = Date.now() - openedAt;
  console.log(`DM materialized for the recipient in ${(elapsed / 1000).toFixed(1)}s`);
  expect(
    elapsed,
    "the recipient's side of the DM took a poll interval or worse",
  ).toBeLessThan(60_000);
  report(ana, bo);
  await ana.ctx.close();
  await bo.ctx.close();
});

/**
 * A room is renamed and re-described, and a colleague sees the new identity.
 *
 * Every existing scenario creates rooms and never changes one. A rename is a
 * NIP-29 metadata edit (kind 9002) that the relay re-projects into the room's
 * 39000, so the label a SECOND client renders is the relay's answer rather than
 * a local edit echo — which is why the assertion is made on Bo's sidebar, not
 * Ana's.
 *
 * Archiving deliberately is NOT asserted here. The relay enforces it
 * (`Channel::is_writable`, gated by a unit test), but the web client cannot
 * reach it: `archive_channel` is implemented only in the e2e MOCK backend
 * (`src/testing/e2eBridge.ts`), so in this build the menu item throws
 * `DesktopUnavailableError` and the room stays writable. Encoding that as an
 * expectation would be a test asserting a broken affordance; it is reported as
 * a source finding instead. (An earlier revision of this scenario did assert
 * it, and that is how the gap was found: the "blocked" line came back after a
 * reload, proving the relay had accepted it because nothing ever archived the
 * room.)
 */
test("12. a room is renamed and a colleague sees its new identity", async ({
  browser,
}) => {
  // Two onboardings, a join and an edit. The colleague's sidebar learns the new
  // name from the live room projection (`useLiveRoomProjections`), so no poll
  // interval is budgeted.
  test.setTimeout(300_000);
  const ana = await onboard(browser, ANA);
  const bo = await onboard(browser, BO);
  const room = `ops-${stamp}`;
  const renamed = `ops-handbook-${stamp}`;
  const topic = `runbooks and rotations ${stamp}`;

  await createChannel(ana, room);
  await openChannel(ana, room);
  const line = `runbook ${stamp}: rotate the signing keys quarterly`;
  await say(ana, line);
  await expect(messages(ana.page).filter({ hasText: line })).toBeVisible({
    timeout: 45_000,
  });

  // Bo joins the room under its ORIGINAL name, so the rename has to reach an
  // already-joined member rather than being discovered with the room.
  await bo.page.getByTestId("section-actions-channels-quick-create").click();
  const entry = bo.page.getByRole("button", { name: new RegExp(room) }).first();
  await expect(entry).toBeVisible({ timeout: 45_000 });
  await entry.click();
  await bo.page.keyboard.press("Escape");
  await bo.page.getByRole("button", { name: /^Join$/ }).first().click();
  await openChannel(bo, room);
  await expect(messages(bo.page).filter({ hasText: line })).toBeAttached({
    timeout: 60_000,
  });

  // Rename + re-describe: management panel → Edit → fields → Save. Both are
  // privileged tags (`effects::PRIVILEGED_EDIT_TAGS`), which Ana holds as owner.
  await ana.page.getByTestId("channel-management-trigger").first().click();
  await ana.page.getByTestId("channel-management-edit").click();
  await ana.page.getByTestId("channel-management-name").fill(renamed);
  // Two elements carry this testid in the synced client: the sheet's read-only
  // description line and the edit form's textarea. The textarea is the field.
  await ana.page
    .locator("textarea[data-testid='channel-management-description']")
    .fill(topic);
  await ana.page.getByTestId("channel-management-save-changes").click();

  // Ana's own sidebar relabels, and the room keeps its history across the edit.
  await expect(ana.page.getByTestId("app-sidebar")).toContainText(renamed, {
    timeout: 60_000,
  });
  await expect(messages(ana.page).filter({ hasText: line })).toBeAttached({
    timeout: 45_000,
  });

  // The receipt that matters: BO's sidebar carries the new name. That label can
  // only have come from the relay's re-published 39000, which Bo's client now
  // receives live and applies to its channel list (`useLiveRoomProjections`).
  // Before that it waited for the 60 s channel-list poll (measured 2026-09-12
  // 03:17Z: +60 s), and the budget allowed three intervals. 30 s is well under
  // one interval, so this cannot pass on the poll alone.
  await expect(bo.page.getByTestId("app-sidebar")).toContainText(renamed, {
    timeout: 30_000,
  });
  await expect(bo.page.getByTestId("app-sidebar")).not.toContainText(room);
  report(ana, bo);
  await ana.ctx.close();
  await bo.ctx.close();
});
