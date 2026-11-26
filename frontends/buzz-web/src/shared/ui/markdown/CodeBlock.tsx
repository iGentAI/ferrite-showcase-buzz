import * as React from "react";
import { Copy } from "lucide-react";
import { toast } from "sonner";
import {
  bundledLanguages,
  bundledLanguagesAlias,
  createHighlighter,
  type HighlighterGeneric,
  type BundledLanguage,
  type BundledTheme,
  type ThemedToken,
} from "shiki";

import { useTheme } from "@/shared/theme/ThemeProvider";
import { resolveShikiThemeName } from "@/shared/theme/theme-loader";
import { copyCodeBlockToClipboard } from "@/shared/lib/codeBlockClipboard";
import { cn } from "@/shared/lib/cn";
import { Button } from "@/shared/ui/button";
import { useSmoothCorners } from "@/shared/ui/smoothCorners";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/shared/ui/tooltip";

import { createShikiLanguagePool } from "./shikiLanguagePool";
import { getReactNodeText } from "./utils";

const tokenCache = new Map<string, ThemedToken[][]>();
const MAX_CACHE_ENTRIES = 100;
const MAX_LOADED_LANGUAGES = 30;
const MAX_HIGHLIGHT_LINES = 150;
// Grammar slots are shared by every code block and released when the blocks
// using a language unmount (see ./shikiLanguagePool). Unknown language names
// never take a slot or trigger a rebuild.
const shikiPool = createShikiLanguagePool<
  HighlighterGeneric<BundledLanguage, BundledTheme>
>({
  create: () => createHighlighter({ themes: [], langs: [] }),
  isKnownLanguage: (language) =>
    Object.hasOwn(bundledLanguages, language) ||
    Object.hasOwn(bundledLanguagesAlias, language),
  maxLanguages: MAX_LOADED_LANGUAGES,
});
export const CODE_BLOCK_CLASS =
  "code-block-lines block min-w-full whitespace-pre font-mono text-sm font-medium text-foreground";
/** The most characters a block may hold and still be syntax-highlighted. */
const MAX_HIGHLIGHT_CHARS = 20_000;
const DIFF_ADD_RE = /\s*\/\/\s*\[!code\s*\+\+\]\s*$/;
const DIFF_REMOVE_RE = /\s*\/\/\s*\[!code\s*--\]\s*$/;

export function extractLanguage(className?: string): string {
  if (typeof className !== "string") return "";
  const match = className.match(/language-(\S+)/);
  return match ? match[1] : "";
}

function stripDiffMarker(tokens: ThemedToken[], marker: RegExp): ThemedToken[] {
  const last = tokens[tokens.length - 1];
  if (!last) return tokens;
  const stripped = last.content.replace(marker, "");
  if (stripped === last.content) return tokens;
  if (stripped === "") return tokens.slice(0, -1);
  return [...tokens.slice(0, -1), { ...last, content: stripped }];
}

function getCodeBlockText(children: React.ReactNode) {
  return getReactNodeText(children).replace(/\n$/, "");
}

export function MarkdownCodeBlock({
  children,
  language,
}: {
  children?: React.ReactNode;
  language?: string;
}) {
  const [isCopying, setIsCopying] = React.useState(false);
  const codeBlockRef = React.useRef<HTMLPreElement | null>(null);
  const code = React.useMemo(() => getCodeBlockText(children), [children]);
  useSmoothCorners(codeBlockRef);

  const handleCopy = React.useCallback(
    async (event: React.MouseEvent<HTMLButtonElement>) => {
      event.preventDefault();
      event.stopPropagation();
      setIsCopying(true);

      try {
        await copyCodeBlockToClipboard(code);
        toast.success("Copied code to clipboard");
      } catch (error) {
        console.error("Failed to copy code block", error);
        toast.error("Failed to copy code");
      } finally {
        setIsCopying(false);
      }
    },
    [code],
  );

  return (
    <div className="group relative" data-code-block="">
      <pre
        ref={codeBlockRef}
        className="max-h-[400px] overflow-x-auto overflow-y-auto rounded-2xl border border-border/70 bg-muted/60 px-3 py-1.5 pr-12 shadow-xs"
        style={{ borderRadius: "1rem" }}
      >
        {language && (
          <div className="mb-1 text-xs text-muted-foreground/70">
            {language}
          </div>
        )}
        {children}
      </pre>
      <Tooltip>
        <TooltipTrigger asChild>
          <Button
            aria-label="Copy code block"
            className="absolute right-2 top-2 h-7 w-7 bg-background/80 text-muted-foreground opacity-0 shadow-xs ring-1 ring-border/60 backdrop-blur-sm transition-opacity hover:bg-background hover:text-foreground hover:opacity-100 focus-visible:opacity-100 group-hover:opacity-100 group-focus-within:opacity-100 disabled:opacity-60"
            disabled={isCopying}
            onClick={handleCopy}
            size="icon"
            type="button"
            variant="ghost"
          >
            <Copy className="h-4 w-4" />
            <span className="sr-only">Copy code block</span>
          </Button>
        </TooltipTrigger>
        <TooltipContent>Copy code</TooltipContent>
      </Tooltip>
    </div>
  );
}

