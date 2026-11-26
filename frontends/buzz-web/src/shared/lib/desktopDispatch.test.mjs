// The hybrid dispatch decision, and the bridge that
// applies it, driven through its exports with a staged window.
import test from "node:test";
import assert from "node:assert/strict";

import {
  DESKTOP_MARKER_VERSION,
  canNativeEvents,
  detectDesktopRuntime,
  dispatchDecision,
  hasRealTauriInternals,
  readDesktopMarker,
  routesNative,
} from "./desktopDispatch.ts";
import * as bridge from "./desktop-bridge.ts";

const realInternals = {
  invoke: () => Promise.resolve(),
  transformCallback: () => 1,
};

test("no marker means pure seam, even with real internals", () => {
  assert.deepEqual(detectDesktopRuntime({ __TAURI_INTERNALS__: realInternals }), {
    native: false,
  });
  assert.deepEqual(detectDesktopRuntime(undefined), { native: false });
  assert.deepEqual(detectDesktopRuntime({}), { native: false });
});

test("an unknown marker version fails CLOSED to pure seam", () => {
  const w = {
    __BUZZ_DESKTOP__: { version: DESKTOP_MARKER_VERSION + 1, commands: ["x"] },
    __TAURI_INTERNALS__: realInternals,
  };
  assert.deepEqual(detectDesktopRuntime(w), { native: false });
});

test("a malformed command list fails CLOSED", () => {
  for (const commands of [undefined, null, "get_git_identity", [1, 2], ["ok", 3]]) {
    const w = {
      __BUZZ_DESKTOP__: { version: DESKTOP_MARKER_VERSION, commands },
      __TAURI_INTERNALS__: realInternals,
    };
    assert.deepEqual(
      detectDesktopRuntime(w),
      { native: false },
      `commands=${JSON.stringify(commands)} must not enable native mode`,
    );
  }
});

test("a mock's internals (no transformCallback) never count as a shell", () => {
  const w = {
    __BUZZ_DESKTOP__: { version: DESKTOP_MARKER_VERSION, commands: ["a"] },
    __TAURI_INTERNALS__: { invoke: () => {} },
  };
  assert.equal(hasRealTauriInternals(w), false);
  assert.deepEqual(detectDesktopRuntime(w), { native: false });
});

test("marker plus real internals is native mode with the registry as a set", () => {
  const w = {
    __BUZZ_DESKTOP__: {
      version: DESKTOP_MARKER_VERSION,
      commands: ["get_git_identity", "clone_project_repository"],
    },
    __TAURI_INTERNALS__: realInternals,
  };
  const rt = detectDesktopRuntime(w);
  assert.equal(rt.native, true);
  assert.equal(routesNative(rt, "get_git_identity"), true);
  assert.equal(routesNative(rt, "clone_project_repository"), true);
  assert.equal(routesNative(rt, "send_message"), false, "unregistered commands stay on the seam");
});

test("readDesktopMarker returns exactly the declared commands", () => {
  const commands = readDesktopMarker({
    __BUZZ_DESKTOP__: { version: DESKTOP_MARKER_VERSION, commands: ["a", "b", "a"] },
  });
  assert.ok(commands);
  assert.deepEqual([...commands].sort(), ["a", "b"]);
});

const nativeRt = detectDesktopRuntime({
  __BUZZ_DESKTOP__: {
    version: DESKTOP_MARKER_VERSION,
    commands: ["get_git_identity", "plugin:event|listen", "plugin:event|unlisten"],
  },
  __TAURI_INTERNALS__: realInternals,
});

test("dispatch: an installed mock wins over everything, native included", () => {
  assert.equal(dispatchDecision(true, nativeRt, "get_git_identity"), "mock");
  assert.equal(dispatchDecision(true, { native: false }, "anything"), "mock");
});

test("dispatch: native only for registered commands, seam for the rest", () => {
  assert.equal(dispatchDecision(false, nativeRt, "get_git_identity"), "native");
  assert.equal(dispatchDecision(false, nativeRt, "send_message"), "seam");
  assert.equal(dispatchDecision(false, { native: false }, "get_git_identity"), "seam");
});

test("events: native event routing needs BOTH plugin:event halves registered", () => {
  assert.equal(canNativeEvents(nativeRt), true);
  const listenOnly = detectDesktopRuntime({
    __BUZZ_DESKTOP__: {
      version: DESKTOP_MARKER_VERSION,
      commands: ["plugin:event|listen"],
    },
    __TAURI_INTERNALS__: realInternals,
  });
  assert.equal(canNativeEvents(listenOnly), false, "listen without unlisten must not route native");
  assert.equal(canNativeEvents({ native: false }), false);
});

/** A window as the shell stages it: marker, genuine internals, recorded IPC. */
function shell(commands) {
  const calls = [];
  return {
    calls,
    window: {
      __BUZZ_DESKTOP__: { version: DESKTOP_MARKER_VERSION, commands },
      __TAURI_INTERNALS__: {
        invoke: async (...call) => {
          calls.push(call);
          return `native:${call[0]}`;
        },
        transformCallback: () => 7,
      },
    },
  };
}

async function onWindow(stage, run) {
  const previous = globalThis.window;
  globalThis.window = stage;
  bridge.__resetDesktopRuntimeForTests();
  try {
    await run();
  } finally {
    bridge.clearMocks();
    globalThis.window = previous;
    bridge.__resetDesktopRuntimeForTests();
  }
}

