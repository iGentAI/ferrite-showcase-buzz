/**
 * The desktop bridge — buzz's client, running in a plain browser tab.
 *
 * buzz's UI was written against a Tauri shell: an IPC channel into a Rust
 * process (`invoke`), an app-wide event bus (`listen`/`emit`), and OS plugins
 * for notifications, opening URLs, self-updating, relaunching and driving the
 * native window. This module is the ONE place that stands in for all of it, so
 * the web build has no `@tauri-apps/*` dependency anywhere and no call site had
 * to change shape: every export keeps the signature of the Tauri API it
 * replaces.
 *
 * Three kinds of stand-in live here, and the difference is always stated:
 *
 *   1. A real browser equivalent. `openUrl` opens a tab, notifications go
 *      through the Notification API, `relaunch` reloads, theme changes come off
 *      a media query, fullscreen off the Fullscreen API.
 *   2. A browser backend for buzz's own Rust commands. `invoke` routes the
 *      commands the chat path needs — relay config, identity, signing, and the
 *      relay socket itself — into `@/shared/browser-backend`. That is how the
 *      real client connects and authenticates from a tab.
 *   3. An honest unavailable state. Self-updating, dragging an OS window,
 *      native STT/huddle audio, local model (mesh) compute and locally spawned
 *      agent harnesses do not exist in a tab. Those reject with a
 *      `DesktopUnavailableError` whose message is written for a human, because
 *      buzz's own UI shows a failed action's message: the feature keeps
 *      rendering and the button explains itself instead of dying quietly.
 *
 * Nothing here fakes success. A capability that cannot exist says so.
 */

import {
  handleAppCommand,
  handleRawAppCommand,
  unavailableCommandMessage,
} from "@/shared/browser-backend/commands";
import { handleWebsocketCommand } from "@/shared/browser-backend/websocket";
import {
  canNativeEvents,
  detectDesktopRuntime,
  dispatchDecision,
  routesNative,
  type DesktopRuntime,
} from "@/shared/lib/desktopDispatch";

/**
 * Which world this bundle woke up in. Detected once: the shell injects its
 * marker via an initialization script BEFORE any bundle code runs, so the
 * first caller — however early — sees the truth. In a tab (and under every
 * test mock) this is `{ native: false }`.
 */
let cachedRuntime: DesktopRuntime | null = null;

function desktopRuntime(): DesktopRuntime {
  if (cachedRuntime === null) {
    cachedRuntime = detectDesktopRuntime(
      typeof window === "undefined" ? undefined : window,
    );
  }
  return cachedRuntime;
}

type RealInvoke = (
  cmd: string,
  args?: unknown,
  options?: InvokeOptions,
) => Promise<unknown>;

/** Route one command to the shell's genuine IPC. Only called in native mode. */
function nativeInvoke<T>(
  cmd: string,
  args?: unknown,
  options?: InvokeOptions,
): Promise<T> {
  const t = internals();
  if (!t || typeof t.invoke !== "function") {
    return Promise.reject(
      new DesktopUnavailableError(cmd, "the desktop shell's IPC is not available"),
    );
  }
  return (t.invoke as unknown as RealInvoke)(cmd, args ?? {}, options) as Promise<T>;
}

/**
 * Thrown when a caller asks for something only the desktop shell can do. The
 * message is user-facing: buzz surfaces a rejected action's message directly
 * (see `formatHuddleActionError`, `relayError`, the settings panels), so it
 * reads as an explanation rather than a stack trace.
 */
export class DesktopUnavailableError extends Error {
  /** The Tauri command or capability that was asked for. */
  readonly capability: string;

  constructor(capability: string, message: string) {
    super(message);
    this.name = "DesktopUnavailableError";
    this.capability = capability;
  }
}

const announced = new Set<string>();

/** Log a missing capability once per session — loud enough to find, quiet enough to live with. */
function announceOnce(capability: string, detail: string): void {
  if (announced.has(capability)) {
    return;
  }
  announced.add(capability);
  console.info(`[desktop-bridge] ${capability}: ${detail}`);
}

// `@tauri-apps/api/core`: IPC

