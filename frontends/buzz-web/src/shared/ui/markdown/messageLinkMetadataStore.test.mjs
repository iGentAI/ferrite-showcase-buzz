// F169 / F432: the shared cache and scheduler behind message-link pills.
import assert from "node:assert/strict";
import test from "node:test";

import {
  createMessageLinkMetadataStore,
  MESSAGE_LINK_METADATA_DEFAULTS,
} from "./messageLinkMetadataStore.ts";

const CHANNEL = "f570339f-8f8a-4e08-a779-8d954aa44109";

function id(n) {
  return n.toString(16).padStart(64, "0");
}

function event(messageId, overrides = {}) {
  return {
    content: `message ${messageId.slice(-4)}`,
    created_at: 1_700_000_000,
    kind: 9,
    pubkey: "a".repeat(64),
    tags: [["h", CHANNEL]],
    ...overrides,
  };
}

/** A store whose relay lookups stay pending until the test settles them. */
function harness(options = {}, { getEvent } = {}) {
  let clock = 0;
  let active = 0;
  let peak = 0;
  const calls = [];
  const pending = [];
  const store = createMessageLinkMetadataStore(
    {
      getEvent:
        getEvent ??
        ((messageId) => {
          calls.push(messageId);
          active += 1;
          peak = Math.max(peak, active);
          return new Promise((resolve, reject) => {
            pending.push({
              messageId,
              resolve: (value) => {
                active -= 1;
                resolve(value);
              },
              reject: (error) => {
                active -= 1;
                reject(error);
              },
            });
          });
        }),
      getProfile: async () => ({ displayName: "Ana" }),
      isDefinitiveNotFound: (error) =>
        String(error?.message ?? error).includes("event not found"),
      now: () => clock,
      shortPubkey: (pubkey) => pubkey.slice(0, 8),
      summarize: (content) => content,
      wait: async () => {},
    },
    options,
  );
  return {
    advance: (ms) => {
      clock += ms;
    },
    calls,
    peak: () => peak,
    pending,
    store,
  };
}

const flush = () => new Promise((resolve) => setImmediate(resolve));

test("at most two lookups run at once, however many links mount", async () => {
  const h = harness({ maxEager: 100 });
  const requests = [];
  for (let n = 0; n < 20; n += 1) {
    requests.push(h.store.load({ channelId: CHANNEL, messageId: id(n) }, { eager: true }));
  }
  await flush();
  assert.equal(h.calls.length, 2);
  while (h.pending.length > 0) {
    const next = h.pending.shift();
    next.resolve(event(next.messageId));
    await flush();
  }
  await Promise.all(requests);
  assert.equal(h.calls.length, 20);
  assert.equal(h.peak(), MESSAGE_LINK_METADATA_DEFAULTS.concurrency);
});

test("eager lookups are refused past the cap; a hover is still served", async () => {
  const h = harness();
  const admitted = [];
  for (let n = 0; n < 50; n += 1) {
    admitted.push(
      h.store.load({ channelId: CHANNEL, messageId: id(n) }, { eager: true }),
    );
  }
  assert.equal(
    admitted.filter(Boolean).length,
    MESSAGE_LINK_METADATA_DEFAULTS.maxEager,
  );
  const hovered = h.store.load(
    { channelId: CHANNEL, messageId: id(49) },
    { eager: false },
  );
  assert.ok(hovered, "a reader's hover is never refused");
  // Settle everything; the eager budget is released.
  while (h.pending.length > 0) {
    const next = h.pending.shift();
    next.resolve(event(next.messageId));
    await flush();
  }
  assert.ok(
    h.store.load({ channelId: CHANNEL, messageId: id(60) }, { eager: true }),
  );
  // 8 eager + 1 hover resolved; the 41 refused links cost nothing.
  assert.equal(h.calls.length, MESSAGE_LINK_METADATA_DEFAULTS.maxEager + 2);
});

test("the same link shares one lookup and a fresh answer is reused", async () => {
  const h = harness();
  const link = { channelId: CHANNEL, messageId: id(1) };
  const first = h.store.load(link, { eager: true });
  const second = h.store.load(link, { eager: true });
  assert.equal(first, second);
  await flush();
  h.pending.shift().resolve(event(id(1)));
  const value = await first;
  assert.equal(value.kind, "ready");
  assert.equal(value.author, "Ana");
  assert.equal(await h.store.load(link, { eager: true }), value);
  assert.equal(h.calls.length, 1);
  assert.deepEqual(h.store.peek(link), { fresh: true, value });
});

test("the cache is bounded (least recently used evicted)", async () => {
  const h = harness({ maxEntries: 5, maxEager: 100 }, {
    getEvent: async (messageId) => event(messageId),
  });
  for (let n = 0; n < 12; n += 1) {
    await h.store.load({ channelId: CHANNEL, messageId: id(n) }, { eager: true });
  }
  assert.equal(h.store.size(), 5);
  assert.equal(h.store.peek({ channelId: CHANNEL, messageId: id(0) }), undefined);
  assert.ok(h.store.peek({ channelId: CHANNEL, messageId: id(11) }));
});

