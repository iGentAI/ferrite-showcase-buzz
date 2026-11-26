import * as React from "react";
import { findProjectHomeByChannelId } from "@/features/projects/lib/projectHomeSelection";
import { toast } from "sonner";

import { classifyAgentManagementOrigin } from "@/features/agents/agentManagementBuffer";
import { subscribeProjectChannelRequests } from "@/features/agents/observerRelayStore";
import { useManagedAgentsQuery } from "@/features/agents/hooks";
import { useChannelsQuery } from "@/features/channels/hooks";
import { useChannelTemplatesQuery } from "@/features/channel-templates/hooks";
import { useProjectsQuery } from "@/features/projects/hooks";
import type { ProjectChannelRequest } from "@/features/projects/projectChannelRequest";
import {
  advanceProjectChannelRequestQueue,
  createProjectChannelRequestQueue,
  enqueueProjectChannelRequest,
  type AcceptedProjectChannelRequest,
} from "@/features/projects/projectChannelRequestQueue";
import { useAddProjectChannelMutation } from "@/features/projects/useAddProjectChannel";
import { useIdentityQuery } from "@/shared/api/hooks";
import { normalizePubkey } from "@/shared/lib/pubkey";
import { useUsersBatchQuery } from "@/features/profile/hooks";
import { ownsAuthorAgent } from "@/features/profile/lib/identity";

/**
 * Who may approve an agent's channel request for a project, and how the
 * approval is published. Matches the project's channel management: the
 * viewer owns the project, the owner is one of the viewer's locally managed
 * agents (signed for here), or the owner is the viewer's remote agent
 * (published through that agent).
 */
export function projectChannelApprovalAuthority({
  owner,
  ownerIsManaged,
  viewer,
  viewerOwnsOwnerAgent,
}: {
  owner: string;
  ownerIsManaged: boolean;
  viewer: string | null | undefined;
  viewerOwnsOwnerAgent: boolean;
}): { allowed: boolean; ownerControlAgentPubkey: string | undefined } {
  const viewerIsOwner = viewer != null && normalizePubkey(viewer) === owner;
  return {
    allowed: viewerIsOwner || ownerIsManaged || viewerOwnsOwnerAgent,
    ownerControlAgentPubkey:
      viewerOwnsOwnerAgent && !ownerIsManaged && !viewerIsOwner
        ? owner
        : undefined,
  };
}

