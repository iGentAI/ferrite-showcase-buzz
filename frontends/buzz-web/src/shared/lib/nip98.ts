/**
 * NIP-98 HTTP Auth helper — signs a kind:27235 event for authenticating
 * HTTP requests to the relay (used by isomorphic-git for smart HTTP transport).
 */

import { randomUuid, sha256Hex as sha256BytesHex } from "./insecureContextCrypto";
import { signNostrEvent } from "./nostr-signer";

/**
 * The Ferrite managed-auth edge owns the standard `Authorization` header.
 * Buzz carries its inner NIP-98/Bearer credential in this app-specific header,
 * which the deployment manifest explicitly forwards to the relay.
 */
export const BUZZ_AUTHORIZATION_HEADER = "X-Buzz-Authorization";

async function sha256Hex(value: string): Promise<string> {
  return sha256BytesHex(new TextEncoder().encode(value));
}

/**
 * Build a NIP-98 Authorization header value.
 *
 * Signed POST bodies include the payload digest required by invite endpoints.
 */
export async function makeNip98AuthHeader(
  url: string,
  method: string,
  options?: { body?: string },
): Promise<string> {
  // The nonce is unconditional, and deliberately so: it is the only thing that
  // makes two otherwise-identical requests distinct EVENTS. `created_at` has
  // one-second resolution, so without it any two authenticated requests to the
  // same path in the same second sign the same event id, and the relay's
  // single-use burn (`auth.rs: consume_once`) refuses the second as a replay.
  const tags = [
    ["u", url],
    ["method", method],
    ["nonce", randomUuid()],
  ];
  if (options?.body !== undefined) {
    tags.push(["payload", await sha256Hex(options.body)]);
  }
  const event = await signNostrEvent({
    kind: 27235,
    tags,
    content: "",
  });

  const json = JSON.stringify(event);
  const base64 = btoa(json);
  return `Nostr ${base64}`;
}
