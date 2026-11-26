import assert from "node:assert/strict";
import test from "node:test";

import {
  fetchThreadSubtree,
  isRelayEventId,
  outgoingMessageTags,
  sendChannelMessageExtraTags,
  sendChannelMessageViaRelay,
  threadDepthOf,
  threadRepliesPage,
  validatedExtraTags,
  validatedLinkPreviewTags,
  validatedSentFromThreadTag,
} from "./messageSend.ts";

const AUTHOR = "a".repeat(64);
const MENTION = "b".repeat(64);
const ROOT = "1".repeat(64);
const PARENT = "2".repeat(64);
const CHANNEL = "design";

/** A minimal stored event; only id/created_at/tags matter to these helpers. */
function event(id, createdAt, tags) {
  return {
    id,
    pubkey: AUTHOR,
    created_at: createdAt,
    kind: 9,
    tags,
    content: `msg ${id.slice(0, 4)}`,
    sig: "",
  };
}

test("outgoingMessageTags — a reply to the root carries h, author p, and one reply-marked e", () => {
  assert.deepEqual(
    outgoingMessageTags({
      channelId: CHANNEL,
      authorPubkey: AUTHOR,
      parentEventId: ROOT,
      rootEventId: ROOT,
      mentionPubkeys: [],
      extraTags: [],
    }),
    [
      ["p", AUTHOR],
      ["h", CHANNEL],
      ["e", ROOT, "", "reply"],
    ],
  );
});

test("outgoingMessageTags — a nested reply marks root AND reply, and mention ps ride after the author", () => {
  assert.deepEqual(
    outgoingMessageTags({
      channelId: CHANNEL,
      authorPubkey: AUTHOR,
      parentEventId: PARENT,
      rootEventId: ROOT,
      mentionPubkeys: [MENTION, AUTHOR, MENTION.toUpperCase()],
      extraTags: [],
    }),
    [
      ["p", AUTHOR],
      ["h", CHANNEL],
      // Self and the case-only duplicate are dropped, as the send path's
      // normalization (and the relay's authoritative copy of it) drops them.
      ["p", MENTION],
      ["e", ROOT, "", "root"],
      ["e", PARENT, "", "reply"],
    ],
  );
});

test("outgoingMessageTags — a top-level message is h + author p + mention ps, then the extras", () => {
  assert.deepEqual(
    outgoingMessageTags({
      channelId: CHANNEL,
      authorPubkey: AUTHOR,
      parentEventId: null,
      rootEventId: null,
      mentionPubkeys: [MENTION],
      extraTags: [["imeta", "url https://x/img.png", "m image/png"]],
    }),
    [
      ["h", CHANNEL],
      ["p", AUTHOR],
      ["p", MENTION],
      ["imeta", "url https://x/img.png", "m image/png"],
    ],
  );
});

test("validatedExtraTags — a NIP-30 emoji tag survives the send verbatim", () => {
  const emoji = ["emoji", "partyparrot", "https://cdn/x/partyparrot.gif"];
  const extras = validatedExtraTags({ emojiTags: [emoji] });
  assert.deepEqual(extras, [emoji]);
  const tags = outgoingMessageTags({
    channelId: CHANNEL,
    authorPubkey: AUTHOR,
    parentEventId: ROOT,
    rootEventId: ROOT,
    mentionPubkeys: [],
    extraTags: extras,
  });
  assert.deepEqual(tags[tags.length - 1], emoji);
});

test("validatedExtraTags — a foreign prefix on a validated channel is refused, not signed", () => {
  // The injection defense splitOutgoingTags documents: content that became a
  // tag must be refused by the arg gate, exactly as the desktop command does.
  assert.throws(
    () => validatedExtraTags({ mediaTags: [["e", ROOT, "", "reply"]] }),
    /only "imeta" tags/,
  );
  assert.throws(
    () => validatedExtraTags({ emojiTags: [["imeta", "url https://x"]] }),
    /only "emoji" tags/,
  );
  assert.throws(
    () => validatedExtraTags({ mentionTags: [["p", MENTION]] }),
    /only "mention" tags/,
  );
});

test("isRelayEventId — 64-hex only; placeholders and synthetic rows are client-local", () => {
  assert.equal(isRelayEventId(ROOT), true);
  assert.equal(isRelayEventId("A".repeat(64)), true);
  assert.equal(
    isRelayEventId("optimistic-2f482e67-3a69-47d8-8330-836056227f04"),
    false,
  );
  assert.equal(isRelayEventId(`summary:${ROOT}`), false);
  assert.equal(isRelayEventId("1".repeat(63)), false);
  assert.equal(isRelayEventId(""), false);
});