test("an unavailable answer is not re-fetched on every remount", async () => {
  const h = harness();
  const link = { channelId: CHANNEL, messageId: id(2) };
  const first = h.store.load(link, { eager: true });
  await flush();
  h.pending.shift().reject(new Error("No event on this relay."));
  await flush();
  // Retry after the delay also fails.
  h.pending.shift().reject(new Error("No event on this relay."));
  assert.equal((await first).kind, "unavailable");
  assert.equal(h.calls.length, 2);
  // Remount within the negative window: no new relay query.
  assert.equal((await h.store.load(link, { eager: true })).kind, "unavailable");
  assert.equal(h.calls.length, 2);
  h.advance(MESSAGE_LINK_METADATA_DEFAULTS.unavailableTtlMs + 1);
  h.store.load(link, { eager: true });
  await flush();
  assert.equal(h.calls.length, 3);
});

test("a ready preview goes stale after the freshness window and is re-fetched", async () => {
  const h = harness();
  const link = { channelId: CHANNEL, messageId: id(3) };
  const first = h.store.load(link, { eager: true });
  await flush();
  h.pending.shift().resolve(event(id(3), { content: "original" }));
  assert.equal((await first).snippet, "original");
  h.advance(MESSAGE_LINK_METADATA_DEFAULTS.settledTtlMs + 1);
  const stale = h.store.peek(link);
  assert.equal(stale.fresh, false);
  assert.equal(stale.value.snippet, "original");
  const refreshed = h.store.load(link, { eager: true });
  await flush();
  h.pending.shift().resolve(event(id(3), { content: "edited" }));
  assert.equal((await refreshed).snippet, "edited");
});

for (const [kind, label] of [
  [5, "deletion"],
  [9005, "NIP-29 delete"],
  [40003, "edit"],
  [40099, "moderation tombstone"],
]) {
  test(`a live ${label} evicts cached previews of its target and notifies`, async () => {
    const h = harness({}, { getEvent: async (messageId) => event(messageId) });
    const target = { channelId: CHANNEL, messageId: id(4) };
    const other = { channelId: CHANNEL, messageId: id(5) };
    await h.store.load(target, { eager: true });
    await h.store.load(other, { eager: true });
    let notified = 0;
    h.store.subscribe(target, () => {
      notified += 1;
    });
    h.store.invalidateForEvent({ kind, tags: [["e", id(4).toUpperCase()]] });
    assert.equal(h.store.peek(target), undefined);
    assert.ok(h.store.peek(other));
    assert.equal(notified, 1);
  });
}

test("events of other kinds do not evict", async () => {
  const h = harness({}, { getEvent: async (messageId) => event(messageId) });
  const target = { channelId: CHANNEL, messageId: id(6) };
  await h.store.load(target, { eager: true });
  h.store.invalidateForEvent({ kind: 7, tags: [["e", id(6)]] });
  h.store.invalidateForEvent({ kind: 9, tags: [["e", id(6)]] });
  assert.ok(h.store.peek(target));
});

test("a lookup that settles after a deletion does not resurrect the preview", async () => {
  const h = harness();
  const link = { channelId: CHANNEL, messageId: id(7) };
  const request = h.store.load(link, { eager: true });
  await flush();
  h.store.invalidateForEvent({ kind: 5, tags: [["e", id(7)]] });
  h.pending.shift().resolve(event(id(7)));
  await request;
  await flush();
  assert.equal(h.store.peek(link), undefined);
});

test("reset drops everything and ignores lookups still in flight", async () => {
  const h = harness();
  const link = { channelId: CHANNEL, messageId: id(8) };
  const request = h.store.load(link, { eager: true });
  await flush();
  h.store.reset();
  h.pending.shift().resolve(event(id(8)));
  await request;
  await flush();
  assert.equal(h.store.size(), 0);
});

test("finding_92a9be9600f6d34d: a reset drops queued lookups and never retries the old community's ids", async () => {
  const requested = [];
  let failNext = true;
  let releaseWait;
  const store = createMessageLinkMetadataStore(
    {
      getEvent: async (id) => {
        requested.push(id);
        if (failNext) {
          failNext = false;
          throw new Error("Relay disconnected for community switch.");
        }
        return new Promise(() => {});
      },
      getProfile: async () => null,
      isDefinitiveNotFound: () => false,
      shortPubkey: (p) => p.slice(0, 8),
      summarize: (c) => c,
      wait: () =>
        new Promise((resolve) => {
          releaseWait = resolve;
        }),
    },
    { concurrency: 1, maxEager: 1 },
  );

  const first = store.load({ channelId: "c", messageId: "m1" }, { eager: true });
  const queued = store.load({ channelId: "c", messageId: "m2" }, { eager: false });
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(requested, ["m1"]);

  store.reset();
  assert.deepEqual(await queued, { kind: "unavailable" });
  releaseWait();
  assert.deepEqual(await first, { kind: "unavailable" });
  assert.deepEqual(requested, ["m1"], "no retry and no queued lookup after the reset");
  assert.notEqual(
    store.load({ channelId: "c", messageId: "m3" }, { eager: true }),
    null,
    "the eager budget starts fresh",
  );
});
