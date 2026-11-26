import type { SnapshotMemoryLevel } from "@/shared/api/tauriPersonas";

/**
 * Destination-scoped confirmation for DMing an agent card.
 *
 * `useSnapshotSendController.beginSend` requires the caller to have obtained
 * explicit, destination-scoped confirmation before sending a memory-bearing
 * payload. An unlocked card that embeds memory carries it as plaintext inside
 * the PNG, and the upload to the relay's media store cannot be recalled, so
 * the card viewer asks first, naming the recipients, exactly as the persona
 * share dialog does. A locked card's manifest is encrypted to the owner and
 * the agent, so recipients cannot read the memory and no confirmation is
 * needed; neither is one for a config-only card.
 */
export type AgentCardSendConfirmation = {
  memoryLabel: "core memory" | "all memories";
  recipientAudience: string;
};

export function agentCardSendConfirmation(
  card: { locked: boolean; memoryLevel: SnapshotMemoryLevel },
  recipientNames: readonly string[],
): AgentCardSendConfirmation | null {
  if (card.locked || card.memoryLevel === "none") return null;
  return {
    memoryLabel: card.memoryLevel === "core" ? "core memory" : "all memories",
    recipientAudience: formatCardRecipientAudience(recipientNames),
  };
}

/** Same wording as the persona share dialog's memory confirmation. */
export function formatCardRecipientAudience(names: readonly string[]): string {
  if (names.length === 0) return "The people you selected";
  if (names.length === 1) return names[0] ?? "The person you selected";
  if (names.length === 2) return `${names[0]} and ${names[1]}`;
  return `${names.slice(0, -1).join(", ")}, and ${names.at(-1)}`;
}
