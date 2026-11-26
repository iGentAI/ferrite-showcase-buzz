import { Extension, type KeyboardShortcutCommand } from "@tiptap/core";
import type { Node as ProseMirrorNode, ResolvedPos } from "@tiptap/pm/model";
import { Selection } from "@tiptap/pm/state";

import { isMacPlatform } from "@/shared/lib/platform";

/**
 * Bounds of the hard-break-delimited "line" containing `$from`.
 *
 * Chat composers use hard breaks for continuation lines, so a ProseMirror
 * block spans several visual lines. Emacs-style movement has to respect the
 * visual line, not the block.
 */
export function hardBreakLineBounds($from: ResolvedPos) {
  const parentStart = $from.start();
  let start = parentStart;
  let end = parentStart + $from.parent.content.size;

  const consider = (breakPosition: number, breakSize: number) => {
    if (breakPosition < $from.pos) {
      start = Math.max(start, breakPosition + breakSize);
    } else if (end > breakPosition) {
      end = breakPosition;
    }
  };

  $from.parent.forEach((node, offset) => {
    const nodeStart = parentStart + offset;
    if (node.type.name === "hardBreak") {
      consider(nodeStart, node.nodeSize);
      return;
    }
    // A code block keeps its lines as literal "\n" characters in its text.
    if (node.isText && node.text) {
      let index = node.text.indexOf("\n");
      while (index !== -1) {
        consider(nodeStart + index, 1);
        index = node.text.indexOf("\n", index + 1);
      }
    }
  });

  return { end, start };
}

function isHighSurrogate(code: number) {
  return code >= 0xd800 && code <= 0xdbff;
}

function isLowSurrogate(code: number) {
  return code >= 0xdc00 && code <= 0xdfff;
}

/**
 * The caret position one character before (`dir` -1) or after (`dir` 1)
 * `pos`, or null at the edge of the document. Inside a textblock it steps a
 * whole code point (never into a surrogate pair) or a whole inline node; at
 * a textblock's edge it moves into the neighbouring textblock, instead of
 * landing on the block boundary and snapping back.
 */
export function emacsCharacterStep(
  doc: ProseMirrorNode,
  pos: number,
  dir: -1 | 1,
): number | null {
  const $pos = doc.resolve(pos);
  if ($pos.parent.inlineContent) {
    const neighbour = dir < 0 ? $pos.nodeBefore : $pos.nodeAfter;
    if (neighbour) {
      if (!neighbour.isText || !neighbour.text) return pos + dir * neighbour.nodeSize;
      const text = neighbour.text;
      if (dir < 0) {
        const last = text.charCodeAt(text.length - 1);
        const prev = text.length > 1 ? text.charCodeAt(text.length - 2) : 0;
        return pos - (isLowSurrogate(last) && isHighSurrogate(prev) ? 2 : 1);
      }
      const firstCode = text.charCodeAt(0);
      const second = text.length > 1 ? text.charCodeAt(1) : 0;
      return pos + (isHighSurrogate(firstCode) && isLowSurrogate(second) ? 2 : 1);
    }
    if ($pos.depth === 0) return null;
    const outside = dir < 0 ? $pos.before() : $pos.after();
    const next = Selection.findFrom(doc.resolve(outside), dir, true);
    return next ? next.from : null;
  }
  const next = Selection.findFrom($pos, dir, true);
  return next && next.from !== pos ? next.from : null;
}

/**
 * macOS text fields traditionally support a small set of Emacs-style Control
 * shortcuts. Keep movement and kill-line scoped to the current
 * hard-break-delimited line rather than the whole ProseMirror block.
 */
export const MacEmacsTextShortcuts = Extension.create({
  name: "macEmacsTextShortcuts",
  addKeyboardShortcuts() {
    const shortcuts: Record<string, KeyboardShortcutCommand> = {};
    if (!isMacPlatform()) {
      return shortcuts;
    }

    return {
      "Ctrl-a": ({ editor: ed }) => {
        const { $from } = ed.state.selection;
        if (!$from.parent.inlineContent) return false;
        return ed.commands.setTextSelection(hardBreakLineBounds($from).start);
      },
      "Ctrl-e": ({ editor: ed }) => {
        const { $from } = ed.state.selection;
        if (!$from.parent.inlineContent) return false;
        return ed.commands.setTextSelection(hardBreakLineBounds($from).end);
      },
      "Ctrl-b": ({ editor: ed }) => {
        const { empty, from } = ed.state.selection;
        if (!empty) return false;
        const target = emacsCharacterStep(ed.state.doc, from, -1);
        if (target === null) return false;
        return ed.commands.setTextSelection(target);
      },
      "Ctrl-f": ({ editor: ed }) => {
        const { empty, from } = ed.state.selection;
        if (!empty) return false;
        const target = emacsCharacterStep(ed.state.doc, from, 1);
        if (target === null) return false;
        return ed.commands.setTextSelection(target);
      },
      "Ctrl-k": ({ editor: ed }) => {
        const { state, view } = ed;
        const { $from, empty, from, to } = state.selection;

        if (!empty) {
          return ed.commands.deleteSelection();
        }

        if ($from.parent.inlineContent) {
          const lineEnd = hardBreakLineBounds($from).end;
          if (from < lineEnd) {
            return ed.commands.deleteRange({ from, to: lineEnd });
          }

          const nodeAfter = $from.nodeAfter;
          if (nodeAfter?.type.name === "hardBreak") {
            return ed.commands.deleteRange({
              from,
              to: from + nodeAfter.nodeSize,
            });
          }
          // At the end of a code-block line, kill the newline itself.
          if (nodeAfter?.isText && nodeAfter.text?.startsWith("\n")) {
            return ed.commands.deleteRange({ from, to: from + 1 });
          }
        }

        const blockEnd = $from.end();
        if (from < blockEnd) {
          return ed.commands.deleteRange({ from, to: blockEnd });
        }

        const nextSelection = Selection.findFrom(
          state.doc.resolve(to),
          1,
          true,
        );
        if (!nextSelection) return false;

        const transaction = state.tr.delete(to, nextSelection.from);
        view.dispatch(transaction.scrollIntoView());
        return true;
      },
    };
  },
});
