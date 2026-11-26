import assert from "node:assert/strict";
import test from "node:test";

import { safeLocalStorage } from "./safeStorage.ts";

// On a storage-denied WebKit origin, reading the window.localStorage GETTER
// throws. Default parameters evaluated it before any try block, crashing the
// pre-render bootstrap and onboarding (finding_84e4436364226b23,
// finding_8f2ccf6ca76636a6, finding_1df42cfaa2d13591).
test("safeLocalStorage degrades to an in-memory store when the getter throws", async () => {
  const previous = globalThis.window;
  globalThis.window = {};
  Object.defineProperty(globalThis.window, "localStorage", {
    get() {
      throw new DOMException("denied", "SecurityError");
    },
  });
  try {
    const storage = safeLocalStorage();
    storage.setItem("k", "v");
    assert.equal(storage.getItem("k"), "v");
    assert.equal(safeLocalStorage(), storage, "one stand-in per page");
    const { loadCommunityOnboardingTransaction, clearCommunityOnboardingTransaction } = await import(
      "../../features/onboarding/communityOnboarding.tsx"
    );
    assert.doesNotThrow(() => loadCommunityOnboardingTransaction());
    assert.doesNotThrow(() => clearCommunityOnboardingTransaction());
  } finally {
    if (previous === undefined) delete globalThis.window;
    else globalThis.window = previous;
  }
});
