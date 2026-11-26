/**
 * Rehype plugin that highlights text matching a search query by wrapping
 * matches in `<mark>` elements during the HAST (HTML AST) phase.
 *
 * This runs inside the react-markdown pipeline, so it works correctly with
 * ReactMarkdown's architecture — no post-render tree walking needed.
 */

import { splitSearchMatches } from "@/features/search/lib/searchMatch";
import { SEARCH_MATCH_HIGHLIGHT_CLASS } from "@/shared/lib/searchHighlightStyle";

// Minimal HAST types — matches the pattern in rehypeImageGallery.ts.
interface HastText {
  type: "text";
  value: string;
}

interface HastElement {
  type: "element";
  tagName: string;
  properties: Record<string, unknown>;
  children: HastNode[];
}

type HastNode = HastElement | HastText | { type: string };

interface HastRoot {
  type: "root";
  children: HastNode[];
}

function isElement(node: HastNode): node is HastElement {
  return node.type === "element";
}

function isText(node: HastNode): node is HastText {
  return node.type === "text";
}

/**
 * Elements whose text is never split into highlight fragments.
 *
 * `code` and `pre` keep code untouched. The rest are the inline elements the
 * remark plugins synthesize (`data.hName` with one text child): their
 * renderers read that single text child as the mention label, the emoji
 * shortcode or the link to parse, so wrapping part of it in `<mark>` turned
 * mention chips and message/entity/channel pills into `[object Object]`
 * text (F426).
 */
const NO_HIGHLIGHT_TAGS = new Set([
  "code",
  "pre",
  "mention",
  "emoji",
  "message-link",
  "entity-link",
  "channel-link",
  "channel-deep-link",
]);

export default function rehypeSearchHighlight({ query }: { query: string }) {
  return (tree: HastRoot) => {
    function walk(nodes: HastNode[]): HastNode[] {
      const result: HastNode[] = [];

      for (const node of nodes) {
        if (isText(node)) {
          const parts = splitSearchMatches(node.value, query);
          if (!parts.some((part) => part.isMatch)) {
            result.push(node);
            continue;
          }

          for (const part of parts) {
            if (part.isMatch) {
              result.push({
                type: "element",
                tagName: "mark",
                properties: {
                  className: SEARCH_MATCH_HIGHLIGHT_CLASS,
                  "data-search-match": "true",
                },
                children: [{ type: "text", value: part.text }],
              });
            } else {
              result.push({ type: "text", value: part.text });
            }
          }
        } else if (isElement(node)) {
          if (NO_HIGHLIGHT_TAGS.has(node.tagName)) {
            result.push(node);
          } else {
            result.push({
              ...node,
              children: walk(node.children),
            });
          }
        } else {
          result.push(node);
        }
      }

      return result;
    }

    tree.children = walk(tree.children);
  };
}
