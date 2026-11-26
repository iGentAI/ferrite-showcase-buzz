import {
  canonicalizeRelayWsUrl,
  relayHttpFromWs,
} from "@/shared/api/inviteHelpers";
import {
  getRelayHttpUrl,
  invokeTauri,
  signRelayEvent,
} from "@/shared/api/tauri";
import { randomUuid, sha256Hex } from "@/shared/lib/insecureContextCrypto";
import { BUZZ_AUTHORIZATION_HEADER } from "@/shared/lib/nip98";

// Relay invite data layer. Both endpoints are NIP-98-authed HTTP POSTs
// (mirrors the read path in moderation.ts, plus the payload tag the relay
// requires for signed POST bodies):
//
// - POST /api/invites        — mint a code (relay checks owner/admin role)
// - POST /api/invites/claim  — claim a code, signed by the *joining* key.
//   This one targets an arbitrary relay (the invite's relay, not necessarily
//   the active community), so the claim helper takes an explicit ws URL.

const NIP98_KIND = 27235;

// Bound invite requests so an unreachable relay surfaces as an error in the
// invite-loading UI within seconds instead of hanging for the OS-level
// connect timeout (a minute or more on macOS). Every request in this module
// goes through `boundedFetch`, so a new endpoint cannot be added unbounded
// (F410).
const INVITE_REQUEST_TIMEOUT_MS = 15_000;

function boundedFetch(url: string, init: RequestInit = {}): Promise<Response> {
  return fetch(url, {
    redirect: "error",
    ...init,
    signal: AbortSignal.timeout(INVITE_REQUEST_TIMEOUT_MS),
  });
}

function validateHttpBase(rawBase: string): { trimmedBase: string; parsed: URL } {
  const trimmed = rawBase.trim().replace(/\/+$/, "");
  if (!trimmed || /[\s\\?#]/.test(trimmed)) {
    throw new Error(`Invalid relay HTTP base URL: ${rawBase}`);
  }
  const afterScheme = trimmed.slice(trimmed.indexOf("://") + 3);
  const authority = afterScheme.split("/", 1)[0] ?? "";
  if (!authority || authority.includes("@")) {
    throw new Error(`Invalid relay HTTP base URL: ${rawBase}`);
  }
  const parsed = new URL(trimmed);
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error(`Invalid relay HTTP base URL: ${rawBase}`);
  }
  if (parsed.username || parsed.password || parsed.search || parsed.hash) {
    throw new Error(`Invalid relay HTTP base URL: ${rawBase}`);
  }
  return { trimmedBase: trimmed, parsed };
}

