#!/usr/bin/env node
/**
 * Push a built web bundle onto the live relay's operator assets face.
 *
 *   BUZZ_OPERATOR_SK=<64-hex> node push-ui.mjs
 *
 * The relay serves the SPA out of its `web-assets` blob plane; this script is
 * the CLIENT of the `/buzz/api/operator/assets/{begin,chunk,finish}` routes
 * (`buzz/src/relay/ingress.rs`), the way `bzcli.mjs` is the client of the rest
 * of the operator face. The SPA itself never uploads anything.
 *
 * Flow, matching the route contract exactly:
 *   begin  — the full manifest `[{path,size,sha256}]` (+ prune flag); the
 *            reply's `missing` is the diff against the COMMITTED manifest, so
 *            a re-run of an unchanged dist uploads nothing and an incremental
 *            push re-sends only changed files.
 *   chunk  — 131072-byte slices, base64. Chunks are staged idempotently, so a
 *            5xx is retried once as a plain rewrite. Concurrency is 4 ACROSS
 *            files but strictly sequential WITHIN a file: the `last: true`
 *            chunk triggers server-side assembly and must arrive after the
 *            rest of that file.
 *   finish — refused by the relay unless every path verified; prints the etag.
 *
 * Then it VERIFIES, not just reports: `/buzz/ui/index.html` and one hashed
 * asset are fetched back through the PUBLIC content route and byte-compared
 * against dist. Any refusal or mismatch exits non-zero.
 *
 * Env: BUZZ_OPERATOR_SK (required, never printed), DIST (default ./dist),
 * PRUNE=1 to delete previously-served paths the new manifest no longer names,
 * BUZZ_HOST / BUZZ_BASE as in bzcli.mjs.
 */
import fs from "node:fs";
import path from "node:path";
import { createHash } from "node:crypto";
import { getPublicKey } from "nostr-tools/pure";
import { call } from "./bzcli.mjs";
import { relayBase } from "./operator-local-state.mjs";

// Same base bzcli.mjs uses for the operator calls: plain http only for a
// loopback host, https for any other BUZZ_HOST unless BUZZ_BASE says otherwise.
const BASE = relayBase();
const DIST = process.env.DIST || "./dist";
const PRUNE = process.env.PRUNE === "1";
/** Must match `operator::CHUNK_BYTES`; the relay refuses anything larger. */
const CHUNK_BYTES = 131_072;
const CONCURRENCY = 4;

const skHex = process.env.BUZZ_OPERATOR_SK || "";
if (!/^[0-9a-f]{64}$/i.test(skHex)) {
  console.error("BUZZ_OPERATOR_SK (the operator's 64-hex secret key) is required.");
  process.exit(2);
}
const sk = new Uint8Array(skHex.match(/.{2}/g).map((b) => parseInt(b, 16)));
const operator = { sk, pk: getPublicKey(sk) };

const sha256hex = (bytes) => createHash("sha256").update(bytes).digest("hex");

/** Every file under dist, as `{path,size,sha256}` with '/'-joined relative paths. */
function manifestOf(root) {
  const files = [];
  const walk = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const absolute = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(absolute);
        continue;
      }
      const bytes = fs.readFileSync(absolute);
      files.push({
        path: path.relative(root, absolute).split(path.sep).join("/"),
        size: bytes.length,
        sha256: sha256hex(bytes),
      });
    }
  };
  walk(root);
  return files;
}

/**
 * Stay under the relay's operator rate budget (600 requests / minute /
 * pubkey, `OPERATOR_RATE_LIMIT`): the real bundle is ~500 files ≈ 630
 * requests, which tripped the 429 at full speed. Pause before the budget
 * line and let the fixed window roll over.
 */
const RATE_BUDGET = 540;
const RATE_WINDOW_MS = 65_000;
let windowStart = Date.now();
let windowCount = 0;
async function pace() {
  if (Date.now() - windowStart >= RATE_WINDOW_MS) {
    windowStart = Date.now();
    windowCount = 0;
  }
  if (windowCount >= RATE_BUDGET) {
    const wait = RATE_WINDOW_MS - (Date.now() - windowStart);
    if (wait > 0) {
      console.log(`pausing ${Math.ceil(wait / 1000)}s for the operator rate window`);
      await new Promise((resolve) => setTimeout(resolve, wait));
    }
    windowStart = Date.now();
    windowCount = 0;
  }
  windowCount += 1;
}

