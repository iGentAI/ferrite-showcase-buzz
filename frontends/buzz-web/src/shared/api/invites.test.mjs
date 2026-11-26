import assert from "node:assert/strict";
import test from "node:test";

import {
  acceptJoinPolicy,
  claimInvite,
  getJoinPolicy,
  inviteTargetUrl,
  joinPolicyDocumentUrl,
  mintInvite,
} from "./invites.ts";

function withFetch(response, run) {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (url) => {
    assert.equal(url, "https://relay.example/api/join-policy");
    return response;
  };
  return Promise.resolve(run()).finally(() => {
    globalThis.fetch = originalFetch;
  });
}

test("getJoinPolicy maps relay-hosted Markdown and age requirements", async () => {
  await withFetch(
    new Response(
      JSON.stringify({
        policy: {
          terms_markdown: "# Terms",
          privacy_markdown: "# Privacy",
          age_attestation_required: true,
          version: "policy-v1",
        },
      }),
      { status: 200 },
    ),
    async () => {
      assert.deepEqual(await getJoinPolicy("wss://relay.example", "webview"), {
        termsMarkdown: "# Terms",
        privacyMarkdown: "# Privacy",
        ageAttestationRequired: true,
        version: "policy-v1",
      });
    },
  );
});

test("getJoinPolicy preserves opt-in behavior for unconfigured and older relays", async () => {
  await withFetch(new Response(JSON.stringify({}), { status: 200 }), async () =>
    assert.equal(await getJoinPolicy("wss://relay.example", "webview"), null),
  );
  await withFetch(new Response(null, { status: 404 }), async () =>
    assert.equal(await getJoinPolicy("wss://relay.example", "webview"), null),
  );
});

test("getJoinPolicy fails closed on a policy endpoint error", async () => {
  await withFetch(new Response(null, { status: 503 }), async () =>
    assert.rejects(getJoinPolicy("wss://relay.example", "webview"), /HTTP 503/),
  );
});

test("getJoinPolicy maps the native command response", async () => {
  const previousWindow = globalThis.window;
  globalThis.window = {
    __TAURI_INTERNALS__: {
      invoke(command, args) {
        assert.equal(command, "fetch_join_policy");
        assert.deepEqual(args, { relayUrl: "wss://relay.example" });
        return Promise.resolve({
          terms_markdown: "# Terms",
          privacy_markdown: "# Privacy",
          age_attestation_required: true,
          version: "policy-v1",
        });
      },
    },
  };

  try {
    assert.deepEqual(await getJoinPolicy("wss://relay.example", "native"), {
      termsMarkdown: "# Terms",
      privacyMarkdown: "# Privacy",
      ageAttestationRequired: true,
      version: "policy-v1",
    });
  } finally {
    globalThis.window = previousWindow;
  }
});

// --- mintInvite serialization ---

// The test-loader transpiles TS imports. tauri.ts imports `invoke` from
// @tauri-apps/api/core, which calls `window.__TAURI_INTERNALS__.invoke`.
// We stub that here so getRelayHttpUrl() and signRelayEvent() work in node.

function setupTauriStubs(
  httpBase,
  authEvent = {
    id: "x",
    sig: "y",
    pubkey: "z",
    kind: 27235,
    created_at: 1,
    tags: [],
  },
) {
  const calls = { invokeArgs: [] };
  globalThis.window = globalThis.window ?? {};
  globalThis.window.__TAURI_INTERNALS__ = {
    invoke: async (command, args) => {
      calls.invokeArgs.push({ command, args });
      if (command === "get_relay_http_url") return httpBase;
      if (command === "sign_event") return JSON.stringify(authEvent);
      throw new Error(`Unexpected Tauri command: ${command}`);
    },
  };
  return calls;
}

function teardownTauriStubs() {
  delete globalThis.window.__TAURI_INTERNALS__;
}

test("mintInvite serializes bounded max_uses in the request body", async () => {
  setupTauriStubs("https://relay.example");
  try {
    const originalFetch = globalThis.fetch;
    let capturedBody;
    globalThis.fetch = async (_url, init) => {
      capturedBody = JSON.parse(init.body);
      return new Response(
        JSON.stringify({
          code: "v2.abc123",
          expires_at: 1785100000,
          url: "https://relay.example/invite/v2.abc123",
          max_uses: 10,
          uses_remaining: 10,
        }),
      );
    };
    try {
      const result = await mintInvite({ ttlSecs: 259200, maxUses: 10 });
      assert.equal(capturedBody.ttl_secs, 259200);
      assert.equal(capturedBody.max_uses, 10);
      assert.equal(result.code, "v2.abc123");
      assert.equal(result.maxUses, 10);
      assert.equal(result.usesRemaining, 10);
      assert.equal(result.expiresAt, 1785100000);
      assert.equal(result.url, "https://relay.example/invite/v2.abc123");
    } finally {
      globalThis.fetch = originalFetch;
    }
  } finally {
    teardownTauriStubs();
  }
});

