#!/usr/bin/env node
// Namespace adapter between block/buzz's E2E suites and a Ferrite Buzz deployment.
//
// WHY THIS EXISTS
// buzz's relay owns its whole origin: the WebSocket is at `/`, the HTTP bridge
// is at `/events`, `/query`, `/count`, NIP-11 is at `/info` and NIP-05 is at
// `/.well-known/nostr.json`. A Ferrite app owns a *namespace*, not an origin:
// the gateway hands it `/buzz/` and nothing above that, so the same faces live
// at `/buzz/` and `/buzz/api/...`. That is a deployment fact, not a protocol
// disagreement, and rewriting a path is the honest way to absorb it -- the
// alternative is editing upstream's tests, which would make the results ours
// instead of theirs.
//
// WHAT IT DELIBERATELY DOES NOT DO
// It does not touch authentication. Upstream's suites authenticate HTTP calls
// with a bare `X-Pubkey:` header (buzz's dev-mode fallback, gated on
// `require_auth_token = false`); our relay requires a NIP-98 event signed by
// the caller. A proxy cannot bridge that: it never sees the caller's secret
// key, and `submit_event` refuses any event whose author differs from the
// authenticated caller (ingress.rs, "event pubkey does not match the
// authenticated caller"). Those calls are forwarded unchanged and fail with a
// 401, which is the true answer.
//
// It also preserves the path for `/buzz/api/...` calls, because the suites sign
// those with NIP-98 and our `auth::url_covers_path` compares the `u` tag's path
// to the real path. Rewriting them would break signatures that are currently
// valid. Point the suites at `ws://127.0.0.1:<port>/buzz` and every NIP-98 `u`
// tag they mint already names the path the relay will see.
//
// Usage: node shim.mjs --port 38443 --upstream-relay ws://host:28443/buzz/

import http from "node:http";
import net from "node:net";

function arg(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

const PORT = Number(arg("port", "38443"));
const UPSTREAM = arg("upstream-relay", "ws://127.0.0.1:3000/buzz/");
const VERBOSE = process.argv.includes("--verbose");

const u = new URL(UPSTREAM.replace(/^ws:/, "http:").replace(/^wss:/, "https:"));
if (u.protocol === "https:") {
  console.error("shim: TLS upstream is not implemented; tunnel it to plain http first");
  process.exit(2);
}
const UP_HOST = u.hostname;
const UP_PORT = Number(u.port || 80);
const UP_HOSTHEADER = u.port ? `${u.hostname}:${u.port}` : u.hostname;
// The namespace the app is mounted under, always with exactly one trailing slash.
const NS = ("/" + u.pathname.replace(/^\/+|\/+$/g, "") + "/").replace("//", "/");
const API = NS + "api/";

// Upstream-origin path -> Ferrite-namespaced path.
// Keys are matched against the path AFTER the namespace prefix is stripped.
const REWRITE = new Map([
  ["events", "events"],
  ["query", "query"],
  ["count", "count"],
  ["info", "relay-info"],
  [".well-known/nostr.json", "nip05"],
]);

function mapPath(raw) {
  const [pathOnly, query] = raw.split("?");
  let rest = pathOnly.startsWith(NS)
    ? pathOnly.slice(NS.length)
    : pathOnly.replace(/^\/+/, "");
  rest = rest.replace(/^\/+/, "");

  // Already namespaced (`/buzz/api/...`): pass through untouched so the NIP-98
  // `u` tag the caller signed still names the path the relay checks.
  if (rest.startsWith("api/") || rest === "api") {
    return pathOnly + (query ? "?" + query : "");
  }
  if (rest === "" ) {
    // Bare `/buzz` or `/buzz/`: the WebSocket face, and the NIP-11 door for a
    // client that asks with `Accept: application/nostr+json`.
    return NS + (query ? "?" + query : "");
  }
  const mapped = REWRITE.get(rest);
  if (mapped) return API + mapped + (query ? "?" + query : "");
  // Anything else (media, unknown routes) is forwarded verbatim under the
  // namespace so the 404 the caller sees is the relay's own.
  return NS + rest + (query ? "?" + query : "");
}

const log = [];
function note(kind, from, to, extra) {
  const line = `${new Date().toISOString()} ${kind} ${from} -> ${to}${extra ? " " + extra : ""}`;
  log.push(line);
  if (VERBOSE) console.log(line);
}

const server = http.createServer((req, res) => {
  const target = mapPath(req.url);
  const headers = { ...req.headers, host: UP_HOSTHEADER };
  // NIP-11 convention: `Accept: application/nostr+json` on the relay root is
  // the relay-info document. Ferrite serves it at a named route.
  if (
    target === NS &&
    String(req.headers["accept"] || "").includes("application/nostr+json")
  ) {
    return proxyHttp(req, res, API + "relay-info", headers);
  }
  return proxyHttp(req, res, target, headers);
});

function proxyHttp(req, res, target, headers) {
  const upstreamReq = http.request(
    { host: UP_HOST, port: UP_PORT, method: req.method, path: target, headers },
    (upstreamRes) => {
      note("http", `${req.method} ${req.url}`, target, `=> ${upstreamRes.statusCode}`);
      res.writeHead(upstreamRes.statusCode || 502, upstreamRes.headers);
      upstreamRes.pipe(res);
    },
  );
  upstreamReq.on("error", (e) => {
    note("http", `${req.method} ${req.url}`, target, `=> ERR ${e.message}`);
    if (!res.headersSent) res.writeHead(502, { "content-type": "application/json" });
    res.end(JSON.stringify({ ok: false, error: `shim upstream error: ${e.message}` }));
  });
  req.pipe(upstreamReq);
}

server.on("upgrade", (req, clientSocket, head) => {
  const target = mapPath(req.url);
  const upstream = net.connect(UP_PORT, UP_HOST, () => {
    const lines = [`${req.method} ${target} HTTP/1.1`, `Host: ${UP_HOSTHEADER}`];
    for (const [k, v] of Object.entries(req.headers)) {
      if (k.toLowerCase() === "host") continue;
      for (const one of Array.isArray(v) ? v : [v]) lines.push(`${k}: ${one}`);
    }
    upstream.write(lines.join("\r\n") + "\r\n\r\n");
    if (head && head.length) upstream.write(head);
    upstream.pipe(clientSocket);
    clientSocket.pipe(upstream);
    note("ws", req.url, target);
  });
  const bail = (e) => {
    note("ws", req.url, target, `=> ERR ${e.message}`);
    clientSocket.destroy();
  };
  upstream.on("error", bail);
  clientSocket.on("error", () => upstream.destroy());
});

process.on("SIGTERM", () => dump(0));
process.on("SIGINT", () => dump(0));
function dump(code) {
  console.log(`\n--- shim saw ${log.length} request(s) ---`);
  for (const l of log) console.log(l);
  process.exit(code);
}

server.listen(PORT, "127.0.0.1", () => {
  console.log(
    `shim: 127.0.0.1:${PORT} -> ${UP_HOST}:${UP_PORT} (namespace ${NS}); ` +
      `point suites at ws://127.0.0.1:${PORT}${NS.replace(/\/$/, "")}`,
  );
});
