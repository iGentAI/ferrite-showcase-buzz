/**
 * Workspace verbs a browser tab can answer with a relay verb or a browser API.
 *
 * | command | how |
 * | --- | --- |
 * | `edit_message` | a kind 40003 the relay already scopes and indexes |
 * | `get_contact_list` | NIP-02 kind 3, read from the relay |
 * | `set_contact_list` | the same kind, signed and published |
 * | `download_file` / `download_image` | an anchor with `download` |
 * | `copy_image_to_clipboard` | the async clipboard, via a PNG canvas |
 */
import type { RelayEvent } from "@/shared/api/types";
import { validatedExtraTags } from "@/shared/browser-backend/messageSend";
import {
  publishSigned,
  relayEvents,
  type RelayIdentity,
} from "@/shared/browser-backend/relayConnection";

/** buzz's edit kind (`KIND_STREAM_MESSAGE_EDIT` on the relay). */
const KIND_MESSAGE_EDIT = 40003;
/** NIP-02. */
const KIND_CONTACT_LIST = 3;
/** buzz's timeline message kinds — what an edit is allowed to target. */
const EDITABLE_KINDS = [9, 40002];

/**
 * `edit_message` — replace the text of a message you wrote.
 *
 * A kind 40003 carrying the room (`h`), the target (`e`) and the new content.
 * The relay stores and delivers it like any other channel event; the client
 * applies it as an overlay on the original (`formatTimelineMessages`).
 *
 * **Only the author may edit.** Checked here, and not merely for politeness: the
 * relay accepts a 40003 from anybody, and every CLIENT decides whose edit to
 * honour by comparing authors. So an edit of someone else's message would be
 * accepted, stored, delivered — and ignored by every reader. That is a button
 * that reports success and changes nothing, which is worse than a refusal.
 *
 * The media, emoji and mention tags ride along because an edit REPLACES the
 * content: an edited message that dropped its `imeta` tags would lose the
 * attachments the original rendered.
 */
export async function editMessage(
  identity: RelayIdentity,
  args: {
    channelId?: string;
    eventId?: string;
    content?: string;
    mediaTags?: string[][];
    emojiTags?: string[][];
    mentionPubkeys?: string[] | null;
    mentionTags?: string[][] | null;
    suppressLinkPreviews?: boolean;
  },
): Promise<null> {
  const channelId = args.channelId ?? "";
  const eventId = args.eventId ?? "";
  if (!channelId || !eventId) {
    throw new Error("edit_message needs a channelId and an eventId.");
  }

  // The target must exist, be one of ours, and be a kind an edit can apply to.
  const [target] = await relayEvents({
    ids: [eventId],
    kinds: EDITABLE_KINDS,
    limit: 1,
  });
  if (!target) {
    throw new Error("That message is not on the relay, so it cannot be edited.");
  }
  const me = (await identity.pubkey()).toLowerCase();
  if (target.pubkey.toLowerCase() !== me) {
    throw new Error("You can only edit your own messages.");
  }

  // The room comes from the ORIGINAL, not from the caller. An edit carries an
  // `h` tag, and the relay scopes it by that tag: pointing it at a different
  // room produces an event that is accepted, stored and delivered, and that
  // then overlays nothing anywhere — the same report-success-change-nothing
  // failure the author check above exists to prevent.
  //
  // A caller whose channel disagrees is REFUSED rather than quietly corrected.
  // Disagreement means they are working from a stale view of where that message
  // lives, and silently fixing it up would hide that from them.
  const targetChannel = (target.tags ?? []).find(
    (candidate) => candidate[0] === "h" && candidate[1],
  )?.[1];
  if (!targetChannel) {
    throw new Error("That message names no channel, so an edit cannot be scoped to one.");
  }
  if (targetChannel !== channelId) {
    throw new Error(
      "That message is in a different channel from the one this edit named.",
    );
  }

  // The same contract as the desktop command: each tag argument carries only
  // its own tag name, mention references are a snapshot (so readers replace
  // rather than merge them), and "remove link previews" is a real tag rather
  // than an edit that changes nothing.
  const tags: string[][] = [
    ["h", targetChannel],
    ["e", eventId],
    ...validatedExtraTags(
      {
        mediaTags: args.mediaTags,
        emojiTags: args.emojiTags,
        mentionTags: args.mentionTags,
      },
      "edit_message",
    ),
    ...(args.mentionTags != null ? [["buzz:mention-snapshot"]] : []),
    ...(args.mentionPubkeys ?? []).map((pubkey) => ["p", pubkey]),
    ...(args.suppressLinkPreviews ? [["link-preview", "none"]] : []),
  ];
  const signed = await identity.sign({
    kind: KIND_MESSAGE_EDIT,
    content: (args.content ?? "").trim(),
    tags,
  });
  await publishSigned(signed, "Could not save that edit.");
  return null;
}

