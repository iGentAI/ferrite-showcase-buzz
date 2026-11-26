/**
 * The persona (kind 30175) and team (kind 30178) CATALOGS, read from the relay.
 *
 * Upstream answers `fetch_persona_catalog` / `fetch_team_catalog` from its native
 * relay session (`desktop/src-tauri/src/persona_catalog.rs`, `team_catalog.rs`):
 * page the kind, Schnorr-verify, NIP-33 head-select per `(owner, d)`, keep only
 * heads carrying exactly `["shared","true"]`, parse the content. This module is
 * the same decision, minus the signature step: the relay verified every event
 * at ingest, and it is the only store a tab reads from (the same trust the
 * other `relayEvents` reads in this seam already place in it).
 *
 * Head selection happens BEFORE the shared check and the parse, exactly as
 * upstream: a newer unshared or malformed head is still the head, and must not
 * resurrect an older shared definition. That ordering is the reason the pure
 * functions below exist separately from the fetch — `catalogs.test.mjs` pins it.
 */

import type { RelayEvent } from "@/shared/api/types";
import { readAllPages } from "@/shared/browser-backend/channelDirectory";
import { relayEvents } from "@/shared/browser-backend/relayConnection";

export const KIND_PERSONA = 30175;
export const KIND_TEAM_CATALOG = 30178;

/** Upstream's paging is 500 per REQ, which is also the relay's `MAX_PAGE_EVENTS`. */
const CATALOG_PAGE_LIMIT = 500;

export type CatalogAgentProjection = {
  displayName: string;
  avatarUrl: string | null;
  description: string | null;
  systemPrompt: string;
  runtime: string | null;
  model: string | null;
  provider: string | null;
  namePool: string[];
  respondTo: "owner-only" | "anyone" | null;
  parallelism: number | null;
  /** Upstream #7578: `thread` or, for anything else including absent, `channel`. */
  sessionPolicy: "channel" | "thread";
};

export type PersonaCatalogPublication = {
  eventId: string;
  ownerPubkey: string;
  sourcePersonaId: string;
  createdAt: number;
  agent: CatalogAgentProjection;
};

export type TeamCatalogMemberProjection = {
  memberKey: string;
  displayName: string;
  systemPrompt: string;
  avatarUrl: string | null;
  runtime: string | null;
  model: string | null;
  provider: string | null;
};

export type TeamCatalogPublication = {
  eventId: string;
  ownerPubkey: string;
  teamDTag: string;
  name: string;
  description: string | null;
  instructions: string | null;
  members: TeamCatalogMemberProjection[];
};

// Bounds mirrored from upstream's validators (`managed_agents/team_catalog.rs`
// and the agent definition text policy). A projection past any of them is
// dropped whole rather than truncated — a truncated prompt is a different
// agent, not a shorter one.
const MAX_NAME_BYTES = 256;
const MAX_DESCRIPTION_CHARS = 280;
const MAX_TEXT_BYTES = 4 * 1024;
const MAX_INSTRUCTIONS_BYTES = 16 * 1024;
const MAX_SYSTEM_PROMPT_BYTES = 16 * 1024;
const MAX_MEMBERS = 64;
/** A persona's `name_pool` becomes displayed bot names (`pickBotName`), so
 * each entry is held to the display-name policy and the pool is bounded. */
const MAX_NAME_POOL = 64;
const TEAM_CATALOG_SCHEMA_VERSION = 1;

const utf8 = new TextEncoder();
function bytes(value: string): number {
  return utf8.encode(value).length;
}

/**
 * NIP-33 head selection: newest `created_at` wins, ties broken by the lower id
 * (upstream's comparator). Returns the head per `(owner, d)` in that order.
 */
export function selectHeads(events: RelayEvent[], kind: number): RelayEvent[] {
  const sorted = [...events]
    .filter((event) => event.kind === kind)
    .sort((left, right) =>
      right.created_at !== left.created_at
        ? right.created_at - left.created_at
        : left.id < right.id
          ? -1
          : left.id > right.id
            ? 1
            : 0,
    );
  const claimed = new Set<string>();
  const heads: RelayEvent[] = [];
  for (const event of sorted) {
    const d = coordinateTag(event, "d");
    if (!d) continue;
    const key = `${event.pubkey.toLowerCase()}\u0000${d}`;
    if (claimed.has(key)) continue;
    claimed.add(key);
    heads.push(event);
  }
  return heads;
}

