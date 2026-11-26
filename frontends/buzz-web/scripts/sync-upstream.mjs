#!/usr/bin/env node
/**
 * sync-upstream — re-copy block/buzz's `desktop/src` over `buzz-web/src`, and
 * report the result against the divergence allowlist.
 *
 *   node scripts/sync-upstream.mjs --check [--upstream <path/to/buzz>]
 *   node scripts/sync-upstream.mjs --apply [--upstream <path/to/buzz>]
 *   node scripts/sync-upstream.mjs --write-manifest [--upstream <path>]
 *   node scripts/sync-upstream.mjs --check-offline
 *
 * `--check` changes nothing. `--apply` copies, vendors and parks, then runs the
 * same report. Both are idempotent: a second `--apply` writes nothing.
 *
 * `--write-manifest` records the pinned subtree's per-file sha256 into
 * scripts/upstream-manifest.json (run it at every sync, playbook phase 3), and
 * `--check-offline` verifies our tree against that manifest with NO clone at
 * all, which closes the one drift mode that rots silently between syncs:
 * hand-editing a vendored copy.
 *
 * THE CONTRACT. `buzz-web/src` is upstream's tree. Every file in it must be
 * byte-identical to `desktop/src` unless `scripts/upstream-allowlist.json` says
 * otherwise, with a bucket and a reason. That is the whole gate; there is no
 * second list anywhere.
 *
 * Comparison is plain byte equality: `tauriWebAlias()` in vite.config.ts and
 * `paths` in tsconfig.json resolve `@tauri-apps/*` onto `src/shared/tauri-web/`,
 * so no codemod has to rewrite import lines first.
 *
 * TWO HARD RULES, enforced rather than intended:
 *   1. The upstream checkout is READ-ONLY. Every write goes through `writeOurs`,
 *      which throws unless the destination is inside our own tree, and `--apply`
 *      re-asserts `git status --porcelain` is empty upstream when it finishes.
 *   2. Nothing is ever deleted. A file we hold that upstream has dropped and the
 *      allowlist does not cover is PARKED — moved to `.sync-parked/<timestamp>/`
 *      — never removed.
 */
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const WEB_ROOT = path.resolve(HERE, "..");
const OURS = path.join(WEB_ROOT, "src");
const ALLOWLIST_FILE = path.join(HERE, "upstream-allowlist.json");
const PIN_FILE = path.join(WEB_ROOT, "UPSTREAM-PIN.md");
const PARK_ROOT = path.join(WEB_ROOT, ".sync-parked");
const MANIFEST_FILE = path.join(HERE, "upstream-manifest.json");

const DIVERGENCE_BUCKETS = new Set([
  "browser-only",
  "product",
  "upstreamable-pending",
]);

function parseArgs(argv) {
  const opts = { mode: null, upstream: process.env.BUZZ_UPSTREAM ?? null };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--check" || arg === "--apply") opts.mode = arg.slice(2);
    else if (arg === "--write-manifest" || arg === "--check-offline") {
      opts.mode = arg.slice(2);
    } else if (arg === "--upstream") opts.upstream = argv[++i];
    else if (arg.startsWith("--upstream=")) opts.upstream = arg.slice(11);
    else if (arg === "-h" || arg === "--help") opts.mode = "help";
    else die(`unknown argument: ${arg}\nTry --help.`);
  }
  return opts;
}

function die(message) {
  process.stderr.write(`sync-upstream: ${message}\n`);
  process.exit(2);
}

const USAGE = `usage: node scripts/sync-upstream.mjs
         (--check | --apply | --write-manifest | --check-offline) [--upstream <path>]

  --check           report only; changes nothing. Exit 0 when every difference
                    is on the allowlist, 1 otherwise.
  --apply           copy upstream over ours (skipping allowlisted divergences),
                    refresh the vendored assets, park orphans, then report.
  --write-manifest  record the pinned subtree's per-file sha256 into
                    scripts/upstream-manifest.json. Run at every sync, so the
                    offline check has a current baseline.
  --check-offline   verify our tree against the manifest with NO clone: every
                    non-allowlisted file must hash-match, every ours-only file
                    must be covered by an additive entry. Exit 0/1.
  --upstream        the block/buzz checkout. Defaults to $BUZZ_UPSTREAM, then to
                    ../../../buzz relative to this package. Never written to.
`;

