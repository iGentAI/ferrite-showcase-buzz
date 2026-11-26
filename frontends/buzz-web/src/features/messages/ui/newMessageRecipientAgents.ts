import { shouldHideAgentFromMentions } from "@/features/agents/lib/agentAutocompleteEligibility";
import type { ManagedAgent, UserSearchResult } from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * A directory hit with what this client KNOWS about it as an agent. A kind-0
 * profile's `bot` flag is self-asserted and may be absent, so a pubkey the
 * managed-agent or relay-agent directory lists is an agent whatever its
 * profile says: classified as a person, a restricted agent would skip the
 * eligibility gate and render without its agent badge.
 */
export function withKnownAgentMetadata<T extends UserSearchResult>(
  candidate: T,
  {
    managedAgentsByPubkey,
    relayAgentPubkeys,
  }: {
    managedAgentsByPubkey: ReadonlyMap<string, ManagedAgent>;
    relayAgentPubkeys: ReadonlySet<string>;
  },
): T & { isManagedAgent: boolean; personaId: string | null | undefined } {
  const pubkey = normalizePubkey(candidate.pubkey);
  const agent = managedAgentsByPubkey.get(pubkey);
  return {
    ...candidate,
    isAgent: Boolean(candidate.isAgent || agent || relayAgentPubkeys.has(pubkey)),
    isManagedAgent: Boolean(agent),
    personaId: agent?.personaId,
  };
}

/**
 * Whether a candidate may be offered as a DM recipient: a person always, an
 * agent only when the agent directories loaded without error and list it as
 * reachable. A directory whose refresh failed hides agents rather than
 * offering ones that may since have been revoked.
 */
export function admitsRecipientCandidate({
  isAgent,
  pubkey,
  eligibleAgentPubkeys,
  directoryReady,
}: {
  isAgent: boolean;
  pubkey: string;
  eligibleAgentPubkeys: ReadonlySet<string>;
  directoryReady: boolean;
}) {
  return !shouldHideAgentFromMentions({
    isAgent,
    pubkey,
    mentionableAgentPubkeys: eligibleAgentPubkeys,
    directoryReady,
  });
}
