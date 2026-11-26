import assert from "node:assert/strict";
import test from "node:test";

import {
  RelayEventRejectedError,
  shouldRetryPublishOverHttp,
} from "./relayClientSession.ts";

test("an explicit relay refusal is not retried over HTTP", () => {
  for (const reason of ["rate-limited: slow down", "restricted: no", "blocked: x", "invalid: bad"]) {
    assert.equal(shouldRetryPublishOverHttp(new RelayEventRejectedError(reason)), false, reason);
  }
});

test("transport failures and unbound-socket auth refusals still fall back to HTTP", () => {
  assert.equal(shouldRetryPublishOverHttp(new Error("socket closed")), true);
  assert.equal(
    shouldRetryPublishOverHttp(new RelayEventRejectedError("auth-required: sign in")),
    true,
  );
});
