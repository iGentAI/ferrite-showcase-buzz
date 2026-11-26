// Every command the client invokes must get an answer or a reason ABOUT IT.
//
// The browser backend answers what a tab can answer and refuses the rest with a
// sentence the UI shows the user directly. A refusal is therefore a product
// surface, and a wrong one is worse than none: "this is a desktop feature" sent
// people looking for a desktop app to do things no desktop app can do either.
//
// This gate exists because that decayed silently. Adding an `invokeTauri` call
// site announces nothing; the command just starts landing on the generic
// fallback. At the point this was written, 51 of 77 unanswered commands had.
//
// Parsed from source rather than imported: `commands.ts` is TypeScript and this
// suite runs plain node. That makes the parsing itself load-bearing, so it is
// narrow — see the note on array extraction below.
import { existsSync, readFileSync } from "node:fs";
import { execSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import test from "node:test";
import assert from "node:assert/strict";

const here = dirname(fileURLToPath(import.meta.url));
const srcRoot = resolve(here, "../..");
const source = readFileSync(resolve(here, "commands.ts"), "utf8");

/** Every command the client asks for, through the one wrapper it calls.
 *
 * Read from FILE CONTENT rather than piped through `grep`, because the call is
 * routinely written across lines:
 *
 * ```ts
 * const snapshot = await invokeTauri<RawProjectRepoSnapshot>(
 *   "get_project_repo_snapshot",
 *   { … },
 * );
 * ```
 *
 * `grep` is line-based, so a line-scoped pattern cannot see that name at all —
 * and a command that is invoked but invisible to this gate is exactly the case
 * the gate exists to catch. Nearly every `projectGit.ts` call has this shape,
 * so the undercount was not a corner case. In JavaScript `\s` matches a
 * newline, so one regex over the whole file finds both spellings.
 */
function invokedCommands() {
  const files = execSync(
    `find . -type f \\( -name '*.ts' -o -name '*.tsx' \\) -not -path '*/node_modules/*'`,
    { cwd: srcRoot, encoding: "utf8", maxBuffer: 64 << 20 },
  )
    .split("\n")
    .filter(Boolean);
  const found = new Set();
  for (const file of files) {
    const text = readFileSync(resolve(srcRoot, file), "utf8");
    for (const m of text.matchAll(/invokeTauri\s*(?:<[^>]*>)?\s*\(\s*"([a-z0-9_]+)"/g)) {
      found.add(m[1]);
    }
    // Upstream modules that call `invoke` from `@tauri-apps/api/core` directly
    // (`shared/deep-link.ts` does) reach the same seam. Missing them let the
    // deep-link queue commands fall to the generic refusal unnoticed. The
    // `plugin:` family is the bridge's own and never a seam command.
    if (/from\s+"@tauri-apps\/api\/core"/.test(text)) {
      for (const m of text.matchAll(/\binvoke\s*(?:<[^>]*>)?\s*\(\s*"([a-z0-9_]+)"/g)) {
        found.add(m[1]);
      }
    }
  }
  return found;
}

/** Every command `handleAppCommand` answers. */
function answeredCommands() {
  return new Set([...source.matchAll(/^\s*case\s+"([a-z0-9_]+)":/gm)].map((m) => m[1]));
}

/** Every command the exact-match table names. */
function exactlyExplained() {
  const start = source.indexOf("const UNAVAILABLE_BY_COMMAND");
  const end = source.indexOf("const UNAVAILABLE_BY_FAMILY");
  assert.ok(start !== -1 && end > start, "the exact-match table should be findable");
  const block = source.slice(start, end);

  const named = new Set();
  // Only quoted names in an ARRAY LITERAL that feeds `.map(` count. Matching
  // every quoted identifier in the block would also pick up names mentioned
  // inside the message text (`list_save_subscriptions`, `get_users_batch`) and
  // report coverage that is not there.
  for (const arr of block.matchAll(/\[([\s\S]*?)\]\.map\(/g)) {
    for (const q of arr[1].matchAll(/"([a-z0-9_]+)"/g)) named.add(q[1]);
  }
  // Plus the plain object keys, one command each.
  for (const key of block.matchAll(/^ {2}([a-z0-9_]+):\s*$/gm)) named.add(key[1]);
  return named;
}

/** Every regex family, in the order `unavailableCommandMessage` tries them. */
function familyPatterns() {
  const start = source.indexOf("const UNAVAILABLE_BY_FAMILY");
  const end = source.indexOf("export function unavailableCommandMessage");
  assert.ok(start !== -1 && end > start, "the family table should be findable");
  const block = source.slice(start, end);
  return [...block.matchAll(/\[\s*(?:\/\/[^\n]*\n\s*)*(\/(?:[^/\\\n]|\\.)+\/[a-z]*),/gs)].map(
    (m) => {
      const [, body, flags] = m[1].match(/^\/(.*)\/([a-z]*)$/s);
      return new RegExp(body, flags);
    },
  );
}

test("every invoked command is answered or has a reason about it", () => {
  const invoked = invokedCommands();
  const answered = answeredCommands();
  const exact = exactlyExplained();
  const families = familyPatterns();

  assert.ok(invoked.size > 100, `expected the full command surface, saw ${invoked.size}`);
  assert.ok(families.length > 0, "expected at least one refusal family");

  const unexplained = [...invoked]
    .filter((cmd) => !answered.has(cmd))
    .filter((cmd) => !exact.has(cmd))
    .filter((cmd) => !families.some((rx) => rx.test(cmd)))
    .sort();

  assert.deepEqual(
    unexplained,
    [],
    `these commands fall through to the generic "no native backend" sentence, which may not be ` +
      `true of them. Give each an entry in UNAVAILABLE_BY_COMMAND (or answer it):\n  ` +
      unexplained.join("\n  "),
  );
});

test("no reason is kept for a command that is now answered", () => {
  // The other direction: once a command IS implemented, a leftover explanation
  // of why it cannot work is a message that contradicts working behaviour.
  const answered = answeredCommands();
  const stale = [...exactlyExplained()].filter((cmd) => answered.has(cmd)).sort();
  assert.deepEqual(
    stale,
    [],
    `these are answered by handleAppCommand, so their entry in UNAVAILABLE_BY_COMMAND is dead ` +
      `and should be removed:\n  ` + stale.join("\n  "),
  );
});

// Plan 031 Contract C: the desktop shell's native registry
// (buzz-desktop/native-commands.json, the same list its init script injects as
// window.__BUZZ_DESKTOP__) must stay inside the client's real command surface
// and must never double-implement a command the seam already answers. Skipped
// until the shell lands (WP-D1); active forever after.
const shellRegistryPath = resolve(srcRoot, "../../buzz-desktop/native-commands.json");

test(
  "the desktop shell's registry stays inside the invoked surface and off the seam",
  { skip: !existsSync(shellRegistryPath) && "buzz-desktop/native-commands.json not present (WP-D1 not landed)" },
  () => {
    const registry = JSON.parse(readFileSync(shellRegistryPath, "utf8"));
    assert.equal(
      registry.version,
      1,
      "unknown registry version — revise this gate together with Contract C",
    );
    assert.ok(Array.isArray(registry.commands) && registry.commands.length > 0);
    const native = new Set(registry.commands);
    // Commands the shell answers INSTEAD of the seam, declared in the registry.
    // The exception exists because a few answers are truthfully face-dependent
    // (`list_managed_agents` is `[]` in a tab and a real list in the shell), and
    // declaring them is what keeps a deliberate override distinguishable from an
    // accidental double answer.
    const overrides = new Set(registry.overrides ?? []);

    const answered = answeredCommands();
    const doubly = [...native]
      .filter((c) => answered.has(c) && !overrides.has(c))
      .sort();
    assert.deepEqual(
      doubly,
      [],
      `answered by BOTH the seam and the shell — exactly one implementation must own each ` +
        `command, unless the shell DECLARES an override:\n  ` + doubly.join("\n  "),
    );

    // An override must override something: an entry the seam does not answer is
    // stale, and would sit in the registry reading like a considered decision.
    const overridesNothing = [...overrides].filter((c) => !answered.has(c)).sort();
    assert.deepEqual(
      overridesNothing,
      [],
      `declared as overrides but the seam does not answer them, so there is nothing to ` +
        `override:\n  ` + overridesNothing.join("\n  "),
    );
    // ...and it must be registered, or it routes to the seam anyway and the
    // declaration is a comment rather than behaviour.
    const unrouted = [...overrides].filter((c) => !native.has(c)).sort();
    assert.deepEqual(
      unrouted,
      [],
      `declared as overrides but not in the native registry:\n  ` + unrouted.join("\n  "),
    );

    const invoked = invokedCommands();
    const pluginLike = /^plugin:[a-z0-9-]+\|[a-z0-9_]+$/;
    const meta = new Set(["desktop_capabilities"]);
    const stale = [...native]
      .filter((c) => !invoked.has(c) && !pluginLike.test(c) && !meta.has(c))
      .sort();
    assert.deepEqual(
      stale,
      [],
      `registered natively but invoked nowhere in the client — stale registry entries:\n  ` +
        stale.join("\n  "),
    );
  },
);
