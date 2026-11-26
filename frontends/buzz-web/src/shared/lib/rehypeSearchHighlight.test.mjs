import assert from "node:assert/strict";
import test from "node:test";

import rehypeSearchHighlight from "./rehypeSearchHighlight.ts";

// F426: search highlighting must not split the single text child of the
// synthetic inline elements whose renderers parse that text.

function element(tagName, children, properties = {}) {
  return { type: "element", tagName, properties, children };
}
function text(value) {
  return { type: "text", value };
}
function highlight(tree, query) {
  rehypeSearchHighlight({ query })(tree);
  return tree;
}
function marks(node) {
  if (node.type !== "element" && node.type !== "root") return 0;
  const own = node.type === "element" && node.tagName === "mark" ? 1 : 0;
  return own + node.children.reduce((sum, child) => sum + marks(child), 0);
}

test("plain paragraph text is still highlighted", () => {
  const tree = highlight(
    { type: "root", children: [element("p", [text("hello alice there")])] },
    "alice",
  );
  assert.equal(marks(tree), 1);
});

test("F426: mention, emoji and link pills keep their one text child", () => {
  const pills = [
    element("mention", [text("@alice")]),
    element("emoji", [text(":alice:")], { src: "https://relay/a.png" }),
    element("message-link", [text("buzz://message?channel=alice&id=1")]),
    element("entity-link", [text("buzz://entity/alice")]),
    element("channel-link", [text("#alice")]),
    element("channel-deep-link", [text("buzz://channel/alice")]),
  ];
  const tree = highlight(
    {
      type: "root",
      children: [element("p", [text("alice said "), ...pills])],
    },
    "alice",
  );
  const paragraph = tree.children[0];
  // The leading plain text is highlighted; the pills are untouched.
  assert.equal(paragraph.children[0].tagName, "mark");
  for (const pill of paragraph.children.filter(
    (child) => child.type === "element" && child.tagName !== "mark",
  )) {
    assert.equal(pill.children.length, 1, pill.tagName);
    assert.equal(pill.children[0].type, "text", pill.tagName);
  }
  assert.equal(marks(tree), 1);
});

test("code stays untouched", () => {
  const tree = highlight(
    {
      type: "root",
      children: [element("pre", [element("code", [text("alice")])])],
    },
    "alice",
  );
  assert.equal(marks(tree), 0);
});
