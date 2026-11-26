/**
 * CLOUD agents from a browser tab.
 *
 * buzz's desktop app creates an agent by starting a local harness PROCESS and
 * supervising it, which a tab genuinely cannot do. For a CLOUD agent that
 * refusal would be wrong: the workspace itself runs the persona turn
 * pipeline (the relay's `agent-turns` queue and its audited provider egress),
 * so "create an agent that runs on Ferrite" is a relay WRITE, not a desktop
 * capability. Nothing local is started, and nothing local has to stay running
 * for the agent to answer — close the tab and it still replies.
 *
 * This module is the client of that relay surface:
 *
 * | command | route |
 * | --- | --- |
 * | `list_personas` | `GET /buzz/api/agents` → persona rows |
 * | `list_relay_agents` | `GET /buzz/api/agents` → RELAY-AGENT rows |
 * | `create_persona` / `update_persona` | `POST /buzz/api/agents` |
 * | `delete_persona` | `POST /buzz/api/agents/remove` |
 * | `get_agent_models` / `discover_agent_models` | `GET /buzz/api/agent-models` |
 *
 * The two list commands read the SAME route and return DIFFERENT shapes, which
 * is not an accident to tidy away: `list_personas` answers "what agent
 * definitions exist" (`RawPersona`, keyed by id, carrying instructions and
 * model), while `list_relay_agents` answers "who is present in this workspace"
 * (`RawRelayAgent`, keyed by pubkey, carrying name/status/channels). Returning
 * a persona where a relay agent is expected leaves `name` undefined and crashes
 * the welcome screen on `name.toLowerCase()` — the shapes are load-bearing.
 *
 * The relay owns every decision that matters: who may create an agent, the
 * agent's signing key (derived server-side, never sent over the wire), which
 * models are routable, and the ownership rule that stops one member rewriting
 * another's teammate. This file only translates shapes.
 */
import { apiGet, apiPost } from "@/shared/lib/api";
import { relayEvents } from "@/shared/browser-backend/relayConnection";
import type { RawPersona } from "@/shared/api/tauriPersonas";
import type { AgentModelsResponse } from "@/shared/api/types";

/** The harness id the relay reports for its own in-workspace pipeline. */
export const CLOUD_RUNTIME = "ferrite-cloud";

/** One row of `GET /buzz/api/agents`. */
type RelayAgent = {
  agent: string;
  pubkey: string;
  harness: string;
  instructions: string | null;
  model: string | null;
};

/** One row of `GET /buzz/api/agent-models`. */
type RelayModel = {
  id: string;
  provider: string;
  context_window: number;
  max_output_tokens: number;
};

type EncodedCloudAgentSnapshot = {
  fileBytes: number[];
  fileName: string;
};

/**
 * The relay stores no created/updated timestamps for an agent — a persona is a
 * registry entry, not an audited record — so these carry the epoch rather than
 * `now`, which would change on every read and make the UI look like the agent
 * had just been edited.
 */
const UNKNOWN_TIME = new Date(0).toISOString();

function toRawPersona(row: RelayAgent): RawPersona {
  return {
    id: row.agent,
    display_name: row.agent,
    avatar_url: null,
    system_prompt: row.instructions ?? "",
    runtime: row.harness || CLOUD_RUNTIME,
    model: row.model,
    provider: null,
    name_pool: [],
    is_builtin: false,
    is_active: true,
    source_team: null,
    env_vars: {},
    respond_to: null,
    respond_to_allowlist: [],
    parallelism: null,
    // A relay-held agent keeps ONE durable turn thread per channel
    // (`buzz-{channel}`, `agents/turns.rs::thread_for_channel`), which is
    // upstream's `channel` policy. Answered explicitly so the dialog shows the
    // context this workspace actually applies.
    session_policy: "channel",
    created_at: UNKNOWN_TIME,
    updated_at: UNKNOWN_TIME,
  };
}

/** Every cloud agent registered on this workspace. */
export async function listCloudAgents(): Promise<RawPersona[]> {
  const reply = await apiGet<{ agents?: RelayAgent[] }>("/agents");
  return (reply.agents ?? []).map(toRawPersona);
}

function snapshotFileStem(value: string): string {
  const stem = value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 80);
  return stem || "agent";
}

/**
 * Package one relay-held cloud definition for sharing from a browser tab.
 *
 * The desktop encoder can include local files and memories and can render the
 * manifest into a PNG. A hosted agent has neither: its complete definition is
 * already on the relay, so the portable v1 JSON form is the honest browser
 * snapshot. Memory is deliberately fixed to `none` because no browser API can
 * export it.
 */
