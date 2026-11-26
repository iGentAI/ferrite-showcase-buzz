import assert from "node:assert/strict";
import test from "node:test";

import { sendChannelMessageViaRelay } from "./messageSend.ts";

const A = "a".repeat(64);
const B = "b".repeat(64);

function identity(pubkey, signedAs) {
  return {
    pubkey: async () => pubkey,
    sign: async (event) => ({ ...event, id: "1".repeat(64), pubkey: signedAs, created_at: 1, sig: "0".repeat(128) }),
  };
}

test("finding_cb19d19e5fe1a1b4: a send refuses when the signer is not the expected identity", async () => {
  await assert.rejects(
    () => sendChannelMessageViaRelay(identity(A, A), { channelId: "c", content: "hi", expectedSignerPubkey: B }),
    /signing identity changed/,
  );
});

test("finding_cb19d19e5fe1a1b4: a send refuses an event signed by a different key than the author", async () => {
  await assert.rejects(
    () => sendChannelMessageViaRelay(identity(A, B), { channelId: "c", content: "hi" }),
    /signing identity changed/,
  );
});
