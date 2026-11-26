/**
 * Escape special regex characters in a string.
 */
function escapeRegExp(str: string): string {
  return str.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Literal label grammar shared by routing and rendered mention tokens.
 * A full-key qualification (and its numeric reservation suffix) belongs to the
 * label, never to a shorter prefix, even when that longer label is unbound.
 */
export function mentionLabelPattern(label: string): string {
  const qualification = `(?! \\([0-9a-f]{64}\\))`;
  const suffix = / \([0-9a-f]{64}\)$/i.test(label)
    ? `(?! (?:[2-9]|[1-9][0-9]+)(?=[\\s,;.!?:)\\]}*_]|$))`
    : "";
  return `${escapeRegExp(label)}${qualification}${suffix}`;
}

function maskRange(
  chars: string[],
  text: string,
  start: number,
  end: number,
): void {
  for (let index = start; index < end; index += 1) {
    if (text[index] !== "\n" && text[index] !== "\r") chars[index] = " ";
  }
}

/**
 * Replace Markdown code with spaces while retaining offsets and line endings.
 * Handles fenced blocks, four-space/tab-indented lines, and backtick code spans.
 *
 * Memoized on the last input: mention resolution asks about the same message
 * body once per candidate label, and the body is untrusted (F168).
 */
let lastMaskInput: string | null = null;
let lastMaskOutput = "";

function maskMarkdownCode(text: string): string {
  if (text !== lastMaskInput) {
    lastMaskOutput = computeMaskedMarkdownCode(text);
    lastMaskInput = text;
  }
  return lastMaskOutput;
}

function computeMaskedMarkdownCode(text: string): string {
  const chars = text.split("");
  const lines: Array<{ start: number; end: number; content: string }> = [];

  let lineStart = 0;
  while (lineStart < text.length) {
    let lineEnd = lineStart;
    while (
      lineEnd < text.length &&
      text[lineEnd] !== "\n" &&
      text[lineEnd] !== "\r"
    ) {
      lineEnd += 1;
    }
    lines.push({
      start: lineStart,
      end: lineEnd,
      content: text.slice(lineStart, lineEnd),
    });
    if (text[lineEnd] === "\r" && text[lineEnd + 1] === "\n") lineEnd += 1;
    lineStart = lineEnd + 1;
  }

  let fence: { marker: string; length: number } | null = null;
  for (const line of lines) {
    if (fence) {
      maskRange(chars, text, line.start, line.end);
      const closing = line.content.match(/^ {0,3}(`+|~+)[ \t]*$/);
      if (
        closing &&
        closing[1][0] === fence.marker &&
        closing[1].length >= fence.length
      ) {
        fence = null;
      }
      continue;
    }

    const opening = line.content.match(/^ {0,3}(`{3,}|~{3,})(.*)$/);
    if (opening && !(opening[1][0] === "`" && opening[2].includes("`"))) {
      fence = { marker: opening[1][0], length: opening[1].length };
      maskRange(chars, text, line.start, line.end);
      continue;
    }

    if (/^(?: {4}|\t)/.test(line.content)) {
      maskRange(chars, text, line.start, line.end);
    }
  }

  const isMasked = (index: number) =>
    chars[index] === " " && text[index] !== " ";
  const isEscaped = (index: number) => {
    let slashCount = 0;
    for (
      let cursor = index - 1;
      cursor >= 0 && text[cursor] === "\\";
      cursor -= 1
    ) {
      slashCount += 1;
    }
    return slashCount % 2 === 1;
  };

  // Index the unmasked backtick runs once. Code-span masking below only ever
  // touches text behind the scan cursor, so the runs ahead of it never change,
  // and each closer search becomes a pointer step instead of a rescan to the
  // end of the text. Rescanning made a body of unbalanced runs of growing
  // length quadratic (F168). The spans matched are exactly the ones the
  // rescanning loop found: the first later run of the opener's length.
  const runStartsByLength = new Map<number, number[]>();
  for (let index = 0; index < text.length; ) {
    if (text[index] !== "`" || isMasked(index)) {
      index += 1;
      continue;
    }
    let runEnd = index + 1;
    while (
      runEnd < text.length &&
      text[runEnd] === "`" &&
      !isMasked(runEnd)
    ) {
      runEnd += 1;
    }
    const length = runEnd - index;
    const starts = runStartsByLength.get(length);
    if (starts) starts.push(index);
    else runStartsByLength.set(length, [index]);
    index = runEnd;
  }
  const cursorByLength = new Map<number, number>();
  // `from` never decreases across calls: the scan cursor only moves forward.
  const findCloser = (length: number, from: number): number => {
    const starts = runStartsByLength.get(length);
    if (!starts) return -1;
    let cursor = cursorByLength.get(length) ?? 0;
    while (cursor < starts.length && starts[cursor] < from) cursor += 1;
    cursorByLength.set(length, cursor);
    return cursor < starts.length ? starts[cursor] : -1;
  };

  for (let index = 0; index < text.length; ) {
    if (text[index] !== "`" || isMasked(index) || isEscaped(index)) {
      index += 1;
      continue;
    }

    let openerEnd = index + 1;
    while (
      openerEnd < text.length &&
      text[openerEnd] === "`" &&
      !isMasked(openerEnd)
    ) {
      openerEnd += 1;
    }
    const delimiterLength = openerEnd - index;
    const closer = findCloser(delimiterLength, openerEnd);
    if (closer === -1) {
      index = openerEnd;
      continue;
    }
    const closerEnd = closer + delimiterLength;
    maskRange(chars, text, index, closerEnd);
    index = closerEnd;
  }

  return chars.join("");
}

/**
 * Check whether `text` contains an @mention of `name`.
 *
 * Matches `@Name` preceded by start-of-string, whitespace, an opening
 * parenthesis (for team expansions), markdown
 * bold/italic markers (`*`, `**`, `***`, `_`, `__`, `___`), or spoiler
 * delimiters (`||`). This handles the case where a mention is pasted from the
 * chat area and TipTap's Bold extension wraps it in bold marks (font-weight >=
 * 500 -> bold), plus messages whose visible mention text is spoilered.
 *
 * Exported separately so it can be unit-tested without importing React.
 */
export function getMentionOffsets(text: string, name: string): number[] {
  const escaped = mentionLabelPattern(name);
  const pattern = new RegExp(
    `(^|\\s|\\(|[*_]{1,3}|\\|\\|)(@${escaped})(?=\\|\\||[\\s,;.!?:)\\]}*_]|$)`,
    "gi",
  );
  const maskedText = maskMarkdownCode(text);
  const offsets: number[] = [];
  let match = pattern.exec(maskedText);
  while (match !== null) {
    offsets.push(match.index + match[1].length);
    match = pattern.exec(maskedText);
  }
  return offsets;
}

export function getMentionOffset(text: string, name: string): number | null {
  return getMentionOffsets(text, name)[0] ?? null;
}

export function hasMention(text: string, name: string): boolean {
  return getMentionOffset(text, name) !== null;
}
