/**
 * A complete read of one relay filter, walked page by page.
 *
 * The relay answers a filter read with ONE newest-first page, bounded by bytes
 * as well as by `limit`, and says nothing when it stopped early. A one-page read
 * of `kinds: [39000]` loses old, rarely edited rooms first: a starter room like
 * `general`, whose 39000 was written once, falls off the end of the page.
 *
 * The walk re-reads with `until` set to the oldest `created_at` seen, keeping
 * only events it has not seen by id, because one second can hold several events
 * and an `oldest - 1` cursor would skip the rest of that second. When a page adds
 * nothing new, the cursor moves one second earlier. The walk stops on an empty
 * page or at `maxPages`. With `until` as the only cursor, more same-second
 * events than one page holds cannot all be reached; room projections are not
 * written that densely.
 */
export async function readAllPages<
  Event extends { id: string; created_at: number },
  Filter extends Record<string, unknown>,
>(
  filter: Filter,
  fetchPage: (filter: Filter & { until?: number }) => Promise<Event[]>,
  maxPages = 20,
): Promise<Event[]> {
  const byId = new Map<string, Event>();
  let until: number | undefined;
  for (let page = 0; page < maxPages; page += 1) {
    const events = await fetchPage(
      until === undefined ? { ...filter } : { ...filter, until },
    );
    let added = 0;
    let oldest = Number.POSITIVE_INFINITY;
    let newest = Number.NEGATIVE_INFINITY;
    for (const event of events) {
      if (event.created_at < oldest) oldest = event.created_at;
      if (event.created_at > newest) newest = event.created_at;
      if (!byId.has(event.id)) {
        byId.set(event.id, event);
        added += 1;
      }
    }
    if (events.length === 0 || !Number.isFinite(oldest)) break;
    // Re-asking for `until = oldest` repeats the page when it all sits in one
    // second or the cursor did not move, spending a page on duplicates; step
    // past it instead. The relay reads `until` as unsigned, so stop at 0.
    const next =
      added === 0 || newest === oldest || until === oldest ? oldest - 1 : oldest;
    if (next < 0) break;
    until = next;
  }
  return [...byId.values()];
}
