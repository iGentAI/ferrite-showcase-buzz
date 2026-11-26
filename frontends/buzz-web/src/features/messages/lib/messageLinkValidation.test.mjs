// F169 / F430: `buzz://message?…` links come from remote message content, and
// every accepted link becomes a chip plus relay lookups. parseMessageLink
// accepts only the shape buildMessageLink produces.
import assert from "node:assert/strict";
import test from "node:test";

import {
  buildMessageLink,
  parseMessageLink,
  resolveMessageLinkRenderTarget,
} from "./messageLink.ts";

const UUID_CHANNEL = "f570339f-8f8a-4e08-a779-8d954aa44109";
const DM_CHANNEL = "9".repeat(64);
const STARTER_CHANNEL = "general";
const EVENT = "b04819ffc1f7c8ffb49c6d30b5899f470198264680d02e78894a658e30a9059f";
const THREAD = "1234567890abcdef".repeat(4);

function reasonOf(url) {
  const result = parseMessageLink(url);
  return result.ok ? null : result.reason;
}

test("accepts every channel id shape this client creates", () => {
  for (const channelId of [UUID_CHANNEL, DM_CHANNEL, STARTER_CHANNEL, "room_2-b"]) {
    const url = buildMessageLink({ channelId, messageId: EVENT });
    const parsed = parseMessageLink(url);
    assert.equal(parsed.ok, true, channelId);
    assert.equal(parsed.ok && parsed.value.channelId, channelId);
  }
});

test("refuses channel ids no channel could have (spoofed chip labels)", () => {
  for (const channel of [
    "Announce-me",
    "Announcements",
    "announce me",
    "-leading-dash",
    "a".repeat(97),
    "x/y",
    "<b>",
  ]) {
    const url = `buzz://message?channel=${encodeURIComponent(channel)}&id=${EVENT}`;
    assert.equal(reasonOf(url), "invalid-channel", channel);
  }
});

test("refuses message ids that are not 64-hex event ids", () => {
  for (const id of ["x", "root-event", "a".repeat(63), "g".repeat(64), `${EVENT}0`]) {
    const url = `buzz://message?channel=${UUID_CHANNEL}&id=${id}`;
    assert.equal(reasonOf(url), "invalid-id", id);
  }
});

test("refuses a malformed thread id but treats an empty one as absent", () => {
  assert.equal(
    reasonOf(`buzz://message?channel=${UUID_CHANNEL}&id=${EVENT}&thread=t1`),
    "invalid-thread",
  );
  const parsed = parseMessageLink(
    `buzz://message?channel=${UUID_CHANNEL}&id=${EVENT}&thread=`,
  );
  assert.equal(parsed.ok && parsed.value.threadRootId, null);
  const threaded = parseMessageLink(
    buildMessageLink({ channelId: UUID_CHANNEL, messageId: EVENT, threadRootId: THREAD }),
  );
  assert.equal(threaded.ok && threaded.value.threadRootId, THREAD);
});

test("refuses duplicate and unknown parameters", () => {
  assert.equal(
    reasonOf(
      `buzz://message?channel=${UUID_CHANNEL}&channel=general&id=${EVENT}`,
    ),
    "duplicate-param",
  );
  assert.equal(
    reasonOf(`buzz://message?channel=${UUID_CHANNEL}&id=${EVENT}&label=Pay`),
    "unexpected-param",
  );
});

test("lowercases event ids so one message has one cache key", () => {
  const parsed = parseMessageLink(
    `buzz://message?channel=${UUID_CHANNEL}&id=${EVENT.toUpperCase()}&thread=${THREAD.toUpperCase()}`,
  );
  assert.equal(parsed.ok && parsed.value.messageId, EVENT);
  assert.equal(parsed.ok && parsed.value.threadRootId, THREAD);
});

test("a malformed autolink renders no message chip", () => {
  const href = "buzz://message?channel=Announce-me&id=x";
  assert.deepEqual(resolveMessageLinkRenderTarget({ href, label: href }), {
    kind: "none",
  });
});
