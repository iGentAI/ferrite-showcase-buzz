import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { JSDOM } from "jsdom";

import { useComposerAttachmentSpoilers } from "./useComposerAttachmentSpoilers.ts";

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

const COPY = "https://relay.example/media/copy.png";
const EDITED = "https://relay.example/media/edited.png";

async function renderSpoilers(pendingImetaRef, uploadEditedAttachment) {
  const { renderHook } = await import("@testing-library/react");
  return renderHook(() =>
    useComposerAttachmentSpoilers({
      pendingImetaRef,
      removeAttachment: () => {},
      revertAttachment: () => null,
      uploadEditedAttachment,
    }),
  );
}

test("the spoiler ref moves with the setter, before React commits", async () => {
  const { act } = await import("@testing-library/react");
  const hook = await renderSpoilers({ current: [] }, async () => null);
  let seen;
  act(() => {
    hook.result.current.setSpoileredAttachmentUrls(new Set([COPY]));
    seen = hook.result.current.spoileredAttachmentUrlsRef.current;
  });
  assert.deepEqual([...seen], [COPY]);
});

test("editing one of two identical spoilered attachments keeps the other spoilered", async () => {
  const { act } = await import("@testing-library/react");
  const pendingImetaRef = { current: [{ url: COPY }, { url: COPY }] };
  const hook = await renderSpoilers(pendingImetaRef, async () => {
    pendingImetaRef.current = [{ url: EDITED }, { url: COPY }];
    return { url: EDITED };
  });
  act(() => hook.result.current.handleToggleAttachmentSpoiler(COPY));
  await act(() => hook.result.current.handleAttachmentEditSave(COPY, new Uint8Array()));
  assert.deepEqual(
    [...hook.result.current.spoileredAttachmentUrls].sort(),
    [COPY, EDITED].sort(),
  );
});
