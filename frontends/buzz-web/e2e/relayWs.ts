/**
 * A minimal NIP-01/NIP-42 socket client, for asserting what the relay will and
 * will not SERVE to a given key.
 *
 * Why this exists at all: some access rules are enforced at the FILTER layer,
 * before any event is considered, and a refusal there is a `CLOSED` frame with
 * a reason — a wire fact no browser surface exposes. The shipped client only
 * ever subscribes with filters it is allowed to use, so driving it can never
 * distinguish "the relay refused this filter" from "the client never asked".
 *
 * Deliberately tiny and deliberately not a relay library: one connection, one
 * authenticated identity, sequential REQs, and every frame the relay sent kept
 * for the failure message. Mirrors the handshake in `e2e/probe-window.mjs` and
 * `e2e/probe-membership-notice.mjs`, which are the scripts this was extracted
 * from after they proved the shape works against the live relay.
 */
import { finalizeEvent } from "nostr-tools/pure";

import { RELAY_WS } from "./onboarding";
import type { Identity } from "./relayHttp";

export type WireEvent = {
  id: string;
  kind: number;
  pubkey: string;
  content: string;
  created_at: number;
  tags: string[][];
};

/** One REQ's outcome: what came back, and whether the relay refused it. */
export type ReqOutcome = {
  events: WireEvent[];
  /** The relay's `CLOSED` reason, or null when it served the subscription. */
  closed: string | null;
  eose: boolean;
};

export type RelaySocket = {
  /** Run one REQ to completion (EOSE, CLOSED, or the budget). */
  req(filter: Record<string, unknown>, budgetMs?: number): Promise<ReqOutcome>;
  /**
   * The most recent frames the relay sent (at most `FRAME_LIMIT`), for a
   * failure message worth reading.
   */
  frames: string[];
  /** Close the socket; a REQ still waiting settles at once with what it has. */
  close(): void;
};

/** How many relay frames a socket keeps for failure messages. */
export const FRAME_LIMIT = 200;

/**
 * Open a socket and, when `who` is given, complete the NIP-42 exchange.
 *
 * The relay sends its `AUTH` challenge unprompted on connect, so the challenge
 * is awaited rather than requested. Passing no identity leaves the connection
 * anonymous on purpose: that is how the `auth-required:` refusal is observed.
 */
