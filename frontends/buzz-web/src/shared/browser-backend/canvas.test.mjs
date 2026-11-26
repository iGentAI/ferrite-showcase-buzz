import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_CANVAS_CONTENT_BYTES,
  canvasResponseFromEvents,
  canvasTags,
  setCanvasViaRelay,
} from "./canvas.ts";

test("canvasTags — one h tag naming the room, nothing else", () => {
  // The desktop's build_set_canvas signs exactly this; a stray tag here would
  // be a wire divergence every relay-side reader inherits.
  assert.deepEqual(canvasTags("room-1"), [["h", "room-1"]]);
});

test("canvasResponseFromEvents — no canvas yet is the explicit-nulls shape", () => {
  // Explicit nulls, not absent keys: ensureWelcomeCanvas distinguishes "no
  // canvas yet" via updated_at/author, and the desktop command spells the
  // keys out for exactly that reason.
  assert.deepEqual(canvasResponseFromEvents([]), {
    content: "",
    event_id: null,
    updated_at: null,
    author: null,
  });
});

test("canvasResponseFromEvents — maps the event onto the desktop reply, verbatim keys", () => {
  const event = {
    id: "e".repeat(64),
    pubkey: "a".repeat(64),
    created_at: 1_700_000_000,
    kind: 40100,
    tags: [["h", "room-1"]],
    content: "# Welcome",
  };
  assert.deepEqual(canvasResponseFromEvents([event]), {
    content: "# Welcome",
    event_id: event.id,
    updated_at: 1_700_000_000,
    author: event.pubkey,
  });
});

test("canvasResponseFromEvents — the NEWEST event is the canvas, whatever the delivery order", () => {
  const older = {
    id: "1".repeat(64),
    pubkey: "a".repeat(64),
    created_at: 100,
    kind: 40100,
    tags: [["h", "room-1"]],
    content: "old",
  };
  const newer = { ...older, id: "2".repeat(64), created_at: 200, content: "new" };
  assert.equal(canvasResponseFromEvents([older, newer]).content, "new");
  assert.equal(canvasResponseFromEvents([newer, older]).content, "new");
});

test("setCanvasViaRelay — refuses a missing channelId and a non-string content before signing", async () => {
  const identity = {
    sign: () => {
      throw new Error("must not sign");
    },
    pubkey: async () => "a".repeat(64),
  };
  await assert.rejects(
    setCanvasViaRelay(identity, { content: "x" }),
    /needs a channelId/,
  );
  await assert.rejects(
    setCanvasViaRelay(identity, { channelId: "room-1" }),
    /needs a string content/,
  );
});

test("setCanvasViaRelay — enforces the desktop's 64 KiB cap in BYTES, not characters", async () => {
  const identity = {
    sign: () => {
      throw new Error("must not sign");
    },
    pubkey: async () => "a".repeat(64),
  };
  // 22,000 three-byte characters: 22k chars but 66k bytes — over the cap only
  // when measured the way Rust measures it.
  const threeBytesEach = "€".repeat(22_000);
  assert.ok(threeBytesEach.length < MAX_CANVAS_CONTENT_BYTES);
  await assert.rejects(
    setCanvasViaRelay(identity, { channelId: "room-1", content: threeBytesEach }),
    /exceeds maximum size/,
  );
});
