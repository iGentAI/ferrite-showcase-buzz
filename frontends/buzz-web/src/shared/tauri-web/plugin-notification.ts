/** `@tauri-apps/plugin-notification`, served by the desktop bridge. */
export {
  isPermissionGranted,
  onAction,
  requestPermission,
} from "@/shared/lib/desktop-bridge";
export type { NotificationPayload } from "@/shared/lib/desktop-bridge";
