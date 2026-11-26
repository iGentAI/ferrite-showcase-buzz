/**
 * Channel administration, browser edition — membership, archive, and deletion.
 *
 * These are the commands that CHANGE a room rather than read it. The desktop
 * app answers them in Rust; each one is really a Nostr event this relay
 * authorizes and then materializes into its own projections, so the browser
 * build sends exactly that event and lets the relay decide:
 *
 *   add_channel_members        → 9000 NIP-29 put-user, one per invitee
 *   change_channel_member_role → 9000 with a different `role` tag
 *   remove_channel_member      → 9001 NIP-29 remove-user
 *   archive_channel            → 9002 metadata edit carrying ONLY `archived`
 *   unarchive_channel          → the same 9002 with `archived=false`
 *   delete_channel             → 9008 NIP-29 delete-group
 *   delete_message             → 5 NIP-09 deletion
 *
 * Nothing here decides who may do what — the relay does, and its refusal
 * sentence is what the caller shows. Two places where that matters:
 *
 *   * `add_channel_members` reports its outcome PER PUBKEY, because the relay
 *     answers per event: adding four people can leave two added and two refused
 *     for different reasons, and the invite card renders exactly that.
 *   * `delete_message` refuses up front for a message this identity did not
 *     write. See {@link deleteMessageViaRelay} — the relay honours a NIP-09
 *     deletion only from the event's own author, and publishing one anyway
 *     would look like it worked while the relay kept serving the message.
 *
 * Lives outside `commands.ts` so the dispatch table stays a table: each command
 * there is one `case` that names the verb this module implements.
 */

import { CHANNEL_EVENT_KINDS } from "@/shared/constants/kinds";

import {
  publishSigned,
  relayEvents,
  type RelayIdentity,
} from "@/shared/browser-backend/relayConnection";

/** NIP-29 put-user: add a member, or set an existing member's role. */
const KIND_NIP29_PUT_USER = 9000;
/** NIP-29 remove-user: drop somebody else's membership. */
const KIND_NIP29_REMOVE_USER = 9001;
/** NIP-29 edit-group-metadata. The relay re-publishes the room's 39000 from it. */
const KIND_NIP29_EDIT_METADATA = 9002;
/** NIP-29 delete-group. The relay tombstones the room after owner/admin auth. */
const KIND_NIP29_DELETE_GROUP = 9008;
/** NIP-09 deletion. */
const KIND_DELETION = 5;

function requireChannelId(command: string, value: unknown): string {
  if (typeof value !== "string" || !value) {
    throw new Error(`${command} needs a channelId.`);
  }
  return value;
}

function requirePubkey(command: string, value: unknown): string {
  if (typeof value !== "string" || !/^[0-9a-f]{64}$/i.test(value)) {
    throw new Error(`${command} needs a 32-byte hex pubkey.`);
  }
  return value.toLowerCase();
}

/**
 * The tags of one NIP-29 membership command.
 *
 * `h` scopes it to the room and `p` names the target — the relay reads exactly
 * those (`effects::target_pubkey`). The `role` tag is the command's payload on
 * a put-user and meaningless on a remove, so it is only ever carried by 9000.
 *
 * The role is always stated rather than left to the relay's default, because
 * the relay's default is `member`: an add with no role SETS the target to
 * member, which for somebody who is already an admin is a demotion either way.
 * Saying it out loud makes the wire match the caller's intent.
 */
export function membershipTags(input: {
  channelId: string;
  pubkey: string;
  role?: string | null;
}): string[][] {
  const tags: string[][] = [
    ["h", input.channelId],
    ["p", input.pubkey.toLowerCase()],
  ];
  if (input.role) tags.push(["role", input.role]);
  return tags;
}

/**
 * `add_channel_members` — invite people into a room.
 *
 * One 9000 per invitee, in sequence, and the result is per pubkey. That shape
 * is not a convenience: the relay authorizes each command on its own
 * (`effects::authorize_membership_command` — the channel's add-policy, whether
 * the actor is a moderator, whether the requested role is elevated), so a batch
 * of four can come back "two added, one 'only owners/admins may grant elevated
 * roles', one 'this channel does not accept new members'". The invite card
 * lists those refusals next to the people they belong to, so they have to stay
 * attached to a pubkey rather than collapsing into one thrown error.
 *
 * Sequential, not parallel: the relay's per-author write budget is a fixed
 * 60/minute window, and a burst of invites is exactly the shape that trips it.
 */
