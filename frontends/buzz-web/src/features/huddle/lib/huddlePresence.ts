import type { RelaySubscriptionFilter } from "@/shared/api/relayClientShared";
import type { RelayEvent } from "@/shared/api/types";
import {
  KIND_HUDDLE_ENDED,
  KIND_HUDDLE_PARTICIPANT_JOINED,
  KIND_HUDDLE_PARTICIPANT_LEFT,
  KIND_HUDDLE_STARTED,
} from "@/shared/constants/kinds";
import { normalizePubkey } from "@/shared/lib/pubkey";

type AdmissionState = {
  present: boolean;
  rosterRevision: number | null;
  generation: string | null;
  createdAt: number;
  eventId: string;
};

type LivenessGeneration = string;

type HuddleSession = {
  startCreator: string;
  startCreatedAt: number;
  startEventId: string;
  /** Parent channel (`h` tag) named by the start that opened the session. */
  parentChannelId: string | null;
  /**
   * Set once a START from a different author named this session. A START is
   * its publisher's own claim, not relay-attested, so from then on nobody can
   * tell which author really opened the session and neither may end it: only
   * a relay-signed END (or liveness) can.
   */
  startCreatorContested: boolean;
  endState: AdmissionState | null;
  /** Whether `endState` came from the relay rather than the start's author. */
  endSignedByRelay: boolean;
  admissionsByParticipant: Map<string, Map<string, AdmissionState>>;
  legacyStateByParticipant: Map<string, AdmissionState>;
  latestRosterRevision: number | null;
  latestRosterCreatedAt: number;
  latestRosterEventId: string;
  generation: LivenessGeneration | null;
  generationIsAuthoritative: boolean;
  departedAdmissions: Map<string, AdmissionState>;
  compactedRosterRevisionFloor: number | null;
};

type LifecycleContent = {
  ephemeralChannelId: string | null;
  rosterRevision: number | null;
  admissionId: string | null;
  generation: string | null;
};

/** Compare monotonic decimal mesh generations; opaque epochs are unordered. */
export function compareHuddleGenerations(
  candidate: LivenessGeneration,
  current: LivenessGeneration,
): number | null {
  if (!/^\d+$/.test(candidate) || !/^\d+$/.test(current)) return null;
  const candidateEpoch = BigInt(candidate);
  const currentEpoch = BigInt(current);
  return candidateEpoch < currentEpoch
    ? -1
    : candidateEpoch > currentEpoch
      ? 1
      : 0;
}

function lifecycleContent(event: RelayEvent): LifecycleContent {
  try {
    const content = JSON.parse(event.content) as {
      ephemeral_channel_id?: unknown;
      roster_revision?: unknown;
      admission_id?: unknown;
      generation?: unknown;
    };
    return {
      ephemeralChannelId:
        typeof content.ephemeral_channel_id === "string" &&
        content.ephemeral_channel_id
          ? content.ephemeral_channel_id
          : null,
      rosterRevision:
        typeof content.roster_revision === "number" &&
        Number.isSafeInteger(content.roster_revision) &&
        content.roster_revision >= 0
          ? content.roster_revision
          : null,
      admissionId:
        typeof content.admission_id === "string" && content.admission_id
          ? content.admission_id
          : null,
      generation:
        typeof content.generation === "string" && content.generation
          ? content.generation
          : null,
    };
  } catch {
    return {
      ephemeralChannelId: null,
      rosterRevision: null,
      admissionId: null,
      generation: null,
    };
  }
}

export function huddleSessionId(event: RelayEvent): string | null {
  return lifecycleContent(event).ephemeralChannelId;
}

export function huddleParentChannelId(event: RelayEvent): string | null {
  const value = event.tags.find((tag) => tag[0] === "h")?.[1] ?? "";
  return value.trim() || null;
}

export function huddleLifecycleGeneration(event: RelayEvent): string | null {
  return lifecycleContent(event).generation;
}

