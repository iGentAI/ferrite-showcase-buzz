// Local files and relay addresses for the operator one-off scripts at this
// directory's root (push-ui.mjs through bzcli.mjs).
//
// KEY FILES. A persona's signing key lives in `$XDG_STATE_HOME/ferrite-buzz/`
// (default `~/.local/state/ferrite-buzz/`), a 0700 directory, in a 0600 file created
// exclusively and moved into place with a rename. Reads open with O_NOFOLLOW
// and O_NONBLOCK (a FIFO planted at a predictable path cannot stall the open)
// and accept only a regular file owned by the current user with no group or
// other bits, so a planted symlink, FIFO or world-readable key file is refused
// rather than used.
//
// TRANSPORT. The default relay base is plain http only for a loopback host
// (`*.localhost`, 127.0.0.0/8, ::1); any other host defaults to https.
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const { O_CREAT, O_EXCL, O_NONBLOCK, O_NOFOLLOW, O_RDONLY, O_WRONLY } = fs.constants;
const currentUid = () => (typeof process.getuid === "function" ? process.getuid() : null);

/** The default community host of the local sandbox relay. */
export const DEFAULT_BUZZ_HOST = "3emul7w0sqvwfom6.localhost";

/** The provisioning file's legacy location, in the shared temporary directory. */
export const LEGACY_PROVISION_FILE = "/tmp/buzz-agent-provision.json";

/**
 * Where an operator's provisioning file (it holds `operator_sk`) is read from:
 * BUZZ_PROVISION_FILE, else the owner-only state directory, else the legacy
 * /tmp path only when no state-directory copy exists. Reading it still goes
 * through readPrivateJson, which refuses a file or parent another user controls.
 */
export function provisionFile(env = process.env) {
  if (env.BUZZ_PROVISION_FILE) return env.BUZZ_PROVISION_FILE;
  const preferred = path.join(stateDirectory(env), "buzz-agent-provision.json");
  return fs.existsSync(preferred) || !fs.existsSync(LEGACY_PROVISION_FILE) ? preferred : LEGACY_PROVISION_FILE;
}

/**
 * Refuse a parent directory another user could rename a planted file into:
 * it must belong to us or root and be writable by nobody else, unless it is
 * sticky (as /tmp is), where only a file's owner may replace it.
 */
function assertSafeParent(file) {
  const parent = path.dirname(path.resolve(file));
  const stat = fs.statSync(parent);
  const uid = currentUid();
  if (uid !== null && stat.uid !== uid && stat.uid !== 0) {
    throw new Error(`${parent} is owned by another user; refusing to read a key from it`);
  }
  const sticky = (stat.mode & 0o1000) !== 0;
  if ((stat.mode & 0o022) !== 0 && !sticky) {
    throw new Error(`${parent} is writable by other users; refusing to read a key from it`);
  }
}

/** `$XDG_STATE_HOME/ferrite-buzz`, or `~/.local/state/ferrite-buzz`. */
export function stateDirectory(env = process.env) {
  const base = env.XDG_STATE_HOME || path.join(env.HOME || os.homedir(), ".local", "state");
  return path.join(base, "ferrite-buzz");
}

/** The owner-only file that holds one persona's `{ sk, pk }`. */
export function personaKeyFile(agent, env = process.env) {
  if (!/^[a-z0-9][a-z0-9-]{0,62}$/.test(agent)) throw new Error(`invalid persona name ${JSON.stringify(agent)}`);
  return path.join(stateDirectory(env), `buzz-persona-${agent}.json`);
}

/** Where earlier versions of the persona registration script left the key, world-readable. */
export const LEGACY_FABRIZIO_FILE = "/tmp/fabrizio.json";

function ensurePrivateDirectory(directory) {
  fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
  const stat = fs.lstatSync(directory);
  if (!stat.isDirectory() || stat.isSymbolicLink()) throw new Error(`${directory} is not a real directory`);
  const uid = currentUid();
  if (uid !== null && stat.uid !== uid) throw new Error(`${directory} is not owned by the current user`);
  if ((stat.mode & 0o777) !== 0o700) fs.chmodSync(directory, 0o700);
  return directory;
}

