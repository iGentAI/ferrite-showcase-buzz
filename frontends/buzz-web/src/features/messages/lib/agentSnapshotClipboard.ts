import { getCachedRelayOrigin } from "@/shared/lib/mediaUrl";
import type { BlobDescriptor } from "@/shared/api/tauri";
import type { ImetaMedia } from "./imetaMediaMarkdown";

// Keep the original attribute name for clipboard compatibility. The payload
// now accepts both agent and team snapshot filenames.
const CLIPBOARD_ATTRIBUTE = "data-buzz-agent-snapshot";
const MAX_AGENT_SNAPSHOT_BYTES = 10 * 1024 * 1024;
const MAX_TEAM_SNAPSHOT_BYTES = 50 * 1024 * 1024;
const MAX_DISPLAY_NAME_LENGTH = 200;
const MAX_FILENAME_LENGTH = 255;

type SnapshotClipboardPayload = {
  version: 1;
  displayName: string;
  filename: string;
  sha256: string;
  size: number;
  type: string;
  url: string;
};

function escapeHtml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll('"', "&quot;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}

// Line breaks, other control characters and bidirectional overrides in a
// pasted name would survive into the outgoing Markdown (the attachment label is
// written as `[label](url)`), letting a crafted clipboard append its own lines
// to a message the victim signs, or disguise the visible name. A real snapshot
// name never contains them, so such a payload is refused outright.
const UNSAFE_LABEL_CHARACTERS =
  /[\u0000-\u001f\u007f-\u009f\u2028\u2029\u202a-\u202e\u2066-\u2069]/u;

function isSafeSnapshotUrl(value: string): boolean {
  // The raw value is later emitted inside Markdown's `(url)` syntax. Keep
  // delimiters and whitespace out even when URL parsing would normalize them.
  if (/[\s()]/u.test(value)) return false;

  try {
    const url = new URL(value);
    if (url.protocol !== "https:" && url.protocol !== "http:") return false;
    if (url.username || url.password) return false;
    // A snapshot lives in this workspace's relay media store. Any other host
    // would be fetched the moment the paste renders, a tracking beacon the
    // clipboard author chose (finding_83f3010cb4b8e8a3).
    const relayOrigin = getCachedRelayOrigin();
    if (!relayOrigin || url.origin !== new URL(relayOrigin).origin) return false;
    return /\/(?:media|blob)\//.test(url.pathname);
  } catch {
    return false;
  }
}

/** Build Buzz-specific clipboard HTML with the raw URL as its visible link. */
export function buildSnapshotClipboardHtml({
  attachment,
  displayName,
  snapshotKind,
}: {
  attachment: ImetaMedia;
  displayName: string;
  snapshotKind: "agent" | "team";
}): string {
  const payload: SnapshotClipboardPayload = {
    version: 1,
    displayName,
    filename: attachment.filename ?? `shared.${snapshotKind}.png`,
    sha256: attachment.sha256,
    size: attachment.size,
    type: attachment.type,
    url: attachment.url,
  };
  const encodedPayload = encodeURIComponent(JSON.stringify(payload));

  return `<a ${CLIPBOARD_ATTRIBUTE}="${encodedPayload}" href="${escapeHtml(attachment.url)}">${escapeHtml(displayName)}</a>`;
}

export function buildAgentSnapshotClipboardHtml(
  input: Omit<Parameters<typeof buildSnapshotClipboardHtml>[0], "snapshotKind">,
): string {
  return buildSnapshotClipboardHtml({ ...input, snapshotKind: "agent" });
}

export function buildTeamSnapshotClipboardHtml(
  input: Omit<Parameters<typeof buildSnapshotClipboardHtml>[0], "snapshotKind">,
): string {
  return buildSnapshotClipboardHtml({ ...input, snapshotKind: "team" });
}

/**
 * Restore a copied agent or team snapshot as composer attachment state.
 *
 * Clipboard HTML is untrusted input, so every field is checked before it can
 * become an outbound imeta tag. Invalid or non-snapshot payloads fall through to
 * the composer's normal paste behavior.
 */
export function parseSnapshotClipboardHtml(html: string): ImetaMedia | null {
  const match = html.match(
    new RegExp(`\\b${CLIPBOARD_ATTRIBUTE}=(?:"([^"]+)"|'([^']+)')`, "i"),
  );
  const encodedPayload = match?.[1] ?? match?.[2];
  if (!encodedPayload) return null;

  let payload: unknown;
  try {
    payload = JSON.parse(decodeURIComponent(encodedPayload));
  } catch {
    return null;
  }

  if (!payload || typeof payload !== "object") return null;
  const candidate = payload as Partial<SnapshotClipboardPayload>;
  const displayName =
    typeof candidate.displayName === "string"
      ? candidate.displayName.trim()
      : undefined;
  const filename =
    typeof candidate.filename === "string"
      ? candidate.filename.trim()
      : undefined;
  const sha256 =
    typeof candidate.sha256 === "string"
      ? candidate.sha256.trim().toLowerCase()
      : undefined;
  const lowerFilename = filename?.toLowerCase();
  const maxSnapshotBytes = lowerFilename?.includes(".team.")
    ? MAX_TEAM_SNAPSHOT_BYTES
    : MAX_AGENT_SNAPSHOT_BYTES;
  const isPngSnapshot = /\.(?:agent|team)\.png$/i.test(filename ?? "");
  const isJsonSnapshot = /\.(?:agent|team)\.json$/i.test(filename ?? "");

  if (
    candidate.version !== 1 ||
    !displayName ||
    displayName.length > MAX_DISPLAY_NAME_LENGTH ||
    UNSAFE_LABEL_CHARACTERS.test(displayName) ||
    !filename ||
    filename.length > MAX_FILENAME_LENGTH ||
    UNSAFE_LABEL_CHARACTERS.test(filename) ||
    filename.includes("/") ||
    filename.includes("\\") ||
    (!isPngSnapshot && !isJsonSnapshot) ||
    !sha256 ||
    !/^[0-9a-f]{64}$/.test(sha256) ||
    typeof candidate.size !== "number" ||
    !Number.isInteger(candidate.size) ||
    candidate.size <= 0 ||
    candidate.size > maxSnapshotBytes ||
    (isPngSnapshot
      ? candidate.type !== "image/png"
      : candidate.type !== "application/json") ||
    typeof candidate.url !== "string" ||
    !isSafeSnapshotUrl(candidate.url)
  ) {
    return null;
  }

  return {
    displayLabel: displayName,
    filename,
    sha256,
    size: candidate.size,
    type: isPngSnapshot ? "image/png" : "application/json",
    uploaded: 0,
    url: candidate.url,
  };
}

export const parseAgentSnapshotClipboardHtml = parseSnapshotClipboardHtml;

/** Convert a Buzz snapshot clipboard payload into one pending attachment. */
export function handleSnapshotPaste(
  event: Pick<ClipboardEvent, "clipboardData" | "preventDefault">,
  setPendingImeta: (
    update: (current: BlobDescriptor[]) => BlobDescriptor[],
  ) => void,
): boolean {
  const html = event.clipboardData?.getData("text/html") ?? "";
  const pastedSnapshot = parseSnapshotClipboardHtml(html);
  if (!pastedSnapshot) return false;

  event.preventDefault();
  setPendingImeta((current) =>
    current.some(
      (attachment) =>
        attachment.url === pastedSnapshot.url &&
        attachment.sha256 === pastedSnapshot.sha256,
    )
      ? current
      : [...current, pastedSnapshot],
  );
  return true;
}

export const handleAgentSnapshotPaste = handleSnapshotPaste;
