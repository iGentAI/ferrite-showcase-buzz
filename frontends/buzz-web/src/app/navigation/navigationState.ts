type NavigationState = Record<string, unknown>;

export type NavigationStateUpdate =
  | NavigationState
  | ((previousState: NavigationState) => NavigationState)
  | undefined;

/**
 * Whether applying `update` to the current history state changes anything.
 *
 * A same-href navigation is skipped unless it changes state. Callers attach a
 * state updater on ordinary navigations too (clearing a search highlight that
 * is already clear, marking an editor origin already marked), so the presence
 * of an updater alone must not count as a change, or re-selecting the current
 * channel pushes a duplicate history entry. `null` and `undefined` count as
 * the same value: clearing a key that was never set changes nothing.
 */
export function navigationStateChanges(
  update: NavigationStateUpdate,
  currentState: unknown,
): boolean {
  if (update === undefined) return false;
  const previous: NavigationState =
    currentState && typeof currentState === "object"
      ? (currentState as NavigationState)
      : {};
  const next = typeof update === "function" ? update({ ...previous }) : update;
  for (const key of Object.keys(next)) {
    const before = previous[key] ?? null;
    const after = next[key] ?? null;
    if (!Object.is(before, after)) return true;
  }
  return false;
}
