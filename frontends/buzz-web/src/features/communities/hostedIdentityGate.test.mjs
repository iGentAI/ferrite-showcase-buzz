/**
 * Regression tests for F367/F369 (the hosted identity gate failed open when
 * the device key was unknown) and F368/F370 (switching identity deleted the
 * account's binding before the replacement bind, with no pre-check).
 */
import assert from "node:assert/strict";
import test from "node:test";

import {
  evaluateHostedIdentityGate,
  settleFailedIdentitySwitch,
  switchHostedIdentityErrorMessage,
  switchHostedIdentityToDevice,
} from "./hostedCommunityApi.ts";

const AUTH = { expiresAt: "2099-01-01T00:00:00Z" };
const KEY_A = "a".repeat(64);
const KEY_B = "b".repeat(64);

test("F367/F369: an unknown local key is never treated as a match", () => {
  for (const localPubkey of [null, undefined, "", "npub1notahexkey", "zz"]) {
    const gate = evaluateHostedIdentityGate({
      auth: AUTH,
      identity: { pubkey_hex: KEY_A },
      localPubkey,
    });
    assert.equal(gate.ready, false, String(localPubkey));
    assert.equal(gate.localKeyUnknown, true, String(localPubkey));
    assert.equal(gate.canSwitchToDevice, false, String(localPubkey));
  }
});

test("F367/F369: ready only when both keys are known and equal", () => {
  assert.equal(
    evaluateHostedIdentityGate({
      auth: AUTH,
      identity: { pubkey_hex: ` ${KEY_A.toUpperCase()} ` },
      localPubkey: KEY_A,
    }).ready,
    true,
  );
  const mismatch = evaluateHostedIdentityGate({
    auth: AUTH,
    identity: { pubkey_hex: KEY_A },
    localPubkey: KEY_B,
  });
  assert.equal(mismatch.ready, false);
  assert.equal(mismatch.identityMismatch, true);
  assert.equal(mismatch.canSwitchToDevice, true);
  assert.equal(
    evaluateHostedIdentityGate({
      auth: null,
      identity: { pubkey_hex: KEY_A },
      localPubkey: KEY_A,
    }).ready,
    false,
  );
  const unbound = evaluateHostedIdentityGate({
    auth: AUTH,
    identity: null,
    localPubkey: null,
  });
  assert.equal(unbound.ready, false);
  assert.equal(unbound.localKeyUnknown, false);
});

test("F368/F370: an unusable bound key never offers the destructive switch", () => {
  const gate = evaluateHostedIdentityGate({
    auth: AUTH,
    identity: { pubkey_hex: "npub1storedinthehexfield" },
    localPubkey: KEY_A,
  });
  assert.equal(gate.identityMismatch, true);
  assert.equal(gate.canSwitchToDevice, false);
  assert.equal(gate.ready, false);
});

function scriptedApi({ bind = [], release = [] }) {
  const calls = [];
  return {
    calls,
    api: {
      bind: async () => {
        calls.push("bind");
        return bind.shift();
      },
      release: async () => {
        calls.push("release");
        return release.shift();
      },
    },
  };
}

test("F368/F370: a refused switch makes no call at all", async () => {
  const { calls, api } = scriptedApi({});
  const outcome = await switchHostedIdentityToDevice(
    { canSwitchToDevice: false },
    api,
  );
  assert.deepEqual(outcome, { kind: "refused" });
  assert.deepEqual(calls, []);
  assert.match(
    switchHostedIdentityErrorMessage(outcome),
    /won't disconnect anything/,
  );
});

test("F368/F370: a device key owned by another account stops before the delete", async () => {
  const { calls, api } = scriptedApi({
    bind: [{ error: { code: "pubkey_already_bound" } }],
  });
  const outcome = await switchHostedIdentityToDevice(
    { canSwitchToDevice: true },
    api,
  );
  assert.equal(outcome.kind, "unchanged");
  assert.deepEqual(calls, ["bind"]);
  const message = switchHostedIdentityErrorMessage(outcome);
  assert.match(message, /different Builderlab account/);
  assert.match(message, /Nothing was changed/);
});

test("F368/F370: an unexpected pre-check error stops before the delete", async () => {
  const { calls, api } = scriptedApi({
    bind: [{ error: { code: "relay_unavailable" }, correlation_id: "c-1" }],
  });
  const outcome = await switchHostedIdentityToDevice(
    { canSwitchToDevice: true },
    api,
  );
  assert.equal(outcome.kind, "unchanged");
  assert.equal(outcome.correlationId, "c-1");
  assert.deepEqual(calls, ["bind"]);
});

