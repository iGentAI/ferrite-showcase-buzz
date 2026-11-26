import type { AgentWorkingSource } from "../agentWorkingSignal";

/**
 * Chunk F auto-restart policy — the pure decision core.
 *
 * SAFETY-CRITICAL: the stop command is SIGTERM → ≤1s → SIGKILL with no
 * in-process drain, so this predicate is the ONLY thing standing between
 * the policy loop and killing a mid-turn agent. Every never-fire condition
 * below is load-bearing; the test matrix enumerates them exhaustively.
 *
 * Scope: the stop/start commands the loop fires are pair-scoped to the
 * active community, and `needsRestart` reports drift for that same pair —
 * a fire bounces only the community being viewed, never an agent's pairs
 * in other communities.
 *
 * Decisions:
 * - "fire": restart now (all gates green, continuity window satisfied).
 * - "arm": conditions are green but the quiescence window is still
 *   accumulating — keep the timer running.
 * - "hold": some gate is red — reset any accumulated quiescence and show
 *   the badge only.
 */
export type AutoRestartDecision = "fire" | "arm" | "hold";

export type AutoRestartInputs = {
  /** Per-agent opt-out toggle (record field, default ON). */
  autoRestartEnabled: boolean;
  /** Config drift detected by the summary poll (`needsRestart`). */
  needsRestart: boolean;
  /** Unified working signal for this agent (any channel). */
  working: boolean;
  /** Strongest working-signal source; "none" is ambiguous (idle OR absent
   * observer stream) and therefore never sufficient to fire on its own —
   * the connected gate plus the continuity window carry that risk. */
  workingSource: AgentWorkingSource;
  /** Observer relay connection state; anything but "connected" inhibits. */
  connected: boolean;
  /** Only local agents can be restarted by this loop. */
  isLocalBackend: boolean;
  /** Agent process status from the summary ("running" required). */
  isRunning: boolean;
  /** Edge-trigger state: true when this needsRestart rising edge has
   * already consumed its one attempt (failed or in flight). */
  edgeConsumed: boolean;
  /** Milliseconds the fire-conditions have held continuously. */
  quiescentForMs: number;
};

/** Continuity window: fire-conditions must hold this long uninterrupted.
 * 3 minutes = 18× the 10s turn-liveness cadence — comfortably beyond any
 * relay hiccup that could make a mid-turn agent look idle (the turn store
 * prunes after only 25s, which is why this window is minutes-scale). */
export const AUTO_RESTART_QUIESCENCE_MS = 3 * 60 * 1000;

export function decideAutoRestart(
  inputs: AutoRestartInputs,
): AutoRestartDecision {
  const {
    autoRestartEnabled,
    needsRestart,
    working,
    workingSource,
    connected,
    isLocalBackend,
    isRunning,
    edgeConsumed,
    quiescentForMs,
  } = inputs;

  // Never-fire gates. Each resets the continuity window ("hold").
  if (!autoRestartEnabled) return "hold";
  if (!needsRestart) return "hold";
  if (!isLocalBackend) return "hold";
  if (!isRunning) return "hold";
  if (!connected) return "hold";
  // Any working signal — observer OR typing — defers. `working` and
  // `workingSource` travel together, but check both so a partial reader
  // can never slip through.
  if (working || workingSource !== "none") return "hold";
  // One attempt per rising edge: a consumed edge badges until it cycles.
  if (edgeConsumed) return "hold";

  return quiescentForMs >= AUTO_RESTART_QUIESCENCE_MS ? "fire" : "arm";
}

/**
 * Per-agent edge-trigger state, keyed by pubkey in the policy hook.
 *
 * Rearm rule (Pinky's review row): the edge resets when `needsRestart`
 * falls OR when the agent stops — a manual stop/start cycle re-arms the
 * edge so a subsequently drifting agent auto-fires again.
 */
export type AutoRestartEdgeState = {
  consumed: boolean;
  /** Wall-clock ms when fire-conditions began holding; null = not armed. */
  armedAt: number | null;
};

