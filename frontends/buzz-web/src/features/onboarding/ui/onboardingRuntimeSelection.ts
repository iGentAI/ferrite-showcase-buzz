import type { AcpRuntimeCatalogEntry } from "@/shared/api/types";

/**
 * The harnesses onboarding offers, in display order.
 *
 * The catalog itself comes from the relay (`GET /buzz/api/agent-harnesses`), so
 * this list only decides ORDER and which ids onboarding surfaces at all —
 * remote backends first, because they are the ones a hosted workspace can
 * actually use, then the CLI harnesses the desktop build launches locally.
 */
export const ONBOARDING_RUNTIME_ORDER = [
  "ferrite-cloud",
  "fabric",
  "a2a",
  "claude",
  "codex",
  "goose",
  "buzz-agent",
];

const VISIBLE_ONBOARDING_RUNTIME_IDS = new Set<string>(
  ONBOARDING_RUNTIME_ORDER,
);

export function runtimeIsVisibleInOnboarding(runtimeId: string) {
  return VISIBLE_ONBOARDING_RUNTIME_IDS.has(runtimeId);
}

export function runtimeIsReadyForOnboarding(runtime: AcpRuntimeCatalogEntry) {
  return (
    runtime.availability === "available" &&
    (runtime.authStatus.status === "logged_in" ||
      runtime.authStatus.status === "not_applicable")
  );
}

export function getRuntimeInstallAction(runtime: AcpRuntimeCatalogEntry) {
  if (runtime.canAutoInstall) return "auto" as const;
  if (runtime.installInstructionsUrl.trim().length > 0) {
    return "instructions" as const;
  }
  return "unavailable" as const;
}

export function getVisibleOnboardingRuntimes(
  runtimes: readonly AcpRuntimeCatalogEntry[],
) {
  return runtimes
    .filter((runtime) => runtimeIsVisibleInOnboarding(runtime.id))
    .sort(
      (left, right) =>
        ONBOARDING_RUNTIME_ORDER.indexOf(left.id) -
        ONBOARDING_RUNTIME_ORDER.indexOf(right.id),
    );
}

export function getReadyOnboardingRuntimes(
  runtimes: readonly AcpRuntimeCatalogEntry[],
) {
  return getVisibleOnboardingRuntimes(runtimes).filter(
    runtimeIsReadyForOnboarding,
  );
}
