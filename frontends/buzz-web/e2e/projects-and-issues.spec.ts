/**
 * A project's task tracker, driven through buzz's own client against a LIVE
 * relay: a task opened, commented on, closed, reopened.
 *
 * WHAT THIS CAN AND CANNOT COVER, stated up front so nobody reads more into a
 * green run than it earns:
 *
 *   - The NOSTR layer is real and is what this file tests. Creating a project
 *     publishes its home channel, a NIP-MP project announcement and a bound
 *     kind 30617 repository announcement (`createProject.ts`). A task ("issue"
 *     in the code) is a kind 1621 addressed to that repository, a comment is a
 *     kind 1 carrying the issue `e` root tag and the repo's `a` address, and a
 *     status change is one of the NIP-34 lifecycle kinds 1630-1633 (1630 Open,
 *     1631 Done, 1632 Closed, 1633 Triage). All of those are published by the
 *     client and stored by the relay.
 *   - The GIT layer is NOT testable here at all. Snapshots, branches, diffs,
 *     commits, clone/push/pull/merge are Tauri `invoke` calls with no browser
 *     implementation (`shared/api/projectGit.ts`), so anything that touches an
 *     actual repository is dead in this build. No scenario below goes near it.
 *
 * THE PATH, since upstream sync 003 reshaped the surface: a repository is no
 * longer created on its own from a Projects "create" menu. A person adds a
 * PROJECT from the sidebar's Projects section (`sidebar-projects-create` →
 * `project-browser-create` → `create-project-dialog`), which lands on the
 * project's channel home (`ProjectChannelHome`). Its overview rail opens the
 * Tasks workspace sheet (`project-home-context-tasks`), whose header "+" is
 * "Create task" (`project-home-workspace-sheet-create`) and opens
 * `CreateProjectIssueDialog` scoped to that one project. A created task opens
 * straight into its detail (`ProjectHomeWorkspaceSheet.handleIssueCreated`).
 *
 * `projects` is also a PREVIEW feature that is off by default and declares
 * `platforms: ["desktop"]` (`preview-features.json`), so the sidebar entry is
 * hidden unless the flag is overridden. Each scenario therefore plants the
 * override in localStorage before the app boots — the same thing the in-app
 * preview-features toggle writes. That is a deliberate, documented act, not a
 * workaround: without it this whole surface is unreachable in a browser and the
 * "tasks opened, commented and closed" behaviour would go untested.
 *
 *   BUZZ_UI=http://localhost:4173/buzz/ui/index.html \
 *   BUZZ_WS=ws://localhost:4173/buzz/ \
 *   npx playwright test e2e/projects-and-issues.spec.ts
 */
import { test, expect, type Browser } from "@playwright/test";
import { onboard, type Person, type ReceivedEvent } from "./onboarding";

const stamp = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
const ANA = `Ana Ruiz ${stamp}`;

/** The task kind and its NIP-34 lifecycle kinds, as the client reads them back. */
const KIND_ISSUE = 1621;
const KIND_STATUS_OPEN = 1630;
const KIND_STATUS_CLOSED = 1632;
const ISSUE_KINDS = [KIND_ISSUE, 1630, 1631, 1632, 1633];

/** Report what the page complained about, whether or not the scenario passed. */
function report(...people: Person[]) {
  for (const person of people) {
    if (person.problems.length) {
      console.log(`--- ${person.name} ---\n${[...new Set(person.problems)].join("\n")}`);
    }
  }
}

/**
 * Onboard with the `projects` preview flag on.
 *
 * The override has to be in localStorage BEFORE the app's first render, because
 * `useFeatureEnabled` reads it through `useSyncExternalStore` at mount and the
 * sidebar entry is behind a `FeatureGate`. `addInitScript` runs on every
 * document in the context, so it is set before any bundle executes. The key is
 * `buzz-feature-overrides-v<manifest version>` (`shared/features/store.ts`).
 */
