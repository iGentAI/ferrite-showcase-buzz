import assert from "node:assert/strict";
import test from "node:test";

import { resolveEmojiKeySelection } from "./useEmojiAutocomplete.ts";

test("a pending query that left the text gives Enter back to send", () => {
  assert.equal(
    resolveEmojiKeySelection({ pending: true, value: "hello ", cursor: 6, startIndex: 6 }),
    null,
  );
  assert.equal(
    resolveEmojiKeySelection({ pending: true, value: "hello :smile: ", cursor: 14, startIndex: 6 }),
    null,
  );
});

test("a pending query that moved supplies its fresh start", () => {
  assert.deepEqual(
    resolveEmojiKeySelection({ pending: true, value: "ab :smi", cursor: 7, startIndex: 6 }),
    { startIndex: 3 },
  );
});

test("a start past the caret is never inserted over", () => {
  assert.equal(
    resolveEmojiKeySelection({ pending: false, value: "hi", cursor: 2, startIndex: 6 }),
    null,
  );
  assert.deepEqual(
    resolveEmojiKeySelection({ pending: false, value: "hi :sm", cursor: 6, startIndex: 3 }),
    { startIndex: 3 },
  );
});
