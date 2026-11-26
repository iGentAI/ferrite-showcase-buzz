import { mergeAttributes, Node, nodeInputRule } from "@tiptap/core";

import { rewriteRelayUrl } from "@/shared/lib/mediaUrl";
import { escapeRegExp } from "@/shared/lib/mentionPattern";

/**
 * Inline atom node for a custom emoji, modeled on Tiptap's mention/emoji
 * extensions. It renders the emoji image but behaves as a single selectable,
 * copyable, deletable unit — exactly like a built-in unicode emoji glyph —
 * unlike a decoration overlay, which can't be selected.
 *
 * Crucially it serializes to `:shortcode:` everywhere the rest of the app reads
 * the composer:
 *  - Markdown (tiptap-markdown `addStorage().markdown.serialize`) → `:shortcode:`
 *    so the send path (`buildCustomEmojiTags`/`splitOutgoingTags`) is untouched.
 *  - Plain text (`renderText`) → `:shortcode:` so `doc.textContent`,
 *    `doc.textBetween`, and the autocomplete plain-text projection see the
 *    shortcode at its natural width.
 *
 * An input rule converts a completed known `:shortcode:` into the node as the
 * user types or picks from the emoji menu. Unknown `:foo:` sequences stay plain
 * text (a user mid-typing `:par` shouldn't flicker into a node).
 */

export const CUSTOM_EMOJI_NODE_NAME = "customEmoji";

export interface CustomEmojiNodeOptions {
  /** Resolve a (lowercased) shortcode to its image URL. */
  resolveUrl: (shortcode: string) => string | undefined;
  /** All known shortcodes, used to build the input-rule pattern. */
  shortcodes: () => string[];
}

/** NIP-30 shortcode characters, as the relay and `shared/api/customEmoji` validate them. */
const SHORTCODE_RE = /^[a-z0-9_-]+$/i;

/**
 * The lowercase shortcode when `value` is a well-formed shortcode the community
 * palette actually holds, otherwise null.
 *
 * Pasted HTML reaches the schema's `parseHTML` directly, so an
 * `<img data-custom-emoji>` from a page the user copied could otherwise carry
 * any `data-shortcode` (newlines and Markdown included, written verbatim into
 * the sent message) and any `src` (fetched the moment the paste lands). Only a
 * known shortcode becomes a node, and its image always comes from the palette.
 */
export function knownCustomEmojiShortcode(
  value: unknown,
  shortcodes: readonly string[],
): string | null {
  if (typeof value !== "string" || !SHORTCODE_RE.test(value)) return null;
  const shortcode = value.toLowerCase();
  return shortcodes.some((known) => known.toLowerCase() === shortcode)
    ? shortcode
    : null;
}

function pastedShortcode(element: HTMLElement): string {
  const fromData = element.getAttribute("data-shortcode");
  if (fromData) return fromData;
  // Timeline-rendered emoji (markdown.tsx) carry no data-shortcode; recover it
  // from the `:shortcode:` alt so copy-from-timeline → paste-into-composer
  // still produces a proper node.
  const alt = element.getAttribute("alt") ?? "";
  return /^:([^:\s]+):$/.exec(alt)?.[1] ?? "";
}

/** The serialized form of a node's shortcode; malformed values write nothing. */
function serializedShortcode(value: unknown): string {
  return typeof value === "string" && SHORTCODE_RE.test(value)
    ? `:${value}:`
    : "";
}

/**
 * Build a case-insensitive regex matching a completed `:shortcode:` for any
 * known shortcode, longest-first so a longer name isn't shadowed by a shorter
 * prefix. Returns null when there are no known shortcodes. Exported for testing
 * and reused by the input rule.
 */
export function buildKnownShortcodeAlternation(
  shortcodes: string[],
): string | null {
  const sorted = [...new Set(shortcodes)]
    .filter((s) => s.trim().length > 0)
    .sort((a, b) => b.length - a.length);
  if (sorted.length === 0) return null;
  return sorted.map((s) => escapeRegExp(s)).join("|");
}

/**
 * Register a markdown-it inline rule that turns a known `:shortcode:` into an
 * `<img data-custom-emoji ...>` (the same shape `renderHTML` emits). Used by the
 * node's markdown `parse.setup` so loading content via `setContent` — editing
 * an existing message — materializes custom-emoji nodes instead of leaving raw
 * `:shortcode:` text. Unknown shortcodes are left untouched (rendered as the
 * literal text they are).
 *
 * The rule and renderer are self-contained: matching uses the *current* known
 * set (read lazily on each parse), and the token carries the resolved url so
 * the renderer needs no further lookup.
 */
