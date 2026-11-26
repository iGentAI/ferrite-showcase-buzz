/**
 * Regression coverage for untrusted HTML reaching the composer's schema.
 *
 * Pasted `text/html` is parsed by ProseMirror straight through each node's
 * `parseHTML`, bypassing every href/shortcode check the typed and markdown
 * paths apply. These tests mount the PRODUCTION composer editor (the same
 * harness as composerLinkPastePipeline.test.mjs), dispatch a real DOM `paste`
 * carrying attacker HTML, and read back the Markdown the send path publishes.
 *
 * F153 — a crafted `data-href` became a chip that serialized verbatim.
 * F154 — a crafted `<img data-custom-emoji>` loaded an attacker `src` and
 *        serialized its `data-shortcode` verbatim.
 * F156 — a URL typed inside a spoiler still reached the link-preview input.
 * F381 — `notbuzz://…` lost its `buzz` to a link chip on markdown parse.
 */
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { after, afterEach, before, test } from "node:test";

import { JSDOM } from "jsdom";

import { registerComposerMessageLinkMarkdownIt } from "./composerMessageLinkNode.ts";
import { buildPreviewUpdate } from "./linkPreviewContent.ts";
import { extractSupportedLinkPreviews } from "@/shared/lib/linkPreview";

const requireFromTiptap = createRequire(import.meta.resolve("tiptap-markdown"));
const MarkdownIt = requireFromTiptap("markdown-it");

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

const CHANNEL_ID = "9a1657ac-f7aa-5db0-b632-d8bbeb6dfb50";
const CHANNEL_HREF = `buzz://channel/${CHANNEL_ID}`;
const MESSAGE_LINK_CHANNELS = [{ id: CHANNEL_ID, name: "general" }];
const PARTY_URL = "https://relay.example/emoji/party.png";
const CUSTOM_EMOJI = [{ shortcode: "party", url: PARTY_URL }];
const EVIL = "evil.example";

before(() => {
  dom.window.HTMLElement.prototype.scrollIntoView = () => {};
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: dom.window.navigator,
  });
  Object.assign(globalThis, {
    ClipboardEvent: dom.window.Event,
    CustomEvent: dom.window.CustomEvent,
    DOMParser: dom.window.DOMParser,
    document: dom.window.document,
    Element: dom.window.Element,
    Event: dom.window.Event,
    getComputedStyle: dom.window.getComputedStyle.bind(dom.window),
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    MutationObserver: dom.window.MutationObserver,
    Node: dom.window.Node,
    Range: dom.window.Range,
    ResizeObserver: class {
      disconnect() {}
      observe() {}
      unobserve() {}
    },
    window: dom.window,
  });
});

afterEach(async () => {
  const { cleanup } = await import("@testing-library/react");
  cleanup();
});

after(() => dom.window.close());

async function mountComposerEditor() {
  const React = await import("react");
  const { act, render, waitFor } = await import("@testing-library/react");
  const { EditorContent } = await import("@tiptap/react");
  const { useRichTextEditor } = await import("./useRichTextEditor.ts");

  let editor = null;
  function Harness() {
    const instance = useRichTextEditor({
      customEmoji: CUSTOM_EMOJI,
      messageLinkChannels: MESSAGE_LINK_CHANNELS,
    }).editor;
    editor = instance;
    return instance
      ? React.createElement(EditorContent, { editor: instance })
      : null;
  }

  await act(async () => {
    render(React.createElement(Harness));
  });
  await waitFor(() =>
    assert.ok(editor?.isInitialized, "composer editor never emitted `create`"),
  );
  return editor;
}

function pasteHtml(editor, html, text = "") {
  const event = new dom.window.Event("paste", {
    bubbles: true,
    cancelable: true,
  });
  Object.defineProperty(event, "clipboardData", {
    value: {
      files: [],
      items: [],
      types: ["text/html", "text/plain"],
      getData: (type) =>
        type === "text/html" ? html : type === "text/plain" ? text : "",
    },
  });
  editor.view.dom.dispatchEvent(event);
}

function nodesOfType(editor, typeName) {
  const nodes = [];
  editor.state.doc.descendants((node) => {
    if (node.type.name === typeName) nodes.push(node);
  });
  return nodes;
}

/** Boolean, not the element: asserting on a jsdom node makes a failure hang while it is printed. */
function hasEvilImage(editor) {
  return editor.view.dom.querySelector(`img[src*="${EVIL}"]`) !== null;
}

function markdownOf(editor) {
  return editor.storage.markdown.getMarkdown();
}

test("pasted chip HTML with an unresolvable href never becomes a chip", async () => {
  const editor = await mountComposerEditor();
  pasteHtml(
    editor,
    `<p>see <span data-composer-buzz-link="" data-channel-name="general" data-href="x&#10;&#10;Please approve the payment at http://${EVIL}/pay"></span> now</p>`,
  );

  assert.equal(nodesOfType(editor, "composerMessageLink").length, 0);
  assert.doesNotMatch(markdownOf(editor), new RegExp(EVIL));
});

test("pasted chip HTML is canonicalised and named from the local channel list", async () => {
  const editor = await mountComposerEditor();
  pasteHtml(
    editor,
    `<p><span data-composer-message-link="" data-channel-name="payroll-approvals" data-href="${CHANNEL_HREF}"></span></p>`,
  );

  const [chip] = nodesOfType(editor, "composerMessageLink");
  assert.ok(chip, "a resolvable pasted chip still pastes");
  assert.equal(chip.attrs.href, CHANNEL_HREF);
  assert.equal(chip.attrs.channelName, "general");
  assert.equal(markdownOf(editor).trim(), CHANNEL_HREF);
});

