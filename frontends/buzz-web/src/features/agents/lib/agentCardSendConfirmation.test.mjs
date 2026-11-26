import assert from "node:assert/strict";
import test from "node:test";

import {
  agentCardSendConfirmation,
  formatCardRecipientAudience,
} from "./agentCardSendConfirmation.ts";

// F347: an unlocked card with memory carries it as plaintext; sending it must
// go through a confirmation that names the recipients.

test("an unlocked card with memory needs a confirmation naming the recipients", () => {
  assert.deepEqual(
    agentCardSendConfirmation({ locked: false, memoryLevel: "core" }, [
      "Ada",
      "Grace",
    ]),
    { memoryLabel: "core memory", recipientAudience: "Ada and Grace" },
  );
  assert.deepEqual(
    agentCardSendConfirmation({ locked: false, memoryLevel: "everything" }, [
      "Ada",
    ]),
    { memoryLabel: "all memories", recipientAudience: "Ada" },
  );
});

test("config-only and locked cards send without a memory confirmation", () => {
  assert.equal(
    agentCardSendConfirmation({ locked: false, memoryLevel: "none" }, ["Ada"]),
    null,
  );
  assert.equal(
    agentCardSendConfirmation({ locked: true, memoryLevel: "everything" }, [
      "Ada",
    ]),
    null,
  );
});

test("recipient audiences read naturally", () => {
  assert.equal(formatCardRecipientAudience([]), "The people you selected");
  assert.equal(
    formatCardRecipientAudience(["Ada", "Grace", "Linus"]),
    "Ada, Grace, and Linus",
  );
});
