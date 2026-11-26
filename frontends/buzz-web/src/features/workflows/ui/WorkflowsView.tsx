import { Plus, RefreshCw } from "lucide-react";
import { markWorkflowDeleted } from "./workflowDeletionSignal";
import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";

import {
  allWorkflowsQueryKey,
  workflowListFocusRefetchPolicy,
  workflowQueryKey,
} from "@/features/workflows/hooks";
import { getWorkflowActivationWarning } from "@/features/workflows/ui/workflowActivationWarning";
import { WorkflowCard } from "@/features/workflows/ui/WorkflowCard";
import { WorkflowDeleteDialog } from "@/features/workflows/ui/WorkflowDeleteDialog";
import { WorkflowEditorHost } from "@/features/workflows/ui/WorkflowEditorHost";
import {
  useWorkflowListAuthorPresentations,
  workflowPresentationMapKey,
} from "@/features/workflows/ui/useWorkflowListAuthorPresentations";
import { useWorkflowListMessagePresentations } from "@/features/workflows/ui/useWorkflowListMessagePresentations";
import type { WorkflowEditorRoute } from "@/features/workflows/ui/WorkflowsScreen";
import type { WorkflowEditorPane } from "@/features/workflows/ui/workflowEditorPane";
import {
  readWorkflowDefinitionEnabled,
  workflowDefinitionYaml,
  yamlWithWorkflowEnabled,
} from "@/features/workflows/ui/workflowYamlDocument";
import { useIdentityQuery } from "@/shared/api/hooks";
import type { Channel, Workflow } from "@/shared/api/types";
import {
  deleteWorkflow,
  getChannelsWorkflows,
  triggerWorkflow,
  updateWorkflow,
} from "@/shared/api/tauriWorkflows";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/shared/ui/alert-dialog";
import { Button } from "@/shared/ui/button";
import { PageHeader } from "@/shared/ui/PageHeader";
import { Skeleton } from "@/shared/ui/skeleton";

type WorkflowsViewProps = {
  channels: Channel[];
  editor: WorkflowEditorRoute | null;
  onCloseEditor: () => void;
  onCreateWorkflow: () => void;
  onDuplicateWorkflow: (workflowId: string) => void;
  onEditWorkflow: (workflowId: string) => void;
  onViewWorkflow: (workflowId: string) => void;
  onEditorPaneChange: (pane: WorkflowEditorPane) => void;
};

type WorkflowWithChannel = {
  workflow: Workflow;
  channelName: string;
};

const WORKFLOW_CARD_GRID_CLASS =
  "grid grid-cols-1 gap-3 [@container(min-width:42rem)]:grid-cols-2 [@container(min-width:63rem)]:grid-cols-3";

function WorkflowsListSkeleton() {
  return (
    <div className={WORKFLOW_CARD_GRID_CLASS}>
      {["first", "second", "third", "fourth"].map((card) => (
        <div
          className="flex min-h-60 flex-col rounded-2xl bg-muted/50 p-5 shadow-xs"
          key={card}
        >
          <div className="flex items-start justify-between">
            <div className="flex items-center gap-2">
              <Skeleton className="h-9 w-9 rounded-xl" />
              <Skeleton className="h-4 w-4" />
              <Skeleton className="h-9 w-9 rounded-xl" />
            </div>
            <Skeleton className="h-6 w-16 rounded-full" />
          </div>
          <Skeleton className="mt-5 h-3 w-28" />
          <Skeleton className="mt-2 h-6 w-full" />
          <Skeleton className="mt-2 h-6 w-4/5" />
          <Skeleton className="mt-auto h-4 w-32" />
        </div>
      ))}
    </div>
  );
}

function CreateWorkflowCard({ onClick }: { onClick: () => void }) {
  return (
    <button
      aria-label="Create Workflow"
      className="group relative flex min-h-60 w-full min-w-0 items-center justify-center overflow-hidden rounded-2xl border border-dashed border-border/80 bg-transparent text-muted-foreground shadow-xs transition-colors hover:border-border hover:bg-muted/70 hover:text-foreground focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring"
      data-testid="new-workflow-card"
      onClick={onClick}
      type="button"
    >
      <Plus className="h-7 w-7 transition-colors" />
    </button>
  );
}

