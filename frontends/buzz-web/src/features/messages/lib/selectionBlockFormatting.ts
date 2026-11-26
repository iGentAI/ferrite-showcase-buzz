import type { Node as ProseMirrorNode } from "@tiptap/pm/model";
import { TextSelection, type Transaction } from "@tiptap/pm/state";
import { canSplit, liftTarget } from "@tiptap/pm/transform";

function canSplitInsideTextblock(
  transaction: Transaction,
  position: number,
): boolean {
  const $position = transaction.doc.resolve(position);
  return (
    $position.parent.inlineContent &&
    $position.parentOffset > 0 &&
    $position.parentOffset < $position.parent.content.size &&
    canSplit(transaction.doc, position)
  );
}

function mapRangeThroughLatestStep(
  transaction: Transaction,
  from: number,
  to: number,
  { fromAssoc = 1, toAssoc = -1 }: { fromAssoc?: -1 | 1; toAssoc?: -1 | 1 } = {},
): { from: number; to: number } {
  const stepMap = transaction.steps.at(-1)?.getMap();
  return stepMap
    ? {
        from: stepMap.map(from, fromAssoc),
        to: stepMap.map(to, toAssoc),
      }
    : { from, to };
}

/**
 * Isolate the hard-break-delimited line under a collapsed caret.
 *
 * The composer represents Shift+Enter lines as `hardBreak` nodes inside one
 * paragraph, so a block toggle at a collapsed caret otherwise reformats every
 * line of the draft. Replacing the line's bordering hard breaks with block
 * splits gives the caret's line its own textblock, which scopes the following
 * block toggle to just that line.
 */
function isolateCaretLineForBlockFormatting(transaction: Transaction): boolean {
  const { $from } = transaction.selection;
  if (!$from.parent.isTextblock || !$from.parent.inlineContent) return false;

  const blockStart = $from.start();
  const blockEnd = $from.end();
  let caret = transaction.selection.from;

  let lineFrom = blockStart;
  let lineTo = blockEnd;
  $from.parent.forEach((child, offset) => {
    if (child.type.name !== "hardBreak") return;
    const breakFrom = blockStart + offset;
    const breakTo = breakFrom + child.nodeSize;
    if (breakTo <= caret) lineFrom = breakTo;
    if (breakFrom >= caret) lineTo = Math.min(lineTo, breakFrom);
  });

  // No hard breaks around the caret — the line already is the whole
  // textblock, so the block toggle is correctly scoped as-is.
  if (lineFrom === blockStart && lineTo === blockEnd) return false;

  const nodeAfterLine = transaction.doc.resolve(lineTo).nodeAfter;
  if (nodeAfterLine?.type.name === "hardBreak") {
    transaction.delete(lineTo, lineTo + nodeAfterLine.nodeSize);
    if (canSplit(transaction.doc, lineTo)) {
      transaction.split(lineTo);
      const stepMap = transaction.steps.at(-1)?.getMap();
      if (stepMap) {
        caret = stepMap.map(caret, -1);
        lineFrom = stepMap.map(lineFrom, -1);
      }
    }
  }

  const nodeBeforeLine = transaction.doc.resolve(lineFrom).nodeBefore;
  if (nodeBeforeLine?.type.name === "hardBreak") {
    transaction.delete(lineFrom - nodeBeforeLine.nodeSize, lineFrom);
    let stepMap = transaction.steps.at(-1)?.getMap();
    if (stepMap) {
      caret = stepMap.map(caret, 1);
      lineFrom = stepMap.map(lineFrom, -1);
    }
    if (canSplit(transaction.doc, lineFrom)) {
      transaction.split(lineFrom);
      stepMap = transaction.steps.at(-1)?.getMap();
      if (stepMap) caret = stepMap.map(caret, 1);
    }
  }

  transaction.setSelection(TextSelection.create(transaction.doc, caret));
  return true;
}

function listItemTextRange(
  $position: Transaction["selection"]["$from"],
): { from: number; to: number } | null {
  let itemDepth = -1;
  for (let depth = $position.depth; depth > 0; depth -= 1) {
    if ($position.node(depth).type.name === "listItem") {
      itemDepth = depth;
      break;
    }
  }
  if (itemDepth < 0) return null;

  const item = $position.node(itemDepth);
  const itemPosition = $position.before(itemDepth);
  let from: number | null = null;
  let to: number | null = null;
  item.descendants((node, relativePosition) => {
    // A nested list belongs to its own items, not to this one.
    if (
      node.type.name === "bulletList" ||
      node.type.name === "orderedList" ||
      node.type.name === "listItem"
    ) {
      return false;
    }
    if (!node.isTextblock) return true;
    const position = itemPosition + 1 + relativePosition;
    from ??= position + 1;
    to = position + node.nodeSize - 1;
    return false;
  });
  return from === null || to === null ? null : { from, to };
}