/**
 * Tauri's IPC `Channel`: a callback slot the other side pushes messages into.
 * Same shape here, fed by the browser transports in `@/shared/browser-backend`
 * (the relay socket is the one that matters).
 */
export class Channel<T = unknown> {
  #handler: ((message: T) => void) | undefined;
  id = 0;

  constructor(handler?: (message: T) => void) {
    this.#handler = handler;
  }

  set onmessage(handler: (message: T) => void) {
    this.#handler = handler;
  }

  get onmessage(): (message: T) => void {
    return this.#handler ?? (() => {});
  }

  /** Deliver one message to the subscriber (natively the Rust side's job). */
  send(message: T): void {
    this.#handler?.(message);
  }

  toJSON(): string {
    return `__CHANNEL__:${this.id}`;
  }
}

/** Handler installed by {@link mockIPC}; the e2e bridge's mocked command backend. */
type MockHandler = (cmd: string, payload?: unknown) => unknown;
let mockHandler: MockHandler | null = null;

/**
 * The object Tauri publishes on `window` for its own API to talk to. The only
 * reason it exists here is that buzz's e2e bridge writes its event mock onto it
 * (`__TAURI_INTERNALS__.listen`) after calling {@link mockIPC}.
 */
type TauriInternals = {
  invoke?: MockHandler;
  listen?: (
    event: string,
    cb: (payload?: unknown) => void,
  ) => Promise<() => void>;
  /** Present ONLY on the genuine Tauri runtime; no mock installs it. */
  transformCallback?: (cb: (payload: unknown) => void, once?: boolean) => number;
};

function internals(): TauriInternals | undefined {
  return (window as unknown as { __TAURI_INTERNALS__?: TauriInternals })
    .__TAURI_INTERNALS__;
}

/**
 * Tauri 2's invoke options: mirrored request headers, used by the raw-body
 * commands to carry metadata (filename, progress id) beside a binary payload.
 */
export type InvokeOptions = { headers?: Record<string, string> };

function lowercaseHeaders(
  headers: Record<string, string> | undefined,
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [key, value] of Object.entries(headers ?? {})) {
    out[key.toLowerCase()] = value;
  }
  return out;
}

/**
 * buzz's bridge into its Rust backend, browser edition. In order:
 *
 *   1. A mocked command backend: one installed via {@link mockIPC}, or a
 *      handler published straight onto `window.__TAURI_INTERNALS__.invoke` —
 *      the seam the real `@tauri-apps/api/core` routes through, which
 *      upstream's unit tests stub by command name without calling mockIPC.
 *      Same precedence Tauri's own mock has (and {@link listen} already gives
 *      `internals().listen`), which keeps buzz's Playwright and node suites
 *      meaningful against this build. Neither exists in a live tab.
 *   2. `plugin:websocket|*`, served by a real browser `WebSocket`. This is the
 *      relay transport, so every line of subscription/reconnect logic above it
 *      is buzz's own, unchanged.
 *   3. buzz's own commands that a tab can answer: relay URLs, identity,
 *      signing, NIP-44 self-encryption (see `browser-backend/commands.ts`).
 *   4. Anything else — a native capability. Rejects with a sentence the UI can
 *      show the user.
 */
