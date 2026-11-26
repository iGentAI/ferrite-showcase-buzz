import assert from "node:assert/strict";
import test from "node:test";

const store = new Map();
globalThis.window ??= globalThis;
Object.defineProperty(globalThis, "localStorage", {
  configurable: true,
  value: {
    getItem: (key) => store.get(key) ?? null,
    setItem: (key, value) => store.set(key, String(value)),
    removeItem: (key) => store.delete(key),
  },
});

const { readMachineOnboardingCompletion, recordMachineOnboardingCompletion } =
  await import("./machineOnboarding.ts");

test("a key imported during community onboarding counts as set up on this device", () => {
  const imported = "c".repeat(64);
  assert.equal(readMachineOnboardingCompletion(imported), false);
  recordMachineOnboardingCompletion(imported);
  assert.equal(readMachineOnboardingCompletion(imported), true);
});
