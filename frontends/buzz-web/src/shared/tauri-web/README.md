# `shared/tauri-web` — the `@tauri-apps/*` alias target

buzz's client was written against a Tauri shell. This build runs in a plain
browser tab. `shared/lib/desktop-bridge.ts` is the single hand-written stand-in
for that shell; **this directory is the address book that lets upstream's own
import lines find it**.

Every module here is a thin re-export facade shaped like the real Tauri module
of the same name. `@tauri-apps/api/core` → `tauri-web/api/core.ts`,
`@tauri-apps/plugin-opener` → `tauri-web/plugin-opener.ts`, and so on. The
mapping is done twice, once per toolchain, and both are pure path rules with no
per-file list to maintain:

| toolchain  | where                                    | rule                                                 |
| ---------- | ---------------------------------------- | ---------------------------------------------------- |
| TypeScript | `tsconfig.json` `compilerOptions.paths`  | `@tauri-apps/api/*` → `./src/shared/tauri-web/api/*`  |
| Vite       | the `tauri-web-alias` plugin in `vite.config.ts` | same, resolved on disk                        |

**Why this exists at all.** Without it, every call site in upstream's tree has
to be edited to say `@/shared/lib/desktop-bridge` — 72 files on 2026-10-01, and each
edited file is a permanent merge conflict against a repo that lands hundreds of
commits a month. With the alias they are byte-identical to upstream and cost
nothing to re-sync. See `UPSTREAM-PIN.md` and `scripts/sync-upstream.mjs` at
the `buzz-web` package root.

## The loud-failure property

The old `vite.config.ts` had **no** alias on purpose, so "a stray `@tauri-apps`
import fails loudly at build time instead of being quietly resolved." That
property is kept, and sharpened, at three levels:

1. **A module we implement, a symbol we do not** → `tsc` error naming the
   symbol: `Module '"@tauri-apps/api/window"' has no exported member
   'getAllWindows'`. The facades re-export an explicit list, never `export *`,
   precisely so this stays true.
2. **A module we do not implement at all** (upstream starts importing
   `@tauri-apps/plugin-fs`) → the Vite plugin resolves it to a generated module
   whose every export throws `TauriModuleUnavailableError` naming both the
   module and the symbol, **at the call site**, e.g.
   `@tauri-apps/plugin-fs.readFile is not implemented in buzz-web`. TypeScript
   accepts it (via the shorthand ambient wildcard in `tauri-apps.d.ts`) so the
   rest of the sync is not blocked by one unported capability.
3. **The build says so out loud anyway.** The plugin scans `src/` on every
   `buildStart` and prints a `[tauri-web]` warning per unimplemented module,
   listing the symbols and the importing files. Setting `BUZZ_TAURI_STRICT=1`
   turns that warning into a hard build error — that is the CI knob that
   restores the original all-or-nothing behaviour when you want it.

So the failure is louder than before: it names the capability, the symbol and
the file, instead of saying "failed to resolve import".

## Adding a capability

1. Implement it in `shared/lib/desktop-bridge.ts` (that file, and only that
   file, is allowed to know how the browser does it).
2. Re-export it from the facade here whose name matches the Tauri module.
3. Run `npm run build`; the `[tauri-web]` warning for that module disappears.

Never implement browser behaviour in this directory. These files are address
book entries, not code.
