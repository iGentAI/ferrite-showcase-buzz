/**
 * The socket plumbing shared by the hand-run relay scripts in this directory
 * (`probe-window.mjs`, `repair-starter-rooms.mjs`).
 *
 * Both scripts used to open `ws://${BUZZ_HOST}` and await each OK inside the
 * socket's async message handler. That had two faults. A TLS deployment was
 * addressed with a cleartext scheme, and the AUTH event's `relay` tag named a
 * URL the relay does not answer on. And a missing OK became an unhandled
 * rejection that killed the process with no report: in
 * `repair-starter-rooms.mjs` that could land between an accepted join and the
 * topic edit the script exists to make.
 *
 * Here every wait is bounded, cleared when it settles, and failed as soon as
 * the socket goes away; `runScript` turns any failure into a printed diagnosis
 * and exit code 1.
 *
 * Not a Playwright test (`.mjs`, no `.spec.`), so the suite never collects it.
 */
import { finalizeEvent } from "nostr-tools/pure";

/** True for a host the developer's own machine answers, where plain ws:// is fine. */
export function isLoopbackHost(host) {
  let name;
  try {
    name = new URL(`http://${host}`).hostname.toLowerCase();
  } catch {
    return false;
  }
  return (
    name === "localhost" ||
    name.endsWith(".localhost") ||
    /^127(?:\.\d{1,3}){3}$/.test(name) ||
    name === "[::1]"
  );
}

/**
 * The relay's websocket URL: `BUZZ_WS` verbatim when set, otherwise built from
 * `BUZZ_HOST` (or the script's default) with `wss://` for anything that is not
 * loopback. The same string goes into the AUTH event's `relay` tag.
 */
export function relayWsUrl(env, defaultHost) {
  if (env.BUZZ_WS) return env.BUZZ_WS;
  const host = env.BUZZ_HOST || defaultHost;
  return `${isLoopbackHost(host) ? "ws" : "wss"}://${host}/buzz/`;
}

function budgetFromEnv(name, fallback) {
  const value = Number(process.env[name]);
  return Number.isFinite(value) && value > 0 ? value : fallback;
}

/**
 * Open a socket and complete the NIP-42 exchange.
 *
 * Resolves with `{ url, publish, query, close }` once the relay has accepted
 * the AUTH. `publish` resolves with `[accepted, message]` and rejects when no
 * OK arrives within the budget (`BUZZ_PROBE_OK_MS`, default 30 s) or the socket
 * closes. `query` resolves with the events at EOSE, or with what arrived when
 * its budget runs out (the scripts' old behaviour); it rejects at once when the
 * relay refuses it (CLOSED) or the socket closes, and it always sends CLOSE for
 * a subscription the relay still holds.
 */
