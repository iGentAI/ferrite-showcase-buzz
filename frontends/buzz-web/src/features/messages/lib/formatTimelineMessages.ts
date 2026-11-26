import { isSafeCustomEmojiUrl } from "@/shared/lib/remarkCustomEmoji";
import type {
  Channel,
  ChannelMember,
  RelayEvent,
  RespondToMode,
} from "@/shared/api/types";

import type {
  TimelineMessage,
  TimelineReaction,
} from "@/features/messages/types";
import {
  getThreadReference,
  isBroadcastReply,
} from "@/features/messages/lib/threading";
import {
  formatOwnerLabel,
  resolveUserLabel,
  type UserProfileLookup,
} from "@/features/profile/lib/identity";
import { getMentionTagPubkey } from "@/shared/lib/resolveMentionNames";
import {
  KIND_JOB_ACCEPTED,
  KIND_JOB_CANCEL,
  KIND_JOB_ERROR,
  KIND_JOB_PROGRESS,
  KIND_JOB_REQUEST,
  KIND_JOB_RESULT,
  KIND_HUDDLE_STARTED,
  KIND_DELETION,
  KIND_NIP29_DELETE_EVENT,
  KIND_REACTION,
  KIND_STREAM_MESSAGE,
  KIND_STREAM_MESSAGE_V2,
  KIND_STREAM_MESSAGE_EDIT,
  KIND_STREAM_MESSAGE_DIFF,
  KIND_SYSTEM_MESSAGE,
} from "@/shared/constants/kinds";
import { resolveEventAuthorPubkey } from "@/shared/lib/authors";
import { normalizePubkey } from "@/shared/lib/pubkey";
import { channelRoleMap } from "@/shared/lib/rosterDerivations";

const EMPTY_ROLE_MAP: ReadonlyMap<string, string> = new Map();
import { formatTime } from "@/features/messages/lib/dateFormatters";
// Pure overlay helper lives in a sibling .mjs so node:test (no TS loader)
// can exercise the exact same source the renderer uses.
import { applyEditTagOverlay } from "@/features/messages/lib/applyEditTagOverlay.mjs";
import { truncateNpub } from "@/shared/lib/pubkey";

const HEX_RE = /^[0-9a-f]+$/i;
const PUBKEY_HEX_RE = /^[0-9a-f]{64}$/;

export function isTimelineContentEvent(event: RelayEvent) {
  return (
    event.kind === KIND_STREAM_MESSAGE ||
    event.kind === KIND_STREAM_MESSAGE_V2 ||
    event.kind === KIND_STREAM_MESSAGE_DIFF ||
    event.kind === KIND_SYSTEM_MESSAGE ||
    event.kind === KIND_JOB_REQUEST ||
    event.kind === KIND_JOB_ACCEPTED ||
    event.kind === KIND_JOB_PROGRESS ||
    event.kind === KIND_JOB_RESULT ||
    event.kind === KIND_JOB_CANCEL ||
    event.kind === KIND_JOB_ERROR ||
    event.kind === KIND_HUDDLE_STARTED
  );
}

function getDeletionTargets(tags: string[][]) {
  return tags
    .filter(
      (tag) =>
        tag[0] === "e" &&
        typeof tag[1] === "string" &&
        tag[1].length === 64 &&
        HEX_RE.test(tag[1]),
    )
    .map((tag) => tag[1]);
}

/**
 * Ids (lowercase) of the events in `events` that a deletion marker in the same
 * window legitimately removes.
 *
 * A kind:5 (or legacy kind:9005) hides its target only when the target's body
 * is held and the deletion's signer also signed the target — the rule the
 * relay applies before it erases anything (`effects::apply_deletion`). The
 * relay still stores and delivers a deletion that names somebody else's event,
 * so honouring every marker would let any room member blank any message for
 * every reader while the relay went on serving it.
 */