export async function addChannelMembersViaRelay(
  identity: RelayIdentity,
  raw: unknown,
): Promise<{ added: string[]; errors: Array<{ pubkey: string; error: string }> }> {
  const input = (raw ?? {}) as {
    channelId?: unknown;
    pubkeys?: unknown;
    role?: unknown;
  };
  const channelId = requireChannelId("add_channel_members", input.channelId);
  const pubkeys = Array.isArray(input.pubkeys) ? (input.pubkeys as unknown[]) : [];
  const role = typeof input.role === "string" && input.role ? input.role : "member";

  const added: string[] = [];
  const errors: Array<{ pubkey: string; error: string }> = [];
  for (const pubkey of pubkeys) {
    // A malformed entry is REPORTED, not dropped. Silently skipping it is the
    // failure this whole command family is here to stop: the invite would
    // simply never happen and the card would show nothing at all.
    if (typeof pubkey !== "string" || !/^[0-9a-f]{64}$/i.test(pubkey)) {
      errors.push({
        pubkey: typeof pubkey === "string" ? pubkey : String(pubkey),
        error: "That is not a 32-byte hex pubkey.",
      });
      continue;
    }
    const target = pubkey.toLowerCase();
    try {
      const signed = await identity.sign({
        kind: KIND_NIP29_PUT_USER,
        content: "",
        tags: membershipTags({ channelId, pubkey: target, role }),
      });
      await publishSigned(signed, "Could not add that member.");
      added.push(target);
    } catch (error) {
      errors.push({
        pubkey: target,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }
  return { added, errors };
}

/**
 * `change_channel_member_role` — the same 9000, carrying the new role.
 *
 * There is no separate "change role" verb: put-user IS the verb that assigns a
 * role (`effects::apply_membership`), which is also why a plain join request
 * deliberately does not touch one. The role string is not validated here — the
 * relay parses it and refuses an unknown one before storage, and its sentence
 * is more accurate than a guess made in the tab.
 */
export async function changeChannelMemberRoleViaRelay(
  identity: RelayIdentity,
  raw: unknown,
): Promise<null> {
  const input = (raw ?? {}) as {
    channelId?: unknown;
    pubkey?: unknown;
    role?: unknown;
  };
  const channelId = requireChannelId("change_channel_member_role", input.channelId);
  const pubkey = requirePubkey("change_channel_member_role", input.pubkey);
  const role = typeof input.role === "string" ? input.role : "";
  if (!role) throw new Error("change_channel_member_role needs a role.");

  const signed = await identity.sign({
    kind: KIND_NIP29_PUT_USER,
    content: "",
    tags: membershipTags({ channelId, pubkey, role }),
  });
  await publishSigned(signed, "Could not change that member's role.");
  return null;
}

/**
 * `remove_channel_member` — drop somebody else's membership (9001).
 *
 * The self twin is `leave_channel` (9022), which `commands.ts` already answers;
 * this is the one an owner/admin uses on another member, and the one the huddle
 * bar uses to eject an agent. The relay soft-removes the row and bumps the
 * target's revocation tombstone, so a removed reader's live socket stops being
 * served the room on its next batch rather than at the next reconnect.
 */
export async function removeChannelMemberViaRelay(
  identity: RelayIdentity,
  raw: unknown,
): Promise<null> {
  const input = (raw ?? {}) as { channelId?: unknown; pubkey?: unknown };
  const channelId = requireChannelId("remove_channel_member", input.channelId);
  const pubkey = requirePubkey("remove_channel_member", input.pubkey);

  const signed = await identity.sign({
    kind: KIND_NIP29_REMOVE_USER,
    content: "",
    tags: membershipTags({ channelId, pubkey }),
  });
  await publishSigned(signed, "Could not remove that member.");
  return null;
}

/**
 * The tags of an archive (or unarchive) edit: the room, and `archived` alone.
 *
 * A 9002 is authorized PER TAG (`effects::authorize_channel_edit`), so nothing
 * else may ride along. That is not tidiness — an archived room refuses every
 * edit except an unarchive, so a 9002 that carried `name` beside
 * `archived=false` would be refused as "channel is archived" and the room could
 * never be brought back.
 */
export function archiveTags(channelId: string, archived: boolean): string[][] {
  return [
    ["h", channelId],
    ["d", channelId],
    ["archived", archived ? "true" : "false"],
  ];
}

/**
 * `archive_channel` / `unarchive_channel`.
 *
 * Archiving is a real relay state, not a client-side filter: the channel
 * document's `archived_at` is set, `Channel::is_writable` goes false, and every
 * subsequent write into the room is refused with "channel is archived or
 * deleted" while reads keep working — history does not vanish. Left
 * unanswered, the menu item did nothing at all and the room stayed writable
 * from the user's side, which is the one state the relay never puts it in.
 *
 * Returns nothing, as the desktop command does. The caller invalidates the
 * channel queries and the refetch reads `archived` back off the republished
 * 39000 projection (`channelsFromRelay` maps it to `archived_at`).
 */
export async function setChannelArchivedViaRelay(
  identity: RelayIdentity,
  raw: unknown,
  archived: boolean,
): Promise<null> {
  const command = archived ? "archive_channel" : "unarchive_channel";
  const input = (raw ?? {}) as { channelId?: unknown };
  const channelId = requireChannelId(command, input.channelId);

  const signed = await identity.sign({
    kind: KIND_NIP29_EDIT_METADATA,
    content: "",
    tags: archiveTags(channelId, archived),
  });
  await publishSigned(
    signed,
    archived ? "Could not archive that channel." : "Could not unarchive that channel.",
  );
  return null;
}

/** `delete_channel` — permanently remove a room from reads and writes (9008). */
export async function deleteChannelViaRelay(
  identity: RelayIdentity,
  raw: unknown,
): Promise<null> {
  const input = (raw ?? {}) as { channelId?: unknown };
  const channelId = requireChannelId("delete_channel", input.channelId);
  const signed = await identity.sign({
    kind: KIND_NIP29_DELETE_GROUP,
    content: "",
    tags: [
      ["h", channelId],
      ["d", channelId],
    ],
  });
  await publishSigned(signed, "Could not delete that channel.");
  return null;
}

/**
 * The tags of a NIP-09 deletion of one message.
 *
 * `e` names the target and `k` its kind, per NIP-09. `h` is the room — the
 * relay ignores it when deciding scope (a deletion INHERITS its target's
 * channel, so a client cannot file one into a room the target was never in),
 * but the client's live subscription is `kinds + #h`, so a deletion without it
 * is stored and never delivered to anyone watching the room. The message would
 * then stay on screen until a reload.
 */
export function deletionTags(input: {
  channelId: string;
  eventId: string;
  targetKind: number;
}): string[][] {
  return [
    ["e", input.eventId],
    ["k", String(input.targetKind)],
    ["h", input.channelId],
  ];
}

/**
 * `delete_message` — retract a message (NIP-09 kind 5).
 *
 * The target is read back first, for one reason that is worth the round trip:
 * **the relay honours a deletion only from the event's own author**
 * (`effects::apply_deletion` skips a target whose `pubkey` differs from the
 * deleter's). It still ACCEPTS the event — and every buzz client, this one
 * included, hides any message named by any kind:5 it sees — so publishing one
 * for somebody else's message would blank the message on screen while the relay
 * went on serving it to anyone who asked. That is worse than refusing, so this
 * refuses, and says which capability is missing.
 *
 * The same applies to the two other callers the desktop has for this command:
 * an agent's owner deleting their agent's message, and the moderation queue's
 * "delete" resolution. Both need a moderator deletion verb, and this relay has
 * none — its NIP-29 9005 is admitted and stored but never applied to anything.
 */
export async function deleteMessageViaRelay(
  identity: RelayIdentity,
  raw: unknown,
): Promise<null> {
  const input = (raw ?? {}) as { channelId?: unknown; eventId?: unknown };
  const channelId = requireChannelId("delete_message", input.channelId);
  const eventId =
    typeof input.eventId === "string" ? input.eventId : "";
  if (!eventId) throw new Error("delete_message needs an eventId.");

  const [me, found] = await Promise.all([
    identity.pubkey(),
    relayEvents({
      ids: [eventId],
      // `kinds` is required by the filter type. This is every kind the delete
      // affordance can be attached to — stream messages, diffs, system rows,
      // forum posts and comments — so a forum post is not mistaken for missing.
      kinds: [...CHANNEL_EVENT_KINDS],
      limit: 1,
    }),
  ]);
  const target = found.find((event) => event.id === eventId);
  if (!target) {
    throw new Error("The relay no longer holds that message.");
  }
  if (target.pubkey.toLowerCase() !== me.toLowerCase()) {
    throw new Error(
      "This relay only honours a deletion from the message's own author, so it cannot delete somebody else's message.",
    );
  }

  const signed = await identity.sign({
    kind: KIND_DELETION,
    content: "",
    tags: deletionTags({ channelId, eventId, targetKind: target.kind }),
  });
  await publishSigned(signed, "Could not delete that message.");
  return null;
}
