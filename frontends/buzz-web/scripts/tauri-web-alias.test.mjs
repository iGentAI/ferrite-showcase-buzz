// Run: node --test scripts/tauri-web-alias.test.mjs
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

delete process.env.BUZZ_TAURI_STRICT;
const { scanTauriDemand, tauriWebAlias } = await import("./tauri-web-alias.ts");

const WEB_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

/** A Vite root whose src/ holds the given files and no facades. */
function rootWith(files) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "tauri-alias-"));
  for (const [name, code] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(root, "src", name)), { recursive: true });
    fs.writeFileSync(path.join(root, "src", name), code);
  }
  return root;
}

/** Run the plugin's buildStart the way Rollup would, collecting warnings. */
function buildStart(root) {
  const plugin = tauriWebAlias();
  plugin.configResolved({ root });
  const warnings = [];
  plugin.buildStart.call({
    error(message) {
      throw new Error(message);
    },
    warn(message) {
      warnings.push(message);
    },
  });
  return warnings;
}

test("F339: a namespace import of a module with no facade fails the build even without BUZZ_TAURI_STRICT", () => {
  const root = rootWith({ "a.ts": 'import * as fsx from "@tauri-apps/plugin-fs";\nfsx.readFile("x");\n' });
  assert.throws(() => buildStart(root), /plugin-fs has no facade .* namespace or dynamically/);
  fs.rmSync(root, { recursive: true, force: true });
});

test("F339: a dynamic import of a module with no facade fails the build even without BUZZ_TAURI_STRICT", () => {
  const root = rootWith({ "b.ts": 'const { open } = await import("@tauri-apps/plugin-dialog");\nopen();\n' });
  assert.throws(() => buildStart(root), /plugin-dialog has no facade .* namespace or dynamically/);
  fs.rmSync(root, { recursive: true, force: true });
});

test("F339: a named import of a module with no facade still only warns outside strict mode", () => {
  const root = rootWith({ "c.ts": 'import { exit } from "@tauri-apps/plugin-shell";\nexit();\n' });
  const warnings = buildStart(root);
  assert.equal(warnings.length, 1);
  assert.match(warnings[0], /plugin-shell has no facade.*\{exit\}/);
  fs.rmSync(root, { recursive: true, force: true });
});

test("F339: every @tauri-apps module the real src/ imports has a facade, so the new rule does not break the build", () => {
  const orphans = [...scanTauriDemand(path.join(WEB_ROOT, "src")).keys()].filter((capability) => {
    const base = path.join(WEB_ROOT, "src", "shared", "tauri-web", capability);
    return ![`${base}.ts`, `${base}.tsx`, path.join(base, "index.ts")].some((file) => fs.existsSync(file));
  });
  assert.deepEqual(orphans, []);
});

test("scanTauriDemand: comments naming import and semicolon-free files do not hide a namespace import", () => {
  const root = rootWith({
    "a.ts": '/** Helpers for snapshot import. */\nimport * as fsx from "@tauri-apps/plugin-fs";\n',
    "b.ts": 'import { a } from "react"\nimport * as clip from "@tauri-apps/plugin-clipboard-manager"\n',
    "c.ts": 'import Def, * as shell from "@tauri-apps/plugin-shell";\n',
    "d.ts": '// import { nope } from "@tauri-apps/plugin-os";\nexport const x = "// not a comment";\n',
  });
  const demand = scanTauriDemand(path.join(root, "src"));
  assert.equal(demand.get("plugin-fs")?.namespace, true);
  assert.equal(demand.get("plugin-clipboard-manager")?.namespace, true);
  assert.equal(demand.get("plugin-shell")?.namespace, true);
  assert.equal(demand.get("plugin-os")?.symbols.has("nope") ?? false, false);
});
