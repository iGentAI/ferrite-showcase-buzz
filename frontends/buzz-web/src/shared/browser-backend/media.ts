/**
 * MEDIA from a browser tab.
 *
 * buzz's desktop app uploads by handing Rust a FILE PATH and letting it read,
 * hash and post the bytes. A tab has no file paths, but it can hash bytes with
 * WebCrypto, slice them, and post them. So this module
 * is the browser's half of the relay's media routes
 * (`POST /buzz/api/media/begin` and `…/chunk`), and the pickers are the
 * browser's own file dialog.
 *
 * | command | how |
 * | --- | --- |
 * | `upload_media_bytes` | hash → begin → chunk → the relay's URL |
 * | `pick_and_upload_media` | `<input type=file multiple>` → upload each |
 * | `pick_and_upload_image` | the same, images only, single |
 * | `fetch_media_bytes` | a plain `fetch` of the public blob URL |
 * | `upload_media` | refused: a file PATH is genuinely desktop-only |
 *
 * ## What this client deliberately does not decide
 *
 * It computes the digest, but the relay VERIFIES it. It detects the type, but
 * the relay PROVES it against the bytes. It never constructs the object's URL —
 * that comes back from the relay, because a client that built it would be
 * guessing at a key layout it does not own. Every one of those checks exists on
 * the server precisely because a client cannot be trusted to have done it.
 *
 * ## Two things that fall out for free
 *
 * The accept filter comes from `GET /buzz/api/media/limits`, so the picker can
 * never offer a type the upload would then refuse — one allowlist, published.
 *
 * And because storage is content-addressed, posting an image the workspace
 * already holds sends NO bytes: `begin` answers `have: true` and the upload is
 * already finished. Re-sharing is free, and an interrupted upload is safe to
 * simply retry.
 */
import { apiGet, apiPost } from "@/shared/lib/api";
import { relayMediaUrl, type RelayLocation } from "@/shared/browser-backend/workspace";
// Lowercase hex SHA-256, which is how the relay names an object.
import { sha256Hex } from "@/shared/lib/insecureContextCrypto";
import type { BlobDescriptor } from "@/shared/api/tauri";

/** `GET /buzz/api/media/limits`. */
type MediaLimits = {
  max_bytes: number;
  chunk_bytes: number;
  accepted: string[];
  url_prefix: string;
};

type BeginReply = {
  have: boolean;
  url?: string;
  upload?: string;
  chunk_bytes?: number;
};

type ChunkReply = {
  complete: boolean;
  url?: string;
};

/**
 * The deployment's limits, fetched once per page.
 *
 * Cached because the picker asks for them on every open and they change only
 * when the relay is redeployed — at which point the tab reloads anyway. The
 * promise itself is cached, not its result, so concurrent callers share one
 * request instead of racing several.
 */
let limitsOnce: Promise<MediaLimits> | null = null;

function mediaLimits(): Promise<MediaLimits> {
  limitsOnce ??= apiGet<MediaLimits>("/media/limits").catch((error) => {
    // A failed fetch must not poison the cache: the next attempt should try
    // again rather than replay the failure forever.
    limitsOnce = null;
    throw error;
  });
  return limitsOnce;
}

/**
 * Base64 for one chunk.
 *
 * Built in 32 KiB slices rather than one `String.fromCharCode(...bytes)`: spread
 * pushes every byte as an argument, and a 128 KiB chunk overflows the call stack
 * in several engines.
 */
function toBase64(bytes: Uint8Array): string {
  let binary = "";
  const SLICE = 0x8000;
  for (let at = 0; at < bytes.length; at += SLICE) {
    binary += String.fromCharCode(...bytes.subarray(at, at + SLICE));
  }
  return btoa(binary);
}

/** Extension → the MIME the relay's allowlist names it by. */
const BY_EXTENSION: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  avif: "image/avif",
  mp4: "video/mp4",
  webm: "video/webm",
  mp3: "audio/mpeg",
  ogg: "audio/ogg",
  wav: "audio/wav",
  pdf: "application/pdf",
  zip: "application/zip",
  txt: "text/plain",
  csv: "text/csv",
  md: "text/markdown",
};

