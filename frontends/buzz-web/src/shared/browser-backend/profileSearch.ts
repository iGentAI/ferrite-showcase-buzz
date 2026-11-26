/**
 * The paged walk behind `search_users`, taking its pages from a callback.
 *
 * ## Why a walk at all
 *
 * The desktop searches an indexed local mirror of every profile it has seen. A
 * tab has no mirror and this relay refuses NIP-50 (`ingress.rs` answers a
 * search filter with `CLOSED unsupported`), so matching happens in the client,
 * over kind-0 events read off the relay's profile index.
 *
 * That index holds one row per identity and the relay caps a page at 500, so
 * one page with no cursor would silently hide everyone past the first 500
 * profiles and tell every caller those 500 were everyone.
 *
 * ## The contract
 *
 * * Results are **newest profile first**, in walk order, never re-sorted by
 *   name. A timestamp index can only page in its own order, and the newest
 *   kind 0 is the identity its owner most recently used — which is the fact
 *   that separates a live colleague from the same-named husks a shared relay
 *   accumulates.
 * * One identity appears at most once: the walk is newest-first, so the first
 *   row an author contributes is their current profile and later ones are
 *   superseded revisions.
 * * `nextCursor` is non-null **exactly when** the walk stopped with the index
 *   unexhausted — either because the caller's `limit` filled or because the
 *   scan bound was reached. `null` means there is nothing further to ask for.
 *   Neither answer pretends to be the other.
 *
 * ## The one thing it cannot reach, stated
 *
 * `until` has second resolution, so a second holding MORE profiles than one
 * relay page can carry cannot be paged past: every read at that `until` returns
 * the same capped page, and the rest of that second is unreachable at any
 * cursor. The walk detects this and stops rather than spinning. It needs 500
 * distinct identities to publish a profile inside one second, which per-author
 * replaceable kind 0 makes a theoretical shape rather than an operational one —
 * the live relay accumulated 640 profiles over sixteen hours.
 */
import type { RelayEvent } from "@/shared/api/types";

/**
 * Backstop on rounds. NOT the real bound — {@link SEARCH_BUDGET_MS} is.
 *
 * A page-count bound is the wrong shape: it behaves on a dense query, which fills its `limit` and leaves after
 * one page, and pathologically on a sparse one, which cannot fill and pays the
 * full count. Sparse is the normal case — "find this one person" — so the bound
 * has to be on the thing the user actually feels, which is TIME. This stays only
 * so a relay answering instantly cannot spin the loop unboundedly.
 */
const SEARCH_SCAN_PAGES = 8;

/**
 * How long one search may spend walking before it answers with a cursor.
 *
 * Checked BETWEEN pages — a read in flight cannot be abandoned — so the real
 * worst case is this budget plus one page. On the live dev relay a
 * `kind 0, limit 500` page measures ~3.3 s (debug-build data plane), so 4 s
 * admits a second page and stops: ~7 s worst case against the DM picker's 30 s
 * timeout and the mention picker's 45 s. On a release fleet, where that page is
 * a fraction of the cost, the same budget buys many more pages and the coverage
 * this walk exists for comes back automatically. That is the point of budgeting
 * time rather than pages: it tracks the relay you actually have.
 */
const SEARCH_BUDGET_MS = 4_000;

/** A query that IS an x-only pubkey: 64 lowercase hex. */
const PUBKEY_QUERY = /^[0-9a-f]{64}$/;

/**
 * Where the next page resumes: a NIP-01 `until`, plus the ids already answered
 * for AT that exact second.
 *
 * `until` is inclusive and the index's only client-visible coordinate is a
 * whole second, so resuming at the oldest timestamp a page reached would serve
 * that second's profiles twice. Carrying their ids makes the boundary exact.
 * The relay's own cursor is exact too, but NIP-01 does not expose it over
 * `REQ`, and a client may not invent one.
 */
export function encodeSearchCursor(
  until: number,
  boundary: readonly string[],
): string {
  return [String(until), ...boundary].join(".");
}

export function decodeSearchCursor(
  cursor: unknown,
): { until: number; boundary: Set<string> } | null {
  if (typeof cursor !== "string" || cursor.length === 0) return null;
  const [head, ...ids] = cursor.split(".");
  const until = Number(head);
  if (!Number.isSafeInteger(until) || until < 0) return null;
  return {
    until,
    boundary: new Set(ids.filter((id) => /^[0-9a-f]{64}$/.test(id))),
  };
}

export type ProfileSearchPage<Row> = {
  rows: Row[];
  /** Non-null exactly when the index was left unexhausted. */
  nextCursor: string | null;
};

export type ProfileSearchOptions<Row> = {
  /** How many matching identities the caller asked for. */
  limit: number;
  /** An opaque cursor from a previous page, or anything else for "start fresh". */
  cursor?: unknown;
  /** One page of the kind-0 index at or below `until` (undefined = newest). */
  readPage: (until: number | undefined) => Promise<RelayEvent[]>;
  /**
   * Project an identity's current profile into a result row, or `null` when it
   * is not a match. Called once per identity — for the caller's own key, for a
   * profile the query does not hit, and for an unreadable one alike, `null`
   * means "seen, not offered", never "look again".
   */
  match: (event: RelayEvent) => Row | null;
  /** Overridable only so tests can exercise the bound cheaply. */
  scanPages?: number;
  /** Overridable only so tests can exercise the bound cheaply. */
  budgetMs?: number;
  /** Injectable clock, so the time bound is testable without sleeping. */
  now?: () => number;
};