/** Absolute-path containment that cannot be fooled by a shared name prefix. */
function isInside(child, parent) {
  const rel = path.relative(parent, child);
  return rel !== "" && !rel.startsWith("..") && !path.isAbsolute(rel);
}

let WRITE_GUARD = [];

function writeOurs(destination, act) {
  const resolved = path.resolve(destination);
  if (!WRITE_GUARD.some((root) => isInside(resolved, root))) {
    throw new Error(
      `refusing to write outside our own tree: ${resolved}\n` +
        `(allowed roots: ${WRITE_GUARD.join(", ")})`,
    );
  }
  fs.mkdirSync(path.dirname(resolved), { recursive: true });
  act(resolved);
}

function listFiles(root) {
  const out = [];
  const walk = (dir, prefix) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) =>
      a.name.localeCompare(b.name),
    )) {
      const rel = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (entry.isDirectory()) walk(path.join(dir, entry.name), rel);
      else if (entry.isFile()) out.push(rel);
    }
  };
  if (fs.existsSync(root)) walk(root, "");
  return out;
}

const sameBytes = (a, b) => Buffer.compare(fs.readFileSync(a), fs.readFileSync(b)) === 0;
const isBinary = (buf) => buf.includes(0);

/**
 * LCS over lines. The files it runs on are a few hundred lines each, so the
 * quadratic table is cheaper than pulling in a diff dependency — and this script
 * having no dependencies is the point.
 */
function lineDiff(oldText, newText) {
  const a = oldText.split("\n");
  const b = newText.split("\n");
  if (a.length * b.length > 40_000_000) {
    return { removed: a.length, added: b.length, hunks: [], approximate: true };
  }
  const w = b.length + 1;
  const table = new Int32Array((a.length + 1) * w);
  for (let i = a.length - 1; i >= 0; i -= 1) {
    for (let j = b.length - 1; j >= 0; j -= 1) {
      table[i * w + j] =
        a[i] === b[j]
          ? table[(i + 1) * w + (j + 1)] + 1
          : Math.max(table[(i + 1) * w + j], table[i * w + (j + 1)]);
    }
  }
  let removed = 0;
  let added = 0;
  const hunks = [];
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      i += 1;
      j += 1;
    } else if (table[(i + 1) * w + j] >= table[i * w + (j + 1)]) {
      hunks.push(`-${i + 1} ${a[i]}`);
      removed += 1;
      i += 1;
    } else {
      hunks.push(`+${j + 1} ${b[j]}`);
      added += 1;
      j += 1;
    }
  }
  for (; i < a.length; i += 1, removed += 1) hunks.push(`-${i + 1} ${a[i]}`);
  for (; j < b.length; j += 1, added += 1) hunks.push(`+${j + 1} ${b[j]}`);
  return { removed, added, hunks, approximate: false };
}

function diffCounts(upstreamFile, oursFile) {
  const up = fs.readFileSync(upstreamFile);
  const ours = fs.readFileSync(oursFile);
  if (isBinary(up) || isBinary(ours)) {
    return { removed: 0, added: 0, hunks: ["(binary)"], binary: true };
  }
  return lineDiff(up.toString("utf8"), ours.toString("utf8"));
}