/** Expand partial list endpoint selections to whole list-item textblocks. */
function expandSelectionToListItems(transaction: Transaction): boolean {
  const selection = transaction.selection;
  if (!(selection instanceof TextSelection) || selection.empty) return false;

  const startItem = listItemTextRange(selection.$from);
  const endItem = listItemTextRange(selection.$to);
  if (!(startItem || endItem)) return false;

  const isBackward = selection.anchor > selection.head;
  const from = startItem?.from ?? selection.from;
  const to = endItem?.to ?? selection.to;
  transaction.setSelection(
    TextSelection.create(
      transaction.doc,
      isBackward ? to : from,
      isBackward ? from : to,
    ),
  );
  return true;
}

export function selectionIncludesList(transaction: Transaction): boolean {
  const { from, to } = transaction.selection;
  let includesList = false;
  transaction.doc.nodesBetween(from, to, (node) => {
    if (node.type.name === "listItem") {
      includesList = true;
      return false;
    }
    return !includesList;
  });
  return includesList;
}

function normalizeSelectionBlockBoundaries(transaction: Transaction): boolean {
  const selection = transaction.selection;
  if (!(selection instanceof TextSelection) || selection.empty) return false;

  const isBackward = selection.anchor > selection.head;
  let { from, to } = selection;
  if (
    selection.$from.parent.isTextblock &&
    selection.$from.parentOffset === selection.$from.parent.content.size &&
    selection.$from.depth > 0
  ) {
    from = selection.$from.after();
  }
  if (
    selection.$to.parent.isTextblock &&
    selection.$to.parentOffset === 0 &&
    selection.$to.depth > 0
  ) {
    to = selection.$to.before();
  }
  if (from >= to) return false;

  transaction.setSelection(
    TextSelection.create(
      transaction.doc,
      isBackward ? to : from,
      isBackward ? from : to,
    ),
  );
  return from !== selection.from || to !== selection.to;
}

/**
 * Isolate the current text selection at exact block boundaries.
 *
 * ProseMirror's block commands operate on whole textblocks. The composer can
 * hold an entire draft in one paragraph, so toggling a list or code block for
 * a substring otherwise formats the whole draft. Splitting at the selection
 * end and start first gives the selected text its own block while preserving
 * the surrounding content as sibling paragraphs. A collapsed caret isolates
 * its hard-break-delimited line so the block format starts at that line.
 *
 * This mutates the transaction supplied by a Tiptap command chain so the
 * isolation and the following block toggle remain one undoable edit.
 */
export function isolateSelectionForBlockFormatting(
  transaction: Transaction,
): boolean {
  if (!(transaction.selection instanceof TextSelection)) {
    return false;
  }

  if (transaction.selection.empty) {
    return isolateCaretLineForBlockFormatting(transaction);
  }

  expandSelectionToListItems(transaction);
  normalizeSelectionBlockBoundaries(transaction);
  const isBackward = transaction.selection.anchor > transaction.selection.head;
  let { from, to } = transaction.selection;

  const nodeAfterSelection = transaction.doc.resolve(to).nodeAfter;
  if (nodeAfterSelection?.type.name === "hardBreak") {
    transaction.delete(to, to + nodeAfterSelection.nodeSize);
    ({ from, to } = mapRangeThroughLatestStep(transaction, from, to));
  }

  const nodeBeforeSelection = transaction.doc.resolve(from).nodeBefore;
  if (nodeBeforeSelection?.type.name === "hardBreak") {
    transaction.delete(from - nodeBeforeSelection.nodeSize, from);
    ({ from, to } = mapRangeThroughLatestStep(transaction, from, to));
  }

  if (canSplitInsideTextblock(transaction, to)) {
    transaction.split(to);
    ({ from, to } = mapRangeThroughLatestStep(transaction, from, to));
  }

  if (canSplitInsideTextblock(transaction, from)) {
    transaction.split(from);
    ({ from, to } = mapRangeThroughLatestStep(transaction, from, to));
  }

  transaction.setSelection(
    TextSelection.create(
      transaction.doc,
      isBackward ? to : from,
      isBackward ? from : to,
    ),
  );
  return true;
}

