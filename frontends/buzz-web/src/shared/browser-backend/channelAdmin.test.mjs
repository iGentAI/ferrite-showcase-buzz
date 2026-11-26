import assert from "node:assert/strict";
import test from "node:test";

import {
  archiveTags,
  deletionTags,
  membershipTags,
} from "./channelAdmin.ts";

test("membershipTags — h scopes the room, p names the target", () => {
  const pubkey = "a".repeat(64);
  assert.deepEqual(membershipTags({ channelId: "general", pubkey }), [
    ["h", "general"],
    ["p", pubkey],
  ]);
});

test("membershipTags — a role rides only when one is asked for", () => {
  const pubkey = "b".repeat(64);
  assert.deepEqual(membershipTags({ channelId: "general", pubkey, role: "admin" }), [
    ["h", "general"],
    ["p", pubkey],
    ["role", "admin"],
  ]);
  // A remove (9001) carries no role, and an empty one must not become a tag
  // the relay would parse as a role change.
  assert.deepEqual(
    membershipTags({ channelId: "general", pubkey, role: "" }).map((t) => t[0]),
    ["h", "p"],
  );
});

test("membershipTags — the target pubkey is normalized to lowercase", () => {
  const [, p] = membershipTags({
    channelId: "general",
    pubkey: "AB".repeat(32),
  });
  assert.equal(p[1], "ab".repeat(32));
});

test("archiveTags — carries the archived flag and NOTHING else", () => {
  // A 9002 is authorized per tag and an archived room refuses every edit but
  // an unarchive, so a stray `name` here would make the room unrecoverable.
  assert.deepEqual(archiveTags("general", true), [
    ["h", "general"],
    ["d", "general"],
    ["archived", "true"],
  ]);
  assert.deepEqual(archiveTags("general", false), [
    ["h", "general"],
    ["d", "general"],
    ["archived", "false"],
  ]);
});

test("deletionTags — e/k per NIP-09, plus the h the live subscription filters on", () => {
  const eventId = "c".repeat(64);
  assert.deepEqual(deletionTags({ channelId: "general", eventId, targetKind: 40002 }), [
    ["e", eventId],
    ["k", "40002"],
    ["h", "general"],
  ]);
});
