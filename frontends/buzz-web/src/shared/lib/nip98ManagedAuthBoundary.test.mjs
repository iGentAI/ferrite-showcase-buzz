/**
 * CLAIM: A browser-served Buzz request never puts its inner NIP-98 credential
 * in the standard Authorization header, which Ferrite managed auth owns and
 * strips before forwarding to the app.
 *
 * ORACLE: apps/buzz/ferrite.json forwards only x-buzz-authorization on relay
 * API routes, and ingress.rs reads that header as the Authorization fallback.
 */
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, extname, join, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { BUZZ_AUTHORIZATION_HEADER } from "./nip98.ts";

const sourceRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");

function sourceFiles(directory) {
  const files = [];
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) files.push(...sourceFiles(path));
    else if ([".ts", ".tsx"].includes(extname(entry.name))) files.push(path);
  }
  return files;
}

test("managed-auth browser requests use Buzz's forwarded auth header", () => {
  assert.equal(BUZZ_AUTHORIZATION_HEADER, "X-Buzz-Authorization");

  const offenders = sourceFiles(sourceRoot)
    .filter((path) => /\bAuthorization\s*:/.test(readFileSync(path, "utf8")))
    .map((path) => path.slice(sourceRoot.length + 1));

  assert.deepEqual(
    offenders,
    [],
    "standard Authorization is consumed by Ferrite managed auth; carry the inner credential in BUZZ_AUTHORIZATION_HEADER",
  );
});
