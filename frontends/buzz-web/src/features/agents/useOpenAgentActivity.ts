import * as React from "react";
import { useIsFetching, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";

import { useAppNavigation } from "@/app/navigation/useAppNavigation";
import {
  channelReferenceQueryKey,
  useChannelReferences,
} from "@/features/channels/openChannelDirectory";
import { useAgentSession } from "@/shared/context/AgentSessionContext";
import type { Channel } from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";
import { getAgentWorkingState } from "./agentWorkingSignal";
import { useRelayAgentsQuery } from "./hooks";

const INACCESSIBLE_ACTIVITY_MESSAGE =
  "This agent is active in a channel you haven't joined, so its activity can't be opened from here.";
const CHANNELS_STILL_LOADING_MESSAGE =
  "Channels are still loading, so this agent's activity can't be opened yet. Try again in a moment.";
/** How long a click made before channels resolve waits for them. */
export const PENDING_ACTIVITY_OPEN_TIMEOUT_MS = 10_000;

/**
 * Decide an explicit-channel activity open. The viewer's channels, and the
 * per-id reference lookup for a channel outside them, load asynchronously;
 * before they settle an unknown channel is not evidence that the viewer is
 * locked out. "wait" means decide again once they settle, instead of telling
 * a member they haven't joined their own channel.
 */
export function decideExplicitActivityOpen({
  channel,
  areChannelsReady,
  isReferencePending,
}: {
  channel: Pick<Channel, "isMember" | "visibility"> | undefined;
  areChannelsReady: boolean;
  /** A per-id reference lookup for this channel is loading or yet to run. */
  isReferencePending: boolean;
}): "open" | "inaccessible" | "wait" {
  if (isChannelOpenable(channel)) return "open";
  if (!areChannelsReady) return "wait";
  if (channel === undefined && isReferencePending) return "wait";
  return "inaccessible";
}

/**
 * Whether a channel-reference lookup has produced an answer: a channel, a
 * cached "not found", or an error. A lookup that has not started (disabled
 * until the member list settles) or is in flight has not.
 */
export function isReferenceLookupSettled(
  state: { status: string; fetchStatus: string } | undefined,
): boolean {
  return (
    state !== undefined &&
    state.fetchStatus === "idle" &&
    (state.status === "success" || state.status === "error")
  );
}

/**
 * Can the viewer actually open this channel? Joined channels always;
 * open-visibility channels are readable without joining. Channels missing
 * from the viewer's channel list (e.g. private rooms they aren't in) are
 * not navigable destinations.
 */
export function isChannelOpenable(
  channel: Pick<Channel, "isMember" | "visibility"> | undefined,
): boolean {
  return (
    channel !== undefined && (channel.isMember || channel.visibility === "open")
  );
}

/**
 * Pick the channel to land in when opening an agent's activity from a
 * non-channel route: the agent's first working channel the viewer can open,
 * else the agent's first member channel the viewer can open, else null.
 */
export function resolveOpenableActivityChannelId({
  agentChannelIds,
  openableChannelIds,
  workingChannelIds,
}: {
  agentChannelIds: readonly string[];
  openableChannelIds: ReadonlySet<string>;
  workingChannelIds: readonly string[];
}): string | null {
  for (const channelId of workingChannelIds) {
    if (openableChannelIds.has(channelId)) {
      return channelId;
    }
  }
  for (const channelId of agentChannelIds) {
    if (openableChannelIds.has(channelId)) {
      return channelId;
    }
  }
  return null;
}

/**
 * Universal ingress for opening an agent's activity pane.
 *
 * Inside a channel screen the AgentSessionContext handler opens the pane in
 * place. Everywhere else (agents page, home profile panel, popovers reached
 * from non-channel routes) there is no provider, so we navigate to a channel
 * with the `agentSession` search param instead — preferring a channel the
 * agent is currently working in (unified working signal), then falling back
 * to the first channel the agent is a member of.
 *
 * Navigation only ever targets channels the viewer can actually open
 * (joined, or open visibility). Owner-global ingestion means the working
 * signal can report activity in rooms the viewer can't access; deep-linking
 * there would land on a screen they can't read. In that case we surface a
 * safe warning instead of navigating — no channel content, no trap-door.
 */
export function useOpenAgentActivity() {
  const { channelId: sessionChannelId, onOpenAgentSession: providerOpen } =
    useAgentSession();
  // A provider whose view renders no activity panel cannot open in place.
  const onOpenAgentSession = sessionChannelId === null ? null : providerOpen;
  const { goChannel } = useAppNavigation();
  const queryClient = useQueryClient();
  const relayAgentsQuery = useRelayAgentsQuery();
  const relayAgents = relayAgentsQuery.data;
  // A click made before channels resolved, decided once they do (F363).
  const [pendingOpen, setPendingOpen] = React.useState<{
    pubkey: string;
    channelId: string | null;
    startedAt: number;
  } | null>(null);
  // Agent metadata and the working-signal snapshot are both finite id sources.
  // Resolve them by id, never by scanning the all-open directory, so an agent
  // can link to a readable open channel the viewer has not browsed this session.
  // A pending explicit target joins them so its reference lookup runs even when
  // no relay agent lists it (e.g. a purely locally managed agent).
  const pendingChannelId = pendingOpen?.channelId ?? null;
  const activityChannelIds = React.useMemo(
    () => [
      ...(relayAgents ?? []).flatMap((agent) => agent.channelIds),
      ...(relayAgents ?? []).flatMap((agent) =>
        getAgentWorkingState(agent.pubkey).channels.map(
          (working) => working.channelId,
        ),
      ),
      ...(pendingChannelId ? [pendingChannelId] : []),
    ],
    [pendingChannelId, relayAgents],
  );
  const { channelsById, isReady: areChannelsReady } =
    useChannelReferences(activityChannelIds);
  // Re-renders when the pending target's reference lookup starts or settles,
  // including a settled miss that leaves channelsById unchanged.
  const pendingReferenceFetches = useIsFetching({
    queryKey: channelReferenceQueryKey(pendingChannelId ?? ""),
    exact: true,
  });

  const resolveChannelId = React.useCallback(
    (pubkey: string): string | null => {
      const key = normalizePubkey(pubkey);
      const relayAgent = relayAgents?.find(
        (agent) => normalizePubkey(agent.pubkey) === key,
      );
      const openableChannelIds = new Set(
        [...channelsById.values()]
          .filter((channel) => isChannelOpenable(channel))
          .map((channel) => channel.id),
      );
      return resolveOpenableActivityChannelId({
        agentChannelIds: relayAgent?.channelIds ?? [],
        openableChannelIds,
        // Deliberately an unsubscribed snapshot: this callback runs on click
        // (and in canOpenAgentActivity), not in render, so we don't need to
        // recompute when working state changes — its deps are only
        // [channelsById, relayAgents]. Worst case the preferred working-channel
        // target lags a just-changed signal; the member-channel fallback in
        // resolveOpenableActivityChannelId keeps the destination valid.
        workingChannelIds: getAgentWorkingState(pubkey).channels.map(
          (working) => working.channelId,
        ),
      });
    },
    [channelsById, relayAgents],
  );

  const canOpenAgentActivity = React.useCallback(
    (pubkey: string | null | undefined): boolean => {
      if (!pubkey) {
        return false;
      }
      if (onOpenAgentSession) {
        return true;
      }
      // While channels are still loading, resolveChannelId can only see an
      // empty openable set and would report a transient false. Stay
      // optimistic until channels resolve so "View activity log" doesn't
      // flicker in on cold start; openAgentActivity still guards the actual
      // navigation.
      if (!areChannelsReady) {
        return true;
      }
      return resolveChannelId(pubkey) !== null;
    },
    [areChannelsReady, onOpenAgentSession, resolveChannelId],
  );

  const openAgentActivity = React.useCallback(
    (pubkey: string, options?: { channelId?: string | null }): boolean => {
      // An explicit channel target (e.g. clicking a "Working in #channel"
      // badge) navigates so the pane opens scoped to that channel — but only
      // when the viewer can actually open that channel. Scoping the pane to
      // an inaccessible room (in place or via navigation) would expose that
      // room's activity content, so we warn and stop instead.
      if (options?.channelId) {
        // In place only for the provider's own channel: another channel's
        // activity (from a profile panel's carousel, say) is navigated to,
        // with its access checked like any explicit open.
        const opensInPlace =
          onOpenAgentSession !== null &&
          (sessionChannelId === undefined ||
            sessionChannelId === options.channelId);
        if (!opensInPlace) {
          const channelId = options.channelId;
          const decision = decideExplicitActivityOpen({
            channel: channelsById.get(channelId),
            areChannelsReady,
            isReferencePending: !isReferenceLookupSettled(
              queryClient.getQueryState(channelReferenceQueryKey(channelId)),
            ),
          });
          if (decision === "open") {
            void goChannel(channelId, { agentSession: pubkey });
            return true;
          }
          if (decision === "wait") {
            setPendingOpen({ pubkey, channelId, startedAt: Date.now() });
            return true;
          }
          toast.warning(INACCESSIBLE_ACTIVITY_MESSAGE);
          return false;
        }
        // A channel-scoped AgentSessionProvider belongs to the channel view
        // already authorized by its route. Do not reject its own current
        // channel while the member/reference query is still settling.
        onOpenAgentSession?.(pubkey, options.channelId);
        return true;
      }
      if (onOpenAgentSession) {
        onOpenAgentSession(pubkey);
        return true;
      }
      const channelId = resolveChannelId(pubkey);
      if (channelId) {
        void goChannel(channelId, { agentSession: pubkey });
        return true;
      }
      // Before channels resolve the openable set is empty; wait for it rather
      // than reporting the agent's channels as inaccessible.
      if (!areChannelsReady) {
        setPendingOpen({ pubkey, channelId: null, startedAt: Date.now() });
        return true;
      }
      // The agent may be working somewhere, just nowhere the viewer can open.
      // Say so plainly rather than failing silently — without leaking which
      // room, or navigating into it.
      if (getAgentWorkingState(pubkey).channels.length > 0) {
        toast.warning(INACCESSIBLE_ACTIVITY_MESSAGE);
      }
      return false;
    },
    [
      areChannelsReady,
      channelsById,
      goChannel,
      onOpenAgentSession,
      queryClient,
      resolveChannelId,
      sessionChannelId,
    ],
  );

  // Decide a pending open once channels (and the target's reference lookup)
  // settle, or give up honestly after a timeout.
  React.useEffect(() => {
    if (!pendingOpen) return;
    const timedOut =
      Date.now() - pendingOpen.startedAt >= PENDING_ACTIVITY_OPEN_TIMEOUT_MS;
    if (pendingOpen.channelId) {
      const decision = decideExplicitActivityOpen({
        channel: channelsById.get(pendingOpen.channelId),
        areChannelsReady,
        isReferencePending: !isReferenceLookupSettled(
          queryClient.getQueryState(
            channelReferenceQueryKey(pendingOpen.channelId),
          ),
        ),
      });
      if (decision === "open") {
        setPendingOpen(null);
        void goChannel(pendingOpen.channelId, {
          agentSession: pendingOpen.pubkey,
        });
        return;
      }
      if (decision === "inaccessible") {
        setPendingOpen(null);
        toast.warning(INACCESSIBLE_ACTIVITY_MESSAGE);
        return;
      }
    } else if (areChannelsReady) {
      setPendingOpen(null);
      const channelId = resolveChannelId(pendingOpen.pubkey);
      if (channelId) {
        void goChannel(channelId, { agentSession: pendingOpen.pubkey });
      } else if (getAgentWorkingState(pendingOpen.pubkey).channels.length > 0) {
        toast.warning(INACCESSIBLE_ACTIVITY_MESSAGE);
      }
      return;
    }
    if (timedOut) {
      setPendingOpen(null);
      toast.info(CHANNELS_STILL_LOADING_MESSAGE);
      return;
    }
    const timer = setTimeout(
      () => setPendingOpen((current) => (current ? { ...current } : current)),
      PENDING_ACTIVITY_OPEN_TIMEOUT_MS -
        (Date.now() - pendingOpen.startedAt),
    );
    return () => clearTimeout(timer);
  }, [
    areChannelsReady,
    channelsById,
    goChannel,
    pendingOpen,
    pendingReferenceFetches,
    queryClient,
    resolveChannelId,
  ]);

  return { canOpenAgentActivity, openAgentActivity };
}
