// Regression tests for F145: a channel admin must not be offered the removal
// of a channel owner.

import assert from "node:assert/strict";
import test from "node:test";

import { canRemoveChannelMember } from "./channelMemberRemoval.ts";

const SELF = "self";
const member = (pubkey, role) => ({ pubkey, role });
const noBots = () => false;

function can(selfRole, target, isMyBot = noBots) {
  return canRemoveChannelMember({
    selfMember: selfRole ? { role: selfRole } : null,
    member: target,
    currentPubkey: SELF,
    isMyBot,
  });
}

test("an admin cannot remove an owner", () => {
  assert.equal(can("admin", member("owner-1", "owner")), false);
});

test("an admin can still remove admins and members", () => {
  assert.equal(can("admin", member("admin-2", "admin")), true);
  assert.equal(can("admin", member("member-1", "member")), true);
});

test("an owner can remove admins and members but not another owner", () => {
  assert.equal(can("owner", member("admin-1", "admin")), true);
  assert.equal(can("owner", member("member-1", "member")), true);
  assert.equal(can("owner", member("owner-2", "owner")), false);
});

test("a member cannot remove others, but anyone can remove themselves", () => {
  assert.equal(can("member", member("member-2", "member")), false);
  assert.equal(can("member", member(SELF, "member")), true);
  assert.equal(can("owner", member(SELF, "owner")), true);
});

test("a member can remove their own bot, a non-member cannot", () => {
  const bot = member("bot-1", "member");
  const isMine = (m) => m.pubkey === "bot-1";
  assert.equal(can("member", bot, isMine), true);
  assert.equal(can(null, bot, isMine), false);
});

test("an own bot that was made an owner can only be removed by an owner", () => {
  const ownerBot = member("bot-1", "owner");
  const isMine = (m) => m.pubkey === "bot-1";
  assert.equal(can("member", ownerBot, isMine), false);
  assert.equal(can("admin", ownerBot, isMine), false);
  assert.equal(can("owner", ownerBot, isMine), true);
});
