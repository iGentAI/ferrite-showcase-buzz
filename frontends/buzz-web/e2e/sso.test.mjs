// Run: node --import ./test-loader.mjs --test e2e/sso.test.mjs
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

delete process.env.BUZZ_SSO_ORIGIN;
const { brokerOrigin, carriesBrokerSession } = await import("./sso.ts");

const WEB_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const ORIGIN = "https://buzz.example.com";

test("F135: the session goes only to the broker's exact origin", () => {
  assert.equal(carriesBrokerSession(`${ORIGIN}/buzz/api/query`, ORIGIN), true);
  assert.equal(carriesBrokerSession(`${ORIGIN}/buzz/api/query`, `${ORIGIN}/`), true, "a trailing slash in the setting is fine");
  for (const url of [
    "https://buzz.example.com.attacker.tld/x",
    "https://buzz.example.com@attacker.tld/x",
    "https://buzz.example.com:8443.attacker.tld/",
    "https://buzz.example.com:8443/x",
    "http://buzz.example.com/x",
    "https://user:pass@buzz.example.com/x",
    "/relative/path",
  ]) {
    assert.equal(carriesBrokerSession(url, ORIGIN), false, url);
  }
});

test("F135: the redemption callback never carries the session it is minting", () => {
  assert.equal(carriesBrokerSession(`${ORIGIN}/_sso/callback?a=x`, ORIGIN), false);
});

test("F135: a cleartext broker origin is refused unless it is loopback", () => {
  assert.throws(() => brokerOrigin("http://buzz.example.com"), /must be https/);
  assert.equal(carriesBrokerSession("http://buzz.example.com/x", "http://buzz.example.com"), false);
  assert.equal(carriesBrokerSession("http://localhost:4173/buzz/", "http://localhost:4173"), true);
  assert.equal(carriesBrokerSession("http://abc.localhost:28443/buzz/", "http://abc.localhost:28443"), true);
});

test("F135: the installed fetch wrapper sends no cookie to a look-alike host", () => {
  // The wrapper installs itself at import when BUZZ_SSO_ORIGIN is set, so it
  // runs in a child process with a stubbed network.
  const script = `
    const calls = [];
    globalThis.fetch = async (input, init = {}) => {
      const url = typeof input === "string" ? input : input.url;
      calls.push({ url, cookie: new Headers(init.headers ?? {}).get("cookie") });
      if (new URL(url).pathname === "/_sso/callback") {
        const headers = new Headers();
        headers.append("set-cookie", "__Host-ferrite_app_session=SESSIONVALUE; Path=/; Secure; HttpOnly");
        return { status: 303, headers, text: async () => "" };
      }
      return new Response("ok");
    };
    await import("./e2e/sso.ts");
    await fetch("https://buzz.example.com.attacker.tld/steal");
    await fetch("https://buzz.example.com@attacker.tld/steal");
    await fetch("https://buzz.example.com/buzz/api/agents");
    console.log(JSON.stringify(calls));
  `;
  const run = spawnSync(process.execPath, ["--import", "./test-loader.mjs", "--input-type=module", "-e", script], {
    cwd: WEB_ROOT,
    env: {
      ...process.env,
      BUZZ_SSO_ORIGIN: ORIGIN,
      BUZZ_SSO_SUBJECT_DOMAIN: "example.com",
      SSO_SHARED_SECRET: "test-shared-secret-test-shared-secret",
    },
    encoding: "utf8",
    timeout: 20_000,
  });
  assert.equal(run.status, 0, run.stderr);
  const calls = JSON.parse(run.stdout.trim().split("\n").at(-1));
  const toAttacker = calls.filter((call) => call.url.includes("steal"));
  assert.equal(toAttacker.length, 2);
  for (const call of toAttacker) assert.equal(call.cookie, null, `no cookie to ${call.url}`);
  const own = calls.find((call) => call.url === "https://buzz.example.com/buzz/api/agents");
  assert.equal(own.cookie, "__Host-ferrite_app_session=SESSIONVALUE");
});
