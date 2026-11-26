/**
 * The app's own relay connection, as this directory's command modules use it.
 *
 * `relayClient` is imported lazily on purpose: it reaches `invoke`, and `invoke`
 * dispatches into these modules, so a static import would be a cycle.
 */
import type { RelaySubscriptionFilter } from "@/shared/api/relayClientShared";
import type { RelayEvent } from "@/shared/api/types";

/**
 * Who a command acts as: the tab's key or its NIP-07 extension's. Handed in
 * rather than imported from `commands.ts`, whose dispatch table reaches every
 * command module, for the same reason.
 */
export type RelayIdentity = {
  sign(input: {
    kind: number;
    content: string;
    tags: string[][];
    created_at?: number;
  }): Promise<Record<string, unknown>>;
  pubkey(): Promise<string>;
};

export async function relayEvents(
  filter: RelaySubscriptionFilter,
): Promise<RelayEvent[]> {
  const { relayClient } = await import("@/shared/api/relayClient");
  return relayClient.fetchEvents(filter);
}

/**
 * Publish one signed event. A relay refusal rejects with the relay's own
 * sentence (`OK, false, "restricted: …"`), which is the message the UI shows;
 * `failed` is the fallback when the relay gave none.
 */
export async function publishSigned(
  signed: unknown,
  failed: string,
  timedOut = "The relay did not answer in time.",
): Promise<void> {
  const { relayClient } = await import("@/shared/api/relayClient");
  await relayClient.publishEvent(signed as never, timedOut, failed);
}