function loadAllowlist() {
  const raw = JSON.parse(fs.readFileSync(ALLOWLIST_FILE, "utf8"));
  const diverge = new Map();
  const additiveFiles = new Map();
  const additivePrefixes = [];
  const vendored = [];
  for (const entry of raw.entries) {
    if (!entry.path || !entry.bucket || !entry.reason) {
      die(`allowlist entry is missing path/bucket/reason: ${JSON.stringify(entry)}`);
    }
    if (DIVERGENCE_BUCKETS.has(entry.bucket)) {
      if (entry.path.endsWith("/")) {
        die(`only an 'additive' entry may be a directory prefix: ${entry.path}`);
      }
      diverge.set(entry.path, entry);
    } else if (entry.bucket === "additive") {
      if (entry.path.endsWith("/")) additivePrefixes.push(entry);
      else additiveFiles.set(entry.path, entry);
      if (entry.vendoredFrom) vendored.push(entry);
    } else {
      die(
        `unknown bucket "${entry.bucket}" on ${entry.path} — must be one of ` +
          `browser-only, product, upstreamable-pending, additive`,
      );
    }
  }
  return { raw, diverge, additiveFiles, additivePrefixes, vendored };
}

/** Pull the first fenced ```json block out of UPSTREAM-PIN.md. */
function loadPin() {
  if (!fs.existsSync(PIN_FILE)) return null;
  const match = /```json\n([\s\S]*?)\n```/.exec(fs.readFileSync(PIN_FILE, "utf8"));
  if (!match) return null;
  try {
    return JSON.parse(match[1]);
  } catch (error) {
    die(`UPSTREAM-PIN.md has a malformed json block: ${error.message}`);
  }
}

/** Read-only git. `--no-optional-locks` so we cannot even refresh their index. */
function gitRead(repo, args) {
  try {
    return execFileSync("git", ["--no-optional-locks", "-C", repo, ...args], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    }).trim();
  } catch {
    return null;
  }
}

/**
 * The git tree object id of `dir`, computed from the files on disk.
 *
 * Platform fact, measured 2026-09-11: repository checkouts here can be
 * FILES-ONLY — no `.git` at all — so `git rev-parse HEAD:desktop/src` has
 * nothing to read. The tree id is a pure function of content (blob = sha1 of
 * `blob <len>\0<bytes>`, tree = sha1 of the sorted `<mode> <name>\0<sha>`
 * entries), so it can be recomputed exactly and compared with the recorded
 * `subtreeGitTree` — which is the anchor that cannot rot (playbook §2).
 */
export function gitTreeHash(dir) {
  const entries = [];
  for (const name of fs.readdirSync(dir)) {
    const full = path.join(dir, name);
    const stat = fs.lstatSync(full);
    let mode;
    let sha;
    if (stat.isDirectory()) {
      mode = "40000";
      sha = gitTreeHash(full);
    } else if (stat.isSymbolicLink()) {
      mode = "120000";
      sha = gitObjectHash("blob", Buffer.from(fs.readlinkSync(full)));
    } else {
      mode = stat.mode & 0o111 ? "100755" : "100644";
      sha = gitObjectHash("blob", fs.readFileSync(full));
    }
    entries.push({ name, mode, sha, sortKey: mode === "40000" ? `${name}/` : name });
  }
  entries.sort((a, b) => Buffer.compare(Buffer.from(a.sortKey), Buffer.from(b.sortKey)));
  const body = Buffer.concat(
    entries.flatMap((e) => [Buffer.from(`${e.mode} ${e.name}\0`), Buffer.from(e.sha, "hex")]),
  );
  return gitObjectHash("tree", body);
}

function gitObjectHash(type, body) {
  return createHash("sha1")
    .update(Buffer.from(`${type} ${body.length}\0`))
    .update(body)
    .digest("hex");
}

/**
 * Which upstream commit and `desktop/src` tree the checkout holds.
 *
 * With git metadata both come from git. A files-only checkout has no commit to
 * read, so the commit comes from `BUZZ_UPSTREAM_COMMIT` (what the checkout tool
 * says it fetched) and is labelled as declared; the tree is COMPUTED from the
 * content either way, so what the report and the manifest record is what is on
 * disk rather than what a checkout claims.
 */
