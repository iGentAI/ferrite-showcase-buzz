/**
 * `tauri-web-alias` — the Vite half of the `@tauri-apps/*` seam.
 *
 * buzz's client imports `@tauri-apps/api/core`, `@tauri-apps/plugin-opener` and
 * nine more Tauri modules. None of them is installed here; `src/shared/tauri-web/`
 * holds one facade per module, each re-exporting the browser stand-in from
 * `src/shared/lib/desktop-bridge.ts`. This plugin points the specifier at the
 * facade so upstream's own import lines resolve UNCHANGED, which keeps those
 * files byte-identical to upstream.
 *
 * The TypeScript half is `compilerOptions.paths` in `tsconfig.json`, plus the
 * shorthand ambient wildcard in `src/shared/tauri-web/tauri-apps.d.ts`. Both
 * halves must agree; there is no list of modules in either, only a path rule.
 *
 * THE LOUD-FAILURE PROPERTY. A stray `@tauri-apps` import must fail by name:
 *
 *   - a module WITH a facade but a symbol without one still fails `tsc`, by
 *     name, because the facades re-export explicit lists and never `export *`;
 *   - a module with NO facade resolves to a generated stand-in whose every
 *     statically named export throws `TauriModuleUnavailableError` naming the
 *     module AND the symbol, at the call site — strictly more information than
 *     "failed to resolve import", and it does not block the rest of an upstream
 *     sync;
 *   - a module with NO facade that is reached through `import * as x` or a
 *     dynamic `import()` is a fatal build error, strict or not: those names are
 *     read off the module namespace at runtime, where the stand-in cannot give
 *     them a throwing binding, so they would be a silent `undefined`;
 *   - every unimplemented module is reported at `buildStart` as a `[tauri-web]`
 *     warning listing its symbols and the files that want them, and
 *     `BUZZ_TAURI_STRICT=1` promotes those warnings to a fatal build error.
 */
import fs from "node:fs";
import path from "node:path";
import type { Plugin } from "vite";

const SPECIFIER_PREFIX = "@tauri-apps/";

/** Rollup convention: a leading NUL marks an id no other plugin should touch. */
const VIRTUAL_PREFIX = "\0tauri-web-unavailable:";

/** Where the facades live, relative to the Vite root. */
const FACADE_DIR = ["src", "shared", "tauri-web"];

const SOURCE_EXTENSIONS = new Set([".ts", ".tsx", ".js", ".jsx", ".mjs"]);

/** One Tauri module's demand: which symbols, wanted by which files. */
type Demand = { symbols: Set<string>; files: Set<string>; namespace: boolean };

/**
 * `import <clause> from "@tauri-apps/x"` and `export <clause> from "..."`.
 * `[^;]*?` cannot cross a statement terminator, and an import clause never
 * contains one, so this stays inside a single statement while still spanning
 * the newlines of a multi-line `import { a, b } from`.
 */
// The clause starts a statement (after a line break, `;` or `}`), carries no
// quote, and never contains another import/export keyword, so it cannot span
// from an earlier semicolon-free statement into this one. Comments are
// stripped first (stripComments), so a doc comment naming "import" is inert.
const CLAUSE_RE =
  /(?:^|[;\r\n}])\s*(?:import|export)\s+((?:(?!\b(?:import|export)\b)[^;"'])*?)\s+from\s*["']@tauri-apps\/([^"']+)["']/g;

/**
 * Blank out `//` and block comments, keeping string and template literals
 * (where `//` and `/*` are text) and every newline, so offsets and statement
 * boundaries survive.
 */
export function stripComments(code: string): string {
  let out = "";
  let i = 0;
  while (i < code.length) {
    const ch = code[i];
    const next = code[i + 1];
    if (ch === "/" && next === "/") {
      while (i < code.length && code[i] !== "\n") i += 1;
      continue;
    }
    if (ch === "/" && next === "*") {
      const end = code.indexOf("*/", i + 2);
      const stop = end < 0 ? code.length : end + 2;
      out += code.slice(i, stop).replace(/[^\n]/g, " ");
      i = stop;
      continue;
    }
    if (ch === '"' || ch === "'" || ch === "`") {
      let j = i + 1;
      while (j < code.length && code[j] !== ch) {
        if (code[j] === "\\") j += 1;
        if (ch !== "`" && code[j] === "\n") break;
        j += 1;
      }
      out += code.slice(i, j + 1);
      i = j + 1;
      continue;
    }
    out += ch;
    i += 1;
  }
  return out;
}
const BARE_RE = /import\s*["']@tauri-apps\/([^"']+)["']/g;
const DYNAMIC_RE = /import\s*\(\s*["']@tauri-apps\/([^"']+)["']\s*\)/g;

const IDENTIFIER_RE = /^[A-Za-z_$][A-Za-z0-9_$]*$/;

function demandFor(map: Map<string, Demand>, capability: string): Demand {
  let demand = map.get(capability);
  if (!demand) {
    demand = { symbols: new Set(), files: new Set(), namespace: false };
    map.set(capability, demand);
  }
  return demand;
}

/** Pull the imported export names out of an import/export clause. */
function readClause(clause: string, demand: Demand): void {
  const trimmed = clause.trim().replace(/^type\s+/, "");
  // `* as ns`, alone or after a default (`Default, * as ns`).
  if (/(?:^|,)\s*\*\s+as\s+/.test(trimmed) || trimmed === "*") {
    demand.namespace = true;
    return;
  }
  const braced = /\{([\s\S]*)\}/.exec(trimmed);
  const beforeBrace = braced ? trimmed.slice(0, trimmed.indexOf("{")) : trimmed;
  if (beforeBrace.replace(/[\s,]/g, "") !== "") demand.symbols.add("default");
  if (!braced) return;
  for (const raw of braced[1].split(",")) {
    const name = raw.trim().replace(/^type\s+/, "").split(/\s+as\s+/)[0].trim();
    if (name) demand.symbols.add(name);
  }
}

/** Walk `src/` once and record every `@tauri-apps/*` module the tree asks for. */
export function scanTauriDemand(srcDir: string): Map<string, Demand> {
  const found = new Map<string, Demand>();
  const walk = (dir: string): void => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(full);
        continue;
      }
      if (!SOURCE_EXTENSIONS.has(path.extname(entry.name))) continue;
      const raw = fs.readFileSync(full, "utf8");
      if (!raw.includes(SPECIFIER_PREFIX)) continue;
      const code = stripComments(raw);
      for (const m of code.matchAll(CLAUSE_RE)) {
        const demand = demandFor(found, m[2]);
        demand.files.add(full);
        readClause(m[1], demand);
      }
      for (const m of code.matchAll(BARE_RE)) demandFor(found, m[1]).files.add(full);
      for (const m of code.matchAll(DYNAMIC_RE)) {
        const demand = demandFor(found, m[1]);
        demand.files.add(full);
        // A dynamic import destructures the namespace at runtime, where no
        // static scan can see the names, so the stand-in cannot cover it:
        // `buildStart` refuses this for a module with no facade.
        demand.namespace = true;
      }
    }
  };
  walk(srcDir);
  return found;
}

