/**
 * A process-wide budget for channel lookups that message-link pills start on
 * their own when they mount. Message content is written by any relay peer, so
 * one message packed with links to unknown channels must not fan out into a
 * lookup per link (finding_9ae3958abad81004). A pill past the budget resolves
 * only when the reader points at or focuses it.
 */
export const MAX_EAGER_CHANNEL_LOOKUPS = 8;

let outstanding = 0;

/** Claim a slot; returns its release, or null when the budget is spent. */
export function claimEagerChannelLookup(): (() => void) | null {
  if (outstanding >= MAX_EAGER_CHANNEL_LOOKUPS) return null;
  outstanding += 1;
  let released = false;
  return () => {
    if (released) return;
    released = true;
    outstanding -= 1;
  };
}

export function eagerChannelLookupsOutstanding(): number {
  return outstanding;
}
