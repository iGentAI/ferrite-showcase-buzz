import { resolveTeamPersonas } from "@/features/agents/lib/teamPersonas";
import type {
  AgentPersona,
  AgentTeam,
  ChannelRole,
  UserSearchResult,
} from "@/shared/api/types";
import { truncateNpub } from "@/shared/lib/pubkey";

export function formatSearchUserDisplayName(user: UserSearchResult) {
  return user.displayName?.trim() || user.nip05Handle?.trim() || null;
}

export function formatSearchUserSecondaryLabel(user: UserSearchResult) {
  const displayName = user.displayName?.trim();
  const nip05Handle = user.nip05Handle?.trim();
  return displayName && nip05Handle ? nip05Handle : null;
}

export function appendUniqueName(current: string[], name: string): string[] {
  return current.some(
    (candidate) => candidate.toLowerCase() === name.toLowerCase(),
  )
    ? current
    : [...current, name];
}

export type TeamMentionMember = {
  displayName: string;
  kind: "identity" | "persona";
  personaId?: string;
  pubkey?: string;
};

export type MentionCandidate = {
  kind: "identity" | "persona" | "team";
  pubkey?: string;
  personaId?: string;
  teamId?: string;
  teamMembers?: TeamMentionMember[];
  displayName: string | null;
  avatarUrl?: string | null;
  isMember: boolean;
  role?: ChannelRole | null;
  personaName?: string | null;
  secondaryLabel?: string | null;
  ownerPubkey?: string | null;
  isAgent: boolean;
  isActiveAgent?: boolean;
  isManagedAgent?: boolean;
  isGlobalSearchResult?: boolean;
  /** Unix seconds of the newest kind-0 behind a global-search identity;
   * null/absent when the backend does not report it (the desktop mirror). */
  profileCreatedAt?: number | null;
};

export function mentionCandidateLabel(candidate: MentionCandidate) {
  return (
    candidate.displayName ??
    (candidate.pubkey ? truncateNpub(candidate.pubkey) : "agent")
  );
}

/*
 * Same-named global-search identities are deliberately NOT collapsed onto one
 * suggestion. Every input to such a collapse — display name, nip05 label and
 * kind-0 `created_at` — is chosen by the identity's owner, so whichever rule
 * picked the survivor (first seen, or newest profile) let an impostor copy a
 * person's name and handle and remove the genuine identity from the picker,
 * binding the mention's `p` tag to the impostor. Each pubkey keeps its own row;
 * `MentionAutocomplete` shows the npub of every row whose name collides, and
 * `orderMentionCandidatesByRecency` still lists the most recently used channel
 * MEMBER first, so a live colleague is never hidden behind stale same-named
 * roster profiles.
 */

/**
 * The recency fact a candidate built from a channel's ROSTER carries.
 *
 * A roster `p` tag is a pubkey and a role; everything else about that person
 * comes from the batch profile read, so this is the only place a member
 * candidate can learn when its identity was last used. Before `get_users_batch`
 * reported the kind-0 timestamp there was nothing to read here, member
 * candidates were all undated, and two members sharing a display name could
 * only be separated by roster position — which is pubkey order, i.e. chance.
 */
export function profileRecency(
  profile: { createdAt?: number | null } | null | undefined,
): number | null {
  return profile?.createdAt ?? null;
}

/**
 * Order candidates so the RECENTLY-USED identity wins a ranking tie.
 *
 * `rankMentionCandidates` sorts by group, then by how well the label matches,
 * then by input order — and then the composer keeps only the first 50. On a
 * long-lived workspace that last tie-break decides who is mentionable: #general
 * on the dev relay holds 580 members, 134 of them named "Bo Nilsson <run>", and
 * every one of them is a channel member matching "@Bo" with the identical
 * prefix score. Input order there is roster order, the roster is sorted by
 * pubkey, so which Bo survived the slice was effectively a coin toss — the live
 * colleague who had just joined was cut about two times in three.
 *
 * Sorting by kind-0 recency first makes that tie-break mean something: the
 * newest profile is the identity its owner most recently used. Candidates with
 * no timestamp (personas, teams, members the profile read did not date) keep
 * their relative order and sit after the dated ones, so a workspace with no
 * duplicates is ordered exactly as before.
 */