async function onboardWithProjects(
  browser: Browser,
  name: string,
  options: { watchKinds?: number[] } = {},
): Promise<Person> {
  const person = await onboard(browser, name, options);
  await person.ctx.addInitScript(() => {
    window.localStorage.setItem(
      "buzz-feature-overrides-v1",
      JSON.stringify({ projects: true }),
    );
  });
  // The flag is read at mount, so the already-booted page needs one reload to
  // pick it up. Onboarding is the expensive part and is deliberately not redone.
  await person.page.reload({ waitUntil: "domcontentloaded" });
  await expect(person.page.getByTestId("app-sidebar")).toBeVisible({ timeout: 90_000 });
  // Both entries sit behind `FeatureGate feature="projects"`, so seeing them is
  // the proof the override took.
  await person.page.getByTestId("open-projects-view").click({ timeout: 45_000 });
  await expect(person.page.getByTestId("sidebar-projects-create")).toBeAttached({
    timeout: 45_000,
  });
  return person;
}

/**
 * Create a project from the sidebar, and wait for its channel home.
 *
 * The create dialog REFUSES by staying open with its error in a destructive
 * paragraph (`CreateProjectFormContent.tsx`), so a dialog that does not close is
 * reported with what it said rather than as some later click timing out behind
 * its overlay.
 */
async function createProject(person: Person, name: string): Promise<void> {
  const { page } = person;
  // The section's "+" is only revealed on hover; the hover is what a person does.
  await page.getByTestId("sidebar-projects-section-label").hover();
  await page.getByTestId("sidebar-projects-create").click({ timeout: 45_000 });
  await expect(page.getByTestId("project-browser-dialog")).toBeVisible({
    timeout: 45_000,
  });
  await page.getByTestId("project-browser-create").click();
  const dialog = page.getByTestId("create-project-dialog");
  await expect(dialog).toBeVisible({ timeout: 45_000 });
  await page.getByTestId("create-project-name").fill(name);
  await page.getByTestId("create-project-submit").click();
  try {
    await expect(dialog).toBeHidden({ timeout: 120_000 });
  } catch {
    const said = await dialog
      .locator("p.text-destructive")
      .innerText({ timeout: 2_000 })
      .catch(() => "");
    throw new Error(
      `the create-project dialog stayed open for "${name}"` +
        (said ? `; it said: ${said}` : "; it showed no error text"),
    );
  }
  await expect(
    page.getByTestId("project-channel-home"),
    "creating the project did not land on its channel home",
  ).toBeVisible({ timeout: 60_000 });
}

/**
 * Open a task from the Tasks list and wait for its detail.
 *
 * The row (`ProjectWorkItemRow`) is an `article` whose only open affordances are
 * its title and identifier buttons, so the click goes to the title button. The
 * sheet can re-render under a click while its work-item read settles, so the
 * open is retried a few times against the detail actually appearing.
 */
async function openIssueFromList(person: Person, title: string): Promise<void> {
  const { page } = person;
  const detail = page.getByTestId("project-issue-detail");
  const open = page
    .getByTestId("project-issue-row")
    .filter({ hasText: title })
    .first()
    .getByRole("button", { name: title, exact: true });
  await expect(open).toBeVisible({ timeout: 90_000 });
  for (let attempt = 0; attempt < 3; attempt += 1) {
    await open.click({ timeout: 20_000 }).catch(() => undefined);
    if (
      await detail
        .waitFor({ state: "visible", timeout: 15_000 })
        .then(() => true)
        .catch(() => false)
    ) {
      break;
    }
  }
  await expect(detail).toContainText(title, { timeout: 60_000 });
}

/**
 * Record kind-1 events the relay serves from now on, independent of `onboard`'s
 * tap. That tap deduplicates by event id for the page's whole lifetime, so an
 * event it already saw before a reload is never recorded again; a read made
 * after the reload needs its own listener to be counted at all. Both transports
 * are tapped, as `onboard` does: REQ answers on the socket (`["EVENT", sub,
 * event]`, NIP-01) and the served build's signed `/filter-query` reads.
 */
