// The browser twins of the desktop's download commands must hold the same gate
// the Rust `validate_download_url` check does: relay media only (F166). A
// message's imeta `url`/`filename` and an image `src` are the sender's to
// choose, so an ungated fetch-and-blob saved third-party payloads under a
// chosen name and issued authenticated same-origin GETs.
import assert from "node:assert/strict";
import test from "node:test";

import {
  copyImageToClipboard,
  downloadFile,
  downloadImage,
  relayMediaUrl,
  safeDownloadName,
} from "./workspace.ts";

const SHA = "ab".repeat(32);
const RELAY = {
  httpUrl: "https://work.example/buzz/api",
  pageUrl: "https://work.example/buzz/app/rooms/general",
};

test("only relay media URLs pass the gate", () => {
  const ok = (url) => relayMediaUrl(url, RELAY)?.href ?? null;
  assert.equal(ok(`/buzz/blob/${SHA}.png`), `https://work.example/buzz/blob/${SHA}.png`);
  assert.equal(ok(`https://work.example/buzz/blob/${SHA}.pdf`), `https://work.example/buzz/blob/${SHA}.pdf`);
  assert.equal(ok(`https://WORK.example/media/${SHA}.mp4`), `https://work.example/media/${SHA}.mp4`);
  for (const refused of [
    "https://evil.example/payload.exe",
    `https://evil.example/buzz/blob/${SHA}.png`,
    "data:application/octet-stream;base64,TVqQ",
    "javascript:alert(1)",
    "blob:https://work.example/1234",
    "/buzz/api/me",
    `/buzz/blob/../api/members`,
    `/buzz/blob/%2e%2e/api/members`,
    `https://user:pw@work.example/buzz/blob/${SHA}.png`,
    `http://work.example/buzz/blob/${SHA}.png`,
    "not a url at all \u0000",
  ]) {
    assert.equal(ok(refused), null, refused);
  }
});

test("a relay on another origin keeps its own origin as the only one accepted", () => {
  const relay = { httpUrl: "https://relay.example/buzz/api", pageUrl: "https://app.example/" };
  assert.equal(relayMediaUrl(`/buzz/blob/${SHA}.png`, relay), null, "a relative URL lands on the page origin");
  assert.ok(relayMediaUrl(`https://relay.example/buzz/blob/${SHA}.png`, relay));
});

test("a suggested filename loses paths, controls and a mismatched final extension", () => {
  const media = new URL(`https://work.example/buzz/blob/${SHA}.pdf`);
  assert.equal(safeDownloadName("report.pdf", media), "report.pdf");
  assert.equal(safeDownloadName("quarterly-report.pdf.exe", media), "quarterly-report.pdf.exe.pdf");
  assert.equal(safeDownloadName("../../.bashrc", media), "bashrc.pdf");
  assert.equal(safeDownloadName("C:\\Windows\\evil.PDF", media), "evil.PDF");
  assert.equal(safeDownloadName("invoice\u202Efdp.exe", media), "invoicefdp.exe.pdf");
  assert.equal(safeDownloadName(undefined, media), `${SHA}.pdf`);
  assert.equal(safeDownloadName("\u0000", media), `${SHA}.pdf`);
  assert.equal(
    safeDownloadName("notes", new URL(`https://work.example/media/${SHA}`)),
    "notes",
    "no relay extension, nothing appended",
  );
});

test("an off-relay link is refused before anything is fetched", async () => {
  const originalFetch = globalThis.fetch;
  let fetched = 0;
  globalThis.fetch = async () => {
    fetched += 1;
    throw new Error("network disabled in this test");
  };
  try {
    await assert.rejects(
      downloadFile({ url: "https://evil.example/payload", filename: "a.pdf.exe" }, RELAY),
      /only saves files stored on this workspace's relay/,
    );
    await assert.rejects(
      downloadImage({ url: "data:image/png;base64,iVBORw0KGgo=" }, RELAY),
      /only saves files stored on this workspace's relay/,
    );
    await assert.rejects(
      copyImageToClipboard({ url: "https://work.example/buzz/api/me" }, RELAY),
      /only saves files stored on this workspace's relay/,
    );
    assert.equal(fetched, 0);
  } finally {
    globalThis.fetch = originalFetch;
  }
});
