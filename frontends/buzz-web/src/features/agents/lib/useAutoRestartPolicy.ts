import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";

import {
  managedAgentsQueryKey,
  useManagedAgentsQuery,
} from "@/features/agents/hooks";
import {
  clearActiveTurnsForAgentOnStop,
  managedAgentRuntimesQueryKey,
} from "@/features/agents/managedAgentRuntimeHooks";
import {
  startManagedAgentRuntime,
  stopManagedAgentRuntime,
} from "@/shared/api/tauriManagedAgents";
import { listManagedAgents } from "@/shared/api/tauri";
import type { ManagedAgent } from "@/shared/api/types";
import { useDocumentVisible } from "@/shared/lib/useDocumentVisible";
import { useIdentityQuery } from "@/shared/api/hooks";
import { useCommunities } from "@/features/communities/useCommunities";
import {
  getAgentObserverSnapshot,
  subscribeAgentObserverStore,
} from "../observerRelayStore";
import {
  getAgentWorkingState,
  subscribeAgentWorkingSignal,
} from "../agentWorkingSignal";
import {
  decideAutoRestart,
  executeAutoRestart,
  nextEdgeState,
  stillFiresBeforeRestart,
  type AutoRestartEdgeState,
} from "./autoRestartPolicy";

function sleep(ms: number) {
  return new Promise<void>((resolve) => setTimeout(resolve, ms));
}

function errorText(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "unknown error";
}

/** How often the policy re-evaluates between summary refetches. Keeps the
 * continuity clock honest without waiting for the next 5s poll. */
const POLICY_TICK_MS = 15_000;

/**
 * Chunk F policy loop: watches managed-agent summaries and auto-restarts
 * drifted, idle, connected, local agents (per-agent opt-out, default ON).
 *
 * All decision logic lives in `decideAutoRestart` (pure, exhaustively
 * tested). This hook only wires inputs, owns per-pubkey edge state, and
 * calls the existing stop/start commands — both idempotent and serialized
 * on the backend store lock, so a cross-window double-fire is benign (and
 * further shrunk by the pre-fire summary re-fetch).
 */
