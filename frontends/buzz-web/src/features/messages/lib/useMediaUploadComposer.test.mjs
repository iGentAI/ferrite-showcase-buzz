import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { emit } from "@tauri-apps/api/event";
import { JSDOM } from "jsdom";

import { useMediaUpload } from "./useMediaUpload.ts";

// The composer's media hook, rendered for real against a mocked command
// backend (`window.__TAURI_INTERNALS__.invoke`, the seam the desktop bridge
// routes every command through when a mock is installed).

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

afterEach(async () => {
  const { cleanup } = await import("@testing-library/react");
  cleanup();
  delete window.__TAURI_INTERNALS__;
});

after(() => dom.window.close());

const SHA = "a".repeat(64);

function descriptor(name, sha256 = SHA) {
  return {
    url: `https://relay.example/media/${name}`,
    sha256,
    size: 1,
    type: "image/png",
    uploaded: 0,
  };
}

const png = (name) => new File(["png"], name, { type: "image/png" });

/** A backend whose uploads stay in flight until the test settles them. */
function installBackend({ bytesUpload } = {}) {
  const calls = [];
  const pendingUploads = [];
  window.__TAURI_INTERNALS__ = {
    invoke: (command, args) => {
      calls.push({ command, args });
      if (command === "upload_media_bytes_raw") {
        return new Promise((resolve, reject) => {
          pendingUploads.push({ resolve, reject });
        });
      }
      if (command === "upload_media_bytes") return bytesUpload;
      return null;
    },
  };
  const named = (command) => calls.filter((call) => call.command === command);
  return { calls, named, pendingUploads };
}

async function renderMediaHook() {
  const { renderHook } = await import("@testing-library/react");
  return renderHook(() => useMediaUpload());
}

// F389: every hook instance numbered its previews from 0, so two composers'
// concurrent uploads shared one progress/cancellation id.
test("two composers' uploads get distinct progress ids (F389)", async () => {
  const { act, waitFor } = await import("@testing-library/react");
  installBackend();
  const channel = await renderMediaHook();
  const thread = await renderMediaHook();

  act(() => {
    void channel.result.current.uploadFile(png("a.png"));
    void thread.result.current.uploadFile(png("b.png"));
  });
  const [channelPreview] = channel.result.current.uploadingPreviews;
  const [threadPreview] = thread.result.current.uploadingPreviews;
  assert.notEqual(channelPreview.id, threadPreview.id);

  // Progress for the channel composer's upload moves only its bar.
  await waitFor(async () => {
    await act(() =>
      emit("media-upload-progress", {
        id: `composer-upload-${channelPreview.id}`,
        sent: 50,
        total: 100,
      }),
    );
    assert.equal(channel.result.current.uploadingPreviews[0].progress, 50);
  });
  assert.equal(thread.result.current.uploadingPreviews[0].progress, undefined);
});

// F390: cancel only hid the preview; the transfer ran to completion and the
// file became a stored blob anyway.
test("cancelling a dispatched upload cancels its transfer, then releases it (F390)", async () => {
  const { act, waitFor } = await import("@testing-library/react");
  const backend = installBackend();
  const media = await renderMediaHook();

  act(() => {
    void media.result.current.uploadFile(png("secret.png"));
  });
  const [preview] = media.result.current.uploadingPreviews;
  const progressId = `composer-upload-${preview.id}`;
  await waitFor(() =>
    assert.equal(backend.named("upload_media_bytes_raw").length, 1),
  );

  act(() => media.result.current.cancelUpload(preview.id));
  await waitFor(() =>
    assert.deepEqual(
      backend.named("cancel_media_upload").map((call) => call.args),
      [{ progressId }],
    ),
  );
  assert.equal(media.result.current.uploadingPreviews.length, 0);
  assert.equal(media.result.current.isUploading, false);

  // The transfer stops (the backend reports the cancel); the record is
  // released after the cancel, and nothing is attached.
  await act(async () => {
    backend.pendingUploads[0].reject(new Error("upload cancelled"));
  });
  await waitFor(() =>
    assert.deepEqual(
      backend.named("release_media_upload").map((call) => call.args),
      [{ progressId }],
    ),
  );
  const commands = backend.calls.map((call) => call.command);
  assert.ok(
    commands.indexOf("cancel_media_upload") <
      commands.indexOf("release_media_upload"),
  );
  assert.deepEqual(media.result.current.pendingImeta, []);
  assert.equal(media.result.current.uploadState.status, "idle");
});