/** The facade file for a capability, or null when nobody has ported it. */
function facadeFor(root: string, capability: string): string | null {
  const base = path.join(root, ...FACADE_DIR, capability);
  for (const candidate of [`${base}.ts`, `${base}.tsx`, path.join(base, "index.ts")]) {
    if (fs.existsSync(candidate)) return candidate;
  }
  return null;
}

export function tauriWebAlias(): Plugin {
  let root = process.cwd();
  let srcDir = path.join(root, "src");
  let demand = new Map<string, Demand>();
  const strict = process.env.BUZZ_TAURI_STRICT === "1";

  return {
    name: "tauri-web-alias",
    // Must beat Vite's own resolver: `@tauri-apps/*` is a bare specifier and
    // would otherwise be looked for in node_modules, where it is not installed.
    enforce: "pre",

    configResolved(config) {
      root = config.root;
      srcDir = path.join(root, "src");
    },

    buildStart() {
      demand = scanTauriDemand(srcDir);
      const orphans = [...demand.entries()]
        .filter(([capability]) => facadeFor(root, capability) === null)
        .sort(([a], [b]) => a.localeCompare(b));
      for (const [capability, use] of orphans) {
        const symbols = [...use.symbols].sort().join(", ") || "(namespace)";
        const files = [...use.files]
          .map((f) => path.relative(root, f))
          .sort()
          .join(", ");
        const message =
          `@tauri-apps/${capability} has no facade in src/shared/tauri-web/. ` +
          `Symbols {${symbols}} will throw TauriModuleUnavailableError at the call site. ` +
          `Wanted by: ${files}`;
        if (use.namespace) {
          // Names read off a namespace object (`import * as x`, `await
          // import()`) are not bindings the stand-in can make throw; they
          // would be `undefined` at runtime. No call-site error is possible,
          // so this cannot wait for BUZZ_TAURI_STRICT.
          this.error(
            `@tauri-apps/${capability} has no facade in src/shared/tauri-web/ and is imported as a ` +
              `namespace or dynamically, so its members would silently be undefined. ` +
              `Add a facade (see src/shared/tauri-web/README.md). Wanted by: ${files}`,
          );
        }
        if (strict) this.error(message);
        else this.warn(message);
      }
    },

    resolveId(source) {
      if (!source.startsWith(SPECIFIER_PREFIX)) return null;
      const capability = source.slice(SPECIFIER_PREFIX.length);
      return facadeFor(root, capability) ?? VIRTUAL_PREFIX + capability;
    },

    load(id) {
      if (!id.startsWith(VIRTUAL_PREFIX)) return null;
      const capability = id.slice(VIRTUAL_PREFIX.length);
      const use = demand.get(capability);
      const unavailable = path.join(root, ...FACADE_DIR, "unavailable.ts");
      const q = JSON.stringify;
      const lines = [
        `import { unavailableTauriBinding, unavailableTauriNamespace } from ${q(unavailable)};`,
      ];
      for (const symbol of [...(use?.symbols ?? [])].sort()) {
        if (symbol === "default" || !IDENTIFIER_RE.test(symbol)) continue;
        lines.push(
          `export const ${symbol} = /*#__PURE__*/ unavailableTauriBinding(${q(capability)}, ${q(symbol)});`,
        );
      }
      lines.push(
        `export default /*#__PURE__*/ unavailableTauriNamespace(${q(capability)});`,
      );
      return `${lines.join("\n")}\n`;
    },
  };
}