test("sendChannelMessageViaRelay — refuses to sign a reply to an unconfirmed placeholder, before any IO", async () => {
  // The hazard: a thread opened on an optimistic row hands its
  // `optimistic-<uuid>` id back as parentEventId. Signing it would publish a
  // reply whose e tag can never resolve — durable damage every client
  // renders as a reply to nothing. The refusal must fire before identity or
  // relay use, so a stub that throws on contact proves the ordering.
  const untouchable = {
    sign: () => {
      throw new Error("sign must not be reached");
    },
    pubkey: () => {
      throw new Error("pubkey must not be reached");
    },
  };
  await assert.rejects(
    sendChannelMessageViaRelay(untouchable, {
      channelId: CHANNEL,
      content: "replying too fast",
      parentEventId: "optimistic-2f482e67-3a69-47d8-8330-836056227f04",
    }),
    /still sending/,
  );
});

/* --- get_thread_replies ---------------------------------------------------- */

const root = event(ROOT, 5, [["h", CHANNEL]]);
const replyA = event("a".repeat(63) + "1", 10, [
  ["p", AUTHOR],
  ["h", CHANNEL],
  ["e", ROOT, "", "reply"],
]);
const replyB = event("a".repeat(63) + "2", 10, [
  ["p", AUTHOR],
  ["h", CHANNEL],
  ["e", ROOT, "", "reply"],
]);
const nestedC = event("c".repeat(64), 12, [
  ["p", AUTHOR],
  ["h", CHANNEL],
  ["e", ROOT, "", "root"],
  ["e", replyA.id, "", "reply"],
]);
// e-tags the root with no reply marker — a quote/reference, not a reply.
const quote = event("d".repeat(64), 11, [
  ["h", CHANNEL],
  ["e", ROOT],
]);
// Reply-marked into a DIFFERENT thread while also e-tagging this root.
const foreign = event("e".repeat(64), 11, [
  ["h", CHANNEL],
  ["e", ROOT],
  ["e", "f".repeat(64), "", "reply"],
]);

test("threadRepliesPage — descendants only, oldest first, root and non-replies excluded", () => {
  const page = threadRepliesPage(
    [nestedC, foreign, quote, replyB, root, replyA],
    ROOT,
    { limit: 200, cursor: null, depthLimit: null },
  );
  assert.deepEqual(page, {
    events: [replyA, replyB, nestedC],
    next_cursor: null,
  });
});

test("threadRepliesPage — keyset paging never skips a same-second tie across the boundary", () => {
  const first = threadRepliesPage([replyA, replyB, nestedC], ROOT, {
    limit: 2,
    cursor: null,
    depthLimit: null,
  });
  // replyA and replyB share created_at 10; the id tiebreak orders them, and
  // the cursor names the exact (created_at, event_id) boundary — the caller's
  // documented wire shape, snake_case fields included.
  assert.deepEqual(first.events, [replyA, replyB]);
  assert.deepEqual(first.next_cursor, {
    created_at: 10,
    event_id: replyB.id,
  });

  const second = threadRepliesPage([replyA, replyB, nestedC], ROOT, {
    limit: 2,
    cursor: first.next_cursor,
    depthLimit: null,
  });
  assert.deepEqual(second, { events: [nestedC], next_cursor: null });
});

test("threadRepliesPage — an exact page-multiple thread ends with a null cursor, not an empty round trip", () => {
  const page = threadRepliesPage([replyA, replyB], ROOT, {
    limit: 2,
    cursor: null,
    depthLimit: null,
  });
  assert.equal(page.events.length, 2);
  assert.equal(page.next_cursor, null);
});

test("threadRepliesPage — depthLimit prunes below the asked depth", () => {
  const page = threadRepliesPage([replyA, replyB, nestedC], ROOT, {
    limit: 200,
    cursor: null,
    depthLimit: 1,
  });
  assert.deepEqual(page.events, [replyA, replyB]);
});

test("threadDepthOf — the relay's depth vocabulary: root 0, reply 1, nested 2, broken link stubbed", () => {
  // The root itself is NOT in the map: a `#e` subtree read can never return
  // it (it does not e-tag itself), and depths must still resolve.
  const byId = new Map([replyA, replyB, nestedC].map((e) => [e.id, e]));
  assert.equal(threadDepthOf(ROOT, byId, ROOT), 0);
  assert.equal(threadDepthOf(replyA.id, byId, ROOT), 1);
  assert.equal(threadDepthOf(nestedC.id, byId, ROOT), 2);
  // A reply whose parent the read could not see (and which is not the root):
  // one hop, plus the unseen ancestor — the relay's missing-metadata stub.
  const orphan = event("9".repeat(64), 20, [
    ["e", ROOT, "", "root"],
    ["e", "8".repeat(64), "", "reply"],
  ]);
  assert.equal(
    threadDepthOf(orphan.id, new Map([[orphan.id, orphan]]), ROOT),
    2,
  );
});

