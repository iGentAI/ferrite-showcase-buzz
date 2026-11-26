import {
  coalesceAgentAutocompleteCandidates,
  shouldHideAgentFromMentions,
} from "@/features/agents/lib/agentAutocompleteEligibility";
import type { UserProfileLookup } from "@/features/profile/lib/identity";
import type {
  AgentPersona,
  ChannelMember,
  ManagedAgent,
  RelayAgent,
  UserSearchResult,
} from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";
import {
  formatSearchUserDisplayName,
  formatSearchUserSecondaryLabel,
  type MentionCandidate,
  mentionCandidateLabel,
  profileRecency,
} from "./mentionCandidates";
import { personaMentionCandidates } from "./personaMentionCandidates";

/** Directories and rosters the mention picker merges into one candidate list. */
export type BuildMentionCandidatesInput = {
  activeAgentPubkeys: ReadonlySet<string>;
  activePersonaById: ReadonlyMap<string, AgentPersona>;
  /** Already narrowed to `isActive` personas. */
  activePersonas: readonly AgentPersona[];
  canSearchGlobalUsers: boolean;
  currentPubkey: string | null;
  isArchived: (pubkey: string) => boolean;
  managedAgentDirectoryReady: boolean;
  managedAgentNamesByPubkey: ReadonlyMap<string, string>;
  managedAgentPersonaIds: ReadonlySet<string>;
  managedAgentPersonaIdsByPubkey: ReadonlyMap<string, string>;
  managedAgents: readonly ManagedAgent[] | undefined;
  memberPubkeys: ReadonlySet<string>;
  members: readonly ChannelMember[] | undefined;
  mentionChannelId: string | null;
  mentionableAgentPubkeys: ReadonlySet<string>;
  personaNameByPubkey: ReadonlyMap<string, string>;
  profiles: UserProfileLookup | undefined;
  relayAgentDirectoryReady: boolean;
  relayAgentNamesByPubkey: ReadonlyMap<string, string>;
  relayAgents: readonly RelayAgent[] | undefined;
  userSearchResults: readonly UserSearchResult[];
};

/**
 * Whether a relay agent's respond-to policy admits the viewer at all, channel
 * scope aside: the owner under any policy, an allowlisted viewer, or anyone
 * for `anyone`. Used when a persona is resolved to its relay agent, where the
 * channel half is left to the invite prompt (see `personaMentionCandidates`)
 * but an agent that will never answer this viewer must not be offered.
 */
function respondToPolicyAdmitsViewer(
  agent: RelayAgent,
  currentPubkey: string | null,
): boolean {
  const viewer = currentPubkey ? normalizePubkey(currentPubkey) : null;
  if (agent.respondTo === null) return false;
  if (
    viewer &&
    agent.ownerPubkey &&
    normalizePubkey(agent.ownerPubkey) === viewer
  ) {
    return true;
  }
  if (agent.respondTo === "allowlist") {
    return Boolean(
      viewer &&
        agent.respondToAllowlist.some(
          (pubkey) => normalizePubkey(pubkey) === viewer,
        ),
    );
  }
  return agent.respondTo === "anyone";
}

/**
 * Merge the channel roster, agent directories, global people search, and
 * standalone personas into the deduplicated candidate list the mention
 * autocomplete ranks. Archived identities and agents the viewer may not
 * mention are dropped; identities appearing in several sources are coalesced
 * into a single entry that keeps the richest field from each.
 */
