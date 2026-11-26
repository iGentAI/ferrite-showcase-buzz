import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const STARTUP_SCREENS = [
  "../../communities/ui/CommunityApplyErrorScreen.tsx",
  "../../communities/ui/WelcomeSetup.tsx",
  "CommunityOnboardingFlow.tsx",
  "KeyringLockedScreen.tsx",
  "MachineOnboardingFlow.tsx",
  "OnboardingFlow.tsx",
  "RecoveryScreen.tsx",
];

test("every standalone startup screen owns a main landmark", () => {
  for (const relativePath of STARTUP_SCREENS) {
    const source = fs.readFileSync(path.resolve(HERE, relativePath), "utf8");
    assert.match(
      source,
      /role="main"/,
      `${relativePath} must keep page content inside a main landmark`,
    );
  }
});