/** The value of `name` when the event carries exactly one such tag (≥ 2 cells). */
function coordinateTag(event: RelayEvent, name: string): string | null {
  const matches = event.tags.filter((tag) => tag.length >= 2 && tag[0] === name);
  return matches.length === 1 ? matches[0][1] : null;
}

/** Upstream's `exact_tag`: exactly one two-cell tag named `name`. */
function exactTag(event: RelayEvent, name: string): string | null {
  const matches = event.tags.filter((tag) => tag.length === 2 && tag[0] === name);
  return matches.length === 1 ? matches[0][1] : null;
}

/** The relay's canonical shared spelling, and nothing else. */
function isSharedHead(event: RelayEvent): boolean {
  return exactTag(event, "shared") === "true";
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function optionalString(value: unknown): string | null {
  // Runtime, model and provider are shown to the adopter too: concealed text
  // in them is refused like everywhere else (finding_e8c5ac1704c5a1c7).
  return typeof value === "string" && value.trim().length > 0 && !hasConcealedText(value, false)
    ? value
    : null;
}

/**
 * Concealed text is refused the way upstream's `validate_visible_text` refuses
 * it: bidi/format controls, C0 and C1 controls (newline and tab allowed when
 * `multiline`) and default-ignorable characters can make what is displayed
 * differ from what was reviewed. That includes plane-14 tag characters, which
 * render as nothing while a model reads them as instructions
 * (finding_e8c5ac1704c5a1c7).
 */
function hasConcealedText(value: string, multiline: boolean): boolean {
  for (const ch of value) {
    const code = ch.codePointAt(0) ?? 0;
    if (code < 0x20) {
      if (multiline && (ch === "\n" || ch === "\t" || ch === "\r")) continue;
      return true;
    }
    if (
      (code >= 0x7f && code <= 0x9f) ||
      code === 0xad ||
      code === 0x34f ||
      code === 0x61c ||
      code === 0x115f ||
      code === 0x1160 ||
      code === 0x17b4 ||
      code === 0x17b5 ||
      (code >= 0x180b && code <= 0x180f) ||
      (code >= 0x200b && code <= 0x200f) ||
      (code >= 0x202a && code <= 0x202e) ||
      (code >= 0x2060 && code <= 0x206f) ||
      code === 0x3164 ||
      (code >= 0xfe00 && code <= 0xfe0f) ||
      code === 0xfeff ||
      code === 0xffa0 ||
      (code >= 0xfff0 && code <= 0xfffb) ||
      (code >= 0x1bca0 && code <= 0x1bca3) ||
      (code >= 0x1d173 && code <= 0x1d17a) ||
      (code >= 0xe0000 && code <= 0xe0fff)
    ) {
      return true;
    }
  }
  return false;
}

function safeAvatar(value: unknown): string | null {
  if (typeof value !== "string" || value.length === 0) return null;
  if (value.startsWith("data:image/svg+xml")) {
    return value.length <= 64 * 1024 ? value : null;
  }
  if (value.startsWith("data:image/")) {
    return value.length <= 512 * 1024 ? value : null;
  }
  return /^https?:\/\//i.test(value) ? value : null;
}

/** Upstream's `parse_agent` for one kind-30175 content string. */
export function parsePersonaCatalogAgent(content: string): CatalogAgentProjection | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(content);
  } catch {
    return null;
  }
  if (!isObject(parsed)) return null;
  const displayName = parsed.display_name;
  if (typeof displayName !== "string") return null;
  const trimmedName = displayName.trim();
  if (
    trimmedName.length === 0 ||
    bytes(displayName) > MAX_NAME_BYTES ||
    hasConcealedText(displayName, false)
  ) {
    return null;
  }
  const systemPrompt =
    typeof parsed.system_prompt === "string" ? parsed.system_prompt : "";
  // The system prompt is exactly what the adoption dialog asks the user to
  // review (`AgentInstructionReview`) before it runs verbatim, so concealed
  // text there would let the reviewed rendering differ from the instructions
  // the agent receives. Upstream's `parse_agent` refuses the entry for it.
  if (
    bytes(systemPrompt) > MAX_SYSTEM_PROMPT_BYTES ||
    hasConcealedText(systemPrompt, true)
  ) {
    return null;
  }

  // A description that fails the policy rejects the WHOLE entry, matching how
  // upstream treats every other definition field at this boundary.
  let description: string | null = null;
  if (parsed.description !== undefined && parsed.description !== null) {
    if (typeof parsed.description !== "string") return null;
    if (
      [...parsed.description].length > MAX_DESCRIPTION_CHARS ||
      hasConcealedText(parsed.description, true)
    ) {
      return null;
    }
    description = parsed.description.trim().length > 0 ? parsed.description : null;
  }

  let respondTo: CatalogAgentProjection["respondTo"] = null;
  switch (parsed.respond_to) {
    case "allowlist":
    case "owner-only":
      respondTo = "owner-only";
      break;
    case "anyone":
      respondTo = "anyone";
      break;
    default:
      respondTo = null;
  }
  const parallelism =
    typeof parsed.parallelism === "number" &&
    Number.isInteger(parsed.parallelism) &&
    parsed.parallelism >= 1 &&
    parsed.parallelism <= 32
      ? parsed.parallelism
      : null;
  const namePool = Array.isArray(parsed.name_pool)
    ? parsed.name_pool.filter((value): value is string => typeof value === "string")
    : [];
  if (namePool.length > MAX_NAME_POOL) return null;
  for (const name of namePool) {
    if (
      name.trim().length === 0 ||
      bytes(name) > MAX_NAME_BYTES ||
      hasConcealedText(name, false)
    ) {
      return null;
    }
  }

  return {
    displayName,
    avatarUrl: safeAvatar(parsed.avatar_url),
    description,
    systemPrompt,
    runtime: optionalString(parsed.runtime),
    model: optionalString(parsed.model),
    provider: optionalString(parsed.provider),
    namePool,
    respondTo,
    parallelism,
    // Upstream's `persona_catalog.rs`: only "thread" selects per-thread
    // context; unknown or absent values degrade to the channel default rather
    // than dropping the entry.
    sessionPolicy: parsed.session_policy === "thread" ? "thread" : "channel",
  };
}