export async function connectRelay(url, secret, options = {}) {
  const okMs = options.okMs ?? budgetFromEnv("BUZZ_PROBE_OK_MS", 30_000);
  const queryMs = options.queryMs ?? budgetFromEnv("BUZZ_PROBE_QUERY_MS", 20_000);
  const log = options.log ?? console.log;
  const ws = new WebSocket(url);
  const waiters = new Map();
  let lost = null;
  let nextQuery = 0;

  /** Register a bounded wait under `key`; the timer is cleared however it settles. */
  const track = (key, ms, onTimeout) => {
    let entry;
    const promise = new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        waiters.delete(key);
        onTimeout(resolve, reject, entry);
      }, ms);
      const settle = (fn) => (value) => {
        clearTimeout(timer);
        waiters.delete(key);
        fn(value);
      };
      entry = { resolve: settle(resolve), reject: settle(reject), events: [] };
      waiters.set(key, entry);
    });
    return { entry, promise };
  };

  const lose = (reason) => {
    if (lost) return;
    lost = reason;
    for (const waiter of [...waiters.values()]) waiter.reject(new Error(reason));
  };

  const send = (frame, entry) => {
    try {
      ws.send(JSON.stringify(frame));
    } catch (error) {
      entry.reject(error instanceof Error ? error : new Error(String(error)));
    }
  };

  ws.addEventListener("message", (event) => {
    let message;
    try {
      message = JSON.parse(String(event.data));
    } catch {
      return;
    }
    if (!Array.isArray(message)) return;
    const [verb, id] = message;
    if (verb === "AUTH" && typeof id === "string") {
      waiters.get("challenge")?.resolve(id);
      return;
    }
    if (verb === "OK") {
      waiters.get(`ok:${id}`)?.resolve([Boolean(message[2]), String(message[3] ?? "")]);
      return;
    }
    if (verb === "EVENT") {
      waiters.get(`req:${id}`)?.events.push(message[2]);
      return;
    }
    if (verb === "EOSE") {
      const waiter = waiters.get(`req:${id}`);
      waiter?.resolve(waiter.events);
      return;
    }
    if (verb === "CLOSED") {
      log("relay:", String(event.data).slice(0, 240));
      // A refused subscription is not an empty answer: fail that query now.
      const waiter = waiters.get(`req:${id}`);
      if (waiter) {
        waiter.refused = true;
        waiter.reject(new Error(`the relay refused query ${id}: ${String(message[2] ?? "")}`));
      }
      return;
    }
    if (verb === "NOTICE") {
      log("relay:", String(event.data).slice(0, 240));
    }
  });
  ws.addEventListener("close", (event) => lose(`the relay socket closed (code ${event.code}, ${url})`));
  ws.addEventListener("error", () => lose(`the relay socket errored (${url})`));

  const publish = (event, verb = "EVENT") => {
    if (lost) return Promise.reject(new Error(lost));
    const { entry, promise } = track(`ok:${event.id}`, okMs, (_resolve, reject) =>
      reject(new Error(`the relay sent no OK for ${verb} ${event.id.slice(0, 12)} within ${okMs} ms`)),
    );
    send([verb, event], entry);
    return promise;
  };

  const query = (filter) => {
    if (lost) return Promise.reject(new Error(lost));
    const id = `q${(nextQuery += 1)}`;
    const { entry, promise } = track(`req:${id}`, queryMs, (resolve, _reject, self) => resolve(self.events));
    // A subscription stays open on the relay after EOSE; close it however the
    // query settles, unless the relay already dropped it (CLOSED) or the
    // socket is gone.
    const unsubscribe = () => {
      if (!lost && !entry.refused && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(["CLOSE", id]));
    };
    promise.then(unsubscribe, unsubscribe);
    send(["REQ", id, filter], entry);
    return promise;
  };

  const close = () => {
    lose("closed by the script");
    ws.close();
  };

  const challenge = track("challenge", okMs, (_resolve, reject) =>
    reject(new Error(`the relay sent no NIP-42 challenge within ${okMs} ms (${url})`)),
  ).promise;
  try {
    const auth = finalizeEvent(
      {
        kind: 22242,
        created_at: Math.floor(Date.now() / 1000),
        tags: [
          ["relay", url],
          ["challenge", await challenge],
        ],
        content: "",
      },
      secret,
    );
    const [accepted, reason] = await publish(auth, "AUTH");
    if (!accepted) throw new Error(`the relay refused the NIP-42 AUTH: ${reason}`);
  } catch (error) {
    close();
    throw error;
  }
  return { url, publish, query, close };
}

/**
 * Run a script body with an exit-code contract: the body's return value (default
 * 0) on success, 1 with a printed `FAIL:` diagnosis on any error, and 1 with
 * `TIMEOUT` when the whole run outlives `deadlineMs`.
 */
export function runScript(body, { deadlineMs } = {}) {
  if (deadlineMs) {
    setTimeout(() => {
      console.log(`TIMEOUT after ${deadlineMs} ms`);
      process.exit(1);
    }, deadlineMs).unref();
  }
  return Promise.resolve()
    .then(body)
    .then(
      (code) => process.exit(code ?? 0),
      (error) => {
        console.log(`FAIL: ${error instanceof Error ? error.message : String(error)}`);
        process.exit(1);
      },
    );
}
