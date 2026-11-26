// Run: node --import ./test-loader.mjs --test e2e/teardown-runner.test.mjs
//
// Runs the real Playwright runner with this harness's globalTeardown, because
// Playwright decides its own exit status: a teardown that only sets
// `process.exitCode` still yields a successful run. No browser is launched.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const WEB_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const CLI = path.join(WEB_ROOT, "node_modules", "@playwright", "test", "cli.js");
const TEARDOWN = path.join(WEB_ROOT, "e2e", "teardown.ts");

/** A throwaway Playwright project inside the package, so @playwright/test resolves. */
function runPlaywright(mode) {
  const dir = fs.mkdtempSync(path.join(WEB_ROOT, ".teardown-runner-"));
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "teardown-runner-"));
  try {
    fs.writeFileSync(
      path.join(dir, "playwright.config.mjs"),
      `export default { testDir: ".", testMatch: "fixture.spec.mjs", outputDir: "./out", workers: 1, reporter: "line", globalTeardown: "./teardown.mjs" };\n`,
    );
    fs.writeFileSync(
      path.join(dir, "fixture.spec.mjs"),
      `import fs from "node:fs";\nimport { test } from "@playwright/test";\n` +
        `test("journey", async ({}, testInfo) => {\n` +
        `  if (process.env.FIXTURE_MODE === "broken-trace") fs.writeFileSync(testInfo.outputPath("trace.zip"), "PK\\u0003\\u0004 truncated");\n` +
        `});\n`,
    );
    fs.writeFileSync(
      path.join(dir, "teardown.mjs"),
      `globalThis.fetch = async () => new Response("{}");\n` +
        `const { default: teardown } = await import(${JSON.stringify(TEARDOWN)});\n` +
        `export default teardown;\n`,
    );
    const env = { ...process.env, FIXTURE_MODE: mode, BUZZ_CREATED_LOG: path.join(scratch, "created.jsonl"), BUZZ_TRACE_QUARANTINE: path.join(scratch, "quarantine") };
    delete env.BUZZ_SSO_ORIGIN;
    delete env.BUZZ_KEEP_CREATED;
    return spawnSync(process.execPath, [CLI, "test", "--config", path.join(dir, "playwright.config.mjs")], {
      cwd: WEB_ROOT,
      env,
      encoding: "utf8",
      timeout: 120_000,
    });
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
    fs.rmSync(scratch, { recursive: true, force: true });
  }
}

test("control: a clean run through the real runner succeeds", () => {
  const run = runPlaywright("clean");
  assert.equal(run.status, 0, `${run.stdout}\n${run.stderr}`);
});

test("F136/F329: an unscrubbable trace fails the Playwright run itself", () => {
  const run = runPlaywright("broken-trace");
  const output = `${run.stdout}\n${run.stderr}`;
  assert.match(output, /TRACE NOT SCRUBBED/);
  assert.notEqual(run.status, 0, `Playwright exited 0 despite an unscrubbed trace\n${output}`);
});
