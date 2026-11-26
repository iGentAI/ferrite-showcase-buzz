import assert from "node:assert/strict";
import test from "node:test";

import { getSchema } from "@tiptap/core";
import StarterKit from "@tiptap/starter-kit";

import { buildPreviewUpdate } from "./linkPreviewContent.ts";
import { extractSupportedLinkPreviews } from "@/shared/lib/linkPreview";

const schema = getSchema([
  StarterKit.configure({ heading: false, trailingNode: false, link: false }),
]);
const text = (value, marks) => schema.text(value, marks);
const paragraph = (...content) => schema.nodes.paragraph.create(null, content);
const doc = (...content) => schema.nodes.doc.create(null, content);

const previewHrefs = (update) =>
  extractSupportedLinkPreviews(update.linkPreviewContent).map((p) => p.href);

test("finding_6dc1dd9f12fb2c26: URLs in inline code and code blocks are not previewed", () => {
  const visible = "https://github.com/acme/visible";
  const update = buildPreviewUpdate(
    doc(
      paragraph(
        text("run "),
        text("curl https://github.com/acme/inline", [schema.marks.code.create()]),
      ),
      schema.nodes.codeBlock.create(null, [
        text("curl https://github.com/acme/fenced\necho done"),
      ]),
      paragraph(text(`and see ${visible}`)),
    ),
    0,
  );

  assert.deepEqual(previewHrefs(update), [visible]);
  assert.equal(update.text.includes("acme/inline"), true, "plain text is unchanged");
  assert.equal(update.linkPreviewContent.length, update.text.length, "offsets hold");
});