/** Upstream's `publications_from_verified_events` for personas. */
export function personaPublications(events: RelayEvent[]): PersonaCatalogPublication[] {
  const publications: PersonaCatalogPublication[] = [];
  for (const head of selectHeads(events, KIND_PERSONA)) {
    // Claimed above; visibility and parsing decide only whether it is LISTED.
    if (!isSharedHead(head)) continue;
    const d = coordinateTag(head, "d");
    if (!d) continue;
    const agent = parsePersonaCatalogAgent(head.content);
    if (!agent) continue;
    publications.push({
      eventId: head.id,
      ownerPubkey: head.pubkey.toLowerCase(),
      sourcePersonaId: d,
      createdAt: head.created_at,
      agent,
    });
  }
  return publications;
}

/**
 * Upstream's `team_catalog_content_from_event` + `validate_team_catalog_content`:
 * an all-or-nothing parse, because a team with one invalid member cannot be
 * adopted and a partial projection would only offer an un-addable entry.
 */
export function parseTeamCatalogContent(
  content: string,
): Pick<TeamCatalogPublication, "name" | "description" | "instructions" | "members"> | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(content);
  } catch {
    return null;
  }
  if (!isObject(parsed)) return null;
  if (parsed.v !== TEAM_CATALOG_SCHEMA_VERSION) return null;
  const name = parsed.name;
  if (typeof name !== "string" || name.trim().length === 0) return null;
  if (bytes(name) > MAX_NAME_BYTES || hasConcealedText(name, false)) return null;

  let description: string | null = null;
  if (parsed.description !== undefined && parsed.description !== null) {
    if (typeof parsed.description !== "string") return null;
    if (bytes(parsed.description) > MAX_TEXT_BYTES || hasConcealedText(parsed.description, true)) {
      return null;
    }
    description = parsed.description;
  }
  let instructions: string | null = null;
  if (parsed.instructions !== undefined && parsed.instructions !== null) {
    if (typeof parsed.instructions !== "string") return null;
    if (
      bytes(parsed.instructions) > MAX_INSTRUCTIONS_BYTES ||
      hasConcealedText(parsed.instructions, true)
    ) {
      return null;
    }
    instructions = parsed.instructions;
  }
  if (!Array.isArray(parsed.members) || parsed.members.length > MAX_MEMBERS) return null;

  const members: TeamCatalogMemberProjection[] = [];
  const memberKeys = new Set<string>();
  for (const raw of parsed.members) {
    if (!isObject(raw)) return null;
    const memberKey = raw.member_key;
    const displayName = raw.display_name;
    if (typeof memberKey !== "string" || memberKey.length === 0) return null;
    if (memberKeys.has(memberKey)) return null;
    memberKeys.add(memberKey);
    if (typeof displayName !== "string" || displayName.trim().length === 0) return null;
    if (bytes(displayName) > MAX_NAME_BYTES || hasConcealedText(displayName, false)) return null;
    const systemPrompt =
      raw.system_prompt === undefined || raw.system_prompt === null
        ? ""
        : typeof raw.system_prompt === "string"
          ? raw.system_prompt
          : undefined;
    if (
      systemPrompt === undefined ||
      bytes(systemPrompt) > MAX_SYSTEM_PROMPT_BYTES ||
      hasConcealedText(systemPrompt, true)
    ) {
      return null;
    }
    members.push({
      memberKey,
      displayName,
      systemPrompt,
      avatarUrl: safeAvatar(raw.avatar_url),
      runtime: optionalString(raw.runtime),
      model: optionalString(raw.model),
      provider: optionalString(raw.provider),
    });
  }
  return { name, description, instructions, members };
}

