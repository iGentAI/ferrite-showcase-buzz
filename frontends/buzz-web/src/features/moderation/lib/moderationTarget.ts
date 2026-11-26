import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * Whether a moderator may act on `targetPubkey` from a message menu.
 *
 * Fails closed: until the viewer's own key and the relay's signing key are
 * both known, the menu cannot tell whether the target is the moderator
 * themselves (a self-ban locks them out of unbanning) or the relay's own key
 * on a relay-signed message (finding_2d21091412ccc832). `relaySelfPubkey` is
 * `null` when the relay advertises none, which is a resolved answer;
 * `undefined` means not known yet.
 */
export function canModerateMessageTarget(input: {
  canModerate: boolean;
  targetPubkey: string | null | undefined;
  viewerPubkey: string | null | undefined;
  relaySelfPubkey: string | null | undefined;
}): boolean {
  const { canModerate, targetPubkey, viewerPubkey, relaySelfPubkey } = input;
  if (!canModerate || !targetPubkey || !viewerPubkey) return false;
  if (relaySelfPubkey === undefined) return false;
  const target = normalizePubkey(targetPubkey);
  if (target === normalizePubkey(viewerPubkey)) return false;
  if (relaySelfPubkey !== null && target === normalizePubkey(relaySelfPubkey)) {
    return false;
  }
  return true;
}
