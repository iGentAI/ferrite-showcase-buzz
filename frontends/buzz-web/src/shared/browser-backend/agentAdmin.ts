/**
 * Administering agents and community members from a browser tab.
 *
 * | command | how |
 * | --- | --- |
 * | `set_persona_active` | a pause row the dispatcher reads |
 * | `get_agent_memory` | the relay's memory route |
 * | `get_agent_config_surface` | the relay definition IS the config |
 * | `put_agent_session_config` | same |
 * | `add`/`remove`/`change_relay_member_role` | three routes, authority split |
 *
 * ## The membership authority split
 *
 * A MODERATOR may add or remove a plain member. Only an OPERATOR may grant or
 * revoke moderator status — because the moderator set is what authorizes every
 * moderation verb, so `changeRole(pk, "admin")` in a moderator's hands would be
 * a way to mint more moderators. Splitting it is what makes the add/remove verbs
 * safe to offer at all; the relay enforces both halves.
 *
 * ## What a "session config" is here
 *
 * Nothing, and the surface says so. A cloud agent has no local harness process
 * to hold per-session state: its definition on the relay is its configuration,
 * and a write applies to its next turn. Reporting a session-override capability
 * this deployment does not have would put a control on screen that silently did
 * nothing.
 */
import type { RuntimeConfigSurface } from "@/shared/api/types";
import { apiGet, apiPost } from "@/shared/lib/api";

/** The client's `RawPersona`, as `set_persona_active` must answer with. */
type RawPersona = {
  id: string;
  display_name: string;
  avatar_url: string | null;
  system_prompt: string;
  runtime: string | null;
  model: string | null;
  provider: string | null;
  name_pool: string[];
  is_builtin: boolean;
  is_active: boolean;
  source_team: string | null;
  env_vars: Record<string, string>;
  respond_to: string | null;
  respond_to_allowlist: string[];
  parallelism: number | null;
  created_at: string;
  updated_at: string;
};

/** The harness id the relay reports for its own in-workspace pipeline. */
const CLOUD_RUNTIME = "ferrite-cloud";

/** The relay stores no created/updated timestamps for a persona, so these carry
 * the epoch rather than `now`, which would make every read look like an edit. */
const UNKNOWN_TIME = new Date(0).toISOString();

/**
 * `set_persona_active` — pause or resume a registered agent.
 *
 * Pausing is NOT retirement: the agent keeps its identity, its definition and
 * its history, and stops being woken by a mention. That distinction is why this
 * is a separate verb from `delete_persona` and why the relay stores it as a row
 * of its own rather than as a registry field.
 */
export async function setPersonaActive(args: {
  id?: string;
  active?: boolean;
}): Promise<RawPersona> {
  const agent = args.id ?? "";
  if (!agent) throw new Error("set_persona_active needs an agent id.");
  await apiPost<{ ok: boolean }>("/agents/active", {
    agent,
    active: args.active !== false,
  });
  // Read the agent back through the config route rather than echoing the
  // request: the caller renders what comes back, and reporting the requested
  // state instead of the stored one would hide a write that did not land.
  const config = await apiPost<{
    agent: string;
    instructions: string | null;
    model: string | null;
    active: boolean;
    harness: string;
  }>("/agents/config", { agent });
  return {
    id: config.agent,
    display_name: config.agent,
    avatar_url: null,
    system_prompt: config.instructions ?? "",
    runtime: config.harness || CLOUD_RUNTIME,
    model: config.model,
    provider: null,
    name_pool: [],
    is_builtin: false,
    is_active: config.active,
    source_team: null,
    env_vars: {},
    respond_to: null,
    respond_to_allowlist: [],
    parallelism: null,
    created_at: UNKNOWN_TIME,
    updated_at: UNKNOWN_TIME,
  };
}

/**
 * `get_agent_memory` — what an agent remembers.
 *
 * The desktop reads NIP-AE engrams: encrypted events decrypted with the owner's
 * conversation key. A cloud agent's memory is not that — it lives on the relay's
 * own memory plane, written by the turn pipeline — so this reads the route and
 * shapes it into the same listing.
 *
 * `core` is separated because the UI roots its reachability tree there;
 * `outgoingRefs` is parsed from the body's `[[slug]]` links, which is what the
 * desktop does with the decrypted payload and is the only part of the shape
 * that is derived rather than stored.
 */
export async function getAgentMemory(args: {
  agentPubkey?: string;
  agent?: string;
}): Promise<{
  core: EngramEntry | null;
  memories: EngramEntry[];
  truncated: boolean;
  fetchedAt: number;
}> {
  // The caller passes a PUBKEY (that is what the desktop's engram reader takes),
  // so it is resolved to the agent id the relay keys memory by.
  const agent = await requireAgentId(args.agent, args.agentPubkey);
  const reply = await apiPost<{
    entries?: Array<{ id: string; value: unknown }>;
    complete?: boolean;
  }>("/agents/memory", { agent });

  const entries = (reply.entries ?? []).map((row) => toEngram(row.id, row.value));
  return {
    core: entries.find((entry) => entry.slug === "core") ?? null,
    memories: entries.filter((entry) => entry.slug !== "core"),
    // The relay reports whether the scan was exhausted; `truncated` is its
    // negation, and reporting it honestly is what lets the UI say "there is
    // more" instead of drawing a partial graph as if it were whole.
    truncated: reply.complete === false,
    fetchedAt: Math.floor(Date.now() / 1000),
  };
}

