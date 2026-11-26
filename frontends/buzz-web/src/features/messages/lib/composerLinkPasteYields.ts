import { parseSnapshotClipboardHtml } from "./agentSnapshotClipboard";
import { hasMentionClipboardHtml } from "./normalizeMentionClipboard";
import { getBuzzCodeBlockClipboardText } from "@/shared/lib/codeBlockClipboard";

/**
 * Whether the composer's DOM-level link paste must stand aside for the
 * editor's own paste handling. That handler runs first and turns a URL in
 * `text/plain` into a link or chip, so a paste that also carries a file (a
 * browser "Copy Image"), a Buzz code block, a Buzz mention or an agent
 * snapshot would otherwise lose its upload, its code block or its identity.
 */
export function composerLinkPasteYields(
  clipboardData: DataTransfer | null | undefined,
): boolean {
  if (!clipboardData) return false;
  if (Array.from(clipboardData.items ?? []).some((item) => item.kind === "file")) {
    return true;
  }
  const html = clipboardData.getData("text/html") ?? "";
  if (!html) return false;
  return (
    getBuzzCodeBlockClipboardText(clipboardData) !== null ||
    hasMentionClipboardHtml(html) ||
    Boolean(parseSnapshotClipboardHtml(html))
  );
}