export function invoke<T = unknown>(
  cmd: string,
  args?: unknown,
  options?: InvokeOptions,
): Promise<T> {
  // A mockIPC handler, or the e2e bridge's
  // internals stub (which never carries transformCallback), always wins.
  // Under a REAL shell the internals
  // object is the genuine IPC entry, not a mock, and must not be treated as
  // one. The decision itself is pure and unit-tested (desktopDispatch.ts).
  const rt = desktopRuntime();
  const mocked = mockHandler ?? (rt.native ? undefined : internals()?.invoke);
  const decision = dispatchDecision(Boolean(mocked), rt, cmd);
  // `@tauri-apps/api/core`'s `invoke(cmd, args = {}, options)` defaults the
  // arguments to an empty object before they reach the IPC entry, and
  // upstream's unit tests stub that entry and assert on exactly that shape (a
  // wrapper calling `invokeTauri("fetch_persona_catalog")` is seen as
  // `["fetch_persona_catalog", {}]`). The mock route stands in for that entry,
  // so it applies the same default. The native route passes the caller's
  // arguments through unchanged (`desktopDispatch.test.mjs` pins that), and
  // the seam's own handlers coerce for themselves.
  if (decision === "mock" && mocked) {
    try {
      return Promise.resolve(mocked(cmd, args === undefined ? {} : args) as T);
    } catch (error) {
      return Promise.reject(error);
    }
  }

  // A command the shell REGISTERS routes to native IPC;
  // everything else falls through to the browser seam unchanged, so the web
  // experience is the floor and the shell is a strict superset.
  if (decision === "native") {
    return nativeInvoke<T>(cmd, args, options);
  }

  if (args instanceof Uint8Array) {
    // Tauri's raw-body IPC (`InvokeBody::Raw`): the argument IS the bytes, and
    // the metadata rides in `options.headers`. Routed to the raw command
    // table; a raw command outside it is a native capability.
    const raw = handleRawAppCommand(cmd, args, lowercaseHeaders(options?.headers));
    if (raw) {
      return raw as Promise<T>;
    }
    const message = unavailableCommandMessage(cmd);
    announceOnce(`invoke("${cmd}")`, message);
    return Promise.reject(new DesktopUnavailableError(cmd, message));
  }

  const socket = handleWebsocketCommand(cmd, args);
  if (socket) {
    return socket as Promise<T>;
  }

  const app = handleAppCommand(cmd, args);
  if (app) {
    return app as Promise<T>;
  }

  const message = unavailableCommandMessage(cmd);
  announceOnce(`invoke("${cmd}")`, message);
  return Promise.reject(new DesktopUnavailableError(cmd, message));
}

/**
 * Tauri implements its plugins AS commands (`plugin:opener|open_url`,
 * `plugin:window|set_badge_count`, …), and buzz's e2e bridge mocks that command
 * surface — specs assert on the URLs the app opened, the zoom it set, the
 * updater flow it ran. So when a mock backend is installed, the plugin
 * stand-ins below defer to it under the real command names; with no mock they
 * do the browser thing. Only the commands the bridge actually implements are
 * routed this way: an unknown command is a hard error there, by design.
 */
function mockedPluginCall<T>(cmd: string, args?: unknown): Promise<T> | null {
  const rt = desktopRuntime();
  const mockInstalled = Boolean(
    mockHandler ?? (rt.native ? undefined : internals()?.invoke),
  );
  if (mockInstalled || routesNative(rt, cmd)) {
    return invoke<T>(cmd, args);
  }
  return null;
}

/**
 * Tauri's raw-binary IPC (`InvokeBody::Raw`), used by the huddle mic tap to
 * hand PCM frames to the native STT pipeline with no copy. A browser has no
 * such peer: the mic still opens and the AudioWorklet still runs, but the
 * frames have nowhere to go, so this rejects instead of pretending they landed.
 */
export function invokeRawBinary(
  cmd: string,
  payload: Uint8Array,
): Promise<unknown> {
  // Same rule as invoke(), no exceptions for raw bodies: an installed mock
  // wins, a registered command routes native, everything else is refused with
  // the seam's honest sentence.
  const rt = desktopRuntime();
  const mocked = mockHandler ?? (rt.native ? undefined : internals()?.invoke);
  const decision = dispatchDecision(Boolean(mocked), rt, cmd);
  if (decision === "mock" && mocked) {
    try {
      return Promise.resolve(mocked(cmd, payload));
    } catch (error) {
      return Promise.reject(error);
    }
  }
  if (decision === "native") {
    return nativeInvoke(cmd, payload);
  }
  const message = unavailableCommandMessage(cmd);
  announceOnce(`invokeRawBinary("${cmd}")`, message);
  return Promise.reject(new DesktopUnavailableError(cmd, message));
}

/**
 * Whether the app is running inside the buzz-desktop Tauri shell. False in a
 * tab and under every mock; true exactly when the shell's injected marker AND
 * genuine Tauri internals are both present. buzz
 * branches on this in a dozen places to pick desktop paths — under the thin
 * shell, families the shell does not register keep their seam behavior, so a
 * desktop path whose command is unregistered still explains itself honestly.
 */