/**
 * `get_contact_list` — someone's NIP-02 follow list (kind 3).
 *
 * Replaceable, so the newest one is the list. An identity that has never
 * published one gets an EMPTY list rather than an error: "follows nobody" and
 * "has no list" are the same thing to every caller, and rejecting would paint an
 * error over a perfectly ordinary state.
 */
export async function getContactList(args: { pubkey?: string }): Promise<{
  id: string;
  pubkey: string;
  created_at: number;
  tags: string[][];
  content: string;
}> {
  const pubkey = (args.pubkey ?? "").toLowerCase();
  if (!pubkey) throw new Error("get_contact_list needs a pubkey.");
  const events = await relayEvents({
    kinds: [KIND_CONTACT_LIST],
    authors: [pubkey],
    limit: 1,
  });
  const newest = events.reduce<RelayEvent | null>(
    (best, event) => (!best || event.created_at > best.created_at ? event : best),
    null,
  );
  return {
    id: newest?.id ?? "",
    pubkey,
    created_at: newest?.created_at ?? 0,
    tags: newest?.tags ?? [],
    content: newest?.content ?? "",
  };
}

/**
 * `set_contact_list` — publish this identity's follow list.
 *
 * NIP-02's tag is `["p", pubkey, relay_url, petname]`, positional: a petname
 * with no relay still needs the relay slot present, or the petname would be read
 * as a relay URL. Empty strings hold the position.
 */
export async function setContactList(
  identity: RelayIdentity,
  args: {
    contacts?: Array<{ pubkey?: string; relay_url?: string | null; petname?: string | null }>;
  },
): Promise<{ event_id: string; accepted: boolean; message: string }> {
  const tags: string[][] = [];
  for (const contact of args.contacts ?? []) {
    const pubkey = (contact.pubkey ?? "").toLowerCase();
    if (!/^[0-9a-f]{64}$/.test(pubkey)) continue;
    tags.push(["p", pubkey, contact.relay_url ?? "", contact.petname ?? ""]);
  }
  const signed = (await identity.sign({
    kind: KIND_CONTACT_LIST,
    content: "",
    tags,
  })) as unknown as { id: string };
  await publishSigned(signed, "Could not save your contact list.");
  return { event_id: signed.id, accepted: true, message: "" };
}

/** buzz's forum kinds (`KIND_FORUM_POST` / `KIND_FORUM_COMMENT` on the relay). */
const KIND_FORUM_POST = 45001;

/** One row of the client's `RawForumPost`. */
type RawForumPost = {
  event_id: string;
  pubkey: string;
  content: string;
  kind: number;
  created_at: number;
  channel_id: string;
  tags: string[][];
  sig: string;
  thread_summary: {
    reply_count: number;
    descendant_count: number;
    last_reply_at: number | null;
    participants: string[];
  } | null;
  reactions: unknown;
};

/** How many window pages one forum read will walk before answering. */
const FORUM_MAX_PAGES = 5;

/** `POST /buzz/api/window`'s answer, as far as the forum index reads it. */
type ForumWindowReply = {
  events?: Array<{
    event?: RelayEvent & { sig?: string };
    thread?: {
      reply_count: number;
      descendant_count: number;
      last_reply_at: number | null;
    } | null;
  }>;
  bounds?: { has_more?: boolean; next_cursor?: string | null };
};

type ForumWindowPost = (
  path: "/window",
  body: { channel: string; limit: number; cursor?: string },
) => Promise<ForumWindowReply>;

async function relayWindowPost(
  path: "/window",
  body: { channel: string; limit: number; cursor?: string },
): Promise<ForumWindowReply> {
  const { apiPost } = await import("@/shared/lib/api");
  return apiPost<ForumWindowReply>(path, body);
}

