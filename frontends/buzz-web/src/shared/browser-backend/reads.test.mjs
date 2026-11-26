/**
 * The pure helpers behind the relay-backed reads (`reads.ts`), tested the way
 * `desktopDispatch.test.mjs` is: plain `node --test`, no bundler, the module
 * imported directly. Every function here is a boundary rule — a cursor, an
 * order, a depth, a tally — and each test pins the boundary itself, because a
 * paging rule that is wrong by one row repeats or drops that row forever.
 */
import test from "node:test";
import assert from "node:assert/strict";

import {
  capPerAuthor,
  cursorFrom,
  depthsFor,
  isBroadcast,
  newestFirst,
  pageOf,
  parentOf,
  pastCursor,
  pubkeysOf,
  reactionTarget,
  tallyReactions,
  threadCursorFrom,
  threadCursorOf,
  untilFrom,
} from "./reads.ts";

const row = (created_at, id, extra = {}) => ({ created_at, id, ...extra });

test("ordering is newest first with a stable id tie-break", () => {
  const rows = [row(5, "b"), row(5, "a"), row(9, "z"), row(1, "m")];
  const sorted = [...rows].sort(newestFirst);
  assert.deepEqual(
    sorted.map((r) => r.id),
    ["z", "a", "b", "m"],
  );
  // The tie-break is what keeps two same-second rows in ONE order everywhere:
  // paging on an unstable order repeats one row and drops another.
  assert.equal(newestFirst(row(5, "a"), row(5, "a")), 0);
});

test("a cursor is read only when it is usable", () => {
  assert.equal(cursorFrom({}), null);
  assert.equal(cursorFrom({ before: 0 }), null);
  assert.equal(cursorFrom({ before: -5, beforeId: "x" }), null);
  assert.deepEqual(cursorFrom({ before: 7, beforeId: "abc" }), {
    before: 7,
    beforeId: "abc",
  });
  // An old caller's bare timestamp reads as "strictly before this second":
  // it may skip same-second rows, but it can never repeat them forever.
  assert.deepEqual(cursorFrom({ before: 7 }), { before: 7, beforeId: "" });
});

test("the cursor boundary is exact within one second", () => {
  const cursor = { before: 10, beforeId: "m" };
  assert.equal(pastCursor(row(9, "a"), cursor), true, "an older second is past");
  assert.equal(pastCursor(row(11, "a"), cursor), false, "a newer second is not");
  // Same second: the page ran ascending ids, so only ids AFTER the cursor's remain.
  assert.equal(pastCursor(row(10, "n"), cursor), true);
  assert.equal(pastCursor(row(10, "m"), cursor), false, "the cursor row was shown");
  assert.equal(pastCursor(row(10, "a"), cursor), false);
  // A bare-timestamp cursor skips the boundary second rather than repeat it.
  assert.equal(pastCursor(row(10, "zz"), { before: 10, beforeId: "" }), false);
  assert.equal(pastCursor(row(4, "zz"), null), true, "no cursor, everything shows");
});

test("a page resumes exactly and treats a capped fetch as probably-more", () => {
  const rows = [row(5, "b"), row(5, "a"), row(9, "z"), row(4, "q")];
  const first = pageOf(rows, 2, null, false);
  assert.deepEqual(
    first.rows.map((r) => r.id),
    ["z", "a"],
  );
  assert.deepEqual(first.next, { created_at: 5, id: "a" });
  // Resuming after the same-second row "a" shows "b" — same second, later id —
  // then "q": nothing repeated, nothing skipped.
  const second = pageOf(rows, 2, { before: 5, beforeId: "a" }, false);
  assert.deepEqual(
    second.rows.map((r) => r.id),
    ["b", "q"],
  );
  assert.equal(second.next, null, "everything fetched was shown");
  // A FULL upstream page offers a cursor even when this page showed it all:
  // the cap is what produced the full result, so more probably sits behind it.
  const capped = pageOf(rows, 10, null, true);
  assert.equal(capped.rows.length, 4);
  assert.deepEqual(capped.next, { created_at: 4, id: "q" });
  // An empty result has no cursor to offer, capped or not.
  assert.equal(pageOf([], 5, null, true).next, null);
});

test("the until bound is inclusive, because NIP-01's until is", () => {
  assert.deepEqual(untilFrom(null), {});
  // `before`, not `before - 1`: an exclusive bound would drop every row sharing
  // the cursor's second — pastCursor removes the overlap instead.
  assert.deepEqual(untilFrom({ before: 42, beforeId: "x" }), { until: 42 });
});

test("a thread cursor round-trips through its string form", () => {
  assert.equal(threadCursorOf(null), null);
  const text = threadCursorOf({ created_at: 12, id: "ab:cd" });
  assert.equal(text, "12:ab:cd");
  // The id half may itself contain colons; only the FIRST one separates.
  assert.deepEqual(threadCursorFrom(text), { before: 12, beforeId: "ab:cd" });
  assert.equal(threadCursorFrom(""), null);
  assert.equal(threadCursorFrom("junk:x"), null);
  assert.equal(threadCursorFrom(42), null, "a cursor is a string or nothing");
  assert.deepEqual(threadCursorFrom("15"), { before: 15, beforeId: "" });
});

