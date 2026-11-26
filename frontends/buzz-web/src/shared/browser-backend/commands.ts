/**
 * buzz's Rust commands, browser edition — the core the chat path needs.
 *
 * The desktop app keeps its identity, signing and relay config in Rust and
 * reaches them with `invoke("<command>")`. There is no Rust here, so this
 * module answers the commands that must work for the client to connect,
 * authenticate and publish:
 *
 *   get_relay_ws_url / get_relay_http_url / get_default_relay_url
 *   auto_connect_default_relay_enabled
 *   create_auth_event   (NIP-42, kind 22242)
 *   sign_event          (any event the UI publishes)
 *   get_public_key / is_shared_identity
 *   nip44_encrypt_to_self / nip44_decrypt_from_self
 *
 * …plus the DATA commands the desktop serves out of a relay-fed Rust cache.
 * A tab has no cache, so those read the relay directly — the channel list and
 * roster from the relay's own NIP-29 projections, profiles from kind 0, the
 * home feed and presence from the relay's HTTP routes (see "Relay-backed
 * reads" below). Nothing here invents a value the relay does not hold.
 *
 * Everything else in buzz's ~140-command surface is a desktop capability
 * (native media pickers, git, ACP harnesses, managed agent processes) and
 * either answers with the documented EMPTY shape — "nothing here" is a truth,
 * and a rejection would render as a red error instead — or stays unavailable,
 * rejecting once, loudly, with the sentence {@link unavailableCommandMessage}
 * writes for that family so the feature renders its own unavailable state.
 *
 * Signing uses the SAME identity the rest of this port uses
 * (`browser-backend/identity`): a NIP-07 extension when the browser has one,
 * otherwise this tab's stored key. The key never leaves the tab.
 */

import { finalizeEvent, getPublicKey } from "nostr-tools/pure";
import { nip44 } from "nostr-tools";

// The one key this tab signs with — shared with the NIP-98 header signer, so
// the socket and the HTTP routes are the same person.
import {
  bytesToHex,
  clearLocalIdentity,
  hexToBytes,
  importLocalIdentity,
  localSecretKey,
} from "@/shared/browser-backend/identity";
import {
  publishSigned,
  relayEvents,
} from "@/shared/browser-backend/relayConnection";

// The commands that CHANGE a room: membership, archive, deletion. They are all
// relay verbs, so they live next door rather than in this dispatch table.
import {
  addChannelMembersViaRelay,
  changeChannelMemberRoleViaRelay,
  deleteChannelViaRelay,
  deleteMessageViaRelay,
  removeChannelMemberViaRelay,
  setChannelArchivedViaRelay,
} from "@/shared/browser-backend/channelAdmin";
// The channel canvas — a room's shared document, one kind:40100 event.
import {
  getCanvasFromRelay,
  setCanvasViaRelay,
} from "@/shared/browser-backend/canvas";
// Sending into a thread (replies, media, custom emoji, forum posts) and
// reading a thread's subtree back — the message-send relay verbs.
// `isRelayEventId` is the shared "can this id exist on the relay at all"
// test that keeps `get_event` from asking the relay for a client-local
// placeholder.
import {
  isRelayEventId,
  sendChannelMessageViaRelay,
  threadRepliesFromRelay,
} from "@/shared/browser-backend/messageSend";
import { searchProfiles } from "@/shared/browser-backend/profileSearch";
import {
  mergeProfileUpdate,
  type ProfileUpdate,
} from "@/shared/browser-backend/profileUpdate";

import { windowBoundsKey } from "@/shared/api/channelWindowKey";
// `crypto.subtle` / `crypto.randomUUID` are secure-context-only; a relay
// served over plain http still needs its NIP-98 nonces and payload digests.
import { randomUuid, sha256Bytes, sha256Hex } from "@/shared/lib/insecureContextCrypto";
import { CHANNEL_TIMELINE_CONTENT_KINDS } from "@/shared/constants/kinds";

// Type-only, so these are erased at build time and add no import cycle.
import type { RelaySubscriptionFilter } from "@/shared/api/relayClientShared";
import type { RelayEvent } from "@/shared/api/types";
import { BUZZ_AUTHORIZATION_HEADER } from "@/shared/lib/nip98";

type Nip07 = {
  getPublicKey(): Promise<string>;
  signEvent(e: {
    kind: number;
    created_at: number;
    tags: string[][];
    content: string;
  }): Promise<Record<string, unknown>>;
  /** NIP-07's optional NIP-44 surface; the extension key does the crypto. */
  nip44?: {
    encrypt(pubkey: string, plaintext: string): Promise<string>;
    decrypt(pubkey: string, ciphertext: string): Promise<string>;
  };
};

function extension(): Nip07 | undefined {
  return (window as unknown as { nostr?: Nip07 }).nostr;
}

async function publicKeyHex(): Promise<string> {
  const ext = extension();
  if (ext) return ext.getPublicKey();
  return getPublicKey(localSecretKey());
}

async function sign(input: {
  kind: number;
  content: string;
  tags: string[][];
  created_at?: number;
}): Promise<Record<string, unknown>> {
  const unsigned = {
    kind: input.kind,
    created_at: input.created_at ?? Math.floor(Date.now() / 1000),
    tags: input.tags ?? [],
    content: input.content ?? "",
  };
  const ext = extension();
  if (ext) return ext.signEvent(unsigned);
  return finalizeEvent(unsigned, localSecretKey()) as unknown as Record<
    string,
    unknown
  >;
}

/**
 * Who the channel-admin commands act AS.
 */
const relayIdentity = { sign, pubkey: publicKeyHex };

/**
 * The relay this build talks to: an explicit override, else same origin.
 *
 * The path is `/buzz/`, not `/buzz/ws/`, because buzz derives the relay's HTTP
 * base from this URL by swapping the scheme only (`relayHttpFromWs`) and then
 * appending `/api/...` — so the socket's path IS the API's parent. Putting the
 * socket one segment deeper sends invite and join-policy calls to
 * `/buzz/ws/api/...`, which no route serves.
 */
