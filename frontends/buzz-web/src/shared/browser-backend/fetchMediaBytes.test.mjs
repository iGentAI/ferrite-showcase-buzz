import assert from "node:assert/strict";
import test from "node:test";

import { fetchMediaBytes } from "./media.ts";

const relay = { httpUrl: "https://relay.example/buzz/api", pageUrl: "https://relay.example/buzz/ui/" };

test("fetch_media_bytes reads only this relay's media routes (finding_c3fa7525bbdb9578)", async () => {
  const originalFetch = globalThis.fetch;
  const seen = [];
  globalThis.fetch = async (url, init) => {
    seen.push({ url: String(url), init });
    return new Response(new Uint8Array([1, 2, 3]));
  };
  try {
    for (const hostile of [
      "https://evil.example/x.png",
      "https://relay.example/buzz/api/operator/config/x",
      "data:image/png;base64,AAAA",
      "https://user:pw@relay.example/buzz/blob/a",
    ]) {
      await assert.rejects(() => fetchMediaBytes({ url: hostile }, relay), hostile);
    }
    assert.equal(seen.length, 0, "nothing was fetched for a non-media URL");
    const bytes = await fetchMediaBytes({ url: "https://relay.example/buzz/blob/abc.png" }, relay);
    assert.equal(bytes.byteLength, 3);
    assert.equal(seen[0].init.redirect, "error");
  } finally {
    globalThis.fetch = originalFetch;
  }
});