export function isTauri(): boolean {
  return desktopRuntime().native;
}

/**
 * Whether this shell answers `cmd` natively. `isTauri()` says only that a
 * shell is present; a family the shell does not register still falls through
 * to the browser seam, so a desktop-only feature gates on its commands.
 */
export function commandRoutesNative(cmd: string): boolean {
  return routesNative(desktopRuntime(), cmd);
}

// `@tauri-apps/api/event`

export type UnlistenFn = () => void;
export type Event<T> = { event: string; id: number; payload: T };
export type EventCallback<T> = (event: Event<T>) => void;

/**
 * The app-wide event bus. Natively both ends are used: Rust emits
 * (`ptt-state`, `mesh-download-progress`, `agents-data-changed`, deep links)
 * and the UI listens. Here it is a same-page bus, so events the app emits
 * itself are delivered and desktop-originated ones simply never arrive — which
 * is the truth, and every listener in the tree is written to tolerate silence.
 */
const bus = new EventTarget();
let nextEventId = 1;

export function listen<T>(
  event: string,
  handler: EventCallback<T>,
): Promise<UnlistenFn> {
  // Under the e2e bridge, specs drive backend events through the mocked
  // internals listener; prefer it so those subscriptions stay live.
  const mocked = internals()?.listen;
  if (mocked) {
    return mocked(event, (payload) => {
      handler({ event, id: nextEventId++, payload: payload as T });
    });
  }

  // Under the shell, the genuine Tauri event system carries shell-emitted
  // events (harness lifecycle, permission requests) to the page — but ONLY
  // when the shell registered both halves of it; the per-command rule has no
  // exceptions, events included.
  const rt = desktopRuntime();
  if (canNativeEvents(rt)) {
    const t = internals();
    if (t && typeof t.transformCallback === "function") {
      const callbackId = t.transformCallback((raw: unknown) => {
        const e = raw as { event: string; id: number; payload: T };
        handler({ event: e.event, id: e.id, payload: e.payload });
      });
      return nativeInvoke<number>("plugin:event|listen", {
        event,
        target: { kind: "Any" },
        handler: callbackId,
      }).then((eventId) => () => {
        void nativeInvoke("plugin:event|unlisten", { event, eventId });
      });
    }
  }

  const wrapped = (e: globalThis.Event) => {
    handler({ event, id: nextEventId++, payload: (e as CustomEvent<T>).detail });
  };
  bus.addEventListener(event, wrapped);
  return Promise.resolve(() => bus.removeEventListener(event, wrapped));
}

export function emit<T>(event: string, payload?: T): Promise<void> {
  const rt = desktopRuntime();
  if (routesNative(rt, "plugin:event|emit")) {
    return nativeInvoke<void>("plugin:event|emit", { event, payload });
  }
  bus.dispatchEvent(new CustomEvent(event, { detail: payload }));
  return Promise.resolve();
}

// `@tauri-apps/api/window`

export type Theme = "light" | "dark";

/**
 * Tauri's dock/taskbar attention enum, kept so call sites type-check unchanged
 * (`UserAttentionType.Informational` reads and resolves exactly as upstream).
 *
 * A `const` object rather than a TypeScript `enum` on purpose: an `enum` emits
 * runtime code, so Node's strip-only type stripping refuses the whole module
 * with ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX. Every file that reaches the bridge
 * through the `@tauri-apps/*` facades reaches this declaration, so one `enum`
 * here took 111 unit-test suites down with it. The object plus companion type
 * is behaviourally identical and strips to nothing.
 */
export const UserAttentionType = {
  Critical: 1,
  Informational: 2,
} as const;
export type UserAttentionType =
  (typeof UserAttentionType)[keyof typeof UserAttentionType];

/**
 * The app window, browser edition. A tab is not an OS window: it cannot be
 * dragged by its content, minimised, or badged in the dock. What a browser DOES
 * expose — fullscreen state, focus, the OS colour scheme, resize — is wired to
 * the real thing.
 */
class BrowserAppWindow {
  label = "main";

