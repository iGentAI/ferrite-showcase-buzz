import assert from "node:assert/strict";
import test from "node:test";

import {
  inviteErrorMessage,
  isInviteExhaustedError,
  isInviteExpiredError,
  relayHttpFromWs,
} from "./inviteHelpers.ts";

test("relayHttpFromWs maps secure and local relay schemes", () => {
  assert.equal(
    relayHttpFromWs("wss://relay.example/path"),
    "https://relay.example/path",
  );
  assert.equal(relayHttpFromWs("ws://localhost:7000"), "http://localhost:7000");
});

test("relayHttpFromWs rejects unexpected schemes", () => {
  assert.throws(
    () => relayHttpFromWs("https://relay.example"),
    /Expected ws:\/\/ or wss:\/\//,
  );
  assert.throws(
    () => relayHttpFromWs("relay.example"),
    /Expected ws:\/\/ or wss:\/\//,
  );
});

test("relayHttpFromWs rejects query strings, fragments, credentials, backslashes, and /api subpaths", () => {
  for (const hostile of [
    "wss://attacker.example/buzz/api/mcp/token?x=",
    "wss://relay.example?redirect=1",
    "wss://relay.example/buzz/#frag",
    "wss://relay.example#",
    "wss://user:pass@relay.example",
    "wss://@relay.example",
    "wss://relay.example\\evil",
    "wss://relay.example/buzz/api/invites",
    "wss://relay.example/api/mcp",
    "wss://relay.example/../admin",
  ]) {
    assert.throws(
      () => relayHttpFromWs(hostile),
      /Invalid relay WebSocket URL|Expected ws:\/\/ or wss:\/\//,
      `expected relayHttpFromWs to reject ${hostile}`,
    );
  }
});

test("invite expiry sentinel is recognized without hiding other errors", () => {
  assert.equal(isInviteExpiredError(new Error("invite_expired")), true);
  assert.equal(isInviteExpiredError(new Error("invite_invalid")), false);
  assert.equal(
    inviteErrorMessage("network unavailable"),
    "network unavailable",
  );
});

test("invite exhaustion sentinel is recognized distinctly from expiry", () => {
  assert.equal(isInviteExhaustedError(new Error("invite_exhausted")), true);
  assert.equal(isInviteExhaustedError(new Error("invite_expired")), false);
  assert.equal(isInviteExhaustedError(new Error("invite_invalid")), false);
});