export function buildMentionCandidates({
  activeAgentPubkeys,
  activePersonaById,
  activePersonas,
  canSearchGlobalUsers,
  currentPubkey,
  isArchived,
  managedAgentDirectoryReady,
  managedAgentNamesByPubkey,
  managedAgentPersonaIds,
  managedAgentPersonaIdsByPubkey,
  managedAgents,
  memberPubkeys,
  members,
  mentionChannelId,
  mentionableAgentPubkeys,
  personaNameByPubkey,
  profiles,
  relayAgentDirectoryReady,
  relayAgentNamesByPubkey,
  relayAgents,
  userSearchResults,
}: BuildMentionCandidatesInput): MentionCandidate[] {
  const candidatesByPubkey = new Map<string, MentionCandidate>();
  const addCandidate = (candidate: MentionCandidate & { pubkey: string }) => {
    const pubkey = normalizePubkey(candidate.pubkey);
    if (isArchived(pubkey)) {
      return;
    }
    if (
      shouldHideAgentFromMentions({
        isAgent: candidate.isAgent === true,
        pubkey,
        mentionableAgentPubkeys,
        directoryReady:
          candidate.isManagedAgent === true
            ? managedAgentDirectoryReady
            : relayAgentDirectoryReady,
      })
    ) {
      return;
    }
    const current = candidatesByPubkey.get(pubkey);
    if (!current) {
      candidatesByPubkey.set(pubkey, { ...candidate, pubkey });
      return;
    }
    candidatesByPubkey.set(pubkey, {
      ...current,
      avatarUrl: current.avatarUrl ?? candidate.avatarUrl ?? null,
      displayName:
        current.isAgent && !candidate.isAgent
          ? current.displayName
          : candidate.isAgent && !current.isAgent
            ? (candidate.displayName ?? current.displayName)
            : (current.displayName ?? candidate.displayName),
      isAgent: current.isAgent || candidate.isAgent,
      isActiveAgent: current.isActiveAgent || candidate.isActiveAgent,
      isMember: current.isMember || candidate.isMember,
      personaId: current.personaId ?? candidate.personaId,
      personaName: current.personaName ?? candidate.personaName ?? null,
      role: current.role ?? candidate.role ?? null,
      secondaryLabel:
        current.secondaryLabel ?? candidate.secondaryLabel ?? null,
      ownerPubkey:
        current.ownerPubkey ??
        candidate.ownerPubkey ??
        (candidate.isAgent && candidate.pubkey
          ? profiles?.[pubkey]?.ownerPubkey
          : null) ??
        null,
      isManagedAgent: current.isManagedAgent || candidate.isManagedAgent,
      // A channel member and a global-search hit for the SAME pubkey merge
      // here, and only the search side carries the kind-0 timestamp. Losing
      // it left every member candidate undated, so the recency ordering had
      // nothing to separate same-named members by and fell back to roster
      // order — which is pubkey order, i.e. random.
      profileCreatedAt:
        Math.max(
          current.profileCreatedAt ?? 0,
          candidate.profileCreatedAt ?? 0,
        ) || null,
    });
  };
  for (const member of members ?? []) {
    const pubkey = normalizePubkey(member.pubkey);
    const linkedPersonaId = activePersonaById.has(pubkey) ? pubkey : undefined;
    const agentName =
      managedAgentNamesByPubkey.get(pubkey) ??
      relayAgentNamesByPubkey.get(pubkey) ??
      null;
    const profile = profiles?.[pubkey] ?? null;
    addCandidate({
      kind: "identity",
      pubkey,
      displayName:
        member.displayName?.trim() ||
        agentName ||
        profile?.displayName?.trim() ||
        profile?.nip05Handle?.trim() ||
        null,
      avatarUrl: profile?.avatarUrl ?? null,
      isMember: true,
      personaId: managedAgentPersonaIdsByPubkey.get(pubkey) ?? linkedPersonaId,
      isAgent:
        member.isAgent === true ||
        profile?.isAgent === true ||
        member.role === "bot" ||
        managedAgentNamesByPubkey.has(pubkey) ||
        relayAgentNamesByPubkey.has(pubkey),
      isActiveAgent: activeAgentPubkeys.has(pubkey),
      isManagedAgent: managedAgentNamesByPubkey.has(pubkey),
      ownerPubkey: profile?.ownerPubkey ?? null,
      personaName: personaNameByPubkey.get(pubkey) ?? null,
      role: member.role,
      secondaryLabel:
        profile?.displayName?.trim() && profile?.nip05Handle?.trim()
          ? profile.nip05Handle
          : null,
      // A roster gives an order, not a recency. Without this a workspace where
      // two members share a display name resolved the @mention to whichever
      // the roster listed first, and the p tag addressed a pubkey nobody reads.
      profileCreatedAt: profileRecency(profile),
    });
  }
  for (const agent of relayAgents ?? []) {
    const pubkey = normalizePubkey(agent.pubkey);
    addCandidate({
      kind: "identity",
      pubkey,
      displayName: agent.name,
      // Prefer the active channel's signed roster. The relay-agent directory
      // is filtered by access policy, so its channel ids can legitimately omit
      // a room where this identity is already a member.
      isMember:
        memberPubkeys.has(pubkey) ||
        (mentionChannelId !== null &&
          agent.channelIds.includes(mentionChannelId)),
      personaId:
        managedAgentPersonaIdsByPubkey.get(pubkey) ??
        (activePersonaById.has(pubkey) ? pubkey : undefined),
      ownerPubkey: agent.ownerPubkey,
      isAgent: true,
      isActiveAgent: agent.status === "online" || agent.status === "away",
    });
  }
  for (const agent of managedAgents ?? []) {
    const pubkey = normalizePubkey(agent.pubkey);
    addCandidate({
      kind: "identity",
      pubkey,
      displayName: agent.name,
      isMember: memberPubkeys.has(pubkey),
      isAgent: true,
      isActiveAgent: agent.status === "running" || agent.status === "deployed",
      isManagedAgent: true,
      personaId: agent.personaId ?? undefined,
      personaName:
        personaNameByPubkey.get(normalizePubkey(agent.pubkey)) ?? null,
      ownerPubkey: currentPubkey,
    });
  }
  if (canSearchGlobalUsers) {
    for (const user of userSearchResults) {
      const pubkey = normalizePubkey(user.pubkey);
      addCandidate({
        kind: "identity",
        pubkey,
        displayName: formatSearchUserDisplayName(user),
        avatarUrl: user.avatarUrl ?? null,
        personaId:
          managedAgentPersonaIdsByPubkey.get(pubkey) ??
          (activePersonaById.has(pubkey) ? pubkey : undefined),
        isMember: false,
        isAgent:
          user.isAgent ||
          managedAgentNamesByPubkey.has(pubkey) ||
          relayAgentNamesByPubkey.has(pubkey),
        personaName: personaNameByPubkey.get(pubkey) ?? null,
        secondaryLabel: formatSearchUserSecondaryLabel(user),
        ownerPubkey: user.ownerPubkey ?? null,
        isGlobalSearchResult: true,
        isManagedAgent: managedAgentNamesByPubkey.has(pubkey),
        // The newest kind-0's timestamp, so recency ordering lists the LIVE
        // identity ahead of stale same-named profiles.
        profileCreatedAt: user.createdAt ?? null,
      });
    }
  }
  // Personas, with cloud teammates RESOLVED to the pubkey that makes their
  // mention sendable. A persona mention asks the send path to provision a
  // LOCAL harness agent, which a browser tab cannot do; a pubkey mention
  // becomes a p tag and, when needed, the non-member invite prompt. The
  // reasoning and its measurement live in `personaMentionCandidates`.
  const personaCandidates: MentionCandidate[] = personaMentionCandidates(
    activePersonas,
    managedAgentPersonaIds,
    relayAgents ?? [],
    isArchived,
    {
      currentPubkey,
      admitsViewer: (agent) => respondToPolicyAdmitsViewer(agent, currentPubkey),
    },
  );
  // Same-named global-search identities are NOT collapsed: every input to
  // such a collapse is chosen by the identity's owner, so it let an impostor
  // remove the genuine person from the picker (see mentionCandidates.ts). Each
  // pubkey keeps its own row and the picker disambiguates colliding names.
  return coalesceAgentAutocompleteCandidates(
    [...candidatesByPubkey.values(), ...personaCandidates],
    {
      currentPubkey,
      getLabel: mentionCandidateLabel,
      preferredPubkeys: memberPubkeys,
    },
  );
}