export function collectAuthorizedDeletionTargets(
  events: readonly RelayEvent[],
): Set<string> {
  const signerById = new Map<string, string>();
  for (const event of events) {
    signerById.set(event.id.toLowerCase(), normalizePubkey(event.pubkey));
  }
  const deleted = new Set<string>();
  for (const event of events) {
    if (
      event.kind !== KIND_DELETION &&
      event.kind !== KIND_NIP29_DELETE_EVENT
    ) {
      continue;
    }
    const deleter = normalizePubkey(event.pubkey);
    for (const targetId of getDeletionTargets(event.tags)) {
      const id = targetId.toLowerCase();
      if (signerById.get(id) === deleter) deleted.add(id);
    }
  }
  return deleted;
}

/**
 * A kind:40099 system row is the relay's word about membership and moderation.
 * Any room member can submit that kind, so a row renders only when the active
 * relay (NIP-11 `self`) signed it; with no valid relay identity, none do.
 */
export function isRelaySignedSystemMessage(
  event: RelayEvent,
  relaySelfPubkey: string | null | undefined,
): boolean {
  if (!relaySelfPubkey) return false;
  const relay = normalizePubkey(relaySelfPubkey);
  return PUBKEY_HEX_RE.test(relay) && normalizePubkey(event.pubkey) === relay;
}

/** Content kinds that render as a row, minus system rows the relay did not sign. */
function isRenderableTimelineEvent(
  event: RelayEvent,
  relaySelfPubkey: string | null | undefined,
): boolean {
  if (!isTimelineContentEvent(event)) return false;
  return (
    event.kind !== KIND_SYSTEM_MESSAGE ||
    isRelaySignedSystemMessage(event, relaySelfPubkey)
  );
}

/**
 * Count the *visible top-level rows* a raw event window would render in the
 * main channel timeline — the same unit `buildMainTimelineEntries` produces.
 *
 * This is deliberately NOT `events.length`: thread replies collapse into their
 * parent's summary row, deleted events disappear, and non-content kinds
 * (reactions, edits, deletions) never render as their own row. A history batch
 * heavy with replies can add 100 events but only a handful of rows, which is
 * why fetch-older counts rows here, not messages, when deciding how far to page.
 *
 * Mirrors the two filters that bound the rendered list:
 *   1. `formatTimelineMessages` keeps content kinds that aren't deletion targets.
 *   2. `buildMainTimelineEntries` keeps entries that are top-level
 *      (`parentId == null`) or broadcast replies.
 */
export function countTopLevelTimelineRows(
  events: RelayEvent[],
  relaySelfPubkey?: string | null,
): number {
  const deletedEventIds = collectAuthorizedDeletionTargets(events);

  let count = 0;
  for (const event of events) {
    if (
      !isRenderableTimelineEvent(event, relaySelfPubkey) ||
      deletedEventIds.has(event.id.toLowerCase())
    ) {
      continue;
    }
    const { parentId } = getThreadReference(event.tags);
    if (parentId == null || isBroadcastReply(event.tags)) {
      count += 1;
    }
  }
  return count;
}

function getReactionTargetId(tags: string[][]) {
  for (let index = tags.length - 1; index >= 0; index -= 1) {
    const tag = tags[index];
    if (
      tag?.[0] === "e" &&
      typeof tag[1] === "string" &&
      tag[1].length === 64 &&
      HEX_RE.test(tag[1])
    ) {
      return tag[1];
    }
  }

  return null;
}

function formatMessageAuthor(
  event: RelayEvent,
  channel: Channel | null,
  currentPubkey: string | undefined,
  profiles: UserProfileLookup | undefined,
  relaySelfPubkey: string | null | undefined,
) {
  const authorPubkey = resolveEventAuthorPubkey({
    event,
    preferActorTag: true,
    relaySelfPubkey,
    requireChannelTagForPTags: true,
  });
  const fallbackName =
    channel?.channelType === "dm"
      ? (() => {
          const participantIndex =
            channel.participantPubkeys.indexOf(authorPubkey);
          if (participantIndex < 0) {
            return null;
          }

          return channel.participants[participantIndex] ?? null;
        })()
      : null;

  return resolveUserLabel({
    pubkey: authorPubkey,
    currentPubkey,
    fallbackName,
    profiles,
    preferResolvedSelfLabel: true,
  });
}

