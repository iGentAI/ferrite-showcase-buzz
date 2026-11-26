/**
 * Read one agent's kind 0 profile, its kind 10100 directory entry, and the
 * rooms whose 39002 roster lists it. Used to work out why the mention picker
 * offered `nova` but never `fabrizio`: the answer was the 10100's `h` tags.
 *
 *   PK=<64-hex pubkey> node e2e/probe-fab-profile.mjs
 *
 * Not a Playwright test (`.mjs`, no `.spec.`), so the suite never collects it.
 */
import { finalizeEvent, generateSecretKey, getPublicKey } from "nostr-tools/pure";
const HOST="3emul7w0sqvwfom6.localhost:28443", WS=`ws://${HOST}/buzz/`;
const FAB = process.env.PK || "9a024c1e6266af6b292759589615f49111a77aa3b49e0dc2def82483e6830e70";
// A fresh random key: a key derived from a label is recomputable by anyone.
const sk=generateSecretKey();
const ws=new WebSocket(WS); const okFor=new Map(); let authed=false;
const sign=(k,t,c="")=>finalizeEvent({kind:k,created_at:Math.floor(Date.now()/1000),tags:t,content:c},sk);
const pub=(ev,v="EVENT")=>new Promise(r=>{okFor.set(ev.id,r);ws.send(JSON.stringify([v,ev]))});
ws.onmessage=async e=>{const m=JSON.parse(String(e.data));
 if(m[0]==="OK"){const r=okFor.get(m[1]); if(r){okFor.delete(m[1]);r()}return}
 if(m[0]==="EVENT"){console.log(`kind=${m[2].kind}`, JSON.stringify(m[2].content).slice(0,200), JSON.stringify(m[2].tags).slice(0,150));return}
 if(m[0]==="EOSE"){console.log("--eose",m[1]);return}
 if(m[0]==="AUTH"&&!authed){authed=true; await pub(sign(22242,[["relay",WS],["challenge",m[1]]]),"AUTH");
  ws.send(JSON.stringify(["REQ","p",{authors:[FAB],kinds:[0,10100],limit:5}]));
  ws.send(JSON.stringify(["REQ","m",{kinds:[39002],"#p":[FAB],limit:10}]));
  setTimeout(()=>process.exit(0),8000);}};