/** One operator POST; refusals are fatal — a partial push must not report success. */
async function post(route, body, retryOn5xx = false) {
  await pace();
  let reply = await call(operator, "POST", `/buzz/api/operator/assets/${route}`, body);
  if (retryOn5xx && reply.status >= 500) {
    // Chunk staging is an idempotent expiring rewrite; one retry is safe.
    reply = await call(operator, "POST", `/buzz/api/operator/assets/${route}`, body);
  }
  if (reply.status !== 200) {
    throw new Error(`${route} refused: ${reply.status} ${JSON.stringify(reply.body).slice(0, 300)}`);
  }
  return reply.body;
}

/**
 * The relay's `missing` list, checked against the manifest this run sent.
 *
 * The reply names files for this script to READ FROM DISK and upload, so it is
 * never trusted: a spoofed, compromised or intercepted relay could otherwise
 * answer `missing: ["../../.ssh/id_ed25519"]` and have the operator's own
 * push read and ship any local file. Every entry must be a path from `files`
 * (which `manifestOf` built from DIST), and must resolve inside DIST. Anything
 * else refuses the whole push before a single byte is sent.
 */
function checkedMissing(reply, manifest) {
  const missing = reply?.missing ?? [];
  if (!Array.isArray(missing)) throw new Error(`begin reply's missing is not a list: ${JSON.stringify(missing).slice(0, 200)}`);
  const known = new Set(manifest.map((file) => file.path));
  const refused = missing.filter((entry) => typeof entry !== "string" || !known.has(entry) || !insideDist(entry));
  if (refused.length > 0) {
    throw new Error(
      `the relay asked for ${refused.length} path(s) this push did not offer: ` +
        `${JSON.stringify(refused.slice(0, 5)).slice(0, 300)}; refusing to read or upload them`,
    );
  }
  return [...new Set(missing)];
}

/** True when `relPath` is relative, has no `..` segment, and resolves under DIST. */
function insideDist(relPath) {
  if (relPath === "" || path.isAbsolute(relPath) || relPath.split(/[\\/]/).includes("..")) return false;
  const root = path.resolve(DIST);
  const resolved = path.resolve(root, relPath);
  return resolved.startsWith(root + path.sep);
}

/** Upload one file's chunks IN ORDER; `last` triggers server-side assembly. */
async function pushFile(upload, relPath) {
  if (!insideDist(relPath)) throw new Error(`refusing to read ${JSON.stringify(relPath)}: outside ${DIST}`);
  const bytes = fs.readFileSync(path.join(DIST, relPath));
  const count = Math.max(1, Math.ceil(bytes.length / CHUNK_BYTES));
  for (let index = 0; index < count; index += 1) {
    const offset = index * CHUNK_BYTES;
    const reply = await post(
      "chunk",
      {
        upload,
        path: relPath,
        offset,
        data: bytes.subarray(offset, offset + CHUNK_BYTES).toString("base64"),
        last: index === count - 1,
      },
      true,
    );
    if (index === count - 1 && reply.complete !== true) {
      throw new Error(`${relPath}: the last chunk did not complete the file`);
    }
  }
  return count;
}

/** Fetch one path back through the content route and byte-compare when public. */
async function verifyServed(relPath) {
  const res = await fetch(`${BASE}/buzz/ui/${relPath}`, { redirect: "manual" });
  if (res.status === 401 || res.status === 403) return false;
  if ([301, 302, 303, 307, 308].includes(res.status)) {
    const location = res.headers.get("location") ?? "";
    // The legacy sign-in path and the SSO gate's start path both mean the
    // route is gated, not that the bytes are wrong.
    if (location.includes("/_ferrite/auth/sign-in") || location.includes("/sso/start")) {
      return false;
    }
    throw new Error(`verify ${relPath}: unexpected HTTP ${res.status} redirect to ${location}`);
  }
  if (!res.ok) throw new Error(`verify ${relPath}: HTTP ${res.status}`);
  const served = Buffer.from(await res.arrayBuffer());
  const local = fs.readFileSync(path.join(DIST, relPath));
  if (!served.equals(local)) {
    throw new Error(`verify ${relPath}: served bytes differ from dist (${served.length} vs ${local.length})`);
  }
  return true;
}

