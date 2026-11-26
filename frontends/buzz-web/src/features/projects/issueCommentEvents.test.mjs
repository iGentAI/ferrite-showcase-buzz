import assert from "node:assert/strict";
import test from "node:test";

import { buildProjectIssueCommentEvent } from "./issueCommentEvents.mjs";

const OWNER = "a".repeat(64);
const AUTHOR = "b".repeat(64);
const RECIPIENT = "c".repeat(64);
const MENTION = "d".repeat(64);
const ISSUE_ID = "e".repeat(64);

const project = {
  owner: OWNER,
  repoAddress: `30617:${OWNER}:buzz`,
};

function issue(overrides = {}) {
  return {
    id: ISSUE_ID,
    author: AUTHOR,
    recipients: [RECIPIENT],
    ...overrides,
  };
}

test("an issue comment is a kind 1 with e-root, repo a, and p tags", () => {
  const event = buildProjectIssueCommentEvent({
    content: "  Looks broken on main.  ",
    issue: issue(),
    mentionPubkeys: [MENTION],
    project,
  });

  assert.deepEqual(event, {
    kind: 1,
    content: "Looks broken on main.",
    tags: [
      ["e", ISSUE_ID, "", "root"],
      ["a", project.repoAddress],
      ["p", OWNER],
      ["p", AUTHOR],
      ["p", RECIPIENT],
      ["p", MENTION],
    ],
  });
});

test("recipients are lowercased and deduplicated", () => {
  const event = buildProjectIssueCommentEvent({
    content: "Same person three ways.",
    issue: issue({ author: OWNER, recipients: [OWNER.toUpperCase()] }),
    mentionPubkeys: [OWNER],
    project,
  });

  assert.deepEqual(event.tags, [
    ["e", ISSUE_ID, "", "root"],
    ["a", project.repoAddress],
    ["p", OWNER],
  ]);
});

test("media tags are appended verbatim after the p tags", () => {
  const mediaTags = [
    ["imeta", "url https://relay.example/media/x.png", "m image/png"],
  ];
  const event = buildProjectIssueCommentEvent({
    content: "See screenshot.",
    issue: issue({ recipients: [] }),
    mediaTags,
    project,
  });

  assert.deepEqual(event.tags, [
    ["e", ISSUE_ID, "", "root"],
    ["a", project.repoAddress],
    ["p", OWNER],
    ["p", AUTHOR],
    ["imeta", "url https://relay.example/media/x.png", "m image/png"],
  ]);
});

test("a whitespace-only comment is rejected before signing", () => {
  assert.throws(
    () =>
      buildProjectIssueCommentEvent({
        content: "   ",
        issue: issue(),
        project,
      }),
    /Comment cannot be empty\./,
  );
});