function participantPubkey(event: RelayEvent): string | null {
  const value = event.tags.find((tag) => tag[0] === "p")?.[1] ?? event.pubkey;
  const normalized = normalizePubkey(value);
  return normalized || null;
}

function lifecyclePhase(kind: number): number {
  if (kind === KIND_HUDDLE_STARTED) return 0;
  if (kind === KIND_HUDDLE_ENDED) return 2;
  return 1;
}

export function compareHuddleLifecycleEvents(
  left: RelayEvent,
  right: RelayEvent,
): number {
  const timestampOrder = left.created_at - right.created_at;
  if (timestampOrder !== 0) return timestampOrder;

  const phaseOrder = lifecyclePhase(left.kind) - lifecyclePhase(right.kind);
  if (phaseOrder !== 0) return phaseOrder;

  const leftRevision = lifecycleContent(left).rosterRevision;
  const rightRevision = lifecycleContent(right).rosterRevision;
  if (
    leftRevision !== null &&
    rightRevision !== null &&
    leftRevision !== rightRevision
  ) {
    return leftRevision - rightRevision;
  }

  return left.id.localeCompare(right.id);
}

function isNewerAdmissionState(
  candidate: AdmissionState,
  existing: AdmissionState | undefined,
): boolean {
  if (!existing) return true;
  if (
    candidate.rosterRevision !== null &&
    existing.rosterRevision !== null &&
    candidate.rosterRevision !== existing.rosterRevision
  ) {
    return candidate.rosterRevision > existing.rosterRevision;
  }
  if (candidate.createdAt !== existing.createdAt) {
    return candidate.createdAt > existing.createdAt;
  }
  return candidate.eventId > existing.eventId;
}

function participantHasExplicitPresence(
  session: HuddleSession,
  participant: string,
): boolean {
  const admissions = session.admissionsByParticipant.get(participant);
  if (admissions?.size) {
    return [...admissions.values()].some((admission) => admission.present);
  }
  return session.legacyStateByParticipant.get(participant)?.present ?? false;
}

function participantIsPresent(
  session: HuddleSession,
  participant: string,
): boolean {
  return participantHasExplicitPresence(session, participant);
}

function sessionParticipants(session: HuddleSession): Set<string> {
  const candidates = new Set<string>([
    ...session.admissionsByParticipant.keys(),
    ...session.legacyStateByParticipant.keys(),
  ]);
  return new Set(
    [...candidates].filter((participant) =>
      participantIsPresent(session, participant),
    ),
  );
}

type FetchEvents = (filter: RelaySubscriptionFilter) => Promise<RelayEvent[]>;

export const HUDDLE_LIFECYCLE_PAGE_LIMIT = 500;

/** Sessions a tracker retains before it starts evicting (see snapshot). */
export const MAX_TRACKED_HUDDLE_SESSIONS = 1_000;

function sessionHasPresence(session: HuddleSession): boolean {
  return sessionParticipants(session).size > 0;
}

/**
 * Load complete persisted huddle lifecycle history.
 *
 * A backing channel's TTL is not an audio-session lifetime: already-connected
 * participants can remain in a room after the channel is archived, and relay
 * deployments may configure a longer TTL. Pagination therefore continues to
 * the beginning of lifecycle history instead of imposing a client-side time
 * horizon. Inclusive `until` pages are de-duplicated by event id.
 */