/** Magic-number sniffing, for bytes that arrive with no name and no type. */
function sniff(bytes: Uint8Array): string | null {
  const starts = (...sig: number[]) => sig.every((b, i) => bytes[i] === b);
  const at = (offset: number, text: string) =>
    Array.from(text).every((c, i) => bytes[offset + i] === c.charCodeAt(0));
  if (starts(0x89, 0x50, 0x4e, 0x47)) return "image/png";
  if (starts(0xff, 0xd8, 0xff)) return "image/jpeg";
  if (at(0, "GIF8")) return "image/gif";
  if (at(0, "RIFF") && at(8, "WEBP")) return "image/webp";
  if (at(0, "RIFF") && at(8, "WAVE")) return "audio/wav";
  if (at(4, "ftyp")) return at(8, "avif") || at(8, "avis") ? "image/avif" : "video/mp4";
  if (starts(0x1a, 0x45, 0xdf, 0xa3)) return "video/webm";
  if (at(0, "OggS")) return "audio/ogg";
  if (at(0, "ID3") || (bytes[0] === 0xff && ((bytes[1] ?? 0) & 0xe0) === 0xe0)) return "audio/mpeg";
  if (at(0, "%PDF-")) return "application/pdf";
  if (starts(0x50, 0x4b)) return "application/zip";
  return null;
}

/**
 * What type these bytes should be declared as.
 *
 * A declared type (the `File`'s own) is preferred, then the filename's
 * extension, then the bytes themselves. The relay re-derives this from the bytes
 * and refuses a mismatch, so being wrong here costs a round trip — never a
 * mislabelled object.
 */
function detectMime(bytes: Uint8Array, filename?: string, declared?: string): string | null {
  const bare = declared?.split(";")[0]?.trim().toLowerCase();
  if (bare && Object.values(BY_EXTENSION).includes(bare)) return bare;
  const ext = filename?.includes(".")
    ? filename.split(".").pop()?.toLowerCase()
    : undefined;
  if (ext && BY_EXTENSION[ext]) return BY_EXTENSION[ext];
  return sniff(bytes);
}

function descriptor(
  url: string,
  sha256: string,
  size: number,
  type: string,
  filename?: string,
): BlobDescriptor {
  return {
    url,
    sha256,
    size,
    type,
    uploaded: Math.floor(Date.now() / 1000),
    ...(filename ? { filename } : {}),
  };
}

/**
 * Upload bytes and return the descriptor the composer attaches.
 *
 * Everything checkable before sending is checked before sending — size against
 * the deployment's cap, type against its published allowlist — so a rejected
 * file fails immediately instead of after eight megabytes.
 */
async function uploadBytes(
  bytes: Uint8Array,
  filename?: string,
  declaredType?: string,
  progressId?: string,
): Promise<BlobDescriptor> {
  // The registry entry is cleared on EVERY terminal path — success, refusal,
  // relay error, and the cancellation throw itself. Without the finally, an
  // upload that failed after its id was marked would leave the id in the set,
  // and a retry under the same progress id would be "cancelled" before its
  // first byte.
  try {
    return await uploadBytesInner(bytes, filename, declaredType, progressId);
  } finally {
    if (progressId) {
      cancelledUploads.delete(progressId);
    }
  }
}