export function useAutoRestartPolicy() {
  const queryClient = useQueryClient();
  const agents: ManagedAgent[] | undefined = useManagedAgentsQuery().data;
  const edgesRef = React.useRef(new Map<string, AutoRestartEdgeState>());
  const inFlightRef = React.useRef(new Set<string>());
  const [, setTick] = React.useState(0);
  const documentVisible = useDocumentVisible();

  // The scope a restart runs in. A delayed start retry re-checks it so it
  // never acts after a community or identity switch, or after unmount.
  const { activeCommunity } = useCommunities();
  const identityPubkey = useIdentityQuery().data?.pubkey ?? null;
  const scopeRef = React.useRef("");
  scopeRef.current = `${activeCommunity?.id ?? ""}|${identityPubkey ?? ""}`;
  const mountedRef = React.useRef(true);
  React.useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  // The mid-turn gate below reads the working signal imperatively. Hold a
  // subscription for the policy's lifetime so the signal's turn-store wiring
  // (and so its cache invalidation) is live whenever the policy runs, even on
  // screens where no working badge is mounted.
  //
  // Both subscriptions also disarm the quiescence clock of any agent that is
  // working or whose observer is not open the moment that happens, rather
  // than at the next tick, where a short blip would be missed
  // (finding_3cd9b54f0b209534). Only refs change: no re-render per frame.
  React.useEffect(() => {
    const disarmInterrupted = () => {
      for (const [pubkey, edge] of edgesRef.current) {
        if (edge.armedAt === null) continue;
        const interrupted =
          getAgentWorkingState(pubkey).working ||
          getAgentObserverSnapshot(pubkey, true).connectionState !== "open";
        if (interrupted) edgesRef.current.set(pubkey, { ...edge, armedAt: null });
      }
    };
    const unsubscribeWorking = subscribeAgentWorkingSignal(disarmInterrupted);
    const unsubscribeObserver = subscribeAgentObserverStore(disarmInterrupted);
    return () => {
      unsubscribeWorking();
      unsubscribeObserver();
    };
  }, []);

  // Re-evaluate on an interval so the quiescence clock advances even when
  // summaries and observer stores are quiet.
  React.useEffect(() => {
    if (!documentVisible) {
      // Nothing is evaluated while hidden, so continuity cannot be vouched
      // for across that time: disarm every clock.
      for (const [pubkey, edge] of edgesRef.current) {
        edgesRef.current.set(pubkey, { ...edge, armedAt: null });
      }
      return;
    }

    setTick((t) => t + 1);
    const timer = setInterval(() => setTick((t) => t + 1), POLICY_TICK_MS);
    return () => clearInterval(timer);
  }, [documentVisible]);

  // No dependency array by design: the tick pattern re-runs this effect
  // every render so it reads live store state; all mutation is ref-local.
  React.useEffect(() => {
    if (!agents) return;
    const now = Date.now();
    const edges = edgesRef.current;

    for (const agent of agents) {
      const isRunning = agent.status === "running";
      const edge = nextEdgeState(edges.get(agent.pubkey), {
        needsRestart: agent.needsRestart,
        isRunning,
      });

      const working = getAgentWorkingState(agent.pubkey);
      const observer = getAgentObserverSnapshot(agent.pubkey, true);

      const decision = decideAutoRestart({
        autoRestartEnabled: agent.autoRestartOnConfigChange,
        needsRestart: agent.needsRestart,
        working: working.working,
        workingSource: working.source,
        connected: observer.connectionState === "open",
        isLocalBackend: agent.backend.type === "local",
        isRunning,
        edgeConsumed: edge.consumed,
        quiescentForMs: edge.armedAt === null ? 0 : now - edge.armedAt,
      });

      if (decision === "hold") {
        edges.set(agent.pubkey, { ...edge, armedAt: null });
        continue;
      }
      if (decision === "arm") {
        edges.set(agent.pubkey, {
          ...edge,
          armedAt: edge.armedAt ?? now,
        });
        continue;
      }

      // decision === "fire"
      if (inFlightRef.current.has(agent.pubkey)) continue;
      // The restart is scoped to the (agent, active community) pair, the only
      // pair this loop observes. Without a resolved community and identity
      // there is no pair to act on (finding_c869b4eaae1635ff,
      // finding_03f1ff440772b20f).
      const relayUrl = activeCommunity?.relayUrl?.trim();
      if (!relayUrl || !identityPubkey) continue;
      const firedInScope = scopeRef.current;
      const ownsScope = () =>
        mountedRef.current && scopeRef.current === firedInScope;
      inFlightRef.current.add(agent.pubkey);
      // Consume the edge BEFORE the attempt: a failed restart badges only
      // until needsRestart cycles (edge-triggered debounce, no retry loops).
      // The one exception is a start that fails after a successful stop,
      // which executeAutoRestart retries a bounded number of times.
      edges.set(agent.pubkey, { consumed: true, armedAt: null });

      void (async () => {
        try {
          // Pre-fire re-fetch: shrink the stale-decision window to ~0, and
          // re-run the whole predicate on the fresh record and live stores.
          const fresh = await listManagedAgents();
          if (!ownsScope()) return;
          const current = fresh.find((a) => a.pubkey === agent.pubkey);
          if (!current) return;
          const currentWorking = getAgentWorkingState(agent.pubkey);
          if (
            !stillFiresBeforeRestart({
              autoRestartEnabled: current.autoRestartOnConfigChange,
              needsRestart: current.needsRestart,
              working: currentWorking.working,
              workingSource: currentWorking.source,
              connected:
                getAgentObserverSnapshot(agent.pubkey, true).connectionState ===
                "open",
              isLocalBackend: current.backend.type === "local",
              isRunning: current.status === "running",
            })
          ) {
            return;
          }
          const outcome = await executeAutoRestart({
            // Pair-scoped: an agent-wide stop would also end its runtimes,
            // and any turns in flight, in communities this loop cannot see.
            stop: () => stopManagedAgentRuntime(agent.pubkey, relayUrl),
            afterStop: () =>
              clearActiveTurnsForAgentOnStop(agent.pubkey, relayUrl),
            start: () => startManagedAgentRuntime(agent.pubkey, relayUrl),
            sleep,
            stillOwnsRetry: async () => {
              if (!ownsScope()) return false;
              // Someone may have started (or removed) the agent meanwhile.
              const latest = await listManagedAgents();
              if (!ownsScope()) return false;
              const record = latest.find((a) => a.pubkey === agent.pubkey);
              return record !== undefined && record.status !== "running";
            },
          });
          if (outcome.kind === "stop-failed") {
            // The agent is still running: badge-only until the needsRestart
            // edge cycles. No retry loop by design.
            console.warn(
              `Auto-restart of ${current.name} could not stop it`,
              outcome.error,
            );
          } else if (outcome.kind === "start-failed") {
            // The agent is down and the loop never acts on stopped agents,
            // so say so instead of leaving it silently stopped.
            console.error(
              `Auto-restart stopped ${current.name} but could not start it again`,
              outcome.error,
            );
            toast.error(
              `${current.name} was stopped to apply new settings but couldn't start again (${errorText(outcome.error)}). Start it from Agents.`,
            );
          } else if (outcome.kind === "retry-cancelled") {
            console.warn(
              `Auto-restart of ${current.name}: start failed and the retry was cancelled because the agent or the active workspace changed`,
              outcome.error,
            );
          }
        } catch (error) {
          // The pre-fire re-fetch failed before anything was stopped: edge
          // stays consumed — badge-only until the needsRestart edge cycles.
          console.warn(`Auto-restart check for ${agent.name} failed`, error);
        } finally {
          inFlightRef.current.delete(agent.pubkey);
          void queryClient.invalidateQueries({
            queryKey: managedAgentsQueryKey,
          });
          void queryClient.invalidateQueries({
            queryKey: managedAgentRuntimesQueryKey,
          });
        }
      })();
    }

    // Drop edge state for agents that no longer exist.
    const known = new Set(agents.map((a) => a.pubkey));
    for (const pubkey of edges.keys()) {
      if (!known.has(pubkey)) edges.delete(pubkey);
    }
  });
}