export function nextEdgeState(
  previous: AutoRestartEdgeState | undefined,
  inputs: { needsRestart: boolean; isRunning: boolean },
): AutoRestartEdgeState {
  const prior = previous ?? { consumed: false, armedAt: null };
  // Falling edge or a stopped agent re-arms.
  if (!inputs.needsRestart || !inputs.isRunning) {
    return { consumed: false, armedAt: null };
  }
  return prior;
}

/**
 * Pre-fire re-validation: re-run the full predicate against freshly read
 * inputs just before stop/start, rather than a hand-picked subset of its
 * gates. The continuity window and the edge were already satisfied when the
 * loop decided to fire, so only the live gates (enabled, drift, local
 * backend, running, observer connected, not working) can change the answer.
 * Dropping `connected` here would let a restart proceed at exactly the moment
 * the working signal went blind.
 */
export function stillFiresBeforeRestart(
  inputs: Omit<AutoRestartInputs, "edgeConsumed" | "quiescentForMs">,
): boolean {
  return (
    decideAutoRestart({
      ...inputs,
      edgeConsumed: false,
      quiescentForMs: AUTO_RESTART_QUIESCENCE_MS,
    }) === "fire"
  );
}

/** Delays before re-trying a start that failed after a successful stop. */
export const AUTO_RESTART_START_RETRY_DELAYS_MS = [2_000, 5_000] as const;

export type AutoRestartOutcome =
  | { kind: "restarted" }
  /** The stop failed; the agent was not taken down. */
  | { kind: "stop-failed"; error: unknown }
  /** The stop succeeded but every start attempt failed: the agent is down. */
  | { kind: "start-failed"; error: unknown; attempts: number }
  /**
   * The first start failed and a retry was no longer ours to make (the
   * community or identity changed, the policy unmounted, or someone else
   * already started the agent), so the remaining retries were cancelled.
   */
  | { kind: "retry-cancelled"; error: unknown; attempts: number };

/**
 * Run one auto-restart: stop, then start with bounded retries.
 *
 * The loop only ever acts on running agents, so an agent it stopped but could
 * not start again would stay stopped for good, with nothing retrying it. A
 * start failure after a successful stop is therefore retried a bounded number
 * of times and, if it still fails, reported as `start-failed` so the caller
 * can tell the user their agent is down.
 */
export async function executeAutoRestart({
  stop,
  afterStop,
  start,
  sleep,
  stillOwnsRetry,
  retryDelaysMs = AUTO_RESTART_START_RETRY_DELAYS_MS,
}: {
  stop: () => Promise<unknown>;
  afterStop?: () => void;
  start: () => Promise<unknown>;
  sleep: (ms: number) => Promise<void>;
  /**
   * Re-checked after every backoff, immediately before a retried start. The
   * pre-fire checks ran in the context the restart began in; a retry seconds
   * later must not act in a different community or identity, after the
   * policy has gone away, or on an agent the user has since started.
   */
  stillOwnsRetry: () => Promise<boolean>;
  retryDelaysMs?: readonly number[];
}): Promise<AutoRestartOutcome> {
  try {
    await stop();
  } catch (error) {
    return { kind: "stop-failed", error };
  }
  afterStop?.();

  let lastError: unknown;
  for (let attempt = 0; attempt <= retryDelaysMs.length; attempt++) {
    if (attempt > 0) {
      await sleep(retryDelaysMs[attempt - 1]);
      let owned = false;
      try {
        owned = await stillOwnsRetry();
      } catch {
        owned = false;
      }
      if (!owned) {
        return { kind: "retry-cancelled", error: lastError, attempts: attempt };
      }
    }
    try {
      await start();
      return { kind: "restarted" };
    } catch (error) {
      lastError = error;
    }
  }
  return {
    kind: "start-failed",
    error: lastError,
    attempts: retryDelaysMs.length + 1,
  };
}