test("F368/F370: an account holding the other key is released, then rebound", async () => {
  const { calls, api } = scriptedApi({
    bind: [
      { error: { code: "identity_already_bound" } },
      { identity: { pubkey_hex: KEY_B } },
    ],
    release: [{}],
  });
  const outcome = await switchHostedIdentityToDevice(
    { canSwitchToDevice: true },
    api,
  );
  assert.deepEqual(outcome, {
    kind: "switched",
    identity: { pubkey_hex: KEY_B },
  });
  assert.deepEqual(calls, ["bind", "release", "bind"]);
});

test("F368/F370: a pre-check bind that succeeds needs no delete", async () => {
  const { calls, api } = scriptedApi({
    bind: [{ identity: { pubkey_hex: KEY_B } }],
  });
  const outcome = await switchHostedIdentityToDevice(
    { canSwitchToDevice: true },
    api,
  );
  assert.equal(outcome.kind, "switched");
  assert.deepEqual(calls, ["bind"]);
});

test("F368/F370: a failed release leaves the binding intact", async () => {
  const { calls, api } = scriptedApi({
    bind: [{ error: { code: "identity_already_bound" } }],
    release: [{ error: { code: "not_owner" } }],
  });
  const outcome = await switchHostedIdentityToDevice(
    { canSwitchToDevice: true },
    api,
  );
  assert.equal(outcome.kind, "unchanged");
  assert.deepEqual(calls, ["bind", "release"]);
});

test("F368/F370: a final bind failure is reported as an unbound account", async () => {
  const { calls, api } = scriptedApi({
    bind: [
      { error: { code: "identity_already_bound" } },
      { error: { code: "pubkey_already_bound" } },
    ],
    release: [{}],
  });
  const outcome = await switchHostedIdentityToDevice(
    { canSwitchToDevice: true },
    api,
  );
  assert.equal(outcome.kind, "unbound");
  assert.deepEqual(calls, ["bind", "release", "bind"]);
  assert.match(
    switchHostedIdentityErrorMessage(outcome),
    /previous Buzz identity was disconnected/,
  );
});

function rejectingApi(script) {
  const calls = [];
  return {
    calls,
    api: {
      bind: async () => {
        calls.push("bind");
        const next = script.bind.shift();
        if (next instanceof Error) throw next;
        return next;
      },
      release: async () => {
        calls.push("release");
        const next = script.release.shift();
        if (next instanceof Error) throw next;
        return next;
      },
    },
  };
}

test("F368 review: a rejected pre-check bind is unchanged and never deletes", async () => {
  const { calls, api } = rejectingApi({
    bind: [new Error("network down")],
    release: [],
  });
  const outcome = await switchHostedIdentityToDevice(
    { canSwitchToDevice: true },
    api,
  );
  assert.equal(outcome.kind, "unchanged");
  assert.deepEqual(calls, ["bind"]);
  assert.match(switchHostedIdentityErrorMessage(outcome), /Nothing was changed/);
});

test("F368 review: a rejected release or final bind is reported as uncertain", async () => {
  for (const script of [
    {
      bind: [{ error: { code: "identity_already_bound" } }],
      release: [new Error("timeout")],
    },
    {
      bind: [
        { error: { code: "identity_already_bound" } },
        new Error("timeout"),
      ],
      release: [{}],
    },
  ]) {
    const { api } = rejectingApi(script);
    const outcome = await switchHostedIdentityToDevice(
      { canSwitchToDevice: true },
      api,
    );
    assert.equal(outcome.kind, "uncertain");
    assert.match(
      switchHostedIdentityErrorMessage(outcome),
      /may have been disconnected/,
    );
  }
});

test("F368 review: the warning survives a failed account refresh", async () => {
  const warn = console.warn;
  console.warn = () => {};
  try {
    let cleared = 0;
    const unbound = {
      kind: "unbound",
      error: { code: "pubkey_already_bound" },
    };
    const message = await settleFailedIdentitySwitch(
      unbound,
      async () => {
        throw new Error("refresh failed");
      },
      () => {
        cleared += 1;
      },
    );
    assert.match(message, /previous Buzz identity was disconnected/);
    assert.equal(cleared, 1);

    const unchanged = { kind: "unchanged", error: { code: "relay_unavailable" } };
    assert.match(
      await settleFailedIdentitySwitch(
        unchanged,
        async () => {
          throw new Error("refresh failed");
        },
        () => {
          cleared += 1;
        },
      ),
      /Nothing was changed/,
    );
    assert.equal(cleared, 1);

    let reloads = 0;
    await settleFailedIdentitySwitch(
      { kind: "refused" },
      async () => {
        reloads += 1;
      },
      () => {},
    );
    assert.equal(reloads, 0);
  } finally {
    console.warn = warn;
  }
});
