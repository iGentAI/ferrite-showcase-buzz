// `get_forum_posts` pages a forum's index over the relay's `/window` route,
// whose opaque cursor moves past a WHOLE page. The walk used to request a full
// `limit` rows per page and stop mid-page once its answer was full, then
// resume after the entire page: whenever filtered rows (non-post kinds, or rows
// above a cold `before`) made the answer fill mid-page, the rest of that page
// was never returned by any call.
import assert from "node:assert/strict";
import test from "node:test";

import { getForumPosts } from "./workspace.ts";

test("the public edit API reaches the browser relay with its wrapped IPC input", async () => {
  // The public wrapper, bridge and dispatch remain real. Only the identity and
  // relay I/O are replaced, so a dropped IPC envelope cannot hide behind a mock.
  const previousWindow = globalThis.window;
  const author = "ab".repeat(32);
  globalThis.window = {
    nostr: {
      getPublicKey: async () => author,
      signEvent: async (event) => ({ ...event, pubkey: author }),
    },
  };
  const { relayClient } = await import("../api/relayClient.ts");
  const fetch = relayClient.fetchEvents;
  const publish = relayClient.publishEvent;
  let targetAuthor = author;
  const published = [];
  relayClient.fetchEvents = async (filter) => {
    assert.deepEqual(filter.ids, ["saved-message"]);
    return [{ id: "saved-message", kind: 9, pubkey: targetAuthor, tags: [["h", "private-room"]] }];
  };
  relayClient.publishEvent = async (event) => { published.push(event); return event; };
  try {
    const { editMessage } = await import("../api/editMessage.ts");
    await editMessage("private-room", "saved-message", "replacement text");
    assert.equal(published.length, 1);
    assert.equal(published[0].kind, 40003);
    assert.equal(published[0].content, "replacement text");
    assert.deepEqual(published[0].tags, [["h", "private-room"], ["e", "saved-message"]]);
    // Removing link previews and keeping mention references are real tags.
    await editMessage(
      "private-room",
      "saved-message",
      " kept ",
      [],
      [],
      [],
      true,
      [["mention", "x"]],
    );
    assert.equal(published[1].content, "kept");
    assert.deepEqual(published[1].tags, [
      ["h", "private-room"],
      ["e", "saved-message"],
      ["mention", "x"],
      ["buzz:mention-snapshot"],
      ["link-preview", "none"],
    ]);
    await assert.rejects(
      editMessage("private-room", "saved-message", "bad", [["p", "x"]]),
      /only "imeta" tags/,
    );
    published.length = 1;
    targetAuthor = "cd".repeat(32);
    await assert.rejects(editMessage("private-room", "saved-message", "foreign edit"), /only edit your own/);
    assert.equal(published.length, 1, "a foreign edit must never publish");
  } finally {
    relayClient.fetchEvents = fetch;
    relayClient.publishEvent = publish;
    globalThis.window = previousWindow;
  }
});

const KIND_FORUM_POST = 45001;
const KIND_OTHER = 45003;

/** Rows newest first, as `/window` serves them. `kinds` is one kind per row. */
function channelRows(kinds) {
  return kinds.map((kind, index) => ({
    event: {
      id: `${String(index).padStart(4, "0")}${"e".repeat(60)}`,
      pubkey: "a".repeat(64),
      kind,
      created_at: 10_000 - index * 10,
      content: `row ${index}`,
      tags: [],
      sig: "",
    },
    thread: { reply_count: 0, descendant_count: 0, last_reply_at: null },
  }));
}

/** A `/window` stand-in: the cursor is the offset of the next unread row. */
function fakeWindow(rows) {
  const requests = [];
  const post = async (path, body) => {
    assert.equal(path, "/window");
    requests.push(body);
    const start = body.cursor === undefined ? 0 : Number(body.cursor);
    const end = Math.min(start + body.limit, rows.length);
    return {
      events: rows.slice(start, end),
      bounds: {
        has_more: end < rows.length,
        next_cursor: end < rows.length ? String(end) : null,
      },
    };
  };
  return { post, requests };
}

/** Page with `limit` until the index reports no more; every id returned. */
async function walk(channelId, rows, limit, before = null) {
  const { post } = fakeWindow(rows);
  const seen = [];
  let cursor = before;
  for (let call = 0; call < 100; call += 1) {
    const page = await getForumPosts({ channelId, limit, before: cursor }, post);
    assert.ok(page.messages.length <= limit, "an answer never exceeds its limit");
    seen.push(...page.messages.map((message) => message.event_id));
    if (page.next_cursor === null) return seen;
    cursor = page.next_cursor;
  }
  throw new Error("the walk did not terminate");
}

function postIds(rows, predicate = () => true) {
  return rows
    .filter(({ event }) => event.kind === KIND_FORUM_POST && predicate(event))
    .map(({ event }) => event.id);
}

test("paging from the top returns every post once when other kinds are mixed in", async () => {
  const kinds = [];
  for (let index = 0; index < 40; index += 1) {
    kinds.push(index % 3 === 1 ? KIND_OTHER : KIND_FORUM_POST);
  }
  const rows = channelRows(kinds);
  for (const limit of [1, 2, 3, 5, 7]) {
    assert.deepEqual(await walk(`forum-top-${limit}`, rows, limit), postIds(rows), `limit ${limit}`);
  }
});

test("a cold `before` filters the first pages without stranding what follows", async () => {
  const rows = channelRows(Array.from({ length: 30 }, (_, index) => (index % 4 === 2 ? KIND_OTHER : KIND_FORUM_POST)));
  const before = rows[5].event.created_at;
  for (const limit of [2, 3, 4]) {
    assert.deepEqual(
      await walk(`forum-before-${limit}`, rows, limit, before),
      postIds(rows, (event) => event.created_at < before),
      `limit ${limit}`,
    );
  }
});

test("each page asks the relay for only the capacity left in the answer", async () => {
  const rows = channelRows([KIND_FORUM_POST, KIND_OTHER, KIND_FORUM_POST, KIND_FORUM_POST, KIND_FORUM_POST]);
  const { post, requests } = fakeWindow(rows);
  const page = await getForumPosts({ channelId: "forum-capacity", limit: 3 }, post);
  assert.deepEqual(
    requests.map((request) => request.limit),
    [3, 1],
    "the second page is sized to the one slot left",
  );
  assert.deepEqual(page.messages.map((message) => message.content), ["row 0", "row 2", "row 3"]);
  assert.equal(page.next_cursor, rows[3].event.created_at);
});
