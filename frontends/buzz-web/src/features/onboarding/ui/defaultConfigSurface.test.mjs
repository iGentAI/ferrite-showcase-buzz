import assert from "node:assert/strict";
import test from "node:test";

import { resolveAgentDefaultsSurfaceState } from "./defaultConfigSurface.ts";

test("an empty harness catalog renders a truthful empty state", () => {
  assert.equal(
    resolveAgentDefaultsSurfaceState({
      hasError: false,
      isLoading: false,
      readyRuntimeCount: 0,
    }),
    "empty",
  );
});

test("loading and errors take precedence over catalog contents", () => {
  assert.equal(
    resolveAgentDefaultsSurfaceState({
      hasError: true,
      isLoading: true,
      readyRuntimeCount: 0,
    }),
    "loading",
  );
  assert.equal(
    resolveAgentDefaultsSurfaceState({
      hasError: true,
      isLoading: false,
      readyRuntimeCount: 2,
    }),
    "error",
  );
  assert.equal(
    resolveAgentDefaultsSurfaceState({
      hasError: false,
      isLoading: false,
      readyRuntimeCount: 2,
    }),
    "ready",
  );
});
