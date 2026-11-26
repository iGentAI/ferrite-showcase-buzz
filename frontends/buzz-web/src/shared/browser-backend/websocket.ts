/**
 * Tauri's **websocket plugin**, web edition.
 *
 * buzz's relay client does not open a socket itself: it asks the Tauri Rust
 * side to, through
 *
 *   invoke("plugin:websocket|connect",    { url, onMessage: Channel, config })  -> id
 *   invoke("plugin:websocket|send",       { id, message: { type: "Text", data } })
 *   invoke("plugin:websocket|disconnect", { id })
 *   invoke("plugin:websocket|disconnect_all")
 *
 * and reads inbound frames off the `Channel`. This module answers those four
 * commands with a real browser `WebSocket`, emitting the exact frame shapes the
 * client already parses (`relayClientShared.getTextPayload`,
 * `relayReconnectPolicy.isWebSocketClose`):
 *
 *   { type: "Text",  data: "<frame>" }
 *   { type: "Close", data: { code, reason } }
 *   { type: "Error", data: "<message>" }
 *
 * That is the whole browser port of the transport: every line of subscription,
 * reconnect, replay and backoff logic above it is buzz's own, unchanged.
 */

import type { Channel } from "@/shared/lib/desktop-bridge";

type ConnectArgs = {
  url: string;
  onMessage: Channel<unknown>;
  config?: unknown;
};

type SendArgs = {
  id: number;
  message: { type: "Text"; data: string } | { type: "Binary"; data: number[] };
};

type Sockets = Map<number, WebSocket>;

const sockets: Sockets = new Map();
let nextId = 1;

/** Where the relay mints the ticket its WebSocket face requires. */
export const WS_TICKET_PATH = "/buzz/api/ws-ticket";

/**
 * `url` with the admission ticket the relay's WebSocket face requires.
 *
 * The gateway forwards no cookie to a WebSocket route, so the relay cannot see
 * the Ferrite session on the socket itself; it refuses a new socket that does
 * not carry a ticket, which only its SSO-gated HTTP face mints. A ticket is
 * short-lived, so one is fetched for every connect, reconnects included.
 *
 * Only a relay on this page's own host is asked: another relay has its own
 * rules, and this page's session cookie means nothing to it. A failed fetch
 * leaves the URL unchanged; the relay then refuses the socket with a NOTICE
 * that says why, and the client's reconnect policy handles it like any other
 * refusal.
 */
export async function ticketedRelayUrl(
  url: string,
  pageHost: string | undefined = globalThis.location?.host,
  fetchTicket: typeof fetch = globalThis.fetch,
): Promise<string> {
  let target: URL;
  try {
    target = new URL(url);
  } catch {
    return url;
  }
  if (!pageHost || target.host !== pageHost || !target.pathname.startsWith("/buzz/")) {
    return url;
  }
  try {
    const response = await fetchTicket(WS_TICKET_PATH, {
      method: "POST",
      credentials: "same-origin",
    });
    if (!response.ok) return url;
    const body = (await response.json()) as { ticket?: unknown };
    if (typeof body.ticket !== "string") return url;
    target.searchParams.set("ticket", body.ticket);
    return target.toString();
  } catch {
    return url;
  }
}

/** Open a socket and stream its frames into the caller's channel. */
export async function connect(args: ConnectArgs): Promise<number> {
  const { onMessage } = args;
  const url = await ticketedRelayUrl(args.url);
  return new Promise<number>((resolve, reject) => {
    let socket: WebSocket;
    try {
      socket = new WebSocket(url);
    } catch (error) {
      reject(error instanceof Error ? error : new Error(String(error)));
      return;
    }
    const id = nextId++;
    let settled = false;

    socket.onopen = () => {
      settled = true;
      sockets.set(id, socket);
      resolve(id);
    };

    socket.onmessage = (event: MessageEvent<unknown>) => {
      if (typeof event.data === "string") {
        onMessage.send({ type: "Text", data: event.data });
        return;
      }
      // The relay's face is text-only (NIP-01 is JSON); anything else is
      // surfaced rather than dropped so a protocol change is visible.
      if (event.data instanceof Blob) {
        void event.data.text().then((text) => {
          onMessage.send({ type: "Text", data: text });
        });
        return;
      }
      onMessage.send({ type: "Error", data: "unsupported binary frame" });
    };

    socket.onerror = () => {
      // The browser deliberately hides socket error detail; the client only
      // needs to know the connection is unusable.
      if (!settled) {
        settled = true;
        reject(new Error(`websocket connect failed: ${url}`));
        return;
      }
      onMessage.send({ type: "Error", data: "websocket error" });
    };

    socket.onclose = (event: CloseEvent) => {
      sockets.delete(id);
      if (!settled) {
        settled = true;
        reject(new Error(`websocket closed before open: ${event.code}`));
        return;
      }
      onMessage.send({
        type: "Close",
        data: { code: event.code, reason: event.reason },
      });
    };
  });
}

export function send(args: SendArgs): Promise<void> {
  const socket = sockets.get(args.id);
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    return Promise.reject(new Error(`websocket ${args.id} is not open`));
  }
  if (args.message.type === "Text") {
    socket.send(args.message.data);
  } else {
    socket.send(new Uint8Array(args.message.data));
  }
  return Promise.resolve();
}

/** Idempotent, exactly as the native command is: a missing id is not an error. */
export function disconnect(args: { id: number }): Promise<void> {
  const socket = sockets.get(args.id);
  sockets.delete(args.id);
  socket?.close(1000, "client disconnect");
  return Promise.resolve();
}

function disconnectAll(): Promise<void> {
  for (const [id, socket] of sockets) {
    sockets.delete(id);
    socket.close(1000, "client disconnect");
  }
  return Promise.resolve();
}

/** Dispatch for the `plugin:websocket|*` command namespace. */
export function handleWebsocketCommand(
  cmd: string,
  args: unknown,
): Promise<unknown> | undefined {
  switch (cmd) {
    case "plugin:websocket|connect":
      return connect(args as ConnectArgs);
    case "plugin:websocket|send":
      return send(args as SendArgs);
    case "plugin:websocket|disconnect":
      return disconnect(args as { id: number });
    case "plugin:websocket|disconnect_all":
      return disconnectAll();
    default:
      return undefined;
  }
}
