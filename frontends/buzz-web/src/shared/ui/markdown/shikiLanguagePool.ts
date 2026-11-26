/**
 * Bounded set of Shiki grammars shared by every rendered code block.
 *
 * The language of a fenced block comes from message content, so any author
 * can ask for any bundled grammar. Grammars are held to `maxLanguages` to
 * bound memory, but a plain "refuse once full" cap never frees a slot: one
 * message with that many fenced blocks in distinct languages would leave every
 * later code block in the session unhighlighted. Shiki has no way to unload a
 * single grammar, so this pool counts which languages mounted blocks are using
 * (`acquire`) and, when a new language needs a slot, rebuilds the highlighter
 * with only the in-use languages and disposes the old one. When every loaded
 * language is in use, the request waits: `ensure` answers `false`, and the
 * pool notifies subscribers when a block releases its language.
 */

export type PooledHighlighter = {
  dispose: () => void;
  loadLanguage: (language: never) => Promise<void>;
  loadTheme: (theme: never) => Promise<void>;
};

export function createShikiLanguagePool<H extends PooledHighlighter>({
  create,
  isKnownLanguage = () => true,
  maxLanguages,
}: {
  create: () => Promise<H>;
  isKnownLanguage?: (language: string) => boolean;
  maxLanguages: number;
}) {
  let highlighter: H | null = null;
  let initializing: Promise<H> | null = null;
  const languages = new Set<string>();
  const themes = new Set<string>();
  const references = new Map<string, number>();
  const listeners = new Set<() => void>();
  let queue: Promise<unknown> = Promise.resolve();

  const serialize = <T>(task: () => Promise<T>): Promise<T> => {
    const run = queue.then(task, task);
    queue = run.catch(() => undefined);
    return run;
  };

  const loadLanguage = (target: H, language: string) =>
    target.loadLanguage(language as never);
  const loadTheme = (target: H, theme: string) =>
    target.loadTheme(theme as never);

  const ensureHighlighter = async (): Promise<H> => {
    if (highlighter) return highlighter;
    initializing ??= create().then(
      (created) => {
        highlighter = created;
        return created;
      },
      (error) => {
        initializing = null;
        throw error;
      },
    );
    return initializing;
  };

  const rebuild = async (keep: string[]) => {
    const next = await create();
    try {
      for (const theme of themes) await loadTheme(next, theme);
      for (const language of keep) await loadLanguage(next, language);
    } catch (error) {
      next.dispose();
      throw error;
    }
    const previous = highlighter;
    highlighter = next;
    initializing = Promise.resolve(next);
    languages.clear();
    for (const language of keep) languages.add(language);
    previous?.dispose();
  };

  const inUse = (language: string) => (references.get(language) ?? 0) > 0;

  /**
   * Make `language` and `theme` available. Resolves `true` once both are
   * loaded, `false` when the grammar or theme is unknown or every slot is
   * held by a mounted block (retry after a subscriber notification).
   */
  const ensure = (language: string, theme: string): Promise<boolean> =>
    serialize(async () => {
      await ensureHighlighter();
      if (!languages.has(language)) {
        if (!isKnownLanguage(language)) return false;
        if (languages.size >= maxLanguages) {
          const keep = [...languages].filter(inUse);
          if (keep.length >= maxLanguages) return false;
          try {
            await rebuild(keep);
          } catch {
            return false;
          }
        }
        const current = highlighter;
        if (!current) return false;
        try {
          await loadLanguage(current, language);
        } catch {
          return false;
        }
        languages.add(language);
      }
      if (!themes.has(theme)) {
        const current = highlighter;
        if (!current) return false;
        try {
          await loadTheme(current, theme);
        } catch {
          return false;
        }
        themes.add(theme);
      }
      return true;
    });

  /** Mark `language` as used by a mounted block; call the result on unmount. */
  const acquire = (language: string): (() => void) => {
    references.set(language, (references.get(language) ?? 0) + 1);
    let released = false;
    return () => {
      if (released) return;
      released = true;
      const remaining = (references.get(language) ?? 1) - 1;
      if (remaining > 0) {
        references.set(language, remaining);
        return;
      }
      references.delete(language);
      if (languages.has(language)) {
        for (const listener of [...listeners]) listener();
      }
    };
  };

  const subscribe = (listener: () => void): (() => void) => {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  };

  return {
    acquire,
    current: () => highlighter,
    ensure,
    has: (language: string, theme: string) =>
      highlighter !== null && languages.has(language) && themes.has(theme),
    loadedLanguages: () => [...languages],
    subscribe,
  };
}