/**
 * Timestamp cursor → the relay cursor to resume from.
 *
 * This API's cursor is a TIMESTAMP and the relay's is an opaque index key, so a
 * timestamp alone cannot say "resume where the last page stopped". Without a
 * bridge every call restarts at the newest page and filters forward, which stops
 * working entirely once the cutoff is older than the walk's own page cap: every
 * row is filtered out, the page is empty, and there is no timestamp left to hand
 * back — so the caller is told the channel is exhausted and everything older
 * becomes unreachable.
 *
 * So each answer records where it stopped, under the timestamp it returned. A
 * call carrying that timestamp resumes exactly. A MISS still stores the mapping,
 * so at most one call is degraded before paging becomes exact — which is what
 * makes progress guaranteed rather than merely likely.
 *
 * Keyed by CHANNEL and timestamp together: a relay cursor is a position in one
 * channel's index, and timestamps are not unique across channels, so a
 * timestamp-only key lets two forum channels overwrite each other's entry and
 * resume from a cursor built for the wrong room.
 *
 * Bounded and per-tab: a paging aid, not state anything depends on.
 */
const FORUM_RESUME = new Map<string, string>();
const FORUM_RESUME_MAX = 64;

/** One key, built in one place, so the store and the lookup cannot disagree. */
function resumeKey(channelId: string, at: number): string {
  return `${channelId}\u0000${at}`;
}

function rememberResume(channelId: string, at: number, cursor: string): void {
  if (FORUM_RESUME.size >= FORUM_RESUME_MAX) {
    const oldest = FORUM_RESUME.keys().next().value;
    if (oldest !== undefined) FORUM_RESUME.delete(oldest);
  }
  FORUM_RESUME.set(resumeKey(channelId, at), cursor);
}

/**
 * `get_forum_posts` — a forum channel's index: roots, newest first, each with
 * its reply counts.
 *
 * Built on `POST /buzz/api/window` rather than a fresh query, and that is the
 * substantive choice here: the window route already returns top-level rows WITH
 * a per-row thread summary, which is exactly what a forum index is. Re-deriving
 * reply counts client-side would be a second opinion about how many replies a
 * post has, and the two would drift.
 *
 * Two gaps carried through honestly rather than invented:
 *
 * * `participants` is empty. The window route reports counts, not who replied,
 *   and drawing no avatars beats drawing the wrong people.
 * * `reactions` is null. The window route carries no tallies; the client reads
 *   them per target through `POST /buzz/api/reactions`.
 *
 * Paging bridges this API's timestamp cursor to the relay's opaque one (see
 * [`FORUM_RESUME`]), and `next_cursor` is null ONLY when the relay reported no
 * more — never because this walk's own budget ran out.
 */
export async function getForumPosts(
  args: {
    channelId?: string;
    limit?: number | null;
    before?: number | null;
  },
  post: ForumWindowPost = relayWindowPost,
): Promise<{ messages: RawForumPost[]; next_cursor: number | null }> {
  const channelId = args.channelId ?? "";
  if (!channelId) throw new Error("get_forum_posts needs a channelId.");
  const limit = Math.min(Math.max(args.limit ?? 50, 1), 200);
  const before = typeof args.before === "number" ? args.before : null;

  // An exact resume, when the previous page's stopping point is known. The
  // timestamp filter is then unnecessary — and must NOT be applied, because the
  // relay is already positioned past everything that was returned.
  const resumeFrom =
    before === null ? null : FORUM_RESUME.get(resumeKey(channelId, before)) ?? null;
  const cutoff = resumeFrom === null ? before : null;

  const messages: RawForumPost[] = [];
  let cursor: string | null = resumeFrom;
  let exhausted = false;
  // The oldest row SEEN, not the oldest returned: a page whose rows were all
  // filtered out must still advance, or the next call re-walks the same ground.
  let oldestSeen: number | null = null;

  for (let page = 0; page < FORUM_MAX_PAGES && messages.length < limit; page += 1) {
    // Ask for exactly the capacity left. The relay's cursor moves past a
    // WHOLE page, so a page with more rows than this answer can take would
    // strand the rest: the next call resumes after them. A page no larger
    // than the remaining capacity is always consumed to its end.
    const reply = await post("/window", {
      channel: channelId,
      limit: limit - messages.length,
      ...(cursor ? { cursor } : {}),
    });

    for (const row of reply.events ?? []) {
      const event = row.event;
      if (!event) continue;
      oldestSeen = event.created_at;
      if (event.kind !== KIND_FORUM_POST) continue;
      if (cutoff !== null && event.created_at >= cutoff) continue;
      messages.push({
        event_id: event.id,
        pubkey: event.pubkey,
        content: event.content ?? "",
        kind: event.kind,
        created_at: event.created_at,
        channel_id: channelId,
        tags: event.tags ?? [],
        sig: event.sig ?? "",
        thread_summary: row.thread
          ? { ...row.thread, participants: [] }
          : null,
        reactions: null,
      });
      // No early break: every row of a page is consumed, because the cursor
      // below moves past all of them.
    }

    const next = reply.bounds?.next_cursor ?? null;
    if (reply.bounds?.has_more !== true || !next) {
      exhausted = true;
      break;
    }
    cursor = next;
  }

  // Null ONLY on real exhaustion, as the relay reported it. A bounded walk that
  // answers null because its own budget ran out is the silent truncation this
  // codebase removes wherever it appears.
  if (exhausted || oldestSeen === null) {
    return { messages, next_cursor: null };
  }
  // Hand back a timestamp, and remember the relay position it stands for, so the
  // next call resumes exactly — including after a page that returned nothing.
  if (cursor) rememberResume(channelId, oldestSeen, cursor);
  return { messages, next_cursor: oldestSeen };
}

