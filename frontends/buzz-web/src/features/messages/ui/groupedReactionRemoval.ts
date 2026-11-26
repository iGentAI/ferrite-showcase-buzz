type ReactionSource = {
  id: string;
  reactions?: ReadonlyArray<{ emoji: string; reactedByCurrentUser?: boolean }>;
};

/**
 * The messages a grouped system row removes `emoji` from: every source the
 * current user has reacted to with it, plus the row's own `message` when an
 * add made through the row has not been echoed into `sources` yet. Without
 * that, undoing a fresh reaction sent nothing and the echo brought it back.
 */
export function groupedReactionRemovalTargets<T extends ReactionSource>(
  sources: readonly T[],
  message: T,
  emoji: string,
  hasUnechoedAdd: boolean,
): T[] {
  const reacted = sources.filter((source) =>
    source.reactions?.some(
      (reaction) => reaction.emoji === emoji && reaction.reactedByCurrentUser,
    ),
  );
  if (hasUnechoedAdd && !reacted.some((source) => source.id === message.id)) {
    return [...reacted, message];
  }
  return reacted;
}
