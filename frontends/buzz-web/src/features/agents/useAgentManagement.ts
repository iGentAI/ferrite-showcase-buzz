import * as React from "react";
import { toast } from "sonner";
import { createChannelManagedAgent } from "./channelAgents";
import { isRelayManagedHarness } from "./lib/relayManagedHarness";
import { useQueryClient } from "@tanstack/react-query";

import {
  createInputFromRequest,
  requestTargetsEditablePersona,
  type AgentManagementRequest,
  updateInputFromRequest,
} from "./agentManagement";
import { subscribeAgentManagementRequests } from "./observerRelayStore";
import {
  managedAgentsQueryKey,
  personasQueryKey,
  useAcpRuntimesQuery,
  useCreateManagedAgentMutation,
  useCreatePersonaMutation,
  useManagedAgentsQuery,
  usePersonasQuery,
  useUpdatePersonaMutation,
} from "./hooks";
import {
  availableRuntimesForStart,
  buildInstanceInputForDefinition,
  type BackendIntent,
} from "./lib/instanceInputForDefinition";
import { useCreatedAgentChannelAttachment } from "./useCreatedAgentChannelAttachment";
import { classifyAgentManagementOrigin } from "./agentManagementBuffer";
import {
  AgentManagementRequestQueue,
  describeAgentRequester,
} from "./agentManagementRequestQueue";
import { useChannelsQuery } from "@/features/channels/hooks";
import { resolveManagedAgentAvatarUrl } from "./ui/managedAgentAvatar";
import type { AgentCreateIntent } from "./ui/agentCreateIntent";
import { editPersonaDialogState } from "./ui/personaDialogState";
import type {
  CreatePersonaInput,
  UpdatePersonaInput,
} from "@/shared/api/types";