async function uploadBytesInner(
  bytes: Uint8Array,
  filename?: string,
  declaredType?: string,
  progressId?: string,
): Promise<BlobDescriptor> {
  const limits = await mediaLimits();
  assertNotCancelled(progressId);
  if (bytes.length === 0) {
    throw new Error("That file is empty.");
  }
  if (bytes.length > limits.max_bytes) {
    const mb = (limits.max_bytes / (1024 * 1024)).toFixed(0);
    throw new Error(`That file is larger than the ${mb} MB this workspace accepts.`);
  }
  const mime = detectMime(bytes, filename, declaredType);
  if (!mime || !limits.accepted.includes(mime)) {
    throw new Error(
      `This workspace does not accept ${mime ?? "that kind of file"}.`,
    );
  }

  const sha256 = await sha256Hex(bytes);
  const begin = await apiPost<BeginReply>("/media/begin", {
    sha256,
    size: bytes.length,
    mime,
  });
  // Content-addressed: the workspace already holds these exact bytes, so the
  // upload is already done and nothing is sent.
  if (begin.have && begin.url) {
    return descriptor(begin.url, sha256, bytes.length, mime, filename);
  }
  const upload = begin.upload;
  if (!upload) {
    throw new Error("The relay did not open an upload for that file.");
  }

  const chunk = begin.chunk_bytes || limits.chunk_bytes;
  for (let offset = 0; offset < bytes.length; offset += chunk) {
    assertNotCancelled(progressId);
    const slice = bytes.subarray(offset, Math.min(offset + chunk, bytes.length));
    const last = offset + slice.length >= bytes.length;
    const reply = await apiPost<ChunkReply>("/media/chunk", {
      upload,
      offset,
      data: toBase64(slice),
      last,
    });
    // The relay assembles, verifies and publishes on the final chunk, and the
    // URL it answers with is the one to attach — never one built here.
    if (last) {
      if (!reply.complete || !reply.url) {
        throw new Error("The relay did not finish that upload.");
      }
      return descriptor(reply.url, sha256, bytes.length, mime, filename);
    }
  }
  throw new Error("That upload sent no chunks.");
}

/** `upload_media_bytes`: the command the composer calls with raw bytes. */
export async function uploadMediaBytes(args: {
  data?: number[];
  filename?: string;
}): Promise<BlobDescriptor> {
  const data = args.data ?? [];
  return uploadBytes(Uint8Array.from(data), args.filename);
}

/**
 * Uploads cancelled by progress id, checked between chunks.
 *
 * `cancel_media_upload` natively aborts an HTTP request the Rust side owns.
 * Here the upload IS the chunk loop in this module, so cancellation is a set
 * the loop consults before each chunk — real, though at chunk granularity: the
 * chunk in flight completes, the next one never starts. A cancel that lands
 * after the FINAL chunk was accepted is a no-op by design — storage is
 * content-addressed and the relay already holds the object, so reporting a
 * failure would be false. The active id is cleared by `uploadBytes`'s finally
 * on every terminal path, so a stale cancel can never poison a retry.
 */
const cancelledUploads = new Set<string>();

/** `cancel_media_upload`: stop the chunk loop for one background upload. */
export function cancelMediaUpload(args: { progressId?: string }): null {
  if (args.progressId) {
    cancelledUploads.add(args.progressId);
  }
  return null;
}

/**
 * `release_media_upload`: the caller is done with a progress id. Upstream's
 * `finish_media_upload` drops the cancellation record for it; here that is the
 * one set above, so a re-used id can never inherit a stale cancel.
 */
export function releaseMediaUpload(args: { progressId?: string }): null {
  if (args.progressId) {
    cancelledUploads.delete(args.progressId);
  }
  return null;
}

function assertNotCancelled(progressId?: string): void {
  if (progressId && cancelledUploads.has(progressId)) {
    cancelledUploads.delete(progressId);
    throw new Error("upload cancelled");
  }
}

/** The base64url alphabet `encodeRawIpcHeader` writes (tauriMedia.ts), undone. */
function decodeRawIpcHeader(value: string | undefined): string | null {
  if (!value) return null;
  const base64 = value.replaceAll("-", "+").replaceAll("_", "/");
  const padded = base64 + "=".repeat((4 - (base64.length % 4)) % 4);
  try {
    const binary = atob(padded);
    const bytes = new Uint8Array(binary.length);
    for (let at = 0; at < binary.length; at += 1) {
      bytes[at] = binary.charCodeAt(at);
    }
    return new TextDecoder().decode(bytes);
  } catch {
    return null;
  }
}

/**
 * `upload_media_bytes_raw`: upstream's zero-copy upload path — the body is the
 * file, and filename plus progress id ride in mirrored base64url headers. The
 * copy-avoidance mattered for Tauri's JSON IPC; here the same bytes go through
 * the same chunked relay upload as every other media path.
 */
