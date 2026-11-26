import * as React from "react";

import { useChannelNavigation } from "@/shared/context/ChannelNavigationContext";
import { detectPrefixQuery } from "@/shared/lib/detectPrefixQuery";
import type { Channel } from "@/shared/api/types";
import type { AutocompleteEdit } from "./useRichTextEditor";

export type ChannelSuggestion = {
  id: string;
  name: string;
  channelType: "stream" | "forum";
};

const CHANNEL_QUERY_DEBOUNCE_MS = 120;

/**
 * Archived channels must stay resolvable in historical links (rendered from
 * the unfiltered ChannelNavigationContext list), but are dead ends for new
 * `#channel` references — exclude them here, at generation time, rather than
 * from the shared channel list.
 */
function isChannelSuggestable(
  channel: Pick<Channel, "channelType" | "archivedAt">,
): boolean {
  return channel.channelType !== "dm" && channel.archivedAt === null;
}

/** Exported for unit testing. */
export function selectChannelSuggestions(
  channels: Channel[],
  query: string,
): ChannelSuggestion[] {
  const lowerQuery = query.toLowerCase();
  return channels
    .filter(
      (ch) =>
        isChannelSuggestable(ch) && ch.name.toLowerCase().includes(lowerQuery),
    )
    .slice(0, 8)
    .map((ch) => ({
      id: ch.id,
      name: ch.name,
      channelType: ch.channelType as "stream" | "forum",
    }));
}

/**
 * What Tab/Enter selects, given whether a debounced query update is still
 * pending. With one pending the shown suggestions may be stale, so the query
 * is detected afresh from the live text and cursor: no `#` query there means
 * nothing is selected (Enter goes on to send), and otherwise the fresh top
 * match and its fresh start offset are used.
 */
export function resolveChannelSelection(opts: {
  pending: boolean;
  value: string;
  cursor: number;
  knownNamesLower: string[];
  channels: Channel[];
  suggestions: readonly ChannelSuggestion[];
  selectedIndex: number;
  startIndex: number;
}): { suggestion: ChannelSuggestion; startIndex: number } | null {
  if (!opts.pending) {
    const suggestion = opts.suggestions[opts.selectedIndex];
    return suggestion ? { suggestion, startIndex: opts.startIndex } : null;
  }
  const fresh = detectPrefixQuery(
    "#",
    opts.value,
    opts.cursor,
    opts.knownNamesLower,
  );
  if (!fresh) return null;
  const [suggestion] = selectChannelSuggestions(opts.channels, fresh.query);
  return suggestion ? { suggestion, startIndex: fresh.startIndex } : null;
}

