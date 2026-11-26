import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_EAGER_CHANNEL_LOOKUPS,
  claimEagerChannelLookup,
  eagerChannelLookupsOutstanding,
} from "./channelLookupBudget.ts";

test("finding_9ae3958abad81004: eager channel lookups are capped and released once", () => {
  const releases = [];
  for (let i = 0; i < MAX_EAGER_CHANNEL_LOOKUPS; i += 1) {
    const release = claimEagerChannelLookup();
    assert.ok(release);
    releases.push(release);
  }
  assert.equal(claimEagerChannelLookup(), null);
  releases[0]();
  releases[0]();
  assert.equal(eagerChannelLookupsOutstanding(), MAX_EAGER_CHANNEL_LOOKUPS - 1);
  const again = claimEagerChannelLookup();
  assert.ok(again);
  again();
  for (const release of releases.slice(1)) release();
  assert.equal(eagerChannelLookupsOutstanding(), 0);
});
