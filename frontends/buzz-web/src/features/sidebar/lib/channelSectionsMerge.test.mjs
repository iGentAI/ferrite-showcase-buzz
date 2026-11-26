// Regression tests for F396: a concurrent sections write from another device
// must not silently discard the edit this device just made.

import assert from "node:assert/strict";
import test, { mock } from "node:test";

import { relayClient } from "@/shared/api/relayClient";
import {
  mergeChannelSectionStores,
  mergeRecordsThreeWay,
  reconcileRemoteSections,
} from "./channelSectionsMerge.ts";
import { ChannelSectionSyncManager } from "./channelSectionsSync.ts";
import { makeSyncSigner } from "./sidebarSyncSignedEvents.mjs";
import {
  installFakeWindow,
  installTauriMock,
  makeFakeWindow,
} from "./sidebarSyncTestHelpers.mjs";

const RELAY = "wss://r.test";
const now = () => Math.floor(Date.now() / 1_000);

function store(sections, assignments = {}) {
  return { version: 1, sections, assignments };
}

test("mergeRecordsThreeWay keeps local changes and takes remote ones elsewhere", () => {
  const eq = (a, b) => a === b;
  const merged = mergeRecordsThreeWay(
    { a: 1, b: 2, c: 3 },
    { a: 10, b: 2, d: 4 }, // edited a, deleted c, added d
    { a: 1, b: 20, c: 3, e: 5 }, // edited b, added e
    eq,
  );
  assert.deepEqual(merged, { a: 10, b: 20, d: 4, e: 5 });
});

test("mergeRecordsThreeWay does not let a __proto__ key touch the prototype", () => {
  const remote = JSON.parse('{"__proto__": 1}');
  const merged = mergeRecordsThreeWay({}, {}, remote, (a, b) => a === b);
  assert.equal(Object.getPrototypeOf(merged), Object.prototype);
  assert.equal(Object.prototype.hasOwnProperty.call(merged, "__proto__"), true);
});

test("mergeChannelSectionStores keeps both devices' section edits", () => {
  const base = store([{ id: "work", name: "Work", order: 0 }], {
    c1: "work",
  });
  const local = store(
    [
      { id: "work", name: "Work stuff", order: 0 },
      { id: "new-local", name: "Reading", order: 1 },
    ],
    { c1: "work", c2: "new-local" },
  );
  const remote = store(
    [
      { id: "work", name: "Work", order: 0 },
      { id: "new-remote", name: "Ops", order: 1 },
    ],
    { c1: "work", c3: "new-remote" },
  );
  const merged = mergeChannelSectionStores(base, local, remote);
  assert.deepEqual(
    merged.sections.map((s) => [s.id, s.name]),
    [
      ["work", "Work stuff"],
      ["new-local", "Reading"],
      ["new-remote", "Ops"],
    ],
  );
  assert.deepEqual(merged.assignments, {
    c1: "work",
    c2: "new-local",
    c3: "new-remote",
  });
});

test("mergeChannelSectionStores keeps a local delete and drops its assignments", () => {
  const base = store(
    [
      { id: "a", name: "A", order: 0 },
      { id: "b", name: "B", order: 1 },
    ],
    { c1: "a" },
  );
  const local = store([{ id: "b", name: "B", order: 1 }], {});
  const remote = store(
    [
      { id: "a", name: "A", order: 0 },
      { id: "b", name: "B", order: 1 },
    ],
    { c1: "a", c9: "a" },
  );
  const merged = mergeChannelSectionStores(base, local, remote);
  assert.deepEqual(
    merged.sections.map((s) => s.id),
    ["b"],
  );
  assert.deepEqual(merged.assignments, {});
});

test("a publish racing a newer remote blob keeps the in-flight local edit", async () => {
  const signer = makeSyncSigner("channel-sections");
  const REMOTE = { id: "remote-section", name: "Remote", order: 1 };
  const BASE = { id: "base-section", name: "Base", order: 0 };
  const t = now();
  let fetchCount = 0;
  mock.method(relayClient, "fetchEvents", () => {
    fetchCount += 1;
    return Promise.resolve([
      signer.sign({
        content: fetchCount === 1 ? "base-cipher" : "remote-cipher",
        created_at: fetchCount === 1 ? t - 100 : t - 10,
      }),
    ]);
  });
  mock.method(relayClient, "publishEvent", () => Promise.resolve());
  const fw = makeFakeWindow();
  const restore = installFakeWindow(fw);
  const tauri = installTauriMock("{}");
  let captured = null;
  globalThis.window.__TAURI_INTERNALS__.invoke = (cmd, args) => {
    if (cmd === "nip44_decrypt_from_self") {
      return Promise.resolve(
        JSON.stringify(
          args.ciphertext === "base-cipher"
            ? store([BASE])
            : store([BASE, REMOTE]),
        ),
      );
    }
    if (cmd === "nip44_encrypt_to_self") {
      captured = args.plaintext;
      return Promise.resolve("ct");
    }
    if (cmd === "sign_event") {
      return Promise.resolve(
        JSON.stringify({
          id: "published",
          pubkey: signer.pubkey,
          content: "ct",
          created_at: args.createdAt,
          kind: args.kind,
          tags: args.tags,
          sig: "s",
        }),
      );
    }
    return Promise.reject(new Error(`unmocked: ${cmd}`));
  };
  try {
    const manager = new ChannelSectionSyncManager(signer.pubkey, RELAY);
    const first = await manager.fetchRemoteSections();
    assert.equal(first.status, "found");
    // The user creates a section on top of the blob they last saw ...
    const LOCAL = { id: "local-section", name: "Local", order: 1 };
    manager.publishSections(store([BASE, LOCAL]));
    // ... while another device publishes its own new section.
    fw._fireTimer();
    await new Promise((r) => setTimeout(r, 20));
    assert.ok(captured, "the merged blob must have been encrypted");
    const ids = JSON.parse(captured).sections.map((s) => s.id);
    assert.ok(ids.includes("local-section"), `local edit lost: ${captured}`);
    assert.ok(ids.includes("remote-section"), `remote edit lost: ${captured}`);
    assert.ok(ids.includes("base-section"));
  } finally {
    tauri.restore();
    restore();
    mock.reset();
  }
});

