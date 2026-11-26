// The browser build has no local archive, so the two startup archive hooks must
// not attempt their merge. Before this gate both called the seam's refused
// `merge_save_subscription_kinds` on every AppShell mount and logged a
// DesktopUnavailableError each time, because a failure persists no marker.
import assert from "node:assert/strict";
import test from "node:test";

import { agentMetricArchiveSeedApplies } from "./useAgentMetricArchiveSeed.ts";
import { startReconciliation } from "./useObserverArchiveSeed.ts";

function deps(archiveAvailable) {
  const calls = { merge: 0, ready: [] };
  return {
    calls,
    mergeSaveSubscriptionKinds: async () => {
      calls.merge += 1;
      throw new Error("DesktopUnavailableError: no archive");
    },
    readExplicitChoice: () => "unset",
    hasExplicitChoice: () => false,
    setExplicitChoice: () => {},
    ...(archiveAvailable === undefined ? {} : { archiveAvailable: () => archiveAvailable }),
  };
}

async function warningsDuring(body) {
  const seen = [];
  const original = console.warn;
  console.warn = (...args) => seen.push(args.map(String).join(" "));
  try {
    await body();
    await new Promise((resolve) => setTimeout(resolve, 0));
  } finally {
    console.warn = original;
  }
  return seen;
}

test("the agent-metric seed does not apply to a build without an archive", () => {
  assert.equal(agentMetricArchiveSeedApplies(deps(false)), false);
  assert.equal(agentMetricArchiveSeedApplies(deps(true)), true);
  // Injected deps that predate the flag keep upstream's behaviour.
  assert.equal(agentMetricArchiveSeedApplies(deps(undefined)), true);
});

test("observer reconciliation without an archive never merges, warns or reports ready", async () => {
  const d = deps(false);
  const warnings = await warningsDuring(async () => {
    for (let mount = 0; mount < 3; mount += 1) {
      const cancel = startReconciliation("pk1", d, (pubkey) => d.calls.ready.push(pubkey));
      cancel();
    }
  });
  assert.equal(d.calls.merge, 0);
  assert.deepEqual(d.calls.ready, []);
  assert.deepEqual(warnings, []);
});

test("observer reconciliation with an archive still attempts the merge", async () => {
  const d = deps(true);
  const warnings = await warningsDuring(async () => {
    startReconciliation("pk1", d, (pubkey) => d.calls.ready.push(pubkey));
  });
  assert.equal(d.calls.merge, 1);
  assert.equal(warnings.length, 1, "a real failure in a build that HAS an archive still warns");
});