  async isFullscreen(): Promise<boolean> {
    return Boolean(document.fullscreenElement);
  }

  async setFocus(): Promise<void> {
    const mocked = mockedPluginCall<void>("plugin:window|set_focus");
    if (mocked) {
      await mocked;
      return;
    }
    window.focus();
  }

  /** A tab cannot show or unminimise itself; the OS/browser owns that. */
  async show(): Promise<void> {
    const mocked = mockedPluginCall<void>("plugin:window|show");
    if (mocked) {
      await mocked;
      return;
    }
    announceOnce("window.show", "a browser tab cannot raise itself");
  }

  /**
   * Close the window. Upstream calls this on a huddle COMPANION window after
   * its room ends; in this build huddles render inline, so the main tab never
   * takes that path. `window.close()` is still the honest attempt — a browser
   * honours it exactly when a script opened the window, which is the only way
   * a companion could have existed here.
   */
  async close(): Promise<void> {
    const mocked = mockedPluginCall<void>("plugin:window|close");
    if (mocked) {
      await mocked;
      return;
    }
    window.close();
    announceOnce(
      "window.close",
      "a browser tab closes itself only when a script opened it",
    );
  }

  async unminimize(): Promise<void> {
    const mocked = mockedPluginCall<void>("plugin:window|unminimize");
    if (mocked) {
      await mocked;
      return;
    }
    announceOnce("window.unminimize", "a browser tab cannot unminimise itself");
  }

  /**
   * Native title-bar drag. There is no OS window to move, so this does
   * nothing — buzz's drag region keeps rendering and stays inert rather than
   * throwing under every pointerdown.
   */
  async startDragging(): Promise<void> {
    announceOnce(
      "window.startDragging",
      "the title bar cannot move a browser window",
    );
  }

  /**
   * Dock/taskbar badge. Unavailable in a tab; buzz also renders its unread
   * count in the sidebar, which is what the user actually sees here.
   */
  async setBadgeCount(count?: number): Promise<void> {
    // Tauri 2's window plugin setters take `value` (and an optional window
    // label that defaults to the calling window), like set_webview_zoom.
    const mocked = mockedPluginCall<void>("plugin:window|set_badge_count", {
      value: count,
    });
    if (mocked) {
      await mocked;
      return;
    }
    announceOnce("window.setBadgeCount", "no dock badge in a browser tab");
  }

  async setBadgeLabel(label?: string): Promise<void> {
    const mocked = mockedPluginCall<void>("plugin:window|set_badge_label", {
      value: label,
    });
    if (mocked) {
      await mocked;
      return;
    }
    announceOnce("window.setBadgeLabel", "no dock badge in a browser tab");
  }

  /** Dock bounce / taskbar flash: no equivalent, and faking a flash would be worse. */
  async requestUserAttention(
    _type?: UserAttentionType | null,
  ): Promise<void> {
    announceOnce(
      "window.requestUserAttention",
      "no dock bounce in a browser tab",
    );
  }

  /** OS colour-scheme changes ARE observable in a browser. */
  async onThemeChanged(
    handler: (event: { payload: Theme }) => void,
  ): Promise<UnlistenFn> {
    const query = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = (event: MediaQueryListEvent) => {
      handler({ payload: event.matches ? "dark" : "light" });
    };
    query.addEventListener("change", onChange);
    return () => query.removeEventListener("change", onChange);
  }

  async onResized(
    handler: (event: {
      payload: { width: number; height: number };
    }) => void,
  ): Promise<UnlistenFn> {
    const onResize = () => {
      handler({
        payload: { width: window.innerWidth, height: window.innerHeight },
      });
    };
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }
}

const appWindow = new BrowserAppWindow();

export function getCurrentWindow(): BrowserAppWindow {
  return appWindow;
}

export type { BrowserAppWindow as AppWindow };

// `@tauri-apps/api/webview`

class BrowserWebview {
  label = "main";

