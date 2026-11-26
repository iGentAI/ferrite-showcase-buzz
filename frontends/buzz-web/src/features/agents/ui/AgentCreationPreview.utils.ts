import type * as React from "react";

export function isAvatarFileDrag(event: React.DragEvent<HTMLElement>) {
  return Array.from(event.dataTransfer.types).includes("Files");
}

export const AVATAR_APPLY_MOTION_TRANSITION = {
  duration: 0.14,
  ease: [0.23, 1, 0.32, 1],
} as const;

export type AvatarTab = "image" | "emoji";

export type EmojiMartEmoji = {
  native?: string;
};

/**
 * Decide what a pasted avatar/icon link may commit.
 *
 * Agent avatars accept the same links a human profile does: `http(s)` only,
 * no embedded credentials, at most the persisted size bound (see
 * `isValidRemoteAvatarUrl`). Callers that store images inline (the community
 * icon, whose `processImage` downscales files to a small `data:` URL that
 * every member's client renders without a cross-relay fetch) accept no pasted
 * link at all, because a remote URL would defeat that boundary and hand every
 * member's IP to the link's host.
 */
export function resolvePastedAvatarUrl(
  draft: string,
  options: { inlineImagesOnly: boolean },
  isValidRemoteUrl: (value: string) => boolean,
): string | null {
  if (options.inlineImagesOnly) return null;
  const candidate = draft.trim();
  if (candidate.length === 0 || !isValidRemoteUrl(candidate)) return null;
  return candidate;
}
