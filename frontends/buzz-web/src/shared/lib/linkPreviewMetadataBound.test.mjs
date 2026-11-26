// F431: the entity (and link-preview) metadata cache is keyed by hrefs from
// remote message content, so its settled entries are bounded.
import assert from "node:assert/strict";
import test from "node:test";

import { __linkPreviewMetadataTest } from "./useResolvedLinkPreviews.ts";

const OWNER = "a".repeat(64);

function href(n) {
  return `buzz://issue?id=${n.toString(16).padStart(64, "0")}&owner=${OWNER}&d=repo`;
}

test("settled metadata entries are capped, least recently used first", async () => {
  const fetched = [];
  const loader = __linkPreviewMetadataTest.createMetadataLoader({
    fetcher: async (target) => {
      fetched.push(target);
      return { title: target, description: null };
    },
    maxSettledEntries: 3,
  });
  for (let n = 0; n < 3; n += 1) await loader.load(href(n)).promise;
  // Touch the oldest so it becomes the most recent.
  await loader.load(href(0)).promise;
  for (let n = 3; n < 6; n += 1) await loader.load(href(n)).promise;

  assert.ok(loader.peek(href(5)));
  assert.equal(loader.peek(href(1)), undefined);
  assert.equal(loader.peek(href(2)), undefined);
  assert.equal(loader.peek(href(0)), undefined, "three newer entries pushed it out");
  const before = fetched.length;
  await loader.load(href(4)).promise;
  assert.equal(fetched.length, before, "a retained entry is still a cache hit");
});

test("the default bound holds under a flood of distinct hrefs", async () => {
  const loader = __linkPreviewMetadataTest.createMetadataLoader({
    concurrency: 16,
    fetcher: async (target) => ({ title: target, description: null }),
  });
  for (let n = 0; n < 700; n += 1) await loader.load(href(n)).promise;
  let retained = 0;
  for (let n = 0; n < 700; n += 1) if (loader.peek(href(n))) retained += 1;
  assert.equal(retained, 500);
});