function tapServedNotes(person: Person): ReceivedEvent[] {
  const served: ReceivedEvent[] = [];
  person.page.on("websocket", (ws) => {
    ws.on("framereceived", (frame) => {
      try {
        const parsed = JSON.parse(String(frame.payload)) as unknown[];
        const event = parsed[0] === "EVENT" ? (parsed[2] as ReceivedEvent | undefined) : undefined;
        if (event?.kind === 1) served.push(event);
      } catch {
        // Not a JSON protocol frame.
      }
    });
  });
  person.page.on("response", (response) => {
    if (!response.url().includes("/buzz/api/filter-query") || !response.ok()) return;
    void response
      .json()
      .then((events: unknown) => {
        if (!Array.isArray(events)) return;
        for (const event of events as ReceivedEvent[]) {
          if (event?.kind === 1) served.push(event);
        }
      })
      .catch(() => undefined);
  });
  return served;
}

/** Open the project home's Tasks workspace sheet. */
async function openTasks(person: Person): Promise<void> {
  const { page } = person;
  await page.getByTestId("project-home-context-tasks").click({ timeout: 60_000 });
  await expect(page.getByTestId("project-home-workspace-sheet")).toHaveAttribute(
    "data-tab",
    "issues",
    { timeout: 45_000 },
  );
}

/**
 * The relay's copy of the task this person just filed, off their own reads.
 *
 * `useCreateProjectIssueMutation` writes nothing into the cache: it publishes and
 * invalidates the work-item queries, whose only source is `relayClient.fetchEvents`
 * (a REQ on the socket, or `/filter-query` in the HTTP-polling build). So a 1621
 * on the tap is the relay answering a read, not the client echoing its own write.
 * The `a` tag is what addresses it to THIS repository rather than any other.
 */
async function relayIssueId(person: Person, repo: string, title: string) {
  let id = "";
  await expect
    .poll(
      () => {
        id =
          person.received.find(
            (event) =>
              event.kind === KIND_ISSUE &&
              event.pubkey === person.pubkey &&
              event.tags.some((tag) => tag[0] === "subject" && tag[1] === title) &&
              event.tags.some(
                (tag) => tag[0] === "a" && tag[1].startsWith("30617:") && tag[1].includes(repo),
              ),
          )?.id ?? "";
        return id;
      },
      {
        timeout: 120_000,
        message: "the relay never served back a kind 1621 with this title addressed to the repo",
      },
    )
    .toMatch(/^[0-9a-f]{64}$/);
  return id;
}

/** Whether the relay has served a status event of `kind` for this issue. */
function servedStatus(person: Person, issueId: string, kind: number) {
  return person.received.some(
    (event) =>
      event.kind === kind &&
      event.pubkey === person.pubkey &&
      event.tags.some((tag) => tag[0] === "e" && tag[1] === issueId && tag[3] === "root"),
  );
}

/**
 * Create a project, file a task on it, and leave the task detail on screen.
 * Returns the relay's id for the task.
 */
async function openFreshIssue(
  person: Person,
  repo: string,
  title: string,
  body: string,
): Promise<string> {
  const { page } = person;
  // A repository announcement (kind 30617) has to exist first — a task is
  // addressed TO a repo via its `a` tag, so there is nothing to file against
  // without one. Creating the project publishes it.
  await createProject(person, repo);
  await openTasks(person);

  // "Create task" is disabled until the project read model carries a repository.
  const create = page.getByTestId("project-home-workspace-sheet-create");
  await expect(create).toBeEnabled({ timeout: 60_000 });
  await create.click();
  const dialog = page.getByTestId("create-issue-dialog");
  await expect(dialog).toBeVisible({ timeout: 45_000 });
  // The repository select is scoped to this project and defaults to its
  // repository; assert it resolved rather than trusting it, since submit is
  // disabled without one and the failure would otherwise look like a broken
  // button.
  await expect(page.getByTestId("create-issue-repository")).toContainText(repo, {
    timeout: 45_000,
  });
  await page.getByTestId("create-issue-title").fill(title);
  await page.getByTestId("create-issue-body").fill(body);
  await page.getByTestId("create-issue-submit").click();
  try {
    await expect(dialog).toBeHidden({ timeout: 60_000 });
  } catch {
    const said = await dialog
      .locator("p.text-destructive")
      .innerText({ timeout: 2_000 })
      .catch(() => "");
    throw new Error(
      `the create-task dialog stayed open for "${title}"` +
        (said ? `; it said: ${said}` : "; it showed no error text"),
    );
  }

  const issueId = await relayIssueId(person, repo, title);

  // The sheet opens the created task once its refetch has it. If that refetch
  // raced the relay, the task is still in the list, and a person opens it there.
  const detail = page.getByTestId("project-issue-detail");
  const row = page.getByTestId("project-issue-row").filter({ hasText: title });
  await expect(detail.or(row).first()).toBeVisible({ timeout: 90_000 });
  if (!(await detail.isVisible().catch(() => false))) {
    await openIssueFromList(person, title);
  }
  await expect(detail).toContainText(title, { timeout: 60_000 });
  return issueId;
}