function upstreamIdentity(upstreamRoot) {
  const src = path.join(upstreamRoot, "desktop", "src");
  const computedTree = fs.existsSync(src) ? gitTreeHash(src) : null;
  const gitCommit = gitRead(upstreamRoot, ["rev-parse", "HEAD"]);
  const gitTree = gitRead(upstreamRoot, ["rev-parse", "HEAD:desktop/src"]);
  if (gitCommit) {
    return { commit: gitCommit, commitSource: "git", subtreeGitTree: gitTree ?? computedTree, computedTree };
  }
  const declared = process.env.BUZZ_UPSTREAM_COMMIT?.trim().toLowerCase() ?? "";
  if (declared !== "" && !/^[0-9a-f]{40}$/.test(declared)) {
    die("BUZZ_UPSTREAM_COMMIT must be a full 40-hex commit id");
  }
  return {
    commit: declared || null,
    commitSource: declared ? "BUZZ_UPSTREAM_COMMIT (files-only checkout)" : null,
    subtreeGitTree: computedTree,
    computedTree,
  };
}

function resolveUpstream(given) {
  // WEB_ROOT is <repo>/frontends/buzz-web, so ../../.. is the repo's parent and
  // this is a sibling clone beside this repository, as USAGE says.
  const candidate = given ?? path.resolve(WEB_ROOT, "../../../buzz");
  const root = path.resolve(candidate);
  if (!fs.existsSync(path.join(root, "desktop", "src"))) {
    die(
      `no desktop/src under ${root}.\n` +
        `Pass --upstream <path/to/buzz> or set BUZZ_UPSTREAM.`,
    );
  }
  // The checkout may not live inside THIS REPO, and may not
  // contain our source tree. Write safety does not depend on this check: every
  // write funnels through `writeOurs`, whose WRITE_GUARD admits only src/ and
  // .sync-parked/.
  if (isInside(root, path.resolve(WEB_ROOT, "..", "..")) || isInside(OURS, root)) {
    die(`the upstream checkout must live outside this repo (got ${root})`);
  }
  return root;
}

function coveredByAdditive(rel, allow) {
  if (allow.additiveFiles.has(rel)) return allow.additiveFiles.get(rel);
  return allow.additivePrefixes.find((e) => rel.startsWith(e.path)) ?? null;
}

function compare(upstreamRoot, allow) {
  const upstreamSrc = path.join(upstreamRoot, "desktop", "src");
  const upstreamFiles = listFiles(upstreamSrc);
  const ourFiles = new Set(listFiles(OURS));

  const result = {
    upstreamSrc,
    upstreamCount: upstreamFiles.length,
    ourCount: ourFiles.size,
    identical: 0,
    allowed: [],
    unexpected: [],
    missing: [],
    additive: [],
    uncovered: [],
    staleAllowlist: [],
    staleAdditive: [],
    vendorStale: [],
    vendorOk: 0,
  };

  for (const rel of upstreamFiles) {
    const up = path.join(upstreamSrc, rel);
    const mine = path.join(OURS, rel);
    if (!ourFiles.has(rel)) {
      result.missing.push(rel);
      continue;
    }
    if (sameBytes(up, mine)) {
      result.identical += 1;
      if (allow.diverge.has(rel)) result.staleAllowlist.push(rel);
      continue;
    }
    const entry = allow.diverge.get(rel);
    const counts = diffCounts(up, mine);
    (entry ? result.allowed : result.unexpected).push({ rel, entry, counts });
  }

  const upstreamSet = new Set(upstreamFiles);
  for (const rel of ourFiles) {
    if (upstreamSet.has(rel)) continue;
    const entry = coveredByAdditive(rel, allow);
    if (entry) result.additive.push({ rel, entry });
    else result.uncovered.push(rel);
  }

  // A divergence entry naming a file upstream no longer ships is a merge hazard:
  // it silently stops gating anything.
  for (const rel of allow.diverge.keys()) {
    if (!upstreamSet.has(rel)) result.staleAllowlist.push(`${rel} (gone upstream)`);
  }
  for (const entry of [...allow.additiveFiles.values(), ...allow.additivePrefixes]) {
    const hit = entry.path.endsWith("/")
      ? [...ourFiles].some((rel) => rel.startsWith(entry.path))
      : ourFiles.has(entry.path);
    if (!hit) result.staleAdditive.push(entry.path);
  }

  for (const entry of allow.vendored) {
    for (const rel of vendorPairs(upstreamRoot, entry)) {
      const mine = path.join(OURS, rel.to);
      if (fs.existsSync(mine) && sameBytes(rel.from, mine)) result.vendorOk += 1;
      else result.vendorStale.push(rel);
    }
  }
  return result;
}

