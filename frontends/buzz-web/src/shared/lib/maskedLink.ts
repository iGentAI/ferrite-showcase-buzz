/**
 * A link is "masked" when its visible text is not the destination URL —
 * `[click here](https://example.com)` — so the reader can't tell where it
 * goes. Masked links get a hover tooltip showing the full destination
 * (anti-phishing, mirrors Slack). Bare pasted URLs and GFM autolinks are
 * not masked: their label already is the URL, modulo cosmetic differences
 * (omitted scheme, trailing slash, host case).
 *
 * Deliberately strict where it matters: scheme downgrades (label says
 * https, href is http), path/query case changes, and protocol-relative
 * hrefs all count as masked.
 *
 * The href is read the way a browser's URL parser would read it before any
 * comparison (F167): leading/trailing spaces and control characters are
 * dropped, tabs and newlines removed, and a backslash counts as `/`, so
 * `https:\\evil.example`, `https:/evil.example` and `\\evil.example` are all
 * recognized as links to evil.example. An href with a scheme this module does
 * not know is treated as masked (fail closed); only in-app or non-web schemes
 * (`buzz:`, `mailto:`, `tel:` and the other react-markdown-safe ones) and
 * same-origin relative paths are exempt.
 */
export function isMaskedLink(label: string, href: string): boolean {
  const trimmedLabel = label.trim();
  if (!trimmedLabel) return false;

  const destination = classifyHref(href);
  if (destination.kind === "exempt") {
    // A relative destination behind a label that names an external site is
    // still a disguise: `[https://bank.example](/channels/x)` must show where
    // it really goes (finding_882c1316f8e52778). Non-web schemes (mailto:,
    // tel:) keep their exemption.
    const hasScheme = /^[\u0000-\u0020]*[a-z][a-z0-9+.-]*:/i.test(href);
    return !hasScheme && labelLooksExternal(trimmedLabel);
  }
  if (destination.kind === "unknown") return true;
  const hrefUrl = destination.url;
  // Credentials in the destination (`https://bank.example@evil.example`) are
  // what a disguised link looks like; always show it (finding_551031929fc51ba9).
  if (hrefUrl.username || hrefUrl.password) return true;

  const labelUrl = parseExternal(trimmedLabel, { assumeScheme: true });
  if (!labelUrl) return true; // label isn't URL-shaped at all

  // A label with userinfo (`safe.com@evil.com`) visually leads with the
  // wrong host — always treat it as masked.
  if (labelUrl.username || labelUrl.password) return true;

  // If the label spells out a scheme, it must match the destination's —
  // otherwise [https://x](http://x) silently downgrades.
  const labelHasScheme = /^https?:\/\//i.test(trimmedLabel);
  if (labelHasScheme && labelUrl.protocol !== hrefUrl.protocol) return true;

  // Host comparison is case-insensitive (the URL parser lowercases it);
  // path/query/hash stay case-sensitive — many backends distinguish them.
  return (
    labelUrl.host !== hrefUrl.host ||
    normalizePath(labelUrl.pathname) !== normalizePath(hrefUrl.pathname) ||
    labelUrl.search !== hrefUrl.search ||
    labelUrl.hash !== hrefUrl.hash
  );
}

/** Schemes that are not web destinations and never get the tooltip. */
const EXEMPT_SCHEMES = new Set([
  "buzz",
  "mailto",
  "tel",
  "xmpp",
  "irc",
  "ircs",
]);

type HrefClass =
  | { kind: "exempt" }
  | { kind: "unknown" }
  | { kind: "external"; url: URL };

/**
 * Normalize an href the way the WHATWG URL parser does for special (web)
 * schemes and classify it. Any backslash becomes `/` (the parser treats them
 * alike for http/https and for relative references against a web page), so
 * every spelling the browser or the OS opener would take to another host is
 * read as that host.
 */
function classifyHref(href: string): HrefClass {
  const normalized = href
    // biome-ignore lint/suspicious/noControlCharactersInRegex: the URL parser strips these
    .replace(/^[\u0000-\u0020]+|[\u0000-\u0020]+$/g, "")
    .replace(/[\t\n\r]/g, "")
    .replace(/\\/g, "/");
  if (!normalized) return { kind: "exempt" };

  if (normalized.startsWith("//")) {
    return parsedOrUnknown(`https:${normalized}`);
  }

  const scheme = /^([a-z][a-z0-9+.-]*):/i.exec(normalized)?.[1]?.toLowerCase();
  if (scheme === undefined) {
    // A relative reference (`/channels/x`, `#top`, `notes`) stays on the
    // app's own origin.
    return { kind: "exempt" };
  }
  if (scheme === "http" || scheme === "https") {
    // Parsed without a base, `https:/host` and `https:host` resolve to
    // `https://host/`, the most external reading any consumer could take.
    return parsedOrUnknown(normalized);
  }
  if (EXEMPT_SCHEMES.has(scheme)) return { kind: "exempt" };
  return { kind: "unknown" };
}

function parsedOrUnknown(candidate: string): HrefClass {
  try {
    const url = new URL(candidate);
    if (url.protocol !== "http:" && url.protocol !== "https:") {
      return { kind: "unknown" };
    }
    return { kind: "external", url };
  } catch {
    return { kind: "unknown" };
  }
}

function parseExternal(
  value: string,
  { assumeScheme = false }: { assumeScheme?: boolean } = {},
): URL | null {
  let candidate = value;
  if (/^\/\//.test(candidate)) {
    candidate = `https:${candidate}`;
  } else if (assumeScheme && !/^https?:\/\//i.test(candidate)) {
    candidate = `https://${candidate}`;
  }
  if (!/^https?:\/\//i.test(candidate)) return null;
  try {
    return new URL(candidate);
  } catch {
    return null;
  }
}

function normalizePath(pathname: string): string {
  return pathname.length > 1 ? pathname.replace(/\/$/, "") : pathname;
}

/** Whether a label spells out an external web address. */
function labelLooksExternal(label: string): boolean {
  return /^(?:https?:)?\/\//i.test(label) || /^(?:www\.)?[a-z0-9-]+(?:\.[a-z0-9-]+)+(?:[/:?#]|$)/i.test(label);
}
