/**
 * Unit tests for selectChannelSuggestions — the #/Tab channel-autocomplete
 * suggestion list.
 *
 * Archived channels must stay resolvable in historical links elsewhere in the
 * app, but must not be offered as new `#channel` suggestions (they were
 * leaking through, badged STREAM/FORUM, for archived branch channels).
 */

import assert from "node:assert/strict";
import test from "node:test";

import { selectChannelSuggestions } from "./useChannelLinks.ts";

function channel(overrides = {}) {
  return {
    id: "chan-1",
    name: "general",
    channelType: "stream",
    archivedAt: null,
    ...overrides,
  };
}

test("excludes an archived stream channel from suggestions", () => {
  const channels = [
    channel({
      id: "s1",
      name: "shipped-feature",
      channelType: "stream",
      archivedAt: "2026-01-01T00:00:00Z",
    }),
  ];

  assert.deepEqual(selectChannelSuggestions(channels, "shipped"), []);
});

test("excludes an archived forum channel from suggestions", () => {
  const channels = [
    channel({
      id: "f1",
      name: "old-rfc",
      channelType: "forum",
      archivedAt: "2026-01-01T00:00:00Z",
    }),
  ];

  assert.deepEqual(selectChannelSuggestions(channels, "old"), []);
});

test("includes an active stream channel matching the query", () => {
  const channels = [
    channel({
      id: "s2",
      name: "engineering",
      channelType: "stream",
      archivedAt: null,
    }),
  ];

  assert.deepEqual(selectChannelSuggestions(channels, "eng"), [
    { id: "s2", name: "engineering", channelType: "stream" },
  ]);
});

test("includes an active forum channel matching the query", () => {
  const channels = [
    channel({
      id: "f2",
      name: "design-rfcs",
      channelType: "forum",
      archivedAt: null,
    }),
  ];

  assert.deepEqual(selectChannelSuggestions(channels, "design"), [
    { id: "f2", name: "design-rfcs", channelType: "forum" },
  ]);
});

test("excludes DM channels regardless of archive state", () => {
  const channels = [
    channel({ id: "d1", name: "alice", channelType: "dm", archivedAt: null }),
  ];

  assert.deepEqual(selectChannelSuggestions(channels, "alice"), []);
});

test("mixed list keeps only active, non-DM matches", () => {
  const channels = [
    channel({
      id: "s3",
      name: "random",
      channelType: "stream",
      archivedAt: null,
    }),
    channel({
      id: "s4",
      name: "random-old",
      channelType: "stream",
      archivedAt: "2026-01-01T00:00:00Z",
    }),
    channel({
      id: "f3",
      name: "random-forum",
      channelType: "forum",
      archivedAt: null,
    }),
    channel({
      id: "d2",
      name: "random-dm",
      channelType: "dm",
      archivedAt: null,
    }),
  ];

  assert.deepEqual(
    selectChannelSuggestions(channels, "random").map((s) => s.id),
    ["s3", "f3"],
  );
});

test("a pending query update is resolved from the live text on Enter", async () => {
  const { resolveChannelSelection } = await import("./useChannelLinks.ts");
  const channels = [
    channel({ id: "c-general", name: "general" }),
    channel({ id: "c-dev", name: "dev" }),
  ];
  const stale = selectChannelSuggestions(channels, "g");
  const base = {
    knownNamesLower: ["general", "dev"],
    channels,
    suggestions: stale,
    selectedIndex: 0,
    startIndex: 0,
  };
  const fresh = resolveChannelSelection({ ...base, pending: true, value: "#dev", cursor: 4 });
  assert.equal(fresh?.suggestion.name, "dev", "the typed text wins over the stale menu");
  assert.equal(fresh?.startIndex, 0);

  const gone = resolveChannelSelection({ ...base, pending: true, value: "hello", cursor: 5 });
  assert.equal(gone, null, "no # query at the cursor selects nothing");

  const settled = resolveChannelSelection({ ...base, pending: false, value: "#dev", cursor: 4 });
  assert.equal(settled?.suggestion.name, "general", "without a pending update the menu is current");
});
