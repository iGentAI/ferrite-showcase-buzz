import { useMutation, useQueryClient } from "@tanstack/react-query";

import { relayClient } from "@/shared/api/relayClient";
import { signRelayEvent } from "@/shared/api/tauri";
import type { Repository as Project } from "./hooks";
import {
  buildProjectIssueStatusEvent,
  projectIssueStatusClockDelayMs,
  type ProjectIssueLifecycleStatus,
} from "./issueStatusEvents.mjs";
import type { ProjectIssue } from "./projectIssues.mjs";

/**
 * Publish one issue lifecycle change (close, reopen, done, triage), signed as
 * the current identity and published on the app's own relay connection — the
 * exact path `publishProjectIssue` and the non-managed PR-status branch of
 * `pullRequestReviews.ts` already take. There is deliberately no
 * managed-owner branch here: that signing capability is a desktop keystore
 * (`sign_project_pull_request_status`), and the read side ignores a status
 * signed by anyone but the issue's author or the repo's owner anyway, so the
 * UI offers this to exactly those two.
 */
const MAX_STATUS_CLOCK_WAIT_MS = 5_000;

async function publishProjectIssueStatus(
  project: Project,
  issue: ProjectIssue,
  status: ProjectIssueLifecycleStatus,
) {
  // The wait keeps the status after the issue's newest timestamp, but that
  // timestamp includes anyone's comments, so a future-dated comment must not
  // stall the update: the wait is bounded.
  const delayMs = Math.min(
    projectIssueStatusClockDelayMs(issue, Date.now()),
    MAX_STATUS_CLOCK_WAIT_MS,
  );
  if (delayMs > 0) {
    await new Promise<void>((resolve) => {
      window.setTimeout(resolve, delayMs + 25);
    });
  }
  const template = buildProjectIssueStatusEvent({
    issue,
    project,
    status,
    now: Math.floor(Date.now() / 1_000),
  });
  const event = await signRelayEvent({
    kind: template.kind,
    content: template.content,
    createdAt: template.createdAt,
    tags: template.tags,
  });
  await relayClient.publishEvent(
    event,
    "Timed out updating issue status.",
    "Failed to update issue status.",
  );
  return event.id;
}

export function useUpdateProjectIssueStatusMutation(
  project: Project | null | undefined,
) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({
      issue,
      status,
    }: {
      issue: ProjectIssue;
      status: ProjectIssueLifecycleStatus;
    }) => {
      if (!project) throw new Error("No project selected.");
      return publishProjectIssueStatus(project, issue, status);
    },
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: ["project", project?.id ?? "none", "issues"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["projects", "work-items"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["projects", "activity-summaries"],
        }),
      ]);
    },
  });
}
