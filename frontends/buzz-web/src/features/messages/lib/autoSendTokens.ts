/**
 * One-time capabilities for "send this draft after navigating to it".
 *
 * The intent used to travel as `?autoSend=<draftKey>`, and a draft key is
 * public (a channel id, or `thread:<root id>`), so any link could make the
 * composer sign and publish whatever draft the reader had saved for that room.
 * The URL now carries a random token that exists only in this tab's memory,
 * minted when the person confirms the send, and consumed on first use; a token
 * from a shared or crafted link was never minted here and sends nothing.
 */
import { randomUuid } from "@/shared/lib/insecureContextCrypto";

const pending = new Map<string, string>();

/** Mint the token that lets the composer for `draftKey` send once. */
export function mintAutoSendToken(draftKey: string): string {
  const token = `send-${randomUuid()}`;
  pending.set(token, draftKey);
  return token;
}

/**
 * Whether `token` was minted for `draftKey`; true at most once, after which
 * the token is spent.
 */
export function consumeAutoSendToken(
  token: string | null | undefined,
  draftKey: string | null,
): boolean {
  if (!token || draftKey === null || pending.get(token) !== draftKey) return false;
  pending.delete(token);
  return true;
}
