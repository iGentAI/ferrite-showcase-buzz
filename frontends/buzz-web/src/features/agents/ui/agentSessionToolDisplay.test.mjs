import assert from "node:assert/strict";
import test from "node:test";

import { buildFileEditDiff } from "./agentSessionFileEditDiff.ts";
import { getBuzzToolInfo, normalizeToolName } from "./agentSessionToolCatalog.ts";
import { getSentMessageLink } from "./AgentSessionToolItem/messageLinks.ts";

test("a removed `-- ` line and an added `++ ` line inside a hunk are edits, not headers", () => {
  const diff = [
    "--- a/q.sql",
    "+++ b/q.sql",
    "@@ -1,2 +1,2 @@",
    "--- old comment",
    "+++ new thing",
    " select 1;",
  ].join("\n");
  const built = buildFileEditDiff(
    { result: diff, args: { path: "q.sql" } },
    { renderClass: "file-edit" },
  );
  assert.ok(built);
  const kinds = built.lines.map((line) => line.kind);
  assert.ok(kinds.includes("remove"));
  assert.ok(kinds.includes("add"));
});

test("a numbered foreign title does not collapse into a Buzz tool name", () => {
  assert.notEqual(normalizeToolName("1. search"), "search");
  assert.notEqual(normalizeToolName("01_delete_message"), "delete_message");
});

test("read-only identity and presence tools carry the read tone", () => {
  assert.equal(getBuzzToolInfo("get_users")?.tone, "read");
  assert.equal(getBuzzToolInfo("get_presence")?.tone, "read");
});

test("a sent-message link names the destination channel, not the turn's channel", () => {
  const link = getSentMessageLink({
    status: "completed",
    isError: false,
    descriptor: { renderClass: "message" },
    channelId: "channel-a",
    args: { channel_id: "channel-b", content: "hi" },
    result: JSON.stringify({ event_id: "e".repeat(64), accepted: true }),
  });
  assert.equal(link?.channelId, "channel-b");
});