test("an edit made before the hook saw this device's merged publish keeps the other device's section", async () => {
  const signer = makeSyncSigner("channel-sections");
  const BASE = { id: "base-section", name: "Base", order: 0 };
  const REMOTE = { id: "remote-section", name: "Remote", order: 1 };
  const LOCAL1 = { id: "local-1", name: "Local 1", order: 2 };
  const LOCAL2 = { id: "local-2", name: "Local 2", order: 3 };
  const t = now();
  const payloads = {
    "base-cipher": JSON.stringify(store([BASE])),
    "remote-cipher": JSON.stringify(store([BASE, REMOTE])),
  };
  // The relay first holds the base, then another device's blob, then
  // whatever this manager published last.
  let served = signer.sign({ content: "base-cipher", created_at: t - 100 });
  mock.method(relayClient, "fetchEvents", () => Promise.resolve([served]));
  const published = [];
  mock.method(relayClient, "publishEvent", (event) => {
    const cipher = `published-${published.length}`;
    payloads[cipher] = published.at(-1);
    served = signer.sign({ content: cipher, created_at: event.created_at });
    return Promise.resolve();
  });
  const fw = makeFakeWindow();
  const restore = installFakeWindow(fw);
  const tauri = installTauriMock("{}");
  globalThis.window.__TAURI_INTERNALS__.invoke = (cmd, args) => {
    if (cmd === "nip44_decrypt_from_self") {
      return Promise.resolve(payloads[args.ciphertext]);
    }
    if (cmd === "nip44_encrypt_to_self") {
      published.push(args.plaintext);
      return Promise.resolve("ct");
    }
    if (cmd === "sign_event") {
      return Promise.resolve(
        JSON.stringify({
          id: `published-${published.length}`,
          pubkey: signer.pubkey,
          content: "ct",
          created_at: args.createdAt,
          kind: args.kind,
          tags: args.tags,
          sig: "s",
        }),
      );
    }
    return Promise.reject(new Error(`unmocked: ${cmd}`));
  };
  try {
    const manager = new ChannelSectionSyncManager(signer.pubkey, RELAY);
    assert.equal((await manager.fetchRemoteSections()).status, "found");

    // First edit; meanwhile another device adds a section.
    const base = store([BASE]);
    const local1 = store([BASE, LOCAL1]);
    served = signer.sign({ content: "remote-cipher", created_at: t - 10 });
    manager.publishSections(local1, base);
    fw._fireTimer();
    await new Promise((r) => setTimeout(r, 20));
    assert.equal(published.length, 1);
    assert.deepEqual(
      JSON.parse(published[0]).sections.map((s) => s.id).sort(),
      ["base-section", "local-1", "remote-section"],
    );

    // The hook has not applied the echo of that merge yet, so the next edit
    // is made on top of local1, which lacks the other device's section.
    manager.publishSections(store([BASE, LOCAL1, LOCAL2]), local1);
    fw._fireTimer();
    await new Promise((r) => setTimeout(r, 20));
    assert.equal(published.length, 2);
    assert.deepEqual(
      JSON.parse(published[1]).sections.map((s) => s.id).sort(),
      ["base-section", "local-1", "local-2", "remote-section"],
      "the other device's section must not be published away",
    );
  } finally {
    tauri.restore();
    restore();
    mock.reset();
  }
});

test("two devices adding a section with the same order merge to distinct orders", () => {
  const base = store([{ id: "a", name: "A", order: 0 }]);
  const local = store([...base.sections, { id: "b", name: "B", order: 1 }]);
  const remote = store([...base.sections, { id: "c", name: "C", order: 1 }]);
  const merged = mergeChannelSectionStores(base, local, remote);
  assert.deepEqual(
    merged.sections.map((section) => [section.id, section.order]),
    [
      ["a", 0],
      ["b", 1],
      ["c", 2],
    ],
  );
});

test("finding_412874c055f620ab: a remote blob during the debounce keeps the unsent local edit", () => {
  const section = (id, name, order) => ({ id, name, order });
  const base = store([section("a", "Alpha", 0)]);
  const local = store([section("a", "Alpha", 0), section("b", "Local", 1)]);
  const remote = store([section("a", "Renamed", 0)]);

  const result = reconcileRemoteSections(remote, local, base);
  const names = result.store.sections.map((s) => s.name).sort();
  assert.deepEqual(names, ["Local", "Renamed"]);
  assert.equal(result.republish, true);

  assert.deepEqual(reconcileRemoteSections(remote, null, null), { store: remote, republish: false });
});
