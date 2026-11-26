/**
 * Node test-runner resolver for this app's Vite-shaped imports.
 *
 * The unit tests (`src/**​/*.test.mjs`) import their subjects as the app does:
 * `@/…` for the src root, and extensionless relative paths. Vite resolves
 * both; plain `node --test` resolves neither, so every such test died with
 * ERR_MODULE_NOT_FOUND before running a single assertion. This hook is the
 * missing piece:
 *
 *   node --import ./test-loader.mjs --test src/**​/*.test.mjs
 *
 * It maps `@/` onto `./src/` and retries extensionless specifiers as `.ts`,
 * `.tsx`, and directory `index` modules. Plain `.ts` needs no transpiler:
 * type stripping is native (Node ≥ 23.6). `.tsx` does — Node strips types
 * but cannot compile JSX, so every test whose subject imports a `.tsx`
 * module died with ERR_UNKNOWN_FILE_EXTENSION. The `load` hook below fills
 * that gap with `transformSync` from rolldown — NOT esbuild: this package is
 * on vite 8, the rolldown-based vite, where esbuild is only an optional peer
 * and is therefore not installed at all. Rolldown's oxc pipeline is what
 * compiles the app's TSX for real builds, so test and build JSX cannot
 * drift. It emits the automatic `react/jsx-runtime` form to match
 * `"jsx": "react-jsx"` in tsconfig.json, and inlines a source map so stack
 * traces point at real `.tsx` lines.
 *
 * Three smaller Vite-isms get the same treatment, because each one killed
 * real suites before their first assertion: `.json` imports gain the
 * `type: "json"` attribute Node insists on (the tests then run against the
 * real data, exactly as Vite serves it), side-effect `.css` imports become
 * empty modules, and image/font/audio asset imports become the dev-server
 * URL string Vite would have returned.
 *
 * It ALSO maps `@tauri-apps/*` onto the facades in `src/shared/tauri-web/`,
 * mirroring the `paths` entries in tsconfig.json and the `tauriWebAlias()`
 * plugin in vite.config.ts. All three must agree: once the alias landed, the
 * ported files went back to importing `@tauri-apps/*` verbatim from upstream,
 * and a resolver that knows only `@/` fails 221 suites on a package that is
 * deliberately not installed.
 */
import { createRequire, registerHooks, setSourceMapsSupport } from "node:module";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

// Map transformed-`.tsx` stack frames back onto the original source.
setSourceMapsSupport(true);

// Loaded on first `.tsx` import, not at startup: most test files exercise
// plain `.ts` subjects and should not pay for rolldown's native binding.
let transformSync;
function compileTsx(url) {
  transformSync ??= createRequire(import.meta.url)(
    "rolldown/experimental",
  ).transformSync;
  const filename = fileURLToPath(url);
  const { code, map, errors } = transformSync(
    filename,
    readFileSync(filename, "utf8"),
    { jsx: { runtime: "automatic" }, sourcemap: true },
  );
  if (errors?.length) {
    throw new Error(
      `TSX transform failed for ${filename}:\n${errors.map((e) => e.message ?? String(e)).join("\n")}`,
    );
  }
  const inlineMap = Buffer.from(JSON.stringify(map)).toString("base64");
  return `${code}\n//# sourceMappingURL=data:application/json;base64,${inlineMap}\n`;
}

const SRC = new URL("./src/", import.meta.url);
const ROOT = new URL("./", import.meta.url);
const TAURI_WEB = new URL("./src/shared/tauri-web/", import.meta.url);
// The bare-specifier aliases from tsconfig.json `paths` / vite `resolve.alias`
// (the OSS variants of upstream's protected-feature modules, and the two
// vendored manifests).
const BARE_ALIASES = new Map([
  ["@features-manifest", new URL("./preview-features.json", ROOT).href],
  ["@model-capabilities-manifest", new URL("./model-capabilities.json", ROOT).href],
  ["@protected-features", new URL("protectedFeatures/public.ts", SRC).href],
  ["@protected-feature-components", new URL("protectedFeatures/publicUi.ts", SRC).href],
]);

