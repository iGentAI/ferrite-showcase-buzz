import type { ProjectIssue } from "./projectIssues.mjs";

/** Lifecycle transitions the client can write for an issue. */
export type ProjectIssueLifecycleStatus = "open" | "done" | "closed" | "triage";

export const PROJECT_ISSUE_LIFECYCLE_KINDS: Record<
  ProjectIssueLifecycleStatus,
  number
>;

export function nextProjectIssueStatusCreatedAt(
  issue: Pick<ProjectIssue, "updatedAt">,
  now: number,
): number;

export function projectIssueStatusClockDelayMs(
  issue: Pick<ProjectIssue, "updatedAt">,
  nowMs: number,
): number;

/** The full unsigned event of one issue lifecycle change. */
export type ProjectIssueStatusEventTemplate = {
  kind: number;
  content: string;
  createdAt: number;
  tags: string[][];
};

export function buildProjectIssueStatusEvent(input: {
  issue: Pick<ProjectIssue, "id" | "author" | "updatedAt">;
  project: { repoAddress: string; owner: string };
  status: ProjectIssueLifecycleStatus;
  now: number;
}): ProjectIssueStatusEventTemplate;