test("mintInvite omits max_uses when null (unlimited)", async () => {
  setupTauriStubs("https://relay.example");
  try {
    const originalFetch = globalThis.fetch;
    let capturedBody;
    globalThis.fetch = async (_url, init) => {
      capturedBody = JSON.parse(init.body);
      return new Response(
        JSON.stringify({
          code: "v2.abc123",
          expires_at: 1785100000,
          url: "https://relay.example/invite/v2.abc123",
          max_uses: null,
          uses_remaining: null,
        }),
      );
    };
    try {
      const result = await mintInvite({ ttlSecs: 259200, maxUses: null });
      assert.equal(capturedBody.ttl_secs, 259200);
      assert.equal(Object.hasOwn(capturedBody, "max_uses"), false);
      assert.equal(result.maxUses, null);
      assert.equal(result.usesRemaining, null);
    } finally {
      globalThis.fetch = originalFetch;
    }
  } finally {
    teardownTauriStubs();
  }
});

test("mintInvite omits max_uses when not provided (unlimited default)", async () => {
  setupTauriStubs("https://relay.example");
  try {
    const originalFetch = globalThis.fetch;
    let capturedBody;
    globalThis.fetch = async (_url, init) => {
      capturedBody = JSON.parse(init.body);
      return new Response(
        JSON.stringify({
          code: "v2.abc123",
          expires_at: 1785100000,
          url: "https://relay.example/invite/v2.abc123",
          max_uses: null,
          uses_remaining: null,
        }),
      );
    };
    try {
      await mintInvite({ ttlSecs: 86400 });
      assert.equal(capturedBody.ttl_secs, 86400);
      assert.equal(Object.hasOwn(capturedBody, "max_uses"), false);
    } finally {
      globalThis.fetch = originalFetch;
    }
  } finally {
    teardownTauriStubs();
  }
});

// --- F410: every invite-module request is bounded ---

/**
 * Replace `AbortSignal.timeout` with a controllable signal and `fetch` with
 * one that never answers until its signal aborts, the black-holed-relay case.
 */
async function withHangingRelay(run) {
  const originalTimeout = AbortSignal.timeout;
  const originalFetch = globalThis.fetch;
  const controllers = [];
  const requested = [];
  AbortSignal.timeout = (ms) => {
    const controller = new AbortController();
    controllers.push({ controller, ms });
    return controller.signal;
  };
  globalThis.fetch = (url, init = {}) => {
    requested.push({ url, signal: init.signal });
    return new Promise((_resolve, reject) => {
      init.signal?.addEventListener("abort", () =>
        reject(new DOMException("The operation timed out.", "TimeoutError")),
      );
    });
  };
  try {
    await run({ controllers, requested });
  } finally {
    AbortSignal.timeout = originalTimeout;
    globalThis.fetch = originalFetch;
  }
}

test("F410: the webview join-policy fetch is bounded and fails instead of hanging", async () => {
  await withHangingRelay(async ({ controllers, requested }) => {
    const pending = getJoinPolicy("wss://relay.example", "webview");
    await Promise.resolve();
    assert.equal(requested.length, 1);
    assert.ok(requested[0].signal, "fetch must carry an AbortSignal");
    assert.equal(controllers.length, 1);
    assert.equal(controllers[0].ms, 15_000);
    controllers[0].controller.abort();
    await assert.rejects(pending, /timed out/);
  });
});

test("F410: accepting the join policy is bounded and fails instead of hanging", async () => {
  await withHangingRelay(async ({ controllers, requested }) => {
    const pending = acceptJoinPolicy("wss://relay.example", "v2.abc", "policy-v1", true);
    await Promise.resolve();
    assert.equal(requested.length, 1);
    assert.equal(
      requested[0].url,
      "https://relay.example/api/invites/accept-policy",
    );
    assert.ok(requested[0].signal, "fetch must carry an AbortSignal");
    assert.equal(controllers[0].ms, 15_000);
    controllers[0].controller.abort();
    await assert.rejects(pending, /timed out/);
  });
});

