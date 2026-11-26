// Run: node --import ./test-loader.mjs --test e2e/traceScrub.test.mjs
// (plain `node --test` works too: node strips the types).
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import zlib from "node:zlib";

import { redactSessionCookie, scrubTrace, scrubTraces, stillCarriesSessionCookie } from "./traceScrub.ts";

const SECRET = "v1.eyJzdWIiOiJidXp6LWUyZS1hbmFAZXhhbXBsZSJ9.c2lnbmF0dXJl";
const NETWORK = `{"cookies":[{"name":"__Host-ferrite_app_session","value":"${SECRET}"}]}`;

const tmp = (label) => fs.mkdtempSync(path.join(os.tmpdir(), `trace-scrub-${label}-`));

test("both cookie spellings a trace carries are redacted", () => {
  const header = `cookie: theme=dark; __Host-ferrite_app_session=${SECRET}; other=1`;
  assert.equal(redactSessionCookie(header), "cookie: theme=dark; __Host-ferrite_app_session=redacted; other=1");
  assert.equal(redactSessionCookie(NETWORK), '{"cookies":[{"name":"__Host-ferrite_app_session","value":"redacted"}]}');
  assert.equal(redactSessionCookie("nothing to see"), "nothing to see");
});

test("a value-first JSON cookie is redacted too, and a redacted text carries nothing", () => {
  const reordered = `{"value":"${SECRET}","name":"__Host-ferrite_app_session"}`;
  const clean = redactSessionCookie(reordered);
  assert.equal(clean, '{"value":"redacted","name":"__Host-ferrite_app_session"}');
  assert.equal(stillCarriesSessionCookie(clean), false);
  assert.equal(stillCarriesSessionCookie("__Host-ferrite_app_session=; Max-Age=0"), false);
  assert.equal(stillCarriesSessionCookie(`__Host-ferrite_app_session%3D${SECRET}`), true);
});

/**
 * A zip writer that can express what Playwright and other tools emit:
 * timestamps, attributes, extra fields, entry and archive comments, directory
 * entries, and entries streamed with a data descriptor.
 */
