const HTTP_RELAY_QUERY_BATCH_DELAY_MS = 10;

type PendingQuery<TFilter, TEvent> = {
  filter: TFilter;
  resolve: (events: TEvent[]) => void;
  reject: (error: unknown) => void;
};

/**
 * Coalesce the burst of independent history reads produced by one React render.
 *
 * The relay already accepts up to eight filters in one authenticated request.
 * Keeping this coordinator below the transport lets each HTTP batch use one
 * fresh NIP-98 proof while every caller still receives only events matching its
 * own filter.
 */
export class HttpRelayQueryBatcher<TFilter, TEvent> {
  private pending: Array<PendingQuery<TFilter, TEvent>> = [];
  private timer: ReturnType<typeof setTimeout> | null = null;
  private readonly run: (filters: TFilter[]) => Promise<TEvent[]>;
  private readonly matches: (filter: TFilter, event: TEvent) => boolean;
  private readonly filterKey: (filter: TFilter) => string;
  private readonly eventKey: (event: TEvent) => string;
  private readonly maxFilters: number;
  private readonly delayMs: number;

  constructor(
    run: (filters: TFilter[]) => Promise<TEvent[]>,
    matches: (filter: TFilter, event: TEvent) => boolean,
    filterKey: (filter: TFilter) => string,
    eventKey: (event: TEvent) => string,
    maxFilters = 8,
    delayMs = HTTP_RELAY_QUERY_BATCH_DELAY_MS,
  ) {
    if (!Number.isInteger(maxFilters) || maxFilters < 1) {
      throw new Error("HTTP relay query batch size must be a positive integer.");
    }
    this.run = run;
    this.matches = matches;
    this.filterKey = filterKey;
    this.eventKey = eventKey;
    this.maxFilters = maxFilters;
    this.delayMs = delayMs;
  }

  query(filter: TFilter): Promise<TEvent[]> {
    return new Promise((resolve, reject) => {
      this.pending.push({ filter, resolve, reject });
      if (this.timer !== null) return;
      this.timer = setTimeout(() => {
        this.timer = null;
        void this.flush();
      }, this.delayMs);
    });
  }

  private async flush(): Promise<void> {
    const pending = this.pending.splice(0);
    if (pending.length === 0) return;

    const unique = new Map<string, TFilter>();
    for (const request of pending) {
      unique.set(this.filterKey(request.filter), request.filter);
    }

    try {
      const filters = Array.from(unique.values());
      const events: TEvent[] = [];
      for (let index = 0; index < filters.length; index += this.maxFilters) {
        events.push(
          ...(await this.run(filters.slice(index, index + this.maxFilters))),
        );
      }
      const deduplicatedEvents = Array.from(
        new Map(events.map((event) => [this.eventKey(event), event])).values(),
      );
      for (const request of pending) {
        request.resolve(
          deduplicatedEvents.filter((event) =>
            this.matches(request.filter, event),
          ),
        );
      }
    } catch (error) {
      for (const request of pending) request.reject(error);
    }
  }
}
