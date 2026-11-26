/**
 * The named error a `@tauri-apps/*` module with no facade in this directory
 * throws when it is actually used.
 *
 * The generated stand-in module is minted by the `tauri-web-alias` plugin in
 * `vite.config.ts`; it imports this file so the error type is a real, single,
 * catchable class rather than a string literal buried in generated source.
 *
 * The distinction from `DesktopUnavailableError` (in
 * `shared/lib/desktop-bridge.ts`) is worth keeping straight:
 *
 * - `DesktopUnavailableError` — a capability that was PORTED and cannot exist
 *   in a tab (dragging an OS window, self-updating). Its message is written for
 *   the end user, because buzz renders a failed action's message in the UI.
 * - `TauriModuleUnavailableError` — a capability nobody has looked at yet. Its
 *   message is written for whoever runs the next upstream sync.
 */
export class TauriModuleUnavailableError extends Error {
  /** The Tauri module, without the `@tauri-apps/` prefix, e.g. `plugin-fs`. */
  readonly module: string;

  /** The symbol the call site asked for, e.g. `readFile`. */
  readonly symbol: string;

  constructor(moduleName: string, symbol: string) {
    super(
      `@tauri-apps/${moduleName}.${symbol} is not implemented in buzz-web. ` +
        `Implement it in src/shared/lib/desktop-bridge.ts and re-export it from ` +
        `src/shared/tauri-web/${moduleName}.ts (see that directory's README).`,
    );
    this.name = "TauriModuleUnavailableError";
    this.module = moduleName;
    this.symbol = symbol;
  }
}

/**
 * Mint the binding a generated stand-in module exports for one symbol.
 *
 * It is a function rather than a throwing getter so the error carries a stack
 * that points at the CALL, which is the whole reason a resolved-but-throwing
 * alias beats an unresolved import: the report names the capability and the
 * line that wanted it.
 */
export function unavailableTauriBinding(
  moduleName: string,
  symbol: string,
): (...args: never[]) => never {
  return function tauriModuleUnavailable(): never {
    throw new TauriModuleUnavailableError(moduleName, symbol);
  };
}

/**
 * The namespace object a generated stand-in module default-exports, for a call
 * site that reached the module some way the `src/` scan could not see (a
 * computed dynamic `import()`, say). Any property read throws.
 */
export function unavailableTauriNamespace(moduleName: string): object {
  return new Proxy(Object.create(null) as object, {
    get(_target, property) {
      // Let the module system probe the object without detonating: these are
      // asked for by `await import(...)`, JSON serialisation and devtools, not
      // by buzz.
      if (typeof property === "symbol") return undefined;
      if (property === "then" || property === "__esModule") return undefined;
      throw new TauriModuleUnavailableError(moduleName, String(property));
    },
  });
}
