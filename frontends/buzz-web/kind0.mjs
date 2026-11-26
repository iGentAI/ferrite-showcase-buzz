// Publish ONE kind-0 profile after NIP-42 authentication and report the
// relay's verdict. BUZZ_WS overrides the relay's websocket URL.
//
// Each OK is matched to the event it acknowledges: the profile is published
// once, when the relay accepts the AUTH event, and the script exits on the
// profile's own OK (0 when accepted, 1 when refused). It never publishes in
// response to its own profile's OK.
import { finalizeEvent, generateSecretKey } from 'nostr-tools/pure';
const WS = process.env.BUZZ_WS || 'ws://3qchc7qxq3pdai2r.localhost:28443/buzz/ws/';
// A fresh random key per run: a key derived from a string in this file is a
// key anyone reading the repository can sign as.
const sk = generateSecretKey();
const ws = new WebSocket(WS);
let authId = null;
let profileId = null;
ws.onmessage = (e) => {
  const m = JSON.parse(String(e.data));
  if (m[0] === 'AUTH' && authId === null) {
    const ev = finalizeEvent({ kind: 22242, created_at: Math.floor(Date.now()/1000),
      tags: [['challenge', m[1]], ['relay', WS]], content: '' }, sk);
    authId = ev.id;
    ws.send(JSON.stringify(['AUTH', ev]));
    return;
  }
  if (m[0] === 'OK' && m[1] === authId && profileId === null) {
    if (m[2] !== true) { console.log('<- auth refused', JSON.stringify(m).slice(0,200)); process.exit(1); }
    const profile = finalizeEvent({ kind: 0, created_at: Math.floor(Date.now()/1000),
      tags: [], content: JSON.stringify({ name: 'Ana Ruiz' }) }, sk);
    profileId = profile.id;
    ws.send(JSON.stringify(['EVENT', profile]));
    return;
  }
  if (m[0] === 'OK' && m[1] === profileId) {
    console.log('<- OK', JSON.stringify(m).slice(0,200));
    ws.close();
    process.exit(m[2] === true ? 0 : 1);
  }
  if (m[0] === 'NOTICE') console.log('<- NOTICE', m[1]);
};
ws.onerror = () => { console.log('websocket error talking to', WS); process.exit(1); };
ws.onclose = (e) => { console.log(`relay closed the connection (code ${e.code})`); process.exit(1); };
setTimeout(() => { console.log('no verdict for the profile within 20 s'); process.exit(1); }, 20000);