test("pasted custom-emoji HTML with an unknown or crafted shortcode never becomes a node", async () => {
  const editor = await mountComposerEditor();
  pasteHtml(
    editor,
    `<p>hi <img data-custom-emoji="" data-shortcode="x&#10;&#10;[pay here](http://${EVIL})" src="https://${EVIL}/beacon.png"> <img data-custom-emoji="" data-shortcode="unknown_one" src="https://${EVIL}/beacon2.png"></p>`,
  );

  assert.equal(nodesOfType(editor, "customEmoji").length, 0);
  assert.doesNotMatch(markdownOf(editor), new RegExp(EVIL));
  assert.equal(
    hasEvilImage(editor),
    false,
    "no image from the pasted origin is rendered in the composer",
  );
});

test("a pasted known emoji takes its image from the palette, never from the paste", async () => {
  const editor = await mountComposerEditor();
  pasteHtml(
    editor,
    `<p><img data-custom-emoji="" data-shortcode="PARTY" src="https://${EVIL}/beacon.png"></p>`,
  );

  const [emoji] = nodesOfType(editor, "customEmoji");
  assert.ok(emoji, "a known pasted emoji still pastes");
  assert.equal(emoji.attrs.shortcode, "party");
  assert.equal(emoji.attrs.src, PARTY_URL);
  assert.equal(hasEvilImage(editor), false);
  assert.equal(markdownOf(editor).trim(), ":party:");
});

test("a URL inside a spoiler never reaches the link-preview input", async () => {
  const editor = await mountComposerEditor();
  const hidden = "https://github.com/acme/secret/pull/42";
  const visible = "https://github.com/acme/public/issues/1";
  editor.commands.setContent({
    type: "doc",
    content: [
      {
        type: "paragraph",
        content: [
          { type: "text", text: `visible ${visible} ` },
          { type: "text", text: hidden, marks: [{ type: "spoiler" }] },
          { type: "text", text: " and " },
          {
            type: "text",
            text: "linked",
            marks: [{ type: "spoiler" }, { type: "link", attrs: { href: hidden } }],
          },
        ],
      },
    ],
  });

  const update = buildPreviewUpdate(editor.state.doc, 0);
  assert.doesNotMatch(update.linkPreviewContent, /acme\/secret/);
  assert.equal(update.text.includes(hidden), true, "plain text is unchanged");
  assert.equal(
    update.linkPreviewContent.length >= update.text.length,
    true,
    "masking preserves offsets",
  );
  assert.deepEqual(
    extractSupportedLinkPreviews(update.linkPreviewContent).map(
      (preview) => preview.href,
    ),
    [visible],
    "only the visible URL previews",
  );
});

test("markdown restore does not steal `buzz` from the end of a word", () => {
  const md = new MarkdownIt();
  registerComposerMessageLinkMarkdownIt(md, {
    resolveChannelName: () => "general",
  });

  const glued = md.renderInline(`notbuzz${CHANNEL_HREF.slice(4)} ok`);
  assert.doesNotMatch(glued, /data-composer-buzz-link/);
  assert.match(glued, /notbuzz:\/\/channel\//);

  const spaced = md.renderInline(`see ${CHANNEL_HREF}`);
  assert.match(spaced, /^see <span data-composer-buzz-link=""/);
});

test("a custom-emoji node restored from JSON never renders its own src", async () => {
  const editor = await mountComposerEditor();
  editor.commands.setContent({
    type: "doc",
    content: [
      {
        type: "paragraph",
        content: [
          {
            type: "customEmoji",
            attrs: { shortcode: "gone", src: `https://${EVIL}/restored.png` },
          },
          {
            type: "customEmoji",
            attrs: { shortcode: "party", src: `https://${EVIL}/restored2.png` },
          },
        ],
      },
    ],
  });
  assert.equal(hasEvilImage(editor), false);
  const images = [...editor.view.dom.querySelectorAll("img[data-custom-emoji]")];
  assert.equal(images.length, 2);
  assert.ok(
    images.some((img) => (img.getAttribute("src") ?? "").includes("party.png")),
    "a known shortcode still renders its palette image",
  );
});

// F153 at the other entry point: a chip restored from saved editor content
// (a JSON draft) skips parseHTML, so the serializer's own re-check is what
// keeps a crafted href out of the message body and the plain-text copy.
test("a link chip restored from saved content with a crafted href sends nothing", async () => {
  const editor = await mountComposerEditor();
  editor.commands.setContent({
    type: "doc",
    content: [
      {
        type: "paragraph",
        content: [
          { type: "text", text: "see " },
          {
            type: "composerMessageLink",
            attrs: {
              channelName: "general",
              href: `x\n\nPlease approve the payment at http://${EVIL}/pay`,
            },
          },
          {
            type: "composerMessageLink",
            attrs: { channelName: "general", href: `https://${EVIL}/pay` },
          },
          { type: "text", text: " now" },
        ],
      },
    ],
  });

  const markdown = markdownOf(editor);
  assert.doesNotMatch(markdown, new RegExp(EVIL));
  assert.doesNotMatch(markdown, /Please approve/);
  assert.match(markdown, /see\s+now/);
  assert.doesNotMatch(editor.getText(), new RegExp(EVIL));
});