/** One memory entry in the shape the memory graph renders. */
type EngramEntry = {
  slug: string;
  body: string;
  eventId: string;
  createdAt: number;
  outgoingRefs: string[];
};

function toEngram(id: string, value: unknown): EngramEntry {
  const body =
    typeof value === "string" ? value : JSON.stringify(value ?? null, null, 2);
  return {
    slug: id,
    body,
    // A memory row is a store record, not an event, so there is no event id to
    // report. The row's own key is what identifies it here.
    eventId: id,
    createdAt: 0,
    outgoingRefs: [...body.matchAll(/\[\[([^\]]+)\]\]/g)].map((match) => match[1]),
  };
}

/** The agent id the relay knows a pubkey by, or `null`. */
async function agentIdFor(pubkey: string): Promise<string | null> {
  const wanted = pubkey.toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(wanted)) return null;
  const reply = await apiGet<{ agents?: Array<{ agent: string; pubkey: string }> }>(
    "/agents",
  );
  return (
    (reply.agents ?? []).find((row) => row.pubkey.toLowerCase() === wanted)?.agent ??
    null
  );
}

async function requireAgentId(
  agent: string | undefined,
  pubkey: string | undefined,
): Promise<string> {
  const resolved = agent ?? (await agentIdFor(pubkey ?? ""));
  if (!resolved) {
    throw new Error("That pubkey is not a registered agent on this workspace.");
  }
  return resolved;
}

/**
 * `get_agent_config_surface` — what can be configured, and what cannot.
 *
 * Returns a valid `RuntimeConfigSurface` where every field's `writeVia` is
 * `readOnly` and `isPreSpawn` is `false`: a cloud agent has no per-conversation
 * local process to override, so configuration lives on its relay definition.
 */
export async function getAgentConfigSurface(args: {
  pubkey?: string;
  agent?: string;
}): Promise<RuntimeConfigSurface> {
  const agent = await requireAgentId(args.agent, args.pubkey);
  const config = await apiPost<{
    instructions: string | null;
    model: string | null;
    harness: string;
  }>("/agents/config", { agent });
  const runtimeId = config.harness || CLOUD_RUNTIME;
  return {
    runtimeId,
    runtimeLabel: runtimeId,
    isPreSpawn: false,
    normalized: {
      model: config.model
        ? {
            value: config.model,
            origin: "buzzExplicit",
            writeVia: { type: "readOnly" },
            overriddenValue: null,
            overriddenOrigin: null,
            isRequired: false,
          }
        : null,
      provider: null,
      mode: null,
      thinkingEffort: null,
      maxOutputTokens: null,
      contextLimit: null,
      systemPrompt: config.instructions
        ? {
            value: config.instructions,
            origin: "buzzExplicit",
            writeVia: { type: "readOnly" },
            overriddenValue: null,
            overriddenOrigin: null,
            isRequired: false,
          }
        : null,
    },
    advanced: [],
    extensions: [],
    sources: {
      acpNative: "notApplicable",
      acpConfigOptions: "notApplicable",
      envVars: "notApplicable",
      configFile: "notApplicable",
      configFilePath: null,
      mcpConfigFilePath: null,
    },
  };
}

/**
 * `put_agent_session_config` — write the agent's configuration.
 *
 * Named "session" by the desktop because there it configures one harness
 * process. Here it writes the DEFINITION, which the relay validates with the
 * agent runtime's own parser, so a malformed write is refused at the boundary
 * rather than at the agent's next turn.
 *
 * An ABSENT property carries the stored value forward; a property present as
 * `null` CLEARS it. Collapsing those two — which `?? current` does — makes
 * clearing impossible, because the clear is indistinguishable from not
 * mentioning the field.
 */
/** Attempts at a versioned config write before a conflict is reported. */
const CONFIG_WRITE_ATTEMPTS = 3;

export async function putAgentSessionConfig(
  args: {
    pubkey?: string;
    agent?: string;
    payload?: { model?: string | null; instructions?: string | null };
  },
  post: <T>(path: string, body: unknown) => Promise<T> = apiPost,
): Promise<null> {
  const agent = await requireAgentId(args.agent, args.pubkey);
  // The write carries the generation it was built from, so an edit that
  // lands between the read and the write is refused (409) rather than
  // overwritten with the stale value carried forward; the write is then
  // rebuilt from a fresh read.
  for (let attempt = 1; ; attempt += 1) {
    try {
      await writeAgentConfigOnce(agent, args.payload ?? {}, post);
      return null;
    } catch (error) {
      const status = (error as { status?: unknown }).status;
      if (status !== 409 || attempt >= CONFIG_WRITE_ATTEMPTS) throw error;
    }
  }
}