test("bridge: a registered command reaches the shell with the caller's args and options", async () => {
  const { calls, window: stage } = shell(["get_git_identity"]);
  await onWindow(stage, async () => {
    const options = { headers: { "x-file": "a" } };
    assert.equal(bridge.isTauri(), true);
    assert.equal(
      await bridge.invoke("get_git_identity", { repo: "r" }, options),
      "native:get_git_identity",
    );
    assert.deepEqual(calls, [["get_git_identity", { repo: "r" }, options]]);
  });
});

test("bridge: a shell's internals are not a mock, so an unregistered command stays on the seam", async () => {
  const { calls, window: stage } = shell(["get_git_identity"]);
  await onWindow(stage, async () => {
    await assert.rejects(
      bridge.invoke("some_unclassified_command"),
      (error) =>
        error instanceof bridge.DesktopUnavailableError &&
        /no native backend for "some_unclassified_command"/.test(error.message),
    );
    assert.deepEqual(calls, []);
  });
});

test("bridge: an installed mock wins over the shell and gets the {} default for args", async () => {
  const { calls, window: stage } = shell(["get_git_identity"]);
  await onWindow(stage, async () => {
    bridge.mockIPC((cmd, payload) => ({ cmd, payload }));
    assert.deepEqual(await bridge.invoke("get_git_identity"), {
      cmd: "get_git_identity",
      payload: {},
    });
    assert.deepEqual(calls, []);
  });
});

test("bridge: an internals stub with no transformCallback answers as a mock, and is no shell", async () => {
  const stage = {
    __BUZZ_DESKTOP__: { version: DESKTOP_MARKER_VERSION, commands: ["anything"] },
    __TAURI_INTERNALS__: { invoke: (cmd) => `stub:${cmd}` },
  };
  await onWindow(stage, async () => {
    assert.equal(bridge.isTauri(), false);
    assert.equal(await bridge.invoke("anything"), "stub:anything");
  });
});

test("bridge: invokeRawBinary follows the same order: mock, then registry, then refusal", async () => {
  const bytes = new Uint8Array([1, 2]);
  const { calls, window: stage } = shell(["push_pcm"]);
  await onWindow(stage, async () => {
    assert.equal(await bridge.invokeRawBinary("push_pcm", bytes), "native:push_pcm");
    assert.deepEqual(calls, [["push_pcm", bytes, undefined]]);
    await assert.rejects(
      bridge.invokeRawBinary("unregistered_pcm", bytes),
      bridge.DesktopUnavailableError,
    );
    bridge.mockIPC((cmd, payload) => [cmd, payload]);
    assert.deepEqual(await bridge.invokeRawBinary("push_pcm", bytes), ["push_pcm", bytes]);
    assert.equal(calls.length, 1, "neither the refusal nor the mock reached the shell");
  });
});

test("bridge: listen uses the shell's event system when both halves are registered", async () => {
  const { calls, window: stage } = shell(["plugin:event|listen", "plugin:event|unlisten"]);
  await onWindow(stage, async () => {
    const unlisten = await bridge.listen("ptt-state", () => {});
    unlisten();
    assert.deepEqual(calls, [
      ["plugin:event|listen", { event: "ptt-state", target: { kind: "Any" }, handler: 7 }, undefined],
      ["plugin:event|unlisten", { event: "ptt-state", eventId: "native:plugin:event|listen" }, undefined],
    ]);
  });
});

test("bridge: with one event half registered, events stay on the same-page bus", async () => {
  const { calls, window: stage } = shell(["plugin:event|listen"]);
  await onWindow(stage, async () => {
    const heard = [];
    const unlisten = await bridge.listen("same-page", (event) => heard.push(event.payload));
    await bridge.emit("same-page", 1);
    unlisten();
    await bridge.emit("same-page", 2);
    assert.deepEqual(heard, [1]);
    assert.deepEqual(calls, []);
  });
});

test("bridge: emit goes to the shell only when plugin:event|emit is registered", async () => {
  const { calls, window: stage } = shell(["plugin:event|emit"]);
  await onWindow(stage, async () => {
    const heard = [];
    const unlisten = await bridge.listen("to-shell", (event) => heard.push(event.payload));
    await bridge.emit("to-shell", 3);
    unlisten();
    assert.deepEqual(calls, [["plugin:event|emit", { event: "to-shell", payload: 3 }, undefined]]);
    assert.deepEqual(heard, []);
  });
});

test("bridge: clearMocks keeps a shell's internals and drops a mock's", async () => {
  const { window: stage } = shell(["get_git_identity"]);
  await onWindow(stage, async () => {
    bridge.clearMocks();
    assert.ok(stage.__TAURI_INTERNALS__, "deleting it would sever the app from its backend");
    assert.equal(await bridge.invoke("get_git_identity"), "native:get_git_identity");
  });

  const tab = {};
  await onWindow(tab, async () => {
    bridge.mockIPC(() => "mocked");
    assert.equal(await bridge.invoke("anything"), "mocked");
    bridge.clearMocks();
    assert.equal(tab.__TAURI_INTERNALS__, undefined);
    await assert.rejects(bridge.invoke("some_unclassified_command"), bridge.DesktopUnavailableError);
  });
});
