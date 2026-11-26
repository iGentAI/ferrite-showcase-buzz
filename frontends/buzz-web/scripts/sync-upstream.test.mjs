// Run: node --test scripts/sync-upstream.test.mjs
//
// Copies sync-upstream.mjs into a throwaway <parent>/repo/frontends/buzz-web
// layout and checks where it looks for the block/buzz checkout by default.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));

test("F338: with no --upstream and no BUZZ_UPSTREAM the default is the repo's sibling ../buzz", () => {
  const parent = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "sync-upstream-")));
  const scripts = path.join(parent, "repo", "frontends", "buzz-web", "scripts");
  fs.mkdirSync(scripts, { recursive: true });
  for (const file of ["sync-upstream.mjs", "upstream-allowlist.json"]) {
    fs.copyFileSync(path.join(HERE, file), path.join(scripts, file));
  }
  const env = { ...process.env };
  delete env.BUZZ_UPSTREAM;
  const run = () =>
    spawnSync(process.execPath, [path.join(scripts, "sync-upstream.mjs"), "--check"], { env, encoding: "utf8" });

  const missing = run();
  assert.notEqual(missing.status, 0);
  assert.match(missing.stderr, new RegExp(`no desktop/src under ${path.join(parent, "buzz").replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\.`));

  // With a checkout in that documented place, resolution gets past it.
  fs.mkdirSync(path.join(parent, "buzz", "desktop", "src"), { recursive: true });
  assert.doesNotMatch(run().stderr, /no desktop\/src under/);
  fs.rmSync(parent, { recursive: true, force: true });
});
