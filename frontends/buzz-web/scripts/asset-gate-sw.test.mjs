// Run: node --test scripts/asset-gate-sw.test.mjs
//
// Loads public/asset-gate-sw.js into a sandbox with a fake `self` and a fake
// network. Timers run 100x faster than written, so the 10 s attempt budget
// is 100 ms here.
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const SOURCE = fs.readFileSync(
  path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "public", "asset-gate-sw.js"),
  "utf8",
);
const ORIGIN = "https://buzz.example.com";
const ATTEMPT_BUDGET_MS = 10_000;

function loadWorker(fetch) {
  const listeners = {};
  const delays = [];
  const context = {
    self: {
      addEventListener: (type, listener) => (listeners[type] = listener),
      location: { origin: ORIGIN },
      skipWaiting() {},
      clients: { claim: async () => {} },
    },
    fetch,
    setTimeout: (callback, ms = 0) => {
      delays.push(ms);
      return setTimeout(callback, Math.ceil(ms / 100));
    },
    clearTimeout,
    AbortController,
    Response,
    URL,
    Promise,
  };
  vm.runInNewContext(SOURCE, context);
  const request = (pathname, signal) => {
    let answer = null;
    listeners.fetch({
      request: { method: "GET", url: `${ORIGIN}${pathname}`, signal },
      respondWith: (promise) => (answer = promise),
    });
    return answer;
  };
  return { request, backoffs: () => delays.filter((ms) => ms !== ATTEMPT_BUDGET_MS) };
}

const within = (promise, ms, what) => {
  let timer;
  const deadline = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${what} did not settle within ${ms} ms`)), ms);
  });
  return Promise.race([promise, deadline]).finally(() => clearTimeout(timer));
};

test("F334: a stalled asset cannot pin the lane; it ends in a 504 and the next asset loads", async () => {
  const worker = loadWorker((request, init) => {
    if (request.url.endsWith("/stalled.js")) {
      // Never answers; only an abort ends it.
      return new Promise((_, reject) =>
        init.signal?.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError"))),
      );
    }
    return Promise.resolve(new Response("ok", { status: 200 }));
  });
  const stalled = worker.request("/buzz/ui/assets/stalled.js");
  const next = worker.request("/buzz/ui/assets/next.js");
  assert.equal((await within(next, 5_000, "the request queued behind a stalled one")).status, 200);
  assert.equal((await stalled).status, 504);
});

test("F334: repeated 5xx answers are retried with backoff, but not after the last attempt", async () => {
  let calls = 0;
  const worker = loadWorker(() => {
    calls += 1;
    return Promise.resolve(new Response("busy", { status: 503 }));
  });
  const response = await within(worker.request("/buzz/ui/assets/app.js"), 5_000, "a 5xx asset");
  assert.equal(response.status, 503);
  assert.equal(calls, 3);
  assert.deepEqual(worker.backoffs(), [150, 300], "no sleep follows the third attempt");
});

test("F334: requests outside the asset path are not intercepted", () => {
  const worker = loadWorker(() => assert.fail("must not fetch"));
  assert.equal(worker.request("/buzz/api/events"), null);
});

test("review: a request the page cancels is aborted at once, not retried, and frees the lane", async () => {
  let calls = 0;
  const worker = loadWorker((request, init) => {
    if (request.url.endsWith("/abandoned.js")) {
      calls += 1;
      return new Promise((_, reject) =>
        init.signal?.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError"))),
      );
    }
    return Promise.resolve(new Response("ok", { status: 200 }));
  });
  const page = new AbortController();
  const abandoned = worker.request("/buzz/ui/assets/abandoned.js", page.signal);
  const next = worker.request("/buzz/ui/assets/next.js");
  await new Promise((resolve) => setTimeout(resolve, 5));
  page.abort();
  await assert.rejects(within(abandoned, 1_000, "the cancelled request"), /aborted/);
  assert.equal(calls, 1, "a cancelled request is not retried");
  // Well inside one 100 ms (scaled) attempt budget: the lane was freed by the cancel, not the timeout.
  assert.equal((await within(next, 60, "the request queued behind a cancelled one")).status, 200);
});
