/**
 * What a catalog row offers, and when it offers nothing.
 *
 * Kept out of the card because a decision living inside JSX can only be
 * observed by driving a browser, and
 * this one is about AUTHORITY. A test that can say "a member is offered no
 * enabled control" in a millisecond is worth more than one that has to log in.
 *
 * Three refusals are kept apart because they need three different actions from
 * whoever reads them, and collapsing any two sends somebody to the wrong fix:
 *
 * * NOT CONNECTED — press Connect.
 * * NOT GRANTED — no egress handle covers this connector's host, so authorizing
 *   cannot help. Someone must edit the deploy spec and redeploy.
 * * UNAVAILABLE — the provider cannot work from a deployed app at all.
 */

/** Whether the provider documents the endpoint, or only a community list does. */
export type Provenance = "documented" | "community";

export type CatalogRow = {
  id: string;
  name: string;
  egress: string;
  reachable: boolean;
  connected: boolean;
  provenance: Provenance;
};

export type RowAction =
  | { kind: "connect"; enabled: boolean; reason: string | null }
  | { kind: "disconnect"; enabled: boolean; reason: string | null }
  | { kind: "none"; enabled: false; reason: string };

export type RowState = {
  /** A short label for the row's current condition. */
  status: "connected" | "not connected" | "not granted";
  action: RowAction;
  /**
   * An explanation of the row's CONDITION, independent of its control.
   *
   * Distinct from `action.reason`, which explains why a control is disabled, and
   * the separation is load-bearing rather than tidy: while one field carried
   * both, the only way to explain an unreachable connector was to give it the
   * action that means "nothing to offer" — which stranded the credential of a
   * connected connector whose grant had been withdrawn.
   */
  note: string | null;
  /** Whether to warn that the endpoint is not documented by its provider. */
  unverifiedEndpoint: boolean;
};

/**
 * Why a non-operator cannot press these.
 *
 * The wording states the CONSEQUENCE rather than the rule, because the rule on
 * its own reads as an arbitrary restriction: connecting grants every agent in
 * the workspace access to an external account, which is authority over another
 * system rather than a personal preference.
 */
export const OPERATOR_ONLY_REASON =
  "Connecting an MCP server is an operator action: it grants every agent in this workspace access to that external account.";

/** Why a connector cannot be reached however it is configured. */
function ungrantedReason(egress: string): string {
  return (
    `This deployment does not hold the \`${egress}\` egress grant, so the relay ` +
    `cannot dial this connector at all. Connecting would not help: the grant is ` +
    `part of the deploy spec, and adding one is a spec edit and a redeploy.`
  );
}

/**
 * The row's condition and the one control it should offer.
 *
 * Reachability is reported before connectedness, because a deployment whose spec
 * no longer grants a connector's host cannot dial it however it is configured,
 * and saying "connected" of it would be a claim about a token rather than about
 * anything that works.
 *
 * But an unreachable row is still REVOCABLE when a credential exists, and that
 * distinction is the whole of this function's care. A grant can be withdrawn from
 * the spec long after a workspace authorized the connector, leaving a stored
 * access token for somebody's external account; offering no control there would
 * make the one state that holds a live credential the one state from which it
 * cannot be removed. So reachability decides what CAN be reached, and
 * connectedness decides what can be forgotten — two questions, two answers.
 */
export function rowState(
  row: CatalogRow,
  viewer: { operator: boolean },
): RowState {
  const unverifiedEndpoint = row.provenance !== "documented";
  const operatorReason = viewer.operator ? null : OPERATOR_ONLY_REASON;
  if (!row.reachable) {
    return {
      status: "not granted",
      // A stale credential must remain removable. Connecting cannot help here,
      // so Connect is not offered — but Disconnect still is.
      action: row.connected
        ? { kind: "disconnect", enabled: viewer.operator, reason: operatorReason }
        : { kind: "none", enabled: false, reason: ungrantedReason(row.egress) },
      note: row.connected
        ? `${ungrantedReason(row.egress)} A credential is still stored for it, and ` +
          `disconnecting will forget it.`
        : null,
      unverifiedEndpoint,
    };
  }
  if (row.connected) {
    return {
      status: "connected",
      action: { kind: "disconnect", enabled: viewer.operator, reason: operatorReason },
      note: null,
      unverifiedEndpoint,
    };
  }
  return {
    status: "not connected",
    action: { kind: "connect", enabled: viewer.operator, reason: operatorReason },
    note: null,
    unverifiedEndpoint,
  };
}

/**
 * A one-line summary of the catalog, for the card's header.
 *
 * Counts CONNECTED against REACHABLE rather than against the whole catalog,
 * because "3 of 17" invites an operator to go connect fourteen more when most of
 * them are not dialable by this deployment in the first place.
 */
export function catalogSummary(rows: CatalogRow[]): string {
  const reachable = rows.filter((r) => r.reachable);
  const connected = reachable.filter((r) => r.connected);
  if (reachable.length === 0) {
    return "No connector is reachable from this deployment.";
  }
  if (connected.length === 0) {
    return `${reachable.length} connector${reachable.length === 1 ? "" : "s"} available, none connected yet.`;
  }
  return `${connected.length} of ${reachable.length} reachable connector${
    reachable.length === 1 ? "" : "s"
  } connected.`;
}