export function canManageWorkflow(
  workflow: Pick<Workflow, "ownerPubkey">,
  viewerPubkey: string | null | undefined,
): boolean {
  if (!viewerPubkey || !workflow.ownerPubkey) return false;
  return workflow.ownerPubkey.toLowerCase() === viewerPubkey.toLowerCase();
}

/**
 * Resolve `workflowHint` for `WorkflowEditorHost` without bypassing F165
 * (`getWorkflow` / `resolveWorkflowVersion`).
 *
 * Returns the authenticated viewer's own workflow when unambiguous, or a single
 * non-colliding cell when only one `(ownerPubkey, channelId)` exists for
 * `editorWorkflowId`; returns `undefined` on any cross-author or cross-channel
 * collision so `WorkflowEditorHost` consults `useWorkflowQuery` (`getWorkflow`).
 */
export function resolveEditorWorkflowHint(
  allWorkflows: ReadonlyArray<{ workflow: Workflow }>,
  editorWorkflowId: string | null,
  viewerPubkey: string | null | undefined,
): Workflow | undefined {
  if (!editorWorkflowId || !viewerPubkey) return undefined;
  const matches = allWorkflows
    .map(({ workflow }) => workflow)
    .filter((workflow) => workflow.id === editorWorkflowId);
  if (matches.length === 0) return undefined;

  const normalizedViewer = viewerPubkey.toLowerCase();
  const ownedMatches = matches.filter(
    (workflow) => workflow.ownerPubkey.toLowerCase() === normalizedViewer,
  );
  if (ownedMatches.length === 1) {
    return ownedMatches[0];
  }
  if (ownedMatches.length > 1) {
    return undefined;
  }

  const distinctAuthors = new Set(
    matches.map((workflow) => workflow.ownerPubkey.toLowerCase()),
  );
  const distinctChannels = new Set(
    matches.map((workflow) => workflow.channelId ?? ""),
  );
  if (distinctAuthors.size === 1 && distinctChannels.size === 1) {
    return matches[0];
  }
  return undefined;
}

