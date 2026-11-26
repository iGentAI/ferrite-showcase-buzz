import type { BlobDescriptor } from "@/shared/api/tauri";

/**
 * Slot bookkeeping for composer attachments.
 *
 * Attachments live in a sparse array: an immediate upload calls `reserveSlots`
 * to claim an index up front and fills it by that index when it completes, so
 * concurrent uploads publish in the order they were attached. A `null` is a
 * placeholder for an upload still in flight.
 *
 * Consumers of the composer only ever see the compacted list of real
 * attachments, so any update expressed against that view has to be mapped back
 * onto the slot layout — never applied to it directly.
 */

/**
 * Identity of a descriptor. `url` alone can repeat across re-uploads of
 * identical bytes, so pair it with the digest.
 */
function descriptorKey(descriptor: BlobDescriptor): string {
  return `${descriptor.url}\u0000${descriptor.sha256 ?? ""}`;
}

/** The real attachments, in order, with in-flight placeholders dropped. */
export function compactImetaSlots(
  slots: (BlobDescriptor | null)[],
): BlobDescriptor[] {
  return slots.filter((d): d is BlobDescriptor => d !== null);
}

/**
 * Apply an updater written against the compacted list back onto `slots`.
 *
 * Replacing the array with the updater's result would renumber it while an
 * in-flight upload still holds an index from `reserveSlots`, so that upload's
 * `fillSlot` would overwrite an unrelated attachment. Instead:
 *
 * - survivors stay at the index they already occupy;
 * - removals become `null` rather than shifting their neighbours;
 * - genuinely new descriptors append after the reserved tail, where no pending
 *   `fillSlot` can reach them.
 *
 * Reconciliation counts occurrences rather than testing key membership, so a
 * descriptor the updater deliberately adds a second time (the same GIF picked
 * twice) appends a new slot, and removing one of two identical attachments
 * nulls exactly one slot (F384). When an updater drops copies of a key, the
 * earliest-placed copies are the ones kept.
 *
 * An updater that returns its input unchanged (e.g. the snapshot-paste dedupe)
 * leaves `slots` exactly as it was, identity included.
 */
export function applyImetaUpdate(
  slots: (BlobDescriptor | null)[],
  update: (current: BlobDescriptor[]) => BlobDescriptor[],
): (BlobDescriptor | null)[] {
  const current = compactImetaSlots(slots);
  const next = update(current);
  if (next === current) return slots;

  const wantedCounts = countKeys(next);
  const keptCounts = new Map<string, number>();
  const preserved = slots.map((descriptor) => {
    if (descriptor === null) return null;
    const key = descriptorKey(descriptor);
    const kept = keptCounts.get(key) ?? 0;
    if (kept >= (wantedCounts.get(key) ?? 0)) return null;
    keptCounts.set(key, kept + 1);
    return descriptor;
  });
  const presentCounts = countKeys(current);
  const seenCounts = new Map<string, number>();
  const appended = next.filter((descriptor) => {
    const key = descriptorKey(descriptor);
    const seen = seenCounts.get(key) ?? 0;
    seenCounts.set(key, seen + 1);
    return seen >= (presentCounts.get(key) ?? 0);
  });
  return appended.length > 0 ? [...preserved, ...appended] : preserved;
}

function countKeys(descriptors: BlobDescriptor[]): Map<string, number> {
  const counts = new Map<string, number>();
  for (const descriptor of descriptors) {
    const key = descriptorKey(descriptor);
    counts.set(key, (counts.get(key) ?? 0) + 1);
  }
  return counts;
}

/**
 * Replace the first slot holding `url` with `replacement` (`null` removes it).
 *
 * Blossom URLs are content-addressed, so attaching the same bytes twice gives
 * two slots with one URL. The composer's remove, annotate and revert actions
 * name an attachment by URL, and each of them means ONE thumbnail; mapping
 * over every matching slot removed, replaced or reverted all the copies at
 * once (F391). Returns `slots` itself when no slot holds `url`.
 */
export function replaceFirstImetaSlotByUrl(
  slots: (BlobDescriptor | null)[],
  url: string,
  replacement: BlobDescriptor | null,
): (BlobDescriptor | null)[] {
  const index = slots.findIndex((descriptor) => descriptor?.url === url);
  if (index === -1) return slots;
  const next = [...slots];
  next[index] = replacement;
  return next;
}