export function registerCustomEmojiMarkdownIt(
  // biome-ignore lint/suspicious/noExplicitAny: markdown-it is untyped here
  md: any,
  options: CustomEmojiNodeOptions,
): void {
  const RULE_NAME = "buzz_custom_emoji";
  const TOKEN_TYPE = "buzz_custom_emoji";

  // `parse.setup` runs on every parse against the *same* markdown-it instance,
  // so only register the rule + renderer once — `ruler.before` throws on a
  // duplicate rule name.
  if (md.renderer.rules[TOKEN_TYPE]) return;

  // biome-ignore lint/suspicious/noExplicitAny: markdown-it state/silent
  const rule = (state: any, silent: boolean): boolean => {
    // Fast bail: a shortcode must start with `:`.
    if (state.src.charCodeAt(state.pos) !== 0x3a /* : */) return false;

    // Word-boundary guard: don't fire when the `:` is glued to a preceding
    // word char. This keeps prose and URLs intact — `not:buzz:` and
    // `http://x:y:buzz:` must NOT turn the inner `:buzz:` into an image;
    // only a `:shortcode:` at a boundary (start of line, after whitespace or
    // punctuation) materializes. Slack-style boundary semantics.
    if (state.pos > 0) {
      const prev = state.src.charCodeAt(state.pos - 1);
      const isWordChar =
        (prev >= 0x30 && prev <= 0x39) /* 0-9 */ ||
        (prev >= 0x41 && prev <= 0x5a) /* A-Z */ ||
        (prev >= 0x61 && prev <= 0x7a) /* a-z */ ||
        prev === 0x5f; /* _ */
      if (isWordChar) return false;
    }

    const alternation = buildKnownShortcodeAlternation(options.shortcodes());
    if (!alternation) return false;

    // Match a known `:shortcode:` anchored at the current position. `y` (sticky)
    // anchors at lastIndex without `^`, so we don't accidentally match later in
    // the line.
    const re = new RegExp(`:(?:${alternation}):`, "iy");
    re.lastIndex = state.pos;
    const match = re.exec(state.src);
    if (!match) return false;

    if (!silent) {
      const matched = match[0];
      const shortcode = matched.slice(1, -1).toLowerCase();
      const token = state.push(TOKEN_TYPE, "img", 0);
      token.meta = { shortcode, src: options.resolveUrl(shortcode) ?? "" };
    }
    state.pos += match[0].length;
    return true;
  };

  // Run before markdown-it's own `:` handling (emphasis etc. are unaffected;
  // there is no built-in inline rule named "text" collision here). Inserting
  // before "emphasis" is safe and early enough.
  md.inline.ruler.before("emphasis", RULE_NAME, rule);

  // biome-ignore lint/suspicious/noExplicitAny: markdown-it token
  md.renderer.rules[TOKEN_TYPE] = (tokens: any[], idx: number): string => {
    const { shortcode, src } = tokens[idx].meta as {
      shortcode: string;
      src: string;
    };
    const esc = md.utils.escapeHtml;
    // Mirror renderHTML(): the resolved url is rewritten through the media
    // proxy at PM-render time, so here we emit the raw `src`; `parseHTML`
    // re-derives the node from `data-shortcode` and the palette supplies the
    // live url. We still set `src` so a fully-formed <img> round-trips cleanly.
    return `<img data-custom-emoji data-shortcode="${esc(shortcode)}" src="${esc(src)}" alt=":${esc(shortcode)}:" title=":${esc(shortcode)}:" />`;
  };
}

