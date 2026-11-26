/**
 * Regression tests for F148: a relay-controlled community icon must never
 * put a network URL into an <img src> (cache, fetch path, or render sink).
 */
import assert from "node:assert/strict";
import test from "node:test";

import {
  loadCachedCommunityIcon,
  MAX_CACHED_COMMUNITY_ICON_LENGTH,
  safeCommunityIconUrl,
  saveCachedCommunityIcon,
} from "./communityIconCache.ts";

const DATA_ICON = "data:image/png;base64,iVBORw0KGgo=";
const BEACON = "https://attacker.example/pixel.png";

function installStorage(initial = {}) {
  const values = new Map(Object.entries(initial));
  globalThis.localStorage = {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, String(value)),
    removeItem: (key) => values.delete(key),
  };
  return values;
}

test("F148: only image data URLs are accepted as community icons", () => {
  for (const icon of [
    DATA_ICON,
    "data:image/webp;base64,UklGRg==",
    "data:image/svg+xml;utf8,<svg/>",
    "DATA:IMAGE/JPEG;base64,/9j/",
  ]) {
    assert.equal(safeCommunityIconUrl(icon), icon, icon);
  }
  for (const icon of [
    BEACON,
    "http://attacker.example/pixel.png",
    "//attacker.example/pixel.png",
    "javascript:alert(1)",
    "data:text/html;base64,PHNjcmlwdD4=",
    "data:image/png",
    ` ${DATA_ICON}`,
    "blob:https://attacker.example/uuid",
    `data:image/png;base64,${"A".repeat(MAX_CACHED_COMMUNITY_ICON_LENGTH)}`,
    null,
    undefined,
    42,
  ]) {
    assert.equal(safeCommunityIconUrl(icon), null, String(icon));
  }
});

test("F148: a network icon URL cannot poison or be served from the cache", () => {
  const values = installStorage({
    "buzz-community-icons": JSON.stringify({
      "wss://a.example": BEACON,
      "wss://b.example": DATA_ICON,
    }),
  });

  assert.equal(loadCachedCommunityIcon("wss://a.example"), null);
  assert.equal(loadCachedCommunityIcon("wss://b.example"), DATA_ICON);

  saveCachedCommunityIcon("wss://c.example", BEACON);
  const stored = JSON.parse(values.get("buzz-community-icons"));
  assert.deepEqual(stored, { "wss://b.example": DATA_ICON });
});

test("F148: a fetched network icon renders the placeholder and is not cached", async () => {
  const values = installStorage();
  const { fetchSafeCommunityIcon } = await import("./useCommunityIcons.ts");

  assert.equal(
    await fetchSafeCommunityIcon("wss://relay.example.com", async () => BEACON),
    null,
  );
  assert.deepEqual(JSON.parse(values.get("buzz-community-icons")), {});

  assert.equal(
    await fetchSafeCommunityIcon(
      "wss://relay.example.com",
      async () => DATA_ICON,
    ),
    DATA_ICON,
  );
  assert.equal(loadCachedCommunityIcon("wss://relay.example.com"), DATA_ICON);
});

test("F148: CommunityEmojiIcon never puts a network URL in <img src>", async () => {
  const React = await import("react");
  const { renderToStaticMarkup } = await import("react-dom/server");
  const { CommunityEmojiIcon } = await import("./ui/CommunitySwitcher.tsx");

  const beaconHtml = renderToStaticMarkup(
    React.createElement(CommunityEmojiIcon, { className: "icon", iconUrl: BEACON }),
  );
  assert.ok(!beaconHtml.includes("<img"), beaconHtml);
  assert.ok(!beaconHtml.includes("attacker.example"), beaconHtml);

  const dataHtml = renderToStaticMarkup(
    React.createElement(CommunityEmojiIcon, {
      className: "icon",
      iconUrl: DATA_ICON,
    }),
  );
  assert.ok(dataHtml.includes(`src="${DATA_ICON}"`), dataHtml);
});