// Vite turns these imports into URL strings — or, with `?inline`, into data
// URIs of the real bytes. Mirror both shapes.
const ASSET_RE = /\.(png|jpe?g|gif|svg|webp|avif|ico|mp3|m4a|wav|woff2?|ttf)$/;
const ASSET_MIME = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  svg: "image/svg+xml",
  webp: "image/webp",
  avif: "image/avif",
  ico: "image/x-icon",
  mp3: "audio/mpeg",
  m4a: "audio/mp4",
  wav: "audio/wav",
  woff: "font/woff",
  woff2: "font/woff2",
  ttf: "font/ttf",
};

function assetSource(fileUrl, query) {
  if (query === "?inline") {
    const ext = fileUrl.slice(fileUrl.lastIndexOf(".") + 1).toLowerCase();
    const mime = ASSET_MIME[ext] ?? "application/octet-stream";
    const b64 = readFileSync(fileURLToPath(fileUrl)).toString("base64");
    return `export default "data:${mime};base64,${b64}";`;
  }
  const devPath = `/${fileUrl.slice(ROOT.href.length)}`;
  return `export default ${JSON.stringify(devPath)};`;
}

/** `@tauri-apps/api/core` -> `src/shared/tauri-web/api/core`; `@tauri-apps/plugin-x` -> `.../plugin-x`. */
function tauriFacade(specifier) {
  const rest = specifier.slice("@tauri-apps/".length);
  return rest ? new URL(rest, TAURI_WEB).href : null;
}
const RETRYABLE = new Set([
  "ERR_MODULE_NOT_FOUND",
  "ERR_UNSUPPORTED_DIR_IMPORT",
]);

/** Node refuses a JSON module without `type: "json"`; Vite never asks. */
function withJsonAttribute(resolved) {
  if (!resolved.url?.endsWith(".json") || resolved.importAttributes?.type) {
    return resolved;
  }
  return { ...resolved, importAttributes: { type: "json" } };
}

registerHooks({
  resolve(specifier, context, nextResolve) {
    const candidate = specifier.startsWith("@/")
      ? new URL(specifier.slice(2), SRC).href
      : BARE_ALIASES.has(specifier)
        ? BARE_ALIASES.get(specifier)
        : specifier.startsWith("@tauri-apps/")
        ? (tauriFacade(specifier) ?? specifier)
        : specifier === "emoji-mart"
          ? // Vite honors the package's `module` field (ESM); Node takes
            // `main` (CJS) whose parcel-mangled exports the lexer cannot
            // name, so `import { init }` dies. Pick the file Vite picks.
            "emoji-mart/dist/module.js"
          : specifier;
    try {
      return withJsonAttribute(nextResolve(candidate, context));
    } catch (error) {
      if (!RETRYABLE.has(error?.code)) throw error;
      for (const suffix of [".ts", ".tsx", "/index.ts", "/index.tsx"]) {
        try {
          return nextResolve(candidate + suffix, context);
        } catch {
          // Not this shape; try the next one.
        }
      }
      throw error;
    }
  },
  load(url, context, nextLoad) {
    if (!url.startsWith("file:")) return nextLoad(url, context);
    // Vite import queries (`?inline`, `?url`) ride along in the resolved URL.
    const queryAt = url.indexOf("?");
    const fileUrl = queryAt === -1 ? url : url.slice(0, queryAt);
    const query = queryAt === -1 ? "" : url.slice(queryAt);
    if (fileUrl.endsWith(".tsx")) {
      return {
        format: "module",
        source: compileTsx(fileUrl),
        shortCircuit: true,
      };
    }
    if (fileUrl.endsWith(".css")) {
      return { format: "module", source: "export default {};", shortCircuit: true };
    }
    if (ASSET_RE.test(fileUrl)) {
      return {
        format: "module",
        source: assetSource(fileUrl, query),
        shortCircuit: true,
      };
    }
    return nextLoad(url, context);
  },
});
