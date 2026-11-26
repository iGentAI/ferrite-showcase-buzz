import assert from "node:assert/strict";
import test from "node:test";

import {
  decodeSearchCursor,
  encodeSearchCursor,
  searchProfiles,
  walkProfileSearch,
} from "@/shared/browser-backend/profileSearch";

const ID = (n) => String(n).padStart(64, "0");

/** A kind-0 as the relay serves it: author `pk`, published at `at`. */
function profile(pk, at, extra = {}) {
  return {
    id: extra.id ?? ID(`${pk}${at}`.replace(/\D/g, "") || "1"),
    pubkey: pk,
    created_at: at,
    kind: 0,
    tags: [],
    content: JSON.stringify({ display_name: extra.name ?? pk }),
    sig: "",
  };
}

/**
 * A relay whose kind-0 index holds `all`, served newest-first, 500 to a page —
 * i.e. `query::MAX_PAGE_EVENTS`, the cap that made the old single-page read a
 * silent truncation.
 */
function relayOf(all, pageSize = 500) {
  const sorted = [...all].sort(
    (a, b) => b.created_at - a.created_at || a.id.localeCompare(b.id),
  );
  const reads = [];
  return {
    reads,
    readPage: async (until) => {
      reads.push(until);
      const from =
        until === undefined
          ? sorted
          : sorted.filter((event) => event.created_at <= until);
      return from.slice(0, pageSize);
    },
  };
}

const identity = (event) => ({ pubkey: event.pubkey, at: event.created_at });

test("a full index is walked past the relay's page cap, not truncated at it", async () => {
  // 640 identities, which is what the live dev relay actually held: one page of
  // 500 would have hidden 140 of them and called the answer complete.
  const all = Array.from({ length: 640 }, (_, i) =>
    profile(`pk${i}`, 2_000_000 - i, { id: ID(i) }),
  );
  const relay = relayOf(all);

  const page = await walkProfileSearch({
    limit: 10_000,
    readPage: relay.readPage,
    match: identity,
  });

  assert.equal(page.rows.length, 640);
  assert.equal(page.nextCursor, null, "an exhausted index reports completeness");
  assert.equal(new Set(page.rows.map((r) => r.pubkey)).size, 640);
});

test("results are newest-profile-first, so a live identity outranks its husks", async () => {
  const all = [
    profile("stale-a", 1_000, { id: ID(1) }),
    profile("stale-b", 1_100, { id: ID(2) }),
    profile("live", 9_999, { id: ID(3) }),
  ];
  const page = await walkProfileSearch({
    limit: 10,
    readPage: relayOf(all).readPage,
    match: identity,
  });
  assert.deepEqual(
    page.rows.map((r) => r.pubkey),
    ["live", "stale-b", "stale-a"],
  );
});

test("one identity appears once, at its current profile", async () => {
  const all = [
    profile("bo", 100, { id: ID(1) }),
    profile("bo", 500, { id: ID(2) }),
    profile("bo", 300, { id: ID(3) }),
  ];
  const page = await walkProfileSearch({
    limit: 10,
    readPage: relayOf(all).readPage,
    match: identity,
  });
  assert.deepEqual(page.rows, [{ pubkey: "bo", at: 500 }]);
});

test("a filled request hands back a cursor rather than claiming completeness", async () => {
  const all = Array.from({ length: 50 }, (_, i) =>
    profile(`pk${i}`, 1_000 - i, { id: ID(i) }),
  );
  const page = await walkProfileSearch({
    limit: 10,
    readPage: relayOf(all).readPage,
    match: identity,
  });
  assert.equal(page.rows.length, 10);
  assert.notEqual(page.nextCursor, null);
});

test("paging with the returned cursor enumerates every identity exactly once", async () => {
  const all = Array.from({ length: 137 }, (_, i) =>
    profile(`pk${i}`, 5_000 - i, { id: ID(i) }),
  );
  const relay = relayOf(all, 20);

  const seen = [];
  let cursor;
  for (let guard = 0; guard < 100; guard += 1) {
    const page = await walkProfileSearch({
      limit: 7,
      cursor,
      readPage: relay.readPage,
      match: identity,
    });
    seen.push(...page.rows.map((r) => r.pubkey));
    if (page.nextCursor === null) break;
    cursor = page.nextCursor;
  }

  assert.equal(seen.length, 137, "every identity is served");
  assert.equal(new Set(seen).size, 137, "and none of them twice");
});

test("identities sharing one second are not re-served across the page boundary", async () => {
  // Five identities per second, and pages that cut mid-second. `until` is
  // second resolution, so a cursor that carried only the timestamp would either
  // re-serve the whole second or skip its unread half; only the ids it carries
  // can place the boundary exactly.
  const all = Array.from({ length: 25 }, (_, i) =>
    profile(`pk${i}`, 4_242 - Math.floor(i / 5), { id: ID(i) }),
  );
  const relay = relayOf(all, 12);

  const seen = [];
  let cursor;
  for (let guard = 0; guard < 100; guard += 1) {
    const page = await walkProfileSearch({
      limit: 4,
      cursor,
      readPage: relay.readPage,
      match: identity,
    });
    seen.push(...page.rows.map((r) => r.pubkey));
    if (page.nextCursor === null) break;
    cursor = page.nextCursor;
  }

  assert.equal(seen.length, 25);
  assert.equal(new Set(seen).size, 25);
});

