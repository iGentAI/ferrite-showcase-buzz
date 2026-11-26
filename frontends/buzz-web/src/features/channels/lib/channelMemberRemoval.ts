import type { ChannelMember } from "@/shared/api/types";

/**
 * Whether the viewer may remove `member` from a channel.
 *
 * Roles are a hierarchy: an owner may remove admins and members (but not a
 * fellow owner), an admin may remove admins and members but never an owner,
 * and anyone may remove themselves or their own bot. The relay still decides;
 * this keeps the members list from offering an action the viewer's role does
 * not carry.
 */
export function canRemoveChannelMember<
  Member extends Pick<ChannelMember, "pubkey" | "role">,
>({
  selfMember,
  member,
  currentPubkey,
  isMyBot,
}: {
  selfMember: Pick<ChannelMember, "role"> | null | undefined;
  member: Member;
  currentPubkey: string | null | undefined;
  isMyBot: (member: Member) => boolean;
}): boolean {
  return (
    // An owner is removed only by an owner (the relay refuses anyone else),
    // even when the owner is the viewer's own bot; leaving is always allowed.
    (member.role !== "owner" ||
      selfMember?.role === "owner" ||
      member.pubkey === currentPubkey) &&
    ((selfMember?.role === "admin" &&
      member.pubkey !== currentPubkey &&
      member.role !== "owner") ||
      (selfMember?.role === "owner" && member.role !== "owner") ||
      Boolean(selfMember && isMyBot(member)) ||
      member.pubkey === currentPubkey)
  );
}
