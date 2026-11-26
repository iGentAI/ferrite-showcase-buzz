import * as React from "react";

import {
  useManagedAgentsQuery,
  useRelayAgentsQuery,
} from "@/features/agents/hooks";
import { useChannelMembersQuery } from "@/features/channels/hooks";
import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * Returns a `Set<string>` of persona IDs whose managed agents are already
 * members of the given channel. The query is only enabled when `enabled` is
 * true (e.g. when the dialog is open).
 */
export function useInChannelPersonaIds(
  channelId: string | null,
  enabled: boolean,
): ReadonlySet<string> {
  const membersQuery = useChannelMembersQuery(channelId, enabled);
  const managedAgentsQuery = useManagedAgentsQuery();
  const relayAgentsQuery = useRelayAgentsQuery({ enabled });

  return React.useMemo(() => {
    const members = membersQuery.data;
    const managedAgents = managedAgentsQuery.data;
    const relayAgents = relayAgentsQuery.data;
    if (!members || !managedAgents || !relayAgents) {
      return new Set<string>();
    }

    const memberPubkeys = new Set(
      members.map((m) => normalizePubkey(m.pubkey)),
    );

    const ids = new Set<string>();
    for (const agent of managedAgents) {
      if (agent.personaId && memberPubkeys.has(normalizePubkey(agent.pubkey))) {
        ids.add(agent.personaId);
      }
    }
    // Hosted Ferrite Cloud personas are relay agents rather than supervised
    // ManagedAgent records. Their stable relay name is also their persona id.
    for (const agent of relayAgents) {
      if (memberPubkeys.has(normalizePubkey(agent.pubkey))) {
        ids.add(agent.name);
      }
    }
    return ids;
  }, [membersQuery.data, managedAgentsQuery.data, relayAgentsQuery.data]);
}