/**
 * Where this tab's relay lives, for the media gate below. Handed in by the
 * dispatch table, which owns `relayHttpUrl()`; `pageUrl` is what a relative
 * media URL (the relay answers uploads with `/buzz/blob/<sha256>.<ext>`)
 * resolves against, exactly as `fetch` and `<img>` resolve it.
 */
export type RelayLocation = { httpUrl: string; pageUrl: string };

/**
 * The relay media URL `url` names, or null when it names anything else.
 *
 * The desktop's download commands are gated by the Rust `validate_download_url`
 * SSRF check, which accepts only the relay's own media origin and path. Without
 * the same check these browser twins would fetch whatever a message's imeta or
 * image `src` says, then blob-ify it so a cross-origin `download` name is
 * honoured. So a message could save a third-party payload (or a `data:` URL) under a name of the
 * sender's choosing, presented as a workspace attachment, or make the tab issue
 * authenticated same-origin GETs. Only http(s) URLs on the relay's origin under
 * its media routes pass: `<base>/blob/` (this relay's content route) and
 * `/media/` (the upstream relay shape `isRelayDownloadable` checks). The URL
 * parser has already collapsed `..` segments, so a prefix check is exact.
 */
export function relayMediaUrl(url: string, relay: RelayLocation): URL | null {
  let parsed: URL;
  let relayBase: URL;
  try {
    parsed = new URL(url, relay.pageUrl);
    relayBase = new URL(relay.httpUrl, relay.pageUrl);
  } catch {
    return null;
  }
  if (parsed.protocol !== "https:" && parsed.protocol !== "http:") return null;
  if (parsed.origin !== relayBase.origin) return null;
  if (parsed.username || parsed.password) return null;
  const basePath = relayBase.pathname.replace(/\/api\/?$/, "").replace(/\/+$/, "");
  const prefixes = [`${basePath}/blob/`, "/media/"];
  return prefixes.some((prefix) => parsed.pathname.startsWith(prefix))
    ? parsed
    : null;
}

/**
 * A filename safe to suggest for a relay media download.
 *
 * The name comes from the sender's imeta, so path separators, control and
 * bidi/format characters (an RTL override turns `exe.pdf` around on screen)
 * are removed. The relay's URL carries the extension of the type it accepted,
 * and that extension decides how the saved file opens; when the suggested
 * name ends differently (`report.pdf.exe` for a `.pdf` object), the relay's
 * extension is appended rather than trusting the sender's.
 */