export const CustomEmojiNode = Node.create<CustomEmojiNodeOptions>({
  name: CUSTOM_EMOJI_NODE_NAME,

  // Inline, atomic (selected/deleted as one unit), and a leaf (no content) —
  // the same shape as the built-in emoji "glyph" the user expects to mimic.
  group: "inline",
  inline: true,
  atom: true,
  selectable: true,

  addOptions() {
    return {
      resolveUrl: () => undefined,
      shortcodes: () => [],
    };
  },

  addAttributes() {
    const options = this.options;
    const shortcodeOf = (el: HTMLElement) =>
      knownCustomEmojiShortcode(pastedShortcode(el), options.shortcodes()) ?? "";
    return {
      shortcode: {
        default: "",
        parseHTML: (el) => shortcodeOf(el as HTMLElement),
        renderHTML: (attrs) => ({ "data-shortcode": attrs.shortcode }),
      },
      src: {
        default: "",
        // Never the pasted `src`: the image URL is always re-resolved from the
        // community palette, so pasted HTML cannot make the composer load an
        // image from an origin of its choosing.
        parseHTML: (el) => {
          const shortcode = shortcodeOf(el as HTMLElement);
          return shortcode ? (options.resolveUrl(shortcode) ?? "") : "";
        },
        // `src` is derived from the community palette at render time, not
        // persisted in serialized output — see renderText/markdown below.
        renderHTML: () => ({}),
      },
    };
  },

  parseHTML() {
    const options = this.options;
    return [
      {
        tag: "img[data-custom-emoji]",
        // An unknown or malformed shortcode never becomes a node.
        getAttrs: (element) =>
          typeof element !== "string" &&
          knownCustomEmojiShortcode(
            pastedShortcode(element),
            options.shortcodes(),
          )
            ? null
            : false,
      },
    ];
  },

  renderHTML({ node, HTMLAttributes }) {
    const shortcode = String(node.attrs.shortcode ?? "");
    // Palette only: a node restored from JSON or built elsewhere may carry any
    // `src`, so an unresolvable shortcode renders with no image URL at all.
    const rawSrc = this.options.resolveUrl(shortcode.toLowerCase()) ?? "";
    const src = rawSrc ? rewriteRelayUrl(rawSrc) : rawSrc;
    return [
      "img",
      mergeAttributes(HTMLAttributes, {
        src,
        alt: `:${shortcode}:`,
        title: `:${shortcode}:`,
        "data-custom-emoji": "",
        "data-shortcode": shortcode,
        draggable: "false",
        // Match the message-view <img data-custom-emoji> sizing exactly.
        class: "mx-px inline-block h-[1.25em] w-auto max-w-none align-middle",
      }),
    ];
  },

  // Plain-text projection of the node: the literal shortcode. Powers
  // `doc.textContent` / `doc.textBetween` (and our autocomplete projection),
  // so cursor math treats `:shortcode:` at its real width.
  renderText({ node }) {
    return serializedShortcode(node.attrs.shortcode);
  },

  addStorage() {
    return {
      markdown: {
        serialize(
          // biome-ignore lint/suspicious/noExplicitAny: prosemirror-markdown state is untyped here
          state: any,
          // biome-ignore lint/suspicious/noExplicitAny: PM node
          node: any,
        ) {
          state.write(serializedShortcode(node.attrs.shortcode));
        },
        // Parse loaded markdown (e.g. editing an existing message via
        // `setContent`) so a known `:shortcode:` becomes the atom node instead
        // of staying plain text. Input rules only fire on live keystrokes, so
        // without this an edited message shows the raw `:shortcode:`.
        //
        // We add a markdown-it inline rule that emits an `<img data-custom-emoji>`
        // — the same shape `renderHTML` produces — which the node's `parseHTML`
        // (`img[data-custom-emoji]`) then materializes. Using our own token +
        // renderer sidesteps the `html: false` gate (that only blocks raw HTML
        // the *user* typed, not tokens we synthesize).
        parse: {
          // biome-ignore lint/suspicious/noExplicitAny: markdown-it is untyped here
          setup(this: { options: CustomEmojiNodeOptions }, md: any) {
            registerCustomEmojiMarkdownIt(md, this.options);
          },
        },
      },
    };
  },

  addInputRules() {
    const options = this.options;
    return [
      nodeInputRule({
        // Lazily resolve the pattern from the *current* known set on each
        // keystroke. An empty set → a regex that can never match.
        find: (text: string) => {
          const alternation = buildKnownShortcodeAlternation(
            options.shortcodes(),
          );
          if (!alternation) return null;
          // Match a completed `:shortcode:` ending at the input position.
          const re = new RegExp(`(:(?:${alternation}):)$`, "i");
          const match = re.exec(text);
          if (!match) return null;
          return {
            index: match.index,
            text: match[0],
            replaceWith: match[1],
            match,
            data: undefined,
          };
        },
        type: this.type,
        getAttributes: (match) => {
          const matched = match[1] ?? "";
          const shortcode = matched.slice(1, -1).toLowerCase();
          return {
            shortcode,
            src: options.resolveUrl(shortcode) ?? "",
          };
        },
      }),
    ];
  },
});