export function buildCloudAgentSnapshot(
  persona: RawPersona,
): EncodedCloudAgentSnapshot {
  const snapshot = {
    format: "buzz-agent-snapshot",
    version: 1,
    definition: {
      name: persona.display_name,
      sourceIsBuiltIn: persona.is_builtin,
      systemPrompt: persona.system_prompt,
      runtime: persona.runtime ?? null,
      model: persona.model ?? null,
      provider: persona.provider ?? null,
      respondTo: persona.respond_to ?? null,
      respondToAllowlist: persona.respond_to_allowlist ?? [],
      namePool: persona.name_pool ?? [],
    },
    profile: {
      displayName: persona.display_name,
      avatarUrl: persona.avatar_url,
    },
    memory: {
      level: "none",
      entries: [],
    },
  };

  return {
    fileBytes: Array.from(new TextEncoder().encode(JSON.stringify(snapshot))),
    fileName: `${snapshotFileStem(persona.display_name)}.agent.json`,
  };
}

/** Encode one hosted agent definition without relying on desktop IPC. */
export async function encodeCloudAgentSnapshot(
  personaId: string,
): Promise<EncodedCloudAgentSnapshot> {
  const persona = (await listCloudAgents()).find(
    (candidate) => candidate.id === personaId,
  );
  if (!persona) {
    throw new Error("That agent no longer exists in this workspace.");
  }
  return buildCloudAgentSnapshot(persona);
}

/**
 * The same agents as workspace PARTICIPANTS — the shape `list_relay_agents`
 * expects (`RawRelayAgent`), which is keyed by pubkey and describes presence
 * rather than configuration.
 *
 * `status` is `online` because a cloud agent has no process to be up or down:
 * it is reachable whenever it is registered, since the workspace itself runs
 * its turns. `channels` stays an empty ARRAY (names are not needed by any
 * consumer, and a missing field would break the ones that map over it).
 *
 * `channel_ids` and `respond_to` are what the synced client decides agent
 * eligibility on (`features/agents/lib/agentAutocompleteEligibility.ts`): a
 * relay agent is offered to a reader only where it shares a room with them,
 * and mentioned only in a room it is in. Both are answered from what the relay
 * enforces, see `relayAgentRows`.
 */
export async function listRelayAgents(): Promise<RawRelayAgentRow[]> {
  const reply = await apiGet<{ agents?: RelayAgent[] }>("/agents");
  const agents = reply.agents ?? [];
  const pubkeys = agents
    .map((row) => row.pubkey)
    .filter((pubkey) => /^[0-9a-f]{64}$/i.test(pubkey));
  let rosters: RosterEvent[] = [];
  if (pubkeys.length > 0) {
    try {
      rosters = await readRoomRosters(
        (filter) => relayEvents(filter as never) as Promise<RosterEvent[]>,
      );
    } catch {
      // Fail closed: no roster evidence means no claimed membership, so the
      // client offers and admits these agents nowhere rather than everywhere.
      rosters = [];
    }
  }
  return relayAgentRows(agents, rosters);
}

/**
 * Every room roster the reader may see, walked page by page.
 *
 * NOT filtered by `#p`. A `#p` filter returns only roster versions that still
 * list the persona, so the version that REMOVED it never arrived, the "latest
 * per room" reduction kept the older version, and a removed persona stayed
 * offered in that room's mention autocomplete until send-time revalidation
 * refused it with "Could not authorize a mentioned agent". Walking pages
 * (`readAllPages`) also lifts the one-page, 500-event ceiling.
 */
export async function readRoomRosters(
  fetchPage: (filter: { kinds: number[]; limit: number; until?: number }) => Promise<RosterEvent[]>,
): Promise<RosterEvent[]> {
  const { readAllPages } = await import("@/shared/browser-backend/channelDirectory");
  return readAllPages({ kinds: [KIND_GROUP_MEMBERS], limit: 500 }, fetchPage);
}

/** NIP-29 group members projection (`39002`): `d` names the room, `p` tags the members. */
const KIND_GROUP_MEMBERS = 39002;

/** The part of a 39002 roster event `relayAgentRows` reads. */
export type RosterEvent = { id: string; kind?: number; created_at: number; tags: string[][] };

/** One `RawRelayAgent` row, as `listRelayAgents` answers it. */
export type RawRelayAgentRow = {
  pubkey: string;
  name: string;
  agent_type: string;
  channels: string[];
  channel_ids: string[];
  capabilities: string[];
  status: "online" | "away" | "offline";
  respond_to: "anyone";
  respond_to_allowlist: string[];
};

