/**
 * The `d`-tag key of a channel window's synthesised `39006` bounds event.
 *
 * Two sides of one wire share this function so they can never disagree about
 * WHICH page a bounds event describes:
 *
 * - the browser backend (`browser-backend/commands.ts`) synthesises the
 *   bounds event for the page it just fetched, keyed by the cursor the
 *   caller REQUESTED (null = the head page);
 * - the parser (`features/messages/lib/channelWindowResponse.ts`) computes
 *   the key it expects from the same request cursor and refuses a page whose
 *   bounds answer a different request.
 *
 * When the synthesiser derived this key from the fetched page's newest row
 * instead of the request cursor, every page after the first failed that
 * check and scroll-back history never loaded past the first window.
 */
export function windowBoundsKey(
  channelId: string,
  cursor: { createdAt: number; eventId: string } | null,
): string {
  const suffix = cursor
    ? `${cursor.createdAt}:${cursor.eventId.toLowerCase()}`
    : "head";
  return `${channelId.toLowerCase()}:${suffix}`;
}
