import assert from "node:assert/strict";
import test from "node:test";

import { ReadStateManagerSlot } from "./readStateManagerSlot.ts";

function fakeManager(pubkey) {
  return {
    ownerPubkey: pubkey,
    resolver: undefined,
    setContextParentResolver(resolver) {
      this.resolver = resolver;
    },
  };
}

test("a resolver registered before the manager exists reaches it", () => {
  const slot = new ReadStateManagerSlot();
  const resolver = () => "channel-1";
  slot.setResolver(resolver);
  const manager = fakeManager("a");
  slot.attach(manager);
  assert.equal(manager.resolver, resolver);
});

test("a replacement manager inherits the latest resolver", () => {
  const slot = new ReadStateManagerSlot();
  const first = fakeManager("a");
  slot.attach(first);
  const resolver = () => "channel-1";
  slot.setResolver(resolver);
  assert.equal(first.resolver, resolver);
  slot.detach(first);
  const second = fakeManager("a");
  slot.attach(second);
  assert.equal(second.resolver, resolver);
});

test("the previous account's manager is never returned for a new account", () => {
  const slot = new ReadStateManagerSlot();
  const managerA = fakeManager("a");
  slot.attach(managerA);
  assert.equal(slot.forPubkey("a"), managerA);
  assert.equal(slot.forPubkey("b"), null);
  assert.equal(slot.forPubkey(undefined), null);
});

test("detaching a superseded manager keeps the current one", () => {
  const slot = new ReadStateManagerSlot();
  const old = fakeManager("a");
  const current = fakeManager("a");
  slot.attach(old);
  slot.attach(current);
  slot.detach(old);
  assert.equal(slot.forPubkey("a"), current);
});
