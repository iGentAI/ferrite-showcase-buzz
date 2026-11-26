import {
  stripOrphanedAssignments,
  type ChannelSection,
  type ChannelSectionStore,
} from "./channelSectionsStorage";

type Present<V> = { value: V } | null;

function present<V>(record: Record<string, V>, key: string): Present<V> {
  return Object.prototype.hasOwnProperty.call(record, key)
    ? { value: record[key] as V }
    : null;
}

function samePresence<V>(
  left: Present<V>,
  right: Present<V>,
  equal: (left: V, right: V) => boolean,
): boolean {
  if (!left || !right) return left === right;
  return equal(left.value, right.value);
}

/**
 * Three-way merge of two edits of one keyed record against their common base.
 *
 * A key the local side changed (added, edited or deleted relative to `base`)
 * keeps the local value; every other key takes the remote value. When both
 * sides changed the same key, the local edit wins, because it is the one the
 * user just made on this device.
 */
export function mergeRecordsThreeWay<V>(
  base: Record<string, V>,
  local: Record<string, V>,
  remote: Record<string, V>,
  equal: (left: V, right: V) => boolean,
): Record<string, V> {
  const keys = new Set([
    ...Object.keys(local),
    ...Object.keys(remote),
    ...Object.keys(base),
  ]);
  const merged: Array<[string, V]> = [];
  for (const key of keys) {
    const localEntry = present(local, key);
    const chosen = samePresence(localEntry, present(base, key), equal)
      ? present(remote, key)
      : localEntry;
    if (chosen) merged.push([key, chosen.value]);
  }
  return Object.fromEntries(merged);
}

function sameSection(left: ChannelSection, right: ChannelSection): boolean {
  return (
    left.id === right.id &&
    left.name === right.name &&
    left.icon === right.icon &&
    left.order === right.order
  );
}

function sectionsById(
  store: ChannelSectionStore,
): Record<string, ChannelSection> {
  return Object.fromEntries(
    store.sections.map((section) => [section.id, section]),
  );
}

/**
 * Merge a local sections edit with a newer remote blob from another device.
 *
 * `base` is the last blob this device knew to be on the relay, which is what
 * the local edit was made on top of. Sections and channel assignments merge
 * per entry, so a section created, renamed or deleted here survives a
 * concurrent write elsewhere, and the other device's unrelated changes are
 * kept too. Assignments that point at a section neither side kept are
 * dropped.
 */
export function mergeChannelSectionStores(
  base: ChannelSectionStore,
  local: ChannelSectionStore,
  remote: ChannelSectionStore,
): ChannelSectionStore {
  const sections = Object.values(
    mergeRecordsThreeWay(
      sectionsById(base),
      sectionsById(local),
      sectionsById(remote),
      sameSection,
    ),
  )
    .sort(
      (left, right) =>
        left.order - right.order ||
        (left.id < right.id ? -1 : left.id > right.id ? 1 : 0),
    )
    // Two devices can give two sections the same order; renumbering keeps the
    // merged order and makes move up/down swap distinct values.
    .map((section, index) =>
      section.order === index ? section : { ...section, order: index },
    );
  const assignments = mergeRecordsThreeWay(
    base.assignments,
    local.assignments,
    remote.assignments,
    (left, right) => left === right,
  );
  return stripOrphanedAssignments({ version: 1, sections, assignments });
}

/**
 * What to show and keep when a remote section blob arrives while local edits
 * are still waiting to publish: the local edits merged onto the remote against
 * the store they were made on, and whether the merge still needs publishing
 * (finding_412874c055f620ab). Without pending edits the remote wins whole.
 */
export function reconcileRemoteSections(
  remote: ChannelSectionStore,
  pending: ChannelSectionStore | null,
  pendingBase: ChannelSectionStore | null,
): { store: ChannelSectionStore; republish: boolean } {
  if (!pending || !pendingBase) return { store: remote, republish: false };
  const store = mergeChannelSectionStores(pendingBase, pending, remote);
  return { store, republish: JSON.stringify(store) !== JSON.stringify(remote) };
}
