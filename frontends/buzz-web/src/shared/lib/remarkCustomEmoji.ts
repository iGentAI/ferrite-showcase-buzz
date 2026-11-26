/**
 * Remark plugin that detects custom-emoji shortcodes (`:shortcode:`) in text
 * nodes and replaces them with custom HAST `emoji` elements rendered as inline
 * images via react-markdown.
 *
 * Only *known* shortcodes (those present in the provided `customEmoji` map) are
 * matched. Unknown `:foo:` sequences are left as plain text — native unicode
 * emoji are inserted directly by the picker and never pass through here.
 *
 * Mirrors the `remarkMentions` / `remarkChannelLinks` shape but cannot reuse
 * `createRemarkPrefixPlugin`: that factory keys nodes off the matched *text*,
 * whereas here we must carry the resolved image `url` (a side value) onto the
 * node, so the renderer doesn't need the map again.
 */

export type CustomEmoji = {
  shortcode: string;
  url: string;
};

type RemarkCustomEmojiOptions = {
  customEmoji?: CustomEmoji[];
};

// biome-ignore lint/suspicious/noExplicitAny: building mdast-compatible nodes
type Node = { [key: string]: any };

/**
 * Whether a custom-emoji image URL may be rendered as an `<img src>`.
 *
 * The URL comes straight from a NIP-30 `["emoji", shortcode, url]` tag, which
 * any member (or any message author) chooses. It reaches the DOM through a
 * custom `emoji` element, so react-markdown's `urlTransform` never sees it:
 * this check is the only scheme gate on that path. Accepted: absolute
 * `http:`/`https:` URLs, and root-relative paths on the relay's own origin
 * (the Ferrite relay stores uploads as `/buzz/blob/<sha256>.<ext>`). Refused:
 * every other scheme (`data:`, `file:`, `blob:`, webview schemes),
 * scheme-relative `//host` and `/\host` forms, and any URL carrying
 * whitespace, control characters or a backslash, which the URL parser
 * strips or reinterprets.
 */
export function isSafeCustomEmojiUrl(url: unknown): url is string {
  if (typeof url !== "string" || url.length === 0) return false;
  if (url.length > MAX_CUSTOM_EMOJI_URL_LENGTH) return false;
  // biome-ignore lint/suspicious/noControlCharactersInRegex: rejecting them is the point
  if (/[\u0000-\u0020\u007f\\]/.test(url)) return false;
  if (url.startsWith("/")) return !url.startsWith("//");
  try {
    const parsed = new URL(url);
    return parsed.protocol === "http:" || parsed.protocol === "https:";
  } catch {
    return false;
  }
}

/** Longest custom-emoji URL accepted from a tag. */
export const MAX_CUSTOM_EMOJI_URL_LENGTH = 2048;

/**
 * One generic `:shortcode:` scanner. Known shortcodes are resolved by map
 * lookup instead of being compiled into an alternation: the shortcodes come
 * from other members' events, and a regex built from thousands of them would
 * be compiled and run on every render (F162). Shortcode characters never
 * include `:`, so at any position the generic match is exactly the one a
 * known shortcode would produce. Case-insensitive: the set keys are
 * lowercase, but message content may use mixed case.
 */
const SHORTCODE_SCAN_SOURCE = ":([a-z0-9_-]{1,64}):";

export default function remarkCustomEmoji(options?: RemarkCustomEmojiOptions) {
  const emoji = options?.customEmoji ?? [];
  const urlByShortcode = new Map<string, string>();
  for (const e of emoji) {
    const shortcode = e.shortcode.trim().toLowerCase();
    if (shortcode.length === 0 || urlByShortcode.has(shortcode)) continue;
    urlByShortcode.set(shortcode, e.url);
  }

  return (
    // biome-ignore lint/suspicious/noExplicitAny: remark tree types are not available
    tree: any,
  ) => {
    if (urlByShortcode.size === 0) return;
    walkChildren(tree, new RegExp(SHORTCODE_SCAN_SOURCE, "gi"), urlByShortcode);
  };
}

function walkChildren(
  // biome-ignore lint/suspicious/noExplicitAny: remark tree types are not available
  node: any,
  pattern: RegExp,
  urlByShortcode: Map<string, string>,
) {
  if (
    !node?.children ||
    !Array.isArray(node.children) ||
    shouldSkipNode(node)
  ) {
    return;
  }

  for (let i = node.children.length - 1; i >= 0; i--) {
    const child = node.children[i];
    if (child.type === "text") {
      const parts = splitByPattern(child.value, pattern, urlByShortcode);
      if (
        parts.length > 1 ||
        (parts.length === 1 && parts[0].type !== "text")
      ) {
        node.children.splice(i, 1, ...parts);
      }
    } else {
      walkChildren(child, pattern, urlByShortcode);
    }
  }
}

// biome-ignore lint/suspicious/noExplicitAny: remark tree types are not available
function shouldSkipNode(node: any): boolean {
  return (
    node.type === "link" || node.type === "code" || node.type === "inlineCode"
  );
}

function splitByPattern(
  text: string,
  pattern: RegExp,
  urlByShortcode: Map<string, string>,
): Node[] {
  pattern.lastIndex = 0;
  const parts: Node[] = [];
  let lastIndex = 0;
  let match: RegExpExecArray | null = null;

  while (true) {
    match = pattern.exec(text);
    if (!match) break;

    const matchText = match[0]; // e.g. ":party_parrot:" or ":Party_Parrot:"
    const shortcode = matchText.slice(1, -1).toLowerCase();
    const url = urlByShortcode.get(shortcode);
    if (!url || !isSafeCustomEmojiUrl(url)) {
      // Unknown shortcode (or one whose URL is refused): keep it as text, and
      // retry from the closing colon so `:unknown:known:` still renders
      // `known`, exactly as a known-only alternation would.
      pattern.lastIndex = match.index + matchText.length - 1;
      continue;
    }

    if (match.index > lastIndex) {
      parts.push({ type: "text", value: text.slice(lastIndex, match.index) });
    }
    parts.push(buildEmojiNode(shortcode, url));
    lastIndex = match.index + matchText.length;
  }

  if (parts.length === 0) {
    return [{ type: "text", value: text }];
  }
  if (lastIndex < text.length) {
    parts.push({ type: "text", value: text.slice(lastIndex) });
  }
  return parts;
}

function buildEmojiNode(shortcode: string, url: string): Node {
  return {
    type: "emoji",
    value: `:${shortcode}:`,
    data: {
      hName: "emoji",
      hProperties: {
        src: url,
        alt: `:${shortcode}:`,
        "data-shortcode": shortcode,
      },
    },
  };
}
