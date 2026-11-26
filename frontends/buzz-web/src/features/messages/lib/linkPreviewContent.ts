import type { Node as ProseMirrorNode } from "@tiptap/pm/model";

import { buildPlainTextProjection } from "./plainTextProjection";

export function buildPreviewUpdate(
  doc: ProseMirrorNode,
  selectionAnchor: number,
) {
  const projection = buildPlainTextProjection(doc);
  const plainText = projection.text;
  const hrefs = new Set<string>();
  // The plain-text projection carries no marks, so the `||…||` delimiters the
  // preview extractor masks spoilers by are gone from it. Blank the characters
  // of every spoiler-marked text run instead (same length, so offsets hold):
  // a URL the author hid behind a spoiler is then never resolved, previewed or
  // published in the event's link-preview tags.
  // UTF-16 code units, matching the projection's offsets.
  const previewChars = plainText.split("");
  let masked = false;
  doc.descendants((node, pos) => {
    // Code is never previewed: the Markdown extractor masks fenced and inline
    // code, but this projection has lost the fences and backticks, so blank a
    // code block's characters (keeping its line breaks) and every code-marked
    // run, the same way spoilers are blanked (finding_6dc1dd9f12fb2c26).
    if (node.type.spec.code && !node.isText) {
      const from = projection.mapPMToTextOffset(pos + 1);
      const to = projection.mapPMToTextOffset(pos + 1 + node.content.size);
      for (let offset = from; offset < to; offset += 1) {
        if (previewChars[offset] !== "\n") previewChars[offset] = " ";
      }
      masked = true;
      return false;
    }
    if (!node.isText) return;
    if (
      node.marks.some(
        (mark) => mark.type.name === "spoiler" || mark.type.spec.code,
      )
    ) {
      const from = projection.mapPMToTextOffset(pos);
      const to = projection.mapPMToTextOffset(pos + node.nodeSize);
      for (let offset = from; offset < to; offset += 1) {
        previewChars[offset] = " ";
      }
      masked = true;
      return;
    }
    const href = node.marks.find((mark) => mark.type.name === "link")?.attrs
      .href;
    if (typeof href === "string") hrefs.add(href);
  });
  const previewText = masked ? previewChars.join("") : plainText;
  return {
    cursor: projection.mapPMToTextOffset(selectionAnchor),
    linkPreviewContent: [previewText, ...hrefs].join("\n"),
    text: plainText,
  };
}
