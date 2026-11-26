/** `@tauri-apps/api/event`, served by the desktop bridge. */
export { emit, listen } from "@/shared/lib/desktop-bridge";
export type {
  Event,
  EventCallback,
  UnlistenFn,
} from "@/shared/lib/desktop-bridge";
