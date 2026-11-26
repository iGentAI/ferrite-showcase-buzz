// Run: node --import ./test-loader.mjs --test e2e/sweep.test.mjs
//
// Drives sweep.ts's `walk` against a fake filter-query endpoint with the
// relay's paging rules: newest first, `until` inclusive, at most `limit`.
import assert from "node:assert/strict";
import test from "node:test";

delete process.env.BUZZ_SSO_ORIGIN;
// `walk` takes its key as an argument; no ambient operator key may be used.
delete process.env.BUZZ_OPERATOR_STATE;
let store = [];
globalThis.fetch = async (url, init = {}) => {
  assert.match(String(url), /\/buzz\/api\/filter-query$/);
  const [filter] = JSON.parse(init.body);
  const page = store
    .filter((event) => filter.until === undefined || event.created_at <= filter.until)
    .sort((a, b) => b.created_at - a.created_at || (a.id < b.id ? -1 : 1))
    .slice(0, filter.limit);
  return new Response(JSON.stringify(page), { status: 200 });
};

const { walk, PAGE_LIMIT, PAGE_BUDGET } = await import("./sweep.ts");
const operator = new Uint8Array(32).fill(7);
const event = (i, created_at) => ({ id: `e${String(i).padStart(6, "0")}`, kind: 39000, pubkey: "p", created_at, tags: [] });

test("F328: a healthy walk with shared timestamps finds everything and says so", async () => {
  store = Array.from({ length: 1234 }, (_, i) => event(i, 1_000_000 - Math.floor(i / 40)));
  const found = await walk(operator, { kinds: [39000] });
  assert.equal(found.events.length, 1234);
  assert.equal(found.complete, true);
  assert.deepEqual(found.gaps, []);
});

test("F328: more events at one timestamp than a page holds is reported, and older events are still read", async () => {
  store = [
    ...Array.from({ length: PAGE_LIMIT + 200 }, (_, i) => event(i, 2_000_000)),
    ...Array.from({ length: 100 }, (_, i) => event(10_000 + i, 1_999_000 - i)),
  ];
  const found = await walk(operator, { kinds: [39000] });
  assert.equal(found.complete, false);
  assert.match(found.gaps.join("\n"), /all have created_at 2000000/);
  assert.equal(found.events.filter((e) => e.created_at < 2_000_000).length, 100, "the walk still reached the older events");
});

test("F328: running out of page budget is reported instead of returning a partial inventory as complete", async () => {
  const total = PAGE_BUDGET * PAGE_LIMIT + 500;
  store = Array.from({ length: total }, (_, i) => event(i, 5_000_000 - i));
  const found = await walk(operator, { kinds: [39000] });
  assert.equal(found.complete, false);
  assert.match(found.gaps.join("\n"), new RegExp(`stopped after ${PAGE_BUDGET} pages`));
  assert.ok(found.events.length < total);
});