function relayWsUrl(): string {
  const configured = import.meta.env.VITE_RELAY_WS_URL;
  if (configured) return configured;
  const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${window.location.host}/buzz/`;
}

function relayHttpUrl(): string {
  const ws = relayWsUrl();
  return `${ws.replace(/^ws/, "http").replace(/\/+$/, "")}/api`;
}

/** The relay's HTTP base and the page a relative media URL resolves against,
 * for the download commands' relay-media gate (`workspace.ts`). */
function relayLocation(): { httpUrl: string; pageUrl: string } {
  return { httpUrl: relayHttpUrl(), pageUrl: window.location.href };
}

/**
 * `set_prevent_sleep_active` natively takes a power-management assertion so a
 * long agent run is not cut off by sleep. The browser equivalent is the Screen
 * Wake Lock API, which is weaker but real: it keeps the screen (and with it the
 * tab) awake while the page is visible.
 *
 * Typed locally rather than through lib.dom so the build does not depend on the
 * TypeScript release that added these interfaces.
 */
type WakeLockSentinelLike = { released: boolean; release(): Promise<void> };
type WakeLockLike = { request(type: "screen"): Promise<WakeLockSentinelLike> };

function wakeLockApi(): WakeLockLike | undefined {
  return (navigator as Navigator & { wakeLock?: WakeLockLike }).wakeLock;
}

let wakeLockWanted = false;
let wakeLockSentinel: WakeLockSentinelLike | null = null;
let wakeLockVisibilityHooked = false;

async function acquireWakeLock(): Promise<void> {
  const api = wakeLockApi();
  if (!api || wakeLockSentinel) return;
  try {
    wakeLockSentinel = await api.request("screen");
  } catch {
    // Denied (background tab, or the user's power settings) — best effort only.
    wakeLockSentinel = null;
  }
}

/**
 * The browser drops a screen wake lock whenever the tab is hidden, so re-take
 * it on the way back. Without this, one tab switch silently ends the lock for
 * the rest of the session.
 */
function hookWakeLockVisibility(): void {
  if (wakeLockVisibilityHooked) return;
  wakeLockVisibilityHooked = true;
  document.addEventListener("visibilitychange", () => {
    if (!wakeLockWanted) return;
    if (document.visibilityState === "visible" && !wakeLockSentinel) {
      void acquireWakeLock();
    }
  });
}

async function setPreventSleep(active: boolean): Promise<null> {
  wakeLockWanted = active;
  if (active) {
    hookWakeLockVisibility();
    await acquireWakeLock();
    return null;
  }
  const held = wakeLockSentinel;
  wakeLockSentinel = null;
  if (held && !held.released) {
    await held.release().catch(() => {});
  }
  return null;
}

/** The relay clamps a REQ `limit` to 500 (`query::MAX_PAGE_EVENTS`). */
const RELAY_PAGE_LIMIT = 500;

/** Longest author list this build puts in one filter, so a REQ frame stays small. */
const AUTHORS_PER_FILTER = 100;

const KIND_PROFILE = 0;
const KIND_GROUP_METADATA = 39000;
const KIND_GROUP_MEMBERS = 39002;

/** Kinds buzz files under the feed's `agent_activity` bucket (agent job traffic). */
const AGENT_ACTIVITY_KINDS = new Set([43001, 43003, 43004]);

/**
 * Upstream's `GET_EVENT_KINDS` (`commands/messages/event_batch.rs`): what a
 * batch id lookup may resolve to. 48100 is `KIND_HUDDLE_STARTED`.
 */
const GET_EVENT_KINDS = [
  0, 1, 3, 5, 7, 9, 30078, 40002, 40003, 40008, 40099, 40100, 45001, 45003, 48100,
] as const;

/**
 * Upstream's `CHANNEL_REPAIR_KINDS` (`commands/channel_reconnect_repair.rs`):
 * everything a live channel subscription would have carried while the socket
 * was down.
 */
const CHANNEL_REPAIR_KINDS = [
  5, 7, 9, 9005, 40001, 40002, 40003, 40008, 40099, 45001, 45003, 48100, 48101, 48102, 48103,
] as const;

const HTTP_TIMEOUT_MS = 15_000;

function firstTag(event: RelayEvent, name: string): string | null {
  return event.tags.find((tag) => tag[0] === name)?.[1] ?? null;
}

function hasTag(event: RelayEvent, name: string): boolean {
  return event.tags.some((tag) => tag[0] === name);
}

function tagValues(event: RelayEvent, name: string): string[] {
  return event.tags
    .filter((tag) => tag[0] === name && tag[1])
    .map((tag) => tag[1]);
}

/**
 * Newest event per `d` tag. 39xxx is parameterized-replaceable, so the relay
 * already retires old versions — but a live socket can hold both across a
 * replacement, and the newest one is the current room.
 */
function latestByDTag(events: RelayEvent[]): Map<string, RelayEvent> {
  const latest = new Map<string, RelayEvent>();
  for (const event of events) {
    const id = firstTag(event, "d");
    if (!id) continue;
    const seen = latest.get(id);
    if (!seen || event.created_at > seen.created_at) latest.set(id, event);
  }
  return latest;
}

/** Newest event per author — the shape every replaceable per-user kind wants. */
function latestByAuthor(events: RelayEvent[]): Map<string, RelayEvent> {
  const latest = new Map<string, RelayEvent>();
  for (const event of events) {
    const author = event.pubkey.toLowerCase();
    const seen = latest.get(author);
    if (!seen || event.created_at > seen.created_at) latest.set(author, event);
  }
  return latest;
}

function chunked<T>(items: T[], size: number): T[][] {
  const out: T[][] = [];
  for (let i = 0; i < items.length; i += size) {
    out.push(items.slice(i, i + size));
  }
  return out;
}

/**
 * POST one of the relay's own HTTP routes as THIS tab's identity, at the URL
 * derived from the socket's (`relayHttpUrl`). The `nonce` tag matters: the relay
 * burns an auth event id after a single use, so two requests to the same path in
 * the same second would otherwise be identical events and the second a "replay".
 */
async function relayApiPost<T>(path: string, body: unknown): Promise<T> {
  const url = `${relayHttpUrl()}${path}`;
  const payload = JSON.stringify(body ?? {});
  const auth = await sign({
    kind: 27235,
    content: "",
    tags: [
      ["u", url],
      ["method", "POST"],
      ["payload", await sha256Hex(new TextEncoder().encode(payload))],
      ["nonce", randomUuid()],
    ],
  });
  const response = await fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      [BUZZ_AUTHORIZATION_HEADER]: `Nostr ${btoa(JSON.stringify(auth))}`,
    },
    body: payload,
    signal: AbortSignal.timeout(HTTP_TIMEOUT_MS),
  });
  const parsed = await readJson(response);
  if (!response.ok) {
    throw new Error(relayErrorMessage(parsed, response.status));
  }
  return parsed as T;
}

/**
 * GET one of the relay's own HTTP routes as THIS tab's identity: routes like
 * `/me` answer ABOUT whoever signed the request, and `shared/lib/api.ts`'s
 * `apiGet` signs nothing.
 */
async function relayApiGet<T>(path: string): Promise<T> {
  const url = `${relayHttpUrl()}${path}`;
  const auth = await sign({
    kind: 27235,
    content: "",
    tags: [
      ["u", url],
      ["method", "GET"],
      // The relay burns an authorization event id on use, so two reads of the
      // same path in one second would otherwise be the same event and the
      // second a replay.
      ["nonce", randomUuid()],
    ],
  });
  const response = await fetch(url, {
    headers: {
      Accept: "application/json",
      [BUZZ_AUTHORIZATION_HEADER]: `Nostr ${btoa(JSON.stringify(auth))}`,
    },
    signal: AbortSignal.timeout(HTTP_TIMEOUT_MS),
  });
  const parsed = await readJson(response);
  if (!response.ok) {
    throw new Error(relayErrorMessage(parsed, response.status));
  }
  return parsed as T;
}

async function readJson(response: Response): Promise<unknown> {
  const text = await response.text();
  if (!text) return {};
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return {};
  }
}

/** The relay answers `{ ok: false, error }`; anything else is just its status. */
function relayErrorMessage(parsed: unknown, status: number): string {
  if (parsed && typeof parsed === "object") {
    const { error } = parsed as { error?: unknown };
    if (typeof error === "string" && error) return error;
  }
  return `relay returned ${status}`;
}

/**
 * `get_channels` — buzz's channel list, built the way buzz's own client builds
 * it: from the relay-signed NIP-29 projections.
 *
 * Kind **39000** is one room's metadata (`d`/`h` = channel id, plus `name`,
 * `about`, `topic`, `purpose`, `t` = channel type, `public`/`private`,
 * `archived`, `ttl`), kind **39002** is that room's roster (`p` tags), and the
 * join across the two decides `is_member` for this tab's pubkey. The relay only
 * DELIVERS a projection for a room the caller may read, so the visible set is
 * decided by the relay's access gate, not filtered here.
 *
 * Two fields the projection genuinely does not carry are `null`, not guessed:
 * `last_message_at` (the sidebar then sorts by name instead of recency) and
 * `ttl_deadline` (the relay publishes `ttl` seconds and deliberately no civil
 * date). DM rooms are absent for the same honest reason: the relay does not
 * project them yet, so `open_dm` conversations only show up once it does.
 */
type RawChannelRow = {
  id: string;
  name: string;
  channel_type: "stream" | "forum" | "dm";
  visibility: "open" | "private";
  description: string;
  topic: string | null;
  purpose: string | null;
  member_count: number;
  member_pubkeys: string[];
  last_message_at: string | null;
  archived_at: string | null;
  participants: string[];
  participant_pubkeys: string[];
  is_member: boolean;
  ttl_seconds: number | null;
  ttl_deadline: string | null;
};

/**
 * DMs this browser has hidden.
 *
 * Per-BROWSER, because the relay has no concept of a hidden conversation and
 * inventing a kind to carry one would be inventing protocol for a preference.
 * The desktop keeps it in its local database, which has exactly this scope.
 */
const HIDDEN_DMS_KEY = "buzz-web:hidden-dms";

function hiddenDms(): Set<string> {
  try {
    const raw = window.localStorage.getItem(HIDDEN_DMS_KEY);
    const parsed = raw ? (JSON.parse(raw) as unknown) : [];
    return new Set(
      Array.isArray(parsed)
        ? parsed.filter((id): id is string => typeof id === "string")
        : [],
    );
  } catch {
    return new Set();
  }
}

/**
 * `hide_dm` — take one conversation out of the sidebar.
 *
 * The write is only half of it: `channelsFromRelay` FILTERS on this set, because
 * storing the id and reporting success while the conversation stayed visible
 * would be a button that does nothing.
 */
function hideDm(channelId: string): null {
  if (!channelId) throw new Error("hide_dm needs a channelId.");
  const hidden = hiddenDms();
  hidden.add(channelId);
  window.localStorage.setItem(HIDDEN_DMS_KEY, JSON.stringify([...hidden]));
  return null;
}

/**
 * Every room the relay will give this identity — UNFILTERED.
 *
 * Separate from [`channelsFromRelay`] because hiding a DM is a sidebar
 * preference, not a deletion: a hidden conversation must still resolve by id for
 * a direct route or a detail read. Filtering inside the shared projection would
 * make `get_channel_details` answer "no channel" for a room the relay is serving.
 */
async function channelProjection(): Promise<RawChannelRow[]> {
  return channelProjectionFrom(relayEvents, await publicKeyHex());
}

/**
 * [`channelProjection`] over an injected event reader, for unit tests.
 *
 * Rooms this tab created that the relay projection does not serve yet are
 * carried HERE, in the one resolver every by-id path shares (`get_channels`,
 * `get_channel_details`, the settings read-back, the channel browser). Carrying
 * them only in the sidebar list let the sidebar show a new room while opening
 * it answered "No channel <id> on this relay" during the projection race.
 */
export async function channelProjectionFrom(
  readEvents: (filter: RelaySubscriptionFilter) => Promise<RelayEvent[]>,
  myPubkey: string,
): Promise<RawChannelRow[]> {
  const me = myPubkey.toLowerCase();
  // Walked, not read once: the relay's page is byte-bounded and newest-first,
  // so a single read drops old rooms such as `general` (see `channelDirectory.ts`).
  const { readAllPages } = await import(
    "@/shared/browser-backend/channelDirectory"
  );
  const { withRecentlyCreated } = await import(
    "@/shared/browser-backend/channels"
  );
  const [metadataEvents, memberEvents] = await Promise.all([
    readAllPages(
      { kinds: [KIND_GROUP_METADATA], limit: RELAY_PAGE_LIMIT },
      (filter) => readEvents(filter as unknown as RelaySubscriptionFilter),
    ),
    readAllPages(
      { kinds: [KIND_GROUP_MEMBERS], limit: RELAY_PAGE_LIMIT },
      (filter) => readEvents(filter as unknown as RelaySubscriptionFilter),
    ),
  ]);

  const rosters = latestByDTag(memberEvents);
  const served = [...latestByDTag(metadataEvents)].map(([id, metadata]): RawChannelRow => {
    const roster = rosters.get(id);
    // NIP-29 members are `["p", pubkey, relay, role]`.
    const memberPubkeys = roster
      ? [...new Set(tagValues(roster, "p").map((pk) => pk.toLowerCase()))]
      : [];
    const ttl = Number.parseInt(firstTag(metadata, "ttl") ?? "", 10);
    return {
      id,
      name: firstTag(metadata, "name") ?? id,
      channel_type: channelTypeFromTag(firstTag(metadata, "t")),
      visibility: hasTag(metadata, "private") ? "private" : "open",
      description: firstTag(metadata, "about") ?? "",
      topic: firstTag(metadata, "topic"),
      purpose: firstTag(metadata, "purpose"),
      member_count: memberPubkeys.length,
      member_pubkeys: memberPubkeys,
      last_message_at: null,
      // The projection says "archived", never when; this version's timestamp is
      // the one fact we do have about it, and the UI only tests for presence.
      archived_at:
        firstTag(metadata, "archived") === "true"
          ? new Date(metadata.created_at * 1_000).toISOString()
          : null,
      // `participants` is the parallel array of DISPLAY names, which a roster
      // event has none of — empty keeps the label resolver on the profile
      // lookup instead of showing a raw pubkey as if it were a name.
      participants: [],
      participant_pubkeys: memberPubkeys,
      is_member: memberPubkeys.includes(me),
      ttl_seconds: Number.isFinite(ttl) ? ttl : null,
      ttl_deadline: null,
    };
  });
  return withRecentlyCreated(served) as RawChannelRow[];
}

function channelTypeFromTag(value: string | null): "stream" | "forum" | "dm" {
  return value === "forum" || value === "dm" ? value : "stream";
}

/**
 * The channel list the SIDEBAR renders: the projection, minus the DMs this
 * browser has hidden.
 *
 * The preference is applied here and nowhere else. A hidden conversation is
 * still on the relay, still readable, and still reachable by id through
 * `get_channel_details` — it is only absent from the list, which is what hiding
 * one means.
 */
async function channelsFromRelay(): Promise<RawChannelRow[]> {
  const hidden = hiddenDms();
  const rows = await channelProjection();
  if (hidden.size === 0) return rows;
  return rows.filter(
    (row) => !(row.channel_type === "dm" && hidden.has(row.id)),
  );
}

/** The NIP-29 role vocabulary; anything the projection doesn't say is "member". */
function memberRoleFromTag(
  value: string | undefined,
): "owner" | "admin" | "member" | "guest" | "bot" {
  return value === "owner" ||
    value === "admin" ||
    value === "guest" ||
    value === "bot"
    ? value
    : "member";
}

/**
 * `get_channel_members` — one room's roster, from the relay's own 39002
 * projection (the same event `get_channels` derives `is_member` from).
 *
 * The desktop answers this from its local membership DB; a tab's only roster
 * source is the projection, whose `p` tags are NIP-29 shaped:
 * `["p", pubkey, relay_url, role]`.
 *
 * Two honest gaps: `joined_at` is the projection VERSION's timestamp (the
 * roster carries no per-member join times), and `display_name` comes from a
 * best-effort kind-0 batch, so a member who never published a profile stays
 * null and the caller's profile lookup takes over, same as everywhere else.
 */
async function channelMembersFromRelay(input: {
  channelId?: unknown;
}): Promise<{
  members: Array<{
    pubkey: string;
    role: "owner" | "admin" | "member" | "guest" | "bot";
    is_agent: boolean;
    joined_at: string;
    display_name: string | null;
  }>;
  next_cursor: string | null;
}> {
  const channelId =
    typeof input.channelId === "string" ? input.channelId : "";
  if (!channelId) throw new Error("get_channel_members needs a channelId.");

  const events = await relayEvents({
    kinds: [KIND_GROUP_MEMBERS],
    "#d": [channelId],
    limit: RELAY_PAGE_LIMIT,
  } as unknown as RelaySubscriptionFilter);
  const roster = latestByDTag(events).get(channelId);
  if (!roster) return { members: [], next_cursor: null };

  const seen = new Set<string>();
  const rows: Array<{
    pubkey: string;
    role: "owner" | "admin" | "member" | "guest" | "bot";
  }> = [];
  for (const tag of roster.tags) {
    if (tag[0] !== "p" || !tag[1]) continue;
    const pubkey = tag[1].toLowerCase();
    if (seen.has(pubkey)) continue;
    seen.add(pubkey);
    rows.push({ pubkey, role: memberRoleFromTag(tag[3]) });
  }

  // Names are decoration on the roster: a failed profile read must not turn a
  // readable membership list into an error.
  const { profiles } = await usersBatchFromRelay(
    rows.map((row) => row.pubkey),
  ).catch(() => ({ profiles: {} as Record<string, RawUserSummaryRow> }));
  const joinedAt = new Date(roster.created_at * 1_000).toISOString();
  return {
    members: rows.map((row) => {
      const profile = profiles[row.pubkey];
      return {
        pubkey: row.pubkey,
        role: row.role,
        is_agent: row.role === "bot" || profile?.is_agent === true,
        joined_at: joinedAt,
        display_name: profile?.display_name ?? profile?.name ?? null,
      };
    }),
    next_cursor: null,
  };
}

/** NIP-29 self join-request. The relay admits it on an open room, refuses on a private one. */
const KIND_NIP29_JOIN_REQUEST = 9021;
/** NIP-29 self leave-request. */
const KIND_NIP29_LEAVE_REQUEST = 9022;

/**
 * Join (or leave) a room by publishing the NIP-29 command for it.
 *
 * Membership is not a relay RPC — it is an event, and the relay's roster
 * projection (39002) is recomputed from it, which is what turns `is_member`
 * true on the next read.
 */
async function membershipCommand(kind: number, channelId: string): Promise<void> {
  const signed = await sign({ kind, content: "", tags: [["h", channelId]] });
  await publishSigned(
    signed,
    kind === KIND_NIP29_JOIN_REQUEST
      ? "Could not join that channel."
      : "Could not leave that channel.",
  );
}

/**
 * `get_global_agent_config` / `set_global_agent_config`, stored per browser.
 *
 * Upstream keeps this in a file next to the desktop install — per machine, not
 * synced to the relay and not shared between people. `localStorage` has the
 * same scope in a tab, so it is the honest equivalent. Left unanswered, the
 * agent-defaults screens can never persist a harness choice and onboarding's
 * "Next" stays disabled forever, because it waits for the save to land.
 *
 * `restarted_count` is zero because there are no local agent processes to
 * restart: the harness is Ferrite Cloud, which reads its config per request.
 */
const AGENT_CONFIG_STORAGE_KEY = "buzz-web:global-agent-config";

type GlobalAgentConfigRow = {
  env_vars: Record<string, string>;
  provider: string | null;
  model: string | null;
  preferred_runtime: string | null;
};

const EMPTY_AGENT_CONFIG: GlobalAgentConfigRow = {
  env_vars: {},
  provider: null,
  model: null,
  preferred_runtime: null,
};

function readAgentConfig(): GlobalAgentConfigRow {
  try {
    const raw = window.localStorage.getItem(AGENT_CONFIG_STORAGE_KEY);
    if (!raw) return EMPTY_AGENT_CONFIG;
    const parsed = JSON.parse(raw) as Partial<GlobalAgentConfigRow>;
    return {
      env_vars:
        parsed.env_vars && typeof parsed.env_vars === "object"
          ? parsed.env_vars
          : {},
      provider: parsed.provider ?? null,
      model: parsed.model ?? null,
      preferred_runtime: parsed.preferred_runtime ?? null,
    };
  } catch {
    return EMPTY_AGENT_CONFIG;
  }
}

function writeAgentConfig(input: unknown): {
  config: GlobalAgentConfigRow;
  restarted_count: number;
  failed_restart_count: number;
} {
  const incoming = (input ?? {}) as Partial<GlobalAgentConfigRow>;
  // Strip-on-write, as the Rust side does: an empty value means "inherit", and
  // storing it would shadow the inherited one with an empty string.
  const env_vars: Record<string, string> = {};
  for (const [key, value] of Object.entries(incoming.env_vars ?? {})) {
    if (typeof value === "string" && value.length > 0) env_vars[key] = value;
  }
  const config: GlobalAgentConfigRow = {
    env_vars,
    provider: incoming.provider ?? null,
    model: incoming.model ?? null,
    preferred_runtime: incoming.preferred_runtime ?? null,
  };
  window.localStorage.setItem(AGENT_CONFIG_STORAGE_KEY, JSON.stringify(config));
  return { config, restarted_count: 0, failed_restart_count: 0 };
}

/**
 * buzz's "Set up your community" screen is written against Builderlab, buzz's
 * hosted service: sign in to an account, see the communities you own, enter
 * one. A Ferrite deployment has no such service — one deployment IS one
 * community, and the person's Nostr key is the only account there is.
 *
 * So these three answers describe that truthfully instead of pretending to be
 * Builderlab: the account is this tab's identity, it is already linked, and it
 * owns exactly one community — the relay this build talks to. Creating another
 * is refused with a reason, because it means deploying another relay.
 */
async function hostedAccount(): Promise<{ name: string; expiresAt: string }> {
  const profile = await ownProfileFromRelay();
  const { pubkeyToNpub } = await import("@/shared/lib/nostrUtils");
  return {
    name: profile.display_name ?? pubkeyToNpub(profile.pubkey),
    // The session is the tab; there is no server-side expiry to report, and an
    // expiry in the past would make the UI ask for a sign-in that cannot happen.
    expiresAt: new Date(Date.now() + 365 * 24 * 60 * 60 * 1000).toISOString(),
  };
}

async function hostedIdentity(): Promise<{
  identity: { npub: string; pubkey_hex: string };
}> {
  const pubkey = await publicKeyHex();
  const { pubkeyToNpub } = await import("@/shared/lib/nostrUtils");
  return { identity: { npub: pubkeyToNpub(pubkey), pubkey_hex: pubkey } };
}

async function hostedCommunities(): Promise<{
  communities: Array<{
    id: string;
    name: string;
    slug: string;
    normalized_host: string;
    owner_pubkey: string;
    archived_at: null;
  }>;
}> {
  const pubkey = await publicKeyHex();
  const url = relayWsUrl();
  const { host } = new URL(url);
  return {
    communities: [
      {
        id: host,
        name: host,
        slug: host,
        // The FULL ws URL, scheme included: `hostedCommunityRelayUrl` keeps a
        // scheme it is given, which is what lets a relay served over plain
        // http (local, or through a tunnel) be entered from this screen.
        normalized_host: url,
        owner_pubkey: pubkey,
        archived_at: null,
      },
    ],
  };
}

/**
 * The agent harnesses this workspace can reach, as the RELAY declares them.
 *
 * Upstream buzz answers `discover_acp_providers` by probing `PATH` for CLI
 * harnesses. That is the right question for a laptop and the wrong one for a
 * hosted workspace: whether this deployment has Ferrite Cloud behind it, or a
 * Fabric ingress, or an A2A agent card, is a fact about the DEPLOYMENT, and
 * only the relay knows it. So the catalog is `GET /buzz/api/agent-harnesses`
 * (see `ingress::agent_harnesses`), and adding a backend is an operator write
 * rather than a client release.
 *
 * The CLI harnesses are still listed, and still honest: they are local
 * processes, so a browser tab reports them `not_installed` with the reason
 * rather than hiding them. On the desktop build this whole function is unused
 * — the Rust backend answers, and it can really probe.
 */
type HarnessRow = {
  id: string;
  label: string;
  availability: string;
  auth_status?: { status: string; diagnostic?: string };
  install_hint?: string;
};

/** The CLI-based harnesses buzz supports, which no browser tab can launch. */
const DESKTOP_ONLY_HARNESSES: ReadonlyArray<{ id: string; label: string }> = [
  { id: "claude", label: "Claude Code" },
  { id: "codex", label: "Codex" },
  { id: "goose", label: "Goose" },
];

function catalogEntry(row: HarnessRow, hint: string) {
  return {
    id: row.id,
    label: row.label,
    avatar_url: "",
    availability: row.availability,
    // A remote harness has no local binary, adapter or install step; saying
    // otherwise puts an "Install the CLI" button on a thing with no CLI.
    command: null,
    binary_path: null,
    default_args: [],
    mcp_command: null,
    model_env_var: null,
    provider_env_var: null,
    thinking_env_var: null,
    install_hint: row.install_hint ?? hint,
    install_instructions_url: "",
    can_auto_install: false,
    requires_external_cli: false,
    underlying_cli_path: null,
    node_required: false,
    auth_status: row.auth_status ?? { status: "not_applicable" },
    login_hint: undefined,
    source: "builtin" as const,
  };
}

async function harnessCatalog(): Promise<unknown[]> {
  let declared: HarnessRow[] = [];
  try {
    const { apiGet } = await import("@/shared/lib/api");
    const reply = await apiGet<{ harnesses?: HarnessRow[] }>("/agent-harnesses");
    declared = Array.isArray(reply.harnesses) ? reply.harnesses : [];
  } catch {
    // An older relay has no such route. Report no remote harnesses rather than
    // claiming one: the step then says so instead of offering a dead runtime.
    declared = [];
  }
  const seen = new Set(declared.map((row) => row.id));
  return [
    ...declared.map((row) => catalogEntry(row, "")),
    ...DESKTOP_ONLY_HARNESSES.filter((cli) => !seen.has(cli.id)).map((cli) =>
      catalogEntry(
        {
          id: cli.id,
          label: cli.label,
          availability: "not_installed",
          auth_status: { status: "not_applicable" },
        },
        "This harness is a command-line process the buzz desktop app starts; a browser tab cannot launch it.",
      ),
    ),
  ];
}

const KIND_THREAD_SUMMARY = 39005;
const KIND_WINDOW_BOUNDS = 39006;
const KIND_REACTION = 7;
const KIND_DELETION = 5;
const KIND_NIP29_DELETE = 9005;
const KIND_MESSAGE_EDIT = 40003;

/**
 * `get_channel_window` — one page of a channel's timeline.
 *
 * This is the whole of channel HISTORY: the live subscription buzz opens on a
 * channel carries `since: now`, so without this the room renders empty for
 * anyone who did not watch it happen.
 *
 * The desktop backend answers from its own cache and returns a FLAT event list
 * — content rows, a `39005` per row that has replies, and exactly one `39006`
 * carrying the page bounds. The relay's `/window` route returns the same facts
 * in a structured envelope, so this rebuilds that flat wire from it. The two
 * aux events are page metadata, not relay events: they are the shape the
 * parser reads, and they carry no field the relay did not just report.
 *
 * The cursor is opaque on both sides and is passed straight through — the
 * relay's is a hex index key, the client's is `{created_at, id}`, so the key
 * rides in `id` and the client hands it back untouched.
 */
async function channelWindowFromRelay(input: {
  channelId?: unknown;
  limitRows?: unknown;
  cursor?: unknown;
}): Promise<RelayEvent[]> {
  const channelId = typeof input.channelId === "string" ? input.channelId : "";
  if (!channelId) throw new Error("get_channel_window needs a channelId.");
  const limit =
    typeof input.limitRows === "number" && input.limitRows > 0
      ? Math.min(Math.floor(input.limitRows), 200)
      : 50;
  const cursor = (input.cursor ?? null) as {
    created_at?: number;
    event_id?: string;
  } | null;

  const { apiPost } = await import("@/shared/lib/api");
  const reply = await apiPost<{
    events?: Array<{
      event: RelayEvent;
      thread?: {
        reply_count: number;
        descendant_count: number;
        last_reply_at: number | null;
      } | null;
    }>;
    bounds?: { has_more?: boolean; next_cursor?: string | null };
  }>("/window", {
    channel: channelId,
    limit,
    ...(cursor?.event_id ? { cursor: cursor.event_id } : {}),
  });

  const rows = Array.isArray(reply.events) ? reply.events : [];
  const out: RelayEvent[] = [];
  for (const row of rows) {
    if (!row?.event) continue;
    out.push(row.event);
    if (!row.thread) continue;
    out.push({
      id: `summary:${row.event.id}`,
      pubkey: "",
      created_at: row.event.created_at,
      kind: KIND_THREAD_SUMMARY,
      tags: [["e", row.event.id]],
      content: JSON.stringify({
        reply_count: row.thread.reply_count,
        descendant_count: row.thread.descendant_count,
        last_reply_at: row.thread.last_reply_at,
        // The window route reports counts, not who replied; the timeline reads
        // participants only to draw avatars, and drawing none beats drawing
        // the wrong people.
        participants: [],
      }),
      sig: "",
    } as unknown as RelayEvent);
  }

  // Auxiliary overlays for this page: reactions, edits and deletions, which
  // reference their target by `e` and so are NOT in the channel's timeline
  // index. The desktop backend serves them from its cache alongside the rows;
  // here they are one extra REQ, and without it a reaction is stored on the
  // relay and never drawn.
  const rowIds = rows.map((row) => row.event.id).filter(Boolean);
  if (rowIds.length > 0) {
    const overlays = await relayEvents({
      kinds: [KIND_REACTION, KIND_DELETION, KIND_NIP29_DELETE, KIND_MESSAGE_EDIT],
      "#e": rowIds,
      limit: RELAY_PAGE_LIMIT,
    } as unknown as RelaySubscriptionFilter).catch(() => [] as RelayEvent[]);
    out.push(...overlays);
  }

  const nextCursor = reply.bounds?.next_cursor ?? null;
  const newest = rows[0]?.event.created_at ?? Math.floor(Date.now() / 1000);
  // The bounds key answers the REQUEST, so it is built from the cursor the
  // caller sent — `windowBoundsKey` is the same function the parser checks
  // with. Deriving it from this page's newest row made every page after the
  // first fail the parser's bounds check, so history stopped at one window.
  const boundsKey = windowBoundsKey(
    channelId,
    cursor?.event_id && typeof cursor.created_at === "number"
      ? { createdAt: cursor.created_at, eventId: cursor.event_id }
      : null,
  );
  out.push({
    id: `bounds:${boundsKey}`,
    pubkey: "",
    created_at: newest,
    kind: KIND_WINDOW_BOUNDS,
    tags: [["d", boundsKey]],
    content: JSON.stringify({
      has_more: reply.bounds?.has_more === true,
      next_cursor: nextCursor ? { created_at: newest, id: nextCursor } : null,
    }),
    sig: "",
  } as unknown as RelayEvent);
  return out;
}

/** NIP-29 edit-group-metadata. The relay re-publishes the room's 39000 from it. */
const KIND_NIP29_EDIT_METADATA = 9002;

/**
 * `update_channel` / `set_channel_topic` — one NIP-29 metadata edit.
 *
 * The relay accepts exactly these tags on a 9002 (`effects::apply_channel_edit`)
 * and authorizes them per tag, so only the fields the caller actually changed
 * are sent: including an unchanged `name` would make a topic edit require
 * rename rights.
 */
async function updateChannelViaRelay(raw: unknown): Promise<RawChannelRow> {
  const input = (raw ?? {}) as {
    channelId?: string;
    name?: string;
    description?: string;
    topic?: string;
    purpose?: string;
    visibility?: string;
    ttlSeconds?: number | null;
  };
  const channelId = input.channelId;
  if (!channelId) throw new Error("update_channel needs a channelId.");

  const tags: string[][] = [["h", channelId], ["d", channelId]];
  if (input.name !== undefined) tags.push(["name", input.name]);
  if (input.description !== undefined) tags.push(["about", input.description]);
  if (input.topic !== undefined) tags.push(["topic", input.topic]);
  if (input.purpose !== undefined) tags.push(["purpose", input.purpose]);
  if (input.visibility !== undefined) {
    tags.push(["visibility", input.visibility]);
  }
  if (input.ttlSeconds !== undefined) {
    tags.push(["ttl", input.ttlSeconds === null ? "" : String(input.ttlSeconds)]);
  }

  const signed = await sign({ kind: KIND_NIP29_EDIT_METADATA, content: "", tags });
  await publishSigned(signed, "Could not save those channel settings.");

  // Read the room back from the projection the edit just invalidated, so the
  // caller sees what the relay actually stored rather than what was asked for.
  // The UNFILTERED projection: a hidden DM is still a room a caller may rename,
  // and reading back through the sidebar list would report that write as a
  // failure after it had already committed.
  const rows = await channelProjection();
  const row = rows.find((candidate) => candidate.id === channelId);
  if (!row) throw new Error("The relay did not return that channel.");
  return row;
}

/** NIP-buzz DM open. The relay creates the room and adds every participant. */
const KIND_DM_OPEN = 41010;

/**
 * The DM room's id, derived exactly as the relay derives it
 * (`channel::dm_participant_hash`): sha256 over the sorted, de-duplicated
 * 32-byte pubkeys. Both sides computing the same id from the same participants
 * is what makes re-opening a conversation land in the same room instead of
 * forking a new one.
 */
async function dmChannelId(participants: string[]): Promise<string> {
  const sorted = [...new Set(participants.map((pk) => pk.toLowerCase()))].sort();
  const bytes = new Uint8Array(sorted.length * 32);
  sorted.forEach((pk, index) => bytes.set(hexToBytes(pk), index * 32));
  return bytesToHex(await sha256Bytes(bytes));
}

/**
 * `open_dm` — start (or re-find) a conversation.
 *
 * The relay has no DM route: the conversation IS a kind 41010, and the room it
 * creates is keyed by the participant hash, so this is idempotent. The row is
 * synthesised from what we just asked for rather than read back, because the
 * caller opens the conversation with it immediately and the relay's projection
 * for a brand-new room is one round trip behind.
 */
async function openDmViaRelay(pubkeys: unknown): Promise<RawChannelRow> {
  const others = Array.isArray(pubkeys)
    ? (pubkeys as unknown[]).filter(
        (pk): pk is string => typeof pk === "string" && /^[0-9a-f]{64}$/i.test(pk),
      )
    : [];
  if (others.length === 0) {
    throw new Error("A direct message needs someone to send it to.");
  }
  const me = await publicKeyHex();
  const participants = [me, ...others];
  const id = await dmChannelId(participants);
  const signed = await sign({
    kind: KIND_DM_OPEN,
    content: "",
    tags: others.map((pk) => ["p", pk.toLowerCase()]),
  });
  await publishSigned(signed, "Could not open that conversation.");
  const members = participants.map((pk) => pk.toLowerCase());
  return {
    id,
    name: `DM (${members.length})`,
    channel_type: "dm",
    visibility: "private",
    description: "",
    topic: null,
    purpose: null,
    member_count: members.length,
    member_pubkeys: members,
    last_message_at: null,
    archived_at: null,
    participants: [],
    participant_pubkeys: members,
    is_member: true,
    ttl_seconds: null,
    ttl_deadline: null,
  };
}

/**
 * The two rooms buzz's onboarding requires before it will show the workspace
 * (`welcome.ts`: `STARTER_GENERAL_CHANNEL_NAME` / `STARTER_WELCOME_CHANNEL_NAME`).
 * Both must come back as an OPEN STREAM this identity is a MEMBER of, or
 * `ensureStarterChannels` throws and onboarding stalls on its last step.
 */
const STARTER_CHANNELS = [
  { name: "general", description: "General conversation and community updates." },
  {
    name: "welcome-everyone",
    description: "Say hi, ask a question, or share what brought you here.",
  },
];

/**
 * `ensure_starter_channels` — make the starter rooms exist and make this
 * identity a member of them.
 *
 * The desktop backend creates them locally and syncs; here the relay is the
 * only store, so the first person to onboard CREATES them (409 means someone
 * else won that race, which is a success, not a failure) and everyone after
 * JOINS them. Joining is what `is_member` reads, and the second member of a
 * room would otherwise be handed a list where the starters look foreign.
 */
async function ensureStarterChannels(): Promise<RawChannelRow[]> {
  const { channelIdFor, createChannelViaRelay } = await import(
    "@/shared/browser-backend/channels"
  );
  const me = await publicKeyHex();
  let rows = await channelsFromRelay();

  for (const starter of STARTER_CHANNELS) {
    const id = channelIdFor(starter.name);
    const existing = rows.find((row) => row.id === id);
    if (!existing) {
      try {
        await createChannelViaRelay(
          { id, name: starter.name, channelType: "stream", visibility: "open" },
          me,
        );
        continue;
      } catch (error) {
        // Another tab created it between the read and the write.
        const status =
          typeof error === "object" && error !== null && "status" in error
            ? (error as { status?: unknown }).status
            : undefined;
        if (status !== 409 && !/already exists|409/i.test(String(error))) {
          throw error;
        }
      }
    }
    if (!existing?.is_member) await membershipCommand(KIND_NIP29_JOIN_REQUEST, id);
  }

  // The roster projection is republished as part of the same write, but the
  // read is a separate round trip; give it a moment rather than reporting a
  // room the caller just joined as one it is not in.
  for (let attempt = 0; attempt < 5; attempt += 1) {
    rows = await channelsFromRelay();
    const ready = STARTER_CHANNELS.every((starter) =>
      rows.some(
        (row) => row.id === channelIdFor(starter.name) && row.is_member,
      ),
    );
    if (ready) break;
    await new Promise((resolve) => setTimeout(resolve, 600));
  }
  return rows;
}

/** Channel id → the name/type its 39000 projection advertises. */
async function channelIndex(): Promise<Map<string, { name: string; type: string | null }>> {
  const events = await relayEvents({
    kinds: [KIND_GROUP_METADATA],
    limit: RELAY_PAGE_LIMIT,
  });
  const index = new Map<string, { name: string; type: string | null }>();
  for (const [id, event] of latestByDTag(events)) {
    index.set(id, {
      name: firstTag(event, "name") ?? id,
      type: firstTag(event, "t"),
    });
  }
  return index;
}

/**
 * `get_users_batch` — the requested pubkeys' kind-0 profiles (NIP-01 metadata),
 * newest per author. A pubkey with no kind 0 on this relay is reported as
 * `missing`, which is exactly what the desktop's relay-fed cache reports and
 * what makes the caller record a resolved miss instead of re-asking forever.
 *
 * `owner_pubkey` stays `null` on purpose: an agent's owner is a NIP-OA claim in
 * the profile whose `auth` tag must be VERIFIED before it is shown, and this
 * build has no verifier — attributing an unverified owner would be a lie the UI
 * renders as fact. `is_agent` is the profile's own NIP-24 `bot` flag; the relay
 * publishes no agent directory a tab could cross-check it against.
 */
type RawUserSummaryRow = {
  display_name: string | null;
  name: string | null;
  avatar_url: string | null;
  nip05_handle: string | null;
  owner_pubkey: string | null;
  is_agent: boolean;
  /**
   * The chosen kind-0's own timestamp — the fact that separates a live identity
   * from a same-named one that is nobody's account any more. The
   * desktop's local mirror keeps no event
   * timestamps, so its rows omit the field and callers treat it as unknown.
   */
  created_at: number;
};

async function usersBatchFromRelay(pubkeys: unknown): Promise<{
  profiles: Record<string, RawUserSummaryRow>;
  missing: string[];
}> {
  const wanted = Array.isArray(pubkeys)
    ? [...new Set((pubkeys as unknown[]).filter((pk): pk is string => typeof pk === "string" && pk.length > 0))]
    : [];
  if (wanted.length === 0) return { profiles: {}, missing: [] };

  const pages = await Promise.all(
    chunked(
      wanted.map((pk) => pk.toLowerCase()),
      AUTHORS_PER_FILTER,
    ).map((authors) =>
      relayEvents({ kinds: [KIND_PROFILE], authors, limit: RELAY_PAGE_LIMIT }),
    ),
  );
  const latest = latestByAuthor(pages.flat());

  const profiles: Record<string, RawUserSummaryRow> = {};
  const missing: string[] = [];
  for (const pubkey of wanted) {
    const event = latest.get(pubkey.toLowerCase());
    if (!event) {
      missing.push(pubkey);
      continue;
    }
    const metadata = parseProfileContent(event.content);
    // Keyed by the pubkey AS ASKED FOR: the caller looks each one up by the
    // string it sent, so re-casing the key would read as an unresolved profile.
    profiles[pubkey] = {
      display_name: stringOrNull(metadata.display_name),
      name: stringOrNull(metadata.name),
      avatar_url: stringOrNull(metadata.picture),
      nip05_handle: stringOrNull(metadata.nip05),
      owner_pubkey: null,
      is_agent: metadata.bot === true,
      created_at: event.created_at,
    };
  }
  return { profiles, missing };
}

/**
 * `get_profile` — this identity's own kind 0, read back from the relay.
 *
 * The desktop keeps a local copy and answers from it; the relay is this
 * build's only store, so the round trip IS the profile. `has_profile_event`
 * distinguishes "published nothing yet" (the UI then offers to set a name)
 * from "published an empty name", which the caller cannot tell apart from the
 * fields alone.
 */
async function ownProfileFromRelay() {
  return profileFromRelay(await publicKeyHex());
}

/**
 * One identity's kind 0, in the shape both `get_profile` (this tab's own) and
 * `get_user_profile` (anybody's) answer with.
 */
async function profileFromRelay(pubkey: string): Promise<{
  pubkey: string;
  display_name: string | null;
  avatar_url: string | null;
  about: string | null;
  nip05_handle: string | null;
  owner_pubkey: string | null;
  has_profile_event: boolean;
}> {
  const events = await relayEvents({
    kinds: [KIND_PROFILE],
    authors: [pubkey.toLowerCase()],
    limit: 1,
  }).catch(() => [] as RelayEvent[]);
  const event = latestByAuthor(events).get(pubkey.toLowerCase());
  if (!event) {
    return {
      pubkey,
      display_name: null,
      avatar_url: null,
      about: null,
      nip05_handle: null,
      owner_pubkey: null,
      has_profile_event: false,
    };
  }
  const metadata = parseProfileContent(event.content);
  return {
    pubkey,
    display_name:
      stringOrNull(metadata.display_name) ?? stringOrNull(metadata.name),
    avatar_url: stringOrNull(metadata.picture),
    about: stringOrNull(metadata.about),
    nip05_handle: stringOrNull(metadata.nip05),
    owner_pubkey: null,
    has_profile_event: true,
  };
}

/**
 * `search_users` — people and agents, matched by name over the relay's kind 0s.
 *
 * The desktop searches an indexed local mirror of every profile it has seen. A
 * tab has no mirror and the relay refuses NIP-50 (`ingress.rs` answers a search
 * filter with `CLOSED unsupported`), so the match has to happen here, over
 * events read off the relay's kind-0 index.
 *
 * It therefore WALKS that index rather than taking one page — see
 * {@link walkProfileSearch} for the walk and its cursor. This function is only
 * the projection: which kind 0 counts as a
 * match, and what a matched identity looks like to the caller.
 *
 * An empty query lists everyone — that is what the "New message" screen opens
 * with, and answering nothing there reads as "you have no colleagues".
 */
async function searchUsersFromRelay(input: {
  query?: unknown;
  limit?: unknown;
  cursor?: unknown;
}): Promise<{
  users: Array<RawUserSummaryRow & { pubkey: string }>;
  next_cursor: string | null;
}> {
  const query = typeof input.query === "string" ? input.query.trim().toLowerCase() : "";
  const limit =
    typeof input.limit === "number" && input.limit > 0 ? Math.floor(input.limit) : 8;
  const me = (await publicKeyHex()).toLowerCase();

  const { rows, nextCursor } = await searchProfiles({
    query,
    limit,
    cursor: input.cursor,
    readPage: (until) =>
      relayEvents({
        kinds: [KIND_PROFILE],
        limit: RELAY_PAGE_LIMIT,
        ...(until === undefined ? {} : { until }),
      }).catch(() => [] as RelayEvent[]),
    // The author index, not the corpus: one read for a query that names one
    // identity. Same shape `usersBatchFromRelay` uses.
    readAuthor: (pubkey) =>
      relayEvents({
        kinds: [KIND_PROFILE],
        authors: [pubkey],
        limit: 1,
      }).catch(() => [] as RelayEvent[]),
    match: (event) => {
      const pubkey = event.pubkey.toLowerCase();
      // Messaging yourself is not a person the picker should offer.
      if (pubkey === me) return null;
      const metadata = parseProfileContent(event.content);
      const displayName = stringOrNull(metadata.display_name);
      const name = stringOrNull(metadata.name);
      const nip05 = stringOrNull(metadata.nip05);
      const haystack = [displayName, name, nip05, pubkey]
        .filter((value): value is string => typeof value === "string")
        .join(" ")
        .toLowerCase();
      if (query && !haystack.includes(query)) return null;
      return {
        pubkey,
        display_name: displayName,
        name,
        avatar_url: stringOrNull(metadata.picture),
        nip05_handle: nip05,
        owner_pubkey: null,
        is_agent: metadata.bot === true,
        // The profile's own timestamp, so a caller can tell a live identity
        // from a stale one that happens to share its name.
        created_at: event.created_at,
      };
    },
  });

  return { users: rows, next_cursor: nextCursor };
}

type ProfileContent = {
  name?: unknown;
  display_name?: unknown;
  picture?: unknown;
  nip05?: unknown;
  about?: unknown;
  bot?: unknown;
};

function parseProfileContent(content: string): ProfileContent {
  try {
    const parsed = JSON.parse(content) as unknown;
    return parsed && typeof parsed === "object" ? (parsed as ProfileContent) : {};
  } catch {
    // A kind 0 whose content is not JSON is a profile we cannot read, which is
    // the same answer as no profile at all — never a thrown batch.
    return {};
  }
}

function stringOrNull(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value : null;
}

/** One row of the client's `RawRelayMember`. */
type RawRelayMemberRow = {
  pubkey: string;
  role: string;
  added_by: string | null;
  created_at: string;
};

/** Epoch seconds as the ISO string the client's mapper expects. */
function isoFromSeconds(seconds: unknown): string {
  return typeof seconds === "number" && Number.isFinite(seconds)
    ? new Date(seconds * 1_000).toISOString()
    : new Date(0).toISOString();
}

/**
 * `get_my_relay_membership` — this caller's own standing, from `/buzz/api/me`.
 *
 * Every moderation affordance in the
 * client is gated on a NIP-43 membership snapshot the relay does not publish and
 * will not, so `/me` answers the question directly about the caller and only the
 * caller.
 *
 * `null` means "not a member", which is a state only a CLOSED community has. An
 * open one has no membership to lack — everyone who authenticated is in it — so
 * answering `null` there would hide moderation from its own moderators.
 */
async function myRelayMembership(): Promise<RawRelayMemberRow | null> {
  const me = await relayApiGet<{
    pubkey?: string;
    role?: string;
    member?: boolean;
    membership_required?: boolean;
  }>("/me");
  if (me.membership_required === true && me.member !== true) return null;
  return {
    pubkey: me.pubkey ?? (await publicKeyHex()),
    role: me.role === "admin" ? "admin" : "member",
    added_by: null,
    created_at: new Date(0).toISOString(),
  };
}

/**
 * `list_relay_members` — the community roster, from `/buzz/api/members`.
 *
 * Paged by the relay, so this walks until it says `complete`. The walk is
 * BOUNDED: a roster that never reports completion is a relay bug, and looping on
 * it would hang the members screen instead of showing what was read.
 */
async function relayMembersFromRelay(): Promise<{ members: RawRelayMemberRow[] }> {
  const members: RawRelayMemberRow[] = [];
  let cursor: string | null = null;
  for (let page = 0; page < 25; page += 1) {
    const reply: {
      members?: Array<{ pubkey?: string; role?: string; created_at?: number | null }>;
      complete?: boolean;
      next_cursor?: string | null;
    } = await relayApiGet(
      `/members${cursor ? `?cursor=${encodeURIComponent(cursor)}` : ""}`,
    );
    for (const row of reply.members ?? []) {
      if (typeof row.pubkey !== "string") continue;
      members.push({
        pubkey: row.pubkey,
        role: row.role === "admin" ? "admin" : "member",
        added_by: null,
        created_at: isoFromSeconds(row.created_at),
      });
    }
    if (reply.complete !== false || !reply.next_cursor) break;
    cursor = reply.next_cursor;
  }
  return { members };
}

/**
 * `get_presence` — who is online right now, from `POST /buzz/api/presence`.
 *
 * This one has to be the HTTP route: presence is kind 20001, an EPHEMERAL kind
 * the relay never stores, so a `REQ` for history returns nothing and only the
 * relay's TTL-key scan knows who is live. The route answers with the online
 * SET, so every requested pubkey outside it is `offline` — the finer `away`
 * state rides the live kind-20001 subscription the presence hook already keeps
 * open, and inventing it here would overwrite a real one.
 */
async function presenceFromRelay(
  pubkeys: unknown,
): Promise<Record<string, "online" | "away" | "offline">> {
  const wanted = Array.isArray(pubkeys)
    ? (pubkeys as unknown[]).filter(
        (pk): pk is string => typeof pk === "string" && pk.length > 0,
      )
    : [];
  if (wanted.length === 0) return {};

  const response = await relayApiPost<{ online?: unknown }>("/presence", {});
  const online = new Set(
    (Array.isArray(response.online) ? response.online : [])
      .filter((pk): pk is string => typeof pk === "string")
      .map((pk) => pk.toLowerCase()),
  );

  const lookup: Record<string, "online" | "away" | "offline"> = {};
  for (const pubkey of wanted) {
    lookup[pubkey] = online.has(pubkey.toLowerCase()) ? "online" : "offline";
  }
  return lookup;
}

/**
 * `get_feed` — the home feed, from the relay's own `POST /buzz/api/feed`.
 *
 * The route is the honest source because the category definitions live there:
 * `mentions` and `needs_action` walk the caller's mention index, `activity`
 * walks readable channels, and every candidate is re-authorized per event. A
 * client-side reimplementation would be a second copy of that policy.
 *
 * The relay serves three categories; buzz shows four. `agent_activity` is the
 * agent job traffic (43001/43003/43004) split out of `activity` by kind — the
 * only agent signal on the wire, since a persona's chat is signed by an
 * ordinary workspace key with no directory to recognize it by.
 */
type RawFeedItemRow = {
  id: string;
  kind: number;
  pubkey: string;
  content: string;
  created_at: number;
  channel_id: string | null;
  channel_name: string;
  channel_type: string | null;
  tags: string[][];
  category: "mention" | "needs_action" | "activity" | "agent_activity";
};

async function feedFromRelay(input: {
  since?: number;
  limit?: number;
  types?: string;
}): Promise<{
  feed: {
    mentions: RawFeedItemRow[];
    needs_action: RawFeedItemRow[];
    activity: RawFeedItemRow[];
    agent_activity: RawFeedItemRow[];
  };
  meta: { since: number; total: number; generated_at: number };
}> {
  const requested = (input.types ?? "mentions,needs_action,activity,agent_activity")
    .split(",")
    .map((type) => type.trim())
    .filter(Boolean);
  const wants = (type: string) => requested.length === 0 || requested.includes(type);
  // The relay clamps `limit` to 100 (`FEED_MAX_LIMIT`); ask for what it can give.
  const limit = Math.min(Math.max(input.limit ?? 50, 1), 100);
  const body = (category: string) => ({
    category,
    limit,
    ...(input.since === undefined ? {} : { since: input.since }),
  });

  const [channels, mentions, needsAction, activity] = await Promise.all([
    channelIndex(),
    wants("mentions") ? feedCategory("mentions", body("mentions")) : [],
    wants("needs_action") ? feedCategory("needs_action", body("needs_action")) : [],
    wants("activity") || wants("agent_activity")
      ? feedCategory("activity", body("activity"))
      : [],
  ]);

  const item = (
    event: RelayEvent,
    category: RawFeedItemRow["category"],
  ): RawFeedItemRow => {
    const channelId = firstTag(event, "h");
    const channel = channelId ? channels.get(channelId) : undefined;
    return {
      id: event.id,
      kind: event.kind,
      pubkey: event.pubkey,
      content: event.content,
      created_at: event.created_at,
      channel_id: channelId,
      // A community-global row has no channel, which the UI renders as no
      // "#channel" chip — hence the empty string rather than a stand-in name.
      channel_name: channel?.name ?? "",
      channel_type: channel?.type ?? null,
      tags: event.tags,
      category,
    };
  };

  const feed = {
    mentions: mentions.map((event) => item(event, "mention")),
    needs_action: needsAction.map((event) => item(event, "needs_action")),
    activity: wants("activity")
      ? activity
          .filter((event) => !AGENT_ACTIVITY_KINDS.has(event.kind))
          .map((event) => item(event, "activity"))
      : [],
    agent_activity: wants("agent_activity")
      ? activity
          .filter((event) => AGENT_ACTIVITY_KINDS.has(event.kind))
          .map((event) => item(event, "agent_activity"))
      : [],
  };

  return {
    feed,
    meta: {
      // `since` echoes the window the caller asked for; 0 means "no cutoff",
      // which is what an omitted `since` asked the relay for.
      since: input.since ?? 0,
      total:
        feed.mentions.length +
        feed.needs_action.length +
        feed.activity.length +
        feed.agent_activity.length,
      generated_at: Math.floor(Date.now() / 1_000),
    },
  };
}

async function feedCategory(
  category: string,
  body: Record<string, unknown>,
): Promise<RelayEvent[]> {
  const response = await relayApiPost<{ events?: unknown }>("/feed", {
    ...body,
    category,
  });
  return Array.isArray(response.events)
    ? (response.events as RelayEvent[])
    : [];
}

/**
 * `get_relay_self` — the relay's own signing key, from its NIP-11 document.
 *
 * NIP-11 is the ONLY non-circular source for this: callers use the value to
 * decide which relay-signed state to trust, so deriving it from the author of a
 * relay-signed event would be trusting the thing being checked. This relay's
 * document advertises no `self` today, so the answer is `null` — the documented
 * "advertises none", which the callers treat as "trust nothing relay-signed".
 * A network or malformed-document failure rejects, as the caller expects.
 */
async function relaySelfFromNip11(): Promise<string | null> {
  const response = await fetch(`${relayHttpUrl()}/relay-info`, {
    headers: { Accept: "application/json" },
    signal: AbortSignal.timeout(HTTP_TIMEOUT_MS),
  });
  const parsed = await readJson(response);
  if (!response.ok) {
    throw new Error(relayErrorMessage(parsed, response.status));
  }
  const { self, pubkey } = (parsed ?? {}) as {
    self?: unknown;
    pubkey?: unknown;
  };
  const claimed = typeof self === "string" ? self : pubkey;
  return typeof claimed === "string" && /^[0-9a-f]{64}$/i.test(claimed)
    ? claimed.toLowerCase()
    : null;
}

/**
 * The commands whose refusal needs a reason of its OWN, keyed exactly.
 *
 * For many of these commands the generic "no native backend" sentence at the
 * bottom of this file is FALSE, which sends someone looking for a desktop app
 * to do something no desktop app can do either. Community membership is the
 * clearest case: there is no route that adds or removes a community member on
 * any platform.
 *
 * Keyed EXACTLY rather than by pattern, and checked first, because a regex can
 * swallow a neighbour — a bare `window` matches `get_channel_window` and every
 * room then renders empty until somebody speaks —
 * and an exact key cannot do that. Same reasoning as the relay's writable-key
 * table.
 */
const UNAVAILABLE_BY_COMMAND: Record<string, string> = {
  set_agent_managed_profiles:
    "Agent-managed profiles are a desktop-only preview feature — preview-features.json lists " +
    "`agentManagedProfiles` as platforms: [desktop]. On this build an agent's relay profile is " +
    "already the only copy, so there is nothing for the setting to arbitrate between.",

  // Local pack files: teams and channel templates
  ...Object.fromEntries(
    [
      "create_team",
      "update_team",
      "delete_team",
      "create_channel_template",
      "update_channel_template",
      "delete_channel_template",
      "duplicate_channel_template",
    ].map((cmd) => [
      cmd,
      "Teams and channel templates are pack files in the desktop app's local store, and a browser " +
        "tab has no filesystem to keep them in. Listing them answers with an empty catalog here, " +
        "so the pickers show 'none defined yet' rather than an error.",
    ]),
  ),

  // Local archive sync
  ...Object.fromEntries(
    [
      "archive_events",
      "read_archived_events",
      "create_save_subscription",
      "delete_save_subscription",
      "merge_save_subscription_kinds",
      "remove_save_subscription_kind",
    ].map((cmd) => [
      cmd,
      "Archive sync mirrors channels to a local database on disk, which a browser tab does not " +
        "have. `list_save_subscriptions` answers with an empty list here, so the sync manager " +
        "stays quiet instead of reporting a failure on every boot.",
    ]),
  ),

  get_agent_usage_series:
    "Usage series are computed from the kind-44200 turn metrics the desktop app archives into " +
    "its local SQLite database; this build keeps no local archive. (Nothing in the vendored " +
    "client calls this yet — the entry exists so the day something does, the reason is written.)",

  // Observed-unread store and its batched catch-up
  // The client has a complete localStorage fallback that is TRIGGERED by this
  // rejection (`useObservedUnreadPersistence` sets nativeFailedRef and reads
  // `observedUnreadStorage`). Answering a synthetic empty snapshot instead
  // would REPLACE that fallback and drop every badge, so this must stay a
  // refusal rather than a no-op.
  ...Object.fromEntries(
    ["observed_unread_open_scope", "observed_unread_ingest"].map((cmd) => [
      cmd,
      "The observed-unread store is a SQLite file in the desktop app's data directory. A tab " +
        "keeps the same read state in localStorage, which the unread hook falls back to when " +
        "this command is unavailable — badges keep working.",
    ]),
  ),
  unread_catch_up:
    "Batched unread catch-up classifies relay history against the desktop app's observed-unread " +
    "database, which this tab does not have. Unread state here is computed from live relay " +
    "traffic and the tab's own localStorage read markers instead.",

  // Teams: the local pack store, again
  set_team_shared:
    "Sharing publishes a kind-30178 catalog head built from a team in the desktop app's local " +
    "pack store. This build keeps no local teams (the team list is empty here), so there is " +
    "nothing to publish. The catalog itself IS readable here — `fetch_team_catalog` is answered.",
  add_team_from_catalog:
    "Adding a team copies the published definition and every member into the desktop app's " +
    "local team and persona packs; this build has no local packs to copy into. Cloud agents " +
    "are created directly on the relay instead.",

  relay_reconnect_hook:
    "The reconnect hook is a shell command a desktop user configures to repair their network " +
    "(a VPN reconnect, say) before buzz retries the relay. A tab cannot run a local command; " +
    "`relay_reconnect_hook_configured` answers false here, so the reconnect controller never " +
    "asks for it.",
  get_model_status:
    "Model status reports the local speech models the desktop huddle pipeline downloads and " +
    "loads. This build has no huddle audio pipeline, so there is no model to report on.",
  speak_agent_message:
    "Reading an agent's reply aloud runs the desktop app's local text-to-speech engine, which a " +
    "browser tab does not carry.",
  ...Object.fromEntries(
    [
      "terminal_attach",
      "terminal_detach",
      "terminal_close",
      "terminal_input",
      "terminal_resize",
      "terminal_scroll",
      "terminal_focus",
      "terminal_ack",
      "terminal_viewport_ready",
    ].map((cmd) => [
      cmd,
      "The agent terminal attaches to a pseudo-terminal the desktop app spawns on this machine. " +
        "A browser tab can spawn no local process, so there is no terminal to attach to.",
    ]),
  ),
  transfer_builderlab_community:
    "Transferring a hosted community moves it between accounts at the hosted community service " +
    "the desktop app signs in to. A community served by its own relay has no hosted account to " +
    "transfer between.",

  // Snapshots: packaged to and from a file
  ...Object.fromEntries(
    [
      "export_team_snapshot",
      "export_agent_snapshot",
      "fetch_snapshot_bytes",
    ].map((cmd) => [
      cmd,
      "A snapshot packages a local agent or team definition into a file. This build keeps no local " +
        "definitions — a cloud agent's definition lives on the relay — so there is nothing to " +
        "package. Downloading bytes the tab already holds does work; see the attachment menu.",
    ]),
  ),

  // Observer plane
  ...Object.fromEntries(
    ["build_observer_control_event", "decrypt_observer_event", "index_observer_channel_id"].map(
      (cmd) => [
        cmd,
        "Observer archiving is an internal desktop build capability: it encrypts channel events to " +
          "an operator key and writes them to a local archive. `observer_archive_default_enabled` " +
          "answers false here, which is the same answer an OSS desktop build gives.",
      ],
    ),
  ),

  sign_nostr_identity_binding:
    "Binding a Nostr identity to a hosted account is a Builderlab operation. This build has no " +
    "hosted account — the key in this tab IS the account — so there is nothing to bind it to.",

  // Local environment probes
  get_git_identity:
    "Reads your machine's git config (`user.name` and `user.email`), which a browser tab cannot see.",
  probe_backend_provider:
    "Probes your PATH for a locally installed harness, and a tab has no PATH to probe. The " +
    "harnesses this deployment can actually reach are declared by the relay — they appear in the " +
    "harness list without any probing.",

  // Relay-side reconciliation
  reconcile_inbound_persona_event:
    "The desktop app reconciles a persona's inbound profile against its own local copy. This build " +
    "keeps no local copy — the relay's kind-0 is the profile — so there is nothing to reconcile.",

  // Native-transport twins of HTTP reads
  fetch_join_policy:
    "Only the native relay transport asks Rust for the join policy; this build's transport is a " +
    "browser WebSocket, and its caller (shared/api/invites.ts) already takes the HTTP branch, " +
    "fetching /api/join-policy from the relay directly. Invoked here it would answer about a " +
    "transport that does not exist.",

  // Persona sharing
  set_persona_shared:
    "Sharing publishes a LOCAL persona's definition so the rest of the workspace can see it. This " +
    "build has no local personas — a cloud agent's definition lives on the relay and is already " +
    "visible to the workspace through the roster — so there is nothing unshared to publish.",

  // Agent cards: the desktop's portable persona snapshots
  ...Object.fromEntries(
    [
      "list_agent_cards",
      "load_agent_card",
      "save_agent_card",
      "mint_agent_card",
      "card_mint_key_status",
      "card_mint_save_openai_key",
    ].map((cmd) => [
      cmd,
      "Agent cards are portable persona snapshots the desktop app mints and keeps in its local " +
        "archive, using an OpenAI key stored in the OS keychain. A tab has neither the archive nor " +
        "the keychain. Cloud agents need no card here: their definitions live on the relay and move " +
        "with the workspace.",
    ]),
  ),

  // Pocket voices: the desktop's bundled neural TTS
  ...Object.fromEntries(
    [
      "get_tts_settings",
      "set_tts_enabled",
      "list_voice_registry",
      "preview_pocket_voice",
      "set_pocket_voice",
    ].map((cmd) => [
      cmd,
      "Pocket voices are neural TTS models bundled with the buzz desktop app and played through its " +
        "native audio pipeline; a browser tab has neither the voices nor the pipeline. Text chat with " +
        "agents is unaffected.",
    ]),
  ),
};

/**
 * The sentence a user sees when a native-only command is invoked.
 *
 * buzz renders a failed action's message directly (the huddle error banner, the
 * agents panels, `relayError`), so each family gets a specific explanation
 * rather than one generic failure — that is the difference between a button
 * that explains itself and a button that looks broken.
 */
const UNAVAILABLE_BY_FAMILY: Array<[RegExp, string]> = [
  [
    /huddle|audio|^push_audio_pcm$|^set_voice_input_mode$|^check_pipeline_hotstart$|^tts_|^stt_|voice_input|microphone|pocket_voice/,
    "Voice huddles need the buzz desktop app — the browser build has no native audio pipeline (mic tap, speech-to-text and mixing all run in the desktop process).",
  ],
  [
    /^mesh_|mesh_llm|^start_mesh|^stop_mesh|local_model/,
    "Mesh compute runs local models inside the buzz desktop app; a browser tab cannot host them.",
  ],
  [
    // LOCAL harnesses only. A cloud agent runs on the workspace itself, so
    // `create_persona`, `list_personas`, `get_agent_models` and friends are
    // IMPLEMENTED below against the relay — this family is the desktop-only
    // remainder: supervised local processes, ACP transports, PATH discovery.
    /managed_agent|^acp_|_acp$|acp_provider|^discover_backend_providers$|^discover_acp_auth_methods$|custom_harness|^get_runtime_file_config$|harness_process|_runtimes?$/,
    "That harness runs as a local process started by the buzz desktop app, so a browser tab cannot launch or supervise it. Cloud agents work here: create one with the Ferrite Cloud harness and it runs on the workspace itself, with nothing running on your machine.",
  ],
  [
    // Signing a PR status or review request AS THE REPO OWNER, when that owner
    // is a managed agent. `PullRequestReviewCard` asks for this path with
    // `signAsManagedOwner`, and the desktop answers it by signing with the
    // agent's own seckey, which it holds because it supervises the process
    // (`e2eBridge`'s mock signs the event as `input.targetOwner` — the agent —
    // for exactly that reason).
    //
    // A tab holds ONE key: this identity's, or a NIP-07 extension's
    // (`browser-backend/identity`). There is no second key to reach, so there
    // is no honest implementation here — and a dishonest one is worse than a
    // refusal, because the read side trusts a status only from the PR author or
    // the repo owner (`allowedActorsForRoot` in
    // features/projects/projectIssues.mjs). Signing with the viewer's key would
    // publish events every client silently discards, so the PR would look
    // unchanged and the failure would surface as "the relay lost my change"
    // rather than "the browser cannot do this".
    //
    // The one nearby thing that is NOT a way in, so it is not mistaken for one:
    // this deployment's relay does hold persona signing keys
    // (the relay's keys::keyring, operator-seeded per registered agent), but it
    // signs with them only on the agent tool-command path, authorized by the
    // agents-sibling seam token. There is no owner-authenticated "sign this
    // event as agent X" route, and inventing one is a relay authorization
    // design, not a browser-backend gap.
    //
    // Reachability, so nobody hunts for a bug that is not there: this refusal
    // is currently unreachable through the UI. `signAsManagedOwner` is
    // `useIsManagedAgent(project.owner)`, which reads `list_managed_agents` —
    // and this build answers that with `[]` (below), so the managed-owner
    // branch is never taken and the control is not offered at all. The message
    // exists for the day something else calls the command, and so the answer
    // is documented rather than inferred.
    /^sign_project_pull_request|^sign_project_issue_(un)?assignment$/,
    "Only the buzz desktop app can act for a repository owned by a managed agent: the status has to be signed with that agent's key, which the desktop holds because it runs the agent. This tab has just your own key. Your own pull requests, and repositories you own yourself, still close and reopen here.",
  ],
  [
    // A Bestie is ONE local managed agent pinned as a companion, stored in the
    // desktop's retention database (`commands/bestie.rs` requires
    // `backend.type === "local"`). This build runs no local agents —
    // `list_managed_agents` answers `[]` — so `get_bestie_assignment` is
    // answered `null` above and the three writes are refused here. The UI for
    // it is behind the `@protected-feature-components` alias, which resolves
    // to `publicUi.ts` in this build, so the path is not reachable today.
    /bestie/,
    "A Bestie is a local managed agent pinned in the desktop app's own database; this build runs no local agents, so there is nothing to assign, clear or open a conversation with.",
  ],
  [
    // The merge is a real git operation — clone both sides, merge, push, then
    // publish the resulting status — and `publish_project_pull_request_merged_status`
    // is its retry step, re-publishing the status event the merge already
    // signed. Both need a working copy, a git implementation and push
    // credentials. Unlike the signing pair above, THIS one is reachable today:
    // `canMerge` in PullRequestReviewCard is `isOwner || isManagedAgentOwner`,
    // so a repo owner is offered the Merge button in the browser and the
    // confirm dialog promises a push. The refusal is what has to carry the
    // explanation, so it names the alternative.
    /^merge_project_pull_request$|^publish_project_pull_request_merged_status$/,
    "Merging happens in a real git checkout, so it needs the buzz desktop app (or a `git merge` and push from a terminal): the browser build has no working copy, no git and no push credentials. Everything else on this pull request — review, approve, comment, close — works here.",
  ],
  [
    // Creating or deleting a branch on the REMOTE. Not a working-copy read, so
    // the filesystem message below would name the wrong reason: what a tab
    // lacks here is PUSH CREDENTIALS for the repository's host. On an RCS
    // remote the git password is the platform account bearer, which no browser
    // tab holds and no member should; on a GitHub remote it is the user's own
    // token, which the desktop app keeps in the OS keychain.
    /project_remote_branch/,
    "Creating or deleting a branch on the remote needs push credentials for that repository, and those live in the buzz desktop app's keychain rather than in a tab. A project room's process commands can do the same branch work from here.",
  ],
  [
    // Opening a terminal AT a working copy. Both commands hand a path to the
    // OS, and there is no path and no shell in a tab. The merge-recovery
    // variant is the same thing with two refs prepared, and those refs are in
    // the error the dialog is already showing — so the work can still be
    // finished by hand, which is what the message says rather than implying the
    // merge is lost.
    /^open_project_terminal$|^open_project_merge_recovery_terminal$/,
    "Opening a terminal at your working copy is a desktop action \u2014 the browser build has no working copy and no shell to open. The branch names this recovery needs are in the message shown with it, so the same merge can be finished in any terminal.",
  ],
  [
    // Agent and team SNAPSHOTS: a file written on export, read on import, with
    // the preview/confirm pair so it can be inspected before it is applied.
    // The export half is named exactly in the table above; this is the import
    // half and the encode-for-send step, which need the same local file.
    /snapshot_import|snapshot_for_send/,
    "Agent and team snapshots are files the buzz desktop app reads and writes on your machine; a tab has no snapshot file to import and none to encode. Cloud agents are created directly here instead \u2014 their definitions live on the relay and move with the workspace.",
  ],
  [
    // The LOCAL archive: events exported to disk and read back. The relay keeps
    // its own history, which is what this build reads.
    /^archive_events$|^read_archived_|archived_observer/,
    "The local archive is a file the buzz desktop app writes on your machine. This build reads history from the relay instead, which is where these events already are.",
  ],
  [
    /^git_|repo|worktree|^validate_repos_dir$|^open_in_editor$|diff|git_bash/,
    "Repository tools read your working copy through the buzz desktop app; the browser build has no filesystem access.",
  ],
  [
    /pairing|keyring|^reset_identity$|^import_nsec$|nostr_bind/,
    "This step uses the desktop app's OS keychain. The browser build keeps its key in this tab, so there is nothing to pair or unlock.",
  ],
  [
    // Narrowed to what a tab genuinely cannot do. `upload_media_bytes`, both
    // pickers, `fetch_media_bytes` and `search_messages` are IMPLEMENTED above
    // against the relay's media and search routes; this family is the
    // remainder — reading a file by PATH, and the native image clipboard.
    /^upload_media$|^copy_image_to_clipboard$|^download_image$|^download_file$/,
    "That one needs the buzz desktop app: it reads or writes a file on your machine. Attaching and viewing media works here \u2014 use the paperclip, or drag a file into the composer.",
  ],
  [
    // `window` is anchored to the OS-window commands. Bare, it also matched
    // `get_channel_window` — the command that loads a channel's HISTORY — so
    // every room rendered empty until somebody spoke in it.
    /notification|badge|dock|tray|global_shortcut|haptic|title_bar|^set_window_|^get_window_|_window_state$|window_chrome/,
    "That is an OS-level feature of the buzz desktop app (window chrome, dock and system shortcuts) and has no browser equivalent.",
  ],
  [
    // `update` has to be anchored to the SELF-update commands. A bare `update`
    // would swallow `update_channel`, `update_managed_agent` and `update_persona`
    // and tell the user their channel edit is a desktop diagnostics feature.
    /doctor|diagnostic|log_file|^open_logs$|^check_for_updates$|^install_update$|_update_status$|version_manifest/,
    "Diagnostics and self-update read the desktop app's install; the browser build has neither.",
  ],
];

function webIdentity(pubkey: string) {
  return {
    pubkey,
    display_name: `web-${pubkey.slice(0, 8)}`,
    lost: false,
    locked: false,
    reset_failed: false,
  };
}

export function unavailableCommandMessage(cmd: string): string {
  // Exact keys first: they name ONE command and cannot swallow a neighbour the
  // way a pattern can.
  const exact = UNAVAILABLE_BY_COMMAND[cmd];
  if (exact) {
    return exact;
  }
  for (const [pattern, message] of UNAVAILABLE_BY_FAMILY) {
    if (pattern.test(cmd)) {
      return message;
    }
  }
  // Reached only by a command nobody has classified. `commandCoverage.test.mjs`
  // asserts that nothing the client actually invokes lands here, so a new
  // unclassified command fails a gate rather than reaching a user with a
  // sentence that may not be true of it.
  return `This is a buzz desktop feature: the browser build has no native backend for "${cmd}".`;
}

/**
 * Answer one command, or `undefined` when this build does not implement it
 * (the caller then rejects, which every feature already handles).
 */
export function handleAppCommand(
  cmd: string,
  args: unknown,
): Promise<unknown> | undefined {
  const input = (args ?? {}) as Record<string, never>;
  switch (cmd) {
    case "get_relay_ws_url":
    case "get_default_relay_url":
      return Promise.resolve(relayWsUrl());
    case "get_relay_http_url":
      return Promise.resolve(relayHttpUrl());
    case "auto_connect_default_relay_enabled":
      return Promise.resolve(true);
    case "is_shared_identity":
      return Promise.resolve(false);
    case "get_public_key":
    case "get_pubkey":
      return publicKeyHex();
    case "get_identity":
    case "persist_current_identity":
      // buzz's RawIdentity. A browser identity is never "lost" or "locked":
      // there is no OS keychain to fail, just this tab's key.
      return publicKeyHex().then(webIdentity);
    case "import_identity": {
      if (extension()) {
        return Promise.reject(
          new Error(
            "Your browser extension controls this tab's identity. Import the key into that extension, or disable it for Buzz before importing an nsec here.",
          ),
        );
      }
      const { nsec, password } = input as unknown as {
        nsec?: unknown;
        password?: unknown;
      };
      const rawKey = String(nsec ?? "").trim();
      if (rawKey.startsWith("ncryptsec1")) {
        return (async () => {
          const [{ decryptBackup }, { setLocalSecretKey }] = await Promise.all([
            import("@/shared/browser-backend/backup"),
            import("@/shared/browser-backend/identity"),
          ]);
          const secretKey = decryptBackup(
            rawKey,
            typeof password === "string" ? password : "",
          );
          setLocalSecretKey(secretKey);
          return webIdentity(getPublicKey(secretKey));
        })();
      }
      try {
        const secretKey = importLocalIdentity(rawKey);
        return Promise.resolve(webIdentity(getPublicKey(secretKey)));
      } catch (error) {
        return Promise.reject(
          error instanceof Error
            ? error
            : new Error("The Nostr identity key could not be imported."),
        );
      }
    }
    // Desktop-only state that simply does not exist in a tab. Answering with
    // the empty value keeps the boot path moving.
    case "get_legacy_workspace_storage":
      // LegacyCommunityStorageSnapshot: the caller destructures every field,
      // so the empty answer must be the empty SHAPE, not null.
      return Promise.resolve({
        workspaces: null,
        activeWorkspaceId: null,
        onboardingCompletions: [],
      });
    case "get_media_proxy_port":
      return Promise.resolve(null);
    case "set_prevent_sleep_active": {
      // A tab CAN ask the OS to stay awake — see setPreventSleep. The desktop
      // app's "expired" event has no counterpart, so the toggle simply stays on
      // for as long as the tab is open and visible.
      const { active } = input as unknown as { active: boolean };
      return setPreventSleep(Boolean(active));
    }
    // The native deep-link queue. `buzz://` links are delivered to a desktop
    // shell by the OS and queued in Rust; a tab is opened BY its URL, so the
    // queue is always empty and every drain ends at once. Answered, not
    // refused: a refusal warns on every navigation (the drains re-subscribe
    // whenever the route changes), and a rejected
    // `clear_pending_navigation_deep_links` leaves
    // `resetNavigationDeepLinkDrain` with the drain disabled, which fails every
    // in-app community switch.
    case "take_pending_community_deep_link":
    case "take_pending_navigation_deep_link":
    case "take_pending_entity_deep_link":
      return Promise.resolve(null);
    case "acknowledge_pending_community_deep_link":
    case "acknowledge_pending_navigation_deep_link":
    case "acknowledge_pending_entity_deep_link":
      // Nothing was handed out, so nothing matches the id.
      return Promise.resolve(false);
    case "clear_pending_navigation_deep_links":
      return Promise.resolve(null);
    case "relay_reconnect_hook_configured":
      // No local reconnect hook exists in a tab, so the controller skips the
      // hook phase and goes straight to its backstop (see
      // `relay_reconnect_hook` in the unavailable table).
      return Promise.resolve(false);
    case "is_auto_update_supported":
      return Promise.resolve(false);
    case "get_baked_build_env":
      // A LIST of {key,value}, not a map: the callers call `.find` on it.
      return Promise.resolve([]);
    case "get_baked_build_env_keys":
      return Promise.resolve([]);
    case "update_profile": {
      // Publish the profile as a real kind-0 through the client's OWN live
      // connection. The relay is the profile's home — the
      // desktop build keeps a local copy too, which a tab has no place for.
      const p = input as unknown as ProfileUpdate;
      return (async () => {
        const pubkey = await publicKeyHex();
        const existing = await relayEvents({
          kinds: [KIND_PROFILE],
          authors: [pubkey],
          limit: 1,
        });
        const newest = latestByAuthor(existing).get(pubkey.toLowerCase());
        let current: Record<string, unknown> = {};
        if (newest?.content) {
          try {
            const parsed = JSON.parse(newest.content) as unknown;
            if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
              current = parsed as Record<string, unknown>;
            }
          } catch {
            // Malformed metadata cannot be preserved field-by-field. The
            // explicit patch still replaces it with a valid kind-0 document.
          }
        }
        const merged = mergeProfileUpdate(current, p);
        const content = JSON.stringify(merged);
        // Kind 0 replaces the whole event, so the existing tags (NIP-39
        // identity claims and the like) are carried over.
        const tags = Array.isArray(newest?.tags) ? newest.tags : [];
        const signed = (await sign({ kind: 0, content, tags })) as unknown as {
          id: string;
        };
        await publishSigned(
          signed,
          "Failed to publish your profile.",
          "Publishing your profile timed out.",
        );
        return {
          pubkey,
          display_name:
            stringOrNull(merged.display_name) ?? stringOrNull(merged.name),
          avatar_url: stringOrNull(merged.picture),
          about: stringOrNull(merged.about),
          nip05_handle: stringOrNull(merged.nip05),
          owner_pubkey: null,
          has_profile_event: true,
        };
      })();
    }
    // An agent that runs ON the workspace needs no local process, so these are
    // answered by the relay rather than refused (see `cloudAgents.ts`).
    case "list_personas": {
      return (async () => {
        const { listCloudAgents } = await import(
          "@/shared/browser-backend/cloudAgents"
        );
        return listCloudAgents();
      })();
    }
    case "encode_agent_snapshot_for_send": {
      return (async () => {
        const { encodeCloudAgentSnapshot } = await import(
          "@/shared/browser-backend/cloudAgents"
        );
        return encodeCloudAgentSnapshot(
          String((input as unknown as { id?: unknown }).id ?? ""),
        );
      })();
    }
    case "list_relay_agents": {
      // A DIFFERENT shape from `list_personas`, deliberately: this one answers
      // "who is present here" (pubkey, name, status) and its consumers call
      // `name.toLowerCase()`, so a persona row would crash them.
      return (async () => {
        const { listRelayAgents } = await import(
          "@/shared/browser-backend/cloudAgents"
        );
        return listRelayAgents();
      })();
    }
    case "revalidate_relay_agents": {
      // The send path's last check before an @mention is authorised
      // (`agentMentionRevalidation.ts`): "are these pubkeys still relay agents,
      // and may they be mentioned here?". Upstream re-reads the 30177 directory
      // and the room's 39002 roster; here the registry is the relay's agent
      // list and the membership is the asked room's 39002 roster — see
      // `relayAgentsForMentionRevalidation` for why both halves are needed.
      return (async () => {
        const { listRelayAgents, relayAgentsForMentionRevalidation } =
          await import("@/shared/browser-backend/cloudAgents");
        const request = input as unknown as {
          pubkeys?: unknown;
          channelId?: unknown;
        };
        const wanted = Array.isArray(request.pubkeys)
          ? request.pubkeys.filter(
              (value): value is string => typeof value === "string",
            )
          : [];
        const channelId =
          typeof request.channelId === "string" && request.channelId
            ? request.channelId
            : undefined;
        const [agents, roster] = await Promise.all([
          listRelayAgents(),
          channelId
            ? channelMembersFromRelay({ channelId }).then((reply) =>
                reply.members.map((member) => member.pubkey),
              )
            : Promise.resolve([] as string[]),
        ]);
        return relayAgentsForMentionRevalidation(
          agents,
          wanted,
          channelId,
          roster,
        );
      })();
    }
    case "fetch_persona_catalog": {
      // The shared kind-30175 heads, exactly as upstream's native session
      // pages, head-selects and parses them (`browser-backend/catalogs.ts`).
      return (async () => {
        const { personaCatalogFromRelay } = await import(
          "@/shared/browser-backend/catalogs"
        );
        return personaCatalogFromRelay();
      })();
    }
    case "fetch_team_catalog": {
      // The shared kind-30178 heads. Reading the catalog is a relay read a tab
      // can do; ADDING one of its teams is not (see `add_team_from_catalog`).
      return (async () => {
        const { teamCatalogFromRelay } = await import(
          "@/shared/browser-backend/catalogs"
        );
        return teamCatalogFromRelay();
      })();
    }
    case "agent_access_owner_only":
      // Upstream answers `option_env!("BUZZ_DESKTOP_BUILD_AGENT_ACCESS_OWNER_ONLY")`,
      // a flag set only on their release packaging. This build is not compiled
      // with it, so the truthful answer is the same one every OSS desktop
      // build gives. Left unanswered, `welcomeGuide` awaited the rejection
      // inside a `Promise.all` and the welcome team never seeded.
      return Promise.resolve(false);
    case "create_persona":
    case "update_persona": {
      return (async () => {
        const { saveCloudPersona } = await import(
          "@/shared/browser-backend/cloudAgents"
        );
        // Both commands carry `{ input: {...} }`; `update_persona` also carries
        // the existing `id`, which the relay treats as the agent to
        // reconfigure — and only its owner, a moderator or an operator may.
        // `update_persona_and_publish` (reads.ts) goes through the same save.
        const payload =
          (input as { input?: Record<string, unknown> })?.input ?? {};
        return saveCloudPersona(payload);
      })();
    }
    case "delete_persona": {
      return (async () => {
        const { removeCloudAgent } = await import(
          "@/shared/browser-backend/cloudAgents"
        );
        const id =
          (input as { id?: string })?.id ??
          (input as { input?: { id?: string } })?.input?.id ??
          "";
        await removeCloudAgent(id);
        return null;
      })();
    }
    case "get_agent_models":
    case "discover_agent_models": {
      return (async () => {
        const { cloudAgentModels } = await import(
          "@/shared/browser-backend/cloudAgents"
        );
        return cloudAgentModels();
      })();
    }
    // Media and search: `browser-backend/media.ts` and `…/search.ts`

    case "upload_media_bytes": {
      return (async () => {
        const { uploadMediaBytes } = await import(
          "@/shared/browser-backend/media"
        );
        return uploadMediaBytes(input as unknown as { data?: number[]; filename?: string });
      })();
    }
    case "pick_and_upload_media": {
      return (async () => {
        const { pickAndUploadMedia } = await import(
          "@/shared/browser-backend/media"
        );
        return pickAndUploadMedia();
      })();
    }
    case "pick_and_upload_image": {
      return (async () => {
        const { pickAndUploadImage } = await import(
          "@/shared/browser-backend/media"
        );
        return pickAndUploadImage();
      })();
    }
    case "fetch_media_bytes": {
      // Same-origin here, so no proxy is needed — the desktop tunnels this
      // through Rust only to dodge CORS.
      return (async () => {
        const { fetchMediaBytes } = await import(
          "@/shared/browser-backend/media"
        );
        return fetchMediaBytes(input as unknown as { url?: string }, relayLocation());
      })();
    }
    case "cancel_media_upload": {
      // Cancellation is a set the chunk loop consults between chunks — the
      // upload runs in THIS tab, so there is no native request to abort.
      return (async () => {
        const { cancelMediaUpload } = await import(
          "@/shared/browser-backend/media"
        );
        return cancelMediaUpload(
          input as unknown as { progressId?: string },
        );
      })();
    }
    case "release_media_upload": {
      // The caller is done with a progress id: drop its cancellation record,
      // which is exactly what upstream's `finish_media_upload` does.
      return (async () => {
        const { releaseMediaUpload } = await import(
          "@/shared/browser-backend/media"
        );
        return releaseMediaUpload(
          input as unknown as { progressId?: string },
        );
      })();
    }
    case "cancel_media_fetch":
    case "release_media_fetch":
      // Upstream keeps a registry of in-flight native fetches so a request id
      // can abort or release one. `fetch_media_bytes` here is a same-origin
      // `fetch` the caller already races with its own AbortSignal
      // (`tauriMedia.ts` synthesises the AbortError itself), so there is no
      // record to cancel or release: "done" is the truthful answer.
      return Promise.resolve(null);
    case "upload_media":
      // The ONE media command a tab genuinely cannot serve: it takes a file
      // PATH and reads it. Named specifically rather than swept into the
      // family refusal, because the alternative works and the caller should
      // hear about it.
      return Promise.reject(
        new Error(
          "Uploading by file path needs the buzz desktop app. Attaching a file from this tab works \u2014 use the paperclip, or drag a file into the composer.",
        ),
      );
    case "search_messages": {
      // The relay's own term index.
      return (async () => {
        const { searchMessages } = await import(
          "@/shared/browser-backend/search"
        );
        return searchMessages(
          input as unknown as {
            q?: string;
            limit?: number;
            channelId?: string | null;
            authors?: string[];
            since?: number;
            until?: number;
            cursor?: string | null;
          },
        );
      })();
    }

    // Workspace verbs a tab can serve: `browser-backend/workspace.ts`

    case "edit_message":
      // A kind 40003 the relay already scopes and indexes, and which the client
      // already reads back as a timeline overlay.
      return (async () => {
        const { editMessage } = await import("@/shared/browser-backend/workspace");
        return editMessage(
          relayIdentity,
          ((input as unknown as { input?: unknown }).input ?? input) as never,
        );
      })();
    case "get_contact_list":
      return (async () => {
        const { getContactList } = await import("@/shared/browser-backend/workspace");
        return getContactList(input as never);
      })();
    case "set_contact_list":
      return (async () => {
        const { setContactList } = await import("@/shared/browser-backend/workspace");
        return setContactList(relayIdentity, input as never);
      })();
    case "download_file":
      return (async () => {
        const { downloadFile } = await import("@/shared/browser-backend/workspace");
        return downloadFile(input as never, relayLocation());
      })();
    case "download_image":
      return (async () => {
        const { downloadImage } = await import("@/shared/browser-backend/workspace");
        return downloadImage(input as never, relayLocation());
      })();
    case "copy_image_to_clipboard":
      return (async () => {
        const { copyImageToClipboard } = await import(
          "@/shared/browser-backend/workspace"
        );
        return copyImageToClipboard(input as never, relayLocation());
      })();
    case "get_user_profile": {
      // Anybody's kind 0, through the same parser `get_profile` uses.
      const who = (input as unknown as { pubkey?: string }).pubkey;
      return who ? profileFromRelay(who.toLowerCase()) : ownProfileFromRelay();
    }
    case "get_channel_details": {
      // The single-row form of `get_channels`, plus the detail fields. The ones
      // a 39000 genuinely does not carry — who set the topic, and when — are
      // null rather than invented: the projection records current metadata, not
      // its provenance.
      const wanted = (input as unknown as { channelId?: string }).channelId ?? "";
      if (!wanted) {
        return Promise.reject(new Error("get_channel_details needs a channelId."));
      }
      return channelProjection().then((rows) => {
        const row = rows.find((candidate) => candidate.id === wanted);
        if (!row) throw new Error(`No channel ${wanted} on this relay.`);
        return {
          ...row,
          created_by: "",
          created_at: new Date(0).toISOString(),
          updated_at: new Date(0).toISOString(),
          topic_set_by: null,
          topic_set_at: null,
          purpose_set_by: null,
          purpose_set_at: null,
          topic_required: false,
          max_members: null,
          nip29_group_id: row.id,
        };
      });
    }
    case "set_channel_purpose": {
      // A 9002 carrying only `purpose` — the same verb `update_channel` sends,
      // narrowed so a purpose edit does not require rename rights.
      const purpose = input as unknown as { channelId: string; purpose: string };
      return updateChannelViaRelay({
        channelId: purpose.channelId,
        purpose: purpose.purpose,
      }).then(() => null);
    }
    case "hide_dm":
      return Promise.resolve(
        hideDm((input as unknown as { channelId?: string }).channelId ?? ""),
      );

    case "get_my_relay_membership":
      // This caller's own authority, which every moderation affordance is
      // gated on.
      return myRelayMembership();
    case "list_relay_members":
      // The community roster, paged from the relay's own membership rows.
      return relayMembersFromRelay();
    case "sign_out":
      // The key IS the account here: forgetting it is the whole of signing out.
      // Refused under an extension, where the key is not ours to forget —
      // clearing local storage would look like signing out while the extension
      // went on signing exactly as before.
      if (extension()) {
        return Promise.reject(
          new Error(
            "Your browser extension holds this tab's identity \u2014 disconnect Buzz from the extension to sign out.",
          ),
        );
      }
      clearLocalIdentity();
      return Promise.resolve(null);

    case "get_forum_posts":
      // A forum channel's index: roots with reply counts, from the same window
      // route the timeline uses.
      return (async () => {
        const { getForumPosts } = await import("@/shared/browser-backend/workspace");
        return getForumPosts(input as never);
      })();

    case "create_channel": {
      // The relay owns channel creation over its HTTP face; it then publishes
      // the 39000/39002 the list above is built from.
      return (async () => {
        const me = await publicKeyHex();
        const { createChannelViaRelay } = await import(
          "@/shared/browser-backend/channels"
        );
        return createChannelViaRelay(
          input as unknown as {
            name: string;
            channelType?: string;
            visibility?: string;
            description?: string;
            ttlSeconds?: number;
          },
          me,
        );
      })();
    }
    case "copy_text_to_clipboard": {
      // A tab has a clipboard; only the *image* variant needs the native one.
      // Without this the family table below claims the whole clipboard is
      // desktop-only, and "Copy invite link" fails on every invite screen.
      const payload = input as unknown as { text?: string; html?: string };
      return navigator.clipboard.writeText(payload.text ?? "");
    }
    case "read_clipboard_text":
      // The plain-text paste shortcut (Cmd/Ctrl+Shift+V in the composer). The
      // handler runs inside a keydown, so the user-gesture requirement the
      // async clipboard API has is met; a browser that still refuses (no
      // permission, insecure context) rejects and the caller already treats
      // that as "paste nothing".
      return navigator.clipboard.readText();
    case "relay_requires_membership":
      // This relay's community gate is opt-in (buzz-community-closed); the
      // client asks so onboarding can demand an invite. Reporting `false`
      // matches the deployment's default-open posture — a closed deployment
      // still refuses the write itself, so this is a hint, not the gate.
      return Promise.resolve(false);
    case "get_profile":
      // No local profile store in a browser, so read the identity's OWN kind-0
      // back from the relay — the same event `update_profile` published.
      return ownProfileFromRelay();
    case "apply_workspace":
      // The desktop app points its Rust backend at a relay here. This build's
      // relay is fixed at compile time (VITE_RELAY_WS_URL, else same origin),
      // so there is nothing to switch.
      return Promise.resolve(null);
    case "validate_repos_dir":
      return Promise.resolve(null);
    case "get_global_agent_config":
      return Promise.resolve(readAgentConfig());
    case "set_global_agent_config":
      return Promise.resolve(
        writeAgentConfig((input as unknown as { config?: unknown }).config),
      );
    case "discover_acp_providers":
      // Declared by the relay, not probed on this machine — see `harnessCatalog`.
      return harnessCatalog();
    case "discover_backend_providers":
      // Agent harnesses are local CLI processes; a tab has none. The empty
      // catalog is what the UI renders as "no harnesses found".
      return Promise.resolve([]);
    case "get_nsec": {
      // The backup screen shows the key the user would need to re-import.
      // Under a NIP-07 extension that key is the EXTENSION's and cannot be
      // exported from here; handing out the localStorage key instead would
      // let the user "back up" a key that controls nothing (and mint one as
      // a side effect of opening the screen).
      if (extension()) {
        return Promise.reject(
          new Error(
            "Your identity key is held by your browser extension — back it up there.",
          ),
        );
      }
      const sk = localSecretKey();
      return import("nostr-tools/nip19").then((nip19) => nip19.nsecEncode(sk));
    }

    // Encrypted backup (NIP-49)
    /* Pure crypto over the key this tab already holds; see                    */
    /* `browser-backend/backup.ts`. Creating one under a NIP-07 extension is   */
    /* refused for the same reason `get_nsec` is: that key is not ours.        */

    case "create_ncryptsec_backup": {
      if (extension()) {
        return Promise.reject(
          new Error(
            "Your identity key is held by your browser extension — back it up there.",
          ),
        );
      }
      return (async () => {
        const { createBackup } = await import("@/shared/browser-backend/backup");
        return createBackup(
          localSecretKey(),
          String((input as unknown as { password?: unknown }).password ?? ""),
        );
      })();
    }
    case "generate_backup_passphrase": {
      return (async () => {
        const { generatePassphrase } = await import(
          "@/shared/browser-backend/backup"
        );
        return generatePassphrase(
          input as unknown as { words?: number; separator?: string },
        );
      })();
    }
    case "save_ncryptsec_copy": {
      return (async () => {
        const { saveNcryptsecCopy } = await import(
          "@/shared/browser-backend/backup"
        );
        return saveNcryptsecCopy(
          String((input as unknown as { ncryptsec?: unknown }).ncryptsec ?? ""),
        );
      })();
    }
    case "verify_ncryptsec_backup": {
      // Verification needs only the CURRENT PUBKEY for the match check, so it
      // works under an extension too — useful precisely when the backup being
      // checked is for a different identity than the extension's.
      return (async () => {
        const { verifyBackup } = await import("@/shared/browser-backend/backup");
        const { ncryptsec, password } = input as unknown as {
          ncryptsec?: unknown;
          password?: unknown;
        };
        const me = await publicKeyHex().catch(() => null);
        return verifyBackup(String(ncryptsec ?? ""), String(password ?? ""), me);
      })();
    }
    case "list_save_subscriptions":
      // Archive sync mirrors channels to local disk, which a tab has none of.
      // The honest answer is "no subscriptions", not a rejection: rejecting
      // makes the sync manager log a failure on every boot for a feature that
      // is simply not part of this build.
      return Promise.resolve([]);
    // "Set up your community", self-hosted edition
    case "get_builderlab_auth":
    case "start_builderlab_login":
      // There is no hosted account to sign in to: the identity in this tab IS
      // the account, and the community it can reach is the relay this build
      // was pointed at. Anything but an account leaves the flow stuck on "Sign
      // in to continue" with no way forward in a browser.
      return hostedAccount();
    case "cancel_builderlab_login":
    case "clear_builderlab_auth":
      // Nothing to cancel or forget — the account is the tab's own key.
      return Promise.resolve(null);
    case "get_builderlab_nostr_identity":
    case "bind_builderlab_nostr_identity":
      return hostedIdentity();
    case "delete_builderlab_nostr_identity":
      // Unbinding would orphan the only identity this build has.
      return Promise.resolve({
        error: {
          code: "not_owner",
          message:
            "This workspace's identity is the account; it cannot be unlinked here.",
        },
      });
    case "list_builderlab_communities":
      return hostedCommunities();
    case "check_builderlab_community_name":
    case "create_builderlab_community":
    case "archive_builderlab_community":
    case "unarchive_builderlab_community":
      // Provisioning a NEW community means deploying another relay, which is
      // an operator action against Ferrite, not something a tab can do. Say so
      // in the shape the UI already renders rather than failing opaquely.
      return Promise.resolve({
        error: {
          code: "relay_unavailable",
          message:
            "This build connects to one self-hosted community; new ones are provisioned by deploying another relay.",
        },
      });
    case "create_auth_event": {
      // NIP-42: the relay's challenge, bound to the relay URL.
      const { challenge, relayUrl } = input as unknown as {
        challenge: string;
        relayUrl: string;
      };
      return sign({
        kind: 22242,
        content: "",
        tags: [
          ["relay", relayUrl],
          ["challenge", challenge],
        ],
      }).then((e) => JSON.stringify(e));
    }
    case "sign_event": {
      const e = input as unknown as {
        kind: number;
        content: string;
        tags: string[][];
      };
      return sign(e).then((signed) => JSON.stringify(signed));
    }
    case "nip44_encrypt_to_self": {
      // Self-encryption must use the SAME identity that signs (the sidebar
      // sync docs are signed events encrypted to their author): under a
      // NIP-07 extension that is the extension's key, so the extension does
      // the crypto.
      const { plaintext } = input as unknown as { plaintext: string };
      const ext = extension();
      if (ext) {
        if (!ext.nip44) {
          return Promise.reject(
            new Error(
              "Your browser extension holds your key but does not offer NIP-44 encryption.",
            ),
          );
        }
        return ext
          .getPublicKey()
          .then((pk) => ext.nip44!.encrypt(pk, plaintext));
      }
      const sk = localSecretKey();
      const key = nip44.getConversationKey(sk, getPublicKey(sk));
      return Promise.resolve(nip44.encrypt(plaintext, key));
    }
    case "nip44_decrypt_from_self": {
      const { ciphertext } = input as unknown as { ciphertext: string };
      const ext = extension();
      if (ext) {
        if (!ext.nip44) {
          return Promise.reject(
            new Error(
              "Your browser extension holds your key but does not offer NIP-44 decryption.",
            ),
          );
        }
        return ext
          .getPublicKey()
          .then((pk) => ext.nip44!.decrypt(pk, ciphertext));
      }
      const sk = localSecretKey();
      const key = nip44.getConversationKey(sk, getPublicKey(sk));
      return Promise.resolve(nip44.decrypt(ciphertext, key));
    }

    // Data the relay holds: read it from the relay

    case "get_channels": {
      // The workspace itself, from the relay-signed NIP-29 projections — the
      // same events buzz's own client builds its channel list from — in
      // upstream's `{hash, channels | null, last_messages}` answer.
      const knownHash = (input as { knownHash?: unknown } | undefined)?.knownHash;
      return Promise.all([
        import("@/shared/browser-backend/channelsPayload"),
        channelsFromRelay(),
      ]).then(([{ channelsPayload }, rows]) => channelsPayload(rows, knownHash));
    }
    case "get_channel_members":
      // One room's roster, from the same 39002 projection.
      return channelMembersFromRelay(
        input as unknown as { channelId?: unknown },
      );
    case "ensure_starter_channels":
      // Onboarding's last gate: `general` and `welcome-everyone`, joined.
      return ensureStarterChannels();
    case "add_reaction": {
      // NIP-25: a kind 7 whose content is the emoji and whose `e` tag names
      // the target. NIP-30 custom emoji ride as an `emoji` tag.
      const react = input as unknown as {
        eventId: string;
        emoji: string;
        emojiUrl?: string | null;
      };
      const tags: string[][] = [["e", react.eventId]];
      if (react.emojiUrl) {
        tags.push(["emoji", react.emoji.replaceAll(":", ""), react.emojiUrl]);
      }
      return (async () => {
        const signed = await sign({
          kind: KIND_REACTION,
          content: react.emoji,
          tags,
        });
        await publishSigned(signed, "Could not add that reaction.");
        return null;
      })();
    }
    case "remove_reaction": {
      // Removing is a NIP-09 deletion of THIS author's matching kind 7. The
      // relay tallies reactions per (target, author, emoji), so it resolves
      // which one a `k`-scoped delete refers to.
      const undo = input as unknown as { eventId: string; emoji: string };
      return (async () => {
        const me = (await publicKeyHex()).toLowerCase();
        const mine = await relayEvents({
          kinds: [KIND_REACTION],
          authors: [me],
          "#e": [undo.eventId],
          limit: RELAY_PAGE_LIMIT,
        } as unknown as RelaySubscriptionFilter);
        const target = mine.find((event) => event.content === undo.emoji);
        if (!target) return null;
        const signed = await sign({
          kind: KIND_DELETION,
          content: "",
          tags: [
            ["e", target.id],
            ["k", String(KIND_REACTION)],
          ],
        });
        await publishSigned(signed, "Could not remove that reaction.");
        return null;
      })();
    }
    case "get_event": {
      // One event by id, as a JSON STRING (the caller parses it). The relay is
      // the only store here, and its id filter is the same one a REQ uses.
      const wanted = (input as unknown as { eventId?: string }).eventId ?? "";
      // A client-local placeholder id (an `optimistic-<uuid>` cache row the
      // relay has not confirmed yet) is empty BY CONSTRUCTION, not "missing",
      // so asking the relay would burn a round trip to learn nothing. Still a
      // rejection — the callers
      // (`fetchRouteEvent`) already treat one as "no event", and answering
      // an empty success here would hide that the caller is holding an
      // unconfirmed id.
      if (!isRelayEventId(wanted)) {
        return Promise.reject(
          new Error(
            `"${wanted}" is a client-local placeholder id, not a relay event id — the send it stands for has not been confirmed yet.`,
          ),
        );
      }
      // `kinds` is required by the filter type, and the relay refuses an
      // unbounded one; the timeline kinds are the only ones a caller resolves
      // an event id for (a reply target, a route's deep link).
      return relayEvents({
        ids: [wanted],
        kinds: [...CHANNEL_TIMELINE_CONTENT_KINDS],
        limit: 1,
      }).then((events) => {
        const found = events.find((event) => event.id === wanted);
        if (!found) throw new Error(`No event ${wanted} on this relay.`);
        return JSON.stringify(found);
      });
    }
    case "get_events": {
      // The batch twin of `get_event`, as raw events (the workflow list resolves
      // its message presentations through it). Upstream normalises the ids,
      // dedupes, chunks by 1000 and asks for its `GET_EVENT_KINDS`; the relay
      // here pages at 500, so the chunk is 500 and the kind list is upstream's.
      const raw = (input as unknown as { eventIds?: unknown }).eventIds;
      const ids = [
        ...new Set(
          (Array.isArray(raw) ? raw : [])
            .filter((value): value is string => typeof value === "string")
            .map((value) => value.trim().toLowerCase())
            .filter((value) => /^[0-9a-f]{64}$/.test(value)),
        ),
      ];
      if (ids.length === 0) return Promise.resolve([]);
      return (async () => {
        const found: RelayEvent[] = [];
        for (const chunk of chunked(ids, RELAY_PAGE_LIMIT)) {
          const events = await relayEvents({
            ids: chunk,
            kinds: [...GET_EVENT_KINDS],
            limit: chunk.length,
          });
          found.push(...events);
        }
        return found;
      })();
    }
    case "get_channel_reconnect_repair": {
      // After a socket reconnect the client asks, per live channel, for what it
      // missed: the channel's content kinds between `since` and `until`, newest
      // first, paged by `limit` (`relayReconnectReplay.ts`). A rejection would
      // mark the repair window done WITHOUT repair, silently dropping messages
      // that arrived during a disconnect.
      //
      // Upstream also passes `before_id` for exact keyset paging. The relay's
      // filter parser ignores unknown keys, so that cursor cannot be honoured;
      // the page is bounded by `until` instead, and the event the cursor names
      // is dropped so the client's "cursor did not advance" guard cannot trip.
      // The client dedupes across pages (`seenEventIds`), so an `until` page
      // that overlaps the previous one costs a duplicate, not a message.
      const args = input as unknown as {
        channelId?: unknown;
        since?: unknown;
        limit?: unknown;
        until?: unknown;
        beforeId?: unknown;
      };
      const channelId = typeof args.channelId === "string" ? args.channelId : "";
      if (!channelId) {
        return Promise.reject(new Error("channel reconnect repair needs a channelId"));
      }
      const since = typeof args.since === "number" && args.since >= 0 ? Math.floor(args.since) : 0;
      const limit = Math.min(
        RELAY_PAGE_LIMIT,
        Math.max(1, typeof args.limit === "number" ? Math.floor(args.limit) : RELAY_PAGE_LIMIT),
      );
      const filter: RelaySubscriptionFilter = {
        kinds: [...CHANNEL_REPAIR_KINDS],
        "#h": [channelId],
        since,
        limit,
      };
      if (typeof args.until === "number" && Number.isFinite(args.until)) {
        filter.until = Math.floor(args.until);
      }
      const beforeId = typeof args.beforeId === "string" ? args.beforeId : null;
      return relayEvents(filter).then((events) =>
        events
          .filter((event) => event.id !== beforeId)
          .sort((a, b) => b.created_at - a.created_at || (a.id < b.id ? -1 : 1)),
      );
    }
    case "get_open_channel_directory":
      // The channel browser's discovery list: upstream is "my channels plus
      // every open 39000". The projection this seam already builds from the
      // relay's 39000/39002 events is not member-filtered — the relay's access
      // gate decides the visible set — so it is that superset already;
      // `get_channels` merely hides dismissed DMs from it.
      return channelProjection();
    case "channel_head_cache_load":
      // Upstream keeps the last few channels' head pages in a SQLite file so a
      // cold start paints before the relay answers — "a paint accelerator
      // only", by its own comment, replaced by the relay page immediately. A
      // tab persists nothing across sessions here, and `[]` is what "nothing
      // persisted" looks like to the loader: it takes the cold path it would
      // take on a fresh install. Left unanswered this warned on every boot.
      return Promise.resolve([]);
    case "channel_head_cache_store":
    case "channel_head_cache_clear":
      // Nothing is kept (see `channel_head_cache_load`), so storing and
      // clearing are complete the moment they are asked for. Left unanswered
      // the store warned on every channel open.
      return Promise.resolve(null);
    case "get_bestie_assignment":
      // No local managed agent exists in a tab (`list_managed_agents` is `[]`),
      // so none can be assigned — `null` is the same answer the desktop gives a
      // person who has not chosen one. The writes are refused, not no-op'd
      // (see the `bestie` family below).
      return Promise.resolve(null);
    case "announce_archive_sync_epoch":
      // The archive sync manager asks for a fresh epoch, then starts a sync
      // under it. Nothing syncs here (`list_save_subscriptions` is `[]`; the
      // local archive is a desktop file), so the epoch is a constant the
      // manager can carry — it must be a NUMBER, not null, or the manager
      // treats the handshake as failed and warns on every boot.
      return Promise.resolve(1);
    case "start_archive_sync":
    case "stop_archive_sync":
      // Same: there is no archive to sync, so starting and stopping are
      // already done. The e2e bridge answers these identically.
      return Promise.resolve(null);
    case "get_channel_window":
      return channelWindowFromRelay(
        input as unknown as {
          channelId?: unknown;
          limitRows?: unknown;
          cursor?: unknown;
        },
      );
    case "get_thread_replies":
      // The reply subtree under one root, paged oldest-first.
      return threadRepliesFromRelay(input);
    case "send_channel_message":
      // Every reply, media send, custom-emoji send and forum post routes here
      // (plain top-level messages take the client's own WebSocket helper).
      return sendChannelMessageViaRelay(relayIdentity, input);
    case "update_channel":
      return updateChannelViaRelay(
        (input as unknown as { input?: unknown }).input ?? input,
      );
    case "set_channel_topic": {
      const topic = input as unknown as { channelId: string; topic: string };
      return updateChannelViaRelay({
        channelId: topic.channelId,
        topic: topic.topic,
      }).then(() => null);
    }
    case "get_canvas":
      // The newest kind:40100 in the room IS the canvas.
      return getCanvasFromRelay(input);
    case "set_canvas":
      // A whole-document supersede: sign a fresh 40100 and publish it.
      return setCanvasViaRelay(relayIdentity, input);
    case "open_dm":
      return openDmViaRelay((input as unknown as { pubkeys?: unknown }).pubkeys);
    case "join_channel":
      return membershipCommand(
        KIND_NIP29_JOIN_REQUEST,
        (input as unknown as { channelId: string }).channelId,
      );
    case "leave_channel":
      return membershipCommand(
        KIND_NIP29_LEAVE_REQUEST,
        (input as unknown as { channelId: string }).channelId,
      );

    // Changing a room: membership, archive, deletion
    /* All relay verbs; see `channelAdmin` for the rule behind each one.       */

    case "add_channel_members":
      // 9000 per invitee, answered per pubkey because the relay authorizes
      // per event and the invite card lists refusals next to the people.
      return addChannelMembersViaRelay(relayIdentity, input);
    case "change_channel_member_role":
      // The same 9000 — put-user IS the verb that assigns a role.
      return changeChannelMemberRoleViaRelay(relayIdentity, input);
    case "remove_channel_member":
      // 9001. `leave_channel` above is the self twin (9022).
      return removeChannelMemberViaRelay(relayIdentity, input);
    case "archive_channel":
      // A 9002 carrying only `archived` — the relay then refuses writes into
      // the room while keeping it readable.
      return setChannelArchivedViaRelay(relayIdentity, input, true);
    case "unarchive_channel":
      return setChannelArchivedViaRelay(relayIdentity, input, false);
    case "delete_message":
      // NIP-09 kind 5, and only for this identity's OWN message — see
      // `deleteMessageViaRelay` for why publishing one for anybody else's
      // would hide a message the relay goes on serving.
      return deleteMessageViaRelay(relayIdentity, input);
    case "delete_channel":
      return (async () => {
        const { forgetCreatedChannel } = await import(
          "@/shared/browser-backend/channels"
        );
        const channelId = (input as unknown as { channelId?: unknown }).channelId;
        if (typeof channelId === "string") forgetCreatedChannel(channelId);
        return deleteChannelViaRelay(relayIdentity, input);
      })();
    case "search_users":
      // People to message or mention, from the relay's kind 0s. `cursor` is
      // forwarded: the read pages, so dropping it would restart every "load
      // more" at the top of the index.
      return searchUsersFromRelay(
        input as unknown as {
          query?: unknown;
          limit?: unknown;
          cursor?: unknown;
        },
      );
    case "get_users_batch":
      // Profiles are kind-0 events on the relay; the desktop only caches them.
      return usersBatchFromRelay(
        (input as unknown as { pubkeys?: unknown }).pubkeys,
      );
    case "get_presence":
      // Kind 20001 is ephemeral, so the relay's TTL scan is the only snapshot.
      return presenceFromRelay(
        (input as unknown as { pubkeys?: unknown }).pubkeys,
      );
    case "get_feed":
      // The relay owns the feed's category definitions; ask it, don't restate.
      return feedFromRelay(
        input as unknown as { since?: number; limit?: number; types?: string },
      );
    case "get_relay_self":
      // The relay's own key, as it advertises it in NIP-11 — see the helper for
      // why an event author is not an acceptable substitute.
      return relaySelfFromNip11();

    // Capabilities a tab lacks: the documented EMPTY answer
    /* A rejection would paint a red error over a question whose true answer   */
    /* is "there is nothing here", so each of these returns the empty SHAPE.   */

    case "list_managed_agents":
      // Managed agents ARE local processes the desktop supervises; a tab hosts
      // none, so it manages none. (Relay-side agents come from
      // `list_relay_agents` above and are unaffected.)
      return Promise.resolve([]);
    case "list_teams":
      // Teams are files in the desktop app's pack directory. No filesystem, no
      // packs — and an empty catalog is what the pickers render as "none
      // defined yet". (Personas are relay-held cloud agents; see
      // `list_personas` above.)
      return Promise.resolve([]);
    case "list_channel_templates":
      // Templates live in the same local pack store as personas/teams; without
      // it there are no built-ins to list, so channel creation just skips the
      // template step.
      return Promise.resolve([]);
    case "list_archived_identities":
      // The relay's own archive rows — the state, rather than the published
      // 13535 event, which is a derived view that can be one publish behind.
      // Signed as THIS tab's identity: the route is a members-only read.
      return (async () => {
        const { listArchivedIdentities } = await import(
          "@/shared/browser-backend/identityArchive"
        );
        return listArchivedIdentities(relayIdentity);
      })();
    case "get_huddle_state":
      // Huddle audio runs in the desktop process. `phase: "idle"` is the full
      // HuddleState for "not in a huddle" — the bar stays hidden, which beats
      // an error banner over a huddle nobody started.
      return Promise.resolve({
        phase: "idle",
        parent_channel_id: null,
        ephemeral_channel_id: null,
        participants: [],
        agent_pubkeys: [],
        tts_enabled: false,
        transcription_enabled: false,
        is_creator: false,
        voice_input_mode: "voice_activity",
      });
    case "list_audio_output_devices":
      // This command enumerates the devices the NATIVE mixer can route to. A
      // tab cannot route huddle audio anywhere, so offering a device list would
      // promise a control that does nothing.
      return Promise.resolve([]);
    case "get_audio_output_device":
      // No native output selection exists, and "" is the caller's own
      // no-selection value (it seeds a select with it).
      return Promise.resolve("");
    case "get_voice_input_mode":
      // Push-to-talk needs a global hotkey the OS grants the desktop app;
      // `voice_activity` is the mode the caller documents as its default.
      return Promise.resolve("voice_activity");
    case "reconcile_managed_agent_runtimes":
      // Reconciliation compares local agent PROCESSES against the relay's
      // records. With no processes there is nothing to reconcile, and the empty
      // status list is what the UI reads as "no runtimes".
      return Promise.resolve([]);
    case "observer_archive_default_enabled":
    case "agent_metric_archive_default_enabled":
      // Both read a build-time flag baked into an internal desktop build (see
      // `tauriArchive.ts`). This build bakes neither, which is `false` — the
      // same answer an OSS desktop build gives.
      return Promise.resolve(false);
    case "get_os_idle_seconds":
      // OS-wide input idle has no browser API. `null` is the caller's
      // documented "platform has no idle API", and the presence hook then falls
      // back to in-app activity — a wrong number would mark people away.
      return Promise.resolve(null);

    // Workflows: kind 30620; see `browser-backend/workflows.ts`

    case "get_channel_workflows":
      return (async () => {
        const { channelWorkflows } = await import("@/shared/browser-backend/workflows");
        const id = (input as unknown as { channelId?: string }).channelId ?? "";
        return channelWorkflows(id ? [id] : []);
      })();
    case "get_channels_workflows":
      return (async () => {
        const { channelWorkflows } = await import("@/shared/browser-backend/workflows");
        return channelWorkflows(
          (input as unknown as { channelIds?: string[] }).channelIds ?? [],
        );
      })();
    case "get_workflow":
      return (async () => {
        const { getWorkflow } = await import("@/shared/browser-backend/workflows");
        // The caller's own pubkey, so their cell wins over a same-id cell
        // another member published (F165).
        const me = await relayIdentity.pubkey().catch(() => null);
        return getWorkflow(
          (input as unknown as { workflowId?: string }).workflowId ?? "",
          me,
        );
      })();
    case "create_workflow":
      return (async () => {
        const { createWorkflow } = await import("@/shared/browser-backend/workflows");
        return createWorkflow(relayIdentity, input as never);
      })();
    case "update_workflow":
      return (async () => {
        const { updateWorkflow } = await import("@/shared/browser-backend/workflows");
        return updateWorkflow(relayIdentity, input as never);
      })();
    case "delete_workflow":
      return (async () => {
        const { deleteWorkflow } = await import("@/shared/browser-backend/workflows");
        return deleteWorkflow(relayIdentity, input as never);
      })();
    case "get_workflow_runs":
      return (async () => {
        const { workflowRuns } = await import("@/shared/browser-backend/workflows");
        return workflowRuns(input as never);
      })();
    case "get_run_approvals":
      return (async () => {
        const { runApprovals } = await import("@/shared/browser-backend/workflows");
        return runApprovals(input as never);
      })();
    case "grant_approval":
      return (async () => {
        const { decideApproval } = await import("@/shared/browser-backend/workflows");
        return decideApproval(relayIdentity, input as never, true);
      })();
    case "deny_approval":
      return (async () => {
        const { decideApproval } = await import("@/shared/browser-backend/workflows");
        return decideApproval(relayIdentity, input as never, false);
      })();
    case "trigger_workflow":
      return (async () => {
        const { triggerWorkflow } = await import("@/shared/browser-backend/workflows");
        return triggerWorkflow(relayIdentity, input as never);
      })();

    // Notes (pulse): NIP-01 kind 1

    case "publish_note":
      return (async () => {
        const { publishNote } = await import("@/shared/browser-backend/notes");
        return publishNote(relayIdentity, input as never);
      })();
    case "get_note":
      return (async () => {
        const { getNote } = await import("@/shared/browser-backend/notes");
        return getNote(input as never);
      })();
    case "get_user_notes":
      return (async () => {
        const { getUserNotes } = await import("@/shared/browser-backend/notes");
        return getUserNotes(input as never);
      })();
    case "get_global_notes":
      return (async () => {
        const { getGlobalNotes } = await import("@/shared/browser-backend/notes");
        return getGlobalNotes(input as never);
      })();
    case "get_liked_notes":
      return (async () => {
        const { getLikedNotes } = await import("@/shared/browser-backend/notes");
        return getLikedNotes(input as never);
      })();
    case "get_notes_timeline":
      return (async () => {
        const { getNotesTimeline } = await import("@/shared/browser-backend/reads");
        return getNotesTimeline(input as never);
      })();
    case "get_note_reactions":
      return (async () => {
        const { getNoteReactions } = await import("@/shared/browser-backend/reads");
        return getNoteReactions(input as never);
      })();

    /* --- Ordinary relay reads and writes; see `browser-backend/reads.ts` - */

    case "get_channel_messages_before":
      // The scroll-back half of the timeline: `get_channel_window` loads the
      // head, this loads what came before a cursor.
      return (async () => {
        const { getChannelMessagesBefore } = await import(
          "@/shared/browser-backend/reads"
        );
        return getChannelMessagesBefore(input as never);
      })();
    case "get_forum_thread":
      // One forum post and its replies. `get_forum_posts` above lists the roots.
      return (async () => {
        const { getForumThread } = await import("@/shared/browser-backend/reads");
        return getForumThread(input as never);
      })();
    case "update_profile_at_relay":
      // The avatar-only twin of `update_profile`, which reads the current kind 0
      // first so changing a picture does not erase the name beside it.
      return (async () => {
        const { updateProfileAtRelay } = await import(
          "@/shared/browser-backend/reads"
        );
        return updateProfileAtRelay(relayIdentity, input as never);
      })();
    case "update_persona_and_publish":
      // "Save and publish": the same write `update_persona` performs, plus the
      // answer to what the affordance actually promises — that the catalog the
      // workspace reads received it.
      return (async () => {
        const { updatePersonaAndPublish } = await import(
          "@/shared/browser-backend/reads"
        );
        return updatePersonaAndPublish(input as never);
      })();

    // Identity archive (NIP-IA)

    case "archive_identity":
      return (async () => {
        const { archiveIdentity } = await import(
          "@/shared/browser-backend/identityArchive"
        );
        return archiveIdentity(relayIdentity, input as never);
      })();
    case "unarchive_identity":
      return (async () => {
        const { unarchiveIdentity } = await import(
          "@/shared/browser-backend/identityArchive"
        );
        return unarchiveIdentity(relayIdentity, input as never);
      })();
    case "resolve_oa_owner":
      // The relay RECORDED the owner at creation, so this needs no NIP-OA claim
      // verification — a stronger answer than re-checking what a profile says.
      return (async () => {
        const { resolveOaOwner } = await import(
          "@/shared/browser-backend/identityArchive"
        );
        return resolveOaOwner(relayIdentity, input as never);
      })();

    // Agent administration

    case "set_persona_active":
      return (async () => {
        const { setPersonaActive } = await import("@/shared/browser-backend/agentAdmin");
        return setPersonaActive(input as never);
      })();
    case "get_agent_memory":
      return (async () => {
        const { getAgentMemory } = await import("@/shared/browser-backend/agentAdmin");
        return getAgentMemory(input as never);
      })();
    case "get_agent_config_surface":
      return (async () => {
        const { getAgentConfigSurface } = await import(
          "@/shared/browser-backend/agentAdmin"
        );
        return getAgentConfigSurface(input as never);
      })();
    case "put_agent_session_config":
      return (async () => {
        const { putAgentSessionConfig } = await import(
          "@/shared/browser-backend/agentAdmin"
        );
        return putAgentSessionConfig(input as never);
      })();

    // Community membership writes
    /* A moderator adds and removes; only an OPERATOR grants moderator status, */
    /* which is what makes giving moderators the other two safe.               */

    case "add_relay_member":
      return (async () => {
        const { addRelayMember } = await import("@/shared/browser-backend/agentAdmin");
        return addRelayMember(input as never);
      })();
    case "remove_relay_member":
      return (async () => {
        const { removeRelayMember } = await import("@/shared/browser-backend/agentAdmin");
        return removeRelayMember(input as never);
      })();
    case "change_relay_member_role":
      return (async () => {
        const { changeRelayMemberRole } = await import(
          "@/shared/browser-backend/agentAdmin"
        );
        return changeRelayMemberRole(input as never);
      })();

    // Cross-origin fetches, done by the RELAY
    /* A tab is blocked by CORS; the relay has egress. It returns only the     */
    /* title and the icon URL, never the document.                            */

    case "fetch_link_preview_title": {
      const url = (input as unknown as { url?: string }).url ?? "";
      if (!url) return Promise.resolve(null);
      return relayApiPost<{ title?: string | null }>("/link-preview", { url })
        .then((reply) => reply.title ?? null)
        // A preview is decoration: a workspace that cannot fetch one should
        // render the bare link, not an error where a title would be.
        .catch(() => null);
    }
    case "fetch_link_preview_metadata": {
      // Upstream's richer preview: title, site name, description, an inlined
      // og:image and favicon, fetched by the desktop process. The relay's
      // host-scoped route returns the title and an icon URL only, so the
      // metadata is the title and every image field says "none" — the same
      // shape the renderer already handles for a page with no image. The
      // icon URL is NOT placed in `faviconDataUrl`: that field is a data URL
      // by contract and the renderer has not been shown to accept a remote
      // one. `null` (no title) renders the bare link, as upstream.
      const href = (input as unknown as { href?: string }).href ?? "";
      if (!href) return Promise.resolve(null);
      return relayApiPost<{ title?: string | null }>("/link-preview", { url: href })
        .then((reply) => {
          const title = typeof reply.title === "string" ? reply.title.trim() : "";
          if (!title) return null;
          return {
            title,
            siteName: null,
            description: null,
            imageDataUrl: null,
            imageDomain: null,
            imageFetchState: "none" as const,
            imageRetryAfterMs: null,
            faviconDataUrl: null,
          };
        })
        .catch(() => null);
    }
    case "cancel_link_preview_metadata":
    case "release_link_preview_metadata":
      // Request-id bookkeeping for upstream's in-process fetch registry. The
      // fetch above is a single `relayApiPost` with no handle to cancel, and
      // the loader aborts its own side with an AbortController, so there is
      // no record to cancel or release.
      return Promise.resolve(null);
    case "publish_project_owner_announcement": {
      // A repository announcement signed AS the repo owner. For a repository
      // the viewer owns that is this tab's own key: sign and publish, and
      // report a publish failure in `publication_error` rather than throwing,
      // which is upstream's contract (the caller re-throws it). For a
      // managed-agent owner the desktop signs with the agent's key, which it
      // holds because it runs the agent — the same boundary as
      // `sign_project_pull_request*`, refused for the same reason.
      const announcement = (input as unknown as {
        input?: {
          targetOwner?: unknown;
          kind?: unknown;
          content?: unknown;
          createdAt?: unknown;
          tags?: unknown;
        };
      }).input;
      return (async () => {
        const me = (await publicKeyHex()).toLowerCase();
        const targetOwner =
          typeof announcement?.targetOwner === "string"
            ? announcement.targetOwner.toLowerCase()
            : "";
        if (targetOwner !== me) {
          throw new Error(
            "Only the buzz desktop app can publish for a repository owned by a managed agent: the " +
              "announcement has to be signed with that agent's key, which the desktop holds because " +
              "it runs the agent. This tab has just your own key.",
          );
        }
        if (typeof announcement?.kind !== "number") {
          throw new Error("a project owner announcement needs a kind");
        }
        const tags = Array.isArray(announcement.tags)
          ? (announcement.tags as unknown[]).filter(
              (tag): tag is string[] =>
                Array.isArray(tag) && tag.every((cell) => typeof cell === "string"),
            )
          : [];
        const signed = await sign({
          kind: announcement.kind,
          content: typeof announcement.content === "string" ? announcement.content : "",
          tags,
          created_at:
            typeof announcement.createdAt === "number" ? announcement.createdAt : undefined,
        });
        let publicationError: string | null = null;
        try {
          await publishSigned(signed, "Could not publish the repository announcement.");
        } catch (error) {
          publicationError = error instanceof Error ? error.message : String(error);
        }
        return { event: JSON.stringify(signed), publication_error: publicationError };
      })();
    }
    case "fetch_workspace_icon": {
      const url = (input as unknown as { url?: string }).url ?? "";
      if (!url) return Promise.resolve(null);
      return relayApiPost<{ icon?: string | null }>("/link-preview", { url })
        .then((reply) => reply.icon ?? null)
        .catch(() => null);
    }

    default:
      return undefined;
  }
}

/**
 * Answer one RAW-BODY command (Tauri's `InvokeBody::Raw`: the argument is the
 * bytes themselves, and the metadata rides in mirrored base64url headers), or
 * `undefined` when this build does not implement it. Reached from the bridge's
 * `invoke` when the payload is a `Uint8Array` — see `desktop-bridge.ts`.
 */
export function handleRawAppCommand(
  cmd: string,
  payload: Uint8Array,
  headers: Record<string, string>,
): Promise<unknown> | undefined {
  switch (cmd) {
    case "upload_media_bytes_raw":
      return (async () => {
        const { uploadMediaBytesRaw } = await import(
          "@/shared/browser-backend/media"
        );
        return uploadMediaBytesRaw(payload, headers);
      })();
    default:
      return undefined;
  }
}
