/**
 * IDENTITY ARCHIVE (NIP-IA) and agent ownership, from a browser tab.
 *
 * * **Archiving**: the relay materializes an archive row from a 9035/9036
 *   request, publishes the 8002 / 8003 notice, and republishes the 13535 list.
 * * **`resolve_oa_owner`**: the relay RECORDED the owner when the agent was
 *   created, behind the create-if-absent claim that decides who may configure
 *   it. So the answer is verified by the relay at the moment it was established
 *   rather than by re-checking something a profile asserts.
 *
 * An agent with no recorded owner — one an operator seeded straight into the
 * registry — answers `null`. That is the truth about it, not a gap.
 */
import { apiPost } from "@/shared/lib/api";
import { randomUuid } from "@/shared/lib/insecureContextCrypto";
import { BUZZ_AUTHORIZATION_HEADER } from "@/shared/lib/nip98";
import { apiUrl } from "@/shared/lib/relay-url";
import {
  publishSigned,
  type RelayIdentity,
} from "@/shared/browser-backend/relayConnection";

/** NIP-IA request and snapshot kinds, as the relay declares them. */
const KIND_ARCHIVE_REQUEST = 9035;
const KIND_UNARCHIVE_REQUEST = 9036;

/**
 * `archive_identity` / `unarchive_identity` — a NIP-IA request.
 *
 * The `p` tag names the target, and its ABSENCE means "myself" — which is the
 * common case and the one that needs no elevated authority. The relay enforces
 * the rule (self, or a community moderator); this only carries the request, and
 * deliberately does not pre-check it, because a client-side guess at authority
 * that disagreed with the relay's answer would be worse than no guess at all.
 */
async function archiveRequest(
  identity: RelayIdentity,
  kind: number,
  req: { targetPubkey?: string; reason?: string; content?: string } | undefined,
  failed: string,
): Promise<null> {
  const target = (req?.targetPubkey ?? "").toLowerCase();
  const me = (await identity.pubkey()).toLowerCase();
  const tags: string[][] = [];
  // Only tag a target that is somebody else: a bare request is a self-archive,
  // and the relay reads it that way.
  if (target && target !== me) {
    if (!/^[0-9a-f]{64}$/.test(target)) {
      throw new Error("That is not a 32-byte hex pubkey.");
    }
    tags.push(["p", target]);
  }
  await publishSigned(
    await identity.sign({ kind, content: req?.reason ?? req?.content ?? "", tags }),
    failed,
  );
  return null;
}

/** `archive_identity`. */
export function archiveIdentity(
  identity: RelayIdentity,
  args: { req?: { targetPubkey?: string; reason?: string; content?: string } },
): Promise<null> {
  return archiveRequest(
    identity,
    KIND_ARCHIVE_REQUEST,
    args.req,
    "Could not archive that identity.",
  );
}

/** `unarchive_identity`. */
export function unarchiveIdentity(
  identity: RelayIdentity,
  args: { req?: { targetPubkey?: string; reason?: string; content?: string } },
): Promise<null> {
  return archiveRequest(
    identity,
    KIND_UNARCHIVE_REQUEST,
    args.req,
    "Could not unarchive that identity.",
  );
}

/**
 * A GET signed as THIS TAB's identity.
 *
 * `shared/lib/api`'s `apiGet` sends no Authorization header at all, and
 * `/archived-identities` is a members-only read, so calling it through that
 * helper is a 401 whose cause is invisible at the call site.
 */
async function signedGet<T>(identity: RelayIdentity, path: string): Promise<T> {
  const url = apiUrl(path);
  const auth = await identity.sign({
    kind: 27235,
    content: "",
    tags: [
      ["u", url],
      ["method", "GET"],
      // The relay burns an authorization event id on use, so two reads of one
      // path in the same second would otherwise be the same event — and the
      // second a replay.
      ["nonce", randomUuid()],
    ],
  });
  const response = await fetch(url, {
    headers: {
      Accept: "application/json",
      [BUZZ_AUTHORIZATION_HEADER]: `Nostr ${btoa(JSON.stringify(auth))}`,
    },
    signal: AbortSignal.timeout(15_000),
  });
  if (!response.ok) {
    throw new Error(
      `The archived identities could not be read (http ${response.status}).`,
    );
  }
  return (await response.json()) as T;
}

/**
 * `list_archived_identities` — the relay's own archive rows.
 *
 * Read from the route rather than from the published 13535 event, and the
 * difference matters: the rows ARE the state, while the event is a derived view
 * that can be one publish behind. The caller destructures `archived`, so an
 * empty set is the empty array rather than null.
 */
export async function listArchivedIdentities(
  identity: RelayIdentity,
): Promise<{ archived: string[] }> {
  const reply = await signedGet<{ archived?: Array<{ pubkey?: string }> }>(
    identity,
    "/archived-identities",
  );
  return {
    archived: (reply.archived ?? [])
      .map((row) => row.pubkey)
      .filter((pubkey): pubkey is string => typeof pubkey === "string"),
  };
}

/**
 * `resolve_oa_owner` — who owns an agent, as the RELAY recorded it.
 *
 * `null` for anything that is not a registered agent, and for an agent whose
 * owner was never recorded (an operator-seeded one). Both are honest answers:
 * the caller uses this to decide whether to offer an owner-path affordance, and
 * offering one for an unowned agent would be a button that cannot work.
 */
export async function resolveOaOwner(
  identity: RelayIdentity,
  args: { targetPubkey?: string },
): Promise<{ owner: string; is_me: boolean } | null> {
  const target = (args.targetPubkey ?? "").toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(target)) return null;
  const reply = await apiPost<{ owner_pubkey?: string | null }>("/agents/owner", {
    pubkey: target,
  });
  const owner = reply.owner_pubkey;
  if (typeof owner !== "string" || !owner) return null;
  const me = (await identity.pubkey()).toLowerCase();
  return { owner, is_me: owner.toLowerCase() === me };
}
