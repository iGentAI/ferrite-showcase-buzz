import { getCachedRelayOrigin } from "@/shared/lib/mediaUrl";
import { listManagedAgents } from "@/shared/api/tauri";
import { getIdentity } from "@/shared/api/tauriIdentity";
import { getUsersBatch } from "@/shared/api/tauriProfiles";
import {
  buildProjectHomeFromFetcher,
  buildProjectsFromFetcher,
  type FetchProjectEventsExhaustively,
  fetchProjectEventsExhaustively,
} from "./projectEnumeration";
import type { Project } from "./projectModels";
import { markProjectDataAuthoritative } from "./projectSnapshot";

const HIDDEN_PROJECT_CARDS_KEY = "buzz.projects.hidden-cards.v1";

function readHiddenProjectCards(): string[] {
  if (typeof window === "undefined") {
    return [];
  }

  try {
    const parsed = JSON.parse(
      window.localStorage.getItem(HIDDEN_PROJECT_CARDS_KEY) ?? "[]",
    );
    return Array.isArray(parsed)
      ? parsed.filter((item): item is string => typeof item === "string")
      : [];
  } catch {
    return [];
  }
}

/** The verified NIP-OA owners the profile service reports for `pubkeys`. */
async function verifiedAgentOwners(
  pubkeys: string[],
): Promise<ReadonlyMap<string, string>> {
  const batch = await getUsersBatch(pubkeys);
  const owners = new Map<string, string>();
  for (const [pubkey, profile] of Object.entries(batch.profiles)) {
    if (profile.ownerPubkey) {
      owners.set(pubkey.toLowerCase(), profile.ownerPubkey.toLowerCase());
    }
  }
  return owners;
}

async function viewerAuthorizedAgents(
  viewerPubkey: string | undefined,
): Promise<ReadonlyMap<string, ReadonlySet<string>> | undefined> {
  if (!viewerPubkey) return undefined;
  const managed = await listManagedAgents().catch(() => []);
  if (managed.length === 0) return undefined;
  return new Map([
    [
      viewerPubkey.trim().toLowerCase(),
      new Set(managed.map((agent) => agent.pubkey.trim().toLowerCase())),
    ],
  ]);
}

/** Enumerates the projects visible to the current relay identity. */
export async function fetchProjects(
  fetchExhaustively?: FetchProjectEventsExhaustively,
  signal?: AbortSignal,
): Promise<Project[]> {
  // Delegates to `buildProjectsFromFetcher` in `projectEnumeration.ts`, which
  // is the pure, Tauri-free core of this operation. Its javadoc explains
  // fail-closed tombstones and NIP-OA owner-deletion suppression.
  const viewerPubkey = await getIdentity()
    .then((identity) => identity.pubkey)
    .catch(() => undefined);
  const authorizedAgentPubkeysByOwner =
    await viewerAuthorizedAgents(viewerPubkey);
  const fetcher: FetchProjectEventsExhaustively =
    fetchExhaustively ??
    ((kinds, extraFilter) =>
      fetchProjectEventsExhaustively(kinds, extraFilter, undefined, signal));
  const projects = await buildProjectsFromFetcher(fetcher, {
    relayOrigin: getCachedRelayOrigin(),
    hiddenAddresses: new Set(readHiddenProjectCards()),
    viewerPubkey,
    authorizedAgentPubkeysByOwner,
    resolveAgentOwners: verifiedAgentOwners,
  });
  return projects.map((project) =>
    markProjectDataAuthoritative(project, "relay"),
  );
}

/** Resolves the active channel's project home with a scoped relay query. */
export async function fetchProjectHomeForChannel(
  channelId: string,
  signal?: AbortSignal,
): Promise<Project | null> {
  const viewerPubkey = await getIdentity()
    .then((identity) => identity.pubkey)
    .catch(() => undefined);
  const authorizedAgentPubkeysByOwner =
    await viewerAuthorizedAgents(viewerPubkey);
  const project = await buildProjectHomeFromFetcher(
    (kinds, extraFilter) =>
      fetchProjectEventsExhaustively(kinds, extraFilter, undefined, signal),
    channelId,
    {
      relayOrigin: getCachedRelayOrigin(),
      hiddenAddresses: new Set(readHiddenProjectCards()),
      viewerPubkey,
      authorizedAgentPubkeysByOwner,
    },
  );
  return project ? markProjectDataAuthoritative(project, "relay") : null;
}
