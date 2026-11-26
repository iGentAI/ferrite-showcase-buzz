import type { UserProfileSummary } from "@/shared/api/types";
import { parsePubkeyInput } from "@/shared/lib/nostrUtils";

const HEX_PUBKEY = /^[0-9a-f]{64}$/;

export type WorkflowAuthorCandidate = {
  pubkey: string;
  displayName: string | null;
  avatarUrl: string | null;
  nip05Handle: string | null;
  ownerPubkey: string | null;
  isAgent: boolean;
};

export type WorkflowAuthorCandidateInput = {
  pubkey: string;
  displayName?: string | null;
  avatarUrl?: string | null;
  nip05Handle?: string | null;
  ownerPubkey?: string | null;
  isAgent?: boolean;
};

/** Normalize a candidate identity without accepting alternate encodings. */
export function normalizeAuthorPubkey(pubkey: string): string | null {
  const normalized = pubkey.trim().toLowerCase();
  return HEX_PUBKEY.test(normalized) ? normalized : null;
}

/** Parse direct user input (hex or npub) into the canonical stored hex form. */
export function parseDirectAuthorInput(input: string): string | null {
  return parsePubkeyInput(input);
}

/**
 * Merge candidate sources in priority order. The first occurrence of an
 * identity owns its position and the presentation fields it supplies; a
 * later occurrence fills the fields it left empty (a relay member row has no
 * display name, its directory result does), so a search can match them.
 */
export function mergeAuthorCandidateSources(
  sources: readonly (readonly WorkflowAuthorCandidateInput[])[],
): WorkflowAuthorCandidate[] {
  const merged: WorkflowAuthorCandidate[] = [];
  const byPubkey = new Map<string, WorkflowAuthorCandidate>();

  for (const source of sources) {
    for (const candidate of source) {
      const pubkey = normalizeAuthorPubkey(candidate.pubkey);
      if (!pubkey) continue;
      const existing = byPubkey.get(pubkey);
      if (existing) {
        existing.displayName ??= candidate.displayName ?? null;
        existing.avatarUrl ??= candidate.avatarUrl ?? null;
        existing.nip05Handle ??= candidate.nip05Handle ?? null;
        existing.ownerPubkey ??= normalizeAuthorPubkey(
          candidate.ownerPubkey ?? "",
        );
        existing.isAgent ||= candidate.isAgent ?? false;
        continue;
      }
      const entry: WorkflowAuthorCandidate = {
        pubkey,
        displayName: candidate.displayName ?? null,
        avatarUrl: candidate.avatarUrl ?? null,
        nip05Handle: candidate.nip05Handle ?? null,
        ownerPubkey: normalizeAuthorPubkey(candidate.ownerPubkey ?? ""),
        isAgent: candidate.isAgent ?? false,
      };
      byPubkey.set(pubkey, entry);
      merged.push(entry);
    }
  }

  return merged;
}

export function nextWorkflowAuthorIndex(
  current: number | null,
  delta: number,
  length: number,
): number | null {
  if (length === 0) return null;
  const startingIndex = current ?? -1;
  return (((startingIndex + delta) % length) + length) % length;
}

export function filterAuthorCandidatePage(
  candidates: readonly WorkflowAuthorCandidate[],
  query: string,
  directPubkey: string | null,
  limit: number,
): WorkflowAuthorCandidate[] {
  if (directPubkey) {
    return candidates
      .filter(({ pubkey }) => pubkey === directPubkey)
      .slice(0, 1);
  }
  const needle = query.toLowerCase();
  return candidates
    .filter(
      (candidate) =>
        !needle ||
        [candidate.displayName, candidate.nip05Handle, candidate.pubkey].some(
          (field) => field?.toLowerCase().includes(needle),
        ),
    )
    .slice(0, limit);
}

/**
 * Add fetched profile presentation to existing identities without changing
 * candidate membership or order. Missing profile fields preserve useful data
 * already supplied by the higher-priority discovery source.
 */
export function enrichAuthorCandidates(
  candidates: readonly WorkflowAuthorCandidate[],
  profiles: Readonly<Record<string, UserProfileSummary | undefined>>,
): WorkflowAuthorCandidate[] {
  const normalizedProfiles = new Map(
    Object.entries(profiles).flatMap(([pubkey, profile]) => {
      const normalized = normalizeAuthorPubkey(pubkey);
      return normalized && profile ? [[normalized, profile] as const] : [];
    }),
  );

  return candidates.map((candidate) => {
    const profile = normalizedProfiles.get(candidate.pubkey);
    if (!profile) return candidate;
    return {
      ...candidate,
      displayName:
        profile.displayName?.trim() ||
        profile.name?.trim() ||
        candidate.displayName,
      avatarUrl: profile.avatarUrl ?? candidate.avatarUrl,
      nip05Handle: profile.nip05Handle ?? candidate.nip05Handle,
      ownerPubkey:
        normalizeAuthorPubkey(profile.ownerPubkey ?? "") ??
        candidate.ownerPubkey,
      isAgent: profile.isAgent ?? candidate.isAgent,
    };
  });
}