export function orderMentionCandidatesByRecency<
  T extends {
    profileCreatedAt?: number | null;
    isMember?: boolean;
    isGlobalSearchResult?: boolean;
  },
>(
  candidates: readonly T[],
  nowSeconds: number = Math.floor(Date.now() / 1000),
): T[] {
  // `created_at` is self-asserted. A profile dated in the future is ranked as
  // if it were written now. And an outsider found only by global search gets
  // no recency at all: anyone can mint same-named profiles with fresh
  // timestamps, and ranking outsiders by them would let a batch of impostors
  // push an older genuine person past the picker's result cap. Outsiders keep
  // the search's own order; roster members (ranked in their own group first)
  // keep recency, which is what separates their stale same-named profiles.
  const recency = (candidate: T) =>
    candidate.isGlobalSearchResult === true && candidate.isMember === false
      ? 0
      : Math.min(candidate.profileCreatedAt ?? 0, nowSeconds);
  return [...candidates].sort((a, b) => recency(b) - recency(a));
}

function findTeamMemberTarget(
  persona: AgentPersona,
  candidates: readonly MentionCandidate[],
): TeamMentionMember | null {
  const linked = candidates
    .filter(
      (candidate) =>
        candidate.kind !== "team" && candidate.personaId === persona.id,
    )
    .sort((left, right) => {
      const rank = (candidate: MentionCandidate) => {
        if (candidate.kind === "identity" && candidate.isMember) return 0;
        if (candidate.kind === "identity" && candidate.isManagedAgent) return 1;
        if (candidate.kind === "identity") return 2;
        return 3;
      };
      return rank(left) - rank(right);
    })[0];

  if (linked) {
    return {
      displayName: linked.displayName?.trim() || persona.displayName,
      kind: linked.kind === "identity" ? "identity" : "persona",
      personaId: linked.personaId,
      pubkey: linked.pubkey,
    };
  }

  return persona.isActive
    ? {
        displayName: persona.displayName,
        kind: "persona",
        personaId: persona.id,
      }
    : null;
}

/** Build autocomplete entries for editable, locally owned teams. */
export function buildTeamMentionCandidates(
  teams: readonly AgentTeam[],
  personas: AgentPersona[],
  candidates: readonly MentionCandidate[],
): MentionCandidate[] {
  return teams.flatMap((team) => {
    if (team.isBuiltin || !team.name.trim()) return [];

    const resolution = resolveTeamPersonas(team, personas);
    if (!resolution.isUsable) return [];

    const teamMembers = resolution.resolvedPersonas
      .map((persona) => findTeamMemberTarget(persona, candidates))
      .filter((member): member is TeamMentionMember => member !== null);
    if (teamMembers.length !== resolution.resolvedPersonas.length) return [];

    const mentionNames = new Map<string, TeamMentionMember>();
    for (const member of teamMembers) {
      const mentionName = member.displayName.trim().toLowerCase();
      const previous = mentionNames.get(mentionName);
      // Exact-key members can reserve distinct labels at selection time. A
      // persona without a key cannot yet be disambiguated that way.
      if (previous && (!previous.pubkey || !member.pubkey)) return [];
      mentionNames.set(mentionName, member);
    }

    return [
      {
        kind: "team" as const,
        teamId: team.id,
        teamMembers,
        displayName: team.name.trim(),
        isMember: false,
        isAgent: true,
      },
    ];
  });
}

export function formatTeamMention(
  teamName: string,
  members: readonly TeamMentionMember[],
) {
  return `${teamName}(${members.map((member) => `@${member.displayName}`).join(" ")}) `;
}
