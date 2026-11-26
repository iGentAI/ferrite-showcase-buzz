// Regression tests for F375: an inbox-list resize drag that ends without a
// pointerup (pointercancel, window blur) must not leave its window listener
// or body styles behind.

import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { JSDOM } from "jsdom";

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

after(() => dom.window.close());

afterEach(async () => {
  const { cleanup } = await import("@testing-library/react");
  cleanup();
  window.sessionStorage.clear();
  document.body.style.cursor = "";
  document.body.style.userSelect = "";
});

function pointer(type, clientX = 0) {
  return Object.assign(new window.Event(type), { clientX });
}

async function renderResizeHook() {
  const { renderHook } = await import("@testing-library/react");
  const { useResizableInboxListWidth } = await import(
    "./useResizableInboxListWidth.ts"
  );
  return renderHook(() => useResizableInboxListWidth());
}

function beginDrag(result, act, clientX = 100) {
  act(() =>
    result.current.handleInboxListResizeStart({
      clientX,
      preventDefault() {},
    }),
  );
}

for (const ending of ["pointercancel", "blur", "pointerup"]) {
  test(`a drag ended by ${ending} restores the body and stops resizing`, async () => {
    const { act } = await import("@testing-library/react");
    const { result } = await renderResizeHook();
    const initial = result.current.inboxListWidthPx;

    beginDrag(result, act);
    assert.equal(document.body.style.cursor, "col-resize");
    assert.equal(document.body.style.userSelect, "none");
    act(() => window.dispatchEvent(pointer("pointermove", 130)));
    assert.equal(result.current.inboxListWidthPx, initial + 30);

    act(() => window.dispatchEvent(pointer(ending)));
    assert.equal(document.body.style.cursor, "");
    assert.equal(document.body.style.userSelect, "");

    act(() => window.dispatchEvent(pointer("pointermove", 300)));
    assert.equal(
      result.current.inboxListWidthPx,
      initial + 30,
      "no resizing after the drag ended",
    );
  });
}

test("a new drag never stacks on one that lost its end event", async () => {
  const { act } = await import("@testing-library/react");
  const { result } = await renderResizeHook();
  const initial = result.current.inboxListWidthPx;

  beginDrag(result, act, 100);
  beginDrag(result, act, 500);
  act(() => window.dispatchEvent(pointer("pointermove", 520)));
  assert.equal(result.current.inboxListWidthPx, initial + 20);
  act(() => window.dispatchEvent(pointer("pointerup")));
  assert.equal(document.body.style.cursor, "");
});

test("unmounting mid-drag detaches the listeners and restores the body", async () => {
  const { act } = await import("@testing-library/react");
  const { result, unmount } = await renderResizeHook();
  beginDrag(result, act);
  unmount();
  assert.equal(document.body.style.cursor, "");
  assert.equal(document.body.style.userSelect, "");
});
