import assert from "node:assert/strict";
import test from "node:test";

import { unavailableCommandMessage } from "./commands.ts";

/**
 * The sentences a refused command shows the user.
 *
 * buzz renders a rejected action's message directly (`toast.error(error.message)`
 * in MergePullRequestButton and PullRequestReviewCard, `relayError`, the huddle
 * banner), so this string IS the UI. A wrong or generic one reads as "the app is
 * broken" for something that is working exactly as designed.
 *
 * Command names are taken from their invoke sites in `shared/api/projectGit.ts`
 * (mergeProjectPullRequest, signProjectPullRequestReviewRequest,
 * signProjectPullRequestStatus, publishProjectPullRequestMergedStatus).
 */

const GENERIC = /no native backend for/;

test("signing as a managed-agent owner explains whose key is missing", () => {
  for (const cmd of [
    "sign_project_pull_request_status",
    "sign_project_pull_request_review_request",
  ]) {
    const message = unavailableCommandMessage(cmd);
    assert.doesNotMatch(message, GENERIC, `${cmd} still falls through`);
    // The reason, not just the verdict: the key belongs to the agent the
    // desktop supervises, and a tab holds one key of its own.
    assert.match(message, /managed agent/);
    assert.match(message, /agent's key/);
    assert.match(message, /desktop/);
    // And what DOES work, so the user is not left thinking PRs are read-only:
    // author/owner-signed statuses go out over the ordinary sign_event path.
    assert.match(message, /repositories you own yourself/);
  }
});

test("merging names git as the reason and a terminal as the way out", () => {
  for (const cmd of [
    "merge_project_pull_request",
    "publish_project_pull_request_merged_status",
  ]) {
    const message = unavailableCommandMessage(cmd);
    assert.doesNotMatch(message, GENERIC, `${cmd} still falls through`);
    assert.match(message, /git/);
    assert.match(message, /desktop app/);
    // This one is reachable today (canMerge is `isOwner || isManagedAgentOwner`,
    // and isOwner is reachable), so it has to say what still works.
    assert.match(message, /review, approve, comment, close/);
  }
});

test("the pull-request patterns claim nothing beyond those four commands", () => {
  // Anchored on purpose. `pull_request` bare, or `merge` bare, would swallow
  // neighbours, as a bare `window` once swallowed `get_channel_window`.
  for (const cmd of [
    "sign_event",
    "create_auth_event",
    "get_channel_window",
    "publish_event",
    "merge",
    "get_project_branches",
  ]) {
    const message = unavailableCommandMessage(cmd);
    assert.doesNotMatch(
      message,
      /managed agent|Merging happens/,
      `${cmd} was captured by a pull-request family`,
    );
  }
});

test("a command's own reason beats a family pattern that also matches it", () => {
  // `pocket_voice` is also in the huddle family, whose sentence would send the
  // reader looking for a huddle.
  assert.match(unavailableCommandMessage("set_pocket_voice"), /Pocket voices are neural TTS/);
  assert.match(unavailableCommandMessage("other_pocket_voice"), /Voice huddles need/);
});

test("earlier families keep priority over the pull-request ones", () => {
  assert.match(
    unavailableCommandMessage("create_managed_agent"),
    /harness runs as a local process/,
  );
  assert.match(
    unavailableCommandMessage("git_branch_list"),
    /Repository tools read your working copy/,
  );
});

test("an unknown command still gets the generic fallback", () => {
  // The fallback is the honest answer for a command nobody has classified. It
  // must survive: silently mapping the unknown onto a wrong family is worse.
  assert.match(unavailableCommandMessage("some_unclassified_command"), GENERIC);
});
