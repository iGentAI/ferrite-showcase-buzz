import assert from "node:assert/strict";
import test from "node:test";

import {
  isRelayManagedHarness,
  isRelayManagedModelSelectableHarness,
} from "./relayManagedHarness.ts";

test("server-owned harnesses do not require a desktop process", () => {
  for (const id of ["ferrite-cloud", "fabric", "a2a", " Fabric "]) {
    assert.equal(isRelayManagedHarness(id), true, id);
  }
  for (const id of ["claude", "codex", "goose", "custom"] ) {
    assert.equal(isRelayManagedHarness(id), false, id);
  }
});

test("only Ferrite Cloud exposes a server-owned model picker", () => {
  for (const id of ["ferrite-cloud", " Ferrite-Cloud "]) {
    assert.equal(isRelayManagedModelSelectableHarness(id), true, id);
  }
  for (const id of ["fabric", "a2a", "claude", "custom"]) {
    assert.equal(isRelayManagedModelSelectableHarness(id), false, id);
  }
});