function getAuthorAvatarUrl(input: {
  authorPubkey: string;
  currentPubkey: string | undefined;
  currentUserAvatarUrl: string | null;
  profiles: UserProfileLookup | undefined;
}) {
  const { authorPubkey, currentPubkey, currentUserAvatarUrl, profiles } = input;

  if (currentPubkey === authorPubkey) {
    return currentUserAvatarUrl ?? null;
  }

  return profiles?.[authorPubkey.toLowerCase()]?.avatarUrl ?? null;
}

export function hasLinkPreviewSuppression(
  tags: string[][] | undefined,
): boolean {
  return (
    tags?.some(
      (tag) =>
        tag[0] === "link-preview" && tag[1] === "none" && tag.length === 2,
    ) ?? false
  );
}

function isAuthorizedMessageEdit(
  edit: RelayEvent,
  target: RelayEvent,
  profiles: UserProfileLookup | undefined,
  relaySelfPubkey?: string | null,
): boolean {
  const author = normalizePubkey(
    resolveEventAuthorPubkey({
      event: target,
      preferActorTag: true,
      relaySelfPubkey,
      requireChannelTagForPTags: true,
    }),
  );
  const signer = normalizePubkey(edit.pubkey);
  if (signer === author) return true;
  return normalizePubkey(profiles?.[author]?.ownerPubkey ?? "") === signer;
}