/**
 * A managed-auth protected UI cannot be fetched by this non-browser publisher.
 * Verify the relay's committed manifest instead, then leave the actual served
 * response to the authenticated browser acceptance that follows deployment.
 */
async function verifyCommittedManifest(expected) {
  const reply = await call(
    operator,
    "GET",
    "/buzz/api/operator/config/buzz-web-assets-manifest",
  );
  if (reply.status !== 200 || reply.body?.present !== true) {
    throw new Error(`manifest read refused: ${reply.status} ${JSON.stringify(reply.body).slice(0, 300)}`);
  }
  const committed = reply.body?.value?.files;
  if (!Array.isArray(committed) || committed.length !== expected.length) {
    throw new Error(`committed manifest has ${committed?.length ?? "no"} files; expected ${expected.length}`);
  }
  const actual = new Map(committed.map((file) => [file.path, file]));
  for (const file of expected) {
    const got = actual.get(file.path);
    if (!got || got.size !== file.size || got.sha256 !== file.sha256) {
      throw new Error(`committed manifest differs at ${file.path}`);
    }
  }
  return reply.body.value.etag;
}

const files = manifestOf(DIST);
if (!files.some((f) => f.path === "index.html")) {
  throw new Error(`${DIST} has no index.html — is this a built bundle?`);
}
const totalBytes = files.reduce((n, f) => n + f.size, 0);
console.log(`bundle: ${files.length} files, ${(totalBytes / 1e6).toFixed(1)} MB, prune=${PRUNE}`);

const begun = await post("begin", { files, prune: PRUNE });
const missing = checkedMissing(begun, files);
console.log(`upload ${begun.upload}: ${missing.length} file(s) to send, ${files.length - missing.length} unchanged`);

// 4 parallel lanes over the missing-file queue; each lane owns one file at a
// time, so within-file chunk order is preserved while lanes overlap.
const queue = [...missing];
let chunksSent = 0;
await Promise.all(
  Array.from({ length: CONCURRENCY }, async () => {
    for (;;) {
      const relPath = queue.shift();
      if (relPath === undefined) return;
      // Await first, then add: `chunksSent += await …` reads the counter
      // before the await, so the four lanes would overwrite each other's counts.
      const sent = await pushFile(begun.upload, relPath);
      chunksSent += sent;
    }
  }),
);
console.log(`sent ${chunksSent} chunk(s)`);

const finished = await post("finish", { upload: begun.upload });

// Trust nothing until the public route serves the bytes back. index.html is
// the entry; the hashed asset proves the fingerprinted tree landed too.
const indexPublic = await verifyServed("index.html");
const hashed =
  files.find((f) => /assets\/.+-[A-Za-z0-9_-]{8,}\./.test(f.path)) ??
  files.find((f) => f.path !== "index.html");
// The asset's own verdict counts: a public index with gated assets has not
// had its asset bytes compared, so it falls back to the committed manifest.
const assetPublic = indexPublic && hashed ? await verifyServed(hashed.path) : indexPublic;

let verification;
if (indexPublic && assetPublic) {
  verification = `verified index.html${hashed ? ` + ${hashed.path}` : ""}`;
} else {
  const manifestEtag = await verifyCommittedManifest(files);
  if (manifestEtag !== finished.etag) {
    throw new Error(`finish etag ${finished.etag} differs from committed manifest ${manifestEtag}`);
  }
  verification = "verified protected-route committed manifest; authenticated browser read required";
}

console.log(
  `committed etag ${finished.etag}: ${finished.files} file(s) live, ${verification}`,
);
