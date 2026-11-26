import assert from "node:assert/strict";
import test from "node:test";

import { getHomeMessageCapabilities } from "./homeMessageCapabilities.ts";
import { KIND_HUDDLE_STARTED } from "@/shared/constants/kinds";

const CHANNEL_ID = "9a1657ac-f7aa-5db0-b632-d8bbeb6dfb50";
const ME = "a".repeat(64);
const OTHER = "b".repeat(64);

function makeInboxItem(repOverrides = {}) {
  const rep = {
    id: "rep-newer-reply",
    kind: 9,
    pubkey: ME,
    content: "Newer self reply",
    createdAt: 20,
    channelId: CHANNEL_ID,
    channelName: "general",
    tags: [["h", CHANNEL_ID]],
    category: "activity",
    ...repOverrides,
  };
  return {
    id: rep.id,
    conversationId: `channel:${CHANNEL_ID}:root-1`,
    item: rep,
    groupItems: [rep],
  };
}

test("header delete capability targets anchored message rather than mutable conversation representative", () => {
  const inboxItem = makeInboxItem({ id: "rep-by-me", pubkey: ME });
  const anchoredOtherUserMessage = {
    id: "anchored-other-msg",
    kind: 9,
    pubkey: OTHER,
    channelId: CHANNEL_ID,
  };

  // Anchored message belongs to OTHER, even though representative belongs to ME:
  // header delete must NOT be enabled or target rep-by-me.
  const disallowed = getHomeMessageCapabilities(
    inboxItem,
    ME,
    new Set([CHANNEL_ID]),
    { targetMessage: anchoredOtherUserMessage, channelArchivedAt: null },
  );
  assert.equal(disallowed.canDelete, false);
  assert.equal(disallowed.deleteTargetId, null);

  // Anchored message belongs to ME, even when representative advanced to OTHER's reply:
  const inboxWithOtherRep = makeInboxItem({ id: "rep-by-other", pubkey: OTHER });
  const anchoredOwnMessage = {
    id: "anchored-own-msg",
    kind: 9,
    pubkey: ME,
    channelId: CHANNEL_ID,
  };
  const allowed = getHomeMessageCapabilities(
    inboxWithOtherRep,
    ME,
    new Set([CHANNEL_ID]),
    { targetMessage: anchoredOwnMessage, channelArchivedAt: null },
  );
  assert.equal(allowed.canDelete, true);
  assert.equal(allowed.deleteTargetId, "anchored-own-msg");
});

test("header delete capability rejects archived channels and immutable huddle-started messages", () => {
  const inboxItem = makeInboxItem({ id: "own-msg", pubkey: ME });
  const archived = getHomeMessageCapabilities(
    inboxItem,
    ME,
    new Set([CHANNEL_ID]),
    {
      targetMessage: inboxItem.item,
      channelArchivedAt: 1700000000,
    },
  );
  assert.equal(archived.canDelete, false);

  const huddleItem = makeInboxItem({
    id: "huddle-msg",
    kind: KIND_HUDDLE_STARTED,
    pubkey: ME,
  });
  const huddleCaps = getHomeMessageCapabilities(
    huddleItem,
    ME,
    new Set([CHANNEL_ID]),
    {
      targetMessage: huddleItem.item,
      channelArchivedAt: null,
    },
  );
  assert.equal(huddleCaps.canDelete, false);
});
