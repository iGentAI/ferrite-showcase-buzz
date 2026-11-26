/**
 * Regression tests for F371: HostedCommunityOnboarding left a pending
 * Builderlab browser sign-in running after it unmounted. The login tracking
 * it now uses must cancel an in-flight attempt on unmount and discard the
 * late result.
 */
import assert from "node:assert/strict";
import test from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});
Object.assign(globalThis, {
  IS_REACT_ACT_ENVIRONMENT: true,
  HTMLElement: dom.window.HTMLElement,
  document: dom.window.document,
  window: dom.window,
});

const React = await import("react");
const { createRoot } = await import("react-dom/client");
const { useBuilderlabLoginAttempts } = await import(
  "./useBuilderlabLoginAttempts.ts"
);

async function mount(cancel) {
  const handle = { api: null };
  function Probe() {
    handle.api = useBuilderlabLoginAttempts(cancel);
    return null;
  }
  const root = createRoot(document.createElement("div"));
  await React.act(async () => {
    root.render(React.createElement(Probe));
  });
  handle.unmount = () =>
    React.act(async () => {
      root.unmount();
    });
  return handle;
}

test("F371: unmounting during a pending sign-in cancels it and discards the result", async () => {
  let cancels = 0;
  const handle = await mount(async () => {
    cancels += 1;
  });
  const attempt = handle.api.begin();
  assert.equal(attempt.isCurrent(), true);

  await handle.unmount();

  assert.equal(cancels, 1);
  assert.equal(attempt.isCurrent(), false);
});

test("F371: a finished sign-in is not cancelled on unmount", async () => {
  let cancels = 0;
  const handle = await mount(async () => {
    cancels += 1;
  });
  const attempt = handle.api.begin();
  attempt.finish();

  await handle.unmount();

  assert.equal(cancels, 0);
});

test("F371: a newer attempt supersedes an older one, which cannot clear it", async () => {
  let cancels = 0;
  const handle = await mount(async () => {
    cancels += 1;
  });
  const first = handle.api.begin();
  const second = handle.api.begin();
  assert.equal(first.isCurrent(), false);
  assert.equal(second.isCurrent(), true);
  first.finish();

  await handle.unmount();

  assert.equal(cancels, 1);
});

test("F371: an abandoned attempt is invalidated and not cancelled twice", async () => {
  let cancels = 0;
  const handle = await mount(async () => {
    cancels += 1;
  });
  const attempt = handle.api.begin();
  handle.api.abandon();
  assert.equal(attempt.isCurrent(), false);

  await handle.unmount();

  assert.equal(cancels, 0);
});

test("F371: a rejected cancellation is swallowed", async () => {
  const handle = await mount(async () => {
    throw new Error("listener already gone");
  });
  handle.api.begin();
  await handle.unmount();
  // Give the rejected promise a turn; an unhandled rejection fails the run.
  await new Promise((resolve) => setTimeout(resolve, 0));
});
