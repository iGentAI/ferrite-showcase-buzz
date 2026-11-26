import { canonicalNpub } from "@/shared/lib/pubkey";

export type PublicIdDisplay =
  | { kind: "loading" }
  | { kind: "unavailable" }
  | { kind: "ready"; npub: string };

/**
 * The public ID shown on first-run screens. Encoding runs during render, so
 * it must never throw: a pubkey that is not a valid hex key (a corrupted
 * keyring record, or a NIP-07 extension that returns an `npub1…`) reads as
 * unavailable instead of taking down the whole onboarding tree.
 */
export function describePublicId(
  pubkey: string | null | undefined,
  isPending = true,
): PublicIdDisplay {
  // "Loading" only while the identity query runs: a failed or empty answer
  // is unavailable, not forever loading.
  if (!pubkey) return isPending ? { kind: "loading" } : { kind: "unavailable" };
  // The same canonical form the rest of the app shows: a 64-hex key or a
  // checksummed npub (as NIP-07 extensions may return), padded or not, and
  // never a short payload npubEncode would happily encode.
  const npub = canonicalNpub(pubkey);
  return npub ? { kind: "ready", npub } : { kind: "unavailable" };
}

/**
 * Copy a public ID; resolves `false` instead of rejecting when the system
 * clipboard is denied or unavailable, so the caller can tell the user.
 */
export async function copyPublicId(
  npub: string,
  write: (text: string) => Promise<void>,
): Promise<boolean> {
  try {
    await write(npub);
    return true;
  } catch (error) {
    console.warn("[WelcomeSetup] copying the public ID failed:", error);
    return false;
  }
}
