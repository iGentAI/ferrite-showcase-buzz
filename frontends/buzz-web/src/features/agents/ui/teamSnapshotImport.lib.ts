import type {
  TeamSnapshotImportMemberResult,
  TeamSnapshotImportResult,
  TeamSnapshotMemberPreview,
} from "@/shared/api/tauriTeams";

// ── Import phase derivation ──────────────────────────────────────────────────

export type ImportPhase = "preview" | "confirming" | "result";

export function deriveImportPhase(
  result: TeamSnapshotImportResult | null,
  isConfirming: boolean,
): ImportPhase {
  return result !== null ? "result" : isConfirming ? "confirming" : "preview";
}

// ── Profile-sync failure aggregation ─────────────────────────────────────────

export function getProfileSyncFailures(
  members: TeamSnapshotImportMemberResult[],
): TeamSnapshotImportMemberResult[] {
  return members.filter((m) => m.profileSyncError !== null);
}

// ── Pre-confirm memory disclosure ────────────────────────────────────────────

/**
 * What the preview can say about memory before the user confirms. Importing
 * writes each member's memory entries under the new agent's identity, and a
 * snapshot's memory is plaintext anyone could have written, so the user is
 * told before Import, not only in the result.
 *
 * - `counted`: every member reported a count and at least one is non-zero.
 * - `none`: every member reported zero.
 * - `unreported`: at least one member has no count (a backend that predates
 *   the field), so memory may be included.
 */
export type TeamPreviewMemoryDisclosure =
  | { kind: "counted"; total: number }
  | { kind: "none" }
  | { kind: "unreported" };

export function deriveTeamPreviewMemoryDisclosure(
  members: readonly TeamSnapshotMemberPreview[],
): TeamPreviewMemoryDisclosure {
  let total = 0;
  for (const member of members) {
    const count = member.memoryEntryCount;
    if (typeof count !== "number" || !Number.isFinite(count) || count < 0) {
      return { kind: "unreported" };
    }
    total += count;
  }
  return total > 0 ? { kind: "counted", total } : { kind: "none" };
}

// ── Toast message derivation ─────────────────────────────────────────────────

export type ToastOutcome = {
  type: "notice" | "error";
  message: string;
};

export function deriveImportToast(
  result: TeamSnapshotImportResult,
): ToastOutcome {
  const memberCount = result.members.length;
  const totalMemoryErrors = result.members.reduce(
    (sum, m) => sum + m.memoryErrors.length,
    0,
  );
  const profileSyncFailureCount = getProfileSyncFailures(result.members).length;

  if (totalMemoryErrors > 0 || profileSyncFailureCount > 0) {
    const parts: string[] = [];
    if (totalMemoryErrors > 0) {
      parts.push(
        `${totalMemoryErrors} memory entr${totalMemoryErrors === 1 ? "y" : "ies"} failed to restore`,
      );
    }
    if (profileSyncFailureCount > 0) {
      parts.push(
        `${profileSyncFailureCount} member${profileSyncFailureCount === 1 ? "" : "s"} failed to sync profile${profileSyncFailureCount === 1 ? "" : "s"}`,
      );
    }
    return {
      type: "error",
      message: `${result.team.name} imported, but ${parts.join(" and ")}.`,
    };
  }
  return {
    type: "notice",
    message: `Imported ${result.team.name} with ${memberCount} member${memberCount === 1 ? "" : "s"}.`,
  };
}