export function SyntaxHighlightedCode({
  className,
  code,
  language,
  ...props
}: {
  code: string;
  language: string;
} & React.ComponentProps<"code">) {
  const { themeName } = useTheme();
  // Buzz aliases ("buzz" / "buzz-dark") are not bundled Shiki themes — resolve
  // to the real bundle (github-light / github-dark) before touching Shiki, or
  // it throws and code blocks fall back to plain text.
  const shikiTheme = resolveShikiThemeName(themeName);
  const [loadedKey, setLoadedKey] = React.useState(0);
  // Whether this render lacked the grammar/theme. If another block finishes
  // loading them between render and effect, the effect must still re-render
  // this block, or it stays plain text until some unrelated update.
  const assetsMissingAtRender = React.useRef(false);
  assetsMissingAtRender.current = !shikiPool.has(language, shikiTheme);

  React.useEffect(() => shikiPool.acquire(language), [language]);

  React.useEffect(() => {
    let cancelled = false;
    const refresh = () => {
      if (!cancelled) setLoadedKey((k) => k + 1);
    };
    const loadAssets = () => {
      void shikiPool.ensure(language, shikiTheme).then(
        (ready) => {
          if (ready) refresh();
        },
        () => undefined,
      );
    };
    if (shikiPool.has(language, shikiTheme)) {
      if (assetsMissingAtRender.current) refresh();
      return () => {
        cancelled = true;
      };
    }
    loadAssets();
    // Every slot may be held by mounted blocks; retry when one is released.
    const unsubscribe = shikiPool.subscribe(() => {
      if (!shikiPool.has(language, shikiTheme)) loadAssets();
    });
    return () => {
      cancelled = true;
      unsubscribe();
    };
  }, [language, shikiTheme]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: loadedKey intentionally triggers re-memoization after async asset loading
  const tokens = React.useMemo(() => {
    const shikiHighlighter = shikiPool.current();
    if (!shikiHighlighter || !shikiPool.has(language, shikiTheme)) return null;
    if ((code.match(/\n/g) || []).length > MAX_HIGHLIGHT_LINES) return null;
    // Lines alone do not bound the work: one minified or crafted line can be
    // megabytes of synchronous tokenizing (finding_83004563c9798def).
    if (code.length > MAX_HIGHLIGHT_CHARS) return null;
    const cacheKey = `${language}:${shikiTheme}:${code}`;
    const cached = tokenCache.get(cacheKey);
    if (cached) return cached;
    try {
      const result = shikiHighlighter.codeToTokens(code, {
        lang: language as BundledLanguage,
        theme: shikiTheme as BundledTheme,
      });
      if (tokenCache.size >= MAX_CACHE_ENTRIES) {
        const firstKey = tokenCache.keys().next().value;
        if (firstKey !== undefined) tokenCache.delete(firstKey);
      }
      tokenCache.set(cacheKey, result.tokens);
      return result.tokens;
    } catch {
      return null;
    }
  }, [code, language, shikiTheme, loadedKey]);

  const codeClassName = cn(CODE_BLOCK_CLASS, className);

  if (!tokens) {
    const lines = code.split("\n");
    return (
      <code {...props} className={codeClassName}>
        {lines.map((line, i) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: lines are positional
          <span key={i} data-line="">
            {line}
          </span>
        ))}
      </code>
    );
  }

  return (
    <code {...props} className={codeClassName}>
      {tokens.map((line, lineIdx) => {
        const lineText = line.map((t) => t.content).join("");
        // The marker regexes start with an unanchored `\s*`, quadratic over a
        // long whitespace run; only a line that names the marker is tested.
        const hasMarker = lineText.includes("[!code");
        const isAdd = hasMarker && DIFF_ADD_RE.test(lineText);
        const isRemove = hasMarker && DIFF_REMOVE_RE.test(lineText);
        const diffClass = isAdd
          ? "code-line-diff-add"
          : isRemove
            ? "code-line-diff-remove"
            : undefined;

        const renderedTokens =
          isAdd || isRemove
            ? stripDiffMarker(line, isAdd ? DIFF_ADD_RE : DIFF_REMOVE_RE)
            : line;

        return (
          <span
            // biome-ignore lint/suspicious/noArrayIndexKey: tokens are positional and never reordered
            key={lineIdx}
            data-line=""
            className={diffClass}
          >
            {renderedTokens.map((token, tokenIdx) => (
              <span
                // biome-ignore lint/suspicious/noArrayIndexKey: tokens are positional and never reordered
                key={tokenIdx}
                style={token.color ? { color: token.color } : undefined}
              >
                {token.content}
              </span>
            ))}
          </span>
        );
      })}
    </code>
  );
}
