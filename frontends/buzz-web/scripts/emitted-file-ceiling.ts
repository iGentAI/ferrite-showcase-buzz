/**
 * Hold every file of the relay build under the size one api reply can carry.
 *
 * The buzz app serves this bundle from its api route (upstream's gateway
 * consults a content route only when no api route matched), and an api reply
 * carries at most 4 MiB of body (docs/building-on-upstream-core.md, "Route
 * precedence"). A larger file is refused at the publish and the browser sees a
 * 504, which left the page blank when the initial stylesheet reached 5 MiB of
 * inlined card textures.
 *
 * The bundle also has to stay pure UTF-8 text: the relay embeds it with
 * `include_bytes!` and the source upload skips a file that is not UTF-8, so
 * emitting the textures as PNG files would build locally and break the deploy.
 * The assets therefore stay inlined, and this plugin does three things instead:
 *
 * 1. It drops an inlined WOFF font that follows an inlined WOFF2 copy of the
 *    same face in one `src` list (the `@fontsource/jetbrains-mono` faces carry
 *    both). A browser takes the first format it supports, every browser that
 *    can run this bundle's ES2022 modules supports WOFF2, and so the fallback
 *    is never used; inlined, it still cost 150 KiB of every page load and of
 *    the embedded bundle, which the split below would otherwise grow.
 * 2. It splits an oversized stylesheet at top-level rule boundaries. The first
 *    parts become sibling files and the original file keeps the last part
 *    behind one `@import` per earlier part. An imported sheet's rules cascade
 *    at the position of its `@import`, so the result is the original rule
 *    sequence in the original order, and every reference to the original
 *    file name (index.html, the preload lists in the JS chunks) stays valid.
 * 3. After the build is written it measures every file in the output
 *    directory, public files included, and fails the build if one is over
 *    the ceiling or is not UTF-8. JS chunks are kept small by the
 *    `codeSplitting` group's `maxSize` in vite.config.ts; this check is what
 *    notices when that stops being enough.
 */
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import type { Plugin } from "vite";

/** 3.5 MiB: the 4 MiB reply bound less a margin for headers and envelope. */
export const EMITTED_FILE_CEILING = 3.5 * 1024 * 1024;

const byteLength = (text: string) => Buffer.byteLength(text, "utf8");

/** An inlined WOFF2 source followed by an inlined WOFF fallback for the same face. */
const WOFF_AFTER_WOFF2 =
  /(url\(\s*["']?data:font\/woff2[;,][^)]*\)\s*format\(\s*["']woff2["']\s*\))\s*,\s*url\(\s*["']?data:font\/woff[;,][^)]*\)\s*format\(\s*["']woff["']\s*\)/g;

export function dropWoffFallbacks(css: string): string {
  return css.replace(WOFF_AFTER_WOFF2, "$1");
}

/**
 * The top-level statements and blocks of a stylesheet, in order, such that
 * concatenating them reproduces the input. Strings, comments and unquoted
 * `url(...)` bodies are skipped so a brace inside them is not structure, and
 * a backslash escape is skipped whole so an escaped quote opens no string.
 */
export function topLevelRules(css: string): string[] {
  const rules: string[] = [];
  let depth = 0;
  let start = 0;
  let i = 0;
  while (i < css.length) {
    const ch = css[i];
    // An escape outside a string (Tailwind's `.content-\[\'\'\]`) is an
    // identifier character, never a quote or a brace.
    if (ch === "\\") {
      i += 2;
      continue;
    }
    if (ch === '"' || ch === "'") {
      i++;
      while (i < css.length && css[i] !== ch) i += css[i] === "\\" ? 2 : 1;
      i++;
      continue;
    }
    if (ch === "/" && css[i + 1] === "*") {
      const end = css.indexOf("*/", i + 2);
      i = end < 0 ? css.length : end + 2;
      continue;
    }
    if ((ch === "u" || ch === "U") && /^url\(\s*[^\s"']/i.test(css.slice(i, i + 6))) {
      while (i < css.length && css[i] !== ")") i += css[i] === "\\" ? 2 : 1;
      i++;
      continue;
    }
    if (ch === "{") depth++;
    else if (ch === "}") depth--;
    i++;
    if (depth === 0 && (ch === "}" || ch === ";")) {
      rules.push(css.slice(start, i));
      start = i;
    }
  }
  if (depth !== 0) throw new Error("unbalanced braces in stylesheet");
  if (start < css.length) rules.push(css.slice(start));
  return rules;
}

/** Greedily pack rules, in order, into parts of at most `limit` bytes. */
function pack(rules: string[], limit: number, fileName: string): string[] {
  const parts: string[] = [];
  let current = "";
  for (const rule of rules) {
    if (byteLength(rule) > limit) {
      throw new Error(
        `${fileName}: one top-level rule is ${byteLength(rule)} bytes, over the ` +
          `${limit}-byte ceiling; it cannot be split without changing the cascade`,
      );
    }
    if (current && byteLength(current) + byteLength(rule) > limit) {
      parts.push(current);
      current = "";
    }
    current += rule;
  }
  if (current) parts.push(current);
  return parts;
}

function* walk(dir: string): Generator<string> {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(full);
    else yield full;
  }
}

export function emittedFileCeiling(ceiling = EMITTED_FILE_CEILING): Plugin {
  let outDir = "";
  return {
    name: "buzz:emitted-file-ceiling",
    apply: "build",
    enforce: "post",
    configResolved(config) {
      outDir = path.resolve(config.root, config.build.outDir);
    },
    generateBundle: {
      order: "post",
      handler(_options, bundle) {
        for (const asset of Object.values(bundle)) {
          if (asset.type !== "asset" || !asset.fileName.endsWith(".css")) continue;
          const css = dropWoffFallbacks(
            typeof asset.source === "string"
              ? asset.source
              : new TextDecoder("utf-8", { fatal: true }).decode(asset.source),
          );
          asset.source = css;
          if (byteLength(css) <= ceiling) continue;
          const rules = topLevelRules(css);
          if (rules.some((rule) => /^\s*@(import|charset)\b/i.test(rule))) {
            throw new Error(
              `${asset.fileName}: an oversized stylesheet with @import or @charset ` +
                "cannot be split by prepending imports",
            );
          }
          // Leave room in the last part for the @import lines it will carry.
          const parts = pack(rules, ceiling - 4096, asset.fileName);
          const stem = asset.fileName.replace(/\.css$/, "");
          const imports = parts.slice(0, -1).map((part) => {
            const hash = createHash("sha256").update(part).digest("base64url").slice(0, 8);
            const fileName = `${stem}-${hash}.css`;
            this.emitFile({ type: "asset", fileName, source: part });
            return `@import url("./${path.posix.basename(fileName)}");`;
          });
          asset.source = imports.join("") + parts[parts.length - 1];
        }
      },
    },
    closeBundle() {
      const problems: string[] = [];
      for (const file of walk(outDir)) {
        const bytes = fs.readFileSync(file);
        const rel = path.relative(outDir, file);
        if (bytes.length > ceiling) {
          problems.push(`${rel} is ${bytes.length} bytes (ceiling ${ceiling})`);
        }
        try {
          new TextDecoder("utf-8", { fatal: true }).decode(bytes);
        } catch {
          problems.push(`${rel} is not UTF-8`);
        }
      }
      if (problems.length > 0) {
        throw new Error(
          "the relay bundle cannot be served from the api route:\n  " + problems.join("\n  "),
        );
      }
    },
  };
}
