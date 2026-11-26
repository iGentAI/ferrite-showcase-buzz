const HTTP_RELAY_MAX_FILTERS_PER_QUERY = 8;
const HTTP_RELAY_MIN_POLL_MS = 1_000;
const HTTP_RELAY_MAX_POLL_MS = 8_000;

export function nextHttpRelayPollDelay(
  currentDelayMs: number,
  deliveredEvents: number,
): number {
  if (deliveredEvents > 0) return HTTP_RELAY_MIN_POLL_MS;
  return Math.min(
    HTTP_RELAY_MAX_POLL_MS,
    Math.max(HTTP_RELAY_MIN_POLL_MS, currentDelayMs * 2),
  );
}

export type HttpRelayLiveSubscription<TFilter, TEvent extends { id: string }> = {
  filter: TFilter;
  onEvent: (event: TEvent) => void;
  seen: Set<string>;
};

export type HttpRelaySubscriptionGroup<TFilter, TEvent extends { id: string }> = {
  filter: TFilter;
  subscriptions: Array<HttpRelayLiveSubscription<TFilter, TEvent>>;
};

export function groupHttpRelaySubscriptions<
  TFilter,
  TEvent extends { id: string },
>(
  subscriptions: Array<HttpRelayLiveSubscription<TFilter, TEvent>>,
  keyForFilter: (filter: TFilter) => string,
): Array<HttpRelaySubscriptionGroup<TFilter, TEvent>> {
  const groups = new Map<string, HttpRelaySubscriptionGroup<TFilter, TEvent>>();
  for (const subscription of subscriptions) {
    const key = keyForFilter(subscription.filter);
    const group = groups.get(key);
    if (group) {
      group.subscriptions.push(subscription);
    } else {
      groups.set(key, {
        filter: subscription.filter,
        subscriptions: [subscription],
      });
    }
  }
  return Array.from(groups.values());
}

export function chunkHttpRelaySubscriptions<T>(
  subscriptions: T[],
  size = HTTP_RELAY_MAX_FILTERS_PER_QUERY,
): T[][] {
  if (!Number.isInteger(size) || size < 1) {
    throw new Error("HTTP relay poll batch size must be a positive integer.");
  }
  const chunks: T[][] = [];
  for (let index = 0; index < subscriptions.length; index += size) {
    chunks.push(subscriptions.slice(index, index + size));
  }
  return chunks;
}

export function dispatchHttpRelayEvents<
  TFilter,
  TEvent extends { id: string },
>(
  subscriptions: Array<HttpRelayLiveSubscription<TFilter, TEvent>>,
  events: TEvent[],
  matches: (filter: TFilter, event: TEvent) => boolean,
): number {
  let delivered = 0;
  for (const subscription of subscriptions) {
    for (const event of events) {
      if (
        subscription.seen.has(event.id) ||
        !matches(subscription.filter, event)
      ) {
        continue;
      }
      subscription.seen.add(event.id);
      subscription.onEvent(event);
      delivered += 1;
    }
  }
  return delivered;
}