export async function fetchHuddleLifecycleHistory(
  fetchEvents: FetchEvents,
  channelIds?: string[],
): Promise<RelayEvent[]> {
  const events = new Map<string, RelayEvent>();
  let until: number | undefined;
  let beforeId: string | undefined;

  for (;;) {
    const page = await fetchEvents({
      kinds: [
        KIND_HUDDLE_STARTED,
        KIND_HUDDLE_PARTICIPANT_JOINED,
        KIND_HUDDLE_PARTICIPANT_LEFT,
        KIND_HUDDLE_ENDED,
      ],
      ...(channelIds?.length ? { "#h": channelIds } : {}),
      ...(until === undefined ? {} : { until }),
      limit: HUDDLE_LIFECYCLE_PAGE_LIMIT,
      ...(beforeId === undefined ? {} : { before_id: beforeId }),
    });
    for (const event of page) events.set(event.id, event);
    if (page.length < HUDDLE_LIFECYCLE_PAGE_LIMIT) break;

    const terminal = [...page]
      .sort((left, right) =>
        left.created_at !== right.created_at
          ? right.created_at - left.created_at
          : left.id.localeCompare(right.id),
      )
      .at(-1);
    if (!terminal) break;
    if (until === terminal.created_at && beforeId === terminal.id) {
      throw new Error(
        "Could not load active huddles: pagination cursor did not advance.",
      );
    }
    until = terminal.created_at;
    beforeId = terminal.id;
  }

  return [...events.values()];
}

/** Incremental, bounded reconstruction of authenticated active huddles. */
export class HuddlePresenceTracker {
  private readonly relaySelf: string;
  private readonly sessions = new Map<string, HuddleSession>();

  constructor(relaySelfPubkey: string | null | undefined) {
    this.relaySelf = normalizePubkey(relaySelfPubkey ?? "");
  }

