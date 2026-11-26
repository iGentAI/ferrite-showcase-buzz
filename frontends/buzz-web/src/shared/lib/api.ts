/**
 * The one HTTP chokepoint for the buzz relay.
 *
 * Every authenticated request carries a FRESHLY signed NIP-98 header: the relay
 * burns the auth event id after a single use, so a cached header is a guaranteed
 * 401. That is why signing happens inside these helpers and not at a call site
 * that might retry with the same header.
 *
 * The signed `payload` tag digests the exact request body, so the body is
 * serialized once and both the header and the request reuse that same string.
 */

import {
  BUZZ_AUTHORIZATION_HEADER,
  makeNip98AuthHeader,
} from "@/shared/lib/nip98";
import { apiUrl } from "@/shared/lib/relay-url";

const REQUEST_TIMEOUT_MS = 15_000;

/** A non-2xx reply from the relay, carrying its `error` string when present. */
class ApiError extends Error {
  readonly status: number;

  constructor(message: string, status: number) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

/** Longest non-JSON error body worth showing a user verbatim. */
const MAX_PLAIN_ERROR_CHARS = 200;

async function readBody(response: Response): Promise<unknown> {
  const text = await response.text();
  if (!text) return {};
  try {
    return JSON.parse(text) as unknown;
  } catch {
    // A proxy or gateway in front of the relay answers with HTML, not the
    // relay's `{error}` shape. Keep only short plain text; an HTML page in a
    // toast is noise, not a diagnostic.
    const trimmed = text.trim();
    const usable =
      trimmed.length <= MAX_PLAIN_ERROR_CHARS && !trimmed.includes("<");
    return usable ? { error: trimmed } : {};
  }
}

function errorMessage(body: unknown, status: number): string {
  if (body && typeof body === "object") {
    const { error, message } = body as { error?: unknown; message?: unknown };
    if (typeof error === "string" && error) return error;
    if (typeof message === "string" && message) return message;
  }
  return `HTTP ${status}`;
}

/**
 * POST JSON to an API path, e.g. `apiPost("/query", { channel, limit })`.
 *
 * `body` is serialized as-is, so a bare array is sent as a bare array — which
 * is what `/subscriptions` requires.
 */
export async function apiPost<T>(
  path: string,
  body: unknown,
): Promise<T> {
  const url = apiUrl(path);
  const payload = JSON.stringify(body ?? {});
  const response = await fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      [BUZZ_AUTHORIZATION_HEADER]: await makeNip98AuthHeader(url, "POST", {
        body: payload,
      }),
    },
    body: payload,
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  const parsed = await readBody(response);
  if (!response.ok) {
    throw new ApiError(errorMessage(parsed, response.status), response.status);
  }
  return parsed as T;
}

/**
 * GET a JSON API path, UNAUTHENTICATED.
 *
 * This helper sends no Authorization header, so it serves the relay's public
 * GETs only (`/relay-info`, `/agents`, `/agent-models`, `/media/limits`,
 * `/join-policy`). Calling a members-only read like `/members` or
 * `/archived-identities` through here is a 401 whose cause is invisible at the
 * call site; those callers sign the request themselves
 * (`browser-backend/commands.ts`'s `relayApiGet`,
 * `browser-backend/identityArchive.ts`).
 */
export async function apiGet<T>(path: string): Promise<T> {
  const response = await fetch(apiUrl(path), {
    method: "GET",
    headers: { Accept: "application/json" },
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  const parsed = await readBody(response);
  if (!response.ok) {
    throw new ApiError(errorMessage(parsed, response.status), response.status);
  }
  return parsed as T;
}
