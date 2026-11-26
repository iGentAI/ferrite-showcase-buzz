import { useUsersBatchQuery } from "@/features/profile/hooks";
import { resolveUserLabel } from "@/features/profile/lib/identity";
import { useIdentityQuery } from "@/shared/api/hooks";
import type { Workflow } from "@/shared/api/types";
import { getWorkflowTriggerConfig } from "./workflowDefinition";
import {
  type WorkflowAuthorPresentation,
  workflowTriggerAuthorPubkey,
} from "./useWorkflowAuthorPresentation";

export type WorkflowCardAuthorPresentation = Omit<
  WorkflowAuthorPresentation,
  "description"
>;

export function workflowPresentationMapKey(
  workflow: Pick<Workflow, "id"> & {
    channelId?: string | null;
    ownerPubkey?: string;
  },
): string {
  if (!workflow.ownerPubkey) return workflow.id;
  return `${workflow.ownerPubkey.toLowerCase()}:${workflow.channelId ?? ""}:${workflow.id}`;
}

type WorkflowAuthorLookup = {
  mapKey: string;
  pubkey: string;
  workflowId: string;
};

export function workflowAuthorLookups(
  workflows: readonly Workflow[],
): WorkflowAuthorLookup[] {
  return workflows.flatMap((workflow) => {
    const trigger = getWorkflowTriggerConfig(workflow.definition);
    const pubkey = trigger ? workflowTriggerAuthorPubkey(trigger) : null;
    return pubkey
      ? [
          {
            mapKey: workflowPresentationMapKey(workflow),
            pubkey,
            workflowId: workflow.id,
          },
        ]
      : [];
  });
}

export function useWorkflowListAuthorPresentations(
  workflows: readonly Workflow[],
): Map<string, WorkflowCardAuthorPresentation> {
  const identityQuery = useIdentityQuery();
  const lookups = workflowAuthorLookups(workflows);
  const pubkeys = [...new Set(lookups.map(({ pubkey }) => pubkey))];
  const profilesQuery = useUsersBatchQuery(pubkeys);

  return new Map(
    lookups.map(({ mapKey, pubkey }) => {
      const profile = profilesQuery.data?.profiles[pubkey];
      const loading = !profile && profilesQuery.isPending;
      return [
        mapKey,
        {
          avatarUrl: profile?.avatarUrl ?? null,
          isAgent: profile?.isAgent === true,
          label: loading
            ? null
            : resolveUserLabel({
                currentPubkey: identityQuery.data?.pubkey,
                profiles: profilesQuery.data?.profiles,
                pubkey,
              }),
          loading,
          pubkey,
        },
      ];
    }),
  );
}
