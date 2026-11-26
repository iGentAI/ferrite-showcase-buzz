import assert from "node:assert/strict";
import test from "node:test";

import { mergeChannelLists } from "./hooks.ts";

test("mergeChannelLists_keepsOptimisticWelcomeWhenRelayLags", () => {
  const welcome = { id: "w", name: "Welcome", optimistic: true };
  const general = { id: "g", name: "general", stale: true };
  const merged = mergeChannelLists([welcome, general], [
    { id: "g", name: "general" },
  ]);
  assert.deepEqual(
    merged.map((channel) => channel.id).sort(),
    ["g", "w"],
    "the lagging relay snapshot does not erase the Welcome channel",
  );
  assert.equal(
    merged.find((channel) => channel.id === "g").stale,
    undefined,
    "fetched rows replace cached ones",
  );
});