export function useChannelLinks() {
  const { channels } = useChannelNavigation();

  const [channelQuery, setChannelQuery] = React.useState<string | null>(null);
  // A ref, not state: insertChannel can run in the same event as a flush
  // that moved the start offset, before any re-render.
  const channelStartIndexRef = React.useRef(0);
  const setChannelStartIndex = React.useCallback((index: number) => {
    channelStartIndexRef.current = index;
  }, []);
  const [channelSelectedIndex, setChannelSelectedIndex] = React.useState(0);

  const debounceTimerRef = React.useRef<ReturnType<typeof setTimeout> | null>(
    null,
  );
  const latestValueRef = React.useRef<string>("");
  const latestCursorRef = React.useRef<number>(0);

  /** Channel names (original casing) for overlay highlighting. */
  const knownChannelNames = React.useMemo<string[]>(
    () => channels.filter(isChannelSuggestable).map((ch) => ch.name),
    [channels],
  );

  /** Lower-cased channel names for case-insensitive prefix matching. */
  const knownNamesLower = React.useMemo<string[]>(
    () => knownChannelNames.map((n) => n.toLowerCase()),
    [knownChannelNames],
  );

  const knownNamesLowerRef = React.useRef<string[]>(knownNamesLower);

  // Keep the known-names ref in sync so the debounced callback never reads stale data.
  React.useEffect(() => {
    knownNamesLowerRef.current = knownNamesLower;
  }, [knownNamesLower]);

  React.useEffect(() => {
    return () => {
      if (debounceTimerRef.current !== null) {
        clearTimeout(debounceTimerRef.current);
      }
    };
  }, []);

  const channelSuggestions = React.useMemo<ChannelSuggestion[]>(() => {
    if (channelQuery === null) {
      return [];
    }
    return selectChannelSuggestions(channels, channelQuery);
  }, [channels, channelQuery]);

  const isChannelOpen = channelQuery !== null && channelSuggestions.length > 0;

  const insertChannel = React.useCallback(
    (suggestion: ChannelSuggestion, selectionEnd: number): AutocompleteEdit => {
      if (debounceTimerRef.current !== null) {
        clearTimeout(debounceTimerRef.current);
        debounceTimerRef.current = null;
      }

      const insertText = `#${suggestion.name} `;

      setChannelQuery(null);
      setChannelSelectedIndex(0);

      return {
        replaceFromOffset: channelStartIndexRef.current,
        replaceToOffset: selectionEnd,
        insertText,
      };
    },
    [],
  );

  const updateChannelQuery = React.useCallback(
    (value: string, cursorPosition: number) => {
      // Store latest values so the debounced callback always uses fresh data
      latestValueRef.current = value;
      latestCursorRef.current = cursorPosition;

      if (debounceTimerRef.current !== null) {
        clearTimeout(debounceTimerRef.current);
      }

      debounceTimerRef.current = setTimeout(() => {
        debounceTimerRef.current = null;
        const channel = detectPrefixQuery(
          "#",
          latestValueRef.current,
          latestCursorRef.current,
          knownNamesLowerRef.current,
        );
        if (channel) {
          setChannelQuery(channel.query);
          setChannelStartIndex(channel.startIndex);
          setChannelSelectedIndex(0);
        } else {
          setChannelQuery(null);
        }
      }, CHANNEL_QUERY_DEBOUNCE_MS);
    },
    [setChannelStartIndex],
  );

  const clearChannels = React.useCallback(() => {
    if (debounceTimerRef.current !== null) {
      clearTimeout(debounceTimerRef.current);
      debounceTimerRef.current = null;
    }
    setChannelQuery(null);
    setChannelSelectedIndex(0);
  }, []);

  const handleChannelKeyDown = React.useCallback(
    (
      event: React.KeyboardEvent,
    ): { handled: boolean; suggestion?: ChannelSuggestion } => {
      if (!isChannelOpen) {
        return { handled: false };
      }

      if (event.key === "ArrowDown") {
        event.preventDefault();
        setChannelSelectedIndex((current) =>
          current < channelSuggestions.length - 1 ? current + 1 : 0,
        );
        return { handled: true };
      }

      if (event.key === "ArrowUp") {
        event.preventDefault();
        setChannelSelectedIndex((current) =>
          current > 0 ? current - 1 : channelSuggestions.length - 1,
        );
        return { handled: true };
      }

      // Forward Tab selects; Shift+Tab deliberately does not. The reverse
      // move stays the browser's, so this overlay can't swallow a keyboard
      // user's way back out (see useMentions for the same split).
      if (
        (event.key === "Tab" && !event.shiftKey) ||
        (event.key === "Enter" &&
          !event.ctrlKey &&
          !event.metaKey &&
          !event.altKey &&
          !event.shiftKey)
      ) {
        const pending = debounceTimerRef.current !== null;
        const selection = resolveChannelSelection({
          pending,
          value: latestValueRef.current,
          cursor: latestCursorRef.current,
          knownNamesLower: knownNamesLowerRef.current,
          channels,
          suggestions: channelSuggestions,
          selectedIndex: channelSelectedIndex,
          startIndex: channelStartIndexRef.current,
        });
        if (pending) {
          clearTimeout(debounceTimerRef.current as ReturnType<typeof setTimeout>);
          debounceTimerRef.current = null;
        }
        if (!selection) {
          // The text no longer holds a `#` query: close the menu and let
          // the key do what it would have done.
          clearChannels();
          return { handled: false };
        }
        event.preventDefault();
        setChannelStartIndex(selection.startIndex);
        return { handled: true, suggestion: selection.suggestion };
      }

      if (event.key === "Escape") {
        event.preventDefault();
        // clearChannels also cancels a pending debounce, which would
        // otherwise reopen the menu just dismissed.
        clearChannels();
        return { handled: true };
      }

      return { handled: false };
    },
    [
      isChannelOpen,
      channelSelectedIndex,
      channelSuggestions,
      channels,
      clearChannels,
      setChannelStartIndex,
    ],
  );

  return {
    channels,
    channelQuery,
    channelSelectedIndex,
    channelSuggestions,
    clearChannels,
    handleChannelKeyDown,
    insertChannel,
    isChannelOpen,
    knownChannelNames,
    updateChannelQuery,
  };
}

export type UseChannelLinksResult = ReturnType<typeof useChannelLinks>;
