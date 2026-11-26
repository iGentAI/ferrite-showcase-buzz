import assert from "node:assert/strict";
import test from "node:test";

import { bareCommunityPolicyTransport } from "./InviteRedeemForm.tsx";

test("browser reconnect discovers relay policy over HTTP", () => {
  assert.equal(bareCommunityPolicyTransport(false), "webview");
});

test("desktop reconnect keeps the native relay policy command", () => {
  assert.equal(bareCommunityPolicyTransport(true), "native");
});
