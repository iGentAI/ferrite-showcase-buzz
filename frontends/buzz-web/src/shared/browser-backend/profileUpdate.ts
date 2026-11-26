export type ProfileUpdate = {
  displayName?: string | null;
  avatarUrl?: string | null;
  about?: string | null;
};

/**
 * Merge the desktop command's partial profile patch into a relay kind-0.
 *
 * Kind 0 is replaceable as a whole. Omitting an unchanged field from the next
 * event therefore means "preserve it", not "erase it". Keep unknown metadata
 * too: clients commonly carry extra interoperable fields that Buzz does not
 * edit itself.
 */
export function mergeProfileUpdate(
  current: Record<string, unknown>,
  patch: ProfileUpdate,
): Record<string, unknown> {
  const next = { ...current };

  if (patch.displayName !== undefined) {
    const displayName = patch.displayName ?? "";
    next.name = displayName;
    next.display_name = displayName;
  }
  if (patch.avatarUrl !== undefined) {
    next.picture = patch.avatarUrl ?? "";
  }
  if (patch.about !== undefined) {
    next.about = patch.about ?? "";
  }

  return next;
}