/** Expand one vendored entry into concrete from→to file pairs. */
function vendorPairs(upstreamRoot, entry) {
  const source = path.join(upstreamRoot, "desktop", entry.vendoredFrom);
  if (entry.path.endsWith("/")) {
    if (!fs.existsSync(source)) return [];
    return listFiles(source).map((rel) => ({
      from: path.join(source, rel),
      to: `${entry.path}${rel}`,
    }));
  }
  if (!fs.existsSync(source)) return [];
  return [{ from: source, to: entry.path }];
}

const sha256hex = (file) =>
  createHash("sha256").update(fs.readFileSync(file)).digest("hex");

/**
 * Record the pinned subtree's per-file sha256, plus the commit and tree it
 * corresponds to. Written at sync time from the UPSTREAM tree — which is what
 * our tree must equal everywhere the allowlist does not say otherwise — so the
 * offline check needs no clone and no history.
 */
function writeManifest(upstreamRoot) {
  const upstreamSrc = path.join(upstreamRoot, "desktop", "src");
  const files = {};
  for (const rel of listFiles(upstreamSrc)) {
    files[rel] = sha256hex(path.join(upstreamSrc, rel));
  }
  const identity = upstreamIdentity(upstreamRoot);
  const manifest = {
    comment:
      "Per-file sha256 of upstream desktop/src at the pin. Regenerate with " +
      "--write-manifest at every sync; verify offline with --check-offline.",
    commit: identity.commit,
    subtreeGitTree: identity.subtreeGitTree,
    generatedAt: new Date().toISOString(),
    fileCount: Object.keys(files).length,
    files,
  };
  writeOurs(MANIFEST_FILE, (dest) =>
    fs.writeFileSync(dest, `${JSON.stringify(manifest, null, 2)}\n`),
  );
  process.stdout.write(
    `wrote ${path.relative(WEB_ROOT, MANIFEST_FILE)}: ${manifest.fileCount} files ` +
      `at ${manifest.commit ?? "(commit unreadable)"}\n`,
  );
}

/**
 * Verify our tree against the manifest, with no clone. Catches the one
 * mistake that rots silently between syncs — a hand-edited vendored copy —
 * and the ours-only file nobody registered.
 */