export function formatTimelineMessages(
  events: RelayEvent[],
  channel: Channel | null,
  currentPubkey: string | undefined,
  currentUserAvatarUrl: string | null,
  profiles?: UserProfileLookup,
  members?: ChannelMember[],
  /** Map from lowercase pubkey → persona display name for bot messages. */
  personaLookup?: Map<string, string>,
  /** Map from lowercase pubkey → respond-to mode for bot messages. */
  respondToLookup?: Map<string, RespondToMode>,
  /** Active relay identity from NIP-11 `self`; absent or malformed fails closed to the signer. */
  relaySelfPubkey?: string | null,
  /** Profiles for verified agent owners, fetched in one batch by the surface. */
  ownerProfiles?: UserProfileLookup,
): TimelineMessage[] {
  const currentPubkeyLower = currentPubkey?.toLowerCase();
  // Identity-cached: rosters can be 10k+ members and this formatter re-runs
  // on every live message; the map is computed once per distinct roster.
  const roleByPubkey = members ? channelRoleMap(members) : EMPTY_ROLE_MAP;
  // Both kind:5 and kind:9005 are deletion markers, honoured only from the
  // target's own signer; mirror the relay. Ids are compared lowercase.
  const authorizedDeletions = collectAuthorizedDeletionTargets(events);
  const deletedEventIds = {
    has: (id: string) => authorizedDeletions.has(id.toLowerCase()),
  };

  const timelineEventsById = new Map(
    events
      .filter((event) => isRenderableTimelineEvent(event, relaySelfPubkey))
      .map((event) => [event.id, event]),
  );
  const previewSuppressedTargetIds = new Set<string>();

  // Build a map of latest authorized edit per original message. Preview
  // suppression is monotonic: any authorized edit carrying the marker wins
  // forever, independent of which edit supplies the latest body.
  // The edit's own tags are kept so the renderer can overlay imeta tags
  // (attachments) from the edit onto the original event — non-imeta tags on
  // the original (`h`, `p` mentions, etc.) stay untouched.
  const editsByTargetId = new Map<
    string,
    { content: string; tags: string[][]; createdAt: number; id: string }
  >();
  for (const event of events) {
    if (
      event.kind !== KIND_STREAM_MESSAGE_EDIT ||
      deletedEventIds.has(event.id)
    ) {
      continue;
    }

    const targetId = getReactionTargetId(event.tags);
    if (!targetId || deletedEventIds.has(targetId)) {
      continue;
    }
    const target = timelineEventsById.get(targetId);
    // Only stream messages are editable (EDITABLE_KINDS in the browser
    // backend, and the relay search): an edit aimed at a system row, huddle
    // card or job event must not rewrite it (finding_8557205ae09fddae).
    if (
      !target ||
      (target.kind !== KIND_STREAM_MESSAGE && target.kind !== KIND_STREAM_MESSAGE_V2) ||
      !isAuthorizedMessageEdit(event, target, profiles, relaySelfPubkey)
    ) {
      continue;
    }
    if (hasLinkPreviewSuppression(event.tags)) {
      previewSuppressedTargetIds.add(targetId);
    }

    const existing = editsByTargetId.get(targetId);
    // Two edits in the same second resolve the same way whatever order they
    // arrived in: the larger event id wins the tie.
    if (
      !existing ||
      event.created_at > existing.createdAt ||
      (event.created_at === existing.createdAt && event.id > existing.id)
    ) {
      editsByTargetId.set(targetId, {
        content: event.content,
        tags: event.tags,
        createdAt: event.created_at,
        id: event.id,
      });
    }
  }

  const visibleEvents = events.filter(
    (event) =>
      isRenderableTimelineEvent(event, relaySelfPubkey) &&
      !deletedEventIds.has(event.id),
  );
  const eventsById = new Map(visibleEvents.map((event) => [event.id, event]));
  const reactionPresence = new Map<
    string,
    {
      targetId: string;
      actorPubkey: string;
      emoji: string;
      emojiUrl?: string;
      createdAt: number;
    }
  >();

  for (const event of events) {
    if (event.kind !== KIND_REACTION || deletedEventIds.has(event.id)) {
      continue;
    }

    const targetId = getReactionTargetId(event.tags);
    if (!targetId || deletedEventIds.has(targetId)) {
      continue;
    }

    const actorPubkey = resolveEventAuthorPubkey({
      event,
      preferActorTag: true,
      relaySelfPubkey,
      requireChannelTagForPTags: true,
    }).toLowerCase();
    const emoji = event.content.trim() || "+";
    // Custom-emoji reaction (NIP-30): content is `:shortcode:` and the URL
    // rides on a matching `["emoji", shortcode, url]` tag.
    let emojiUrl: string | undefined;
    if (emoji.startsWith(":") && emoji.endsWith(":")) {
      const shortcode = emoji.slice(1, -1);
      const tagged = event.tags.find(
        (t) => t[0] === "emoji" && t[1] === shortcode && t[2],
      )?.[2];
      // The URL is the reactor's to choose: only a URL the custom-emoji
      // renderer itself accepts may become an <img src> (finding_e98867c335ef0c02).
      emojiUrl = isSafeCustomEmojiUrl(tagged) ? tagged : undefined;
    }
    const key = `${targetId}:${actorPubkey}:${emoji}`;
    const prev = reactionPresence.get(key);
    reactionPresence.set(key, {
      targetId,
      actorPubkey,
      emoji,
      emojiUrl,
      // Retain the earliest timestamp seen across duplicate deliveries so pill
      // chronology is invariant to input-array order.
      createdAt: prev
        ? Math.min(prev.createdAt, event.created_at)
        : event.created_at,
    });
  }

  // Internal accumulator: TimelineReaction + earliest timestamp for pill ordering.
  type ReactionAccum = TimelineReaction & { earliestCreatedAt: number };
  const reactionsByEventId = new Map<string, Map<string, ReactionAccum>>();
  for (const {
    targetId,
    actorPubkey,
    emoji,
    emojiUrl,
    createdAt,
  } of reactionPresence.values()) {
    const current = reactionsByEventId.get(targetId) ?? new Map();
    const existing = current.get(emoji) ?? {
      emoji,
      emojiUrl,
      count: 0,
      reactedByCurrentUser: false,
      users: [],
      earliestCreatedAt: createdAt,
    };
    if (createdAt < existing.earliestCreatedAt) {
      existing.earliestCreatedAt = createdAt;
    }

    existing.count += 1;
    if (currentPubkeyLower && actorPubkey === currentPubkeyLower) {
      existing.reactedByCurrentUser = true;
    }

    const profile = profiles?.[actorPubkey];
    const displayName =
      currentPubkeyLower && actorPubkey === currentPubkeyLower
        ? "You"
        : profile?.displayName?.trim() ||
          profile?.nip05Handle?.trim() ||
          truncateNpub(actorPubkey);
    existing.users.push({
      pubkey: actorPubkey,
      displayName,
      avatarUrl: profile?.avatarUrl ?? null,
    });

    current.set(emoji, existing);
    reactionsByEventId.set(targetId, current);
  }

  const authorPubkeyByEventId = new Map<string, string>();
  const authorLabelByEventId = new Map<string, string>();
  const depthByEventId = new Map<string, number>();
  const resolvingEventIds = new Set<string>();

  function getAuthorLabel(event: RelayEvent) {
    const cached = authorLabelByEventId.get(event.id);
    if (cached) {
      return cached;
    }

    const authorPubkey = resolveEventAuthorPubkey({
      event,
      preferActorTag: true,
      relaySelfPubkey,
      requireChannelTagForPTags: true,
    });
    const author = formatMessageAuthor(
      event,
      channel,
      currentPubkey,
      profiles,
      relaySelfPubkey,
    );

    authorPubkeyByEventId.set(event.id, authorPubkey);
    authorLabelByEventId.set(event.id, author);
    return author;
  }

  function getDepth(event: RelayEvent): number {
    const cached = depthByEventId.get(event.id);
    if (cached !== undefined) {
      return cached;
    }

    if (resolvingEventIds.has(event.id)) {
      return 0;
    }

    const thread = getThreadReference(event.tags);
    if (!thread.parentId) {
      depthByEventId.set(event.id, 0);
      return 0;
    }

    const parent = eventsById.get(thread.parentId);
    if (!parent) {
      const fallbackDepth =
        thread.rootId && thread.rootId !== thread.parentId ? 2 : 1;
      depthByEventId.set(event.id, fallbackDepth);
      return fallbackDepth;
    }

    resolvingEventIds.add(event.id);
    const depth = getDepth(parent) + 1;
    resolvingEventIds.delete(event.id);
    depthByEventId.set(event.id, depth);
    return depth;
  }

  return visibleEvents.map((event) => {
    const author = getAuthorLabel(event);
    const authorPubkey =
      authorPubkeyByEventId.get(event.id) ??
      resolveEventAuthorPubkey({
        event,
        preferActorTag: true,
        relaySelfPubkey,
        requireChannelTagForPTags: true,
      });
    const thread = getThreadReference(event.tags);
    const edit = editsByTargetId.get(event.id);
    const role = roleByPubkey.get(authorPubkey.toLowerCase());
    const authorProfile = profiles?.[authorPubkey.toLowerCase()];
    const isAgent = role === "bot" || authorProfile?.isAgent === true;
    const ownerPubkey = isAgent ? (authorProfile?.ownerPubkey ?? null) : null;
    return {
      id: event.id,
      renderKey: event.localKey ?? event.id,
      createdAt: event.created_at,
      pubkey: authorPubkey,
      signerPubkey: normalizePubkey(event.pubkey),
      author,
      isAgent,
      ownerPubkey,
      ownerLabel: isAgent
        ? formatOwnerLabel(ownerPubkey, currentPubkey, ownerProfiles)
        : null,
      avatarUrl: getAuthorAvatarUrl({
        authorPubkey,
        currentPubkey,
        currentUserAvatarUrl,
        profiles,
      }),
      role,
      personaDisplayName:
        role === "bot"
          ? personaLookup?.get(authorPubkey.toLowerCase())
          : undefined,
      respondTo:
        role === "bot"
          ? respondToLookup?.get(authorPubkey.toLowerCase())
          : undefined,
      time: formatTime(event.created_at),
      body: edit ? edit.content : event.content,
      parentId: thread.parentId,
      rootId: thread.rootId,
      depth: getDepth(event),
      accent: currentPubkey === authorPubkey,
      pending: event.pending,
      edited: edit !== undefined,
      kind: event.kind,
      // When edited, swap the original event's imeta tags for the edit's
      // imeta tags. All non-imeta tags on the original are preserved.
      // Logic lives in `applyEditTagOverlay.mjs` so prod and tests share
      // a single source.
      tags: (() => {
        const effectiveTags = applyEditTagOverlay(event.tags, edit?.tags);
        if (
          hasLinkPreviewSuppression(event.tags) ||
          previewSuppressedTargetIds.has(event.id)
        ) {
          return hasLinkPreviewSuppression(effectiveTags)
            ? effectiveTags
            : [...effectiveTags, ["link-preview", "none"]];
        }
        return effectiveTags;
      })(),
      reactions: (() => {
        const reactions = reactionsByEventId.get(event.id);
        if (!reactions) return undefined;
        // Sort pills by earliest reaction time ascending (Slack-style: first-reacted
        // emoji leftmost). Tiebreak on emoji string for determinism.
        return [...reactions.values()]
          .sort(
            (a, b) =>
              a.earliestCreatedAt - b.earliestCreatedAt ||
              a.emoji.localeCompare(b.emoji),
          )
          .map(({ earliestCreatedAt: _drop, ...pill }) => pill);
      })(),
    };
  });
}