export function safeDownloadName(filename: string | undefined, media: URL): string {
  const last = media.pathname.split("/").pop() ?? "";
  const extMatch = /\.([a-z0-9]{1,10})$/i.exec(last);
  const relayExt = extMatch ? extMatch[1].toLowerCase() : null;
  const cleaned = (filename ?? "")
    .replace(/[\u0000-\u001f\u007f\u200b-\u200f\u202a-\u202e\u2060-\u2069\ufeff]/g, "")
    .split(/[\\/]/)
    .pop()!
    .replace(/^[.\s]+/, "")
    .trim()
    .slice(0, 200);
  const name = cleaned || last || "download";
  if (!relayExt) return name;
  const nameExt = /\.([^.]+)$/.exec(name)?.[1]?.toLowerCase();
  return nameExt === relayExt ? name : `${name}.${relayExt}`;
}

function requireRelayMedia(url: string, relay: RelayLocation, command: string): URL {
  const media = relayMediaUrl(url, relay);
  if (!media) {
    throw new Error(
      `${command} only saves files stored on this workspace's relay; that link points somewhere else.`,
    );
  }
  return media;
}

/**
 * Hand bytes to the browser's own download.
 *
 * An anchor with `download` is what the desktop's Rust command is a substitute
 * for, not the other way round: it needs Rust because it writes to a real
 * filesystem, while a tab already has a save dialog. The bytes are fetched into
 * a blob first so the filename survives — a bare `download` attribute is ignored
 * cross-origin, which would silently NAVIGATE to the media instead of saving it.
 * Callers pass a URL that `relayMediaUrl` already accepted.
 */
async function saveToDisk(media: URL, filename?: string): Promise<null> {
  const response = await fetch(media.href);
  if (!response.ok) {
    throw new Error(`That file could not be fetched (http ${response.status}).`);
  }
  const blob = await response.blob();
  const href = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = href;
  anchor.download = safeDownloadName(filename, media);
  anchor.style.display = "none";
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  // Revoked on the next tick: revoking immediately races the click in Safari.
  window.setTimeout(() => URL.revokeObjectURL(href), 10_000);
  return null;
}

/** `download_file` — save an attachment. */
export async function downloadFile(
  args: {
    url?: string;
    filename?: string;
  },
  relay: RelayLocation,
): Promise<null> {
  if (!args.url) throw new Error("download_file needs a url.");
  return saveToDisk(requireRelayMedia(args.url, relay, "download_file"), args.filename);
}

/** `download_image` — the same, from the image context menu. */
export async function downloadImage(
  args: { url?: string },
  relay: RelayLocation,
): Promise<null> {
  if (!args.url) throw new Error("download_image needs a url.");
  return saveToDisk(requireRelayMedia(args.url, relay, "download_image"));
}

/**
 * `copy_image_to_clipboard` — put an image on the system clipboard.
 *
 * Converted to PNG through a canvas first. That is not tidiness: PNG is the one
 * image type browsers reliably accept in a `ClipboardItem`, and handing over a
 * JPEG is rejected by most of them — silently, which would present as a copy
 * button that does nothing. Held to the same relay-media gate as the downloads.
 */
export async function copyImageToClipboard(
  args: { url?: string },
  relay: RelayLocation,
): Promise<null> {
  if (!args.url) throw new Error("copy_image_to_clipboard needs a url.");
  const media = requireRelayMedia(args.url, relay, "copy_image_to_clipboard");
  if (!navigator.clipboard || typeof ClipboardItem === "undefined") {
    throw new Error("This browser cannot put an image on the clipboard.");
  }
  const response = await fetch(media.href);
  if (!response.ok) {
    throw new Error(`That image could not be fetched (http ${response.status}).`);
  }
  const source = await response.blob();
  const png =
    source.type === "image/png" ? source : await toPng(source);
  await navigator.clipboard.write([new ClipboardItem({ "image/png": png })]);
  return null;
}

/** Re-encode an image blob as PNG, which is what the clipboard accepts. */
async function toPng(source: Blob): Promise<Blob> {
  const bitmap = await createImageBitmap(source);
  const canvas = document.createElement("canvas");
  canvas.width = bitmap.width;
  canvas.height = bitmap.height;
  const context = canvas.getContext("2d");
  if (!context) throw new Error("This browser cannot re-encode that image.");
  context.drawImage(bitmap, 0, 0);
  bitmap.close();
  return new Promise<Blob>((resolve, reject) => {
    canvas.toBlob(
      (blob) =>
        blob ? resolve(blob) : reject(new Error("That image could not be converted.")),
      "image/png",
    );
  });
}