function checkOffline(allow) {
  if (!fs.existsSync(MANIFEST_FILE)) {
    die(
      "no upstream-manifest.json — run --write-manifest against the pinned " +
        "clone during a sync first",
    );
  }
  const manifest = JSON.parse(fs.readFileSync(MANIFEST_FILE, "utf8"));
  if (!manifest.files || typeof manifest.files !== "object") {
    die("upstream-manifest.json has no files map; regenerate it");
  }
  // The manifest must BE the pin's baseline, or a coherent tree from the
  // wrong checkout would be blessed. Both identifiers are REQUIRED on both
  // sides and compared unconditionally — a pin or manifest missing one is a
  // configuration error of its own, not a licence to check less. All of
  // these are exit 2 (regenerate / repair), distinct from tree drift (exit 1).
  const pin = loadPin();
  if (!pin) {
    die("UPSTREAM-PIN.md is missing or has no json block; the manifest cannot be tied to a pin");
  }
  for (const [what, pinned, recorded] of [
    ["commit", pin.commit, manifest.commit],
    ["subtreeGitTree", pin.subtreeGitTree, manifest.subtreeGitTree],
  ]) {
    if (typeof pinned !== "string" || pinned === "") {
      die(`UPSTREAM-PIN.md records no ${what}; the offline baseline cannot be bound to the pin`);
    }
    if (typeof recorded !== "string" || recorded === "") {
      die(`upstream-manifest.json records no ${what}; regenerate it with --write-manifest`);
    }
    if (pinned !== recorded) {
      die(
        `the manifest's ${what} (${recorded}) does not match the pin's (${pinned}) — ` +
          "regenerate it with --write-manifest against the pinned clone",
      );
    }
  }
  const ourFiles = new Set(listFiles(OURS));
  const stale = [];
  const missing = [];
  const uncovered = [];
  const nowIdentical = [];
  let matched = 0;
  let divergent = 0;
  for (const [rel, hash] of Object.entries(manifest.files)) {
    if (!ourFiles.has(rel)) {
      missing.push(rel);
      continue;
    }
    const oursHash = sha256hex(path.join(OURS, rel));
    if (allow.diverge.has(rel)) {
      divergent += 1;
      // An allowlisted divergence whose bytes match the pin is a stale entry
      // — informational, exactly as the online report treats it.
      if (oursHash === hash) nowIdentical.push(rel);
      continue;
    }
    if (oursHash === hash) matched += 1;
    else stale.push(rel);
  }
  const manifestSet = new Set(Object.keys(manifest.files));
  for (const rel of ourFiles) {
    if (manifestSet.has(rel)) continue;
    if (!coveredByAdditive(rel, allow)) uncovered.push(rel);
  }

  const out = [];
  out.push("buzz-web upstream sync — CHECK-OFFLINE");
  out.push(`  manifest   : ${manifest.commit ?? "(no commit recorded)"}`);
  out.push(`  hash-match : ${matched}`);
  out.push(`  divergent (allowlisted) : ${divergent}`);
  const fail = [];
  if (stale.length) {
    fail.push("files differ from the pinned copy");
    out.push("  FILES DIFFER FROM THE PINNED COPY — an unregistered edit to a");
    out.push("  vendored file. Move the change to an additive file, or register");
    out.push("  the divergence with a bucket and a reason.");
    for (const rel of stale) out.push(`    ${rel}`);
  }
  if (missing.length) {
    fail.push("pinned files missing from ours");
    out.push("  PINNED FILES MISSING FROM OURS — run a full sync");
    for (const rel of missing) out.push(`    ${rel}`);
  }
  if (uncovered.length) {
    fail.push("uncovered ours-only files");
    out.push("  OURS-ONLY FILES NOT COVERED BY AN ADDITIVE ENTRY");
    for (const rel of uncovered) out.push(`    ${rel}`);
  }
  if (nowIdentical.length) {
    out.push("  note: allowlist entries whose file now matches the pin");
    for (const rel of nowIdentical) out.push(`    ${rel}`);
  }
  out.push(
    fail.length
      ? `FAIL — ${fail.join("; ")}`
      : "PASS — the tree matches the pinned manifest everywhere the allowlist does not say otherwise",
  );
  process.stdout.write(`${out.join("\n")}\n`);
  return fail.length === 0;
}

function apply(upstreamRoot, allow) {
  const upstreamSrc = path.join(upstreamRoot, "desktop", "src");
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const parkDir = path.join(PARK_ROOT, stamp);
  let copied = 0;
  let vendored = 0;
  const parked = [];
  const skipped = [];

  for (const rel of listFiles(upstreamSrc)) {
    if (allow.diverge.has(rel)) {
      skipped.push(rel);
      continue;
    }
    const from = path.join(upstreamSrc, rel);
    const to = path.join(OURS, rel);
    if (fs.existsSync(to) && sameBytes(from, to)) continue;
    writeOurs(to, (dest) => fs.copyFileSync(from, dest));
    copied += 1;
  }

  for (const entry of allow.vendored) {
    for (const pair of vendorPairs(upstreamRoot, entry)) {
      const to = path.join(OURS, pair.to);
      if (fs.existsSync(to) && sameBytes(pair.from, to)) continue;
      writeOurs(to, (dest) => fs.copyFileSync(pair.from, dest));
      vendored += 1;
    }
  }

  // Anything we hold that upstream does not ship and the allowlist does not
  // claim is a leftover — most often a file upstream deleted. Park it; this
  // script never removes anything.
  const upstreamSet = new Set(listFiles(upstreamSrc));
  for (const rel of listFiles(OURS)) {
    if (upstreamSet.has(rel)) continue;
    if (coveredByAdditive(rel, allow)) continue;
    const to = path.join(parkDir, rel);
    writeOurs(to, (dest) => fs.renameSync(path.join(OURS, rel), dest));
    parked.push(rel);
  }

  return { copied, vendored, parked, skipped, parkDir };
}

