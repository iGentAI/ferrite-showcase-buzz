/**
 * The MCP connector catalog, read and acted on from a browser tab.
 *
 * Every call here is authenticated as THIS TAB's identity, which is what makes
 * the operator-gated half work at all: `caller_is_operator` compares the request
 * signer's PUBKEY against the workspace's operator set, so the request has to be
 * signed by the key the operator actually logged in with. `makeNip98AuthHeader`
 * does that — NIP-07 when an extension is present, otherwise the same key the
 * socket authenticates as.
 *
 * The token exchange itself never happens here. `authorize` returns a URL and the
 * relay keeps the PKCE verifier, the OAuth `state` and the eventual access token;
 * a tab holds none of them. That division is the point of the design rather than
 * an implementation detail — a browser that could complete the exchange would be
 * a browser holding a credential for somebody else's account.
 */
import {
  BUZZ_AUTHORIZATION_HEADER,
  makeNip98AuthHeader,
} from "@/shared/lib/nip98";
import { apiPost } from "@/shared/lib/api";
import { apiUrl } from "@/shared/lib/relay-url";

/** Whether the PROVIDER documents an endpoint, or only a community list does. */
export type Provenance = "documented" | "community";

/** One catalog entry, as the relay reports it. */
export type ConnectorRow = {
  id: string;
  name: string;
  endpoint: string;
  summary: string;
  provenance: Provenance;
  /** The egress grant that covers this connector's host. */
  egress: string;
  /** Whether this DEPLOYMENT holds that grant. */
  reachable: boolean;
  /** Whether this WORKSPACE holds a credential for it. */
  connected: boolean;
};

/** A provider that cannot work from a deployed app, and the reason. */
export type UnavailableRow = { id: string; name: string; reason: string };

export type ConnectorCatalog = {
  /** Whether the CALLER may connect or disconnect. */
  operator: boolean;
  /** Whether this deployment holds any catalog egress grant at all. */
  egressAvailable: boolean;
  connectors: ConnectorRow[];
  unavailable: UnavailableRow[];
};

function isProvenance(value: unknown): value is Provenance {
  return value === "documented" || value === "community";
}

/**
 * Read the catalog.
 *
 * A signed GET, because the route is members-only: `apiGet` sends no
 * Authorization header at all and would answer 401 with nothing at the call site
 * explaining it — a trap this client has already fallen into once, on
 * `/archived-identities`.
 *
 * Unknown provenance is normalised to `"community"` rather than trusted: this
 * value decides whether the UI tells an operator the endpoint is unverified, so
 * the failure has to be toward MORE caution, not less.
 */
export async function listConnectors(): Promise<ConnectorCatalog> {
  const url = apiUrl("/mcp/connectors");
  const response = await fetch(url, {
    headers: {
      Accept: "application/json",
      [BUZZ_AUTHORIZATION_HEADER]: await makeNip98AuthHeader(url, "GET"),
    },
    signal: AbortSignal.timeout(20_000),
  });
  if (!response.ok) {
    throw new Error(
      `The connector catalog could not be read (http ${response.status}).`,
    );
  }
  const body = (await response.json()) as {
    operator?: unknown;
    egress_available?: unknown;
    connectors?: unknown;
    unavailable?: unknown;
  };
  const rows = Array.isArray(body.connectors) ? body.connectors : [];
  const gone = Array.isArray(body.unavailable) ? body.unavailable : [];
  return {
    operator: body.operator === true,
    egressAvailable: body.egress_available === true,
    connectors: rows.flatMap((raw): ConnectorRow[] => {
      const row = raw as Record<string, unknown>;
      const id = typeof row.id === "string" ? row.id : "";
      if (!id) return [];
      return [
        {
          id,
          name: typeof row.name === "string" ? row.name : id,
          endpoint: typeof row.endpoint === "string" ? row.endpoint : "",
          summary: typeof row.summary === "string" ? row.summary : "",
          provenance: isProvenance(row.provenance) ? row.provenance : "community",
          egress: typeof row.egress === "string" ? row.egress : "",
          reachable: row.reachable === true,
          connected: row.connected === true,
        },
      ];
    }),
    unavailable: gone.flatMap((raw): UnavailableRow[] => {
      const row = raw as Record<string, unknown>;
      const id = typeof row.id === "string" ? row.id : "";
      if (!id) return [];
      return [
        {
          id,
          name: typeof row.name === "string" ? row.name : id,
          reason: typeof row.reason === "string" ? row.reason : "",
        },
      ];
    }),
  };
}

/**
 * Begin authorization. Operator only, enforced by the relay.
 *
 * Answers the URL the person's browser must visit. The relay has already
 * discovered the connector's protected-resource metadata and its authorization
 * server's, derived a PKCE verifier and a single-use `state` under its own
 * secret, and recorded the pending flow — so this URL is only useful once, and
 * only to finish the flow it belongs to.
 */
/**
 * Validate an OAuth authorization URL returned by the relay.
 *
 * Requires an `https:` scheme with no embedded credentials, backslashes, or
 * whitespace so an untrusted or tampered response cannot navigate a same-origin
 * `about:blank` popup to `javascript:` or a cleartext/local protocol handler.
 */
export function validateAuthorizationUrl(raw: unknown): string {
  if (typeof raw !== "string" || !raw) {
    throw new Error("The relay did not return an authorization URL.");
  }
  const trimmed = raw.trim();
  if (!trimmed || /[\s\\]/.test(trimmed)) {
    throw new Error("The relay returned an invalid authorization URL.");
  }
  const afterScheme = trimmed.includes("://")
    ? trimmed.slice(trimmed.indexOf("://") + 3)
    : "";
  const authority = afterScheme.split(/[/?#]/, 1)[0] ?? "";
  if (authority.includes("@")) {
    throw new Error("The relay returned an invalid authorization URL.");
  }
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    throw new Error("The relay returned an invalid authorization URL.");
  }
  if (
    parsed.protocol !== "https:" ||
    parsed.username ||
    parsed.password ||
    !parsed.hostname
  ) {
    throw new Error("The relay returned an invalid authorization URL.");
  }
  return parsed.toString();
}

export async function authorizeConnector(
  id: string,
): Promise<{ authorizationUrl: string; issuer: string; scopes: string[] }> {
  const reply = await apiPost<{
    authorization_url?: unknown;
    issuer?: unknown;
    scopes?: unknown;
  }>("/mcp/authorize", { connector: id });
  const authorizationUrl = validateAuthorizationUrl(reply.authorization_url);
  return {
    authorizationUrl,
    issuer: typeof reply.issuer === "string" ? reply.issuer : "",
    scopes: Array.isArray(reply.scopes)
      ? reply.scopes.filter((s): s is string => typeof s === "string")
      : [],
  };
}

/**
 * Forget a connector's credential. Operator only, enforced by the relay.
 *
 * `wasConnected` is reported rather than assumed, so disconnecting something
 * already disconnected reads as the no-op it is instead of as a success that
 * silently did nothing.
 */
export async function disconnectConnector(
  id: string,
): Promise<{ wasConnected: boolean }> {
  const reply = await apiPost<{ was_connected?: unknown }>("/mcp/disconnect", {
    connector: id,
  });
  return { wasConnected: reply.was_connected === true };
}

/**
 * List a connected server's tools. Any member may call this.
 *
 * Used to show what a connection actually bought — "connected" on its own is a
 * claim about a stored token, while a tool list is evidence the far side answers.
 */
export async function connectorTools(id: string): Promise<unknown> {
  const reply = await apiPost<{ result?: unknown }>("/mcp/tools", {
    connector: id,
  });
  return reply.result ?? null;
}
