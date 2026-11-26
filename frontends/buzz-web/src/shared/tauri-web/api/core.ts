/**
 * `@tauri-apps/api/core`, served by the desktop bridge.
 *
 * The export list is explicit, never `export *`: a symbol upstream starts
 * importing that this build has not implemented must fail `tsc` by name.
 */
export { Channel, invoke, isTauri } from "@/shared/lib/desktop-bridge";
