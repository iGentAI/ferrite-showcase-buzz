// Vendored into the buzz-web journeys from the operator tooling's SSO
// session helper, so this package imports nothing outside itself. Keep the
// code below this header byte-identical with that helper.

// Mint a Ferrite SSO assertion and redeem it ONCE at an owned property's
// callback, yielding that property's `__Host-` session cookie.
//
// A byte-for-byte port of packages/ferrite-sso/src/assertion.rs (mint side)
// and the redemption leg of packages/ferrite-sso/src/gate.rs:
//
//   key       = HMAC-SHA256(key = "ferrite-sso-assertion-v1", msg = secret)
//   input     = "fsso1.<hex subject>.<hex audience>.<expires_at_ms>.<nonce>"
//   assertion = input + "." + hex(HMAC-SHA256(key, input))
//
// with a 16-byte hex nonce and the broker's 60 s TTL (a verifier refuses an
// expiry further ahead than TTL + 5 s skew). The callback is
// `/_sso/callback?a=<assertion>&return_to=/`; a fresh assertion answers 303
// with `set-cookie: __Host-ferrite_app_session=...`.
//
// The shared secret comes from SSO_SHARED_SECRET and never leaves this
// process; nothing here prints the secret, an assertion, or a cookie. The
// symmetric key means any holder can mint for any audience (assertion.rs
// states this threat model) — this helper is operator tooling for owned
// properties, not an application path.

import { createHmac, randomBytes } from "node:crypto";

const VERSION = "fsso1";
const KEY_LABEL = "ferrite-sso-assertion-v1";
export const DEFAULT_TTL_MS = 60_000;
export const SESSION_COOKIE = "__Host-ferrite_app_session";
export const CALLBACK_PATH = "/_sso/callback";

const hex = (value) => Buffer.from(value, "utf8").toString("hex");

/** The assertion signing key: HMAC(label, secret), exactly as `AssertionKey::from_secret`. */
export function assertionKey(secret) {
  if (typeof secret !== "string" || secret.length === 0) {
    throw new Error("the SSO shared secret is empty");
  }
  return createHmac("sha256", KEY_LABEL).update(secret, "utf8").digest();
}

/** `AssertionKey::mint`: returns the token string. `nonce` is injectable for tests only. */
export function mintAssertion(secret, { subject, audience, nowMs = Date.now(), ttlMs = DEFAULT_TTL_MS, nonce }) {
  if (!subject || !audience) throw new Error("subject and audience are required");
  const key = assertionKey(secret);
  const expiresAtMs = nowMs + ttlMs;
  const n = nonce ?? randomBytes(16).toString("hex");
  const input = `${VERSION}.${hex(subject)}.${hex(audience)}.${expiresAtMs}.${n}`;
  const mac = createHmac("sha256", key).update(input, "utf8").digest("hex");
  return `${input}.${mac}`;
}

/** Pull one cookie's value out of a fetch response's set-cookie headers. */
function cookieFrom(response, name) {
  const headers = typeof response.headers.getSetCookie === "function"
    ? response.headers.getSetCookie()
    : [response.headers.get("set-cookie") ?? ""];
  for (const header of headers) {
    const [pair] = header.split(";");
    const index = pair.indexOf("=");
    if (index > 0 && pair.slice(0, index).trim() === name) {
      return pair.slice(index + 1).trim();
    }
  }
  return null;
}

/**
 * Redeem a freshly minted assertion at `origin` and return the session cookie
 * value. Throws with the property's refusal text (which carries no token
 * material) when the redemption is refused.
 */
export async function sessionCookie({ origin, subject, audience, secret = process.env.SSO_SHARED_SECRET }) {
  const assertion = mintAssertion(secret, { subject, audience });
  const url = `${origin.replace(/\/+$/, "")}${CALLBACK_PATH}?a=${encodeURIComponent(assertion)}&return_to=%2F`;
  const response = await fetch(url, { redirect: "manual", headers: { accept: "text/html" } });
  const value = cookieFrom(response, SESSION_COOKIE);
  if (response.status !== 303 || !value) {
    const reason = (await response.text().catch(() => "")).slice(0, 200);
    throw new Error(`SSO redemption refused at ${origin}: HTTP ${response.status} ${reason}`);
  }
  return value;
}

/** A Playwright storageState document carrying one session cookie for `origin`. */
export function storageState(origin, cookieValue) {
  const { hostname } = new URL(origin);
  return {
    cookies: [
      {
        name: SESSION_COOKIE,
        value: cookieValue,
        domain: hostname,
        path: "/",
        expires: Math.floor(Date.now() / 1000) + 23 * 60 * 60,
        httpOnly: true,
        secure: true,
        sameSite: "Lax",
      },
    ],
    origins: [],
  };
}
