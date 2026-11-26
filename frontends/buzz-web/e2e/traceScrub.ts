/**
 * Remove the SSO broker session cookie from retained Playwright traces.
 *
 * Measured 2026-09-12: a failing journey's `trace.zip` carried the
 * `__Host-ferrite_app_session` value 47 times in `0-trace.network` (request
 * cookies) and once in `0-trace.trace` (the context's storageState). A broker
 * session is stateless and lives 24 h (`packages/ferrite-sso/src/gate.rs`), and
 * `/_sso/logout` only clears the browser's copy, so a shared trace would hand
 * out a working gate session. Playwright has no option to omit network data
 * from a trace, so `globalTeardown` rewrites every retained trace instead.
 *
 * This is the only control between a live session and a CI artefact, so it
 * FAILS CLOSED. A trace this module cannot prove clean (it will not parse, its
 * directory and records disagree, it uses Zip64 or an unknown compression
 * method, or any entry, name, extra field or comment still names the cookie or
 * carries a session value after redaction) is moved, unchanged, out of every
 * output directory into a private quarantine directory and reported as a
 * failure. Deletion is the last resort, used only when no private quarantine
 * can be made, because a live session must never stay where it would be
 * uploaded.
 *
 * The zip is handled with Node's own zlib; no third-party zip code runs. An
 * entry that never mentions the cookie is copied byte for byte, compressed
 * bytes and all. A redacted entry is deflated again. Every entry keeps its
 * timestamps, attributes, extra fields and comment, directory entries and the
 * archive comment are kept, and the rewrite goes to a temporary file that must
 * re-read cleanly before it replaces the original.
 */
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import zlib from "node:zlib";

const COOKIE = "__Host-ferrite_app_session";

/** Redact every value of the session cookie in one text, header or JSON form. */
export function redactSessionCookie(text: string): string {
  return text
    .replace(/(__Host-ferrite_app_session=)[^;\s"\\,]+/g, "$1redacted")
    .replace(
      /("name"\s*:\s*"__Host-ferrite_app_session"\s*,\s*"value"\s*:\s*")(?:[^"\\]|\\.)*/g,
      "$1redacted",
    )
    .replace(
      /("value"\s*:\s*")(?:[^"\\]|\\.)*("\s*,\s*"name"\s*:\s*"__Host-ferrite_app_session")/g,
      "$1redacted$2",
    );
}

/**
 * The spellings of the cookie that carry no value: redacted, or genuinely
 * empty. An optional `redacted` followed by a quote, backslash, comma or space
 * used to count as empty, so a quoted (`="v1…"`), JSON-escaped (`=\"v1…`) or
 * space-led value was neither redacted nor flagged (finding_8baa5ec1788e631d).
 */
const SAFE_FORMS = [
  /__Host-ferrite_app_session=redacted(?=[;\s"\\,]|$)/g,
  /__Host-ferrite_app_session=(?=;|"(?=\s*[,}\]])|$)/g,
  /"name"\s*:\s*"__Host-ferrite_app_session"\s*,\s*"value"\s*:\s*"(?:redacted)?"/g,
  /"value"\s*:\s*"(?:redacted)?"\s*,\s*"name"\s*:\s*"__Host-ferrite_app_session"/g,
];

/**
 * Whether a text still names the cookie anywhere other than in a known,
 * value-free spelling. Any other mention (URL-encoded, reordered, embedded in a
 * form this module has never seen) counts as a possible leak.
 */
export function stillCarriesSessionCookie(text: string): boolean {
  if (!text.includes(COOKIE)) return false;
  let rest = text;
  for (const form of SAFE_FORMS) rest = rest.replace(form, "");
  return rest.includes(COOKIE);
}

const VALUE_FORMS = [
  /__Host-ferrite_app_session=([^;\s"\\,]+)/g,
  /"name"\s*:\s*"__Host-ferrite_app_session"\s*,\s*"value"\s*:\s*"((?:[^"\\]|\\.)*)"/g,
  /"value"\s*:\s*"((?:[^"\\]|\\.)*)"\s*,\s*"name"\s*:\s*"__Host-ferrite_app_session"/g,
];

/**
 * The session values a text carries. After a rewrite, none of them may appear
 * anywhere in the archive, with or without the cookie's name beside it.
 */
