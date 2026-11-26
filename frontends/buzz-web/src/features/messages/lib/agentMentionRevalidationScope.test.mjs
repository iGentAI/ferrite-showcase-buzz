import assert from "node:assert/strict";
import test from "node:test";

const { revalidationScope } = await import("./agentMentionRevalidation.ts");

test("an invitee of a channel composer is judged in the community", () => {
  const channel = { type: "channel", channelId: "c1" };
  assert.deepStrictEqual(
    revalidationScope(channel, "c1", { inviteeAdmission: true }),
    { type: "community" },
  );
  // A member mention is still judged against the destination.
  assert.deepStrictEqual(revalidationScope(channel, "c1", {}), channel);
});

test("invitee admission never widens an owned or managed-only composer", () => {
  const owned = { type: "owned", channelId: "c1" };
  assert.deepStrictEqual(
    revalidationScope(owned, "c1", { inviteeAdmission: true }),
    owned,
  );
  const managed = { type: "managed-only" };
  assert.deepStrictEqual(
    revalidationScope(managed, "c1", { inviteeAdmission: true }),
    managed,
  );
});