  apply(event: RelayEvent): boolean {
    if (!this.relaySelf) return false;
    const content = lifecycleContent(event);
    const sessionId = content.ephemeralChannelId;
    if (!sessionId) return false;

    if (event.kind === KIND_HUDDLE_STARTED) {
      const creator = normalizePubkey(event.pubkey);
      if (!creator) return false;
      const parentChannelId = huddleParentChannelId(event);
      const existing = this.sessions.get(sessionId);
      if (existing?.startEventId === event.id) return false;
      if (existing && creator !== existing.startCreator) {
        // Any member of a parent channel can publish a START naming a live
        // session id. It must not wipe the relay-authenticated admissions or
        // hand its author END authority, whichever of the two starts arrives
        // first; it only revokes the creator's END authority for good.
        existing.startCreatorContested = true;
        // A backdated START and END pair from one member must not keep a live
        // huddle hidden: once the creator is contested, an END only the
        // (now disputed) creator signed no longer stands, and relay-signed
        // joins apply again (finding_80ac99a47c8ad44b).
        if (existing.endState && !existing.endSignedByRelay) {
          existing.endState = null;
          return true;
        }
        return false;
      }
      // Once two authors have claimed the session, no START (not even a newer
      // one from whoever happened to be seen first) may reset its
      // relay-authenticated admissions.
      if (existing?.startCreatorContested) return false;
      if (
        existing &&
        (parentChannelId !== existing.parentChannelId ||
          event.created_at < existing.startCreatedAt ||
          (event.created_at === existing.startCreatedAt &&
            event.id > existing.startEventId))
      ) {
        return false;
      }
      this.sessions.set(sessionId, {
        startCreator: creator,
        startCreatedAt: event.created_at,
        startEventId: event.id,
        parentChannelId,
        startCreatorContested: existing?.startCreatorContested ?? false,
        endState: null,
        endSignedByRelay: false,
        admissionsByParticipant: new Map(),
        legacyStateByParticipant: new Map(),
        latestRosterRevision: null,
        latestRosterCreatedAt: 0,
        latestRosterEventId: "",
        generation: null,
        generationIsAuthoritative: false,
        departedAdmissions: new Map(),
        compactedRosterRevisionFloor: null,
      });
      return true;
    }

    const session = this.sessions.get(sessionId);
    if (!session) return false;

    const generation = content.generation;
    if (event.kind === KIND_HUDDLE_ENDED) {
      const signer = normalizePubkey(event.pubkey);
      const creatorMayEnd =
        !session.startCreatorContested && signer === session.startCreator;
      if (signer !== this.relaySelf && !creatorMayEnd) {
        return false;
      }
      if (
        generation !== null &&
        session.generation !== null &&
        generation !== session.generation
      ) {
        // Teardown from an older room generation must not end the currently
        // live room. A relay-authenticated JOIN or liveness snapshot is the
        // authoritative boundary between generations.
        return false;
      }
      if (generation !== null) session.generation = generation;
      const nextEnd: AdmissionState = {
        present: false,
        rosterRevision: null,
        generation: content.generation,
        createdAt: event.created_at,
        eventId: event.id,
      };
      if (!isNewerAdmissionState(nextEnd, session.endState ?? undefined)) {
        return false;
      }
      session.endState = nextEnd;
      session.endSignedByRelay = signer === this.relaySelf;
      session.admissionsByParticipant.clear();
      session.legacyStateByParticipant.clear();
      session.departedAdmissions.clear();
      return true;
    }

    if (session.endState) return false;
    if (
      event.kind !== KIND_HUDDLE_PARTICIPANT_JOINED &&
      event.kind !== KIND_HUDDLE_PARTICIPANT_LEFT
    ) {
      return false;
    }
    if (normalizePubkey(event.pubkey) !== this.relaySelf) return false;

    const participant = participantPubkey(event);
    if (!participant) return false;

    if (
      generation !== null &&
      session.generation !== null &&
      generation !== session.generation
    ) {
      const generationOrder = compareHuddleGenerations(
        generation,
        session.generation,
      );
      if (session.generationIsAuthoritative && generationOrder !== 1) {
        return false;
      }
      if (event.kind === KIND_HUDDLE_PARTICIPANT_LEFT) return false;
      // Mesh generations are monotonic. An authenticated higher-generation JOIN
      // may supersede liveness; opaque non-mesh epochs require a fresh snapshot.
      session.admissionsByParticipant.clear();
      session.legacyStateByParticipant.clear();
      session.departedAdmissions.clear();
      session.compactedRosterRevisionFloor = null;
    }

    const next: AdmissionState = {
      present: event.kind === KIND_HUDDLE_PARTICIPANT_JOINED,
      rosterRevision: content.rosterRevision,
      generation: content.generation,
      createdAt: event.created_at,
      eventId: event.id,
    };

    const isAfterLatestRosterEvent =
      event.created_at > session.latestRosterCreatedAt ||
      (event.created_at === session.latestRosterCreatedAt &&
        event.id > session.latestRosterEventId);
    if (
      content.rosterRevision !== null &&
      session.latestRosterRevision !== null &&
      content.rosterRevision < session.latestRosterRevision &&
      isAfterLatestRosterEvent &&
      (generation === null ||
        session.generation === null ||
        generation !== session.generation) &&
      (session.compactedRosterRevisionFloor === null ||
        content.rosterRevision > session.compactedRosterRevisionFloor)
    ) {
      // Relay roster revisions are process-local. A strictly lower revision
      // arriving after the latest authenticated roster event starts a new room
      // generation, so admissions from the previous relay process are dead.
      // Equal revisions are valid: a remote join publishes a post-admission
      // snapshot revision that can match a concurrent local mutation.
      session.admissionsByParticipant.clear();
      session.legacyStateByParticipant.clear();
      session.departedAdmissions.clear();
      session.compactedRosterRevisionFloor = null;
    }
    if (generation !== null) session.generation = generation;

    if (
      !next.present &&
      next.rosterRevision !== null &&
      session.compactedRosterRevisionFloor !== null &&
      next.rosterRevision <= session.compactedRosterRevisionFloor
    ) {
      return false;
    }

    if (content.admissionId) {
      const admissions =
        session.admissionsByParticipant.get(participant) ??
        new Map<string, AdmissionState>();
      const departed = session.departedAdmissions.get(content.admissionId);
      const existing = admissions.get(content.admissionId) ?? departed;
      if (!isNewerAdmissionState(next, existing)) return false;
      if (
        !existing &&
        next.rosterRevision !== null &&
        session.compactedRosterRevisionFloor !== null &&
        next.rosterRevision <= session.compactedRosterRevisionFloor
      ) {
        return false;
      }
      if (next.present) {
        session.departedAdmissions.delete(content.admissionId);
        admissions.set(content.admissionId, next);
        session.admissionsByParticipant.set(participant, admissions);
      } else {
        admissions.delete(content.admissionId);
        if (admissions.size === 0) {
          session.admissionsByParticipant.delete(participant);
        }
        session.departedAdmissions.delete(content.admissionId);
        session.departedAdmissions.set(content.admissionId, next);
        while (session.departedAdmissions.size > 1_000) {
          const oldest = session.departedAdmissions.entries().next().value as
            | [string, AdmissionState]
            | undefined;
          if (!oldest) break;
          session.departedAdmissions.delete(oldest[0]);
          if (oldest[1].rosterRevision !== null) {
            session.compactedRosterRevisionFloor = Math.max(
              session.compactedRosterRevisionFloor ?? -1,
              oldest[1].rosterRevision,
            );
          }
        }
      }
    } else {
      const existing = session.legacyStateByParticipant.get(participant);
      if (!isNewerAdmissionState(next, existing)) return false;
      session.legacyStateByParticipant.set(participant, next);
    }
    if (content.rosterRevision !== null && isAfterLatestRosterEvent) {
      session.latestRosterRevision = content.rosterRevision;
      session.latestRosterCreatedAt = event.created_at;
      session.latestRosterEventId = event.id;
    }
    return true;
  }