export function WorkflowsView({
  channels,
  editor,
  onCloseEditor,
  onCreateWorkflow,
  onDuplicateWorkflow,
  onEditWorkflow,
  onViewWorkflow,
  onEditorPaneChange,
}: WorkflowsViewProps) {
  const [deleteTarget, setDeleteTarget] = React.useState<Workflow | null>(null);
  const [activationTarget, setActivationTarget] =
    React.useState<Workflow | null>(null);
  const queryClient = useQueryClient();
  const identityQuery = useIdentityQuery();
  const viewerPubkey = identityQuery.data?.pubkey ?? null;

  const editorWorkflowId =
    editor && editor.mode !== "create" ? editor.workflowId : null;

  const memberChannels = channels.filter((c) => c.isMember);
  const channelIds = memberChannels.map((c) => c.id).sort();
  const channelIdKey = channelIds.join(",");

  const allWorkflowsQuery = useQuery({
    queryKey: allWorkflowsQueryKey(channelIdKey),
    queryFn: async () => {
      // Single batched relay query for all member channels, then group by the
      // channel_id each workflow carries — replaces the per-channel fanout.
      const channelNameById = new Map(
        memberChannels.map((channel) => [channel.id, channel.name]),
      );
      const workflows = await getChannelsWorkflows(channelIds);
      const results: WorkflowWithChannel[] = [];
      for (const workflow of workflows) {
        results.push({
          workflow,
          channelName: workflow.channelId
            ? (channelNameById.get(workflow.channelId) ?? "")
            : "",
        });
      }
      return results;
    },
    enabled: memberChannels.length > 0,
    ...workflowListFocusRefetchPolicy,
  });

  const allWorkflows = allWorkflowsQuery.data ?? [];
  const workflows = allWorkflows.map(({ workflow }) => workflow);
  const authorPresentations = useWorkflowListAuthorPresentations(workflows);
  const messagePresentations = useWorkflowListMessagePresentations(workflows);

  const triggerMutation = useMutation({
    mutationFn: (workflowId: string) => triggerWorkflow(workflowId),
    onSuccess: () => {
      void queryClient.invalidateQueries({
        predicate: (query) => query.queryKey[0] === "workflow-runs",
      });
    },
  });

  const deleteMutation = useMutation({
    mutationFn: (workflow: Workflow) =>
      deleteWorkflow(workflow.id),
    onSuccess: () => {
      void queryClient.invalidateQueries({
        predicate: (query) =>
          query.queryKey[0] === "workflows" ||
          query.queryKey[0] === "workflows-all",
      });
    },
  });

  const toggleEnabledMutation = useMutation({
    mutationFn: (workflow: Workflow) => {
      if (!canManageWorkflow(workflow, viewerPubkey)) {
        throw new Error(
          "Only the workflow owner can change this workflow's status.",
        );
      }
      // Toggle in the definition's own dialect (relay `status`, legacy
      // `enabled`) on the author's text, so a relay workflow is really paused
      // rather than given an `enabled: false` the relay ignores.
      const yaml = yamlWithWorkflowEnabled(
        workflowDefinitionYaml(workflow.definition),
        !readWorkflowDefinitionEnabled(workflow.definition),
      );
      if (yaml === null) {
        throw new Error(
          "This workflow's definition could not be read, so its status was not changed.",
        );
      }
      return updateWorkflow(workflow.id, yaml, workflow.revision, {
        expectedOwnerPubkey: workflow.ownerPubkey,
        expectedChannelId: workflow.channelId,
      });
    },
    onError: (error) => {
      toast.error("Couldn’t change workflow status", {
        description:
          error instanceof Error
            ? error.message
            : "The workflow was not changed. Try again.",
      });
    },
    onSuccess: (_data, workflow) => {
      setActivationTarget(null);
      void queryClient.invalidateQueries({
        queryKey: workflowQueryKey(workflow.id),
      });
      void queryClient.invalidateQueries({
        predicate: (query) =>
          query.queryKey[0] === "workflows" ||
          query.queryKey[0] === "workflows-all",
      });
    },
  });

  const triggerOne = triggerMutation.mutate;
  const handleTrigger = React.useCallback(
    (workflowId: string) => triggerOne(workflowId),
    [triggerOne],
  );

  const handleDelete = React.useCallback(
    (workflow: Workflow) => {
      if (!canManageWorkflow(workflow, viewerPubkey)) return;
      setDeleteTarget(workflow);
    },
    [viewerPubkey],
  );

  const deleteOne = deleteMutation.mutateAsync;
  const handleConfirmDelete = React.useCallback(
    async (workflow: Workflow) => {
      if (!canManageWorkflow(workflow, viewerPubkey)) return;
      try {
        await deleteOne(workflow);
        markWorkflowDeleted(workflow.id);
        setDeleteTarget(null);
        // Deleting the workflow the editor is pointed at would otherwise leave
        // that editor open on a workflow that no longer exists.
        if (workflow.id === editorWorkflowId) onCloseEditor();
      } catch {
        // React Query stores the error; keep the confirmation and editor open.
      }
    },
    [deleteOne, editorWorkflowId, onCloseEditor, viewerPubkey],
  );

  const handleView = React.useCallback(
    (workflow: Workflow) => onViewWorkflow(workflow.id),
    [onViewWorkflow],
  );

  const handleEdit = React.useCallback(
    (workflow: Workflow) => {
      if (!canManageWorkflow(workflow, viewerPubkey)) return;
      onEditWorkflow(workflow.id);
    },
    [onEditWorkflow, viewerPubkey],
  );

  const handleDuplicate = React.useCallback(
    (workflow: Workflow) => onDuplicateWorkflow(workflow.id),
    [onDuplicateWorkflow],
  );

  const toggleEnabled = toggleEnabledMutation.mutate;
  const handleToggleEnabled = React.useCallback(
    (workflow: Workflow) => {
      if (!canManageWorkflow(workflow, viewerPubkey)) return;
      if (
        !readWorkflowDefinitionEnabled(workflow.definition) &&
        getWorkflowActivationWarning(
          workflowDefinitionYaml(workflow.definition),
        )
      ) {
        setActivationTarget(workflow);
        return;
      }
      toggleEnabled(workflow);
    },
    [toggleEnabled, viewerPubkey],
  );
  const activationWarning = activationTarget
    ? getWorkflowActivationWarning(
        workflowDefinitionYaml(activationTarget.definition),
      )
    : null;

  const editorWorkflowHint = resolveEditorWorkflowHint(
    allWorkflows,
    editorWorkflowId,
    viewerPubkey,
  );

  return (
    <div
      className="relative flex min-h-0 flex-1 overflow-hidden"
      data-testid="workflows-view"
    >
      <div
        className="flex min-h-0 flex-1 flex-col overflow-y-auto overflow-x-hidden overscroll-contain px-4 py-7 sm:px-6 sm:py-8"
        data-scroll-restoration-id="workflows-list"
      >
        <div className="mx-auto w-full max-w-6xl space-y-8 [container-type:inline-size]">
          <PageHeader
            action={
              <Button
                aria-label="Refresh workflows"
                disabled={allWorkflowsQuery.isFetching}
                onClick={() => void allWorkflowsQuery.refetch()}
                size="icon"
                variant="ghost"
              >
                <RefreshCw
                  className={`h-4 w-4 ${allWorkflowsQuery.isFetching ? "animate-spin" : ""}`}
                />
              </Button>
            }
            description="Automations that keep your community moving."
            title="Workflows"
          />

          {allWorkflowsQuery.isLoading ? (
            <WorkflowsListSkeleton />
          ) : allWorkflowsQuery.isError ? (
            <div className="flex flex-col items-center justify-center gap-2 py-16 text-muted-foreground">
              <p className="text-sm text-red-400">Failed to load workflows</p>
              <Button
                onClick={() => void allWorkflowsQuery.refetch()}
                size="sm"
                variant="outline"
              >
                Retry
              </Button>
            </div>
          ) : (
            <div className={WORKFLOW_CARD_GRID_CLASS}>
              <CreateWorkflowCard onClick={onCreateWorkflow} />
              {allWorkflows.map(({ workflow, channelName }) => {
                const cardKey = workflowPresentationMapKey(workflow);
                const canManage = canManageWorkflow(workflow, viewerPubkey);
                return (
                  <WorkflowCard
                    authorPresentation={authorPresentations.get(cardKey)}
                    canManage={canManage}
                    channelName={channelName}
                    isTogglingEnabled={
                      toggleEnabledMutation.isPending &&
                      toggleEnabledMutation.variables !== undefined &&
                      workflowPresentationMapKey(
                        toggleEnabledMutation.variables,
                      ) === cardKey
                    }
                    key={cardKey}
                    messagePresentation={messagePresentations.get(cardKey)}
                    onDelete={handleDelete}
                    onDuplicate={handleDuplicate}
                    onEdit={handleEdit}
                    onToggleEnabled={handleToggleEnabled}
                    onTrigger={handleTrigger}
                    onView={handleView}
                    workflow={workflow}
                  />
                );
              })}
            </div>
          )}
        </div>
      </div>

      <WorkflowEditorHost
        channels={memberChannels}
        editor={editor}
        onClose={onCloseEditor}
        onDeleteWorkflow={handleDelete}
        onDuplicateWorkflow={onDuplicateWorkflow}
        onEditWorkflow={onEditWorkflow}
        onEditorPaneChange={onEditorPaneChange}
        onTriggerWorkflow={handleTrigger}
        workflowHint={editorWorkflowHint}
      />

      <AlertDialog
        onOpenChange={(open) => {
          if (!open && !toggleEnabledMutation.isPending) {
            setActivationTarget(null);
          }
        }}
        open={activationTarget !== null}
      >
        <AlertDialogContent data-testid="workflow-activation-confirmation">
          <AlertDialogHeader>
            <AlertDialogTitle>
              {activationWarning?.title ?? "Turn on this workflow?"}
            </AlertDialogTitle>
            <AlertDialogDescription>
              {activationWarning?.description ??
                "Turn it on to let it run immediately, or keep it off until you’re ready."}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Keep off</AlertDialogCancel>
            <AlertDialogAction
              disabled={toggleEnabledMutation.isPending}
              onClick={(event) => {
                if (!activationTarget) return;
                event.preventDefault();
                toggleEnabled(activationTarget);
              }}
            >
              Turn on
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <WorkflowDeleteDialog
        error={
          deleteMutation.error instanceof Error
            ? deleteMutation.error.message
            : null
        }
        isPending={deleteMutation.isPending}
        onConfirm={handleConfirmDelete}
        onOpenChange={(open) => {
          if (!open) {
            deleteMutation.reset();
            setDeleteTarget(null);
          }
        }}
        open={deleteTarget !== null}
        workflow={deleteTarget}
      />
    </div>
  );
}