async function writeAgentConfigOnce(
  agent: string,
  payload: { model?: string | null; instructions?: string | null },
  post: <T>(path: string, body: unknown) => Promise<T>,
): Promise<void> {
  const current = await post<{
    instructions: string | null;
    model: string | null;
    generation?: number | null;
  }>("/agents/config", { agent });

  const named = (field: "model" | "instructions") =>
    Object.prototype.hasOwnProperty.call(payload, field);
  // A definition is written WHOLE, so a field this call does not NAME is carried
  // over rather than dropped — otherwise changing a model would erase the
  // agent's instructions. A field named as null is genuinely cleared.
  const model = named("model") ? payload.model : current.model;
  const instructions = named("instructions")
    ? payload.instructions
    : current.instructions;

  const definition: Record<string, unknown> = { version: 1, agent_id: agent };
  // Nullish, not truthy: an explicitly supplied empty string is a value the
  // relay's validator should judge, not something this client reinterprets as a
  // clear. Only `null` clears.
  if (model !== null && model !== undefined) definition.default_model = model;
  if (instructions !== null && instructions !== undefined) {
    definition.instructions = instructions;
  }
  await post<{ ok: boolean }>("/agents/config", {
    agent,
    set: definition,
    ...(typeof current.generation === "number"
      ? { if_generation: current.generation }
      : {}),
  });
}

/**
 * `add_relay_member` — admit somebody to the community.
 *
 * A `role` of `admin` is TWO acts on this relay: the membership row, and the
 * operator-only moderator set. Both are attempted, and a caller who is a
 * moderator rather than an operator will be refused the second with a message
 * saying so — which is the correct outcome, because a moderator minting
 * moderators is the escalation the split exists to prevent.
 *
 * The relay has no atomic add-with-role route, so the two acts can half-apply:
 * the member row lands and the role is refused. That outcome is reported as
 * exactly that ("added as a member, but moderator status was refused"), never
 * as a bare failure, because the membership change DID persist and an operator
 * reading a bare error could not tell. `post` is injectable for node tests.
 */
export async function addRelayMember(
  args: {
    targetPubkey?: string;
    role?: string;
  },
  post: typeof apiPost = apiPost,
): Promise<null> {
  const pubkey = requirePubkey(args.targetPubkey, "add_relay_member");
  const wanted = args.role ?? "member";
  if (wanted !== "member" && wanted !== "admin" && wanted !== "owner") {
    throw new Error(
      `"${wanted}" is not a role this community has. Use "admin" or "member".`,
    );
  }
  await post<{ ok: boolean }>("/members/add", { pubkey });
  if (wanted === "admin" || wanted === "owner") {
    try {
      await post<{ ok: boolean }>("/members/role", { pubkey, role: "admin" });
    } catch (error) {
      const reason = error instanceof Error ? error.message : String(error);
      throw new Error(
        `Added ${pubkey.slice(0, 8)}… as a member, but moderator status was refused: ${reason}`,
        { cause: error },
      );
    }
  }
  return null;
}

/** `remove_relay_member`. */
export async function removeRelayMember(args: {
  targetPubkey?: string;
}): Promise<null> {
  const pubkey = requirePubkey(args.targetPubkey, "remove_relay_member");
  await apiPost<{ ok: boolean }>("/members/remove", { pubkey });
  return null;
}

/**
 * `change_relay_member_role` — grant or revoke moderator status.
 *
 * Operator-only on the relay. `owner` maps to `admin` rather than being refused:
 * this relay has no owner concept, and community moderator is the closest thing
 * that actually EXISTS — but it is not invented as a third role, because a role
 * nothing enforces would draw controls that do nothing.
 *
 * Anything else is refused by name. Mapping an unrecognised string to `member`
 * would mean a typo silently revokes somebody's moderator status, which is a
 * destructive answer to a request nobody made.
 */
export async function changeRelayMemberRole(args: {
  targetPubkey?: string;
  newRole?: string;
}): Promise<null> {
  const pubkey = requirePubkey(args.targetPubkey, "change_relay_member_role");
  const wanted = args.newRole ?? "";
  const role =
    wanted === "admin" || wanted === "owner"
      ? "admin"
      : wanted === "member"
        ? "member"
        : null;
  if (!role) {
    throw new Error(
      `"${wanted}" is not a role this community has. Use "admin" or "member".`,
    );
  }
  await apiPost<{ ok: boolean }>("/members/role", { pubkey, role });
  return null;
}

function requirePubkey(value: string | undefined, command: string): string {
  const pubkey = (value ?? "").toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(pubkey)) {
    throw new Error(`${command} needs a 32-byte hex pubkey.`);
  }
  return pubkey;
}