  reconcileLiveness(
    generations: ReadonlyMap<string, LivenessGeneration>,
    previousGenerations: ReadonlyMap<string, LivenessGeneration> = new Map(),
  ): void {
    for (const [sessionId, session] of this.sessions) {
      const generation = generations.get(sessionId);
      if (generation === undefined) continue;
      const previous = previousGenerations.get(sessionId) ?? session.generation;
      if (
        previous !== null &&
        previous !== undefined &&
        previous !== generation
      ) {
        session.admissionsByParticipant.clear();
        session.legacyStateByParticipant.clear();
        session.departedAdmissions.clear();
        session.compactedRosterRevisionFloor = null;
      }
      session.generation = generation;
      session.generationIsAuthoritative = true;
    }
  }

  private compactSessions(activeSessionIds?: ReadonlySet<string>): void {
    if (this.sessions.size <= MAX_TRACKED_HUDDLE_SESSIONS) return;
    for (const [sessionId, session] of this.sessions) {
      if (session.endState) this.sessions.delete(sessionId);
      if (this.sessions.size <= MAX_TRACKED_HUDDLE_SESSIONS) return;
    }
    // Nothing ended is left to drop, yet the map is still over its bound: a
    // START is accepted from any member with any session id, and ENDs can be
    // missed. Evict sessions that are not live (when liveness is known),
    // those with nobody present first, oldest start first.
    const evictable = [...this.sessions]
      .filter(([sessionId]) => !activeSessionIds?.has(sessionId))
      .map(([sessionId, session]) => ({
        sessionId,
        session,
        present: sessionHasPresence(session),
      }))
      .sort(
        (left, right) =>
          Number(left.present) - Number(right.present) ||
          left.session.startCreatedAt - right.session.startCreatedAt ||
          left.session.startEventId.localeCompare(right.session.startEventId),
      );
    for (const { sessionId } of evictable) {
      if (this.sessions.size <= MAX_TRACKED_HUDDLE_SESSIONS) return;
      this.sessions.delete(sessionId);
    }
  }

  /** Number of sessions currently retained (bounded by compaction). */
  get sessionCount(): number {
    return this.sessions.size;
  }

  snapshot(activeSessionIds?: ReadonlySet<string>): ReadonlySet<string> {
    this.compactSessions(activeSessionIds);
    const participants = new Set<string>();
    for (const [sessionId, session] of this.sessions) {
      if (activeSessionIds && !activeSessionIds.has(sessionId)) continue;
      if (session.endState) continue;
      for (const participant of sessionParticipants(session)) {
        participants.add(participant);
      }
    }
    return participants;
  }
}

