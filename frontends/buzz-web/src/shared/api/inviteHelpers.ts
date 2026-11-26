export const INVITE_EXPIRED_ERROR = "invite_expired";
export const INVITE_EXHAUSTED_ERROR = "invite_exhausted";

/**
 * Parsed invite — either a full (relay + code) or bare-code form.
 *
 * URL inputs (`https://`, `http://`, `buzz://join`) always carry a
 * `relayWsUrl` (already normalised to `ws(s)://`).  A bare code (no scheme,
 * no slashes) omits it — the caller decides which relay to target.
 */
export type ParsedInvite =
  | { relayWsUrl: string; code: string }
  | { code: string };

/**
 * Validate and canonicalize a relay WebSocket URL (`ws:` or `wss:`).
 *
 * Rejects userinfo (`@`), query strings (`?`), fragments (`#`), backslashes
 * (`\`), dot/percent/empty path segments, and `/api` subpaths so downstream
 * `/api/...` endpoint construction cannot be redirected or absorbed into a
 * query string or fragment.
 */
export function canonicalizeRelayWsUrl(raw: string): string | null {
  const trimmed = raw.trim();
  if (!trimmed || /[\s\\?#]/.test(trimmed)) return null;
  if (!trimmed.startsWith("ws://") && !trimmed.startsWith("wss://")) {
    return null;
  }
  const afterScheme = trimmed.slice(trimmed.indexOf("://") + 3);
  const slashIndex = afterScheme.indexOf("/");
  const authority = slashIndex >= 0 ? afterScheme.slice(0, slashIndex) : afterScheme;
  if (!authority || authority.includes("@")) return null;

  const rawPath = slashIndex >= 0 ? afterScheme.slice(slashIndex) : "";
  if (rawPath.includes("//") || rawPath.includes("%")) return null;
  const rawSegments = rawPath.split("/").filter((seg) => seg.length > 0);
  if (
    rawSegments.some(
      (seg) => seg === "." || seg === ".." || seg.toLowerCase() === "api",
    )
  ) {
    return null;
  }

  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return null;
  }
  if (parsed.protocol !== "ws:" && parsed.protocol !== "wss:") return null;
  if (parsed.username || parsed.password || parsed.search || parsed.hash) {
    return null;
  }
  const hasTrailingSlash = slashIndex >= 0 && trimmed.endsWith("/");
  const cleanPath =
    parsed.pathname === "/"
      ? hasTrailingSlash
        ? "/"
        : ""
      : parsed.pathname;
  return `${parsed.protocol}//${parsed.host}${cleanPath}`;
}

/**
 * Parse an invite input into a structured form.
 *
 * Accepted input forms:
 *  - `https://<relay>/invite/<code>` → `{ relayWsUrl: "wss://<relay>", code }`
 *  - `http://<relay>/invite/<code>`  → `{ relayWsUrl: "ws://<relay>", code }`
 *  - `buzz://join?relay=<wsUrl>&code=<code>` → `{ relayWsUrl, code }`
 *  - bare code (no `://`, no `/`)    → `{ code }`
 *
 * Returns `null` for empty input or inputs that don't match any form.
 */
export function parseInviteInput(input: string): ParsedInvite | null {
  const trimmed = input.trim();
  if (!trimmed) return null;

  // Try URL-form parse first.
  try {
    if (trimmed.includes("\\") || trimmed.includes("#")) return null;
    const url = new URL(trimmed);

    // buzz://join?relay=...&code=...
    // Non-special schemes put the authority in `host`, not `pathname`.
    if (url.protocol === "buzz:") {
      if (url.host !== "join") return null;
      const buzzAuthority =
        trimmed.slice("buzz://".length).split(/[/?]/, 1)[0] ?? "";
      if (buzzAuthority.includes("@")) return null;
      if (url.username || url.password || url.hash) return null;
      const relay = url.searchParams.get("relay");
      const code = url.searchParams.get("code");
      if (!relay || !code) return null;
      const canonicalRelay = canonicalizeRelayWsUrl(relay);
      if (!canonicalRelay) return null;
      return { relayWsUrl: canonicalRelay, code };
    }

    // https(s)://<relay>/invite/<code>
    if (url.protocol === "https:" || url.protocol === "http:") {
      const httpAuthority =
        trimmed.slice(trimmed.indexOf("://") + 3).split(/[/?]/, 1)[0] ?? "";
      if (httpAuthority.includes("@")) return null;
      if (url.username || url.password || url.search || url.hash) return null;
      // pathname must be /invite/<code> with optional single trailing slash
      const match = url.pathname.match(/^\/invite\/([^/]+)\/?$/);
      if (!match?.[1]) return null;
      const code = decodeURIComponent(match[1]);
      // Convert scheme: https → wss, http → ws. url.host already includes port.
      const relayWsUrl =
        url.protocol === "https:" ? `wss://${url.host}` : `ws://${url.host}`;
      return { relayWsUrl, code };
    }

    // ws/wss or any other scheme — not an invite URL.
    return null;
  } catch {
    // Not a URL — fall through to bare-code check.
  }

  // Bare code: no scheme, no slashes.
  if (trimmed.includes("://") || trimmed.includes("/")) return null;
  return { code: trimmed };
}

/** Convert a ws(s) relay URL to its http(s) equivalent. */
export function relayHttpFromWs(wsUrl: string): string {
  const trimmed = wsUrl.trim();
  if (!trimmed.startsWith("ws://") && !trimmed.startsWith("wss://")) {
    throw new Error(`Expected ws:// or wss:// relay URL, got: ${wsUrl}`);
  }
  const canonical = canonicalizeRelayWsUrl(trimmed);
  if (!canonical) {
    throw new Error(`Invalid relay WebSocket URL: ${wsUrl}`);
  }
  if (canonical.startsWith("wss://")) return `https://${canonical.slice(6)}`;
  return `http://${canonical.slice(5)}`;
}

export function inviteErrorMessage(error: unknown): string {
  return error instanceof Error ? error.message : `${error}`;
}

export function isInviteExpiredError(error: unknown): boolean {
  return inviteErrorMessage(error) === INVITE_EXPIRED_ERROR;
}

export function isInviteExhaustedError(error: unknown): boolean {
  return inviteErrorMessage(error) === INVITE_EXHAUSTED_ERROR;
}
