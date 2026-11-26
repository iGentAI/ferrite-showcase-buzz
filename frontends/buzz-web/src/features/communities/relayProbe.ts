/**
 * Normalize a relay URL to ws(s):// form and probe reachability.
 */

/**
 * Accept a ws(s):// candidate only when it parses and carries no userinfo or
 * fragment. Mirrors `parseInviteInput` (shared/api/inviteHelpers.ts), which
 * reads the same input box: `https://relay.trusted.example@evil.example`
 * would otherwise be stored as a relay URL that leads with a trusted-looking
 * host while every connection goes to evil.example with the credentials.
 */
function acceptRelayCandidate(candidate: string): string | null {
  // Whitespace, control characters and backslashes are read differently by
  // WHATWG URL parsing and by the string checks below (a special scheme treats
  // `\` as `/`), so they are refused outright (finding_b24a79d54b005219).
  if (/[\s\\#\u0000-\u001f\u007f]/.test(candidate)) return null;
  const afterScheme = candidate.slice(candidate.indexOf("://") + 3);
  // `wss:///@evil.example`: WHATWG skips the extra slash, so the authority
  // the string check sees must not start with one.
  if (afterScheme.startsWith("/")) return null;
  const authority = afterScheme.split(/[/?]/, 1)[0];
  if (!authority || authority.includes("@")) return null;
  let parsed: URL;
  try {
    parsed = new URL(candidate);
  } catch {
    return null;
  }
  if (parsed.protocol !== "ws:" && parsed.protocol !== "wss:") return null;
  if (parsed.username || parsed.password || parsed.hash || !parsed.hostname) return null;
  // The canonical form the parser resolved, never the raw string, so whatever
  // stored and displayed is exactly what a connection reaches.
  const path = parsed.pathname === "/" ? "" : parsed.pathname;
  return `${parsed.protocol}//${parsed.host}${path}${parsed.search}`;
}

/** Normalize a user-entered relay URL to ws(s):// form. Returns null if invalid. */
export function normalizeRelayUrl(input: string): string | null {
  const raw = input.trim().replace(/\/+$/, "");
  if (!raw) return null;
  // Schemes are case-insensitive: `WSS://` is `wss://`, not a host to prefix.
  const scheme = /^[a-z][a-z0-9+.-]*(?=:\/\/)/i.exec(raw);
  const trimmed = scheme ? scheme[0].toLowerCase() + raw.slice(scheme[0].length) : raw;

  // Already ws(s)://
  if (trimmed.startsWith("wss://") || trimmed.startsWith("ws://")) {
    return acceptRelayCandidate(trimmed);
  }

  // Convert https → wss, http → ws. An explicit http:// keeps the cleartext
  // scheme the user typed (the same mapping parseInviteInput applies), so a
  // local or LAN relay stays reachable; a scheme-less host defaults to wss.
  if (trimmed.startsWith("https://")) {
    return acceptRelayCandidate(`wss://${trimmed.slice(8)}`);
  }
  if (trimmed.startsWith("http://")) {
    return acceptRelayCandidate(`ws://${trimmed.slice(7)}`);
  }

  // Match the legacy add/edit community forms: a scheme-less host is assumed
  // to be a secure relay. Validation still rejects whitespace and malformed
  // values instead of blindly persisting the prefixed string.
  if (!trimmed.includes("://")) {
    return acceptRelayCandidate(`wss://${trimmed}`);
  }

  return null;
}

/**
 * Probe whether a WebSocket relay is reachable. Opens a connection with a
 * timeout; resolves `true` if the socket opens, `false` on timeout/error.
 * `cancel()` closes the socket and resolves `false`, so a caller awaiting
 * the promise always continues. The socket is always closed before the
 * promise settles.
 */
export function probeRelayReachable(
  wsUrl: string,
  timeoutMs = 4000,
): { promise: Promise<boolean>; cancel: () => void } {
  let socket: WebSocket | null = null;
  let timeoutId: ReturnType<typeof setTimeout> | null = null;
  let settled = false;
  let settle: (result: boolean) => void = () => {};

  const promise = new Promise<boolean>((resolve) => {
    settle = (result: boolean) => {
      if (settled) return;
      settled = true;
      if (timeoutId !== null) clearTimeout(timeoutId);
      try {
        socket?.close();
      } catch {
        /* ignore */
      }
      socket = null;
      resolve(result);
    };

    try {
      socket = new WebSocket(wsUrl);
      socket.onopen = () => settle(true);
      socket.onerror = () => settle(false);
      socket.onclose = () => settle(false);
      timeoutId = setTimeout(() => settle(false), timeoutMs);
    } catch {
      settle(false);
    }
  });

  return {
    promise,
    cancel() {
      settle(false);
    },
  };
}