  /**
   * Page zoom belongs to the user agent in a browser, and buzz only calls this
   * to pin the webview at 1 (it scales text itself via the root font size, in
   * `useWebviewZoomShortcuts`) — so the text-scale shortcuts work here exactly
   * as they do on the desktop, and there is nothing left to do.
   */
  async setZoom(factor: number): Promise<void> {
    const mocked = mockedPluginCall<void>("plugin:webview|set_webview_zoom", {
      value: factor,
    });
    if (mocked) {
      await mocked;
    }
  }
}

const webview = new BrowserWebview();

export function getCurrentWebview(): BrowserWebview {
  return webview;
}

export type { BrowserWebview as Webview };

// `@tauri-apps/api/app`

/** The bundle's version, baked in at build time; "web" when unset. */
export async function getVersion(): Promise<string> {
  return import.meta.env.VITE_APP_VERSION ?? "web";
}

// `@tauri-apps/api/path`

/**
 * A browser has no filesystem. buzz asks for `homeDir` only to abbreviate a
 * repo path for display (`~/code/buzz`); the empty string makes that
 * abbreviation a no-op and the full string is shown instead.
 */
export async function homeDir(): Promise<string> {
  return "";
}

// `@tauri-apps/plugin-notification`

export type NotificationPayload = {
  title: string;
  body?: string;
  icon?: string;
  /** Tauri's free-form payload bag, carried through to the action handler. */
  extra?: Record<string, unknown>;
};

export async function isPermissionGranted(): Promise<boolean> {
  return (
    typeof Notification !== "undefined" && Notification.permission === "granted"
  );
}

export async function requestPermission(): Promise<NotificationPermission> {
  if (typeof Notification === "undefined") {
    return "denied";
  }
  return Notification.requestPermission();
}

/**
 * Clicks on a NATIVE OS notification. A browser notification's click arrives on
 * the `Notification` object itself, and buzz already takes that path when
 * `isTauri()` is false (`features/notifications/lib/desktop.ts` sets
 * `notification.onclick`). So this registers nothing and never fires — click
 * routing is not lost, it just happens elsewhere. The returned handle keeps
 * Tauri's shape so the caller unregisters unchanged.
 */
export async function onAction(
  _handler: (notification: NotificationPayload) => void,
): Promise<{ unregister: () => Promise<void> }> {
  return { unregister: async () => {} };
}

// `@tauri-apps/plugin-opener`

/** Open an external URL in a new tab. `noopener` so the target cannot reach back. */
export async function openUrl(url: string, openWith?: string): Promise<void> {
  const mocked = mockedPluginCall<void>("plugin:opener|open_url", {
    url,
    with: openWith,
  });
  if (mocked) {
    await mocked;
    return;
  }
  window.open(url, "_blank", "noopener,noreferrer");
}

// `@tauri-apps/plugin-process`

/**
 * Restart the app. A reload is the browser's version of it, and it is the right
 * one for every caller here: the keyring-locked and recovery screens use it to
 * start over from a clean boot.
 */
export async function relaunch(): Promise<void> {
  const mocked = mockedPluginCall<void>("plugin:process|restart");
  if (mocked) {
    await mocked;
    return;
  }
  window.location.reload();
}

// `@tauri-apps/plugin-updater`

export type DownloadEvent =
  | { event: "Started"; data: { contentLength?: number } }
  | { event: "Progress"; data: { chunkLength: number } }
  | { event: "Finished" };

/** Tauri's `Update` handle. Kept whole so `use-updater`'s refs and calls type-check unchanged. */
export type Update = {
  available: boolean;
  version: string;
  currentVersion: string;
  date?: string;
  body?: string;
  download: (onEvent?: (event: DownloadEvent) => void) => Promise<void>;
  install: () => Promise<void>;
  downloadAndInstall: (
    onEvent?: (event: DownloadEvent) => void,
  ) => Promise<void>;
  close: () => Promise<void>;
};

export type CheckOptions = {
  headers?: Record<string, string>;
  timeout?: number;
  proxy?: string;
  target?: string;
};

const UPDATER_UNAVAILABLE_MESSAGE =
  "The updater plugin is not initialized in the browser build: a web app updates when you reload the page.";

/** What `plugin:updater|check` answers with when an update exists. */
type RawUpdate = {
  rid: number;
  version: string;
  currentVersion: string;
  date?: string;
  body?: string;
};