/**
 * Registry rows plus room rosters → the relay-agent rows the client reasons on.
 *
 * - `respond_to` is `anyone`: the relay dispatches a persona for any stored
 *   channel message that p-tags it and that the persona could read
 *   (`apps/buzz/src/relay/agents.rs`); there is no per-reader policy.
 * - `channel_ids` are the rooms whose latest 39002 roster lists the persona,
 *   which is the membership the relay gates the persona's own post on.
 *
 * Answering the registry verbatim (`respond_to: null`, `channel_ids: []`)
 * makes the synced client refuse every workspace persona: it is never offered
 * in a room's add-member search and every @mention of it fails with "Could not
 * authorize a mentioned agent", even in a room it belongs to.
 */
export function relayAgentRows(
  agents: readonly RelayAgent[],
  rosters: readonly RosterEvent[],
): RawRelayAgentRow[] {
  const latestByRoom = new Map<string, RosterEvent>();
  for (const roster of rosters) {
    if (roster.kind !== undefined && roster.kind !== KIND_GROUP_MEMBERS) continue;
    const room = roster.tags.find((tag) => tag[0] === "d")?.[1];
    if (!room) continue;
    const kept = latestByRoom.get(room);
    if (!kept || (roster.created_at ?? 0) > (kept.created_at ?? 0)) {
      latestByRoom.set(room, roster);
    }
  }
  const roomsByMember = new Map<string, string[]>();
  for (const [room, roster] of latestByRoom) {
    for (const tag of roster.tags) {
      if (tag[0] !== "p" || !tag[1]) continue;
      const member = tag[1].toLowerCase();
      const rooms = roomsByMember.get(member) ?? [];
      if (!rooms.includes(room)) rooms.push(room);
      roomsByMember.set(member, rooms);
    }
  }
  return agents.map((row) => ({
    pubkey: row.pubkey,
    name: row.agent,
    agent_type: row.harness || CLOUD_RUNTIME,
    channels: [],
    channel_ids: [...(roomsByMember.get(row.pubkey.toLowerCase()) ?? [])],
    capabilities: [],
    status: "online" as const,
    respond_to: "anyone" as const,
    respond_to_allowlist: [],
  }));
}

/**
 * The answer to `revalidate_relay_agents`: the send path's last authorization
 * read before an @mention of a relay agent is published
 * (`features/messages/lib/agentMentionRevalidation.ts`), narrowed to the asked
 * pubkeys.
 *
 * The asked room is decided by that room's OWN roster, read at send time, not
 * by the directory scan: a persona added a moment ago is admitted, and one
 * removed a moment ago is refused, whatever the scan saw.
 */
export function relayAgentsForMentionRevalidation(
  rows: readonly RawRelayAgentRow[],
  wantedPubkeys: Iterable<string>,
  channelId: string | undefined,
  rosterPubkeys: Iterable<string>,
): RawRelayAgentRow[] {
  const wanted = new Set([...wantedPubkeys].map((pubkey) => pubkey.toLowerCase()));
  const roster = new Set([...rosterPubkeys].map((pubkey) => pubkey.toLowerCase()));
  return rows
    .filter((row) => wanted.has(row.pubkey.toLowerCase()))
    .map((row) => {
      if (!channelId) return { ...row };
      const others = row.channel_ids.filter((id) => id !== channelId);
      return {
        ...row,
        channel_ids: roster.has(row.pubkey.toLowerCase())
          ? [...others, channelId]
          : others,
      };
    });
}

/**
 * Create (or, for its owner, reconfigure) a cloud agent. The relay derives the
 * agent id from the display name when no explicit id is given, so "Ada the
 * Helper" becomes `ada-the-helper` server-side rather than in two clients that
 * could slugify differently.
 */
export async function createCloudAgent(input: {
  displayName?: string | null;
  agent?: string | null;
  systemPrompt?: string | null;
  runtime?: string | null;
  model?: string | null;
}): Promise<RawPersona> {
  const created = await apiPost<{
    agent: string;
    display_name?: string | null;
    pubkey: string;
    harness: string;
    model: string | null;
    instructions: string | null;
  }>("/agents", {
    agent: input.agent ?? undefined,
    display_name: input.displayName ?? undefined,
    instructions: input.systemPrompt ?? undefined,
    harness: input.runtime ?? undefined,
    model: input.model ?? undefined,
  });
  return {
    ...toRawPersona({
      agent: created.agent,
      pubkey: created.pubkey,
      harness: created.harness,
      instructions: created.instructions ?? null,
      model: created.model,
    }),
    display_name: created.display_name || created.agent,
  };
}

