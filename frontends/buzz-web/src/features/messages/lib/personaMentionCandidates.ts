/**
 * Turning personas into mention candidates, with cloud teammates made SENDABLE.
 *
 * Pure, and separate from `useMentions`.
 *
 * A cloud AI teammate can reach the mention picker through its relay identity or its persona.
 * The current room's signed 39002 roster makes a member identity directly mentionable. A cloud
 * agent outside the room still arrives as a PERSONA candidate, because the browser backend answers
 * `list_personas` from the agent registry so the Agents screen can list and configure teammates.
 *
 * And a persona mention cannot be sent from a browser. The composer reads it as a request to
 * PROVISION a local harness agent in this channel
 * (`useMentionSendFlow.createMentionedPersonaAgents`), asks for the available runtimes, and a
 * browser tab has none — so nothing is sent and, worse, nothing is ASKED.
 *
 * With this resolver the mention carries a pubkey, so the app asks "Mention people outside this
 * channel? … Invite" and answering it sends. Do not delete the persona row instead: it is the only
 * row that exists for a cloud teammate outside the room, so that makes it unmentionable.
 */

/** What this needs from a persona. */
export type SelectablePersona = {
  readonly id: string;
  readonly displayName: string;
  readonly avatarUrl?: string | null;
};

/** What this needs from a relay-side agent. */
export type RelayAgentName = {
  readonly name: string;
  readonly pubkey: string;
  /** The relay identity that owns the agent, when the directory reports one. */
  readonly ownerPubkey?: string | null;
};

/** Who is asking, and what the agent's own policy says about them. */
export type PersonaResolutionPolicy<A extends RelayAgentName = RelayAgentName> = {
  /** The viewer. An agent the viewer owns may resolve their persona by name. */
  readonly currentPubkey?: string | null;
  /**
   * Whether the agent's respond-to policy admits the viewer at all, channel
   * scope aside (see below). Callers without a directory policy to consult
   * leave it out, which admits every agent.
   */
  readonly admitsViewer?: (agent: A) => boolean;
};

/** The candidate shape both branches produce. */
export type PersonaMentionCandidate = {
  readonly kind: "identity" | "persona";
  readonly pubkey?: string;
  readonly personaId?: string;
  readonly displayName: string;
  readonly avatarUrl?: string | null;
  /** Set on a resolved identity, so the picker can show whose agent it is. */
  readonly ownerPubkey?: string | null;
  readonly isMember: false;
  readonly isAgent: true;
};

/**
 * Normalise a name for comparison. One side is a relay-generated agent id and the other a display
 * name someone typed, so neither case nor padding carries meaning.
 */
function comparable(name: string) {
  return name.trim().toLowerCase();
}

function samePubkey(left: string | null | undefined, right: string | null | undefined) {
  return Boolean(left && right && left.trim().toLowerCase() === right.trim().toLowerCase());
}

/**
 * Mention candidates for the given personas, with relay-backed ones resolved to identities.
 *
 * Five rules:
 *
 * * a persona already backing a locally MANAGED agent is skipped — that agent is mentionable by
 *   pubkey, and mentioning the persona would try to start a second one (pre-existing rule);
 * * a persona a RELAY agent backs becomes an identity carrying that agent's pubkey (and owner),
 *   so the mention is sendable;
 * * "backs" means BOUND, not merely same-named: the persona's id is that agent's registry key
 *   (the browser backend answers `list_personas` and `list_relay_agents` from the same registry
 *   row, so a cloud teammate's persona id IS its agent name), or the viewer owns the agent. A
 *   stranger's agent that happens to share a local persona's display name must not capture the
 *   row: it would be rendered with the persona's own name and address a key the user never chose
 *   (F386);
 * * unless that pubkey is ARCHIVED, or the agent's respond-to policy does not admit the viewer
 *   at all (owner-only for someone else, an allowlist without them), in which case the row stays
 *   a persona: pointing a mention at an identity its owner discarded, or at one that will never
 *   answer this viewer, addresses a key nobody reads for them;
 * * anything else stays a persona, which is correct — on a desktop build it provisions a local
 *   agent, and that is what a local persona is for.
 *
 * Names are matched by EQUALITY, never containment: a local persona called "Reviewer" must not be
 * captured by a relay agent called "reviewer-ms93e216", or it would be given the wrong pubkey and
 * address the wrong teammate.
 *
 * CHANNEL scope is deliberately not considered in this fallback. The direct identity candidate
 * already uses the signed current-room roster. For a persona outside the room, the relay resolves
 * read access before starting a turn, and the client offers an invite prompt once the pubkey is
 * present — the right place for that decision to be made. The send path still revalidates every
 * agent pubkey before publishing.
 */
export function personaMentionCandidates<A extends RelayAgentName>(
  personas: readonly SelectablePersona[],
  managedAgentPersonaIds: ReadonlySet<string>,
  relayAgents: readonly A[],
  isArchived: (pubkey: string) => boolean = () => false,
  { currentPubkey = null, admitsViewer = () => true }: PersonaResolutionPolicy<A> = {},
): PersonaMentionCandidate[] {
  // Every agent of a name, in directory order. Several owners can publish agents with one
  // display name, and an archived one stays listed, so the choice waits until each
  // candidate's binding, archive state and policy are known; among those that qualify the
  // first listed wins, so an existing mention keeps addressing the same agent.
  const agentsByName = new Map<string, { agent: A; pubkey: string }[]>();
  for (const agent of relayAgents) {
    const name = comparable(agent?.name ?? "");
    const pubkey = (agent?.pubkey ?? "").trim().toLowerCase();
    if (!name || !pubkey) continue;
    const list = agentsByName.get(name);
    if (list) list.push({ agent, pubkey });
    else agentsByName.set(name, [{ agent, pubkey }]);
  }

  const candidates: PersonaMentionCandidate[] = [];
  for (const persona of personas) {
    if (managedAgentPersonaIds.has(persona.id)) continue;
    const name = comparable(persona.displayName);
    if (!name) continue;
    // An archived pubkey, or an agent whose policy never answers this viewer, is not a
    // resolution: the row survives as a persona rather than pointing a mention at it.
    const resolved = (agentsByName.get(name) ?? []).find(
      ({ agent: found, pubkey }) =>
        (comparable(persona.id ?? "") === comparable(found.name) ||
          samePubkey(found.ownerPubkey, currentPubkey)) &&
        !isArchived(pubkey) &&
        admitsViewer(found),
    );
    const agent = resolved?.agent;
    candidates.push(
      resolved && agent
        ? {
            kind: "identity",
            pubkey: resolved.pubkey,
            displayName: persona.displayName,
            avatarUrl: persona.avatarUrl ?? null,
            ownerPubkey: agent.ownerPubkey ?? null,
            isMember: false,
            isAgent: true,
          }
        : {
            kind: "persona",
            personaId: persona.id,
            displayName: persona.displayName,
            avatarUrl: persona.avatarUrl ?? null,
            isMember: false,
            isAgent: true,
          },
    );
  }
  return candidates;
}
