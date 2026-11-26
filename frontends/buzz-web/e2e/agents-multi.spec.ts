/**
 * TWO agents, ONE message: a multi-agent conversation in a real channel.
 *
 * `company-day.spec.ts` scenarios 9 and 10 each prove ONE agent answers — Nova
 * over the ferrite-cloud harness, Fabrizio over fabric/A2A. Neither shows what
 * happens when a person addresses both at once, which is the ordinary shape of a
 * team asking its agents something. That is what this file covers, and the thing
 * it can actually falsify is whether the relay's dispatch fans out to EVERY
 * registered persona a message p-tags, or only to the first one it finds.
 *
 * WHAT IS DELIBERATELY NOT TESTED HERE, and why. Agent-to-agent triggering — one
 * agent's reply p-tagging another — is plausible from the dispatch rule but the
 * loop-guard behaviour is unverified. Scripting it on a SHARED relay risks two
 * agents answering each other indefinitely, burning real model budget against
 * real provider keys with no bound and no easy way to stop it mid-run. That is
 * not a test, it is a liability, and it should be probed deliberately with a
 * kill switch before anything asserts on it.
 *
 * Both personas are workspace infrastructure, provisioned operator-side and
 * registered in the relay's `buzz-agents` document — a scenario does not mint an
 * agent, it addresses one that exists.
 *
 * WHICH agents it addresses is DISCOVERED, not hardcoded. This spec used to name
 * two pubkeys belonging to one particular dev relay's roster, which meant that
 * anywhere else it ran a full onboarding, spent a real model budget, and then
 * reported "the agent did not answer" about agents that had never been on the
 * deployment at all. Reading the roster keeps the claim identical — dispatch fans
 * out to EVERY p-tagged persona rather than only the first — while making it a
 * claim about the deployment under test.
 *
 *   BUZZ_UI=http://localhost:4173/buzz/ui/index.html \
 *   BUZZ_WS=ws://localhost:4173/buzz/ \
 *   BUZZ_HTTP=http://localhost:4173 \
 *   npx playwright test e2e/agents-multi.spec.ts
 */
import { test, expect } from "@playwright/test";
import {
  addMemberByPubkey,
  createChannel,
  editor,
  expectNoOutsiderPrompt,
  mentionPersona,
  mentionPicker,
  messages,
  onboard,
  openChannel,
  relayPersonas,
  type Person,
  type RelayPersona,
} from "./onboarding";

const stamp = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
const ANA = `Ana Ruiz ${stamp}`;

/** Report what the page complained about, whether or not the scenario passed. */
function report(...people: Person[]) {
  for (const person of people) {
    if (person.problems.length) {
      console.log(`--- ${person.name} ---\n${[...new Set(person.problems)].join("\n")}`);
    }
  }
}

test("one message addressed to two agents is answered by both", async ({
  browser,
}) => {
  // Two real LLM turns on top of onboarding. Measured previously: ~7 s and ~21 s
  // end to end, and both run AFTER the dispatch envelope is picked up, so the
  // budget is generous rather than tight.
  test.setTimeout(600_000);

  // The deployment's own personas, validated and deduplicated by pubkey. Skipped
  // rather than failed when there are fewer than two DISTINCT ones: "this
  // workspace has one agent" is a fact about the deployment, not a defect in
  // dispatch, and burning an onboarding plus a model call to report it would be
  // misleading.
  const personas = await relayPersonas();
  test.skip(
    personas.length < 2,
    `this deployment registers ${personas.length} distinct persona(s); addressing two needs two`,
  );
  const [first, second]: RelayPersona[] = personas;

  // watchKinds tapes inbound kind-9 frames so each answer can be attributed to a
  // PUBKEY off the wire. Attributing by rendered name would not distinguish the
  // two agents from each other, nor an agent's reply from an echo of the ask.
  const ana = await onboard(browser, ANA, { watchKinds: [9] });
  try {
    // A room both agents belong to. The synced client authorizes an agent mention
    // only where the agent is on the room's roster (and the relay only lets a
    // persona post where it is a member), so the room is made and both are added
    // first — the same set-up as the headless `multi-agent` check.
    const room = `two-agents-${stamp}`;
    await createChannel(ana, room);
    await openChannel(ana, room);
    await addMemberByPubkey(ana, first.pubkey);
    await addMemberByPubkey(ana, second.pubkey);

    // One message, both personas p-tagged. Each mention is selected from the
    // autocomplete so the `p` tags are the ones the client really writes — which is
    // the point of driving this through the UI rather than signing the event here.
    // The typed text is a PREFIX of the agent's id; the row is then matched exactly.
    await mentionPersona(ana, first, `@${first.agent.slice(0, 4)}`);
    await mentionPersona(ana, second, ` @${second.agent.slice(0, 4)}`);

    // The answer (1001) is not a substring of the question, so no assertion below
    // can be satisfied by the ask itself — only by a reply.
    const ask = ` both please compute 1000 + 1 for check ${stamp} and reply with just the number`;
    await ana.page.keyboard.type(ask);
    // The picker is dismissed before Enter. NOT because it swallows the key — that was a
    // hypothesis, and measurement disproved it: the picker is closed by the time Enter is
    // pressed. It costs one visibility read and removes an ambiguity, so it stays.
    await expect(mentionPicker(ana)).toBeHidden({ timeout: 20_000 });
    console.log(
      "composer before send:",
      JSON.stringify((await editor(ana.page).innerText().catch(() => "<unreadable>")).slice(0, 200)),
    );
    await ana.page.keyboard.press("Enter");
    // Both agents are members, so the outside-channel confirmation must not appear: if it
    // does, an add did not take, and the scenario fails here quoting the prompt.
    await expectNoOutsiderPrompt(ana);
    await expect(
      messages(ana.page).filter({ hasText: `check ${stamp}` }),
    ).toBeVisible({ timeout: 45_000 });

    // The receipt: a kind 9 signed by EACH persona's own key, carrying the answer.
    // Asserting per-pubkey is what makes this a multi-agent test rather than two
    // chances at a single reply — if dispatch fans out to only the first p-tag,
    // exactly one of these two polls fails and names which agent was skipped.
    for (const persona of [first, second]) {
      await expect
        .poll(
          () =>
            ana.received.some(
              (event) =>
                event.kind === 9 &&
                event.pubkey === persona.pubkey &&
                event.content.includes("1001"),
            ),
          { timeout: 300_000 },
        )
        .toBe(true);
      console.log(`${persona.agent} answered`);
    }
  } finally {
    // A failing scenario owes its report — which is exactly the run that used to
    // print nothing, because the assertion aborted before this line was reached.
    report(ana);
    await ana.ctx.close();
  }
});