test("invites endpoints reject smuggled query/fragment/api relayWsUrl and disable HTTP redirects", async () => {
  setupTauriStubs("https://relay.example/buzz/api");
  const originalFetch = globalThis.fetch;
  const seenRequests = [];
  globalThis.fetch = async (url, init = {}) => {
    seenRequests.push({ url, init });
    return new Response(
      JSON.stringify({
        ok: true,
        code: "v2.minted",
        expires_at: 1785100000,
      }),
      { status: 200 },
    );
  };
  try {
    for (const hostile of [
      "wss://attacker.example/buzz/api/mcp/token?x=",
      "wss://victim.example/buzz/api/invites#",
      "wss://user:pass@relay.example",
      "wss://@relay.example",
      "wss://relay.example/buzz/api/mcp/token",
    ]) {
      assert.throws(() => joinPolicyDocumentUrl(hostile, "terms"));
      await assert.rejects(() => getJoinPolicy(hostile, "webview"));
      await assert.rejects(() => getJoinPolicy(hostile, "native"));
      await assert.rejects(() => acceptJoinPolicy(hostile, "abc", "v1", true));
      await assert.rejects(() => claimInvite(hostile, "abc"));
    }
    assert.equal(
      seenRequests.length,
      0,
      "hostile relayWsUrl must fail before issuing any HTTP request",
    );

    for (const badPath of [
      "//evil.example/x",
      "https://evil.example/x",
      "http://evil.example/x",
      "/\\evil.example/x",
      "/api/../mcp/token",
      "/api/invites?x=1",
      "/api/invites#frag",
    ]) {
      assert.throws(
        () => inviteTargetUrl("https://relay.example/buzz", badPath),
        /Invalid invite endpoint path|Invite target escaped validated origin/,
        `expected inviteTargetUrl to reject ${badPath}`,
      );
    }

    const minted = await mintInvite({ ttlSecs: 3600 });
    assert.equal(seenRequests.length, 1);
    assert.equal(seenRequests[0].url, "https://relay.example/buzz/api/invites");
    assert.equal(seenRequests[0].init.redirect, "error");
    assert.ok(
      typeof minted.url === "string" && minted.url.length > 0,
      "mintInvite must synthesize an invite URL when the relay omits raw.url",
    );
    assert.equal(minted.maxUses, null);
    assert.equal(minted.usesRemaining, null);
  } finally {
    globalThis.fetch = originalFetch;
    teardownTauriStubs();
  }
});

test("claimInvite works in insecure HTTP contexts where crypto.subtle and crypto.randomUUID are undefined", async () => {
  setupTauriStubs("http://relay.local/buzz/api");
  const originalSubtle = globalThis.crypto.subtle;
  const originalRandomUUID = globalThis.crypto.randomUUID;
  const originalFetch = globalThis.fetch;
  Object.defineProperty(globalThis.crypto, "subtle", {
    configurable: true,
    value: undefined,
  });
  Object.defineProperty(globalThis.crypto, "randomUUID", {
    configurable: true,
    value: undefined,
  });
  let capturedInit;
  globalThis.fetch = async (url, init) => {
    assert.equal(url, "http://relay.local/buzz/api/invites/claim");
    capturedInit = init;
    return new Response(
      JSON.stringify({
        status: "joined",
        community_id: "comm-1",
        host: "relay.local",
        role: "member",
      }),
      { status: 200 },
    );
  };
  try {
    const claimed = await claimInvite("ws://relay.local/buzz/", "code-123");
    assert.equal(claimed.status, "joined");
    assert.equal(capturedInit.redirect, "error");
  } finally {
    globalThis.fetch = originalFetch;
    Object.defineProperty(globalThis.crypto, "subtle", {
      configurable: true,
      value: originalSubtle,
    });
    Object.defineProperty(globalThis.crypto, "randomUUID", {
      configurable: true,
      value: originalRandomUUID,
    });
    teardownTauriStubs();
  }
});

test("getJoinPolicy native transport falls back to HTTP when fetch_join_policy is unavailable in buzz-desktop", async () => {
  const previousWindow = globalThis.window;
  const originalFetch = globalThis.fetch;
  globalThis.window = {
    __TAURI_INTERNALS__: {
      invoke(command) {
        if (command === "fetch_join_policy") {
          const err = new Error("fetch_join_policy is not available");
          err.name = "DesktopUnavailableError";
          return Promise.reject(err);
        }
        return Promise.reject(new Error(`unexpected ${command}`));
      },
    },
  };
  globalThis.fetch = async (url) => {
    assert.equal(url, "https://relay.example/buzz/api/join-policy");
    return new Response(
      JSON.stringify({
        policy: {
          terms_markdown: "# Desktop Terms",
          age_attestation_required: false,
          version: "v2",
        },
      }),
      { status: 200 },
    );
  };
  try {
    const policy = await getJoinPolicy("wss://relay.example/buzz/", "native");
    assert.deepEqual(policy, {
      termsMarkdown: "# Desktop Terms",
      privacyMarkdown: undefined,
      ageAttestationRequired: false,
      version: "v2",
    });
  } finally {
    globalThis.fetch = originalFetch;
    globalThis.window = previousWindow;
  }
});