/**
 * The whole task lifecycle in one scenario, because the states only mean
 * anything in sequence: a task that was never open cannot meaningfully close,
 * and a close that cannot be undone is not a status but a delete.
 */
test("an issue is opened, closed, and reopened", async ({ browser }) => {
  test.setTimeout(300_000);
  const ana = await onboardWithProjects(browser, ANA, { watchKinds: ISSUE_KINDS });

  const repo = `runbooks-${stamp}`;
  const title = `rotate signing keys ${stamp}`;
  const issueId = await openFreshIssue(
    ana,
    repo,
    title,
    `quarterly rotation is overdue (${stamp})`,
  );

  // The status rail is only offered to the issue author and the repo owner
  // (`ProjectIssueStatusActions.tsx`); Ana is both, so its absence here would be
  // a real failure rather than a permissions technicality.
  const detail = ana.page.getByTestId("project-issue-detail");
  const actions = detail.getByTestId("project-issue-status-actions");
  await expect(actions).toBeVisible({ timeout: 60_000 });

  // CLOSE. Two receipts. The relay's: a 1632 rooted at this task's id comes back
  // on a read. And the UI's: the reopen affordance replaces the close one,
  // because the rail derives its buttons from the task's CURRENT status, which
  // it reads back from the relay's 1632 (`useUpdateProjectIssueStatusMutation`
  // only invalidates). Asserting "the button was clicked" would prove nothing.
  await detail.getByTestId("project-issue-close").click();
  await expect
    .poll(() => servedStatus(ana, issueId, KIND_STATUS_CLOSED), {
      timeout: 120_000,
      message: "the relay never served back a kind 1632 rooted at this task",
    })
    .toBe(true);
  await expect(detail.getByTestId("project-issue-reopen")).toBeVisible({
    timeout: 90_000,
  });
  await expect(detail.getByTestId("project-issue-close")).toBeHidden({
    timeout: 30_000,
  });

  // REOPEN, and the affordances swap back — which also proves a status is a
  // sequence of events rather than a one-way latch, and that
  // `nextProjectIssueStatusCreatedAt` really does keep the newer event newer
  // (two statuses in the same second would otherwise tie and the older could
  // win the replaceable-event race).
  await detail.getByTestId("project-issue-reopen").click();
  await expect
    .poll(() => servedStatus(ana, issueId, KIND_STATUS_OPEN), {
      timeout: 120_000,
      message: "the relay never served back a kind 1630 rooted at this task",
    })
    .toBe(true);
  await expect(detail.getByTestId("project-issue-close")).toBeVisible({
    timeout: 90_000,
  });
  await expect(detail.getByTestId("project-issue-reopen")).toBeHidden({
    timeout: 30_000,
  });

  report(ana);
  await ana.ctx.close();
});

/**
 * A comment on a task.
 *
 * The composer is a `ForumComposer` wrapped in a `project-issue-comment-composer`
 * div — an additive testid this port carries (see the allowlist entry for
 * `ProjectIssuesPanel.tsx`); upstream ships the composer with no testid at all,
 * so without it this affordance is not drivable. Enter submits (plain Enter is
 * bound to submit in `useRichTextEditor`; Shift+Enter is the hard break).
 *
 * WHAT MAKES THE RENDERED COMMENT A RELAY RECEIPT, since "text appeared" usually
 * is not one: `useCreateProjectIssueCommentMutation` writes NOTHING into the
 * cache. It publishes and then invalidates the work-item queries, whose only
 * source is `relayClient.fetchEvents`. The scenario then goes further and
 * RELOADS the page, dropping the in-memory query cache, reopens the task, and
 * requires both the relay read and the rendered comment again.
 */
