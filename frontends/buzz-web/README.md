# buzz-web — buzz's desktop client, served from the browser

This is Buzz's upstream client source, pinned and adapted for a browser-served
Ferrite transport—not a lookalike UI. The exact pin, the number of files it
covers, and the deliberate divergences are recorded in `UPSTREAM-PIN.md`. The
sidebar, channel list, message timeline, composer, threads, reactions, presence,
huddles, agents, forum, projects, settings, component structure, Tailwind
configuration, and styles remain the compatibility target.

## How it runs in a browser

Three seams, and nothing else, differ from the desktop build:

**1. The Tauri API.** buzz's client was written for a Tauri shell. The
upstream files that call it (72 on 2026-10-01) keep their `@tauri-apps/*` import lines
byte-identical to upstream: a path alias — `compilerOptions.paths` in
`tsconfig.json` plus the `tauri-web-alias` plugin in `vite.config.ts` — resolves
each Tauri module onto a same-named facade in
[`src/shared/tauri-web/`](src/shared/tauri-web/README.md), which re-exports the
browser stand-ins from
[`src/shared/lib/desktop-bridge.ts`](src/shared/lib/desktop-bridge.ts), one
module whose exports keep the *same signatures* as the Tauri APIs they replace —
so **no call site changed shape and no JSX, class name or style was touched**,
and an upstream re-sync costs nothing. A stray import still fails loudly rather
than resolving quietly: the facades re-export explicit symbol lists, an
unimplemented module throws a named error at its call site, and
`BUZZ_TAURI_STRICT=1` — set by the `build` script — promotes the per-module
build warning to a hard error. The mechanics are in the
[tauri-web README](src/shared/tauri-web/README.md).

Where a browser can do the job it does it (open a URL, ask for notification
permission, watch the OS theme, reload instead of relaunch, take a screen wake
lock instead of a sleep assertion). Where it genuinely cannot — self-update, OS
window drag, dock badges, huddle audio, local model compute, locally spawned
agent harnesses — the call rejects with a `DesktopUnavailableError` carrying a
sentence written for a human. buzz already renders a failed action's message, so
those features keep rendering and the button explains itself: never a crash,
never a silently dead control, never a faked success.

**2. The transport.** The client does not open its own socket — it asks Rust to,
via `invoke("plugin:websocket|connect", { url, onMessage: Channel })`. So
[`src/shared/browser-backend/`](src/shared/browser-backend/README.md) answers
that command family with a real browser `WebSocket`, emitting the exact frame
shapes the client already parses (`{type:"Text"|"Close"|"Error"}`), plus the
handful of buzz commands a tab can honestly serve (relay config, identity,
signing). Every line of subscription, reconnect, replay and backoff logic above
it is buzz's own, untouched.

The relay it talks to is the `buzz` app's relay module, which serves **NIP-01 +
NIP-42 over the platform's `ws` route** at `/buzz/` (see
`apps/buzz/src/relay/ws.rs`)
— which is precisely why no client rewrite was needed.

**3. The assets.** The desktop shell served artwork, icons and notification
sounds from its own `public/` as root-absolute URLs (`/sounds/ping.mp3`,
`/pow/poof1@3x.png`, `/app-icon@2x.png`, …). Two things break that here: this app
is mounted under `/buzz/ui/`, so a root-absolute URL misses, and the relay ships
`dist/` through `deploy_source`, whose file map is JSON string-valued — a loose
binary cannot make that hop. So those files (46 distinct `?inline` imports on
2026-10-01) are now **module assets**
imported with `?inline`, exactly as upstream already ships its three runtime
logos, and vite inlines them as data URIs. Same bytes (the starter-team avatars
are animated APNGs and are byte-identical), resolved by the bundler rather than by
a hardcoded path, and `dist/` stays pure text.

`build.assetsInlineLimit` must therefore stay **above the largest asset** — at the
old 1 MiB, `card-texture.png` (1.13 MiB) silently landed in `dist/assets/` as
bytes and would have broken the deploy. What the deploy needs is that every file
in `dist/` decodes as UTF-8. Guard:

```bash
find dist -type f -exec sh -c 'iconv -f UTF-8 -t UTF-8 "$1" >/dev/null 2>&1 || echo "$1"' _ {} \;   # must print nothing
```

(The earlier guard, `file --mime-encoding` piped to `grep binary`, now names the
main script chunk: it is valid UTF-8 but carries one literal DEL character in a
key map, which `file` reports as binary.)

