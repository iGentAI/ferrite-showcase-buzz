import assert from "node:assert/strict";
import test from "node:test";
import { windowBoundsKey } from "@/shared/api/channelWindowKey";
import {
  parseChannelWindowResponse,
  parseLiveThreadSummary,
} from "./channelWindowResponse.ts";

function event(id, kind, createdAt, content = "", tags = []) {
  return {
    id: id.padEnd(64, "0"),
    pubkey: "a".repeat(64),
    created_at: createdAt,
    kind,
    tags: [["h", "channel"], ...tags],
    content,
    sig: "b".repeat(128),
  };
}

test("partitions flat rows, summaries, aux, and authoritative bounds", () => {
  const root = event("a", 9, 100);
  const summary = event(
    "s",
    39005,
    200,
    JSON.stringify({
      reply_count: 2,
      descendant_count: 3,
      last_reply_at: 150,
      participants: ["c".repeat(64)],
    }),
    [
      ["e", root.id],
      ["d", root.id],
    ],
  );
  const aux = event("x", 7, 300, "+", [["e", root.id]]);
  const bounds = event(
    "b",
    39006,
    400,
    JSON.stringify({
      has_more: true,
      next_cursor: { created_at: 100, id: root.id },
    }),
    [["d", "channel:head"]],
  );
  const page = parseChannelWindowResponse(
    [summary, aux, bounds, root],
    "channel",
    null,
  );
  assert.equal(page.rows.length, 1);
  assert.equal(page.rows[0].thread.replyCount, 2);
  assert.deepEqual(
    page.aux.map((item) => item.id),
    [aux.id],
  );
  assert.deepEqual(page.nextCursor, { createdAt: 100, eventId: root.id });
  assert.equal(page.hasMore, true);
});

test("metadata timestamps never influence row cursor math", () => {
  const root = event("a", 9, 100);
  const bounds = event(
    "b",
    39006,
    9999,
    JSON.stringify({ has_more: false, next_cursor: null }),
    [["d", "channel:head"]],
  );
  const page = parseChannelWindowResponse([bounds, root], "channel", null);
  assert.equal(page.nextCursor, null);
  assert.deepEqual(
    page.rows.map((row) => row.event.id),
    [root.id],
  );
});

test("rejects bounds signed for a different request cursor", () => {
  const root = event("a", 9, 100);
  const cursor = { createdAt: 200, eventId: "c".repeat(64) };
  const bounds = event(
    "b",
    39006,
    300,
    JSON.stringify({ has_more: false, next_cursor: null }),
    [["d", `channel:${cursor.createdAt}:${"d".repeat(64)}`]],
  );

  assert.throws(
    () => parseChannelWindowResponse([root, bounds], "channel", cursor),
    /do not match the request cursor/,
  );
});

test("accepts canonical composite request cursor binding", () => {
  const root = event("a", 9, 100);
  const cursor = { createdAt: 200, eventId: "C".repeat(64) };
  const bounds = event(
    "b",
    39006,
    300,
    JSON.stringify({ has_more: false, next_cursor: null }),
    [["d", `channel:200:${"c".repeat(64)}`]],
  );

  assert.doesNotThrow(() =>
    parseChannelWindowResponse([root, bounds], "CHANNEL", cursor),
  );
});

test("rejects absent or contradictory signed bounds", () => {
  const root = event("a", 9, 100);
  assert.throws(
    () => parseChannelWindowResponse([root], "channel", null),
    /exactly one bounds/,
  );
  const bad = event(
    "b",
    39006,
    200,
    JSON.stringify({ has_more: true, next_cursor: null }),
    [["d", "channel:head"]],
  );
  assert.throws(
    () => parseChannelWindowResponse([root, bad], "channel", null),
    /disagree/,
  );
});

test("two-page scroll-back round trip: page 2's bounds answer the REQUEST cursor", () => {
  // The synthesiser (`commands.ts channelWindowFromRelay`) keys each page's
  // 39006 with `windowBoundsKey(channel, requestCursor)` — the same function
  // the parser expects with. This drives the exact sequence that used to
  // throw on every second page, when the key was derived from the fetched
  // page's newest row instead of the request cursor.
  const boundsFor = (requestCursor, payload, createdAt) =>
    event("b", 39006, createdAt, JSON.stringify(payload), [
      ["d", windowBoundsKey("channel", requestCursor)],
    ]);

  // Page 1: the head page (cursor null), newest-first, with more behind it.
  const page1 = parseChannelWindowResponse(
    [
      event("1", 9, 200),
      event("2", 9, 150),
      boundsFor(
        null,
        // next_cursor.created_at echoes this page's newest row — the same
        // number must come back on the next request and compare equal.
        { has_more: true, next_cursor: { created_at: 200, id: "0123abc" } },
        200,
      ),
    ],
    "channel",
    null,
  );
  assert.equal(page1.rows.length, 2);
  assert.equal(page1.hasMore, true);
  assert.deepEqual(page1.nextCursor, { createdAt: 200, eventId: "0123abc" });

  // Page 2: requested with page 1's nextCursor. Its rows are strictly OLDER
  // than the cursor's created_at (the timeline is newest-first), so a key
  // derived from THIS page's newest row (100) can never match the request.
  const requestCursor = page1.nextCursor;
  const page2 = parseChannelWindowResponse(
    [
      event("3", 9, 100),
      event("4", 9, 50),
      boundsFor(requestCursor, { has_more: false, next_cursor: null }, 100),
    ],
    "channel",
    requestCursor,
  );
  assert.equal(page2.rows.length, 2);
  assert.equal(page2.hasMore, false);
  assert.equal(page2.nextCursor, null);

  // The old bug's wire shape — the key built from page 2's own newest row —
  // stays refused, so the regression cannot come back silently.
  assert.throws(
    () =>
      parseChannelWindowResponse(
        [
          event("3", 9, 100),
          boundsFor(
            { createdAt: 100, eventId: "0123abc" },
            { has_more: false, next_cursor: null },
            100,
          ),
        ],
        "channel",
        requestCursor,
      ),
    /do not match the request cursor/,
  );
});

test("parses a relay-pushed live thread summary", () => {
  const rootId = "a".padEnd(64, "0");
  const push = event(
    "s",
    39005,
    700,
    JSON.stringify({
      reply_count: 4,
      descendant_count: 6,
      last_reply_at: 650,
      participants: ["c".repeat(64)],
    }),
    [
      ["e", rootId],
      ["d", rootId],
    ],
  );
  const parsed = parseLiveThreadSummary(push);
  assert.equal(parsed.rootId, rootId);
  assert.equal(parsed.live.createdAt, 700);
  assert.deepEqual(parsed.live.summary, {
    replyCount: 4,
    descendantCount: 6,
    lastReplyAt: 650,
    participantPubkeys: ["c".repeat(64)],
  });
});

test("drops malformed or mistargeted live thread summaries", () => {
  const rootId = "a".padEnd(64, "0");
  // Wrong kind.
  assert.equal(parseLiveThreadSummary(event("x", 9, 700, "hi")), null);
  // No e-tag root.
  assert.equal(parseLiveThreadSummary(event("s", 39005, 700, "{}")), null);
  // Unparseable content only skips one refresh instead of throwing.
  assert.equal(
    parseLiveThreadSummary(event("s", 39005, 700, "not json", [["e", rootId]])),
    null,
  );
});
