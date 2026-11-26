import assert from "node:assert/strict";
import test from "node:test";

import { consumeAutoSendToken, mintAutoSendToken } from "./autoSendTokens.ts";

test("a bare draft key from a URL never authorizes an auto-send", () => {
  assert.equal(consumeAutoSendToken("chan-1", "chan-1"), false);
  assert.equal(consumeAutoSendToken("thread:abc", "thread:abc"), false);
});

test("a minted token sends its own draft exactly once", () => {
  const token = mintAutoSendToken("chan-1");
  assert.notEqual(token, "chan-1");
  assert.equal(consumeAutoSendToken(token, "thread:other"), false, "another composer is not authorized");
  assert.equal(consumeAutoSendToken(token, "chan-1"), true);
  assert.equal(consumeAutoSendToken(token, "chan-1"), false, "spent after one use");
});
