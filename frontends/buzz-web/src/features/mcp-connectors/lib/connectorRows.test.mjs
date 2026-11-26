import assert from "node:assert/strict";
import test from "node:test";

import {
  OPERATOR_ONLY_REASON,
  catalogSummary,
  rowState,
} from "./connectorRows.ts";

/** A reachable, unconnected, provider-documented connector. */
function row(over = {}) {
  return {
    id: "linear",
    name: "Linear",
    egress: "mcp-egress-partners",
    reachable: true,
    connected: false,
    provenance: "documented",
    ...over,
  };
}

test("a member is offered no control that would be refused", () => {
  // The defect this prevents is a button that discovers its own 403. Every
  // reachable state must come back disabled for a non-operator, with the reason
  // present rather than left for the failure to explain.
  for (const variant of [row(), row({ connected: true })]) {
    const state = rowState(variant, { operator: false });
    assert.equal(state.action.enabled, false);
    assert.equal(state.action.reason, OPERATOR_ONLY_REASON);
  }
  // And an operator is offered them.
  assert.equal(rowState(row(), { operator: true }).action.enabled, true);
  assert.equal(
    rowState(row({ connected: true }), { operator: true }).action.enabled,
    true,
  );
});

test("the offered control matches the connector's state", () => {
  assert.equal(rowState(row(), { operator: true }).action.kind, "connect");
  assert.equal(
    rowState(row({ connected: true }), { operator: true }).action.kind,
    "disconnect",
  );
});

test("an unreachable connector says so, and connecting is not offered", () => {
  const state = rowState(row({ reachable: false }), { operator: true });
  assert.equal(state.status, "not granted");
  assert.equal(state.action.kind, "none");
  assert.equal(state.action.enabled, false);
  // The reason names the GRANT, because the fix is a spec edit rather than a
  // press of any button on this screen.
  assert.match(state.action.reason, /mcp-egress-partners/);
  assert.match(state.action.reason, /spec edit/);
});

test("a credential survives its grant being withdrawn, and stays removable", () => {
  // THE REGRESSION TEST. A grant can be taken out of the spec long after a
  // workspace authorized the connector, and the access token for somebody's
  // external account is still sitting in the store. Reporting only "not granted"
  // with no control made that the one state from which a live credential could
  // never be removed.
  const stale = rowState(row({ reachable: false, connected: true }), {
    operator: true,
  });
  assert.equal(stale.status, "not granted");
  assert.equal(stale.action.kind, "disconnect", "a stored credential must be removable");
  assert.equal(stale.action.enabled, true);
  // The condition is explained by `note`, NOT by the field that also means "no
  // action" — which is exactly why the two are separate.
  assert.match(stale.note, /A credential is still stored/);
  assert.match(stale.note, /mcp-egress-partners/);
  // Still operator-gated: forgetting a workspace credential is an operator act.
  const asMember = rowState(row({ reachable: false, connected: true }), {
    operator: false,
  });
  assert.equal(asMember.action.kind, "disconnect");
  assert.equal(asMember.action.enabled, false);
});

test("an undocumented endpoint is flagged in every state", () => {
  // Provenance is a property of the CATALOG ENTRY, so it cannot depend on whether
  // the connector happens to be connected or reachable.
  for (const over of [
    {},
    { connected: true },
    { reachable: false },
    { reachable: false, connected: true },
  ]) {
    const community = rowState(row({ ...over, provenance: "community" }), {
      operator: true,
    });
    assert.equal(community.unverifiedEndpoint, true, JSON.stringify(over));
    const documented = rowState(row({ ...over, provenance: "documented" }), {
      operator: true,
    });
    assert.equal(documented.unverifiedEndpoint, false, JSON.stringify(over));
  }
});

test("the summary counts against what is reachable, not the whole catalog", () => {
  // "3 of 17" would invite an operator to go connect fourteen more, when most of
  // them cannot be dialled by this deployment at all.
  const rows = [
    row({ id: "a", reachable: true, connected: true }),
    row({ id: "b", reachable: true, connected: false }),
    row({ id: "c", reachable: false, connected: false }),
    row({ id: "d", reachable: false, connected: false }),
  ];
  assert.equal(catalogSummary(rows), "1 of 2 reachable connectors connected.");

  assert.equal(
    catalogSummary([row({ reachable: false })]),
    "No connector is reachable from this deployment.",
  );
  assert.equal(
    catalogSummary([row({ id: "a" }), row({ id: "b" })]),
    "2 connectors available, none connected yet.",
  );
  // Singular reads correctly — a one-connector deployment is a real deployment.
  assert.equal(
    catalogSummary([row()]),
    "1 connector available, none connected yet.",
  );
  assert.equal(catalogSummary([]), "No connector is reachable from this deployment.");
});
