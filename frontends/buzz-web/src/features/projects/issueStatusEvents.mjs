/**
 * Issue lifecycle events (NIP-34 statuses 1630–1633), the write side.
 *
 * Everything here is derived from what already exists rather than invented:
 *
 *   * the KINDS are the ones `projectIssues.mjs` reads back — 1631 → Done,
 *     1632 → Closed, 1633 → Triage, and 1630 (Open) deliberately unmapped so
 *     a reopened issue falls back to its label-derived status (Backlog,
 *     In Progress, …), which is exactly what "reopen" means here.
 *   * the TAGS come from `buildGitStatusTags` — upstream ships that builder in
 *     `projectIssues.mjs` with no caller; this module is its caller. The one
 *     addition is a `p` for the issue's author when they are not the repo
 *     owner, mirroring the desktop's PR-status builder
 *     (`build_pull_request_status_event`: `e` root, `a` repo, `p` owner,
 *     `p` author-if-different) and the client's own non-managed PR path
 *     (`pullRequestReviews.ts`).
 *   * the CREATED_AT is monotonic past the issue's newest activity, the same
 *     rule as `nextProjectPullRequestStatusCreatedAt`: two lifecycle writes in
 *     the same whole-second Nostr timestamp would otherwise tie in the
 *     newest-wins sort `latestStatusForIssue` applies.
 *
 * Who may publish one is not decided here: `allowedActorsForRoot` on the read
 * side ignores a status event signed by anyone but the issue's author or the
 * repo's owner, so the UI offers the action to exactly those two.
 */

import { buildGitStatusTags } from "./projectIssues.mjs";

/** Lifecycle transition → NIP-34 status kind, as `projectIssues.mjs` reads them. */
export const PROJECT_ISSUE_LIFECYCLE_KINDS = {
  open: 1630,
  done: 1631,
  closed: 1632,
  triage: 1633,
};

/** Keep consecutive lifecycle writes ordered even when they happen within the
 * same whole-second Nostr timestamp. `updatedAt` is the issue's newest
 * activity (latest of creation, comments, status), so this can only
 * over-advance — never tie with — the status event it must supersede. */
export function nextProjectIssueStatusCreatedAt(issue, now) {
  return Math.max(now, (issue.updatedAt ?? 0) + 1);
}

/**
 * A relay read made immediately after publishing may use its current wall
 * clock as an implicit upper bound. Wait for the required real Nostr second
 * instead of publishing an event whose `created_at` is still in the future.
 */
export function projectIssueStatusClockDelayMs(issue, nowMs) {
  const requiredSecond = (issue.updatedAt ?? 0) + 1;
  return Math.max(0, requiredSecond * 1_000 - nowMs);
}

/**
 * The full unsigned event of one issue lifecycle change. Returned as a
 * template (kind/content/createdAt/tags) so the tag shape is a testable value
 * and the caller owns signing and publishing.
 */
export function buildProjectIssueStatusEvent({ issue, project, status, now }) {
  const kind = PROJECT_ISSUE_LIFECYCLE_KINDS[status];
  if (!kind) {
    throw new Error(`Unknown issue lifecycle status: ${status}`);
  }

  const tags = buildGitStatusTags({
    issueId: issue.id,
    repoAddress: project.repoAddress,
    repoOwner: project.owner,
  });
  const author = (issue.author ?? "").toLowerCase();
  if (
    /^[a-f0-9]{64}$/.test(author) &&
    author !== project.owner.toLowerCase()
  ) {
    tags.push(["p", author]);
  }

  return {
    kind,
    content: "",
    createdAt: nextProjectIssueStatusCreatedAt(issue, now),
    tags,
  };
}
