/**
 * A browser context signed in to an owned property behind the Ferrite SSO broker.
 *
 * Staging Buzz answers every request without its `__Host-ferrite_app_session`
 * cookie with 401 (packages/ferrite-sso/src/gate.rs), before Buzz's own
 * onboarding runs. When `BUZZ_SSO_ORIGIN` is set, each person gets their OWN
 * subject and a freshly redeemed session, loaded as the context's storageState —
 * the same state a person who completed federated sign-in would carry. Without it
 * (the local fleet, the embedded bootstrap UI) contexts are created exactly as
 * before.
 *
 * The shared secret, the assertion and the cookie never reach a log or a trace.
 */
import type { Browser, BrowserContext } from "@playwright/test";

// @ts-expect-error: a plain ESM helper (vendored, see its header), no type declarations.
import { sessionCookie, storageState } from "./ssoSession.mjs";

const ORIGIN = process.env.BUZZ_SSO_ORIGIN ?? "";
// No default domain: a subject names a person at the broker's identity domain,
// and that domain belongs to whoever runs the broker.
const SUBJECT_DOMAIN = process.env.BUZZ_SSO_SUBJECT_DOMAIN ?? "";
if (ORIGIN && !SUBJECT_DOMAIN) {
  throw new Error("BUZZ_SSO_ORIGIN is set but BUZZ_SSO_SUBJECT_DOMAIN is not: name the broker's subject domain");
}

const CALLBACK_PATH = "/_sso/callback";

function isLoopbackHostname(hostname: string): boolean {
  return (
    hostname === "localhost" ||
    hostname.endsWith(".localhost") ||
    /^127(?:\.\d{1,3}){3}$/.test(hostname) ||
    hostname === "[::1]"
  );
}

/**
 * The broker origin as a parsed URL, or an error saying why it cannot carry a
 * session: it must parse, and it must be https unless it is loopback, so the
 * cookie never crosses a network in clear text.
 */
export function brokerOrigin(origin: string): URL {
  let broker: URL;
  try {
    broker = new URL(origin);
  } catch {
    throw new Error(`BUZZ_SSO_ORIGIN is not a URL: ${origin}`);
  }
  if (broker.protocol !== "https:" && !(broker.protocol === "http:" && isLoopbackHostname(broker.hostname))) {
    throw new Error(`BUZZ_SSO_ORIGIN must be https (or a loopback http origin): ${broker.origin}`);
  }
  return broker;
}

/**
 * Whether a Node `fetch` to `url` may carry the broker session.
 *
 * Only an exact origin match (scheme, host and port, compared as parsed URLs)
 * qualifies. A string prefix test is not an origin test:
 * `https://buzz.example.com.attacker.tld/` and
 * `https://buzz.example.com@attacker.tld/` both start with
 * `https://buzz.example.com` and name someone else's host. The redemption
 * callback is excluded because it mints the session and must not wait on it.
 */
export function carriesBrokerSession(url: string, origin: string): boolean {
  if (!origin) return false;
  let target: URL;
  let broker: URL;
  try {
    target = new URL(url);
    broker = brokerOrigin(origin);
  } catch {
    return false;
  }
  if (target.origin !== broker.origin) return false;
  if (target.username || target.password) return false;
  return target.pathname !== CALLBACK_PATH;
}

if (ORIGIN) brokerOrigin(ORIGIN);

/** A stable, per-person subject: one test persona, one identity at the broker. */
function subjectFor(name: string): string {
  const local = name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "") || "person";
  return `buzz-e2e-${local}@${SUBJECT_DOMAIN}`;
}

/** `browser.newContext()`, signed in when the property is behind the SSO broker. */
export async function newBuzzContext(browser: Browser, name: string): Promise<BrowserContext> {
  if (!ORIGIN) return browser.newContext();
  const cookie: string = await sessionCookie({
    origin: ORIGIN,
    subject: subjectFor(name),
    audience: process.env.BUZZ_SSO_AUDIENCE ?? "buzz",
  });
  return browser.newContext({ storageState: storageState(ORIGIN, cookie) });
}

// The journeys also read the relay from the test process itself (roster polls,
// join policy, signed filter queries through `relayHttp.ts`), and the broker
// answers those 401 too. Every Node `fetch` to the SSO origin therefore carries
// one session of its own subject, minted on first use. A broker session lasts
// 24 hours (packages/ferrite-sso/src/gate.rs SESSION_TTL_MS), longer than any
// run. Inside Buzz nothing changes: NIP-98 still decides who is calling.
if (ORIGIN) {
  const plainFetch = globalThis.fetch;
  let session: Promise<string> | null = null;
  globalThis.fetch = async (input: string | URL | Request, init: RequestInit = {}) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
    // Exact origin only; the redemption itself goes to the origin too, and
    // must not wait on itself (see `carriesBrokerSession`).
    if (!carriesBrokerSession(url, ORIGIN)) {
      return plainFetch(input, init);
    }
    // A failed redemption is not cached: the next fetch mints again, so one
    // transient gateway error does not fail every later call (teardown too).
    session ??= sessionCookie({
      origin: ORIGIN,
      subject: subjectFor("test-process"),
      audience: process.env.BUZZ_SSO_AUDIENCE ?? "buzz",
    }).catch((error: unknown) => {
      session = null;
      throw error;
    });
    const headers = new Headers(init.headers ?? (input instanceof Request ? input.headers : undefined));
    const existing = headers.get("cookie");
    const cookie = `__Host-ferrite_app_session=${await session}`;
    headers.set("cookie", existing ? `${existing}; ${cookie}` : cookie);
    return plainFetch(input, { ...init, headers });
  };
}
