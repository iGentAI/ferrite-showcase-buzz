/**
 * Issue comment events, the write side.
 *
 * A work-item comment is a kind 1 (KIND_TEXT_NOTE) carrying the repo's `a`
 * address tag plus an `e` root tag for the issue — the exact shape
 * `projectIssues.mjs` reads back (`commentsForIssue` keeps kind-1 events whose
 * `e` tag is the issue id) and `projectWorkItems.ts` fetches by `#a`. This
 * module owns the template so the published shape is a testable value;
 * `hooks.ts` (`createProjectIssueComment`) signs and publishes it.
 *
 * Recipients mirror the PR-comment path in `hooks.ts`
 * (`createProjectPullRequestComment`): repo owner, issue author, prior
 * recipients, and any @mentions — lowercased and deduplicated into `p` tags.
 */

/**
 * The full unsigned event of one issue comment. Returned as a template
 * (kind/content/tags) so the tag shape is a testable value and the caller
 * owns signing and publishing.
 */
export function buildProjectIssueCommentEvent({
  content,
  issue,
  mediaTags,
  mentionPubkeys = [],
  project,
}) {
  const body = content.trim();
  if (!body) {
    throw new Error("Comment cannot be empty.");
  }

  const recipients = new Set([
    project.owner.toLowerCase(),
    issue.author.toLowerCase(),
    ...issue.recipients.map((recipient) => recipient.toLowerCase()),
    ...mentionPubkeys.map((pubkey) => pubkey.toLowerCase()),
  ]);

  return {
    // KIND_TEXT_NOTE — the kind projectIssues.mjs reads issue comments back as.
    kind: 1,
    content: body,
    tags: [
      ["e", issue.id, "", "root"],
      ["a", project.repoAddress],
      ...[...recipients].map((recipient) => ["p", recipient]),
      ...(mediaTags ?? []),
    ],
  };
}