test("a reply's parent is read from NIP-10's marked form first", () => {
  const root = "root";
  assert.equal(
    parentOf(
      [
        ["e", "p", "", "reply"],
        ["e", root, "", "root"],
      ],
      root,
    ),
    "p",
    "an explicit reply marker is unambiguous",
  );
  assert.equal(
    parentOf([["e", root, "", "root"]], root),
    root,
    "a marked root alone means a direct reply to the root",
  );
  // Deprecated positional form: the LAST e tag is the parent.
  assert.equal(parentOf([["e", root], ["e", "mid"], ["e", "p"]], root), "p");
  assert.equal(parentOf([["p", "somebody"]], root), root, "no e tags: direct reply");
  assert.equal(parentOf([], ""), null);
});

test("thread depth is the relay's rule, with honest fallbacks", () => {
  const root = "r";
  const replies = [
    { id: "a", tags: [["e", root, "", "root"]] },
    { id: "b", tags: [["e", "a", "", "reply"]] },
    { id: "c", tags: [["e", "b", "", "reply"]] },
    // A reply whose parent is NOT in the fetched set.
    { id: "d", tags: [["e", "missing", "", "reply"]] },
    // A tag cycle: neither row may loop, and neither may claim the top level.
    { id: "x", tags: [["e", "y", "", "reply"]] },
    { id: "y", tags: [["e", "x", "", "reply"]] },
  ];
  const depths = depthsFor(root, replies);
  assert.equal(depths.get("a"), 1);
  assert.equal(depths.get("b"), 2);
  assert.equal(depths.get("c"), 3);
  assert.equal(depths.get("d"), 2, "an unheld parent falls back to depth 2");
  assert.equal(depths.get("y"), 2, "a cycle refuses to loop or claim top level");
  assert.equal(depths.get("x"), 3, "…and the row above it stacks on that answer");
});

test("a reaction targets its LAST e tag and tallies dedup by author", () => {
  // NIP-25: a threaded reaction carries the root AND its parent; only the last
  // e tag is the target. Crediting every e tag double-counts one reaction.
  assert.equal(
    reactionTarget([
      ["e", "root"],
      ["p", "x"],
      ["e", "note"],
    ]),
    "note",
  );
  assert.equal(reactionTarget([["p", "x"]]), null);

  const events = [
    { pubkey: "p1", content: "\u{1F44D}", tags: [["e", "root"], ["e", "n1"]] },
    // The same author re-published: counts once.
    { pubkey: "p1", content: "\u{1F44D}", tags: [["e", "n1"]] },
    { pubkey: "p2", content: "\u{1F44D}", tags: [["e", "n1"]] },
    { pubkey: "p3", content: "\u2764", tags: [["e", "n1"]] },
    // A reaction to a note nobody asked about is ignored.
    { pubkey: "p4", content: "\u{1F44D}", tags: [["e", "other"]] },
    // Blank content is not an emoji.
    { pubkey: "p5", content: "  ", tags: [["e", "n1"]] },
  ];
  const tallies = tallyReactions(events, ["n1"]);
  assert.deepEqual(tallies, [
    { note_id: "n1", emoji: "\u2764", count: 1, pubkeys: ["p3"] },
    { note_id: "n1", emoji: "\u{1F44D}", count: 2, pubkeys: ["p1", "p2"] },
  ]);
});

test("the per-author cap keeps each author's newest rows", () => {
  const rows = [
    row(1, "a1", { pubkey: "a" }),
    row(9, "a9", { pubkey: "a" }),
    row(5, "a5", { pubkey: "a" }),
    row(7, "b7", { pubkey: "b" }),
  ];
  const capped = capPerAuthor(rows, 2);
  assert.deepEqual(
    capped.map((r) => r.id),
    ["a9", "b7", "a5"],
    "a1 fell off a's cap; the rest re-sorted newest first",
  );
});

test("only well-formed pubkeys and real broadcast markers pass", () => {
  const good = "a".repeat(64);
  assert.deepEqual(pubkeysOf([good.toUpperCase(), "junk", 5, null]), [good]);
  assert.deepEqual(pubkeysOf("not-an-array"), []);
  assert.equal(isBroadcast([["broadcast", "1"]]), true);
  assert.equal(isBroadcast([["broadcast", "0"]]), false);
  assert.equal(isBroadcast([]), false);
});

test("notes already returned at the cursor second do not use up an author's quota", async () => {
  const { notesTimelinePage } = await import("./reads.ts");
  const seen = { pubkey: "a", created_at: 100, id: "a1" };
  const older = { pubkey: "a", created_at: 90, id: "a2" };
  const cursor = { before: 100, beforeId: "a1" };
  const { rows } = notesTimelinePage([seen, older], 1, cursor, false);
  assert.deepEqual(rows.map((r) => r.id), ["a2"]);
});

test("a quote of a forum post is not a reply in its thread", async () => {
  const { isThreadReplyOf } = await import("./reads.ts");
  const root = "r".repeat(64);
  assert.equal(isThreadReplyOf([["e", root, "", "root"]], root), true);
  assert.equal(isThreadReplyOf([["e", root, "", "root"], ["e", "p", "", "reply"]], root), true);
  assert.equal(isThreadReplyOf([["e", root, "", "mention"]], root), false);
  assert.equal(isThreadReplyOf([["e", "other", "", "root"], ["e", root, "", "mention"]], root), false);
  assert.equal(isThreadReplyOf([["e", root]], root), true, "legacy positional form");
});