test("a second denser than a whole page terminates instead of looping forever", async () => {
  // 30 identities in one second behind a 10-row page. `until` is second
  // resolution, so the walk can never move past that second and the rows beyond
  // the page cap are unreachable over NIP-01 at any cursor — see the module
  // doc. What must hold is that it STOPS: the boundary set is what ends it.
  const relay = relayOf(
    Array.from({ length: 30 }, (_, i) => profile(`pk${i}`, 777, { id: ID(i) })),
    10,
  );
  const page = await walkProfileSearch({
    limit: 10_000,
    scanPages: 50,
    readPage: relay.readPage,
    match: identity,
  });
  assert.equal(page.rows.length, 10, "everything the page cap makes reachable");
  assert.equal(page.nextCursor, null, "and nothing further to ask for");
  assert.ok(relay.reads.length < 50, "it stopped, it did not spin to the bound");
});

test("a query that matches nothing stops at the stated bound and says so", async () => {
  const all = Array.from({ length: 5_000 }, (_, i) =>
    profile(`pk${i}`, 900_000 - i, { id: ID(i) }),
  );
  const relay = relayOf(all, 100);

  const page = await walkProfileSearch({
    limit: 10,
    scanPages: 3,
    readPage: relay.readPage,
    match: () => null,
  });

  assert.deepEqual(page.rows, []);
  assert.notEqual(
    page.nextCursor,
    null,
    "no matches YET must not read as no such person",
  );
  assert.equal(relay.reads.length, 3, "the bound is what stopped it");
});

test("an unmatched identity is not re-offered on the next page", async () => {
  const all = [
    profile("skip-me", 300, { id: ID(1) }),
    profile("wanted", 200, { id: ID(2) }),
  ];
  const page = await walkProfileSearch({
    limit: 10,
    readPage: relayOf(all).readPage,
    match: (event) => (event.pubkey === "skip-me" ? null : identity(event)),
  });
  assert.deepEqual(page.rows, [{ pubkey: "wanted", at: 200 }]);
  assert.equal(page.nextCursor, null);
});

test("an empty relay is complete, not pending", async () => {
  const page = await walkProfileSearch({
    limit: 10,
    readPage: async () => [],
    match: identity,
  });
  assert.deepEqual(page.rows, []);
  assert.equal(page.nextCursor, null);
});

test("a zero limit reads nothing at all", async () => {
  let reads = 0;
  const page = await walkProfileSearch({
    limit: 0,
    readPage: async () => {
      reads += 1;
      return [];
    },
    match: identity,
  });
  assert.deepEqual(page.rows, []);
  assert.equal(page.nextCursor, null);
  assert.equal(reads, 0);
});

test("the cursor round-trips, and a junk cursor starts from the top", () => {
  const ids = [ID(7), ID(8)];
  const decoded = decodeSearchCursor(encodeSearchCursor(1_234, ids));
  assert.equal(decoded.until, 1_234);
  assert.deepEqual([...decoded.boundary].sort(), [...ids].sort());

  for (const junk of [undefined, null, "", 12, "nope", "-1", {}]) {
    assert.equal(decodeSearchCursor(junk), null, `${String(junk)} is not a cursor`);
  }
  // A well-formed timestamp with a garbage id list keeps the timestamp and
  // drops the ids, rather than throwing a picker into an error state.
  assert.deepEqual(decodeSearchCursor("500.zzz").boundary, new Set());
});

test("a garbage cursor does not skip identities", async () => {
  const all = Array.from({ length: 12 }, (_, i) =>
    profile(`pk${i}`, 900 - i, { id: ID(i) }),
  );
  const page = await walkProfileSearch({
    limit: 100,
    cursor: "not-a-cursor",
    readPage: relayOf(all).readPage,
    match: identity,
  });
  assert.equal(page.rows.length, 12);
});

/* ------------------------------------------------------------------------- */
/* searchProfiles: the pubkey short-circuit, and the TIME bound              */
/* ------------------------------------------------------------------------- */

const PK = "75a1f459fd3d52a3e5ac1a98c7a8946223114fef25197911db4101f4969d69c0";