export async function uploadMediaBytesRaw(
  bytes: Uint8Array,
  headers: Record<string, string>,
): Promise<BlobDescriptor> {
  const filename = decodeRawIpcHeader(headers["x-buzz-filename"]);
  const progressId = decodeRawIpcHeader(headers["x-buzz-progress-id"]);
  return uploadBytes(
    bytes,
    filename ?? undefined,
    undefined,
    progressId ?? undefined,
  );
}

/**
 * Open the browser's own file dialog.
 *
 * Resolves with an empty list when the dialog is dismissed — which is harder
 * than it sounds, because dismissing fires no `change`, and the `cancel` event
 * that does fire is not implemented everywhere. Without a fallback the promise
 * simply never settles and the composer waits forever.
 *
 * So both are watched: `cancel` where it exists, and otherwise the window
 * regaining focus with no file chosen. The focus check is deferred briefly
 * because focus returns BEFORE `change` is dispatched on some browsers, and
 * firing first would report a real selection as a dismissal. `done` is
 * idempotent, so whichever arrives first wins.
 */
async function pickFiles(accept: string, multiple: boolean): Promise<File[]> {
  return new Promise<File[]>((resolve) => {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = accept;
    input.multiple = multiple;
    input.style.display = "none";
    document.body.appendChild(input);
    let settled = false;
    const done = (files: File[]) => {
      if (settled) return;
      settled = true;
      window.removeEventListener("focus", onFocus);
      input.remove();
      resolve(files);
    };
    function onFocus() {
      // Long enough for a real selection's `change` to land first.
      window.setTimeout(() => {
        if (!settled && (input.files?.length ?? 0) === 0) done([]);
      }, 500);
    }
    input.addEventListener("change", () => done(Array.from(input.files ?? [])), {
      once: true,
    });
    input.addEventListener("cancel", () => done([]), { once: true });
    window.addEventListener("focus", onFocus);
    input.click();
  });
}

async function uploadFile(file: File): Promise<BlobDescriptor> {
  const bytes = new Uint8Array(await file.arrayBuffer());
  return uploadBytes(bytes, file.name, file.type || undefined);
}

/** `pick_and_upload_media`: any accepted type, several at once. */
export async function pickAndUploadMedia(): Promise<BlobDescriptor[]> {
  const limits = await mediaLimits();
  const files = await pickFiles(limits.accepted.join(","), true);
  const out: BlobDescriptor[] = [];
  for (const file of files) {
    out.push(await uploadFile(file));
  }
  return out;
}

/** `pick_and_upload_image`: one image, or `null` if the dialog was dismissed. */
export async function pickAndUploadImage(): Promise<BlobDescriptor | null> {
  const limits = await mediaLimits();
  const images = limits.accepted.filter((mime) => mime.startsWith("image/"));
  const [file] = await pickFiles(images.join(","), false);
  return file ? uploadFile(file) : null;
}

/**
 * `fetch_media_bytes`: the bytes behind a media URL.
 *
 * The desktop build tunnels this through Rust to dodge CORS for the composer's
 * image editor. Here the media route is same-origin with the app, so a plain
 * fetch gives the canvas its pixels with no proxy at all.
 */
export async function fetchMediaBytes(
  args: { url?: string },
  relay: RelayLocation,
): Promise<ArrayBuffer> {
  const url = args.url;
  if (!url) {
    throw new Error("fetch_media_bytes needs a url.");
  }
  // Only this relay's own media routes, as the desktop's validate_download_url
  // and the browser download twins require: the URL comes from a message, and
  // an unchecked fetch let a sender make every viewer issue credentialed
  // same-origin GETs or zero-click requests to any host
  // (finding_c3fa7525bbdb9578, finding_62e5b5167a8b16d1).
  const media = relayMediaUrl(url, relay);
  if (!media) {
    throw new Error("fetch_media_bytes only reads media stored on this workspace's relay.");
  }
  const response = await fetch(media.href, { redirect: "error" });
  if (!response.ok) {
    throw new Error(`That media could not be fetched (http ${response.status}).`);
  }
  return response.arrayBuffer();
}
