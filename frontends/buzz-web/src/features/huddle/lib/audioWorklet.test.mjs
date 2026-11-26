// Regression tests for F376: when audio-worklet setup fails, the
// AudioContext it opened (and any nodes it built) must be released before the
// error reaches the caller, so retries do not exhaust the browser's cap on
// open audio contexts.

import assert from "node:assert/strict";
import { afterEach, test } from "node:test";

const saved = {
  AudioContext: globalThis.AudioContext,
  AudioWorkletNode: globalThis.AudioWorkletNode,
  MediaStream: globalThis.MediaStream,
};

afterEach(() => {
  Object.assign(globalThis, saved);
});

function installFakeAudio({
  state = "running",
  resume,
  addModule,
  close,
  workletNodeThrows = true,
}) {
  const contexts = [];
  const nodes = [];
  const node = (name) => {
    const n = {
      name,
      disconnected: 0,
      connect() {},
      disconnect() {
        this.disconnected += 1;
      },
    };
    nodes.push(n);
    return n;
  };
  globalThis.MediaStream = class {
    constructor(tracks) {
      this.tracks = tracks;
    }
  };
  globalThis.AudioWorkletNode = class {
    constructor() {
      if (workletNodeThrows) throw new Error("processor not registered");
      return Object.assign(node("worklet"), {
        port: { postMessage() {}, onmessage: null },
      });
    }
  };
  globalThis.AudioContext = class {
    constructor() {
      this.state = state;
      this.closed = 0;
      this.moduleUrls = [];
      this.audioWorklet = {
        addModule: (url) => {
          this.moduleUrls.push(url);
          return addModule ? addModule(url) : Promise.resolve();
        },
      };
      contexts.push(this);
    }
    resume() {
      return resume ? resume() : Promise.resolve();
    }
    createMediaStreamSource() {
      return node("source");
    }
    createGain() {
      return Object.assign(node("gain"), { gain: { value: 1 } });
    }
    close() {
      this.closed += 1;
      return close ? close() : Promise.resolve();
    }
  };
  return { contexts, nodes };
}

test("a failed resume closes the audio context", async () => {
  const { contexts } = installFakeAudio({
    state: "suspended",
    resume: () => Promise.reject(new Error("not allowed")),
  });
  const { setupAudioWorklet } = await import("./audioWorklet.ts");
  await assert.rejects(setupAudioWorklet({ kind: "audio" }), /not allowed/);
  assert.equal(contexts.length, 1);
  assert.equal(contexts[0].closed, 1);
});

test("a worklet that fails to load closes the audio context on every retry", async () => {
  const { contexts } = installFakeAudio({
    addModule: () => Promise.reject(new Error("404 worklet.js")),
  });
  const { setupAudioWorklet } = await import("./audioWorklet.ts");
  for (let attempt = 0; attempt < 3; attempt += 1) {
    await assert.rejects(setupAudioWorklet({ kind: "audio" }), /404 worklet/);
  }
  assert.equal(contexts.length, 3);
  assert.deepEqual(
    contexts.map((context) => context.moduleUrls),
    [["/worklet.js"], ["/worklet.js"], ["/worklet.js"]],
    "the module load itself was attempted and failed",
  );
  assert.deepEqual(
    contexts.map((context) => context.closed),
    [1, 1, 1],
  );
});

test("a failure after nodes were built disconnects them and closes the context", async () => {
  const { contexts, nodes } = installFakeAudio({ workletNodeThrows: true });
  const { setupAudioWorklet } = await import("./audioWorklet.ts");
  await assert.rejects(
    setupAudioWorklet({ kind: "audio" }),
    /processor not registered/,
  );
  assert.deepEqual(
    nodes.map((n) => [n.name, n.disconnected]),
    [
      ["source", 1],
      ["gain", 1],
    ],
  );
  assert.equal(contexts[0].closed, 1);
});

test("the setup error reaches the caller only after the context has closed", async () => {
  let finishClose;
  const closing = new Promise((resolve) => {
    finishClose = resolve;
  });
  installFakeAudio({
    addModule: () => Promise.reject(new Error("404 worklet.js")),
    close: () => closing,
  });
  const { setupAudioWorklet } = await import("./audioWorklet.ts");
  let settled = false;
  const setup = setupAudioWorklet({ kind: "audio" }).catch((error) => {
    settled = true;
    return error;
  });
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(settled, false, "still waiting for close()");
  finishClose();
  const error = await setup;
  assert.equal(settled, true);
  assert.match(String(error), /404 worklet/);
});

test("a successful setup keeps its graph until stop()", async () => {
  const { contexts, nodes } = installFakeAudio({ workletNodeThrows: false });
  const { setupAudioWorklet } = await import("./audioWorklet.ts");
  const handlePromise = setupAudioWorklet({ kind: "audio" });
  await handlePromise.then(
    (handle) => {
      assert.equal(contexts[0].closed, 0);
      handle.stop();
      assert.equal(contexts[0].closed, 1);
    },
    () => {
      assert.fail("setup failed");
    },
  );
  assert.ok(nodes.every((n) => n.disconnected === 1), "stop() disconnects");
});
