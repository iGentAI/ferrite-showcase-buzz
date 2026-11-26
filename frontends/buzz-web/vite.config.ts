import { defineConfig, type ProxyOptions } from "vite";
import react from "@vitejs/plugin-react";
import { tauriWebAlias } from "./scripts/tauri-web-alias.ts";
import { emittedFileCeiling } from "./scripts/emitted-file-ceiling.ts";

const LABS_ASSETS = new URL("./labs-assets/", import.meta.url).pathname;
const WEBP_STAND_INS = [
  { find: /^.*\/assets\/(card-texture(?:-dark)?(?:-compact)?)\.png$/, replacement: `${LABS_ASSETS}$1.webp` },
  { find: /^.*\/assets\/landing\/buzz-wordmark\.png(\?inline)?$/, replacement: `${LABS_ASSETS}buzz-wordmark.webp$1` },
  {
    find: /^.*\/assets\/starter-team\/(bumble|fizz|honey|pollen)\.png(\?inline)?$/,
    replacement: `${LABS_ASSETS}starter-team/$1.webp$2`,
  },
];

/**
 * The SPA is served by the buzz app under `/buzz/ui/`, so every emitted asset URL
 * must carry that prefix — hence `base`.
 *
 * `src/app/router.tsx` deliberately has NO `basepath`, and must not grow one:
 * buzz's own router already runs on `createHashHistory()`, so route paths live
 * after the `#` and are independent of where the document is served from. That
 * is also what makes the content face workable at all — it maps a URL path
 * straight onto a blob key with no index fallback, so a path-history deep link
 * would 404 while a hash one always re-fetches `/buzz/ui/index.html`.
 *
 * For local `vite dev` the relay lives on another port; the proxy keeps the
 * app same-origin (which is what `apiUrl()` assumes) without needing CORS on
 * the relay. Point it elsewhere with BUZZ_RELAY_ORIGIN.
 */
const RELAY_ORIGIN = process.env.BUZZ_RELAY_ORIGIN || "http://127.0.0.1:8787";
const EDGE_BUNDLE = process.env.BUZZ_EDGE_BUNDLE === "1";
const RELAY_VIA_EDGE = process.env.BUZZ_RELAY_VIA_EDGE === "1";

const relayProxy = (ws = false): ProxyOptions => ({
  target: RELAY_ORIGIN,
  changeOrigin: true,
  ws,
  configure(proxy) {
    proxy.on("proxyReq", (proxyReq, req) => {
      const authorization = req.headers.authorization;
      if (!authorization) return;
      // The public nginx edge performs this copy itself so Ferrite managed-auth
      // cookies and Buzz's inner NIP-98 credential stay separate. Rewriting it
      // here as well makes nginx overwrite the already-scoped header with an
      // empty Authorization value, leaving local Vite sessions stuck on 401.
      if (RELAY_VIA_EDGE) return;
      proxyReq.setHeader("x-buzz-authorization", authorization);
      proxyReq.removeHeader("authorization");
    });
  },
});

/**
 * `^/buzz/$` is a regex key on purpose: the relay's WebSocket route is the bare
 * `/buzz/` (see `relayWsUrl`), and a plain string key would prefix-match and
 * swallow `/buzz/ui/` — the bundle this server is here to serve.
 */
const RELAY_PROXY = {
  "/buzz/api": relayProxy(),
  "/buzz/sub": relayProxy(),
  "/buzz/blob": relayProxy(),
  "^/buzz/$": relayProxy(true),
};