/* --- F418 / F419: extra tags and subtree paging ---------------------------- */

const SHA = "c".repeat(64);

const snapshot = [
  "link-preview",
  "snapshot",
  "1",
  "https://example.com/post",
  "A post",
  "Example",
  "About it",
  `https://work.example/media/${SHA}.png`,
  SHA,
  "",
  "",
];
const marker = ["buzz:sent-from-thread", ROOT, "The root, briefly"];

test("link-preview tags and the sent-from-thread marker are signed, in the optimistic row's order", () => {
  const imeta = ["imeta", `url https://work.example/media/${SHA}.png`];
  const emoji = ["emoji", "party", "https://work.example/e.png"];
  const mention = ["mention", "b".repeat(64)];
  assert.deepEqual(
    sendChannelMessageExtraTags({
      mediaTags: [imeta],
      emojiTags: [emoji],
      mentionTags: [mention],
      linkPreviewTags: [snapshot],
      sentFromThreadTag: marker,
    }),
    [imeta, emoji, mention, snapshot, marker],
  );
  assert.deepEqual(
    sendChannelMessageExtraTags({ linkPreviewTags: [["link-preview", "none"]] }),
    [["link-preview", "none"]],
  );
  assert.deepEqual(sendChannelMessageExtraTags({}), []);
});

test("the link-preview argument carries only link-preview tags of a valid shape", () => {
  assert.throws(() => validatedLinkPreviewTags([["p", "b".repeat(64)]]), /only "link-preview" tags/);
  assert.throws(() => validatedLinkPreviewTags([["link-preview", "none"], snapshot]), /invalid link-preview/);
  assert.throws(() => validatedLinkPreviewTags([["link-preview", "snapshot", "1"]]), /invalid link-preview/);
  assert.throws(() => validatedLinkPreviewTags([["link-preview", 7]]), /only "link-preview" tags/);
  assert.throws(() => validatedLinkPreviewTags("link-preview"), /must be a list/);
  assert.deepEqual(validatedLinkPreviewTags(null), []);
});

test("the sent-from-thread argument is one marker naming the root", () => {
  assert.deepEqual(validatedSentFromThreadTag(["buzz:sent-from-thread", ROOT]), [
    ["buzz:sent-from-thread", ROOT],
  ]);
  assert.throws(() => validatedSentFromThreadTag(["e", ROOT]), /sentFromThreadTag must be/);
  assert.throws(() => validatedSentFromThreadTag(["buzz:sent-from-thread", " "]), /sentFromThreadTag/);
  assert.throws(
    () => validatedSentFromThreadTag(["buzz:sent-from-thread", ROOT, "x", "extra"]),
    /sentFromThreadTag/,
  );
  assert.deepEqual(validatedSentFromThreadTag(undefined), []);
});

function reply(index, createdAt) {
  return {
    id: `${String(index).padStart(4, "0")}${"d".repeat(60)}`,
    pubkey: "a".repeat(64),
    created_at: createdAt,
    kind: 9,
    tags: [["e", ROOT, "", "root"]],
    content: `reply ${index}`,
    sig: "",
  };
}

function shavingRelay(events, perPage) {
  const requests = [];
  const fetchPage = async (filter) => {
    requests.push(filter.until);
    return events
      .filter((e) => filter.until === undefined || e.created_at <= filter.until)
      .sort((a, b) => b.created_at - a.created_at || (a.id < b.id ? -1 : 1))
      .slice(0, Math.min(perPage, filter.limit));
  };
  return { requests, fetchPage };
}

test("a shaved page made only of the boundary second does not end the thread", async () => {
  const events = [];
  for (const [second, at] of [[0, 100], [1, 90], [2, 80]]) {
    for (let i = 0; i < 3; i += 1) events.push(reply(second * 3 + i, at));
  }
  const relay = shavingRelay(events, 3);
  const subtree = await fetchThreadSubtree(ROOT, "design", relay.fetchPage);
  assert.equal(subtree.length, 9, "every reply is collected");
  assert.equal(relay.requests.at(-1), 79, "the walk ends on an empty page");
});

test("a full page of one already-seen second refuses instead of serving a truncated thread", async () => {
  const events = Array.from({ length: 501 }, (_, i) => reply(i, 100));
  events.push(reply(9999, 50));
  await assert.rejects(
    fetchThreadSubtree(ROOT, null, shavingRelay(events, 500).fetchPage),
    /more events in one second than a relay page holds/,
  );
});

