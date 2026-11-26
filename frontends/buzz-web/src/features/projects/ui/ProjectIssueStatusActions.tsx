import { CircleCheck, CircleDot, CircleX, RotateCcw } from "lucide-react";
import * as React from "react";
import { toast } from "sonner";

import type {
  Repository as Project,
  ProjectIssue,
} from "@/features/projects/hooks";
import { useUpdateProjectIssueStatusMutation } from "@/features/projects/issueStatusMutations";
import type { ProjectIssueLifecycleStatus } from "@/features/projects/issueStatusEvents.mjs";
import { useIdentityQuery } from "@/shared/api/hooks";
import { normalizePubkey } from "@/shared/lib/pubkey";
import { Button } from "@/shared/ui/button";

const SUCCESS_MESSAGES: Record<ProjectIssueLifecycleStatus, string> = {
  closed: "Issue closed.",
  done: "Issue marked as done.",
  open: "Issue reopened.",
  triage: "Issue sent to triage.",
};

/**
 * Lifecycle actions for the issue detail's Status rail — the write side of
 * the statuses `projectIssues.mjs` already reads (1630–1633).
 *
 * Offered only to the issue's author and the repo's owner: the read side
 * (`allowedActorsForRoot`) ignores a status event signed by anyone else, so a
 * button for a third party would publish an event every client discards.
 * There is no managed-owner branch (see `issueStatusMutations.ts`), so when a
 * managed agent owns the repo, the issue's author is the one who can close it.
 */
export function ProjectIssueStatusActions({
  issue,
  project,
}: {
  issue: ProjectIssue;
  project: Project;
}) {
  const identityQuery = useIdentityQuery();
  const { isPending, mutateAsync: updateIssueStatus } =
    useUpdateProjectIssueStatusMutation(project);

  const viewerPubkey = identityQuery.data?.pubkey ?? null;
  const viewer = viewerPubkey ? normalizePubkey(viewerPubkey) : null;
  const canChangeStatus =
    Boolean(viewer) &&
    (viewer === normalizePubkey(issue.author) ||
      viewer === normalizePubkey(project.owner));

  const handleStatusChange = React.useCallback(
    async (status: ProjectIssueLifecycleStatus) => {
      try {
        await updateIssueStatus({ issue, status });
        toast.success(SUCCESS_MESSAGES[status]);
      } catch (error) {
        toast.error(
          error instanceof Error
            ? error.message
            : "Failed to update issue status.",
        );
      }
    },
    [issue, updateIssueStatus],
  );

  if (!canChangeStatus) return null;

  const isResolved = issue.status === "Closed" || issue.status === "Done";

  return (
    <div
      className="flex flex-col gap-1.5"
      data-testid="project-issue-status-actions"
    >
      {isResolved ? (
        <Button
          className="h-8 justify-start gap-1.5 px-3"
          data-testid="project-issue-reopen"
          disabled={isPending}
          onClick={() => {
            void handleStatusChange("open");
          }}
          size="xs"
          type="button"
          variant="secondary"
        >
          <RotateCcw className="h-3.5 w-3.5" />
          Reopen issue
        </Button>
      ) : (
        <>
          <Button
            className="h-8 justify-start gap-1.5 px-3"
            data-testid="project-issue-mark-done"
            disabled={isPending}
            onClick={() => {
              void handleStatusChange("done");
            }}
            size="xs"
            type="button"
            variant="secondary"
          >
            <CircleCheck className="h-3.5 w-3.5" />
            Mark as done
          </Button>
          <Button
            className="h-8 justify-start gap-1.5 px-3"
            data-testid="project-issue-close"
            disabled={isPending}
            onClick={() => {
              void handleStatusChange("closed");
            }}
            size="xs"
            type="button"
            variant="secondary"
          >
            <CircleX className="h-3.5 w-3.5" />
            Close issue
          </Button>
          {issue.status !== "Triage" ? (
            <Button
              className="h-8 justify-start gap-1.5 px-3"
              data-testid="project-issue-triage"
              disabled={isPending}
              onClick={() => {
                void handleStatusChange("triage");
              }}
              size="xs"
              type="button"
              variant="ghost"
            >
              <CircleDot className="h-3.5 w-3.5" />
              Send to triage
            </Button>
          ) : null}
        </>
      )}
    </div>
  );
}