test("a pubkey query issues ONE targeted read and walks no pages", async () => {
  // The DM picker searches by pubkey on purpose. A pubkey matches exactly one
  // profile, so it can never fill `limit` — under a page-count bound it paid
  // for every page the cap allowed and blew the picker's 30 s timeout.
  const pageReads = [];
  const authorReads = [];
  const page = await searchProfiles({
    query: PK,
    limit: 8,
    readPage: async (until) => { pageReads.push(until); return []; },
    readAuthor: async (pk) => { authorReads.push(pk); return [profile(PK, 1_000, { id: ID(1) })]; },
    match: identity,
  });

  assert.equal(pageReads.length, 0, "the corpus must not be walked at all");
  assert.deepEqual(authorReads, [PK], "exactly one targeted read, for that author");
  assert.deepEqual(page.rows, [{ pubkey: PK, at: 1_000 }]);
  assert.equal(page.nextCursor, null, "one identity is the whole result set");
});

test("a pubkey query resolves an identity the page cap hides", async () => {
  // The short-circuit is not just faster, it is more correct: this author is
  // nowhere in the pages the walk can reach.
  const page = await searchProfiles({
    query: PK,
    limit: 8,
    readPage: async () => Array.from({ length: 500 }, (_, i) => profile(`other${i}`, 9_000, { id: ID(i) })),
    readAuthor: async () => [profile(PK, 5, { id: ID(99) })],
    match: identity,
  });
  assert.deepEqual(page.rows, [{ pubkey: PK, at: 5 }]);
});

test("a pubkey query with no profile on the relay answers empty and complete", async () => {
  let pageReads = 0;
  const page = await searchProfiles({
    query: PK,
    limit: 8,
    readPage: async () => { pageReads += 1; return []; },
    readAuthor: async () => [],
    match: identity,
  });
  assert.deepEqual(page.rows, []);
  assert.equal(page.nextCursor, null);
  assert.equal(pageReads, 0, "no walk: a scan reads the same index and would find nothing");
});

test("a pubkey query ignores a foreign author the relay volunteers", async () => {
  const page = await searchProfiles({
    query: PK,
    limit: 8,
    readPage: async () => [],
    readAuthor: async () => [profile("someone-else", 9_999, { id: ID(2) })],
    match: identity,
  });
  assert.deepEqual(page.rows, []);
});

test("a name query still walks — the short-circuit does not disable paging", async () => {
  let pageReads = 0, authorReads = 0;
  const relay = relayOf(Array.from({ length: 640 }, (_, i) => profile(`pk${i}`, 2_000 - i, { id: ID(i) })));
  const page = await searchProfiles({
    query: "bo",
    limit: 10_000,
    readPage: async (u) => { pageReads += 1; return relay.readPage(u); },
    readAuthor: async () => { authorReads += 1; return []; },
    match: identity,
  });
  assert.equal(authorReads, 0, "a name is not an author");
  assert.ok(pageReads > 1, "the paging improvement is intact");
  assert.equal(page.rows.length, 640);
});

test("a sparse query stops on the TIME budget and returns a usable cursor", async () => {
  // Nothing matches, so `limit` can never fill and only a bound can stop this.
  // Each page "costs" 3.3 s on the fake clock, which is what the live relay
  // measures for kind 0 / limit 500.
  let clock = 0;
  let pageReads = 0;
  const all = Array.from({ length: 100_000 }, (_, i) => profile(`pk${i}`, 9_000_000 - i, { id: ID(i) }));
  const relay = relayOf(all);
  const page = await searchProfiles({
    query: "nobody",
    limit: 50,
    budgetMs: 4_000,
    scanPages: 8,
    now: () => clock,
    readPage: async (u) => { pageReads += 1; clock += 3_300; return relay.readPage(u); },
    readAuthor: async () => [],
    match: () => null,
  });

  assert.equal(pageReads, 2, "budget 4000ms admits a 2nd 3.3s page, then stops — not all 8");
  assert.equal(clock, 6_600, "so the caller waits ~6.6s, not ~26s");
  assert.notEqual(page.nextCursor, null, "and it hands back a cursor rather than claiming completeness");

  // The cursor works: resuming reads on from where the budget cut it off.
  clock = 0;
  const more = await searchProfiles({
    query: "nobody", limit: 50, budgetMs: 4_000, scanPages: 8, cursor: page.nextCursor,
    now: () => clock,
    readPage: async (u) => { clock += 3_300; return relay.readPage(u); },
    readAuthor: async () => [], match: () => null,
  });
  assert.notEqual(more.nextCursor, page.nextCursor, "the resumed page advanced");
});

test("a fast relay still gets the coverage: the same budget buys more pages", async () => {
  // The point of budgeting time rather than pages — on a release fleet where a
  // page is cheap, the walk goes as deep as it always could.
  let clock = 0, pageReads = 0;
  const relay = relayOf(Array.from({ length: 100_000 }, (_, i) => profile(`pk${i}`, 9_000_000 - i, { id: ID(i) })));
  await searchProfiles({
    query: "nobody", limit: 50, budgetMs: 4_000, scanPages: 8,
    now: () => clock,
    readPage: async (u) => { pageReads += 1; clock += 50; return relay.readPage(u); },
    readAuthor: async () => [], match: () => null,
  });
  assert.equal(pageReads, 8, "at 50ms a page the round backstop is what binds, not the clock");
});