export type ProfileSearchEntryOptions<Row> = ProfileSearchOptions<Row> & {
  /** The trimmed, lowercased query the caller typed. */
  query: string;
  /** Every kind 0 by one author — the targeted read behind the short-circuit. */
  readAuthor: (pubkey: string) => Promise<RelayEvent[]>;
};

/**
 * The entry point: resolve one identity directly when the query names one,
 * otherwise walk.
 *
 * A query that IS a pubkey matches EXACTLY ONE profile, so it can never fill a
 * `limit` and a corpus walk pays its whole bound to find a single row — the
 * worst possible trade, on the picker's most common request. The relay indexes
 * kind 0 by author, so asking for that author is one targeted read: ~140 ms
 * against the ~3.3 s a 500-event page costs, measured on the live relay.
 *
 * It is also strictly MORE correct than scanning. A pubkey read finds the
 * profile at any depth in the index, including the identities that sit past the
 * page cap — which is the exact problem the walk was built to solve, solved
 * better for the case where the caller already knows who they mean.
 */
export async function searchProfiles<Row>(
  options: ProfileSearchEntryOptions<Row>,
): Promise<ProfileSearchPage<Row>> {
  const { query, readAuthor, limit, match } = options;
  if (limit <= 0) return { rows: [], nextCursor: null };
  if (!PUBKEY_QUERY.test(query)) return walkProfileSearch(options);

  const events = await readAuthor(query);
  // Newest kind 0 wins: the author's current profile, same rule the walk uses.
  const newest = events
    .filter((event) => event.pubkey.toLowerCase() === query)
    .sort((a, b) => b.created_at - a.created_at)[0];
  const row = newest ? match(newest) : null;
  // No cursor either way. One identity is the whole result set, and an author
  // with no kind 0 on this relay has none to page toward — a walk would not
  // find it, because it reads the same index.
  return { rows: row === null ? [] : [row], nextCursor: null };
}

export async function walkProfileSearch<Row>(
  options: ProfileSearchOptions<Row>,
): Promise<ProfileSearchPage<Row>> {
  const { limit, readPage, match } = options;
  const scanPages = options.scanPages ?? SEARCH_SCAN_PAGES;
  const budgetMs = options.budgetMs ?? SEARCH_BUDGET_MS;
  const now = options.now ?? Date.now;
  const startedAt = now();
  if (limit <= 0) return { rows: [], nextCursor: null };

  const resume = decodeSearchCursor(options.cursor);
  let until = resume?.until;
  let consumedAtBoundary = resume?.boundary ?? new Set<string>();

  const rows: Row[] = [];
  const answered = new Set<string>();

  for (let round = 0; round < scanPages; round += 1) {
    const events = await readPage(until);
    const fresh = events.filter((event) => !consumedAtBoundary.has(event.id));
    // Nothing left the boundary has not already accounted for: the index is
    // exhausted and the answer is complete. This is also what terminates a
    // second dense enough to fill a whole page, because the boundary set below
    // grows to cover every row of it.
    if (fresh.length === 0) break;

    // Newest-first, tie-broken on id. The relay serves this order; re-deriving
    // it here means the boundary a cursor encodes does not depend on the socket
    // having preserved it.
    const ordered = [...fresh].sort(
      (a, b) => b.created_at - a.created_at || a.id.localeCompare(b.id),
    );

    let filled = false;
    let consumed = 0;
    for (const event of ordered) {
      consumed += 1;
      const pubkey = event.pubkey.toLowerCase();
      if (answered.has(pubkey)) continue;
      answered.add(pubkey);
      const row = match(event);
      if (row === null) continue;
      rows.push(row);
      if (rows.length >= limit) {
        filled = true;
        break;
      }
    }

    const last = ordered[consumed - 1];
    if (!last) break;

    // Resume at the last row this call actually CONSUMED — not the last row the
    // page held, which a filled request never reached. The boundary carries the
    // ids at that second already answered for: those in the prefix just walked,
    // plus, when that second is the one we resumed into, the ones the previous
    // call had skipped. Dropping the carried half is what would let the walk
    // re-serve one second forever instead of terminating.
    const boundary = ordered
      .slice(0, consumed)
      .filter((event) => event.created_at === last.created_at)
      .map((event) => event.id);
    const carried =
      until === last.created_at
        ? [...consumedAtBoundary, ...boundary]
        : boundary;
    const resumeCursor = encodeSearchCursor(last.created_at, carried);

    if (filled) return { rows, nextCursor: resumeCursor };
    // Stopped by a stated bound with the index unexhausted: say so. The time
    // budget is the one that normally binds — a sparse query cannot fill its
    // `limit`, so without this it would pay for every page the round cap allows
    // and the picker would sit there for the sum of them.
    if (now() - startedAt >= budgetMs) return { rows, nextCursor: resumeCursor };
    if (round === scanPages - 1) return { rows, nextCursor: resumeCursor };

    until = last.created_at;
    consumedAtBoundary = new Set(carried);
  }

  return { rows, nextCursor: null };
}