/** Upstream's `publications_from_verified_events` for teams. */
export function teamPublications(events: RelayEvent[]): TeamCatalogPublication[] {
  const publications: TeamCatalogPublication[] = [];
  for (const head of selectHeads(events, KIND_TEAM_CATALOG)) {
    if (!isSharedHead(head)) continue;
    const d = coordinateTag(head, "d");
    if (!d) continue;
    const content = parseTeamCatalogContent(head.content);
    if (!content) continue;
    publications.push({
      eventId: head.id,
      ownerPubkey: head.pubkey.toLowerCase(),
      teamDTag: d,
      ...content,
    });
  }
  return publications;
}

/**
 * Every event of `kind` the relay will show this reader, paged newest-first
 * by `until` the way upstream's native session pages. The relay's share gate
 * already withholds other owners' UNSHARED heads (`access::is_unshared_gated`),
 * so a page here is the reader's own heads plus everybody's shared ones — and
 * the head selection above is what keeps an own unshared head from listing.
 *
 * The walk is `readAllPages`: it continues until an EMPTY page, because the
 * relay bounds a page by bytes as well as by count and says nothing when it
 * stopped early. Catalog events are the fat kind (16 KiB prompts, avatars up to
 * 512 KiB as data URLs), so a page well short of the limit is routine and not
 * the end of the data; treating it as the end hid every older shared head.
 * `fetchPage` is injectable so the walk can be driven in node.
 */
export async function pageKind(
  kind: number,
  fetchPage: (filter: {
    kinds: number[];
    limit: number;
    until?: number;
  }) => Promise<RelayEvent[]> = relayEvents,
): Promise<RelayEvent[]> {
  return readAllPages({ kinds: [kind], limit: CATALOG_PAGE_LIMIT }, fetchPage, 20);
}

/** `fetch_persona_catalog`. */
export async function personaCatalogFromRelay(): Promise<PersonaCatalogPublication[]> {
  return personaPublications(await pageKind(KIND_PERSONA));
}

/** `fetch_team_catalog`. */
export async function teamCatalogFromRelay(): Promise<TeamCatalogPublication[]> {
  return teamPublications(await pageKind(KIND_TEAM_CATALOG));
}