export function sessionValues(text: string): string[] {
  const values: string[] = [];
  for (const form of VALUE_FORMS) {
    for (const match of text.matchAll(form)) {
      const value = match[1];
      if (value && value !== "redacted" && value.length >= 8) values.push(value);
    }
  }
  return values;
}

/** A trace this module will not rewrite; the caller quarantines it. */
export class TraceScrubError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "TraceScrubError";
  }
}

type Entry = {
  name: string;
  nameBytes: Buffer;
  versionMadeBy: number;
  versionNeeded: number;
  flags: number;
  method: number;
  time: number;
  date: number;
  crc: number;
  internalAttr: number;
  externalAttr: number;
  extra: Buffer;
  localExtra: Buffer;
  comment: Buffer;
  /** The stored (possibly compressed) bytes, exactly as read. */
  raw: Buffer;
  /** The entry's content. */
  data: Buffer;
  /** Set when the content changed; the entry is then deflated again. */
  replacement?: Buffer;
};

type Archive = { entries: Entry[]; comment: Buffer };

const SIG_LOCAL = 0x04034b50;
const SIG_CENTRAL = 0x02014b50;
const SIG_END = 0x06054b50;
const SIG_ZIP64_LOCATOR = 0x07064b50;
const ZIP64_EXTRA = 0x0001;

function u16(bytes: Buffer, at: number): number {
  if (at < 0 || at + 2 > bytes.length) throw new TraceScrubError("truncated zip");
  return bytes.readUInt16LE(at);
}

function u32(bytes: Buffer, at: number): number {
  if (at < 0 || at + 4 > bytes.length) throw new TraceScrubError("truncated zip");
  return bytes.readUInt32LE(at);
}

function slice(bytes: Buffer, at: number, length: number): Buffer {
  if (at < 0 || length < 0 || at + length > bytes.length) throw new TraceScrubError("truncated zip");
  return bytes.subarray(at, at + length);
}

/**
 * The end-of-central-directory record: the one whose comment ends exactly at
 * the end of the file and whose directory ends exactly where it starts. A
 * `PK\x05\x06` inside an archive comment satisfies neither.
 */
function findEnd(bytes: Buffer): number {
  const lowest = Math.max(0, bytes.length - 22 - 0xffff);
  for (let at = bytes.length - 22; at >= lowest; at -= 1) {
    if (bytes.readUInt32LE(at) !== SIG_END) continue;
    if (at + 22 + bytes.readUInt16LE(at + 20) !== bytes.length) continue;
    if (bytes.readUInt32LE(at + 16) + bytes.readUInt32LE(at + 12) !== at) {
      // Either Zip64 sentinels or a directory that does not end here.
      if (bytes.readUInt32LE(at + 16) === 0xffffffff || bytes.readUInt32LE(at + 12) === 0xffffffff) {
        throw new TraceScrubError("Zip64 archives are not supported");
      }
      continue;
    }
    return at;
  }
  throw new TraceScrubError("not a zip: no end-of-central-directory record");
}

function hasZip64Extra(extra: Buffer): boolean {
  for (let at = 0; at + 4 <= extra.length; ) {
    if (extra.readUInt16LE(at) === ZIP64_EXTRA) return true;
    at += 4 + extra.readUInt16LE(at + 2);
  }
  return false;
}

