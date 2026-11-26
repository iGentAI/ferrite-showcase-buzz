/**
 * Live check of the tiered `/buzz/api/metrics` decision.
 *
 * The public document must carry only liveness + the two `*_configured`
 * booleans + the pubkey NIP-11 already publishes; `identity.source` and the
 * `requests` counters must require a valid operator NIP-98. Two escalation
 * attempts are included: a valid NIP-98 from a NON-operator key, and the
 * operator's own token minted for a DIFFERENT path.
 *
 *   node e2e/probe-metrics-tiers.mjs
 *
 * Not a Playwright test (`.mjs`, no `.spec.`), so the suite never collects it.
 */
import fs from "node:fs";
import { finalizeEvent, generateSecretKey } from "nostr-tools/pure";

const HOST = process.env.BUZZ_HOST || "3emul7w0sqvwfom6.localhost:28443";
const URL_METRICS = `http://${HOST}/buzz/api/metrics`;
const prov = JSON.parse(fs.readFileSync("/tmp/buzz-agent-provision.json", "utf8"));
const opSk = Uint8Array.from(Buffer.from(prov.operator_sk, "hex"));
// A fresh random key: sha256("stranger") was a secret anyone could compute.
const strangerSk = generateSecretKey();

function nip98(url, method, sk) {
  const ev = finalizeEvent(
    {
      kind: 27235,
      created_at: Math.floor(Date.now() / 1000),
      tags: [
        ["u", url],
        ["method", method],
      ],
      content: "",
    },
    sk,
  );
  return `Nostr ${Buffer.from(JSON.stringify(ev)).toString("base64")}`;
}

async function get(label, header) {
  const r = await fetch(URL_METRICS, header ? { headers: { "x-buzz-authorization": header } } : undefined);
  const body = await r.json();
  console.log(
    `${label.padEnd(34)} status=${r.status} scope=${body.scope ?? "-"}` +
      ` has_requests=${Boolean(body.requests)} has_identity_source=${Boolean(body.identity?.source)}`,
  );
  return { status: r.status, body };
}

const anon = await get("anonymous", null);
const operator = await get("operator NIP-98", nip98(URL_METRICS, "GET", opSk));
const stranger = await get("valid NIP-98, non-operator", nip98(URL_METRICS, "GET", strangerSk));
const wrongPath = await get(
  "operator token for another path",
  nip98(`http://${HOST}/buzz/api/relay-info`, "GET", opSk),
);

const problems = [];
if (anon.body.requests || anon.body.identity?.source) problems.push("anonymous saw the operator tier");
if (!operator.body.requests || !operator.body.identity?.source) problems.push("operator did NOT see the operator tier");
if (stranger.body.requests || stranger.status === 401) problems.push("non-operator escalated or got a 401 instead of the public document");
if (wrongPath.body.requests || wrongPath.status === 401) problems.push("a cross-path operator token escalated or 401'd");
if (!anon.body.identity?.configured || !("stream_key_configured" in anon.body)) {
  problems.push("the documented restart probe fields are missing from the public document");
}

console.log(problems.length ? `FAIL: ${problems.join("; ")}` : "PROOF: the tiering behaves as documented");
process.exit(problems.length ? 1 : 0);