export default defineConfig({
  base: "/buzz/ui/",
  // `tauriWebAlias` resolves `@tauri-apps/*` onto the facades in
  // `src/shared/tauri-web/`, which re-export the browser stand-ins from
  // `src/shared/lib/desktop-bridge.ts`. It is a plugin rather than a
  // `resolve.alias` entry because an alias can only map specifiers it has been
  // told about: a Tauri module NOBODY has ported must still resolve — to a
  // generated module that throws `TauriModuleUnavailableError` naming the
  // capability at the call site — and it must be reported at build time. See
  // `scripts/tauri-web-alias.ts` and `src/shared/tauri-web/README.md`.
  // `BUZZ_TAURI_STRICT=1` makes it fatal.
  //
  // `emittedFileCeiling` keeps every file of the relay build under 3.5 MiB and
  // pure UTF-8: the relay serves the bundle from its api route, whose reply
  // carries at most 4 MiB. See `scripts/emitted-file-ceiling.ts`. The edge
  // build is not served that way.
  plugins: [{
    name: "buzz-relay-transport-receipt",
    transformIndexHtml() {
      return [{ tag: "meta", attrs: { name: "buzz-relay-transport", content: process.env.VITE_RELAY_TRANSPORT === "http" ? "http" : "websocket" }, injectTo: "head" }];
    },
  }, tauriWebAlias(), react(), ...(EDGE_BUNDLE ? [] : [emittedFileCeiling()])],
  resolve: {
    alias: [
      // WebP stand-ins for upstream's largest PNGs, which the relay build inlines as base64 (see
      // `assetsInlineLimit` below): the card textures (lossless, so the dither is exact) sit in the
      // render-blocking stylesheet, and the starter-team APNGs (animated WebP, 40 frames at 100 ms)
      // were 1.1 MB each. 8.2 MB of PNG becomes 2.5 MB with upstream's sources untouched;
      // `labs-assets/README.md` says how each file was made. A regex matches the specifier, so
      // the CSS `url("./assets/…")` and the `?inline` imports are both caught.
      ...WEBP_STAND_INS,
      ...Object.entries({
      "@": "/src",
      // buzz keeps its preview-feature flags at the repo root; the port vendors
      // the file so the bundle has the same feature set.
      "@features-manifest": "/preview-features.json",
      // upstream's OSS build resolves these two to the public (empty) protected
      // feature modules; the internal variants ship only in their private build.
      "@protected-features": "/src/protectedFeatures/public.ts",
      "@protected-feature-components": "/src/protectedFeatures/publicUi.ts",
      // upstream keeps this at <repo>/scripts/; vendored beside
      // preview-features.json for the same reason.
      "@model-capabilities-manifest": "/model-capabilities.json",
      }).map(([find, replacement]) => ({ find, replacement })),
    ],
  },
  server: {
    port: parseInt(process.env.VITE_PORT || "5173", 10),
    strictPort: true,
    headers: { "Service-Worker-Allowed": "/" },
    proxy: RELAY_PROXY,
  },
  preview: {
    // `vite preview` serves the built bundle against a real relay; without the
    // same proxy the tab is cross-origin and every relay call dies on CORS.
    port: parseInt(process.env.VITE_PREVIEW_PORT || "4173", 10),
    strictPort: true,
    headers: { "Service-Worker-Allowed": "/" },
    proxy: RELAY_PROXY,
  },
  build: {
    outDir: EDGE_BUNDLE ? "dist-edge" : "dist",
    sourcemap: false,
    // This SPA has one HTML entry but a very broad lazy route graph. Rolldown's
    // default split produced more than 500 tiny files. The relay build
    // coalesces its initial graph while preserving lazy routes and capping its
    // chunks at 3 MiB, under the 3.5 MiB file ceiling `emittedFileCeiling`
    // enforces (an api reply carries at most 4 MiB; at the former 6 MiB cap
    // the initial chunk reached 5.2 MiB and answered 504). The edge build deliberately
    // emits one script to avoid repeated managed-auth checks; never send that
    // monolith through the relay uploader.
    rolldownOptions: {
      output: {
        strictExecutionOrder: true,
        // The edge keeps every asset behind managed auth. A split startup
        // graph would therefore pay a native session-validation round trip for
        // every chunk, and concurrent checks can exhaust the reply waiters.
        // Emit one edge script; the relay bundle keeps its lazy route chunks
        // because it has a per-file blob ceiling and no outer asset auth.
        codeSplitting: EDGE_BUNDLE
          ? false
          : {
              groups: [
                {
                  name: "buzz",
                  tags: ["$initial"],
                  minSize: 512 * 1024,
                  maxSize: 3 * 1024 * 1024,
                },
              ],
            },
      },
    },
    // Inline EVERY asset (the Inter woff2 faces, the card texture, the icons,
    // the notification sounds, the starter-team APNGs) as a data URI so `dist/`
    // is pure text. The relay embeds this bundle with `include_bytes!` and ships
    // it through `deploy_source`, whose file map is JSON string-valued — a
    // binary cannot survive that hop, and base64-ing it separately would desync
    // the filename `include_bytes!` expects. Inlining keeps upstream's
    // typography and artwork without a binary in the payload.
    //
    // This ceiling must stay above the LARGEST asset in the graph, or that one
    // file silently lands in `dist/assets/` as bytes and breaks the deploy. At
    // 1 MiB it did: `shared/ui/assets/card-texture.png` is 1.13 MiB and the
    // starter-team APNGs are 1.13 MiB each. Verify with
    // `find dist -type f -exec file --mime-encoding {} + | grep binary`, which
    // must print nothing; `emittedFileCeiling` also fails the relay build on a
    // non-UTF-8 file. Inlined assets make the stylesheets large (the card
    // textures alone are 4.4 MiB of base64), which is why that plugin splits an
    // oversized stylesheet instead of this limit being lowered.
    assetsInlineLimit: 4 * 1024 * 1024,
  },
});