function extractSystemMessagePubkeys(event: RelayEvent): string[] {
  if (event.kind !== KIND_SYSTEM_MESSAGE) {
    return [];
  }

  try {
    const payload = JSON.parse(event.content);
    const pubkeys: string[] = [];
    if (typeof payload.actor === "string") {
      pubkeys.push(payload.actor.toLowerCase());
    }
    if (typeof payload.target === "string") {
      pubkeys.push(payload.target.toLowerCase());
    }
    return pubkeys;
  } catch {
    return [];
  }
}

export function collectReactionActorPubkeys(
  events: RelayEvent[],
  relaySelfPubkey?: string | null,
) {
  const deletedEventIds = collectAuthorizedDeletionTargets(events);

  const pubkeys = new Set<string>();
  for (const event of events) {
    if (
      event.kind !== KIND_REACTION ||
      deletedEventIds.has(event.id.toLowerCase())
    ) {
      continue;
    }
    pubkeys.add(
      resolveEventAuthorPubkey({
        event,
        preferActorTag: true,
        relaySelfPubkey,
        requireChannelTagForPTags: true,
      }).toLowerCase(),
    );
  }
  return [...pubkeys];
}

export function collectMessageAuthorPubkeys(
  events: RelayEvent[],
  relaySelfPubkey?: string | null,
) {
  const pubkeys = new Set<string>();

  for (const event of events) {
    if (!isTimelineContentEvent(event)) {
      continue;
    }

    if (event.kind === KIND_SYSTEM_MESSAGE) {
      for (const pk of extractSystemMessagePubkeys(event)) {
        pubkeys.add(pk);
      }
    } else {
      pubkeys.add(event.pubkey.toLowerCase());
      pubkeys.add(
        resolveEventAuthorPubkey({
          event,
          preferActorTag: true,
          relaySelfPubkey,
          requireChannelTagForPTags: true,
        }).toLowerCase(),
      );
    }
  }

  return [...pubkeys];
}

export function collectMessageMentionPubkeys(
  events: Array<{ tags?: string[][] }>,
) {
  const pubkeys = new Set<string>();

  for (const event of events) {
    for (const tag of event.tags ?? []) {
      const pubkey = getMentionTagPubkey(tag);
      if (pubkey) {
        pubkeys.add(pubkey);
      }
    }
  }

  return [...pubkeys];
}

/** Every pubkey a channel surface needs profiles for: authors (signer +
 *  attributed actor), mentions, and reaction actors, deduplicated. */
export function collectMessageProfilePubkeys(events: RelayEvent[]) {
  return [
    ...new Set([
      ...collectMessageAuthorPubkeys(events),
      ...collectMessageMentionPubkeys(events),
      ...collectReactionActorPubkeys(events),
    ]),
  ];
}