const pad = (n, w) => String(n).padStart(w);

function report(mode, upstreamRoot, pin, result, applied) {
  const out = [];
  const say = (line = "") => out.push(line);
  const identity = upstreamIdentity(upstreamRoot);
  const head = identity.commit;
  const dirty = gitRead(upstreamRoot, ["status", "--porcelain"]);

  say(`buzz-web upstream sync — ${mode.toUpperCase()}`);
  say(`  upstream   : ${result.upstreamSrc}`);
  say(`  ours       : ${OURS}`);
  say(`  pin        : ${pin ? pin.commit : "(UPSTREAM-PIN.md not found)"}`);
  if (head) {
    const agree = pin && head === pin.commit;
    say(
      `  clone HEAD : ${head}  ${agree ? "[matches pin]" : "[DIFFERS from pin]"}` +
        (identity.commitSource === "git" ? "" : `  (declared via ${identity.commitSource})`),
    );
  }
  if (identity.computedTree) {
    const agreeTree = pin && identity.computedTree === pin.subtreeGitTree;
    say(
      `  clone tree : desktop/src ${identity.computedTree} computed from content  ` +
        (agreeTree ? "[matches pin tree]" : "[DIFFERS from pin tree]"),
    );
  }
  if (dirty === null) {
    say("  git state  : no git metadata (files-only checkout) — content tree above is the anchor");
  } else {
    say(`  git state  : ${dirty === "" ? "clean" : "DIRTY — upstream must be untouched"}`);
  }
  say();

  if (applied) {
    say("  applied");
    say(`    files copied from upstream ... ${applied.copied}`);
    say(`    vendored assets refreshed .... ${applied.vendored}`);
    say(`    divergences left alone ....... ${applied.skipped.length}`);
    say(`    orphans parked ............... ${applied.parked.length}`);
    for (const rel of applied.parked) say(`      -> .sync-parked/…/${rel}`);
    say();
  }

  const byBucket = new Map();
  for (const item of result.allowed) {
    byBucket.set(item.entry.bucket, (byBucket.get(item.entry.bucket) ?? 0) + 1);
  }
  const bucketSummary =
    [...byBucket.entries()].sort().map(([b, n]) => `${b} ${n}`).join(", ") || "none";

  say("  counts");
  say(`    upstream files ............... ${pad(result.upstreamCount, 5)}`);
  say(`    ours files ................... ${pad(result.ourCount, 5)}`);
  say(`    byte-identical ............... ${pad(result.identical, 5)}`);
  say(`    allowlisted divergences ...... ${pad(result.allowed.length, 5)}  (${bucketSummary})`);
  say(`    UNEXPECTED divergences ....... ${pad(result.unexpected.length, 5)}`);
  say(`    upstream files missing here .. ${pad(result.missing.length, 5)}`);
  say(`    ours-only, additive .......... ${pad(result.additive.length, 5)}`);
  say(`    ours-only, UNCOVERED ......... ${pad(result.uncovered.length, 5)}`);
  say(`    vendored assets in sync ...... ${pad(result.vendorOk, 5)}`);
  say(`    vendored assets STALE ........ ${pad(result.vendorStale.length, 5)}`);
  say();

  if (result.allowed.length) {
    let residual = 0;
    say("  allowlisted divergences (the fork a human maintains)");
    const order = { "upstreamable-pending": 0, product: 1, "browser-only": 2 };
    for (const item of [...result.allowed].sort(
      (x, y) =>
        (order[x.entry.bucket] ?? 9) - (order[y.entry.bucket] ?? 9) ||
        x.rel.localeCompare(y.rel),
    )) {
      const c = item.counts;
      residual += c.removed + c.added;
      say(
        `    [${item.entry.bucket.padEnd(20)}] ${item.rel.padEnd(52)} -${c.removed}/+${c.added}`,
      );
    }
    say(`    residual fork: ${residual} changed lines across ${result.allowed.length} files`);
    say();
  }

  const fail = [];
  if (result.unexpected.length) {
    fail.push("unexpected divergences");
    say("  UNEXPECTED divergences — not on the allowlist");
    say("    Either re-apply our intent on top of upstream's new content and add");
    say("    the file to scripts/upstream-allowlist.json with a bucket and a");
    say("    reason, or drop our change and let the copy stand.");
    for (const item of result.unexpected) {
      const c = item.counts;
      say(`    ${item.rel}  -${c.removed}/+${c.added}`);
      for (const h of c.hunks.slice(0, 8)) say(`        ${h}`);
      if (c.hunks.length > 8) say(`        … ${c.hunks.length - 8} more changed lines`);
    }
    say();
  }
  if (result.missing.length) {
    fail.push("upstream files missing from ours");
    say("  UPSTREAM FILES MISSING FROM OURS — run --apply");
    for (const rel of result.missing) say(`    ${rel}`);
    say();
  }
  if (result.uncovered.length) {
    fail.push("uncovered ours-only files");
    say("  OURS-ONLY FILES NOT COVERED BY AN ADDITIVE ENTRY");
    say("    Add an 'additive' entry with a reason, or let --apply park them.");
    for (const rel of result.uncovered) say(`    ${rel}`);
    say();
  }
  if (result.vendorStale.length) {
    fail.push("stale vendored assets");
    say("  VENDORED ASSETS OUT OF SYNC WITH desktop/public — run --apply");
    for (const pair of result.vendorStale) say(`    ${pair.to}`);
    say();
  }
  if (dirty !== "" && dirty !== null) {
    fail.push("upstream checkout is dirty");
    say("  UPSTREAM CHECKOUT IS DIRTY — it must stay pristine");
    say(dirty.split("\n").map((l) => `    ${l}`).join("\n"));
    say();
  }

  if (result.staleAllowlist.length) {
    say("  note: allowlist entries you can now DELETE (no longer divergent)");
    for (const rel of result.staleAllowlist) say(`    ${rel}`);
    say();
  }
  if (result.staleAdditive.length) {
    say("  note: additive entries matching nothing on disk");
    for (const rel of result.staleAdditive) say(`    ${rel}`);
    say();
  }

  say(fail.length ? `FAIL — ${fail.join("; ")}` : "PASS — every difference is on the allowlist");
  process.stdout.write(`${out.join("\n")}\n`);
  return fail.length === 0;
}