export function inviteTargetUrl(rawBase: string, path: string): string {
  if (
    typeof path !== "string" ||
    !path.startsWith("/") ||
    path.startsWith("//") ||
    /[\s\\?#]/.test(path) ||
    path.split("/").some((seg) => seg === "." || seg === "..")
  ) {
    throw new Error(`Invalid invite endpoint path: ${path}`);
  }
  const { trimmedBase, parsed: base } = validateHttpBase(rawBase);
  const baseWithSlash = `${base.origin}${base.pathname.replace(/\/+$/, "")}/`;
  const target = new URL(path.slice(1), baseWithSlash);
  if (
    target.origin !== base.origin ||
    target.username ||
    target.password ||
    target.search ||
    target.hash ||
    target.href !== `${trimmedBase}${path}`
  ) {
    throw new Error(`Invite target escaped validated origin: ${path}`);
  }
  return target.href;
}

function synthesizeMintedInviteUrl(parsedBase: URL, code: string): string {
  const mountPath = parsedBase.pathname
    .replace(/\/+$/, "")
    .replace(/\/api$/, "");
  if (!mountPath || mountPath === "/") {
    return `${parsedBase.origin}/invite/${encodeURIComponent(code)}`;
  }
  const wsProtocol = parsedBase.protocol === "https:" ? "wss:" : "ws:";
  const relayWs = `${wsProtocol}//${parsedBase.host}${mountPath}/`;
  return `buzz://join?relay=${encodeURIComponent(relayWs)}&code=${encodeURIComponent(code)}`;
}

export type MintedInvite = {
  code: string;
  expiresAt: number;
  url: string;
  maxUses: number | null;
  usesRemaining: number | null;
};

export type JoinPolicy = {
  termsMarkdown?: string;
  privacyMarkdown?: string;
  ageAttestationRequired: boolean;
  version: string;
};

export type ClaimResult = {
  status: "joined" | "already_member";
  communityId: string;
  host: string;
  role: string;
};

/**
 * Build the NIP-98 `Authorization` header for a POST with a body.
 *
 * The relay requires a `payload` tag carrying sha256(body) for signed POSTs
 * (api/invites.rs passes `require_payload: true`), and verifies the `u` tag
 * against the exact request URL — so the caller finalizes both before signing.
 */
async function nip98PostHeader(url: string, body: string): Promise<string> {
  const authEvent = await signRelayEvent({
    kind: NIP98_KIND,
    content: "",
    tags: [
      ["u", url],
      ["method", "POST"],
      ["payload", await sha256Hex(new TextEncoder().encode(body))],
      ["nonce", randomUuid()],
    ],
  });
  // NIP-98 events carry empty content and ASCII-only tags, so btoa is safe here.
  return `Nostr ${btoa(JSON.stringify(authEvent))}`;
}

async function invitePost<T>(
  httpBase: string,
  path: string,
  body: string,
): Promise<T> {
  const url = inviteTargetUrl(httpBase, path);
  const authorization = await nip98PostHeader(url, body);
  const response = await boundedFetch(url, {
    method: "POST",
    headers: {
      [BUZZ_AUTHORIZATION_HEADER]: authorization,
      "Content-Type": "application/json",
    },
    body,
  });
  const json = (await response.json().catch(() => ({}))) as Record<
    string,
    unknown
  >;
  if (!response.ok) {
    const message =
      typeof json.error === "string" ? json.error : `HTTP ${response.status}`;
    throw new Error(message);
  }
  return json as T;
}

/** Absolute URL of a relay-hosted policy document page (system-browser target). */
export function joinPolicyDocumentUrl(
  relayWsUrl: string,
  document: "terms" | "privacy",
): string {
  if (document !== "terms" && document !== "privacy") {
    throw new Error(`Invalid join policy document: ${String(document)}`);
  }
  const base = relayHttpFromWs(relayWsUrl);
  return inviteTargetUrl(base, `/api/join-policy/${document}`);
}

/** Whether a normalized relay URL is complete enough for background policy discovery. */
export function isJoinPolicyDiscoveryCandidate(relayWsUrl: string): boolean {
  const canonical = canonicalizeRelayWsUrl(relayWsUrl);
  if (!canonical) return false;
  try {
    const url = new URL(canonical);
    return (
      url.hostname === "localhost" ||
      url.hostname === "127.0.0.1" ||
      url.hostname.includes(".")
    );
  } catch {
    return false;
  }
}

/** Fetch relay-hosted policy content for any join surface. */
export async function getJoinPolicy(
  relayWsUrl: string,
  transport: "native" | "webview",
): Promise<JoinPolicy | null> {
  type RawJoinPolicy = {
    terms_markdown?: string;
    privacy_markdown?: string;
    age_attestation_required: boolean;
    version: string;
  };
  const canonicalWs = canonicalizeRelayWsUrl(relayWsUrl);
  if (!canonicalWs) {
    throw new Error(`Invalid relay WebSocket URL: ${relayWsUrl}`);
  }
  const base = relayHttpFromWs(canonicalWs);

  const fetchOverHttp = async (): Promise<RawJoinPolicy | null> => {
    const response = await boundedFetch(
      inviteTargetUrl(base, "/api/join-policy"),
    );
    // Relays predating join-policy support have no configured policy.
    if (response.status === 404) return null;
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return (
      (
        (await response.json()) as {
          policy?: RawJoinPolicy;
        }
      ).policy ?? null
    );
  };

  let raw: RawJoinPolicy | null;
  if (transport === "native") {
    try {
      raw = await invokeTauri<RawJoinPolicy | null>("fetch_join_policy", {
        relayUrl: canonicalWs,
      });
    } catch (error) {
      if (error instanceof Error && error.name === "DesktopUnavailableError") {
        raw = await fetchOverHttp();
      } else {
        throw error;
      }
    }
  } else {
    raw = await fetchOverHttp();
  }
  return raw
    ? {
        termsMarkdown: raw.terms_markdown,
        privacyMarkdown: raw.privacy_markdown,
        ageAttestationRequired: raw.age_attestation_required,
        version: raw.version,
      }
    : null;
}

/** Accept the current join policy for an invite and receive a bound receipt. */
export async function acceptJoinPolicy(
  relayWsUrl: string,
  code: string,
  policyVersion: string,
  ageConfirmed: boolean,
): Promise<string> {
  const base = relayHttpFromWs(relayWsUrl);
  const response = await boundedFetch(
    inviteTargetUrl(base, "/api/invites/accept-policy"),
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        code,
        policy_version: policyVersion,
        age_confirmed: ageConfirmed,
      }),
    },
  );
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return ((await response.json()) as { receipt: string }).receipt;
}

/** Mint an invite code on the active community's relay (owner/admin only). */
export async function mintInvite(options?: {
  ttlSecs?: number;
  maxUses?: number | null;
}): Promise<MintedInvite> {
  const base = await getRelayHttpUrl();
  const { trimmedBase, parsed } = validateHttpBase(base);
  const invitePath = trimmedBase.endsWith("/api") ? "/invites" : "/api/invites";
  const payload: Record<string, unknown> = {};
  if (options?.ttlSecs != null) payload.ttl_secs = options.ttlSecs;
  if (options?.maxUses != null) payload.max_uses = options.maxUses;
  const body = JSON.stringify(payload);
  const raw = await invitePost<{
    code: string;
    expires_at: number;
    url?: string;
    max_uses?: number | null;
    uses_remaining?: number | null;
  }>(trimmedBase, invitePath, body);
  return {
    code: raw.code,
    expiresAt: raw.expires_at,
    url:
      typeof raw.url === "string" && raw.url.length > 0
        ? raw.url
        : synthesizeMintedInviteUrl(parsed, raw.code),
    maxUses: raw.max_uses ?? null,
    usesRemaining: raw.uses_remaining ?? null,
  };
}

/**
 * Claim an invite code against `relayWsUrl` (the invite's relay — not
 * necessarily the active community), signed by this app's identity key.
 */
export async function claimInvite(
  relayWsUrl: string,
  code: string,
  policyReceipt?: string,
): Promise<ClaimResult> {
  const base = relayHttpFromWs(relayWsUrl);
  const body = JSON.stringify({ code, policy_receipt: policyReceipt });
  const raw = await invitePost<{
    status: "joined" | "already_member";
    community_id: string;
    host: string;
    role: string;
  }>(base, "/api/invites/claim", body);
  return {
    status: raw.status,
    communityId: raw.community_id,
    host: raw.host,
    role: raw.role,
  };
}
