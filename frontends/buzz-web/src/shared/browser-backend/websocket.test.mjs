import assert from "node:assert/strict";
import test from "node:test";

import { WS_TICKET_PATH, ticketedRelayUrl } from "./websocket.ts";

function ticketFetch(ticket, calls = []) {
  return async (path, init) => {
    calls.push({ path, init });
    return {
      ok: true,
      json: async () => ({ ok: true, ticket }),
    };
  };
}

test("a same-host relay socket carries a freshly minted ticket", async () => {
  const calls = [];
  const url = await ticketedRelayUrl(
    "wss://buzz.example/buzz/ws/",
    "buzz.example",
    ticketFetch("1700000060000.ab12", calls),
  );
  assert.equal(url, "wss://buzz.example/buzz/ws/?ticket=1700000060000.ab12");
  assert.equal(calls.length, 1);
  assert.equal(calls[0].path, WS_TICKET_PATH);
  assert.equal(calls[0].init.method, "POST");
  assert.equal(calls[0].init.credentials, "same-origin");
});

test("another host's relay is never sent this page's ticket request", async () => {
  const calls = [];
  const url = await ticketedRelayUrl(
    "wss://relay.elsewhere/",
    "buzz.example",
    ticketFetch("t", calls),
  );
  assert.equal(url, "wss://relay.elsewhere/");
  assert.equal(calls.length, 0);
});

test("a failed ticket fetch leaves the URL for the relay to refuse", async () => {
  const refused = async () => ({ ok: false, json: async () => ({}) });
  assert.equal(
    await ticketedRelayUrl("wss://buzz.example/buzz/ws/", "buzz.example", refused),
    "wss://buzz.example/buzz/ws/",
  );
  const offline = async () => {
    throw new Error("offline");
  };
  assert.equal(
    await ticketedRelayUrl("wss://buzz.example/buzz/ws/", "buzz.example", offline),
    "wss://buzz.example/buzz/ws/",
  );
});
