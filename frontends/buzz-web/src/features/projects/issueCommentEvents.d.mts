import type { ProjectIssue } from "./projectIssues.mjs";

/** The full unsigned event of one issue comment. */
export type ProjectIssueCommentEventTemplate = {
  kind: number;
  content: string;
  tags: string[][];
};

export function buildProjectIssueCommentEvent(input: {
  content: string;
  issue: Pick<ProjectIssue, "id" | "author" | "recipients">;
  mediaTags?: string[][];
  mentionPubkeys?: string[];
  project: { repoAddress: string; owner: string };
}): ProjectIssueCommentEventTemplate;
