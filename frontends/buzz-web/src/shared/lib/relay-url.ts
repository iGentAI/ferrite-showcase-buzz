/**
 * URL construction for the buzz relay.
 *
 * The relay's HTTP API lives under `/buzz/api`. The SPA is served by the same
 * relay under `/buzz/ui/`, so the default origin is the page's own.
 *
 * `VITE_RELAY_ORIGIN` exists for local development against a relay on another
 * origin; it must be an absolute origin (e.g. `http://127.0.0.1:8787`).
 */

const API_PREFIX = "/buzz/api";

/** Absolute origin of the relay (no trailing slash). */
function relayOrigin(): string {
  const configured = import.meta.env?.VITE_RELAY_ORIGIN;
  if (configured) return configured.replace(/\/+$/, "");
  return window.location.origin;
}

/**
 * Absolute URL for an API path, e.g. `apiUrl("/query")` →
 * `https://host/buzz/api/query`.
 *
 * The URL must be absolute and byte-identical to what the request uses: the
 * relay compares it against the NIP-98 `u` tag.
 */
export function apiUrl(path: string): string {
  const suffix = path.startsWith("/") ? path : `/${path}`;
  return `${relayOrigin()}${API_PREFIX}${suffix}`;
}