/** Split each selected hard-break line into a textblock before list wrapping. */
export function splitSelectedLinesForListFormatting(
  transaction: Transaction,
): boolean {
  if (!(transaction.selection instanceof TextSelection)) return false;
  if (transaction.selection.empty) {
    return isolateCaretLineForBlockFormatting(transaction);
  }

  const isBackward = transaction.selection.anchor > transaction.selection.head;
  isolateSelectionForBlockFormatting(transaction);
  let { from, to } = transaction.selection;
  const breakPositions: number[] = [];

  transaction.doc.nodesBetween(from, to, (node, position) => {
    if (node.type.name === "hardBreak") breakPositions.push(position);
  });

  for (const position of breakPositions.reverse()) {
    transaction.delete(position, position + 1);
    ({ from, to } = mapRangeThroughLatestStep(transaction, from, to));
    if (!canSplit(transaction.doc, position)) continue;
    transaction.split(position);
    // Across a split the selection widens: an empty first or last line that
    // the split makes into its own paragraph stays selected.
    ({ from, to } = mapRangeThroughLatestStep(transaction, from, to, {
      fromAssoc: -1,
      toAssoc: 1,
    }));
  }

  transaction.setSelection(
    TextSelection.create(
      transaction.doc,
      isBackward ? to : from,
      isBackward ? from : to,
    ),
  );
  return true;
}

function selectedTextblocks(
  transaction: Transaction,
): Array<{ node: ProseMirrorNode; position: number }> {
  const blocks: Array<{ node: ProseMirrorNode; position: number }> = [];
  const { from, to } = transaction.selection;
  transaction.doc.nodesBetween(from, to, (node, position) => {
    if (node.isTextblock) {
      blocks.push({ node, position });
      return false;
    }
    return true;
  });
  return blocks;
}

function leafTextForCode(leaf: ProseMirrorNode): string {
  if (leaf.type.name === "hardBreak") return "\n";

  const schemaText = leaf.type.spec.leafText?.(leaf);
  if (schemaText !== undefined) return schemaText;

  // Inline atoms should survive conversion whenever they expose a meaningful
  // textual identity. Unknown leaves intentionally fall back to an empty
  // string rather than leaking implementation attributes into user content.
  const attrs = leaf.attrs as Record<string, unknown>;
  if (typeof attrs.label === "string") return attrs.label;
  if (typeof attrs.shortcode === "string") return `:${attrs.shortcode}:`;
  return "";
}

function textblockTextForCode(node: ProseMirrorNode): string {
  return node.textBetween(0, node.content.size, "\n", leafTextForCode);
}

/** Replace selected textblocks with one newline-joined code block. */
export function mergeSelectedTextblocksIntoCodeBlock(
  transaction: Transaction,
): boolean {
  if (!(transaction.selection instanceof TextSelection)) return false;
  if (transaction.selection.empty) return false;

  const codeBlock = transaction.doc.type.schema.nodes.codeBlock;
  if (!codeBlock) return false;
  // Blocks under different wrappers (a paragraph beside a blockquote, a list
  // item beside its parent's text) are lifted to one parent first, so the
  // replacement neither drags a wrapper along nor leaves an empty one behind.
  liftSelectedTextblocksToOneParent(transaction);
  const blocks = selectedTextblocks(transaction);
  const first = blocks[0];
  const last = blocks.at(-1);
  if (!(first && last)) return false;

  const text = blocks.map(({ node }) => textblockTextForCode(node)).join("\n");
  const from = first.position;
  const to = last.position + last.node.nodeSize;
  const content = text ? transaction.doc.type.schema.text(text) : undefined;
  transaction.replaceWith(from, to, codeBlock.create(null, content));
  // The schema may have wrapped or padded the code block (a list item must
  // start with a paragraph), so select inside where it actually landed.
  const stepMap = transaction.steps.at(-1)?.getMap();
  const start = stepMap ? stepMap.map(from, -1) : from;
  const end = stepMap ? stepMap.map(to, 1) : to;
  let inserted: number | null = null;
  transaction.doc.nodesBetween(start, end, (node, position) => {
    if (inserted !== null) return false;
    if (node.type === codeBlock && node.textContent === text) {
      inserted = position;
      return false;
    }
    return true;
  });
  const codeStart = (inserted ?? from) + 1;
  transaction.setSelection(
    TextSelection.create(transaction.doc, codeStart, codeStart + text.length),
  );
  return true;
}

function liftSelectedTextblocksToOneParent(transaction: Transaction): void {
  for (let guard = 0; guard < 32; guard += 1) {
    const blocks = selectedTextblocks(transaction);
    const first = blocks[0];
    const last = blocks.at(-1);
    if (!(first && last)) return;
    const end = last.position + last.node.nodeSize;
    const $first = transaction.doc.resolve(first.position);
    const $last = transaction.doc.resolve(end);
    if ($first.sameParent($last)) return;
    const shared = $first.sharedDepth(end);
    const deeper = $first.depth > shared ? first : last;
    const range = transaction.doc.resolve(deeper.position + 1).blockRange();
    const target = range ? liftTarget(range) : null;
    if (!range || target === null) return;
    transaction.lift(range, target);
  }
}
