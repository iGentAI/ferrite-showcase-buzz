// Review finding 6 (PRs #51-#59): the seam must work in an insecure context
// (plain http from a LAN host), where `crypto.subtle` and `crypto.randomUUID`
// are undefined but `crypto.getRandomValues` still exists.
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import test from "node:test";

import { mintChannelId } from "./channels.ts";
import { stableChannelsHash } from "./channelsPayload.ts";
import { isValidProjectChannelId } from "../../features/projects/projectModels.ts";
import { randomUuid, sha256Bytes, sha256Hex } from "../lib/insecureContextCrypto.ts";
import { makeNip98AuthHeader } from "../lib/nip98.ts";
import {
  requestFirstEventGated,
  requestHistoryGated,
} from "../api/relayGateBoundary.ts";

const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
// A plain-http origin's `crypto`: `getRandomValues` only. Bound to the real
// object, so the stand-in still works while it replaces `globalThis.crypto`.
const secureCrypto = globalThis.crypto;
const insecureCrypto = { getRandomValues: (array) => secureCrypto.getRandomValues(array) };

const rows = [
  { id: "general", name: "general", last_message_at: "2026-09-12T10:00:00Z" },
  { id: "random", name: "random", last_message_at: null },
];

test("the channel list hash works without crypto.subtle and ignores last_message_at", async () => {
  const hash = await stableChannelsHash(rows, undefined);
  assert.match(hash, /^[0-9a-f]{16}$/);
  const touched = rows.map((row) => ({ ...row, last_message_at: "2026-09-12T11:00:00Z" }));
  assert.equal(await stableChannelsHash(touched, undefined), hash);
  const renamed = [{ ...rows[0], name: "general-2" }, rows[1]];
  assert.notEqual(await stableChannelsHash(renamed, undefined), hash);
});

test("a room id is minted without crypto.randomUUID", () => {
  const ids = new Set();
  for (let index = 0; index < 50; index += 1) {
    const id = mintChannelId(insecureCrypto);
    assert.match(id, UUID_V4);
    assert.ok(isValidProjectChannelId(id), id);
    ids.add(id);
  }
  assert.equal(ids.size, 50);
});

test("SHA-256 and UUIDs are real without crypto.subtle and crypto.randomUUID", async () => {
  // The relay recomputes the NIP-98 payload hash and derives DM room ids from
  // SHA-256, so the fallback must be the real function, not a stand-in digest.
  assert.equal(
    await sha256Hex(new TextEncoder().encode("abc"), undefined),
    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
  );
  const bytes = globalThis.crypto.getRandomValues(new Uint8Array(4096));
  assert.deepEqual(
    await sha256Bytes(bytes, undefined),
    new Uint8Array(await globalThis.crypto.subtle.digest("SHA-256", bytes)),
  );
  const ids = new Set();
  for (let index = 0; index < 50; index += 1) {
    const id = randomUuid(insecureCrypto);
    assert.match(id, UUID_V4);
    ids.add(id);
  }
  assert.equal(ids.size, 50);
});

test("a NIP-98 header is signed on a plain-http origin", async () => {
  const secure = Object.getOwnPropertyDescriptor(globalThis, "crypto");
  const hadWindow = "window" in globalThis;
  const previousWindow = globalThis.window;
  const storage = new Map();
  globalThis.window = {
    localStorage: {
      getItem: (key) => storage.get(key) ?? null,
      setItem: (key, value) => storage.set(key, String(value)),
      removeItem: (key) => storage.delete(key),
    },
  };
  Object.defineProperty(globalThis, "crypto", {
    configurable: true,
    enumerable: true,
    value: insecureCrypto,
  });
  let header;
  try {
    header = await makeNip98AuthHeader("http://192.168.1.20:8080/buzz/api/presence", "POST", {
      body: "{}",
    });
  } finally {
    Object.defineProperty(globalThis, "crypto", secure);
    if (hadWindow) globalThis.window = previousWindow;
    else delete globalThis.window;
  }
  const event = JSON.parse(atob(header.replace(/^Nostr /, "")));
  const tags = Object.fromEntries(event.tags.map(([name, value]) => [name, value]));
  assert.match(tags.nonce, UUID_V4);
  assert.equal(
    tags.payload,
    "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
  );
});

test("the seam's relay HTTP helpers call no secure-context-only crypto directly", () => {
  // relayApiPost/relayApiGet/dmChannelId (commands.ts), signedGet
  // (identityArchive.ts), the media upload digest and the NIP-98 header all
  // threw on a plain-http origin; they go through insecureContextCrypto now.
  const direct = /\bcrypto\.subtle\.digest\(|\bcrypto\.randomUUID\(/;
  const here = new URL("./", import.meta.url);
  const apiDir = new URL("../api/", import.meta.url);
  const sources = [
    ...readdirSync(here)
      .filter((name) => name.endsWith(".ts"))
      .map((name) => new URL(name, here)),
    ...readdirSync(apiDir)
      .filter((name) => name.endsWith(".ts"))
      .map((name) => new URL(name, apiDir)),
    new URL("../lib/nip98.ts", import.meta.url),
    new URL("../lib/useResolvedLinkPreviews.ts", import.meta.url),
    new URL("../../features/gifs/relay.ts", import.meta.url),
    new URL("../../features/agents/ui/ModelPicker.tsx", import.meta.url),
  ];
  const offenders = sources
    .filter((url) => direct.test(readFileSync(url, "utf8")))
    .map((url) => url.pathname.split("/src/")[1]);
  assert.deepEqual(offenders, []);
});

test("requestHistoryGated and requestFirstEventGated mint subscription ids on plain-http origins", async () => {
  const secure = Object.getOwnPropertyDescriptor(globalThis, "crypto");
  const hadWindow = "window" in globalThis;
  const previousWindow = globalThis.window;
  globalThis.window = {
    setTimeout: globalThis.setTimeout.bind(globalThis),
    clearTimeout: globalThis.clearTimeout.bind(globalThis),
  };
  Object.defineProperty(globalThis, "crypto", {
    configurable: true,
    enumerable: true,
    value: insecureCrypto,
  });
  try {
    const subscriptions = new Map();
    const sent = [];
    const historyPromise = requestHistoryGated(
      subscriptions,
      async (payload) => {
        sent.push(payload);
        const subId = payload[1];
        const sub = subscriptions.get(subId);
        clearTimeout(sub.timeout);
        subscriptions.delete(subId);
        sub.resolve([]);
      },
      async () => {},
      { kinds: [1], limit: 1 },
      1_000,
    );
    assert.deepEqual(await historyPromise, []);
    assert.match(sent[0][1], /^history-[0-9a-f-]{36}$/);

    const firstPromise = requestFirstEventGated(
      subscriptions,
      async (payload) => {
        sent.push(payload);
        const subId = payload[1];
        const sub = subscriptions.get(subId);
        clearTimeout(sub.timeout);
        subscriptions.delete(subId);
        sub.resolve(null);
      },
      async () => {},
      { ids: ["a".repeat(64)] },
      1_000,
    );
    assert.equal(await firstPromise, null);
    assert.match(sent[1][1], /^first-[0-9a-f-]{36}$/);
  } finally {
    Object.defineProperty(globalThis, "crypto", secure);
    if (hadWindow) globalThis.window = previousWindow;
    else delete globalThis.window;
  }
});
