/**
 * Every started upload reaches exactly one terminal signal (F380).
 *
 * `resetBackgroundMediaUploads` force-cancels with `notify: false`, which
 * suppresses `onCancel`; the task loop then skips `onComplete` and `onError`
 * because the task is cancelled. A sender awaiting the upload (the mention send
 * flow) settled only from those three callbacks, so its promise never resolved.
 */
import assert from "node:assert/strict";
import test from "node:test";

import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";

import {
  prepareBackgroundMediaUpload,
  resetBackgroundMediaUploads,
} from "./backgroundMediaUploadStore.ts";

function attachment(id) {
  return {
    file: new File(["x"], `file-${id}.txt`, { type: "text/plain" }),
    id,
    spoilered: false,
  };
}

function tick() {
  return new Promise((resolve) => setTimeout(resolve, 20));
}

test("a forced silent reset still settles a started upload exactly once", async () => {
  const calls = [];
  const upload = prepareBackgroundMediaUpload([attachment(1)]);
  const started = upload.start({
    onCancel: () => calls.push("cancel"),
    onComplete: async () => calls.push("complete"),
    onError: () => calls.push("error"),
    onSettled: () => calls.push("settled"),
  });
  assert.equal(started, true);

  resetBackgroundMediaUploads();
  await tick();

  assert.deepEqual(calls, ["settled"]);
});

test("an ordinary cancel reports onCancel and then settles once", async () => {
  const calls = [];
  const upload = prepareBackgroundMediaUpload([attachment(2)]);
  upload.start({
    onCancel: () => calls.push("cancel"),
    onComplete: async () => calls.push("complete"),
    onError: () => calls.push("error"),
    onSettled: () => calls.push("settled"),
  });

  upload.cancel();
  await tick();

  assert.deepEqual(calls, ["cancel", "settled"]);
});

test("an upload with no attachments settles after completing", async () => {
  const calls = [];
  prepareBackgroundMediaUpload([]).start({
    onComplete: async () => calls.push("complete"),
    onError: () => calls.push("error"),
    onSettled: () => calls.push("settled"),
  });
  await tick();
  assert.deepEqual(calls, ["complete", "settled"]);
});

test("a forced silent reset during the finishing step settles only after it resolves", async () => {
  // A tab has no native uploader, so the desktop bridge's mock seam answers
  // the raw upload with a descriptor and the upload reaches onComplete.
  const previousWindow = globalThis.window;
  globalThis.window = {
    btoa: (value) => Buffer.from(value, "binary").toString("base64"),
  };
  mockIPC((command) => {
    if (command === "upload_media_bytes_raw") {
      return {
        sha256: "a".repeat(64),
        size: 1,
        type: "text/plain",
        uploaded: 1,
        url: `https://relay.example/media/${"a".repeat(64)}.txt`,
      };
    }
    return null;
  });
  try {
    const calls = [];
    let releaseFinishing;
    const finishing = new Promise((resolve) => {
      releaseFinishing = resolve;
    });
    let reachedFinishing;
    const reached = new Promise((resolve) => {
      reachedFinishing = resolve;
    });

    prepareBackgroundMediaUpload([attachment(3)]).start({
      onCancel: () => calls.push("cancel"),
      onComplete: async (descriptors) => {
        calls.push(`complete:${descriptors.length}`);
        reachedFinishing();
        await finishing;
        calls.push("complete-done");
      },
      onError: (error) => calls.push(`error:${error?.message}`),
      onSettled: () => calls.push("settled"),
    });

    await Promise.race([
      reached,
      new Promise((_, reject) =>
        setTimeout(
          () => reject(new Error(`never reached onComplete: ${calls}`)),
          2_000,
        ),
      ),
    ]);

    resetBackgroundMediaUploads();
    await tick();
    assert.deepEqual(
      calls,
      ["complete:1"],
      "no settle (and no onCancel) while the finishing step is still running",
    );

    releaseFinishing();
    await tick();
    assert.deepEqual(calls, ["complete:1", "complete-done", "settled"]);
  } finally {
    clearMocks();
    globalThis.window = previousWindow;
  }
});