Four files live in [`public/`](public) because they must stay real,
fetchable URLs: `buzz.svg` (the `<link rel="icon">`, which vite rewrites to
`/buzz/ui/buzz.svg`), `favicon.svg`, `asset-gate-sw.js` and `worklet.js`
(`AudioWorklet.addModule` does not reliably
accept a data URI; the call site builds its URL from `import.meta.env.BASE_URL`).
All four are text.

### Known cost of inlining

Inlining is correct but not free, and one case is expensive: `App.tsx` imports
`CommunityOnboardingFlow` **statically**, so the three starter-team APNGs
(1.13 MiB each) become a hard dependency of the entry chunk. They land in one
shared 4.20 MiB chunk that `index.html` `modulepreload`s, taking the first-paint
graph from 7.09 MiB to 11.29 MiB. Upstream paid nothing here — the desktop shell
fetched those files lazily, when the onboarding step actually rendered.

This is the constraint leaking, not a UI change: nothing renders differently. The
fix does **not** belong in this app — it belongs in
`../../scripts/embed-buzz-ui.py` (repo-root `scripts/embed-buzz-ui.py`), which is
what forces `dist/` to be text-only in the first place. If that script base64-ed
non-UTF-8 assets into the generated `ui.rs` and the relay decoded them at seed
time (it already has `apps/buzz/src/relay/b64.rs`),
then binaries could make the `deploy_source` hop, `assetsInlineLimit` could drop
back to a normal value, and every asset here would go back to being a separate,
lazily-fetched, correctly-typed, cacheable object. Until then, prefer a correct
UI over a fast one.

`patches/virtua@0.49.3.patch` is buzz's own patch, vendored: their message
timeline depends on it for a function-valued `itemSize`.

## Ferrite Platform Shape

The browser speaks to the relay's explicit API, stream, WebSocket, UI, and Blob
routes. It receives a browser identity/session surface, never deploy authority,
operator secrets, or model-provider credentials. Production assets are embedded
into or uploaded to the constrained relay; the Vite server is local development
only.

## Not Claimed

“Actual client” means a pinned upstream source base behind documented browser
and transport seams, not that every local file remains byte-identical. Desktop-
only Tauri, huddle-media, local-model, and OS integration cannot all operate in
a browser. The client does not prove every UI path, and an older 12/12 run is not
current evidence.

## Validation

The production commands create different deployment artifacts:

- `pnpm build` creates `dist/`, the split, text-only bundle that
  `push-ui.mjs` publishes to the relay.
- `pnpm build:edge` creates `dist-edge/`, the monolithic bundle for an
  authenticated static edge. Don't upload `dist-edge/` to the relay. Its edge
  script can exceed the relay's 8&nbsp;MiB per-file limit.

```bash
pnpm install && pnpm build      # tsc --noEmit, then a strict (BUZZ_TAURI_STRICT=1) vite build -> dist/

# Unit tests: plain node --test over src/**/*.test.mjs, no build required.
node --import ./test-loader.mjs --test "src/**/*.test.mjs"

# The copy still matches the upstream pin (needs a block/buzz checkout;
# $BUZZ_UPSTREAM or --upstream).
npm run sync:check
```

One caveat, observed 2026-07-30: on a pnpm-installed tree the tests whose
subjects are `.tsx` fail to load — the loader compiles JSX with
`rolldown/experimental` (vite 8's own bundler), which pnpm's isolated layout
does not expose at the package root — while the rest of the suite (2,940 of
2,997 tests) passes. An install that makes `rolldown` resolvable from the
package root closes the gap.

On 2026-10-01, after `pnpm install --frozen-lockfile` on Linux with Node 22,
the build passed and the suite reported 7,147 passed, 13 failed and 3 skipped of
7,163. The failures include tests that need upstream native files this copy does
not carry (for example `scripts/build-protected-feature-artifacts.mjs`); the
[upstream sync playbook](../../docs/upstream-sync-playbook.md), entry 004, lists
them.

The Playwright journeys in `e2e/` (`company-day.spec.ts` and the per-feature
specs, plus one-off probe scripts) drive a deployed relay — `BUZZ_ORIGIN`
selects it — and are not a local gate.

`VITE_RELAY_WS_URL` overrides the relay endpoint (default: same origin,
`/buzz/`). Keep `UPSTREAM-PIN.md`, the compatibility seams, build parity, and
the company-day browser spec together when updating upstream.