export function useProjectChannelRequests() {
  const projectsQuery = useProjectsQuery();
  const channelsQuery = useChannelsQuery();
  const identityQuery = useIdentityQuery();
  const managedAgentsQuery = useManagedAgentsQuery();
  const templatesQuery = useChannelTemplatesQuery();
  const mutation = useAddProjectChannelMutation();
  const [request, setRequest] = React.useState<ProjectChannelRequest | null>(
    null,
  );
  const [sourceAgentPubkey, setSourceAgentPubkey] = React.useState<
    string | null
  >(null);
  const [error, setError] = React.useState<string | null>(null);
  const acceptedRequests = React.useRef(createProjectChannelRequestQueue());
  const bufferedRequests = React.useRef<
    Array<{ agentPubkey: string; request: ProjectChannelRequest }>
  >([]);
  const managedAgentsRef = React.useRef(managedAgentsQuery.data);
  const channelsRef = React.useRef(channelsQuery.data);

  const accept = React.useEffectEvent(
    (agentPubkey: string, next: ProjectChannelRequest) => {
      if (
        classifyAgentManagementOrigin(
          managedAgentsRef.current,
          channelsRef.current,
          agentPubkey,
          next.request.homeChannelId,
        ) !== "accept"
      ) {
        return;
      }
      const result = enqueueProjectChannelRequest(acceptedRequests.current, {
        agentPubkey,
        request: next,
      });
      if (result.status === "show") showRequest(result.candidate);
    },
  );

  React.useEffect(() => {
    managedAgentsRef.current = managedAgentsQuery.data;
    channelsRef.current = channelsQuery.data;
    if (!managedAgentsQuery.data || !channelsQuery.data) return;
    const buffered = bufferedRequests.current.splice(0);
    for (const candidate of buffered) {
      accept(candidate.agentPubkey, candidate.request);
    }
  }, [channelsQuery.data, managedAgentsQuery.data]);

  React.useEffect(
    () =>
      subscribeProjectChannelRequests((agentPubkey, next) => {
        const origin = classifyAgentManagementOrigin(
          managedAgentsRef.current,
          channelsRef.current,
          agentPubkey,
          next.request.homeChannelId,
        );
        if (origin === "buffer") {
          bufferedRequests.current.push({ agentPubkey, request: next });
          if (bufferedRequests.current.length > 100) {
            bufferedRequests.current.shift();
          }
          return;
        }
        accept(agentPubkey, next);
      }),
    [],
  );

  function showRequest(candidate: AcceptedProjectChannelRequest | null) {
    setError(null);
    setSourceAgentPubkey(candidate?.agentPubkey ?? null);
    setRequest(candidate?.request ?? null);
  }

  function advanceRequestQueue() {
    showRequest(advanceProjectChannelRequestQueue(acceptedRequests.current));
  }

  // The home is the project with an authoritative binding to the channel,
  // and only one the viewer or the requesting agent owns can be the target:
  // any member can announce a project naming any channel
  // (finding_ef0a8819e82c41f8, finding_9511d23b4069a024).
  const home =
    request == null
      ? null
      : (findProjectHomeByChannelId(
          request.request.homeChannelId,
          projectsQuery.data ?? [],
        ) ?? null);
  const homeOwner = home ? normalizePubkey(home.owner) : null;
  const ownerProfileQuery = useUsersBatchQuery(homeOwner ? [homeOwner] : [], {
    enabled: homeOwner !== null,
  });
  const authority = homeOwner
    ? projectChannelApprovalAuthority({
        owner: homeOwner,
        ownerIsManaged: (managedAgentsQuery.data ?? []).some(
          (agent) => normalizePubkey(agent.pubkey) === homeOwner,
        ),
        viewer: identityQuery.data?.pubkey,
        viewerOwnsOwnerAgent: ownsAuthorAgent(
          ownerProfileQuery.data?.profiles[homeOwner],
          identityQuery.data?.pubkey,
        ),
      })
    : null;
  const project = home && authority?.allowed ? home : null;
  const template =
    request?.request.templateName == null
      ? null
      : ((templatesQuery.data ?? []).find(
          (candidate) =>
            candidate.name.trim().toLocaleLowerCase() ===
            request.request.templateName?.trim().toLocaleLowerCase(),
        ) ?? null);

  function dismiss() {
    if (mutation.isPending) return;
    advanceRequestQueue();
  }

  async function approve() {
    if (!request || !sourceAgentPubkey || !project) {
      setError(
        "The project home could not be resolved. Refresh and try again.",
      );
      return;
    }
    // The request may have waited in the queue: the agent must still be a
    // managed agent and a member of its home channel now
    // (finding_466b2556fec1f751).
    if (
      classifyAgentManagementOrigin(
        managedAgentsRef.current,
        channelsRef.current,
        sourceAgentPubkey,
        request.request.homeChannelId,
      ) !== "accept"
    ) {
      setError(
        "The requesting agent can no longer act in this project's channel.",
      );
      return;
    }
    // The same authority the project's channel management uses: the viewer
    // owns the project, or one of the viewer's agents does.
    if (!authority?.allowed) {
      setError("Only the project owner can approve this channel.");
      return;
    }
    if (request.request.templateName && !template) {
      setError(
        `Channel template "${request.request.templateName}" is not available on this device.`,
      );
      return;
    }
    setError(null);
    try {
      await mutation.mutateAsync({
        description: request.request.description,
        name: request.request.name,
        // Only a remote owner agent publishes through the observer relay; a
        // locally managed one is signed for here.
        ownerControlAgentPubkey: authority.ownerControlAgentPubkey,
        project,
        templateId: template?.id,
        ttlSeconds: request.request.ttlSeconds,
        visibility: request.request.visibility,
      });
      toast.success(`Channel "#${request.request.name}" created.`);
      advanceRequestQueue();
    } catch (nextError) {
      setError(
        nextError instanceof Error
          ? nextError.message
          : "Failed to create the project channel.",
      );
    }
  }

  return {
    approve,
    dismiss,
    error,
    isPending: mutation.isPending,
    project,
    request,
    template,
  };
}