/**
 * The refusal for a definition asking for per-thread conversation context.
 *
 * Upstream #7578 lets a desktop ACP definition choose `thread`, which spawns a
 * separate local ACP conversation per channel thread. A relay-held agent turns
 * in one durable thread per channel, so `thread` cannot be honoured here.
 * Saving would otherwise succeed and silently read back as `channel`; refusing
 * keeps the user's draft open with the reason instead.
 */
function cloudSessionPolicyRefusal(payload: {
  behavior?: unknown;
}): string | null {
  const behavior = payload.behavior as { sessionPolicy?: unknown } | undefined;
  if (behavior?.sessionPolicy !== "thread") return null;
  return (
    "Each-thread conversation context is not available for agents hosted by this " +
    "workspace: they keep one conversation per channel. Choose \"Entire channel\" " +
    "under Advanced to save."
  );
}

/**
 * The parts of a persona edit the relay's agent registry does not store.
 *
 * `POST /buzz/api/agents` keeps an agent's name, instructions, harness and
 * model, nothing else, and reads everything else back as a default
 * (`toRawPersona`). A "Save and publish" that reported these as published
 * would confirm edits that were never sent, so they are named instead.
 * Only fields the caller actually filled count; an empty value asks for
 * nothing.
 */
export function personaFieldsNotStored(payload: Record<string, unknown>): string[] {
  const filled = (value: unknown) =>
    typeof value === "string" && value.trim() !== "";
  const unstored: string[] = [];
  if (filled(payload.avatarUrl)) unstored.push("avatar");
  if (filled(payload.description)) unstored.push("description");
  if (filled(payload.provider)) unstored.push("provider");
  if (Array.isArray(payload.namePool) && payload.namePool.length > 0) {
    unstored.push("name pool");
  }
  if (
    payload.envVars &&
    typeof payload.envVars === "object" &&
    Object.keys(payload.envVars).length > 0
  ) {
    unstored.push("environment variables");
  }
  const behavior = payload.behavior as
    | {
        respondTo?: unknown;
        respondToAllowlist?: unknown;
        parallelism?: unknown;
      }
    | undefined;
  if (behavior && typeof behavior === "object") {
    if (
      behavior.respondTo != null ||
      (Array.isArray(behavior.respondToAllowlist) &&
        behavior.respondToAllowlist.length > 0)
    ) {
      unstored.push("response policy");
    }
    if (behavior.parallelism != null) unstored.push("parallelism");
  }
  return unstored;
}

/**
 * Save a persona definition as a cloud agent: the ONE write behind
 * `create_persona`, `update_persona` and `update_persona_and_publish`.
 *
 * Kept in one place so the publish path cannot skip the session-policy refusal
 * or drop `runtime`, which would make "Save and publish" silently store a
 * different agent than "Save". `create` is injectable so the rule can be driven in node without a relay.
 */
export async function saveCloudPersona(
  payload: Record<string, unknown>,
  create: typeof createCloudAgent = createCloudAgent,
): Promise<RawPersona> {
  const refusal = cloudSessionPolicyRefusal(payload);
  if (refusal) throw new Error(refusal);
  return create({
    agent: (payload.id as string | undefined) ?? undefined,
    displayName: (payload.displayName as string | undefined) ?? undefined,
    systemPrompt: (payload.systemPrompt as string | undefined) ?? undefined,
    runtime: (payload.runtime as string | undefined) ?? undefined,
    model: (payload.model as string | undefined) ?? undefined,
  });
}

/** Retire a cloud agent (its past messages stay, like a departed human's). */
export async function removeCloudAgent(agent: string): Promise<void> {
  await apiPost<{ ok: boolean }>("/agents/remove", { agent });
}

/**
 * The models this deployment can actually route to, shaped as the model picker
 * expects. `supportsSwitching` is true because the model is a property of the
 * agent's definition here, not of a running process: changing it is a write,
 * not a restart.
 */
export async function cloudAgentModels(): Promise<AgentModelsResponse> {
  const reply = await apiGet<{
    default_model?: string;
    harness?: string;
    models?: RelayModel[];
  }>("/agent-models");
  const models = reply.models ?? [];
  return {
    agentName: reply.harness || CLOUD_RUNTIME,
    agentVersion: "",
    models: models.map((model) => ({
      id: model.id,
      name: model.id,
      description: `${model.provider} · ${Math.round(model.context_window / 1000)}k context`,
    })),
    agentDefaultModel: reply.default_model ?? null,
    selectedModel: null,
    supportsSwitching: models.length > 0,
  };
}