test("cancelling before the bytes are dispatched never sends them (F390)", async () => {
  const { act, waitFor } = await import("@testing-library/react");
  const backend = installBackend();
  const media = await renderMediaHook();

  act(() => {
    void media.result.current.uploadFile(png("oops.png"));
  });
  const [preview] = media.result.current.uploadingPreviews;
  // Still reading the file: the abort signal stops it before dispatch.
  act(() => media.result.current.cancelUpload(preview.id));

  await waitFor(() =>
    assert.equal(backend.named("release_media_upload").length, 1),
  );
  assert.equal(backend.named("upload_media_bytes_raw").length, 0);
  assert.equal(media.result.current.uploadState.status, "idle");
});

test("switching drafts stops the old draft's transfers (F390)", async () => {
  const { act, waitFor } = await import("@testing-library/react");
  const backend = installBackend();
  const media = await renderMediaHook();

  act(() => {
    void media.result.current.uploadFile(png("draft-a.png"));
  });
  const [preview] = media.result.current.uploadingPreviews;
  await waitFor(() =>
    assert.equal(backend.named("upload_media_bytes_raw").length, 1),
  );

  act(() => media.result.current.setPendingImeta([]));
  await waitFor(() =>
    assert.deepEqual(
      backend.named("cancel_media_upload").map((call) => call.args),
      [{ progressId: `composer-upload-${preview.id}` }],
    ),
  );
  assert.equal(media.result.current.isUploading, false);
});

// F391: remove, annotate and revert mapped over every slot with the URL, so
// acting on one of two identical attachments changed both.
test("remove, annotate and revert each change one of two identical attachments (F391)", async () => {
  const { act, waitFor } = await import("@testing-library/react");
  const edited = descriptor("edited.png", "c".repeat(64));
  installBackend({ bytesUpload: edited });
  const media = await renderMediaHook();
  const copy = descriptor("same.png");
  const other = descriptor("other.png", "b".repeat(64));

  act(() => media.result.current.setPendingImeta([copy, other, copy]));
  act(() => media.result.current.removeAttachment(copy.url));
  assert.deepEqual(media.result.current.pendingImeta, [other, copy]);

  act(() => media.result.current.setPendingImeta([copy, other, copy]));
  await act(async () => {
    await media.result.current.uploadEditedAttachment(
      copy.url,
      new Uint8Array([1, 2, 3]),
    );
  });
  await waitFor(() =>
    assert.deepEqual(media.result.current.pendingImeta, [edited, other, copy]),
  );

  act(() => {
    media.result.current.revertAttachment(edited.url);
  });
  assert.deepEqual(media.result.current.pendingImeta, [copy, other, copy]);
});

test("an upload started right after a draft restore lands after the restored attachments", async () => {
  const { act, waitFor } = await import("@testing-library/react");
  const backend = installBackend();
  const hook = await renderMediaHook();
  const d1 = descriptor("d1.png", "1".repeat(64));
  const d2 = descriptor("d2.png", "2".repeat(64));

  act(() => {
    hook.result.current.setPendingImeta([d1, d2]);
    void hook.result.current.uploadFile(png("new.png"));
  });
  await waitFor(() => assert.equal(backend.pendingUploads.length, 1));
  const uploaded = descriptor("new.png", "3".repeat(64));
  await act(async () => backend.pendingUploads[0].resolve(uploaded));

  await waitFor(() =>
    assert.deepEqual(
      hook.result.current.pendingImeta.map((d) => d.url),
      [d1.url, d2.url, uploaded.url],
    ),
  );
});

test("a pasted image keeps its place ahead of a file added while it uploads", async () => {
  const { act, waitFor } = await import("@testing-library/react");
  const backend = installBackend();
  const hook = await renderMediaHook();

  act(() => {
    void hook.result.current.uploadFile(png("pasted.png"));
  });
  act(() => {
    void hook.result.current.uploadFile(png("dropped.png"));
  });
  await waitFor(() => assert.equal(backend.pendingUploads.length, 2));
  const pasted = descriptor("pasted.png", "4".repeat(64));
  const dropped = descriptor("dropped.png", "5".repeat(64));
  await act(async () => backend.pendingUploads[1].resolve(dropped));
  await act(async () => backend.pendingUploads[0].resolve(pasted));

  await waitFor(() =>
    assert.deepEqual(
      hook.result.current.pendingImeta.map((d) => d.url),
      [pasted.url, dropped.url],
    ),
  );
});