export function useAgentManagement() {
  const queryClient = useQueryClient();
  const personasQuery = usePersonasQuery();
  const managedAgentsQuery = useManagedAgentsQuery();
  const channelsQuery = useChannelsQuery();
  const runtimesQuery = useAcpRuntimesQuery({ enabled: true });
  const createPersonaMutation = useCreatePersonaMutation();
  const updatePersonaMutation = useUpdatePersonaMutation();
  const createAgentMutation = useCreateManagedAgentMutation();
  const [request, setRequest] = React.useState<AgentManagementRequest | null>(
    null,
  );
  const [error, setError] = React.useState<string | null>(null);
  const createdAgentAttachment = useCreatedAgentChannelAttachment();
  // Decides which agent request is on screen: ids are recorded only when shown,
  // later requests wait in a bounded queue, and an agent whose dialog the user
  // dismissed is ignored for a cooldown (agentManagementRequestQueue.ts).
  const requestQueue = React.useRef(
    new AgentManagementRequestQueue<AgentManagementRequest>(),
  );
  const [requestAgentPubkey, setRequestAgentPubkey] = React.useState<
    string | null
  >(null);
  const sourceAgentPubkey = React.useRef<string | null>(null);
  const managedAgentsRef = React.useRef(managedAgentsQuery.data);
  const channelsRef = React.useRef(channelsQuery.data);
  const bufferedRequestsRef = React.useRef<
    Array<{ agentPubkey: string; request: AgentManagementRequest }>
  >([]);

  function showRequest(agentPubkey: string, next: AgentManagementRequest) {
    setError(null);
    sourceAgentPubkey.current = agentPubkey;
    setRequestAgentPubkey(agentPubkey);
    setRequest(next);
  }

  const acceptOwnedRequest = React.useEffectEvent(
    (agentPubkey: string, next: AgentManagementRequest) => {
      if (
        classifyAgentManagementOrigin(
          managedAgentsRef.current,
          channelsRef.current,
          agentPubkey,
          next.request.channelId,
        ) !== "accept"
      ) {
        return;
      }
      if (requestQueue.current.offer(agentPubkey, next) === "shown") {
        showRequest(agentPubkey, next);
      }
    },
  );

  React.useEffect(() => {
    managedAgentsRef.current = managedAgentsQuery.data;
    channelsRef.current = channelsQuery.data;
    if (managedAgentsQuery.data && channelsQuery.data) {
      const buffered = bufferedRequestsRef.current.splice(0);
      for (const candidate of buffered) {
        acceptOwnedRequest(candidate.agentPubkey, candidate.request);
      }
    }
  }, [channelsQuery.data, managedAgentsQuery.data]);

  React.useEffect(
    () =>
      subscribeAgentManagementRequests((agentPubkey, next) => {
        // Observer frames are owner-scoped and authenticated. Any managed agent
        // this Desktop owns may draft a change; defer the ownership decision
        // until the managed-agent query has initialized so ephemeral requests
        // cannot disappear during startup.
        if (
          classifyAgentManagementOrigin(
            managedAgentsRef.current,
            channelsRef.current,
            agentPubkey,
            next.request.channelId,
          ) === "buffer"
        ) {
          bufferedRequestsRef.current.push({ agentPubkey, request: next });
          if (bufferedRequestsRef.current.length > 100) {
            bufferedRequestsRef.current.shift();
          }
          return;
        }
        acceptOwnedRequest(agentPubkey, next);
      }),
    [],
  );

  const matchingPersonas = React.useMemo(() => {
    if (request?.action !== "update") return [];
    const target = request.request.agentName.trim().toLocaleLowerCase();
    return (personasQuery.data ?? []).filter(
      (persona) =>
        persona.displayName.trim().toLocaleLowerCase() === target &&
        requestTargetsEditablePersona(persona),
    );
  }, [personasQuery.data, request]);
  const currentPersona =
    matchingPersonas.length === 1 ? matchingPersonas[0] : undefined;

  const isPending =
    createPersonaMutation.isPending ||
    updatePersonaMutation.isPending ||
    createAgentMutation.isPending;

  function assertAgentCanActFromOrigin(channelId: string) {
    const targetChannel = (channelsQuery.data ?? []).find(
      (channel) => channel.id === channelId,
    );
    const requestingPubkey = sourceAgentPubkey.current?.toLowerCase();
    if (
      !targetChannel?.isMember ||
      !requestingPubkey ||
      !targetChannel.memberPubkeys.some(
        (pubkey) => pubkey.toLowerCase() === requestingPubkey,
      )
    ) {
      throw new Error(
        "An agent can only manage agents from a channel you both belong to.",
      );
    }
  }

  async function submitCreate(
    input: CreatePersonaInput | UpdatePersonaInput,
    intent: AgentCreateIntent,
    backendIntent: BackendIntent | null,
  ): Promise<boolean> {
    if (request?.action !== "create" || "id" in input) {
      return false;
    }
    setError(null);
    try {
      assertAgentCanActFromOrigin(request.request.channelId);
      const runtimes = await availableRuntimesForStart(runtimesQuery);
      const runtime = runtimes.find(
        (candidate) => candidate.id === input.runtime,
      );
      if (!runtime) {
        throw new Error("Choose an available runtime for this agent.");
      }

      const avatarUrl = await resolveManagedAgentAvatarUrl(
        input.avatarUrl,
        undefined,
        runtime.avatarUrl,
      );
      const persona = await createPersonaMutation.mutateAsync({
        ...input,
        avatarUrl,
      });

      if (intent === "definition_start") {
        // The persona is saved from here on. A failure starting or attaching
        // its instance must not leave this dialog open, where Create would
        // write a second persona; it closes and says what failed instead.
        try {
          if (isRelayManagedHarness(runtime.id)) {
            // Relay-managed harnesses run on the relay once the definition
            // exists; there is no local instance, only channel membership.
            await createChannelManagedAgent(request.request.channelId, {
              name: persona.displayName,
              personaId: persona.id,
              runtime,
              role: "bot",
            });
          } else {
            const created = await createAgentMutation.mutateAsync(
              await buildInstanceInputForDefinition(
                persona,
                runtime,
                undefined,
                backendIntent ?? undefined,
              ),
            );
            if (created.spawnError) throw new Error(created.spawnError);
            const targetChannel = (channelsQuery.data ?? []).find(
              (channel) => channel.id === request.request.channelId,
            );
            await createdAgentAttachment.presentCreatedAgent(created, {
              id: request.request.channelId,
              name: targetChannel?.name ?? "this channel",
            });
          }
        } catch (instanceError) {
          await Promise.all([
            queryClient.invalidateQueries({ queryKey: personasQueryKey }),
            queryClient.invalidateQueries({ queryKey: managedAgentsQueryKey }),
          ]);
          closeRequest("saved");
          toast.warning(
            `${persona.displayName} was saved, but could not be started: ${
              instanceError instanceof Error
                ? instanceError.message
                : String(instanceError)
            }`,
          );
          return true;
        }
      }

      await Promise.all([
        queryClient.invalidateQueries({ queryKey: personasQueryKey }),
        queryClient.invalidateQueries({ queryKey: managedAgentsQueryKey }),
      ]);
      closeRequest("saved");
      return true;
    } catch (cause) {
      setError(
        cause instanceof Error ? cause.message : "Could not save this agent.",
      );
      return false;
    }
  }

  async function submitUpdate(input: CreatePersonaInput | UpdatePersonaInput) {
    if (request?.action !== "update" || !("id" in input)) {
      return false;
    }
    setError(null);
    try {
      assertAgentCanActFromOrigin(request.request.channelId);
      await updatePersonaMutation.mutateAsync(input);
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: personasQueryKey }),
        queryClient.invalidateQueries({ queryKey: managedAgentsQueryKey }),
      ]);
      closeRequest("saved");
      return true;
    } catch (cause) {
      setError(
        cause instanceof Error ? cause.message : "Could not save this agent.",
      );
      return false;
    }
  }

  // Close the open request and show the next queued one, if any. A dismissal
  // starts the requesting agent's cooldown; a save does not.
  function closeRequest(outcome: "saved" | "dismissed") {
    const next = requestQueue.current.close(outcome);
    if (next) {
      showRequest(next.agentPubkey, next.request);
      return;
    }
    sourceAgentPubkey.current = null;
    setRequestAgentPubkey(null);
    setRequest(null);
  }

  // `requestId` is the request the dialog was opened for. After a save the
  // dialog also reports itself closed; by then the queue may already show the
  // next request, which that stale close must not dismiss.
  function dismiss(requestId?: string) {
    if (
      requestId !== undefined &&
      requestQueue.current.current?.request.requestId !== requestId
    ) {
      return;
    }
    closeRequest("dismissed");
  }

  // Who asked: agent-initiated dialogs must not look like the user's own
  // Create/Edit agent dialog.
  const requesterDescription = React.useMemo(() => {
    if (!request || !requestAgentPubkey) return null;
    const agent = (managedAgentsQuery.data ?? []).find(
      (candidate) =>
        candidate.pubkey.toLowerCase() === requestAgentPubkey.toLowerCase(),
    );
    const channel = (channelsQuery.data ?? []).find(
      (candidate) => candidate.id === request.request.channelId,
    );
    return describeAgentRequester({
      action: request.action,
      agentPubkey: requestAgentPubkey,
      agentName: agent?.name,
      channelName: channel?.name,
    });
  }, [channelsQuery.data, managedAgentsQuery.data, request, requestAgentPubkey]);

  const createInitialValues = React.useMemo(
    () =>
      request?.action === "create" ? createInputFromRequest(request) : null,
    [request],
  );

  const editInitialValues = React.useMemo(() => {
    if (request?.action !== "update" || !currentPersona) return null;
    return updateInputFromRequest(
      request,
      editPersonaDialogState(currentPersona)
        .initialValues as UpdatePersonaInput,
    );
  }, [currentPersona, request]);

  const editError = React.useMemo(() => {
    if (request?.action !== "update") return error;
    if (error) return error;
    if (matchingPersonas.length > 1) {
      return "More than one personal agent has that name. Rename it in Agents, then ask the agent again.";
    }
    if (!currentPersona) {
      return "Agents can only update a personal agent profile by its current name.";
    }
    return null;
  }, [currentPersona, error, matchingPersonas.length, request]);

  return {
    request,
    requesterDescription,
    createInitialValues,
    editInitialValues,
    editError,
    error,
    ...createdAgentAttachment,
    isPending,
    runtimes: runtimesQuery.data ?? [],
    runtimeCatalogStatus: runtimesQuery.isLoading
      ? ("loading" as const)
      : runtimesQuery.isError
        ? ("error" as const)
        : ("ready" as const),
    submitCreate,
    submitUpdate,
    dismiss,
  };
}
