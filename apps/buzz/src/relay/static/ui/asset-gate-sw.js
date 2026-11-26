// Keep authenticated asset checks bounded without turning the whole module
// graph into concurrent native session checks. The edge build emits one script,
// so a single fail-closed lane is both sufficient and immune to reply-waiter
// exhaustion in the managed-session bridge.
const ASSET_LANES = 1;
const assetQueues = Array.from({ length: ASSET_LANES }, () => Promise.resolve());
let nextAssetLane = 0;

// Every attempt is bounded. The lane is strictly serial, so one fetch that
// never settles (a stalled auth_request behind nginx, say) would otherwise hold
// every later module request in every controlled tab for as long as the worker
// lives. A timed-out attempt is aborted and retried like a 5xx, and when every
// attempt timed out the page gets a 504 it can report instead of a hang. A
// request the page itself cancelled (navigation, a closed tab) is aborted at
// once and never retried, so it does not hold the lane either.
const ASSET_ATTEMPTS = 3;
const ASSET_ATTEMPT_TIMEOUT_MS = 10_000;

const cancelled = (request) => Boolean(request.signal && request.signal.aborted);

const fetchWithTimeout = async (request) => {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), ASSET_ATTEMPT_TIMEOUT_MS);
  const forward = () => controller.abort();
  request.signal?.addEventListener("abort", forward, { once: true });
  if (cancelled(request)) controller.abort();
  try {
    return await fetch(request, { cache: "reload", signal: controller.signal });
  } catch (error) {
    if (controller.signal.aborted && !cancelled(request)) return null;
    throw error;
  } finally {
    clearTimeout(timer);
    request.signal?.removeEventListener("abort", forward);
  }
};

const fetchProtectedAsset = async (request) => {
  let response = null;
  for (let attempt = 0; attempt < ASSET_ATTEMPTS; attempt += 1) {
    response = await fetchWithTimeout(request);
    if (response && response.status < 500) return response;
    if (cancelled(request)) break;
    // No backoff after the last attempt: nothing follows it.
    if (attempt + 1 < ASSET_ATTEMPTS) {
      await new Promise((resolve) => setTimeout(resolve, 150 * (attempt + 1)));
    }
  }
  return (
    response ??
    new Response("asset request timed out", {
      status: 504,
      statusText: "Gateway Timeout",
      headers: { "content-type": "text/plain; charset=utf-8", "cache-control": "no-store" },
    })
  );
};

self.addEventListener("install", () => {
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(self.clients.claim());
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (
    event.request.method !== "GET" ||
    url.origin !== self.location.origin ||
    !url.pathname.startsWith("/buzz/ui/assets/")
  ) {
    return;
  }

  // A transient managed-auth failure must never poison Chrome's HTTP cache.
  // Revalidate each serialized module request so a previously cached 500 is
  // replaced by the authenticated response from this controlled page load.
  const lane = nextAssetLane;
  nextAssetLane = (nextAssetLane + 1) % ASSET_LANES;
  const response = assetQueues[lane].then(() =>
    fetchProtectedAsset(event.request),
  );
  assetQueues[lane] = response.then(
    () => undefined,
    () => undefined,
  );
  event.respondWith(response);
});
