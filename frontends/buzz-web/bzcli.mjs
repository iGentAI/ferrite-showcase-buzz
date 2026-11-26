// Minimal NIP-98 client for the live buzz relay (node, no deps beyond nostr-tools).
import { finalizeEvent } from 'nostr-tools/pure';
import { createHash, randomBytes } from 'node:crypto';
import { assertPrivateTransport, relayBase } from './operator-local-state.mjs';

// Plain http only for a loopback host; any other BUZZ_HOST defaults to https.
const BASE = relayBase();

const sha256hex = (s) => createHash('sha256').update(s).digest('hex');

// The `u` tag names the URL the request really goes to, and the nonce is
// unpredictable: Math.random() is not a CSPRNG.
function nip98(a, method, path, body) {
  const url = BASE + path;
  const tags = [['u', url], ['method', method], ['nonce', randomBytes(16).toString('hex')]];
  if (body !== undefined) tags.push(['payload', sha256hex(body)]);
  const ev = finalizeEvent({ kind: 27235, created_at: Math.floor(Date.now() / 1000), tags, content: '' }, a.sk);
  return 'Nostr ' + Buffer.from(JSON.stringify(ev)).toString('base64');
}

export async function call(a, method, path, bodyObj) {
  const body = bodyObj === undefined ? undefined : JSON.stringify(bodyObj);
  const headers = { 'Content-Type': 'application/json' };
  if (a) {
    // A signed header is a credential: never over plaintext to another host,
    // and never replayed to wherever a redirect points.
    assertPrivateTransport(BASE + path, 'a signed NIP-98 header');
    headers["x-buzz-authorization"] = nip98(a, method, path, body);
  }
  const res = await fetch(BASE + path, { method, headers, body, redirect: 'error' });
  const text = await res.text();
  let json; try { json = JSON.parse(text); } catch { json = text; }
  return { status: res.status, body: json };
}

export function signEvent(a, kind, tags, content) {
  return finalizeEvent({ kind, created_at: Math.floor(Date.now() / 1000), tags, content }, a.sk);
}
export const publish = (a, ev) => call(a, 'POST', '/buzz/api/events', ev);
