import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { JSDOM } from "jsdom";
import React from "react";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});
before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    window: dom.window,
    localStorage: dom.window.localStorage,
  });
});
after(() => dom.window.close());

test("back-to-back starts in one turn cannot overwrite an active onboarding", async () => {
  const { act, renderHook } = await import("@testing-library/react");
  const { CommunityOnboardingProvider, useCommunityOnboarding } = await import(
    "./communityOnboarding.tsx"
  );
  window.localStorage.clear();
  const { result } = renderHook(() => useCommunityOnboarding(), {
    wrapper: ({ children }) =>
      React.createElement(CommunityOnboardingProvider, null, children),
  });
  let first;
  let second;
  act(() => {
    const { start } = result.current;
    first = start({ source: "deep-link", relayUrl: "wss://a.example" });
    second = start({ source: "deep-link", relayUrl: "wss://b.example" });
  });
  assert.equal(first, true);
  assert.equal(second, false, "the second relay waits for the first");
  assert.equal(result.current.transaction?.relayUrl.includes("a.example"), true);
});
