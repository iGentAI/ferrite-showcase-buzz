// Run: node --import ./test-loader.mjs --test e2e/teardown.test.mjs
//
// Drives the real globalTeardown against a stubbed relay (global fetch), a
// temporary created-objects log and a temporary Playwright output directory.
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import zlib from "node:zlib";

const work = fs.mkdtempSync(path.join(os.tmpdir(), "teardown-test-"));
const LOG = path.join(work, "created.jsonl");
process.env.BUZZ_CREATED_LOG = LOG;
process.env.BUZZ_TRACE_QUARANTINE = path.join(work, "quarantine");
delete process.env.BUZZ_KEEP_CREATED;
const OPERATOR_STATE = JSON.stringify({ operator_private_key: "11".repeat(32) });

/** The relay: answers each 9008 room deletion by the room id's fate. */
let fate = () => 200;
const seen = [];
globalThis.fetch = async (url, init = {}) => {
  const body = init.body ? JSON.parse(init.body) : null;
  const room = body?.tags?.find((tag) => tag[0] === "h")?.[1];
  seen.push(room ?? String(url));
  const status = fate(room);
  return new Response(status === 200 ? "{}" : "refused", { status });
};

const { default: globalTeardown } = await import("./teardown.ts");

const SECRET = "v1.session-that-must-not-ship";
/** A one-entry trace, STORED (not deflated) so the raw bytes show whether the value is there. */
function traceZip(text) {
  const data = Buffer.from(text);
  const body = data;
  const name = Buffer.from("0-trace.network");
  const local = Buffer.alloc(30);
  local.writeUInt32LE(0x04034b50, 0);
  local.writeUInt16LE(0, 8);
  local.writeUInt32LE(zlib.crc32(data), 14);
  local.writeUInt32LE(body.length, 18);
  local.writeUInt32LE(data.length, 22);
  local.writeUInt16LE(name.length, 26);
  const central = Buffer.alloc(46);
  central.writeUInt32LE(0x02014b50, 0);
  central.writeUInt16LE(0, 10);
  central.writeUInt32LE(zlib.crc32(data), 16);
  central.writeUInt32LE(body.length, 20);
  central.writeUInt32LE(data.length, 24);
  central.writeUInt16LE(name.length, 28);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(1, 8);
  end.writeUInt16LE(1, 10);
  end.writeUInt32LE(46 + name.length, 12);
  end.writeUInt32LE(30 + name.length + body.length, 16);
  return Buffer.concat([local, name, body, central, name, end]);
}

/** The content of a one-entry zip's entry, whether stored or deflated. */
function entryText(bytes) {
  const method = bytes.readUInt16LE(8);
  const start = 30 + bytes.readUInt16LE(26) + bytes.readUInt16LE(28);
  const raw = bytes.subarray(start, start + bytes.readUInt32LE(18));
  return (method === 8 ? zlib.inflateRawSync(raw) : raw).toString("latin1");
}

function freshRun({ rooms, operator = true }) {
  for (const file of fs.readdirSync(work)) {
    if (file.startsWith("created.jsonl")) fs.rmSync(path.join(work, file));
  }
  fs.writeFileSync(LOG, rooms.map((id) => `${JSON.stringify({ type: "room", id })}\n`).join(""));
  if (operator) process.env.BUZZ_OPERATOR_STATE = OPERATOR_STATE;
  else delete process.env.BUZZ_OPERATOR_STATE;
  seen.length = 0;
  fate = () => 200;
  process.exitCode = undefined;
  const outputDir = fs.mkdtempSync(path.join(work, "out-"));
  return { projects: [{ outputDir }], outputDir };
}

const rotated = () => fs.readdirSync(work).filter((file) => file.startsWith("created.jsonl.") && file.endsWith(".done"));
const logged = () => fs.readFileSync(LOG, "utf8").trim().split("\n").filter(Boolean).map((line) => JSON.parse(line).id);

test("F136: teardown scrubs the traces in Playwright's configured outputDir, not only <cwd>/test-results", async () => {
  const config = freshRun({ rooms: ["r1"] });
  const trace = path.join(config.outputDir, "journey-chromium", "trace.zip");
  fs.mkdirSync(path.dirname(trace));
  fs.writeFileSync(trace, traceZip(`cookie: __Host-ferrite_app_session=${SECRET}`));
  assert.ok(fs.readFileSync(trace).includes(SECRET), "the fixture really carries the value");

  await globalTeardown(config);

  const after = fs.readFileSync(trace);
  assert.ok(!entryText(after).includes(SECRET), "the trace under outputDir no longer carries the session");
  process.exitCode = undefined;
});

test("F329: an unreadable trace does not stop the deletions, and fails the run", async () => {
  const config = freshRun({ rooms: ["r1", "r2"] });
  const trace = path.join(config.outputDir, "broken-chromium", "trace.zip");
  fs.mkdirSync(path.dirname(trace));
  fs.writeFileSync(trace, Buffer.from("PK\u0003\u0004 truncated by a killed worker"));

  await assert.rejects(globalTeardown(config), /trace not scrubbed: broken-chromium\/trace\.zip/, "an unscrubbable trace fails the run");

  assert.deepEqual(seen, ["r1", "r2"], "every created room was still deleted");
  assert.ok(!fs.existsSync(trace), "the unreadable trace was moved out of the output directory");
  assert.equal(rotated().length, 1, "the log was still settled before the failure was raised");
  process.exitCode = undefined;
});

test("F330: with no operator key nothing is deleted and the log is kept whole", async (t) => {
  if (fs.existsSync("/tmp/buzz-agent-provision.json")) {
    t.skip("this machine has a provisioning file, so an operator key always exists");
    return;
  }
  freshRun({ rooms: ["r1", "r2"], operator: false });

  await assert.rejects(globalTeardown({ projects: [] }), /not deleted: no operator key/);

  assert.deepEqual(seen, []);
  assert.deepEqual(rotated(), [], "the worklist was not rotated away");
  assert.deepEqual(logged(), ["r1", "r2"]);
  process.exitCode = undefined;
});

test("F330: a partial failure keeps exactly the undeleted entries for the next run", async () => {
  const config = freshRun({ rooms: ["r1", "general", "r2", "r3"] });
  fate = (room) => (room === "r2" ? 500 : 200);

  await assert.rejects(globalTeardown(config), /not deleted: room r2: 500/);

  assert.deepEqual(rotated(), []);
  assert.deepEqual(logged(), ["r2"], "only the failed room is left to retry; the protected room is never retried");
  process.exitCode = undefined;
});

test("F330: a fully successful teardown rotates the log", async () => {
  const config = freshRun({ rooms: ["r1", "r2"] });

  await globalTeardown(config);

  assert.equal(rotated().length, 1);
  assert.ok(!fs.existsSync(LOG));
  assert.notEqual(process.exitCode, 1);
  process.exitCode = undefined;
});

test.after(() => fs.rmSync(work, { recursive: true, force: true }));
