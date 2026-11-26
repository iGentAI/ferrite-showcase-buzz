// `POST /buzz/api/search` answers `{events, complete, next_cursor}`, and
// `complete` is its exhaustion authority: the walk is bounded and filtered
// after the fact, so a short or empty page is not "no more matches" (F422).
import assert from "node:assert/strict";
import test from "node:test";

import { searchMessages } from "./search.ts";
import { buildSearchHitEvent } from "@/app/navigation/searchHitEventCache";
import { formatTimelineMessages } from "@/features/messages/lib/formatTimelineMessages";

const hit = (n) => ({
  id: `${String(n).padStart(4, "0")}${"e".repeat(60)}`,
  pubkey: "a".repeat(64),
  created_at: 1_000 - n,
  kind: 9,
  tags: [["h", "general"]],
  content: `deploy ${n}`,
});

/** A relay whose pages are scripted by the cursor the caller sends. */
function scriptedRelay(pages) {
  const bodies = [];
  const post = async (path, body) => {
    assert.equal(path, "/search");
    bodies.push(body);
    const page = pages[body.cursor ?? "start"];
    assert.ok(page, `unexpected cursor ${body.cursor}`);
    return page;
  };
  return { bodies, post };
}

test("an empty but incomplete first page is followed, not reported as no matches", async () => {
  const relay = scriptedRelay({
    start: { events: [], complete: false, next_cursor: "c1" },
    c1: { events: [hit(1), hit(2)], complete: true, next_cursor: null },
  });
  const result = await searchMessages({ q: "deploy", limit: 10, channelId: "general" }, relay.post);
  assert.equal(result.found, 2);
  assert.equal(result.complete, true);
  assert.equal(result.next_cursor, null);
  assert.deepEqual(relay.bodies.map((b) => b.cursor), [undefined, "c1"]);
  assert.equal(relay.bodies[1].limit, 10, "asks only for what is still missing");
  assert.equal(relay.bodies[1].channel, "general");
});

test("a filled answer stops and hands back the relay's cursor for more", async () => {
  const relay = scriptedRelay({
    start: { events: [hit(1)], complete: false, next_cursor: "c1" },
    c1: { events: [hit(2), hit(3)], complete: false, next_cursor: "c2" },
  });
  const result = await searchMessages({ q: "deploy", limit: 3 }, relay.post);
  assert.equal(result.found, 3);
  assert.equal(result.complete, false);
  assert.equal(result.next_cursor, "c2");
  assert.equal(relay.bodies[1].limit, 2);
});

test("the follow-up requests are bounded and an unfinished walk says so", async () => {
  const pages = {};
  for (let i = 0; i < 20; i += 1) {
    pages[i === 0 ? "start" : `c${i}`] = { events: [], complete: false, next_cursor: `c${i + 1}` };
  }
  const relay = scriptedRelay(pages);
  const result = await searchMessages({ q: "deploy" }, relay.post);
  assert.equal(relay.bodies.length, 8);
  assert.equal(result.found, 0);
  assert.equal(result.complete, false);
  assert.equal(result.next_cursor, "c8");
});

test("a caller's cursor resumes the walk, and a blank query asks nothing", async () => {
  const relay = scriptedRelay({ c5: { events: [hit(9)], complete: true, next_cursor: null } });
  const result = await searchMessages({ q: "deploy", cursor: "c5" }, relay.post);
  assert.equal(result.hits[0].channel_id, "general");
  assert.equal(result.complete, true);
  const none = await searchMessages({ q: "   " }, relay.post);
  assert.deepEqual(none, { hits: [], found: 0, complete: true, next_cursor: null });
  assert.equal(relay.bodies.length, 1);
});


test("a signed edit links search to its original message with the current wording", async () => {
  const original = hit(1);
  const edited = { ...hit(2), kind: 40003, tags: [["h", "general"], ["e", original.id]], content: "updated release" };
  const relay = scriptedRelay({ start: {events: [edited], complete: true} });
  const result = await searchMessages({q: "updated"}, relay.post);
  assert.equal(result.hits[0].event_id, original.id);
  assert.equal(result.hits[0].content, "updated release");
  assert.equal(result.hits[0].channel_id, "general");
  const raw = result.hits[0];
  const selected = buildSearchHitEvent({
    eventId: raw.event_id, content: raw.content, kind: raw.kind,
    pubkey: raw.pubkey, channelId: raw.channel_id,
    channelName: raw.channel_name, createdAt: raw.created_at, score: raw.score,
  });
  const rendered = formatTimelineMessages([selected], null, undefined, null);
  assert.equal(rendered.length, 1, "opening the edited search hit must render its target without a reload");
  assert.equal(rendered[0].body, "updated release");
  assert.equal(rendered[0].id, original.id);
});