function readArchive(bytes: Buffer): Archive {
  const end = findEnd(bytes);
  if (end >= 20 && bytes.readUInt32LE(end - 20) === SIG_ZIP64_LOCATOR) {
    throw new TraceScrubError("Zip64 archives are not supported");
  }
  if (u16(bytes, end + 4) !== 0 || u16(bytes, end + 6) !== 0) {
    throw new TraceScrubError("multi-disk archives are not supported");
  }
  const count = u16(bytes, end + 10);
  if (count === 0xffff) throw new TraceScrubError("Zip64 archives are not supported");
  if (u16(bytes, end + 8) !== count) {
    throw new TraceScrubError("the entry counts in the end record disagree");
  }
  const directorySize = u32(bytes, end + 12);
  const directoryStart = u32(bytes, end + 16);
  const comment = Buffer.from(slice(bytes, end + 22, u16(bytes, end + 20)));
  let offset = directoryStart;
  const entries: Entry[] = [];
  const spans: Array<[number, number]> = [];
  for (let index = 0; index < count; index += 1) {
    if (u32(bytes, offset) !== SIG_CENTRAL) throw new TraceScrubError("bad central directory entry");
    const flags = u16(bytes, offset + 8);
    const method = u16(bytes, offset + 10);
    const crc = u32(bytes, offset + 16);
    const compressed = u32(bytes, offset + 20);
    const size = u32(bytes, offset + 24);
    const nameLength = u16(bytes, offset + 28);
    const extraLength = u16(bytes, offset + 30);
    const commentLength = u16(bytes, offset + 32);
    const local = u32(bytes, offset + 42);
    const nameBytes = Buffer.from(slice(bytes, offset + 46, nameLength));
    const name = nameBytes.toString("utf8");
    const extra = Buffer.from(slice(bytes, offset + 46 + nameLength, extraLength));
    const entryComment = Buffer.from(slice(bytes, offset + 46 + nameLength + extraLength, commentLength));
    if (compressed === 0xffffffff || size === 0xffffffff || local === 0xffffffff || hasZip64Extra(extra)) {
      throw new TraceScrubError(`Zip64 entry ${name} is not supported`);
    }
    if (flags & 0x0001) throw new TraceScrubError(`encrypted entry ${name} is not supported`);
    if (method !== 0 && method !== 8) throw new TraceScrubError(`unsupported zip method ${method} for ${name}`);
    if (u32(bytes, local) !== SIG_LOCAL) throw new TraceScrubError(`bad local header for ${name}`);
    const localNameLength = u16(bytes, local + 26);
    const localExtra = Buffer.from(slice(bytes, local + 30 + localNameLength, u16(bytes, local + 28)));
    // The bytes actually inflated are named by the LOCAL header. One that
    // disagrees with the central entry (another name, method or size) would let
    // inspected and extracted contents differ.
    const localName = Buffer.from(slice(bytes, local + 30, localNameLength));
    if (!localName.equals(nameBytes) || u16(bytes, local + 8) !== method || hasZip64Extra(localExtra)) {
      throw new TraceScrubError(`local header for ${name} disagrees with the central directory`);
    }
    if (!(flags & 0x0008) && (u32(bytes, local + 18) !== compressed || u32(bytes, local + 22) !== size)) {
      throw new TraceScrubError(`local header sizes for ${name} disagree with the central directory`);
    }
    const dataStart = local + 30 + localNameLength + localExtra.length;
    const raw = Buffer.from(slice(bytes, dataStart, compressed));
    // A streamed entry is followed by a data descriptor, with or without its
    // optional signature; the record's span must include it.
    let recordEnd = dataStart + compressed;
    if (flags & 0x0008) recordEnd += u32(bytes, recordEnd) === 0x08074b50 ? 16 : 12;
    if (recordEnd > directoryStart) throw new TraceScrubError(`entry ${name} overlaps the central directory`);
    spans.push([local, recordEnd]);
    let data: Buffer;
    try {
      data = method === 0 ? raw : zlib.inflateRawSync(raw);
    } catch (error) {
      throw new TraceScrubError(`corrupt entry ${name}: ${(error as Error).message}`);
    }
    if (data.length !== size || zlib.crc32(data) !== crc) {
      throw new TraceScrubError(`corrupt entry ${name}: size or CRC does not match`);
    }
    entries.push({
      name,
      nameBytes,
      versionMadeBy: u16(bytes, offset + 4),
      versionNeeded: u16(bytes, offset + 6),
      flags,
      method,
      time: u16(bytes, offset + 12),
      date: u16(bytes, offset + 14),
      crc,
      internalAttr: u16(bytes, offset + 36),
      externalAttr: u32(bytes, offset + 38),
      extra,
      localExtra,
      comment: entryComment,
      raw,
      data,
    });
    offset += 46 + nameLength + extraLength + commentLength;
  }
  // The directory must be exactly what the end record declares, and the local
  // records it names must tile everything before it: bytes no entry accounts
  // for (a record hidden by a falsified count, say) would never be inspected.
  if (offset !== directoryStart + directorySize) {
    throw new TraceScrubError("the central directory does not match its declared size");
  }
  spans.sort((a, b) => a[0] - b[0]);
  let covered = 0;
  for (const [start, stop] of spans) {
    if (start !== covered) throw new TraceScrubError("the archive holds bytes no directory entry accounts for");
    covered = stop;
  }
  if (covered !== directoryStart) {
    throw new TraceScrubError("the archive holds bytes no directory entry accounts for");
  }
  return { entries, comment };
}

