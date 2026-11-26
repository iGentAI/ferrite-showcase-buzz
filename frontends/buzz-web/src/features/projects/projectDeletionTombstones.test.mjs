import assert from "node:assert/strict";
import test from "node:test";

const { buildDeletionThresholds } = await import("./projectModels.ts");

const OWNER = "a".repeat(64);
const OTHER = "b".repeat(64);

function tombstone(pubkey, coordinate, createdAt = 100) {
  return { id: "x", kind: 5, pubkey, created_at: createdAt, content: "", sig: "", tags: [["a", coordinate]] };
}

test("only the coordinate's author can tombstone it", () => {
  const coordinate = `30621:${OWNER}:widgets`;
  const forged = buildDeletionThresholds([tombstone(OTHER, coordinate)]);
  assert.equal(forged.size, 0, "a third party's kind:5 must not hide the project");
  const genuine = buildDeletionThresholds([tombstone(OWNER, coordinate, 120)]);
  assert.equal(genuine.get(coordinate), 120);
});

test("an event that is not a deletion carries no tombstone", () => {
  const coordinate = `30617:${OWNER}:repo`;
  const notDeletion = { ...tombstone(OWNER, coordinate), kind: 1 };
  assert.equal(buildDeletionThresholds([notDeletion]).size, 0);
});
