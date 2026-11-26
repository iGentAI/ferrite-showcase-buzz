import { takePendingWelcomeChannelForDirectEntry } from "@/features/onboarding/welcome";

/**
 * Enter the Welcome channel directly at the end of first-run onboarding.
 *
 * `initializeStarterChannels(..., { focus: true })` remembers the Welcome room
 * as a PENDING entry for the Home route's fallback redirect. The community flow
 * (`CommunityOnboardingFlow.finalize`) consumes that entry when it points the
 * router at Welcome itself; the first-run gate (`completeAndShowWelcome` in
 * `features/onboarding/hooks.ts`) sets the hash directly, so it must consume it
 * here. An entry that outlives the visit bounces the person's first trip to
 * Inbox within its five-minute max age back to Welcome.
 */
export function enterWelcomeChannelDirectly(channelId: string) {
  takePendingWelcomeChannelForDirectEntry();
  window.location.hash = `/channels/${encodeURIComponent(channelId)}`;
}
