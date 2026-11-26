/**
 * The relay's own NIP-98 HTTP face, for the half of a scenario the browser
 * cannot reach.
 *
 * Some authorities are deliberately NOT in the client: minting an invite needs
 * the operator-seeded community-moderator set, and reading the operator config
 * plane needs a key no HTTP request can grant itself
 * (`ingress.rs::operator_caller`). A scenario that needs one of those drives it
 * here and keeps the RECEIPT in a browser — never the other way round.
 *
 * `channel-lifecycle.spec.ts` grew its own copies of these four helpers first
 * and still carries them. They are duplicated rather than refactored on
 * purpose: that file is a watched, passing spec and rewriting its imports
 * mid-flight would put a known-good suite at risk for a cosmetic win. If it is
 * ever touched for another reason, delete its copies and import these.
 */
import { createHash } from "node:crypto";
import fs from "node:fs";
import { finalizeEvent, generateSecretKey, getPublicKey } from "nostr-tools/pure";

/** Same origin the browser uses, so the vite preview proxy fronts both. */
export const RELAY_HTTP = process.env.BUZZ_HTTP ?? "http://localhost:4173";

export type RelayReply = { status: number; body: string };

/** A scenario-local identity: a secret nobody else holds, plus its pubkey. */
export type Identity = { secret: Uint8Array; pubkey: string };

/**
 * A scenario-local identity for a label: a fresh random key, the same one for
 * every call with that label in this process.
 *
 * The secret comes from a CSPRNG, never from the label. These keys sign on a
 * SHARED relay and some are handed real authority (invites.spec.ts seeds the
 * community-moderator set with the minter's pubkey). A key derived from a
 * label would be public: the label text is in this repository, and the run
 * stamp folded into it is published on the relay in display names and room
 * names, so anyone who can read the relay could recompute the secret.
 *
 * Callers still fold this run's stamp into the label, so logs stay readable
 * and two scenarios in one process never share a key by accident. The shared
 * dev relay remembers every key that ever wrote to it; a fresh key per run is
 * what keeps a scenario asserting against its own setup rather than against
 * an earlier run's bans, moderation or membership.
 */
const identities = new Map<string, Identity>();

export function identity(label: string): Identity {
  let known = identities.get(label);
  if (!known) {
    const secret = generateSecretKey();
    known = { secret, pubkey: getPublicKey(secret) };
    identities.set(label, known);
  }
  return known;
}

/**
 * One NIP-98 `Authorization` header.
 *
 * The relay binds the signature to the exact method and full URL and, for a
 * body-carrying POST, to a SHA-256 of the body via the `payload` tag — and it
 * burns the auth event on use (`auth.rs::consume_once`). So the URL is
 * finalized before signing and never touched after, and every request gets its
 * own fresh event with its own nonce.
 */
export function nip98(
  secret: Uint8Array,
  method: string,
  url: string,
  body?: string,
): string {
  const tags: string[][] = [
    ["u", url],
    ["method", method],
    ["nonce", `${Math.random().toString(36).slice(2)}${Date.now()}`],
  ];
  if (body !== undefined) {
    tags.push(["payload", createHash("sha256").update(body).digest("hex")]);
  }
  const event = finalizeEvent(
    {
      kind: 27235,
      created_at: Math.floor(Date.now() / 1000),
      tags,
      content: "",
    },
    secret,
  );
  return `Nostr ${Buffer.from(JSON.stringify(event)).toString("base64")}`;
}

/**
 * Whoever is signing: a scenario identity, or a bare secret.
 *
 * The operator key arrives as raw bytes from a provisioning file and has no
 * pubkey attached, while scenario identities carry both — so every caller would
 * otherwise have to remember which shape it holds.
 */
export type Signer = Identity | Uint8Array;

const secretOf = (who: Signer): Uint8Array =>
  who instanceof Uint8Array ? who : who.secret;

export async function relayRequest(
  who: Signer,
  method: "GET" | "POST",
  path: string,
  payload?: unknown,
): Promise<RelayReply> {
  const url = `${RELAY_HTTP}${path}`;
  const body = payload === undefined ? undefined : JSON.stringify(payload);
  const response = await fetch(url, {
    method,
    headers: {
      "x-buzz-authorization": nip98(secretOf(who), method, url, body),
      "content-type": "application/json",
    },
    body,
  });
  // The body is returned WHOLE: an earlier revision of the copy in
  // channel-lifecycle truncated it for readable failure messages and silently
  // broke `JSON.parse` on a long reply. Callers slice for display instead.
  return { status: response.status, body: await response.text() };
}

/** The relay's answer, trimmed for a failure message. */
export function said(reply: RelayReply): string {
  return `${reply.status} ${reply.body.slice(0, 240)}`;
}

/** Publish a signed event through the relay's HTTP ingest. */
export async function publishAs(
  who: Signer,
  kind: number,
  tags: string[][],
  content = "",
): Promise<RelayReply> {
  const event = finalizeEvent(
    { kind, created_at: Math.floor(Date.now() / 1000), tags, content },
    secretOf(who),
  );
  return relayRequest(who, "POST", "/buzz/api/events", event);
}

/**
 * The operator secret this deployment was provisioned with, or null.
 *
 * The operator plane is rooted in a pubkey set no HTTP request can widen — by
 * design, so a community member cannot escalate into it. The local dev fleet's
 * operator key was written to this file when the workspace agents were
 * provisioned; a run without it must SKIP loudly rather than quietly assert
 * less.
 */
export const PROVISION_FILE =
  process.env.BUZZ_PROVISION_FILE || "/tmp/buzz-agent-provision.json";

/**
 * The provisioning file's text, only when it is a regular file this user owns
 * that nobody else can read or write: the path is predictable and sits in a
 * shared directory, so a planted file or symlink must not supply the key.
 */
function readOwnerOnly(file: string): string | null {
  let descriptor: number;
  try {
    descriptor = fs.openSync(file, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW | fs.constants.O_NONBLOCK);
  } catch {
    return null;
  }
  try {
    const stat = fs.fstatSync(descriptor);
    const uid = typeof process.getuid === "function" ? process.getuid() : null;
    if (!stat.isFile()) return null;
    if (uid !== null && stat.uid !== uid) return null;
    if ((stat.mode & 0o077) !== 0) return null;
    return fs.readFileSync(descriptor, "utf8");
  } finally {
    fs.closeSync(descriptor);
  }
}

export function operatorSecret(): Uint8Array | null {
  const hexToBytes = (hex: unknown) =>
    typeof hex === "string" && /^[0-9a-f]{64}$/i.test(hex)
      ? new Uint8Array((hex.match(/.{2}/g) ?? []).map((byte) => Number.parseInt(byte, 16)))
      : null;
  // A hosted deployment's operator state arrives as the account secret
  // BUZZ_OPERATOR_STATE (`{operator_private_key, ...}`), in the environment
  // and never on a command line or in a log.
  const state = process.env.BUZZ_OPERATOR_STATE;
  if (state) {
    try {
      const key = hexToBytes((JSON.parse(state) as { operator_private_key?: unknown }).operator_private_key);
      if (key) return key;
    } catch {
      // Malformed state: fall through to the provisioning file.
    }
  }
  const text = readOwnerOnly(PROVISION_FILE);
  if (text === null) return null;
  try {
    const raw = JSON.parse(text) as { operator_sk?: unknown };
    return hexToBytes(raw.operator_sk);
  } catch {
    return null;
  }
}
