/**
 * Harnesses whose persona definition is the running server-side agent.
 *
 * These never need a desktop-managed process. Ferrite Cloud runs in the
 * workspace turn loop; Fabric/A2A runs in the relay's remote A2A turn loop.
 */
const RELAY_MANAGED_HARNESSES = new Set(["ferrite-cloud", "fabric", "a2a"]);

const RELAY_MANAGED_MODEL_SELECTABLE_HARNESSES = new Set(["ferrite-cloud"]);

export function isRelayManagedHarness(runtimeId: string): boolean {
  return RELAY_MANAGED_HARNESSES.has(runtimeId.trim().toLowerCase());
}

/**
 * Server-owned harnesses whose model is still an explicit persona setting.
 *
 * Ferrite Cloud serves its routable model catalog from the workspace and uses
 * the selected model when it runs a turn. Fabric/A2A peers own model routing
 * remotely, so exposing the same picker for them would persist a value Buzz
 * cannot enforce.
 */
export function isRelayManagedModelSelectableHarness(
  runtimeId: string,
): boolean {
  return RELAY_MANAGED_MODEL_SELECTABLE_HARNESSES.has(
    runtimeId.trim().toLowerCase(),
  );
}