/** Apply a complete hydration result in lifecycle order to an existing tracker. */
export function applyHuddleLifecycleHistory(
  tracker: HuddlePresenceTracker,
  events: Iterable<RelayEvent>,
): void {
  for (const event of [...events].sort(compareHuddleLifecycleEvents)) {
    tracker.apply(event);
  }
}

/** Reconstruct everyone currently in an authenticated, visible huddle. */
export function reconstructHuddlePresence(
  events: Iterable<RelayEvent>,
  relaySelfPubkey: string | null | undefined,
): ReadonlySet<string> {
  const tracker = new HuddlePresenceTracker(relaySelfPubkey);
  applyHuddleLifecycleHistory(tracker, events);
  return tracker.snapshot();
}

/** Parent channels probed for one session whose parent the relay never named. */
export const MAX_UNATTESTED_HUDDLE_PARENTS = 4;
/** Sessions a parent routing table retains before it evicts the oldest. */
export const MAX_ROUTED_HUDDLE_SESSIONS = 4_096;

type HuddleSessionRoute = { attested: boolean; parents: string[] };

/**
 * Which parent channel(s) to query for each huddle session's relay liveness.
 *
 * A session's parent channel is its lifecycle events' `h` tag. A relay-signed
 * JOIN, LEFT or END names it authoritatively. A START is only its author's
 * claim, so it is used until the relay has spoken, and a later START can add a
 * candidate parent but never replace one: a forged START naming a live session
 * under the wrong channel then costs one extra liveness query instead of
 * redirecting the only one and hiding a live huddle. Only channels the viewer
 * subscribes to are accepted.
 */
export class HuddleSessionParentRouting {
  private readonly relaySelf: string;
  private readonly allowedChannelIds: ReadonlySet<string> | null;
  private readonly routes = new Map<string, HuddleSessionRoute>();

  constructor(
    relaySelfPubkey: string | null | undefined,
    allowedChannelIds?: Iterable<string>,
  ) {
    this.relaySelf = normalizePubkey(relaySelfPubkey ?? "");
    this.allowedChannelIds = allowedChannelIds
      ? new Set(allowedChannelIds)
      : null;
  }

  observe(event: RelayEvent): void {
    const sessionId = huddleSessionId(event);
    const parentChannelId = huddleParentChannelId(event);
    if (!sessionId || !parentChannelId) return;
    if (
      this.allowedChannelIds &&
      !this.allowedChannelIds.has(parentChannelId)
    ) {
      return;
    }
    const relaySigned =
      this.relaySelf !== "" && normalizePubkey(event.pubkey) === this.relaySelf;
    if (relaySigned) {
      this.routes.delete(sessionId);
      this.routes.set(sessionId, {
        attested: true,
        parents: [parentChannelId],
      });
      this.evictOverflow();
      return;
    }
    if (event.kind !== KIND_HUDDLE_STARTED) return;
    const route = this.routes.get(sessionId);
    if (!route) {
      this.routes.set(sessionId, {
        attested: false,
        parents: [parentChannelId],
      });
      this.evictOverflow();
      return;
    }
    if (route.attested || route.parents.includes(parentChannelId)) return;
    if (route.parents.length < MAX_UNATTESTED_HUDDLE_PARENTS) {
      route.parents.push(parentChannelId);
    }
  }

  parentsOf(sessionId: string): readonly string[] {
    return this.routes.get(sessionId)?.parents ?? [];
  }

  sessionIds(): string[] {
    return [...this.routes.keys()];
  }

  clear(): void {
    this.routes.clear();
  }

  private evictOverflow(): void {
    if (this.routes.size <= MAX_ROUTED_HUDDLE_SESSIONS) return;
    for (const [sessionId, route] of this.routes) {
      if (route.attested) continue;
      this.routes.delete(sessionId);
      if (this.routes.size <= MAX_ROUTED_HUDDLE_SESSIONS) return;
    }
    for (const sessionId of this.routes.keys()) {
      this.routes.delete(sessionId);
      if (this.routes.size <= MAX_ROUTED_HUDDLE_SESSIONS) return;
    }
  }
}