function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.mode === "help" || opts.mode === null) {
    process.stdout.write(USAGE);
    process.exit(opts.mode === "help" ? 0 : 2);
  }
  const allow = loadAllowlist();

  // Offline: the manifest is the baseline, no clone is touched or needed.
  if (opts.mode === "check-offline") {
    process.exit(checkOffline(allow) ? 0 : 1);
  }

  const upstreamRoot = resolveUpstream(opts.upstream);
  const pin = loadPin();

  if (opts.mode === "write-manifest") {
    // The manifest is the ONLY thing this mode may write.
    WRITE_GUARD = [HERE];
    writeManifest(upstreamRoot);
    process.exit(0);
  }

  WRITE_GUARD = [OURS, PARK_ROOT];

  let applied = null;
  if (opts.mode === "apply") {
    const before = gitRead(upstreamRoot, ["status", "--porcelain"]);
    if (before !== "" && before !== null) {
      die(`upstream checkout is not clean; refusing to run:\n${before}`);
    }
    applied = apply(upstreamRoot, allow);
    const after = gitRead(upstreamRoot, ["status", "--porcelain"]);
    if (after !== "" && after !== null) {
      die(`this run dirtied the upstream checkout — that is a bug:\n${after}`);
    }
  }

  const ok = report(opts.mode, upstreamRoot, pin, compare(upstreamRoot, allow), applied);
  process.exit(ok ? 0 : 1);
}

main();