test("an issue takes a comment, and the relay serves it back", async ({
  browser,
}) => {
  test.setTimeout(300_000);
  // kind 1 IS the issue-comment kind here (`buildProjectIssueCommentEvent`), and
  // the events the panel reads back arrive as answers to its relay reads. It is
  // not a live push and nothing below claims one.
  const ana = await onboardWithProjects(browser, ANA, { watchKinds: [1, KIND_ISSUE] });

  const repo = `handbooks-${stamp}`;
  const title = `document the rotation ${stamp}`;
  const issueId = await openFreshIssue(
    ana,
    repo,
    title,
    `opened by the comment scenario (${title})`,
  );

  const composer = ana.page.getByTestId("project-issue-comment-composer");
  await expect(
    composer,
    "the task detail rendered no comment composer",
  ).toBeVisible({ timeout: 60_000 });

  const comment = `comment ${stamp}: the runbook lives in ops/rotation.md`;
  const field = composer.locator('[contenteditable="true"]').first();
  await field.click();
  await ana.page.keyboard.type(comment);
  await ana.page.keyboard.press("Enter");

  // THE WIRE RECEIPT. A comment is a kind 1 addressed at TWO things, and both
  // matter: the `e` tag with the `root` marker is what files it under this task
  // (`["e", issue.id, "", "root"]`), and the `a` tag is the NIP-34 repo address
  // that scopes it to this repository. A kind 1 without them is an ordinary
  // note that no client would ever show here — which is exactly the failure a
  // "the text is visible" assertion would have missed.
  const isThisComment = (event: ReceivedEvent) =>
    event.kind === 1 &&
    event.pubkey === ana.pubkey &&
    event.content.includes(comment) &&
    event.tags.some((tag) => tag[0] === "e" && tag[1] === issueId && tag[3] === "root") &&
    event.tags.some((tag) => tag[0] === "a" && tag[1].includes(repo));
  await expect
    .poll(() => ana.received.some(isThisComment), {
      timeout: 120_000,
      message:
        "no kind 1 authored by Ana carried this comment with the task's `e` root tag and the repo `a` address",
    })
    .toBe(true);

  // And the panel serves it back inside the task detail. The composer is
  // cleared on submit, and the detail's Activity section (the comment timeline)
  // is where a posted comment renders, so scoping to the detail is not satisfied
  // by draft text in some other composer on screen.
  const detail = ana.page.getByTestId("project-issue-detail");
  await expect(
    detail.getByTestId("project-issue-comment-composer"),
    "the composer still holds the draft, so the comment was never submitted",
  ).not.toContainText(comment, { timeout: 60_000 });
  await expect(
    detail,
    "the comment never came back into the task detail",
  ).toContainText(comment, { timeout: 120_000 });

  // AFTER A RELOAD. The query cache lives in memory only, so a reload leaves
  // the relay as the only place this comment can come from. A fresh tap is
  // installed first so the read that is counted is one made AFTER the reload.
  const servedAfterReload = tapServedNotes(ana);
  await ana.page.reload({ waitUntil: "domcontentloaded" });
  await expect(ana.page.getByTestId("app-sidebar")).toBeVisible({ timeout: 90_000 });
  await expect(
    ana.page.getByTestId("project-channel-home"),
    "the reload did not come back to the project home",
  ).toBeVisible({ timeout: 90_000 });
  await openTasks(ana);
  await openIssueFromList(ana, title);
  await expect
    .poll(() => servedAfterReload.some(isThisComment), {
      timeout: 120_000,
      message: "after the reload the relay never served the comment back",
    })
    .toBe(true);
  await expect(
    detail,
    "after the reload the comment did not render in the task detail",
  ).toContainText(comment, { timeout: 120_000 });

  report(ana);
  await ana.ctx.close();
});
