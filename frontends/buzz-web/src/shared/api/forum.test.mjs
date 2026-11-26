import assert from "node:assert/strict";
import test from "node:test";
import { finalizeEvent, getPublicKey } from "nostr-tools/pure";

import { getForumPosts, getForumThread } from "./forum.ts";

// F409: forum posts and replies are attributed exactly like the channel
// timeline: a relay-signed `actor` tag names the author, and a leading `p`
// tag only counts on a channel-scoped (`h`-tagged) event.

const RELAY_SECRET = new Uint8Array(32).fill(2);
const RELAY = getPublicKey(RELAY_SECRET);
const AGENT = "44".repeat(32);
const MENTIONED = "55".repeat(32);
const CHANNEL_ID = "36411e44-0e2d-4cfe-bd6e-567eb169db9f";

function relaySigned(kind, tags) {
  return finalizeEvent(
    { kind, created_at: 1_700_000_000, content: "hello", tags },
    RELAY_SECRET,
  );
}

function rawPost(event) {
  return {
    event_id: event.id,
    pubkey: event.pubkey,
    content: event.content,
    kind: event.kind,
    created_at: event.created_at,
    channel_id: CHANNEL_ID,
    tags: event.tags,
    sig: event.sig,
    thread_summary: null,
    reactions: null,
  };
}

function rawReply(event) {
  return {
    ...rawPost(event),
    parent_event_id: null,
    root_event_id: null,
    depth: 1,
    broadcast: false,
  };
}

async function withTauri(handlers, run) {
  const previousWindow = globalThis.window;
  globalThis.window = {
    __TAURI_INTERNALS__: {
      invoke(command) {
        const handler = handlers[command];
        if (!handler) {
          return Promise.reject(new Error(`Unexpected command: ${command}`));
        }
        return Promise.resolve(handler());
      },
    },
  };
  try {
    await run();
  } finally {
    globalThis.window = previousWindow;
  }
}

test("F409: a relay-signed forum post with an actor tag is attributed to the actor", async () => {
  const post = relaySigned(45001, [
    ["p", MENTIONED],
    ["h", CHANNEL_ID],
    ["actor", AGENT],
  ]);
  await withTauri(
    { get_forum_posts: () => ({ messages: [rawPost(post)], next_cursor: null }) },
    async () => {
      const { posts } = await getForumPosts(CHANNEL_ID, 10, undefined, RELAY);
      assert.equal(posts.length, 1);
      assert.equal(posts[0].pubkey, AGENT);
    },
  );
});

test("F409: a leading p tag on a relay-signed event without a channel tag is not an author", async () => {
  const root = relaySigned(45001, [["p", MENTIONED]]);
  const reply = relaySigned(45003, [["p", MENTIONED], ["e", root.id]]);
  await withTauri(
    {
      get_forum_thread: () => ({
        root: rawPost(root),
        replies: [rawReply(reply)],
        total_replies: 1,
        next_cursor: null,
      }),
    },
    async () => {
      const thread = await getForumThread(
        CHANNEL_ID,
        root.id,
        10,
        undefined,
        RELAY,
      );
      assert.equal(thread.post.pubkey, RELAY);
      assert.equal(thread.replies[0].pubkey, RELAY);
    },
  );
});

test("F409: a channel-scoped reply with an actor tag matches the timeline author", async () => {
  const root = relaySigned(45001, [["h", CHANNEL_ID]]);
  const reply = relaySigned(45003, [
    ["p", MENTIONED],
    ["h", CHANNEL_ID],
    ["actor", AGENT],
  ]);
  await withTauri(
    {
      get_forum_thread: () => ({
        root: rawPost(root),
        replies: [rawReply(reply)],
        total_replies: 1,
        next_cursor: null,
      }),
    },
    async () => {
      const thread = await getForumThread(
        CHANNEL_ID,
        root.id,
        10,
        undefined,
        RELAY,
      );
      assert.equal(thread.replies[0].pubkey, AGENT);
    },
  );
});
