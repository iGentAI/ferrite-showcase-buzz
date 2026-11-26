/**
 * Which agent-initiated management request the review dialog shows, and when.
 *
 * An owned agent can ask, through the Buzz harness, to create or change an
 * agent.
 * - a request id is recorded only when its dialog is actually shown, so a
 *   request that arrives while another is on screen is not lost;
 * - a request that arrives while a dialog is open waits in a bounded FIFO and
 *   is shown when the current one closes;
 * - each agent has at most one request on screen or waiting, and after the
 *   user dismisses its dialog it is ignored for a cooldown. A dropped request
 *   is not recorded, so the agent can ask again later;
 * - the seen set is bounded (oldest ids are forgotten first).
 *
 * The queue keeps the requesting agent with each request so the dialog can say
 * who asked.
 */

const AGENT_REQUEST_COOLDOWN_MS = 60_000;
const MAX_QUEUED_AGENT_REQUESTS = 10;
const MAX_SEEN_AGENT_REQUEST_IDS = 500;

export type QueuedAgentRequest<R extends { requestId: string }> = {
  agentPubkey: string;
  request: R;
};

export type AgentRequestOfferOutcome =
  | "shown"
  | "queued"
  | "duplicate"
  | "rate_limited"
  | "queue_full";

export type AgentRequestQueueOptions = {
  now?: () => number;
  cooldownMs?: number;
  maxQueued?: number;
  maxSeen?: number;
};

export class AgentManagementRequestQueue<R extends { requestId: string }> {
  private readonly now: () => number;
  private readonly cooldownMs: number;
  private readonly maxQueued: number;
  private readonly maxSeen: number;
  private readonly seen = new Set<string>();
  private readonly waiting: QueuedAgentRequest<R>[] = [];
  private readonly cooldownUntil = new Map<string, number>();
  private shown: QueuedAgentRequest<R> | null = null;

  constructor(options: AgentRequestQueueOptions = {}) {
    this.now = options.now ?? Date.now;
    this.cooldownMs = options.cooldownMs ?? AGENT_REQUEST_COOLDOWN_MS;
    this.maxQueued = options.maxQueued ?? MAX_QUEUED_AGENT_REQUESTS;
    this.maxSeen = options.maxSeen ?? MAX_SEEN_AGENT_REQUEST_IDS;
  }

  /** The request whose dialog is open, if any. */
  get current(): QueuedAgentRequest<R> | null {
    return this.shown;
  }

  get queuedCount(): number {
    return this.waiting.length;
  }

  hasSeen(requestId: string): boolean {
    return this.seen.has(requestId);
  }

  /** Offer an already origin-checked request. */
  offer(agentPubkey: string, request: R): AgentRequestOfferOutcome {
    const agent = agentPubkey.toLowerCase();
    const id = request.requestId;
    if (
      this.seen.has(id) ||
      this.shown?.request.requestId === id ||
      this.waiting.some((entry) => entry.request.requestId === id)
    ) {
      return "duplicate";
    }

    const now = this.now();
    for (const [key, until] of this.cooldownUntil) {
      if (until <= now) this.cooldownUntil.delete(key);
    }
    if (this.cooldownUntil.has(agent)) return "rate_limited";
    if (
      this.shown?.agentPubkey === agent ||
      this.waiting.some((entry) => entry.agentPubkey === agent)
    ) {
      return "rate_limited";
    }

    const entry = { agentPubkey: agent, request };
    if (this.shown === null) {
      this.show(entry);
      return "shown";
    }
    if (this.waiting.length >= this.maxQueued) return "queue_full";
    this.waiting.push(entry);
    return "queued";
  }

  /**
   * Close the open dialog and return the next request to show, if any.
   * `dismissed` (the user closed it without saving) starts the requesting
   * agent's cooldown; `saved` does not.
   */
  close(outcome: "saved" | "dismissed"): QueuedAgentRequest<R> | null {
    if (this.shown === null) return null;
    if (outcome === "dismissed") {
      this.cooldownUntil.set(
        this.shown.agentPubkey,
        this.now() + this.cooldownMs,
      );
    }
    this.shown = null;
    const next = this.waiting.shift() ?? null;
    if (next) this.show(next);
    return next;
  }

  private show(entry: QueuedAgentRequest<R>) {
    this.shown = entry;
    this.seen.add(entry.request.requestId);
    while (this.seen.size > this.maxSeen) {
      const oldest = this.seen.values().next().value;
      if (oldest === undefined) break;
      this.seen.delete(oldest);
    }
  }
}

/**
 * The dialog copy that says an agent, not the user, opened it. The name is the
 * managed agent's name when known, otherwise a shortened pubkey.
 */
export function describeAgentRequester(params: {
  action: "create" | "update";
  agentPubkey: string;
  agentName?: string | null;
  channelName?: string | null;
}): string {
  const who =
    params.agentName?.trim() ||
    `Agent ${params.agentPubkey.slice(0, 8)}…${params.agentPubkey.slice(-4)}`;
  const where = params.channelName?.trim()
    ? ` in #${params.channelName.trim()}`
    : "";
  return params.action === "create"
    ? `${who}${where} is asking to create this agent. Review it; nothing is created unless you confirm.`
    : `${who}${where} is asking to change this agent. Review the changes; nothing is saved unless you choose Save changes.`;
}
