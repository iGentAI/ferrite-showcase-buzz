import assert from "node:assert/strict";
import test from "node:test";

import {
  buildProjectIssueStatusEvent,
  nextProjectIssueStatusCreatedAt,
  projectIssueStatusClockDelayMs,
} from "./issueStatusEvents.mjs";

const OWNER = "a".repeat(64);
const AUTHOR = "b".repeat(64);
const ISSUE_ID = "c".repeat(64);
const REPO_ADDRESS = `30617:${OWNER}:my-repo`;

const project = { owner: OWNER, repoAddress: REPO_ADDRESS };
const issue = { id: ISSUE_ID, author: AUTHOR, updatedAt: 1_000 };

test("close — the COMPLETE published event: kind 1632, e root, a repo, p owner, p author", () => {
  // Deep equality over the whole template: the tag shape is the contract.
  // Same shape as the desktop's build_pull_request_status_event and the
  // client's own non-managed PR path, keyed the way latestStatusForIssue and
  // the #a fetch filter read it back.
  assert.deepEqual(
    buildProjectIssueStatusEvent({ issue, project, status: "closed", now: 2_000 }),
    {
      kind: 1632,
      content: "",
      createdAt: 2_000,
      tags: [
        ["e", ISSUE_ID, "", "root"],
        ["a", REPO_ADDRESS],
        ["p", OWNER],
        ["p", AUTHOR],
      ],
    },
  );
});

test("reopen — the COMPLETE published event: kind 1630, same tags", () => {
  // 1630 is deliberately unmapped in projectIssues.statusFromEvent, so a
  // reopened issue falls back to its label-derived status — that fallback IS
  // the reopen semantics, and it only works because this event supersedes the
  // 1632 in the newest-wins sort.
  assert.deepEqual(
    buildProjectIssueStatusEvent({ issue, project, status: "open", now: 2_000 }),
    {
      kind: 1630,
      content: "",
      createdAt: 2_000,
      tags: [
        ["e", ISSUE_ID, "", "root"],
        ["a", REPO_ADDRESS],
        ["p", OWNER],
        ["p", AUTHOR],
      ],
    },
  );
});

test("done and triage — kinds 1631 and 1633, as projectIssues.mjs reads them", () => {
  assert.equal(
    buildProjectIssueStatusEvent({ issue, project, status: "done", now: 2_000 }).kind,
    1631,
  );
  assert.equal(
    buildProjectIssueStatusEvent({ issue, project, status: "triage", now: 2_000 }).kind,
    1633,
  );
});

test("an owner-authored issue carries ONE p tag, not a duplicate", () => {
  // Mirrors the desktop builder's dedup: p author rides only when the author
  // is not already the owner.
  const ownIssue = { ...issue, author: OWNER };
  assert.deepEqual(
    buildProjectIssueStatusEvent({ issue: ownIssue, project, status: "closed", now: 2_000 })
      .tags,
    [
      ["e", ISSUE_ID, "", "root"],
      ["a", REPO_ADDRESS],
      ["p", OWNER],
    ],
  );
  // Case must not defeat the dedup — pubkeys compare lowercased.
  const upper = { ...issue, author: OWNER.toUpperCase() };
  assert.equal(
    buildProjectIssueStatusEvent({ issue: upper, project, status: "closed", now: 2_000 })
      .tags.filter((tag) => tag[0] === "p").length,
    1,
  );
});

test("createdAt advances past the issue's newest activity, so the new status always wins the sort", () => {
  // latestStatusForIssue sorts newest-first; a tie with the status being
  // superseded would make close-then-reopen order-dependent.
  assert.equal(nextProjectIssueStatusCreatedAt({ updatedAt: 5_000 }, 2_000), 5_001);
  assert.equal(nextProjectIssueStatusCreatedAt({ updatedAt: 1_000 }, 2_000), 2_000);
  assert.equal(
    buildProjectIssueStatusEvent({ issue: { ...issue, updatedAt: 9_000 }, project, status: "closed", now: 2_000 })
      .createdAt,
    9_001,
  );
});

test("status publication waits instead of emitting a future relay timestamp", () => {
  assert.equal(
    projectIssueStatusClockDelayMs({ updatedAt: 1_000 }, 1_000_250),
    750,
  );
  assert.equal(
    projectIssueStatusClockDelayMs({ updatedAt: 1_000 }, 1_001_000),
    0,
  );
  assert.equal(projectIssueStatusClockDelayMs({}, 1_001_000), 0);
});

test("an unknown transition and a malformed issue id are refused before anything is signed", () => {
  assert.throws(
    () => buildProjectIssueStatusEvent({ issue, project, status: "merged", now: 2_000 }),
    /Unknown issue lifecycle status/,
  );
  assert.throws(
    () =>
      buildProjectIssueStatusEvent({
        issue: { ...issue, id: "not-an-event-id" },
        project,
        status: "closed",
        now: 2_000,
      }),
    /Task ID must be 64 hex characters/,
  );
});
