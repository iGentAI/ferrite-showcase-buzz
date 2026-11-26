/**
 * `buzz://message` link encoding for "Copy link" / deep-link-to-message.
 *
 * Format: `buzz://message?channel=<uuid>&id=<eventId>[&thread=<rootId>]`
 */

const MESSAGE_LINK_SCHEME = "buzz:";
const MESSAGE_LINK_HOST = "message";
/**
 * Channel ids this build can hold: a v4 UUID for rooms created in the app, a
 * 64-hex digest for direct messages, and a lowercase slug for the pinned
 * starter rooms (`channelIdFor`). All three are drawn from this charset and
 * fit the relay's 96-byte key-component limit. Anything else (spaces,
 * punctuation, capitalised words chosen to read like a channel name) is not a
 * channel this client could have linked to, so it is refused before any
 * lookup or chip is produced.
 */
const MESSAGE_LINK_CHANNEL_PATTERN = /^[0-9a-z][0-9a-z_-]{0,95}$/u;
const MESSAGE_LINK_EVENT_ID_PATTERN = /^[0-9a-f]{64}$/iu;
const MESSAGE_LINK_PARAMS = new Set(["channel", "id", "thread"]);

export type MessageLinkInput = {
  channelId: string;
  messageId: string;
  /**
   * Optional thread root event id. Present when the linked message is a
   * reply (so the caller can route into a thread / forum post view).
   *
   * Currently emitted into the URL but not consumed by the click handler
   * or deep-link listener — both route via `goChannel(channelId,
   * { messageId })` and let `useAnchoredScroll` resolve the target.
   * Reserved for future "open in thread view" routing.
   */
  threadRootId?: string | null;
};

export type ParsedMessageLink = {
  channelId: string;
  messageId: string;
  threadRootId: string | null;
};

export type MessageLinkParseResult =
  | { ok: true; value: ParsedMessageLink }
  | { ok: false; reason: string };

/**
 * Build a `buzz://message` URL for a given channel + message.
 *
 * Empty `threadRootId` is treated as "no thread" so callers can pass through
 * the result of `getThreadReference(tags).rootId` without extra null checks.
 */
export function buildMessageLink(input: MessageLinkInput): string {
  if (!input.channelId) {
    throw new Error("buildMessageLink: channelId is required");
  }
  if (!input.messageId) {
    throw new Error("buildMessageLink: messageId is required");
  }

  const params = new URLSearchParams();
  params.set("channel", input.channelId);
  params.set("id", input.messageId);
  if (input.threadRootId) {
    params.set("thread", input.threadRootId);
  }
  return `${MESSAGE_LINK_SCHEME}//${MESSAGE_LINK_HOST}?${params.toString()}`;
}

/**
 * Parse a `buzz://message?…` URL. Returns a discriminated result so callers can
 * render a fallback (e.g. a plain link) without throwing.
 */
export function parseMessageLink(url: string): MessageLinkParseResult {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return { ok: false, reason: "invalid-url" };
  }

  if (parsed.protocol !== MESSAGE_LINK_SCHEME) {
    return { ok: false, reason: "wrong-scheme" };
  }
  // `new URL("buzz://message?…")` puts "message" in `hostname`.
  if (parsed.hostname !== MESSAGE_LINK_HOST) {
    return { ok: false, reason: "wrong-host" };
  }

  const channelId = parsed.searchParams.get("channel");
  const messageId = parsed.searchParams.get("id");
  if (!channelId) {
    return { ok: false, reason: "missing-channel" };
  }
  if (!messageId) {
    return { ok: false, reason: "missing-id" };
  }
  // Remote message content can carry any `buzz://message?…` string, and every
  // accepted link becomes a chip plus relay lookups. Accept only the shape
  // `buildMessageLink` produces: known parameters, each at most once, a real
  // channel id and 64-hex event ids.
  for (const name of new Set(parsed.searchParams.keys())) {
    if (!MESSAGE_LINK_PARAMS.has(name)) {
      return { ok: false, reason: "unexpected-param" };
    }
    if (parsed.searchParams.getAll(name).length > 1) {
      return { ok: false, reason: "duplicate-param" };
    }
  }
  if (!MESSAGE_LINK_CHANNEL_PATTERN.test(channelId)) {
    return { ok: false, reason: "invalid-channel" };
  }
  if (!MESSAGE_LINK_EVENT_ID_PATTERN.test(messageId)) {
    return { ok: false, reason: "invalid-id" };
  }
  const threadRootId = parsed.searchParams.get("thread") || null;
  if (threadRootId !== null && !MESSAGE_LINK_EVENT_ID_PATTERN.test(threadRootId)) {
    return { ok: false, reason: "invalid-thread" };
  }

  return {
    ok: true,
    value: {
      channelId,
      messageId: messageId.toLowerCase(),
      threadRootId: threadRootId?.toLowerCase() ?? null,
    },
  };
}

/**
 * Convenience: returns true if the given href is a supported message link.
 * Cheap pre-check used by the markdown renderer before parsing.
 */
export function isMessageLink(href: string | undefined | null): boolean {
  if (!href) return false;
  return href.startsWith("buzz://message?") || href === "buzz://message";
}

type MessageLinkRenderInput = {
  href: string;
  label: string;
};

export type MessageLinkRenderTarget =
  | { kind: "pill"; link: ParsedMessageLink }
  | { kind: "label"; link: ParsedMessageLink }
  | { kind: "none" };

/**
 * Centralizes how markdown-rendered anchors map to message-link UI. Both
 * CommonMark autolinks (`<buzz://message?...>`) and explicitly labeled links
 * arrive as anchors; autolinks have label === href and should render as pills,
 * while intentionally labeled links keep their label.
 */
export function resolveMessageLinkRenderTarget({
  href,
  label,
}: MessageLinkRenderInput): MessageLinkRenderTarget {
  if (!isMessageLink(href)) return { kind: "none" };

  const parsed = parseMessageLink(href);
  if (!parsed.ok) return { kind: "none" };

  return {
    kind: label === href ? "pill" : "label",
    link: parsed.value,
  };
}