export async function openRelaySocket(
  who?: Identity,
  budgetMs = 45_000,
): Promise<RelaySocket> {
  const ws = new WebSocket(RELAY_WS);
  const frames: string[] = [];
  const okFor = new Map<string, (result: [boolean, string]) => void>();
  const subs = new Map<
    string,
    { outcome: ReqOutcome; finish: (outcome: ReqOutcome) => void }
  >();
  let challenge: string | null = null;
  // Set when the socket closes: every wait (challenge, AUTH OK, REQ) then
  // ends at once instead of running out its budget.
  let socketClosed: string | null = null;

  const opened = new Promise<void>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`the relay socket never opened (${RELAY_WS})`)),
      budgetMs,
    );
    ws.addEventListener("open", () => {
      clearTimeout(timer);
      resolve();
    });
    ws.addEventListener("error", () => {
      clearTimeout(timer);
      reject(new Error(`the relay socket errored (${RELAY_WS})`));
    });
  });

  ws.addEventListener("message", (event: MessageEvent) => {
    const raw = String(event.data);
    frames.push(raw.slice(0, 300));
    if (frames.length > FRAME_LIMIT) frames.splice(0, frames.length - FRAME_LIMIT);
    let parsed: unknown;
    try {
      parsed = JSON.parse(raw);
    } catch {
      return;
    }
    if (!Array.isArray(parsed)) return;
    const message: unknown[] = parsed;
    const verb = message[0];
    if (verb === "AUTH" && typeof message[1] === "string") {
      challenge = message[1];
      return;
    }
    if (verb === "OK" && typeof message[1] === "string") {
      const settle = okFor.get(message[1]);
      if (settle) {
        okFor.delete(message[1]);
        settle([Boolean(message[2]), String(message[3] ?? "")]);
      }
      return;
    }
    if (typeof message[1] !== "string") return;
    const sub = subs.get(message[1]);
    if (!sub) return;
    if (verb === "EVENT") {
      sub.outcome.events.push(message[2] as WireEvent);
      return;
    }
    if (verb === "EOSE") {
      sub.outcome.eose = true;
      sub.finish(sub.outcome);
      return;
    }
    if (verb === "CLOSED") {
      sub.outcome.closed = String(message[2] ?? "");
      sub.finish(sub.outcome);
    }
  });

  ws.addEventListener("close", (event: CloseEvent) => {
    socketClosed = `the relay socket closed (code ${event.code})`;
    for (const settle of [...okFor.values()]) settle([false, socketClosed]);
    okFor.clear();
    for (const sub of [...subs.values()]) {
      sub.outcome.closed ??= socketClosed;
      sub.finish(sub.outcome);
    }
  });

  await opened;

  // A failed handshake closes the socket it opened: the caller never gets a
  // handle to close it.
  try {
    await authenticate();
  } catch (error) {
    ws.close();
    throw error;
  }

  async function authenticate(): Promise<void> {
    if (!who) return;
    // The challenge arrives on its own, a beat after the socket opens.
    const deadline = Date.now() + budgetMs;
    while (challenge === null) {
      if (socketClosed !== null) {
        throw new Error(`${socketClosed} before the NIP-42 challenge`);
      }
      if (Date.now() > deadline) {
        throw new Error(
          `the relay sent no NIP-42 challenge; frames: ${frames.join(" | ").slice(0, 400)}`,
        );
      }
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
    const authEvent = finalizeEvent(
      {
        kind: 22242,
        created_at: Math.floor(Date.now() / 1000),
        tags: [
          ["relay", RELAY_WS],
          ["challenge", challenge],
        ],
        content: "",
      },
      who.secret,
    );
    const accepted = await new Promise<[boolean, string]>((resolve, reject) => {
      // The budget timer is cleared the moment the OK arrives, and the waiter
      // is dropped when it fires: neither may outlive the handshake.
      const timer = setTimeout(() => {
        okFor.delete(authEvent.id);
        reject(new Error("the relay never answered the NIP-42 AUTH"));
      }, budgetMs);
      okFor.set(authEvent.id, (result) => {
        clearTimeout(timer);
        resolve(result);
      });
      ws.send(JSON.stringify(["AUTH", authEvent]));
    });
    if (!accepted[0]) {
      throw new Error(`the relay refused the NIP-42 AUTH: ${accepted[1]}`);
    }
  }

  let nextId = 0;
  return {
    frames,
    close: () => {
      // Settle every REQ still waiting (its timer is cleared in `finish`), so
      // no budget timer keeps the worker alive after the socket is gone.
      for (const sub of [...subs.values()]) sub.finish(sub.outcome);
      ws.close();
    },
    req: (filter, reqBudgetMs = 30_000) => {
      const subId = `probe-${(nextId += 1)}`;
      const outcome: ReqOutcome = { events: [], closed: null, eose: false };
      if (socketClosed !== null) {
        outcome.closed = socketClosed;
        return Promise.resolve(outcome);
      }
      return new Promise<ReqOutcome>((resolve) => {
        let settled = false;
        let timer: ReturnType<typeof setTimeout> | undefined;
        const finish = (result: ReqOutcome) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          subs.delete(subId);
          // CLOSE only after a served subscription: the relay has already
          // dropped a refused one, and closing it again is a frame the test
          // would have to explain.
          if (result.closed === null && ws.readyState === WebSocket.OPEN) {
            ws.send(JSON.stringify(["CLOSE", subId]));
          }
          resolve(result);
        };
        subs.set(subId, { outcome, finish });
        ws.send(JSON.stringify(["REQ", subId, filter]));
        // A budget expiry is NOT an error here: "the relay answered nothing" is
        // itself an outcome a caller may need to assert (`eose: false`), and
        // turning it into a throw would hide which of the three happened.
        timer = setTimeout(() => finish(outcome), reqBudgetMs);
      });
    },
  };
}
