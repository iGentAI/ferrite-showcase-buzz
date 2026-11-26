/**
 * The model picker's highlight decisions, kept pure so they can be tested.
 *
 * These three functions are the whole decision surface of the dropdown's
 * keyboard behaviour: what the visible list CONTAINS, where the highlight
 * belongs when that list changes, and where an arrow key moves it.
 *
 * `AgentDefinitionDialog` builds its filtered-options array inline, so its
 * reference changes on every parent render, and model discovery re-renders the
 * dialog asynchronously. A highlight reset keyed on identity would put the
 * highlight back on the first row under the user, and Enter would select the
 * wrong model. So the reset is keyed on CONTENT, through [`optionsSignature`].
 */

/** The minimal shape the highlight logic needs from a dropdown option. */
export type HighlightableOption = {
  readonly disabled?: boolean;
  readonly value: string;
};

/**
 * A stable string describing what the list contains, for use as an effect key.
 *
 * Two arrays with equal content produce an equal signature even when they are
 * different objects, which is the entire point: a parent re-render that hands
 * the combobox an equal-but-new array must not disturb the user's highlight.
 * The `disabled` flag is part of the signature because it changes which rows
 * are selectable.
 *
 * JSON tuples rather than a joined string, because the encoding has to be
 * unambiguous: joining values with a separator lets a value that CONTAINS that
 * separator imitate two options, so a genuine change would produce an identical
 * signature and go unnoticed. JSON escapes delimiters inside string values, so
 * no value can forge the structure around it.
 */
export function optionsSignature(
  options: readonly HighlightableOption[],
): string {
  return JSON.stringify(
    options.map((option) => [option.value, Boolean(option.disabled)]),
  );
}

/**
 * Where the highlight belongs for a given list: the first row a user may pick.
 *
 * Falls back to 0 for an empty list, or one where every row is disabled, so the
 * highlight is always a valid index.
 */
export function firstEnabledIndex(
  options: readonly HighlightableOption[],
): number {
  const found = options.findIndex((option) => !option.disabled);
  return found === -1 ? 0 : found;
}

/**
 * Walk from `from` in `direction` (+1 or -1), wrapping once, and return the
 * first index that is not disabled.
 *
 * Returns `from` unchanged when nothing else is selectable, so the highlight
 * never vanishes or lands somewhere the user cannot choose.
 */
export function nextEnabledIndex(
  options: readonly HighlightableOption[],
  from: number,
  direction: 1 | -1,
): number {
  const len = options.length;
  if (len === 0) return 0;
  for (let step = 1; step <= len; step++) {
    // The ordinary non-negative modulo: `%` keeps the sign of its left operand
    // in JavaScript, so a backwards walk needs the `+ len` before the second `%`.
    const candidate = (((from + direction * step) % len) + len) % len;
    if (!options[candidate]?.disabled) return candidate;
  }
  return from;
}