function zip(entries, { comment = Buffer.alloc(0) } = {}) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const entry of entries) {
    const {
      name,
      text = "",
      method = 8,
      time = 0,
      date = 0,
      externalAttr = 0,
      extra = Buffer.alloc(0),
      localExtra = Buffer.alloc(0),
      entryComment = Buffer.alloc(0),
      descriptor = false,
    } = entry;
    const data = Buffer.isBuffer(text) ? text : Buffer.from(text);
    const body = entry.raw ?? (method === 8 ? zlib.deflateRawSync(data) : data);
    const crc = zlib.crc32(data);
    const nameBytes = Buffer.from(name);
    const flags = 0x0800 | (descriptor ? 0x0008 : 0);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(flags, 6);
    local.writeUInt16LE(method, 8);
    local.writeUInt16LE(time, 10);
    local.writeUInt16LE(date, 12);
    if (!descriptor) {
      local.writeUInt32LE(crc, 14);
      local.writeUInt32LE(body.length, 18);
      local.writeUInt32LE(data.length, 22);
    }
    local.writeUInt16LE(nameBytes.length, 26);
    local.writeUInt16LE(localExtra.length, 28);
    const trailer = [];
    if (descriptor) {
      const dd = Buffer.alloc(16);
      dd.writeUInt32LE(0x08074b50, 0);
      dd.writeUInt32LE(crc, 4);
      dd.writeUInt32LE(body.length, 8);
      dd.writeUInt32LE(data.length, 12);
      trailer.push(dd);
    }
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(0x031e, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(flags, 8);
    central.writeUInt16LE(method, 10);
    central.writeUInt16LE(time, 12);
    central.writeUInt16LE(date, 14);
    central.writeUInt32LE(crc, 16);
    central.writeUInt32LE(body.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(nameBytes.length, 28);
    central.writeUInt16LE(extra.length, 30);
    central.writeUInt16LE(entryComment.length, 32);
    central.writeUInt32LE(externalAttr, 38);
    central.writeUInt32LE(offset, 42);
    locals.push(local, nameBytes, localExtra, body, ...trailer);
    centrals.push(central, nameBytes, extra, entryComment);
    offset += 30 + nameBytes.length + localExtra.length + body.length + trailer.reduce((n, b) => n + b.length, 0);
  }
  const directory = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  end.writeUInt16LE(comment.length, 20);
  return Buffer.concat([...locals, directory, end, comment]);
}

/** An independent central-directory reader, so the test does not trust the subject's. */
function listing(bytes) {
  let end = bytes.length - 22;
  while (end >= 0 && !(bytes.readUInt32LE(end) === 0x06054b50 && end + 22 + bytes.readUInt16LE(end + 20) === bytes.length)) end -= 1;
  assert.ok(end >= 0, "the rewrite has an end record");
  const count = bytes.readUInt16LE(end + 10);
  let offset = bytes.readUInt32LE(end + 16);
  const out = [];
  for (let i = 0; i < count; i += 1) {
    const nameLength = bytes.readUInt16LE(offset + 28);
    const extraLength = bytes.readUInt16LE(offset + 30);
    const commentLength = bytes.readUInt16LE(offset + 32);
    const local = bytes.readUInt32LE(offset + 42);
    const method = bytes.readUInt16LE(offset + 10);
    const compressed = bytes.readUInt32LE(offset + 20);
    const start = local + 30 + bytes.readUInt16LE(local + 26) + bytes.readUInt16LE(local + 28);
    const raw = bytes.subarray(start, start + compressed);
    out.push({
      name: bytes.subarray(offset + 46, offset + 46 + nameLength).toString(),
      method,
      time: bytes.readUInt16LE(offset + 12),
      date: bytes.readUInt16LE(offset + 14),
      externalAttr: bytes.readUInt32LE(offset + 38),
      extra: bytes.subarray(offset + 46 + nameLength, offset + 46 + nameLength + extraLength),
      comment: bytes.subarray(offset + 46 + nameLength + extraLength, offset + 46 + nameLength + extraLength + commentLength),
      raw,
      data: method === 8 ? zlib.inflateRawSync(raw) : raw,
    });
    offset += 46 + nameLength + extraLength + commentLength;
  }
  return { entries: out, comment: bytes.subarray(end + 22) };
}

test("a trace zip is rewritten with no cookie value left in any entry", () => {
  const dir = tmp("basic");
  const file = path.join(dir, "trace.zip");
  fs.writeFileSync(
    file,
    zip([
      { name: "0-trace.network", text: NETWORK, method: 8 },
      { name: "0-trace.trace", text: `{"storageState":${NETWORK}}`, method: 0 },
      { name: "resources/page.html", text: "<html>untouched</html>", method: 8 },
    ]),
  );
  assert.equal(scrubTrace(file), 2);
  // Re-reading the rewritten zip finds the cookie name but no value left to
  // redact, so the rewrite is a readable zip and every value is gone.
  assert.equal(scrubTrace(file), 0, "a scrubbed trace has nothing left to redact");
  assert.ok(listing(fs.readFileSync(file)).entries.every((entry) => !entry.data.includes(SECRET)), "no stored copy survives");
  assert.deepEqual(fs.readdirSync(dir), ["trace.zip"], "no temporary file is left behind");
  fs.rmSync(dir, { recursive: true, force: true });
});

test("F331: a rewrite keeps entry metadata, directory entries and the archive comment, and copies untouched entries verbatim", () => {
  const dir = tmp("metadata");
  const file = path.join(dir, "trace.zip");
  const extra = Buffer.from([0x55, 0x54, 0x05, 0x00, 0x01, 0x10, 0x20, 0x30, 0x40]); // an extended-timestamp field
  // An archive comment that contains an end-of-central-directory signature.
  const comment = Buffer.concat([Buffer.from("note "), Buffer.from([0x50, 0x4b, 0x05, 0x06]), Buffer.alloc(18, 7)]);
  const original = zip(
    [
      { name: "resources/", text: "", method: 0, time: 0x6000, date: 0x5a21, externalAttr: 0x41ed0010 },
      { name: "0-trace.network", text: NETWORK, method: 8, time: 0x6001, date: 0x5a21, externalAttr: 0x81a40000, extra, entryComment: Buffer.from("net") },
      { name: "resources/app.js", text: "console.log('untouched')", method: 8, time: 0x6002, date: 0x5a21, externalAttr: 0x81a40000, descriptor: true },
      { name: "0-trace.trace", text: `{"storageState":${NETWORK}}`, method: 0, time: 0x6003, date: 0x5a21, descriptor: true },
    ],
    { comment },
  );
  fs.writeFileSync(file, original);
  const before = listing(original);

  assert.equal(scrubTrace(file), 2);

  const after = listing(fs.readFileSync(file));
  assert.deepEqual(after.entries.map((e) => e.name), before.entries.map((e) => e.name));
  assert.deepEqual(after.comment, before.comment, "the archive comment survives");
  for (const [index, entry] of after.entries.entries()) {
    const was = before.entries[index];
    assert.equal(entry.time, was.time, `${entry.name} keeps its time`);
    assert.equal(entry.date, was.date, `${entry.name} keeps its date`);
    assert.equal(entry.externalAttr, was.externalAttr, `${entry.name} keeps its attributes`);
    assert.deepEqual(entry.extra, was.extra, `${entry.name} keeps its extra field`);
    assert.deepEqual(entry.comment, was.comment, `${entry.name} keeps its comment`);
    assert.ok(!entry.data.includes(SECRET), `${entry.name} carries no session`);
  }
  const untouched = after.entries.find((e) => e.name === "resources/app.js");
  const untouchedBefore = before.entries.find((e) => e.name === "resources/app.js");
  assert.deepEqual(untouched.raw, untouchedBefore.raw, "an untouched entry's stored bytes are copied verbatim");
  assert.equal(untouched.method, untouchedBefore.method);
  fs.rmSync(dir, { recursive: true, force: true });
});

test("F136/F329: one unreadable trace is quarantined and the walk goes on; other zips are not touched", () => {
  const root = tmp("walk");
  const quarantineDir = tmp("quarantine");
  const good = zip([{ name: "0-trace.network", text: NETWORK }]);
  const truncated = good.subarray(0, good.length - 30);
  const attachment = Buffer.from("PK not really a zip");
  for (const [rel, bytes] of [
    ["a-failed/trace.zip", truncated],
    ["b-failed/trace.zip", good],
    ["c-attachment/report.zip", attachment],
  ]) {
    fs.mkdirSync(path.join(root, path.dirname(rel)), { recursive: true });
    fs.writeFileSync(path.join(root, rel), bytes);
  }

  const report = scrubTraces(root, { quarantineDir });

  assert.equal(report.found, true);
  assert.equal(report.seen, 2, "only trace files are counted");
  assert.equal(report.rewritten, 1);
  assert.equal(report.failures.length, 1);
  assert.match(report.failures[0], /a-failed\/trace\.zip/);
  assert.ok(!fs.existsSync(path.join(root, "a-failed/trace.zip")), "the unreadable trace left the output directory");
  const [slot] = fs.readdirSync(quarantineDir);
  assert.deepEqual(fs.readFileSync(path.join(quarantineDir, slot, "trace.zip")), truncated, "quarantined unchanged");
  assert.ok(
    listing(fs.readFileSync(path.join(root, "b-failed/trace.zip"))).entries.every((entry) => !entry.data.includes(SECRET)),
    "the next trace was still scrubbed",
  );
  assert.deepEqual(fs.readFileSync(path.join(root, "c-attachment/report.zip")), attachment, "a non-trace zip is left alone");
  fs.rmSync(root, { recursive: true, force: true });
  fs.rmSync(quarantineDir, { recursive: true, force: true });
});

test("F136: a trace naming the cookie in a form the redactor does not know is quarantined, not kept", () => {
  const root = tmp("unknown-form");
  const quarantineDir = tmp("quarantine");
  fs.mkdirSync(path.join(root, "t"));
  const file = path.join(root, "t", "trace.zip");
  fs.writeFileSync(file, zip([{ name: "0-trace.network", text: `GET /x?c=__Host-ferrite_app_session%3D${SECRET}` }]));

  const report = scrubTraces(root, { quarantineDir });

  assert.equal(report.failures.length, 1);
  assert.match(report.failures[0], /does not recognise/);
  assert.ok(!fs.existsSync(file), "no trace that may carry a live session stays in the output directory");
  fs.rmSync(root, { recursive: true, force: true });
  fs.rmSync(quarantineDir, { recursive: true, force: true });
});

test("F331: an unsupported method or a Zip64 archive is quarantined byte for byte, never rewritten", () => {
  const root = tmp("unsupported");
  const quarantineDir = tmp("quarantine");
  const bzip2 = zip([{ name: "0-trace.network", text: NETWORK, method: 12, raw: Buffer.from("BZh9...") }]);
  const zip64 = Buffer.from(zip([{ name: "0-trace.network", text: NETWORK }]));
  zip64.writeUInt16LE(0xffff, zip64.length - 22 + 8);
  zip64.writeUInt16LE(0xffff, zip64.length - 22 + 10);
  zip64.writeUInt32LE(0xffffffff, zip64.length - 22 + 16);
  fs.mkdirSync(path.join(root, "a"));
  fs.mkdirSync(path.join(root, "b"));
  fs.writeFileSync(path.join(root, "a", "trace.zip"), bzip2);
  fs.writeFileSync(path.join(root, "b", "trace.zip"), zip64);

  const report = scrubTraces(root, { quarantineDir });

  assert.equal(report.seen, 2);
  assert.equal(report.failures.length, 2);
  const kept = fs.readdirSync(quarantineDir).map((slot) => fs.readFileSync(path.join(quarantineDir, slot, "trace.zip")));
  assert.ok(kept.some((bytes) => bytes.equals(bzip2)), "the unsupported-method trace is kept intact");
  assert.ok(kept.some((bytes) => bytes.equals(zip64)), "the Zip64 trace is kept intact");
  fs.rmSync(root, { recursive: true, force: true });
  fs.rmSync(quarantineDir, { recursive: true, force: true });
});

test("F136: a missing output directory is reported, not silently counted as clean", () => {
  const report = scrubTraces(path.join(os.tmpdir(), "does-not-exist-trace-root"));
  assert.equal(report.found, false);
  assert.equal(report.seen, 0);
});

/** Scrub one trace file in a fresh root; answers the report and where things ended up. */
function scrubOne(bytes, options = {}) {
  const root = tmp("one");
  const quarantineDir = options.quarantineDir ?? tmp("quarantine");
  fs.mkdirSync(path.join(root, "t"));
  const file = path.join(root, "t", "trace.zip");
  fs.writeFileSync(file, bytes);
  const report = scrubTraces(root, { quarantineDir, outputRoots: options.outputRoots });
  return { root, file, quarantineDir, report };
}

const endOf = (bytes) => bytes.length - 22 - bytes.readUInt16LE(bytes.length - 2);

test("review: a falsified entry count cannot hide the records that carry the session", () => {
  const good = zip([{ name: "0-trace.network", text: NETWORK, method: 0 }]);
  const zeroCount = Buffer.from(good);
  zeroCount.writeUInt16LE(0, endOf(zeroCount) + 8);
  zeroCount.writeUInt16LE(0, endOf(zeroCount) + 10);
  const emptyDirectory = Buffer.from(good);
  const end = endOf(emptyDirectory);
  emptyDirectory.writeUInt16LE(0, end + 8);
  emptyDirectory.writeUInt16LE(0, end + 10);
  emptyDirectory.writeUInt32LE(0, end + 12);
  emptyDirectory.writeUInt32LE(end, end + 16);
  const diskMismatch = Buffer.from(good);
  diskMismatch.writeUInt16LE(0, endOf(diskMismatch) + 8);
  for (const [label, bytes] of [["zero count", zeroCount], ["empty directory", emptyDirectory], ["disk count mismatch", diskMismatch]]) {
    const { root, file, quarantineDir, report } = scrubOne(bytes);
    assert.equal(report.failures.length, 1, `${label}: quarantined, not reported clean`);
    assert.ok(!fs.existsSync(file), `${label}: the trace left the output directory`);
    fs.rmSync(root, { recursive: true, force: true });
    fs.rmSync(quarantineDir, { recursive: true, force: true });
  }
});

test("review: a session in a name, comment or extra field, or a value repeated elsewhere, is not declared clean", () => {
  const cases = {
    "entry comment": zip([{ name: "a", text: "x", entryComment: Buffer.from(`__Host-ferrite_app_session=${SECRET}`) }]),
    "archive comment": zip([{ name: "a", text: "x" }], { comment: Buffer.from(`__Host-ferrite_app_session=${SECRET}`) }),
    "file name": zip([
      { name: "0-trace.network", text: NETWORK },
      { name: `resources/${SECRET}.json`, text: "{}" },
    ]),
    "extra field": zip([
      { name: "0-trace.network", text: NETWORK },
      { name: "b", text: "y", extra: Buffer.concat([Buffer.from([0x99, 0x99, SECRET.length, 0]), Buffer.from(SECRET)]) },
    ]),
    "another entry": zip([
      { name: "0-trace.network", text: NETWORK },
      { name: "resources/app.js", text: `const token = "${SECRET}";` },
    ]),
  };
  for (const [label, bytes] of Object.entries(cases)) {
    const { root, file, quarantineDir, report } = scrubOne(bytes);
    assert.equal(report.failures.length, 1, `${label}: quarantined`);
    assert.ok(!fs.existsSync(file), `${label}: the trace left the output directory`);
    const [slot] = fs.readdirSync(quarantineDir);
    assert.deepEqual(fs.readFileSync(path.join(quarantineDir, slot, "trace.zip")), bytes, `${label}: preserved unchanged`);
    fs.rmSync(root, { recursive: true, force: true });
    fs.rmSync(quarantineDir, { recursive: true, force: true });
  }
});

// The fallback is a fresh mkdtemp slot in the sticky temp directory, never a
// fixed name another user could pre-create.
const fallbackQuarantine = () => path.join(os.tmpdir(), "buzz-trace-quarantine-");
const slotOf = (failure) => path.dirname(failure.split("moved to ")[1]);
const unknownForm = () => zip([{ name: "0-trace.network", text: `x=__Host-ferrite_app_session%3D${SECRET}` }]);

test("review: a quarantine directory named '..quarantine' inside the output root is refused, and the trace is preserved elsewhere", () => {
  const root = tmp("dotdot");
  const quarantineDir = path.join(root, "..quarantine");
  fs.mkdirSync(path.join(root, "t"));
  const file = path.join(root, "t", "trace.zip");
  fs.writeFileSync(file, unknownForm());
  const report = scrubTraces(root, { quarantineDir });
  assert.equal(report.failures.length, 1);
  assert.ok(!fs.existsSync(file));
  assert.ok(!fs.existsSync(quarantineDir) || fs.readdirSync(quarantineDir).length === 0, "nothing was left inside the output root");
  assert.match(report.failures[0], new RegExp(`moved to ${fallbackQuarantine().replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`));
  fs.rmSync(root, { recursive: true, force: true });
  fs.rmSync(slotOf(report.failures[0]), { recursive: true, force: true });
});

test("review: a symlink into the output root, or another project's output root, is not a quarantine", () => {
  const root = tmp("alias");
  const other = tmp("other-project");
  const link = path.join(tmp("links"), "q");
  fs.mkdirSync(path.join(root, "inside"));
  fs.symlinkSync(path.join(root, "inside"), link);
  for (const [label, quarantineDir, outputRoots] of [
    ["symlink alias", link, []],
    ["another project's output root", path.join(other, "q"), [other]],
  ]) {
    fs.mkdirSync(path.join(root, "t"), { recursive: true });
    const file = path.join(root, "t", "trace.zip");
    fs.writeFileSync(file, unknownForm());
    const report = scrubTraces(root, { quarantineDir, outputRoots });
    assert.equal(report.failures.length, 1, label);
    assert.ok(!fs.existsSync(file), label);
    assert.match(report.failures[0], /moved to /, label);
    const destination = fs.realpathSync(report.failures[0].split("moved to ")[1]);
    for (const forbidden of [root, other]) {
      assert.ok(!destination.startsWith(fs.realpathSync(forbidden) + path.sep), `${label}: not moved into ${forbidden}`);
    }
    assert.equal(fs.readdirSync(path.join(root, "inside")).length, 0, `${label}: nothing landed behind the symlink`);
  }
  fs.rmSync(root, { recursive: true, force: true });
  fs.rmSync(other, { recursive: true, force: true });
  fs.rmSync(path.dirname(link), { recursive: true, force: true });
  fs.rmSync(fallbackQuarantine(), { recursive: true, force: true });
});

test("review: a directory that cannot be read is moved out of the output root whole", (t) => {
  if (process.getuid?.() === 0) {
    t.skip("root can read any directory");
    return;
  }
  const root = tmp("unreadable");
  const quarantineDir = tmp("quarantine");
  const locked = path.join(root, "locked-chromium");
  fs.mkdirSync(locked);
  fs.writeFileSync(path.join(locked, "trace.zip"), unknownForm());
  fs.chmodSync(locked, 0o000);
  try {
    const report = scrubTraces(root, { quarantineDir });
    assert.equal(report.failures.length, 1);
    assert.match(report.failures[0], /could not be read.*moved to/);
    assert.ok(!fs.existsSync(locked), "the uninspectable directory left the output root");
  } finally {
    for (const slot of fs.readdirSync(quarantineDir)) {
      for (const item of fs.readdirSync(path.join(quarantineDir, slot))) fs.chmodSync(path.join(quarantineDir, slot, item), 0o700);
    }
    if (fs.existsSync(locked)) fs.chmodSync(locked, 0o700);
    fs.rmSync(root, { recursive: true, force: true });
    fs.rmSync(quarantineDir, { recursive: true, force: true });
  }
});

test("finding_8baa5ec1788e631d: quoted, escaped or space-led cookie values are never treated as empty", () => {
  for (const text of [
    `Cookie: __Host-ferrite_app_session="${SECRET}"`,
    `{"cookie":"__Host-ferrite_app_session=\\"${SECRET}\\""}`,
    `Cookie: __Host-ferrite_app_session= ${SECRET}`,
  ]) {
    const redacted = redactSessionCookie(text);
    assert.ok(
      !redacted.includes(SECRET) || stillCarriesSessionCookie(redacted),
      `a live value must be redacted or flagged: ${text}`,
    );
  }
  assert.equal(stillCarriesSessionCookie("Cookie: __Host-ferrite_app_session=redacted; a=b"), false);
  assert.equal(stillCarriesSessionCookie("Cookie: __Host-ferrite_app_session=; a=b"), false);
  const json = `{"name":"__Host-ferrite_app_session","value":"\\"${SECRET}\\""}`;
  assert.ok(!redactSessionCookie(json).includes(SECRET), "an escaped JSON value is redacted whole");
});

test("finding_f048d00e56e0c1bf: a configured quarantine directory another user could write is not used", () => {
  const root = tmp("open-quarantine");
  const quarantineDir = tmp("shared-quarantine");
  fs.chmodSync(quarantineDir, 0o777);
  fs.mkdirSync(path.join(root, "t"));
  fs.writeFileSync(path.join(root, "t", "trace.zip"), unknownForm());
  const report = scrubTraces(root, { quarantineDir });
  assert.equal(report.failures.length, 1);
  assert.deepEqual(fs.readdirSync(quarantineDir), [], "nothing was moved into the shared directory");
  assert.match(report.failures[0], /moved to /);
  fs.rmSync(slotOf(report.failures[0]), { recursive: true, force: true });
  fs.rmSync(root, { recursive: true, force: true });
  fs.rmSync(quarantineDir, { recursive: true, force: true });
});
