import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { JSDOM } from "jsdom";

import { useAutoSubmitDraft } from "./messageComposerAutoSubmit.ts";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    window: dom.window,
  });
});

afterEach(async () => {
  const { cleanup } = await import("@testing-library/react");
  cleanup();
});

after(() => dom.window.close());

function harness() {
  const tokens = new Map();
  const scheduled = [];
  const log = { submitted: [], completed: 0 };
  return {
    tokens,
    scheduled,
    log,
    consumeToken: (token, key) => {
      if (!token || tokens.get(token) !== key) return false;
      tokens.delete(token);
      return true;
    },
    schedule: ({ submit }) => {
      const entry = { submit, cancelled: false };
      scheduled.push(entry);
      return () => {
        entry.cancelled = true;
      };
    },
  };
}

async function renderAutoSubmit(h, initialProps) {
  const { renderHook } = await import("@testing-library/react");
  return renderHook(
    ({ autoSubmitDraftKey, effectiveDraftKey }) =>
      useAutoSubmitDraft({
        autoSubmitDraftKey,
        effectiveDraftKey,
        onAutoSubmitComplete: () => h.log.completed++,
        submit: () => h.log.submitted.push(effectiveDraftKey),
        consumeToken: h.consumeToken,
        schedule: h.schedule,
      }),
    { initialProps },
  );
}

test("an auto-send for a composer that stayed mounted fires", async () => {
  const h = harness();
  const view = await renderAutoSubmit(h, { autoSubmitDraftKey: null, effectiveDraftKey: "a" });
  assert.equal(h.scheduled.length, 0);
  h.tokens.set("t1", "b");
  view.rerender({ autoSubmitDraftKey: "t1", effectiveDraftKey: "b" });
  assert.equal(h.scheduled.length, 1, "the trigger is read after mount too");
  assert.equal(h.log.completed, 1);
  // Clearing the trigger from the URL does not cancel the scheduled send.
  view.rerender({ autoSubmitDraftKey: null, effectiveDraftKey: "b" });
  assert.equal(h.scheduled[0].cancelled, false);
  h.scheduled[0].submit();
  assert.deepEqual(h.log.submitted, ["b"]);
});

test("a send scheduled for one draft is cancelled when the composer moves on", async () => {
  const h = harness();
  h.tokens.set("t1", "a");
  const view = await renderAutoSubmit(h, { autoSubmitDraftKey: "t1", effectiveDraftKey: "a" });
  assert.equal(h.scheduled.length, 1);
  view.rerender({ autoSubmitDraftKey: "t1", effectiveDraftKey: "b" });
  assert.equal(h.scheduled[0].cancelled, true);
  assert.equal(h.scheduled.length, 1, "the spent token does not fire again");
});
