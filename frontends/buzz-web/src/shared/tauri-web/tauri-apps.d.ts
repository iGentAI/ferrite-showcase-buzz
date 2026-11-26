/**
 * The last-resort declaration for a `@tauri-apps/*` module this build does not
 * implement.
 *
 * TypeScript consults pattern ambient modules only AFTER path mapping and node
 * resolution have both failed, so this never shadows a facade that exists:
 * `@tauri-apps/api/core` still resolves to `tauri-web/api/core.ts` and still
 * reports a wrong symbol as `has no exported member`. It applies only to a
 * module with no facade — the case where upstream has started using a Tauri
 * plugin nobody has ported yet.
 *
 * A shorthand declaration (no body) makes every import from the module `any`.
 * That is deliberate: the failure for an unported plugin is a NAMED RUNTIME
 * ERROR at the call site (`TauriModuleUnavailableError`, minted by the
 * `tauri-web-alias` Vite plugin), not a type error that blocks the whole sync.
 * The build still says so out loud — see the `[tauri-web]` warnings, and
 * `BUZZ_TAURI_STRICT=1` to make them fatal.
 */
declare module "@tauri-apps/*";
