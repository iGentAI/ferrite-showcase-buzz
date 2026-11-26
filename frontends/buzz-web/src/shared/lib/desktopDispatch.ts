/**
 * The hybrid dispatch decision, extracted pure.
 *
 * One SPA serves three run targets: a browser tab (the seam answers), the
 * relay-pushed embedded UI (the seam answers), and the buzz-desktop Tauri
 * shell (the shell answers the command families it registers; the seam
 * answers everything else). This module owns the DECISION of which world the
 * bundle woke up in and which commands route native. It deliberately imports
 * nothing, so a plain `node --test` can exercise it without dragging the
 * browser-backend graph along.
 *
 * How the shell identifies itself — two independent signals, both required:
 *
 *   1. `window.__BUZZ_DESKTOP__ = { version: 1, commands: […] }`, injected by
 *      the shell's initialization script BEFORE any bundle code runs. The
 *      command list is the shell's native registry, available synchronously —
 *      no async handshake, no race with early `isTauri()` callers. An unknown
 *      version fails CLOSED to pure-seam mode: a mismatched shell renders the
 *      web experience rather than half-working native paths.
 *   2. Genuine Tauri internals: `window.__TAURI_INTERNALS__` carrying BOTH
 *      `invoke` and `transformCallback`. Test mocks (mockIPC, the e2e bridge)
 *      install `invoke` (and sometimes `listen`) but never `transformCallback`,
 *      which is what keeps every existing suite meaningful: a mock is never
 *      mistaken for a shell, and a shell is never mistaken for a mock.
 *
 * The shell also answers a `desktop_capabilities` command returning the same
 * `{ version, commands }` document, so an e2e run can assert the injected
 * marker and the compiled registry cannot drift apart.
 */

export const DESKTOP_MARKER_VERSION = 1;

export type DesktopRuntime =
  | { native: true; commands: ReadonlySet<string> }
  | { native: false; commands?: undefined };

type MarkerShape = { version?: unknown; commands?: unknown };

/**
 * Read and validate the shell's injected marker. Returns the native command
 * registry, or null when the marker is absent or malformed — malformed
 * includes a version this bundle does not know, which must fail closed.
 */
export function readDesktopMarker(w: unknown): ReadonlySet<string> | null {
  if (!w || typeof w !== "object") return null;
  const marker = (w as { __BUZZ_DESKTOP__?: MarkerShape }).__BUZZ_DESKTOP__;
  if (!marker || typeof marker !== "object") return null;
  if (marker.version !== DESKTOP_MARKER_VERSION) return null;
  if (!Array.isArray(marker.commands)) return null;
  if (!marker.commands.every((c) => typeof c === "string")) return null;
  return new Set(marker.commands as string[]);
}

/**
 * Whether genuine Tauri internals are present. The discriminator is
 * `transformCallback`: the real runtime always injects it, and no mock in
 * this repository (mockIPC, the e2e bridge) ever has — they stub `invoke`
 * alone, precisely so they keep winning the dispatch they always won.
 */
export function hasRealTauriInternals(w: unknown): boolean {
  if (!w || typeof w !== "object") return false;
  const internals = (
    w as {
      __TAURI_INTERNALS__?: { invoke?: unknown; transformCallback?: unknown };
    }
  ).__TAURI_INTERNALS__;
  if (!internals || typeof internals !== "object") return false;
  return (
    typeof internals.invoke === "function" &&
    typeof internals.transformCallback === "function"
  );
}

/** The one decision: which world did this bundle wake up in? */
export function detectDesktopRuntime(w: unknown): DesktopRuntime {
  const commands = readDesktopMarker(w);
  if (commands === null) return { native: false };
  if (!hasRealTauriInternals(w)) return { native: false };
  return { native: true, commands };
}

/** Whether `cmd` routes to native IPC under `runtime`. */
export function routesNative(runtime: DesktopRuntime, cmd: string): boolean {
  return runtime.native === true && runtime.commands.has(cmd);
}

export type DispatchTarget = "mock" | "native" | "seam";

/**
 * The bridge's per-command dispatch, as one pure decision: an installed mock
 * ALWAYS wins (this is what keeps every existing suite meaningful); a command
 * the shell registered routes native; everything else — including every
 * command of an unregistered family — stays on the browser seam.
 */
export function dispatchDecision(
  mockInstalled: boolean,
  runtime: DesktopRuntime,
  cmd: string,
): DispatchTarget {
  if (mockInstalled) return "mock";
  if (routesNative(runtime, cmd)) return "native";
  return "seam";
}

/**
 * Whether the shell registered the genuine Tauri event system. Both halves
 * are required: a listen whose unlisten cannot be delivered would leak the
 * subscription, so a shell that registers one without the other gets neither.
 */
export function canNativeEvents(runtime: DesktopRuntime): boolean {
  return (
    routesNative(runtime, "plugin:event|listen") &&
    routesNative(runtime, "plugin:event|unlisten")
  );
}