function writeArchive({ entries, comment }: Archive): Buffer {
  if (entries.length >= 0xffff) throw new TraceScrubError("too many entries to write without Zip64");
  const locals: Buffer[] = [];
  const centrals: Buffer[] = [];
  let offset = 0;
  for (const entry of entries) {
    const changed = entry.replacement !== undefined;
    const data = entry.replacement ?? entry.data;
    const raw = changed ? zlib.deflateRawSync(data) : entry.raw;
    const method = changed ? 8 : entry.method;
    const crc = changed ? zlib.crc32(data) : entry.crc;
    const versionNeeded = changed ? Math.max(entry.versionNeeded, 20) : entry.versionNeeded;
    // Sizes are written in the local header, so no data descriptor follows.
    const flags = entry.flags & ~0x0008;
    if (offset + 30 + entry.nameBytes.length + entry.localExtra.length + raw.length >= 0xffffffff) {
      throw new TraceScrubError("archive too large to write without Zip64");
    }
    const local = Buffer.alloc(30);
    local.writeUInt32LE(SIG_LOCAL, 0);
    local.writeUInt16LE(versionNeeded, 4);
    local.writeUInt16LE(flags, 6);
    local.writeUInt16LE(method, 8);
    local.writeUInt16LE(entry.time, 10);
    local.writeUInt16LE(entry.date, 12);
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(raw.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(entry.nameBytes.length, 26);
    local.writeUInt16LE(entry.localExtra.length, 28);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(SIG_CENTRAL, 0);
    central.writeUInt16LE(entry.versionMadeBy, 4);
    central.writeUInt16LE(versionNeeded, 6);
    central.writeUInt16LE(flags, 8);
    central.writeUInt16LE(method, 10);
    central.writeUInt16LE(entry.time, 12);
    central.writeUInt16LE(entry.date, 14);
    central.writeUInt32LE(crc, 16);
    central.writeUInt32LE(raw.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(entry.nameBytes.length, 28);
    central.writeUInt16LE(entry.extra.length, 30);
    central.writeUInt16LE(entry.comment.length, 32);
    central.writeUInt16LE(entry.internalAttr, 36);
    central.writeUInt32LE(entry.externalAttr, 38);
    central.writeUInt32LE(offset, 42);
    locals.push(local, entry.nameBytes, entry.localExtra, raw);
    centrals.push(central, entry.nameBytes, entry.extra, entry.comment);
    offset += 30 + entry.nameBytes.length + entry.localExtra.length + raw.length;
  }
  const directory = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(SIG_END, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  end.writeUInt16LE(comment.length, 20);
  return Buffer.concat([...locals, directory, end, comment]);
}

/** Every piece of an archive this module preserves without redacting: names, extra fields, comments. */
function metadataTexts({ entries, comment }: Archive): Array<[string, string]> {
  const texts: Array<[string, string]> = [["archive comment", comment.toString("latin1")]];
  for (const entry of entries) {
    texts.push(
      [`name of ${entry.name}`, entry.nameBytes.toString("latin1")],
      [`extra field of ${entry.name}`, entry.extra.toString("latin1")],
      [`local extra field of ${entry.name}`, entry.localExtra.toString("latin1")],
      [`comment of ${entry.name}`, entry.comment.toString("latin1")],
    );
  }
  return texts;
}

/** Throw unless no part of the archive names the cookie or carries one of `values`. */
function assertClean(archive: Archive, values: Set<string>, when: string) {
  const parts: Array<[string, string]> = [
    ...archive.entries.map((entry): [string, string] => [entry.name, (entry.replacement ?? entry.data).toString("latin1")]),
    ...metadataTexts(archive),
  ];
  for (const [label, text] of parts) {
    if (stillCarriesSessionCookie(text)) throw new TraceScrubError(`${when}: ${label} still names ${COOKIE}`);
    for (const value of values) {
      if (text.includes(value)) throw new TraceScrubError(`${when}: ${label} still carries a session value`);
    }
  }
}

/**
 * Rewrite one trace zip in place; answers how many entries were redacted.
 *
 * Throws `TraceScrubError` when the trace cannot be proven clean, and leaves
 * the file exactly as it was in that case (the caller quarantines it). The
 * rewrite goes to a temporary file that is re-read and checked before it is
 * renamed over the original, so a failure never leaves a half-written trace.
 * "Clean" covers the whole archive: every entry, and every name, extra field
 * and comment, which are kept verbatim and so must not mention the cookie or
 * any session value found in the entries.
 */
export function scrubTrace(file: string): number {
  const archive = readArchive(fs.readFileSync(file));
  for (const [label, text] of metadataTexts(archive)) {
    if (text.includes(COOKIE)) throw new TraceScrubError(`the ${label} names ${COOKIE}`);
  }
  const values = new Set<string>();
  let redacted = 0;
  for (const entry of archive.entries) {
    const text = entry.data.toString("latin1");
    if (!text.includes(COOKIE)) continue;
    for (const value of sessionValues(text)) values.add(value);
    const clean = redactSessionCookie(text);
    if (stillCarriesSessionCookie(clean)) {
      throw new TraceScrubError(`${entry.name} names ${COOKIE} in a form the scrubber does not recognise`);
    }
    if (clean !== text) {
      entry.replacement = Buffer.from(clean, "latin1");
      redacted += 1;
    }
  }
  assertClean(archive, values, "before rewriting");
  if (redacted === 0) return 0;
  const temporary = `${file}.scrub-${process.pid}.tmp`;
  try {
    fs.writeFileSync(temporary, writeArchive(archive), { mode: 0o600 });
    assertClean(readArchive(fs.readFileSync(temporary)), values, "after rewriting");
    fs.renameSync(temporary, file);
  } finally {
    fs.rmSync(temporary, { force: true });
  }
  return redacted;
}

/** The files Playwright writes as retained traces. */
export function isTraceFile(name: string): boolean {
  return name === "trace.zip" || /^trace-\d+\.zip$/.test(name) || name.endsWith(".trace.zip");
}

/** Component-aware containment: `..quarantine` is a name inside `parent`, `..` is not. */
function isInside(child: string, parent: string): boolean {
  const relative = path.relative(parent, child);
  return (
    relative === "" ||
    (relative !== ".." && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative))
  );
}

/** The canonical path (symlinks resolved) of an existing path, else its resolved form. */
function canonical(target: string): string {
  try {
    return fs.realpathSync.native(target);
  } catch {
    return path.resolve(target);
  }
}

/**
 * Move a trace (or an unreadable directory) that could not be proven clean
 * out of every output directory, preserving it.
 *
 * It goes to a fresh private (0700) slot under `BUZZ_TRACE_QUARANTINE`
 * (default `<tmpdir>/buzz-trace-quarantine`), and if that is unusable, under a
 * per-process directory in `<tmpdir>`. A slot is used only when its canonical
 * path lies outside every output root, so an artefact upload of those roots
 * cannot find it. Only when no private slot can be made at all is a file
 * deleted, because a live broker session must never stay where it would be
 * uploaded; and only when deletion also fails is it left in place, which the
 * answer states and the caller treats as a failed run.
 */
function quarantine(target: string, roots: string[], quarantineDir: string | null): string {
  const canonicalRoots = roots.map(canonical);
  // A configured home must be a real directory this user owns with mode 0700;
  // otherwise each slot is made by mkdtemp directly in the sticky temp
  // directory, never under a fixed name another user could pre-create
  // (finding_f048d00e56e0c1bf).
  const homes: Array<string | null> = quarantineDir === null ? [null] : [quarantineDir, null];
  const refusals: string[] = [];
  for (const home of homes) {
    let slot: string | null = null;
    try {
      if (home === null) {
        slot = fs.mkdtempSync(path.join(os.tmpdir(), "buzz-trace-quarantine-"));
      } else {
        fs.mkdirSync(home, { recursive: true, mode: 0o700 });
        assertPrivateDirectory(home);
        slot = fs.mkdtempSync(path.join(home, "trace-"));
      }
      const place = canonical(slot);
      const inside = canonicalRoots.find((root) => isInside(place, root));
      if (inside) throw new Error(`${place} is inside the output directory ${inside}`);
      const destination = path.join(slot, path.basename(target));
      try {
        fs.renameSync(target, destination);
      } catch (error) {
        if (!fs.statSync(target).isDirectory()) {
          fs.copyFileSync(target, destination, fs.constants.COPYFILE_EXCL);
          fs.rmSync(target);
        } else {
          // Moving a directory updates its `..` entry, which needs write
          // permission on the directory itself. The output tree is this
          // run's own, so restore owner access and try once more.
          fs.chmodSync(target, 0o700);
          try {
            fs.renameSync(target, destination);
          } catch {
            throw error;
          }
        }
      }
      fs.chmodSync(destination, fs.statSync(destination).isDirectory() ? 0o700 : 0o600);
      return `moved to ${destination}`;
    } catch (error) {
      refusals.push((error as Error).message);
      if (slot) fs.rmSync(slot, { recursive: true, force: true });
    }
  }
  try {
    if (fs.statSync(target).isDirectory()) throw new Error("an unreadable directory cannot be deleted safely");
    fs.rmSync(target, { force: true });
    return `deleted: no private quarantine could be made (${refusals.join("; ")})`;
  } catch (error) {
    return `LEFT IN PLACE: could neither quarantine nor delete it (${refusals.join("; ")}; ${(error as Error).message})`;
  }
}

/** Refuse a quarantine home that is a symlink, another user's, or not 0700. */
function assertPrivateDirectory(dir: string): void {
  const stat = fs.lstatSync(dir);
  if (!stat.isDirectory() || stat.isSymbolicLink()) throw new Error(`${dir} is not a real directory`);
  const uid = typeof process.geteuid === "function" ? process.geteuid() : null;
  if (uid !== null && stat.uid !== uid) throw new Error(`${dir} is owned by another user`);
  if ((stat.mode & 0o777) !== 0o700) throw new Error(`${dir} is not private (mode ${(stat.mode & 0o777).toString(8)})`);
}

export type ScrubReport = {
  /** The directory that was walked. */
  root: string;
  /** False when that directory does not exist. */
  found: boolean;
  /** Trace files found. */
  seen: number;
  /** Traces rewritten with at least one value redacted. */
  rewritten: number;
  /** One line per trace that could not be scrubbed, saying where it went. */
  failures: string[];
};

/**
 * Scrub every retained trace under `root`.
 *
 * Only trace files are touched (`isTraceFile`); other zips in the output
 * directory, such as attachments, are not archives this module owns. One bad
 * trace never stops the walk: it is quarantined, recorded in `failures`, and
 * the next trace is scrubbed. A directory that cannot be read is quarantined
 * whole, since the traces inside it cannot be inspected. `outputRoots` names
 * every output directory of the run, none of which may receive quarantine.
 */
export function scrubTraces(
  root: string,
  options: { quarantineDir?: string; outputRoots?: string[] } = {},
): ScrubReport {
  const quarantineDir = options.quarantineDir ?? process.env.BUZZ_TRACE_QUARANTINE ?? null;
  const roots = [root, ...(options.outputRoots ?? [])];
  const report: ScrubReport = { root, found: fs.existsSync(root), seen: 0, rewritten: 0, failures: [] };
  if (!report.found) return report;
  const walk = (dir: string) => {
    let items: fs.Dirent[];
    try {
      items = fs.readdirSync(dir, { withFileTypes: true });
    } catch (error) {
      const where = dir === root ? "LEFT IN PLACE: the output directory itself" : quarantine(dir, roots, quarantineDir);
      report.failures.push(`${dir}: could not be read (${(error as Error).message}); ${where}`);
      return;
    }
    for (const item of items) {
      const full = path.join(dir, item.name);
      if (item.isDirectory()) walk(full);
      else if (item.isFile() && isTraceFile(item.name)) {
        report.seen += 1;
        try {
          if (scrubTrace(full) > 0) report.rewritten += 1;
        } catch (error) {
          const where = quarantine(full, roots, quarantineDir);
          report.failures.push(`${path.relative(root, full)}: ${(error as Error).message}; ${where}`);
        }
      }
    }
  };
  walk(root);
  return report;
}