/** Wrap a raw update in Tauri's `Update` handle, backed by the same commands. */
function updateHandle(raw: RawUpdate): Update {
  let downloadedRid: number | null = null;

  const download = async (onEvent?: (event: DownloadEvent) => void) => {
    const channel = new Channel<DownloadEvent>();
    if (onEvent) {
      channel.onmessage = onEvent;
    }
    downloadedRid = await invoke<number>("plugin:updater|download", {
      onEvent: channel,
      rid: raw.rid,
    });
  };

  const install = async () => {
    await invoke<void>("plugin:updater|install", {
      rid: downloadedRid ?? raw.rid,
    });
  };

  return {
    available: true,
    version: raw.version,
    currentVersion: raw.currentVersion,
    date: raw.date,
    body: raw.body,
    download,
    install,
    downloadAndInstall: async (onEvent) => {
      await download(onEvent);
      await install();
    },
    close: async () => {
      await invoke<void>("plugin:resources|close", { rid: raw.rid });
    },
  };
}

/**
 * Check for an app update.
 *
 * A served bundle has no self-updater, and answering "you're on the latest
 * version" would be a guess about the server. Rejecting is the honest answer,
 * and it lands buzz's updater hook in the `unavailable` state it already has a
 * card for — "Automatic updates aren't available on this build" — with the
 * Check Again button still live. (`use-updater.ts` classifies that state by
 * matching "not initialized" in the message; the wording above is deliberate.)
 *
 * Under the e2e mock backend the whole check → download → install flow still
 * runs, so buzz's updater specs keep testing the hook and its cards.
 */
export async function check(options?: CheckOptions): Promise<Update | null> {
  const mocked = mockedPluginCall<RawUpdate | null>("plugin:updater|check", {
    headers: options?.headers,
    timeout: options?.timeout,
    proxy: options?.proxy,
    target: options?.target,
  });
  if (mocked) {
    const raw = await mocked;
    return raw ? updateHandle(raw) : null;
  }

  throw new DesktopUnavailableError("updater", UPDATER_UNAVAILABLE_MESSAGE);
}

// `@tauri-apps/api/mocks`

export type MockIPCOptions = { shouldMockEvents?: boolean };

/**
 * Install a mocked command backend, exactly as Tauri's own `mockIPC` does:
 * {@link invoke} routes to it ahead of every browser transport, and
 * `window.__TAURI_INTERNALS__.invoke` is published because buzz's e2e bridge
 * hangs its event mock off that same object.
 *
 * Events are always "mocked" here in the sense that matters — {@link listen}
 * prefers the internals listener the bridge installs — so `shouldMockEvents`
 * needs no separate switch.
 */
export function mockIPC(
  handler: MockHandler,
  _options?: MockIPCOptions,
): void {
  mockHandler = handler;
  const existing = internals();
  if (existing) {
    existing.invoke = handler;
    return;
  }
  (
    window as unknown as { __TAURI_INTERNALS__: TauriInternals }
  ).__TAURI_INTERNALS__ = { invoke: handler };
}

/**
 * Remove the mocked command backend, as Tauri's own `clearMocks` does:
 * {@link invoke} goes back to the browser transports, and the published
 * `window.__TAURI_INTERNALS__` is dropped so nothing keeps preferring a
 * mock that is no longer installed.
 */
export function clearMocks(): void {
  mockHandler = null;
  // Under a REAL shell the internals object is the genuine IPC entry —
  // deleting it would sever the app from its own backend. Only a mock's
  // internals (never native) are dropped.
  if (typeof window !== "undefined" && !desktopRuntime().native) {
    delete (window as { __TAURI_INTERNALS__?: TauriInternals })
      .__TAURI_INTERNALS__;
  }
}

/**
 * Name the current window, as Tauri's mock does. A tab has exactly one window
 * and one webview, so the extra labels have nothing to attach to — the label
 * itself is applied, which is what `getCurrentWindow().label` reports.
 */
export function mockWindows(current: string, ..._rest: string[]): void {
  appWindow.label = current;
  webview.label = current;
}
