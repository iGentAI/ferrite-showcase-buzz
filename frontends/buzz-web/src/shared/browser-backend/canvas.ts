/**
 * The channel canvas, browser edition — `get_canvas` and `set_canvas`.
 *
 * A canvas is buzz's shared document for a channel: one kind:40100 event whose
 * content is the whole markdown document, `h`-tagged to the room. The desktop
 * app answers both commands in Rust (`commands/canvas.rs`): the read is a relay
 * REQ `{kinds:[40100], "#h":[channel], limit:1}` taking the newest event, the
 * write signs a fresh 40100 via `events::build_set_canvas` — content, one `h`
 * tag, nothing else. The relay stores and delivers 40100 as an ordinary channel
 * event (its channel-document `canvas` field is never written by anything; see
 * the buzz app's relay `effects.rs` on why 40100 is deliberately not a 9002 metadata
 * field), so "current canvas" simply means "newest 40100 in the room" — every
 * save is a whole-document supersede, and history stays on the relay.
 *
 * Two deliberate divergences from the desktop command, both browser-shaped:
 *
 *   * no UUID check on the channel id. The desktop mints channel UUIDs locally
 *     and validates them before signing; this build's channel ids come from the
 *     relay, which authorizes the write against the room's membership either
 *     way and refuses with a better sentence than a guess made in the tab.
 *   * the 64 KiB content cap (`events::MAX_CONTENT_BYTES`) is enforced here in
 *     bytes, exactly as Rust measures it, so a canvas that saves on desktop
 *     saves here and one that does not, does not.
 */

import type { RelayEvent } from "@/shared/api/types";
import {
  publishSigned,
  relayEvents,
  type RelayIdentity,
} from "@/shared/browser-backend/relayConnection";

/** Canvas (shared document) for a channel — the relay`s `KIND_CANVAS`. */
const KIND_CANVAS = 40100;

/** The desktop's `events::MAX_CONTENT_BYTES`, measured the same way (bytes). */
export const MAX_CANVAS_CONTENT_BYTES = 64 * 1024;

/** The tags of a canvas save: the room, and nothing else (`build_set_canvas`). */
export function canvasTags(channelId: string): string[][] {
  return [["h", channelId]];
}

/**
 * The desktop `get_canvas` reply, verbatim (see `RawCanvasResponse` in
 * `tauri.ts`): explicit nulls, because the TS caller distinguishes "no canvas
 * yet" from "canvas exists" via `updated_at`/`author`, and an absent key
 * deserializes as `undefined` — which `ensureWelcomeCanvas` deliberately
 * treats the same as null, but the desktop shape is the contract.
 */
export type RawCanvasResponse = {
  content: string;
  event_id: string | null;
  updated_at: number | null;
  author: string | null;
};

/**
 * Map a canvas read's events to the desktop reply. The newest 40100 IS the
 * canvas; the relay already returns newest-first for a limited REQ, but the
 * sort is restated here so the contract does not rest on delivery order.
 */
export function canvasResponseFromEvents(
  events: RelayEvent[],
): RawCanvasResponse {
  const newest = [...events].sort(
    (left, right) => right.created_at - left.created_at,
  )[0];
  if (!newest) {
    return { content: "", event_id: null, updated_at: null, author: null };
  }
  return {
    content: newest.content,
    event_id: newest.id,
    updated_at: newest.created_at,
    author: newest.pubkey,
  };
}

function requireChannelId(command: string, value: unknown): string {
  if (typeof value !== "string" || !value) {
    throw new Error(`${command} needs a channelId.`);
  }
  return value;
}

/** `get_canvas` — the newest kind:40100 in the room, or the explicit-nulls "no canvas yet". */
export async function getCanvasFromRelay(
  raw: unknown,
): Promise<RawCanvasResponse> {
  const input = (raw ?? {}) as { channelId?: unknown };
  const channelId = requireChannelId("get_canvas", input.channelId);
  const events = await relayEvents({
    kinds: [KIND_CANVAS],
    "#h": [channelId],
    limit: 1,
  });
  return canvasResponseFromEvents(events);
}

/** The desktop `set_canvas` reply, verbatim (`RawSetCanvasResult` in `tauri.ts`). */
export type RawSetCanvasResult = {
  ok: boolean;
  event_id: string;
};

/** `set_canvas` — sign and publish a whole-document canvas supersede. */
export async function setCanvasViaRelay(
  identity: RelayIdentity,
  raw: unknown,
): Promise<RawSetCanvasResult> {
  const input = (raw ?? {}) as { channelId?: unknown; content?: unknown };
  const channelId = requireChannelId("set_canvas", input.channelId);
  if (typeof input.content !== "string") {
    throw new Error("set_canvas needs a string content.");
  }
  const contentBytes = new TextEncoder().encode(input.content).length;
  if (contentBytes > MAX_CANVAS_CONTENT_BYTES) {
    throw new Error(
      `content exceeds maximum size of ${MAX_CANVAS_CONTENT_BYTES} bytes (got ${contentBytes})`,
    );
  }

  const signed = await identity.sign({
    kind: KIND_CANVAS,
    content: input.content,
    tags: canvasTags(channelId),
  });
  await publishSigned(signed, "Could not save the canvas.");
  return { ok: true, event_id: String(signed.id) };
}
