import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { getSchema } from "@tiptap/core";
import { EditorState } from "@tiptap/pm/state";
import StarterKit from "@tiptap/starter-kit";
import { JSDOM } from "jsdom";

import { ComposerMessageLinkNode } from "./composerMessageLinkNode.ts";
import { useComposerMessageLinks } from "./useComposerMessageLinks.ts";

// F388: syncChannelNames only understood message links, so every channel-link
// chip had its resolved name cleared on each channel-list change, and every
// chip was blanked while the channel list was still loading.

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
});

after(() => dom.window.close());

const schema = getSchema([
  StarterKit.configure({ heading: false, trailingNode: false, link: false }),
  ComposerMessageLinkNode.configure({ resolveChannelName: () => undefined }),
]);

const CHANNEL_ID = "9a1657ac-f7aa-5db0-b632-d8bbeb6dfb50";
const MESSAGE_HREF = `buzz://message?channel=${CHANNEL_ID}&id=${"e".repeat(64)}`;
const CHANNEL_HREF = `buzz://channel/${CHANNEL_ID}`;
const REPO_HREF = `buzz://repo?owner=${"a".repeat(64)}&d=buzz-world`;

const chip = (href, channelName) =>
  schema.nodes.composerMessageLink.create({ href, channelName });

function editorWith(...chips) {
  let state = EditorState.create({
    doc: schema.nodes.doc.create(null, [
      schema.nodes.paragraph.create(null, chips),
    ]),
  });
  const dispatched = [];
  return {
    dispatched,
    get state() {
      return state;
    },
    view: {
      dispatch(transaction) {
        dispatched.push(transaction);
        state = state.apply(transaction);
      },
    },
  };
}

function chipNames(editor) {
  const names = [];
  editor.state.doc.descendants((node) => {
    if (node.type.name === "composerMessageLink") {
      names.push(node.attrs.channelName);
    }
  });
  return names;
}

async function syncWith(channels, editor) {
  const { renderHook } = await import("@testing-library/react");
  const view = renderHook(() => useComposerMessageLinks(channels));
  view.result.current.syncChannelNames(editor);
}

test("a channel-link chip keeps its name when the channel list changes (F388)", async () => {
  const editor = editorWith(
    chip(CHANNEL_HREF, "general"),
    chip(MESSAGE_HREF, "general"),
  );
  await syncWith([{ id: "another-channel", name: "random" }], editor);
  assert.deepEqual(chipNames(editor), ["general", "general"]);
  assert.equal(editor.dispatched.length, 0, "no transaction for no change");
});

test("a channel list that has not loaded yet blanks nothing (F388)", async () => {
  for (const channels of [[], undefined]) {
    const editor = editorWith(
      chip(MESSAGE_HREF, "general"),
      chip(CHANNEL_HREF, "general"),
    );
    await syncWith(channels, editor);
    assert.deepEqual(chipNames(editor), ["general", "general"]);
    assert.equal(editor.dispatched.length, 0);
  }
});

test("a renamed channel reaches message and channel chips, not entity chips (F388)", async () => {
  const editor = editorWith(
    chip(MESSAGE_HREF, "general"),
    chip(CHANNEL_HREF, ""),
    chip(REPO_HREF, ""),
  );
  await syncWith([{ id: CHANNEL_ID, name: "announcements" }], editor);
  assert.deepEqual(chipNames(editor), ["announcements", "announcements", ""]);
  assert.equal(editor.dispatched.length, 1, "one transaction for the batch");
  assert.equal(editor.dispatched[0].getMeta("addToHistory"), false);
});