/** The parsed contents of an owner-only JSON file; throws with a fix when it is unsafe or absent. */
export function readPrivateJson(file) {
  try {
    assertSafeParent(file);
  } catch (error) {
    if (error.code === "ENOENT") throw new Error(`${file} does not exist`);
    throw error;
  }
  let descriptor;
  try {
    descriptor = fs.openSync(file, O_RDONLY | O_NOFOLLOW | O_NONBLOCK);
  } catch (error) {
    if (error.code === "ENOENT") throw new Error(`${file} does not exist`);
    if (error.code === "ELOOP") throw new Error(`${file} is a symbolic link; refusing to read a key through it`);
    throw error;
  }
  let text;
  try {
    const stat = fs.fstatSync(descriptor);
    const uid = currentUid();
    if (!stat.isFile()) throw new Error(`${file} is not a regular file`);
    if (uid !== null && stat.uid !== uid) throw new Error(`${file} is not owned by the current user`);
    if ((stat.mode & 0o077) !== 0) {
      throw new Error(
        `${file} is readable or writable by other users (mode ${(stat.mode & 0o777).toString(8)}); ` +
          `it holds a secret key, so run: chmod 600 ${file}`,
      );
    }
    text = fs.readFileSync(descriptor, "utf8");
  } finally {
    fs.closeSync(descriptor);
  }
  try {
    return JSON.parse(text);
  } catch (error) {
    throw new Error(`${file} is not valid JSON: ${error.message}`);
  }
}

/** Atomically write `value` as JSON to `file` (mode 0600) inside a private directory. */
export function writePrivateJson(file, value) {
  ensurePrivateDirectory(path.dirname(file));
  const temporary = `${file}.${process.pid}.${crypto.randomBytes(8).toString("hex")}.tmp`;
  try {
    const descriptor = fs.openSync(temporary, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0o600);
    try {
      fs.writeFileSync(descriptor, `${JSON.stringify(value, null, 2)}\n`);
      fs.fchmodSync(descriptor, 0o600);
      fs.fsyncSync(descriptor);
    } finally {
      fs.closeSync(descriptor);
    }
    fs.renameSync(temporary, file);
  } catch (error) {
    fs.rmSync(temporary, { force: true });
    throw error;
  }
  return file;
}

/**
 * Take an exclusive lock file (O_CREAT|O_EXCL, 0600) inside a private
 * directory; refuses while another holder's lock exists. The caller removes it.
 */
export function createPrivateLock(file) {
  ensurePrivateDirectory(path.dirname(file));
  let descriptor;
  try {
    descriptor = fs.openSync(file, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0o600);
  } catch (error) {
    if (error.code === "EEXIST") {
      throw new Error(`another registration holds ${file}; if none is running, delete that file and retry`);
    }
    throw error;
  }
  try {
    fs.writeFileSync(descriptor, `${process.pid}\n`);
  } finally {
    fs.closeSync(descriptor);
  }
  return file;
}

/** Read one persona's key file, explaining the move from the old /tmp location. */
export function readPersonaKey(agent, env = process.env) {
  const file = personaKeyFile(agent, env);
  if (!fs.existsSync(file) && agent === "fabrizio" && fs.existsSync(LEGACY_FABRIZIO_FILE)) {
    throw new Error(
      `${file} does not exist, but ${LEGACY_FABRIZIO_FILE} does. That key was stored world-readable and ` +
        `must be treated as exposed: delete it and register the persona again to mint a new persona key.`,
    );
  }
  const key = readPrivateJson(file);
  if (!/^[0-9a-f]{64}$/.test(key?.sk ?? "") || !/^[0-9a-f]{64}$/.test(key?.pk ?? "")) {
    throw new Error(`${file} does not hold a 64-hex { sk, pk } pair`);
  }
  return key;
}

/** True for a host name that only ever resolves to this machine. */
export function isLoopbackHost(hostname) {
  const host = String(hostname).toLowerCase().replace(/^\[|\]$/g, "");
  return (
    host === "localhost" ||
    host.endsWith(".localhost") ||
    host === "::1" ||
    /^127(?:\.\d{1,3}){3}$/.test(host)
  );
}

/**
 * The relay's HTTP base: BUZZ_BASE when set, else http://HOST:28443 for a
 * loopback BUZZ_HOST (a bare host name, as bzcli.mjs has always taken it) and
 * https://HOST for any other host.
 */
export function relayBase(env = process.env) {
  if (env.BUZZ_BASE) return env.BUZZ_BASE.replace(/\/+$/, "");
  const host = env.BUZZ_HOST || DEFAULT_BUZZ_HOST;
  // A bare IPv6 literal (two or more colons) needs brackets inside a URL.
  const literal = !host.startsWith("[") && (host.match(/:/g) ?? []).length >= 2 ? `[${host}]` : host;
  return isLoopbackHost(host) ? `http://${literal}:28443` : `https://${literal}`;
}

/** Refuse to send `what` (a secret) over plaintext to anything but this machine. */
export function assertPrivateTransport(url, what) {
  const parsed = new URL(url);
  if ((parsed.protocol === "http:" || parsed.protocol === "ws:") && !isLoopbackHost(parsed.hostname)) {
    throw new Error(`refusing to send ${what} over plaintext ${parsed.protocol}// to ${parsed.host}; use https`);
  }
}
