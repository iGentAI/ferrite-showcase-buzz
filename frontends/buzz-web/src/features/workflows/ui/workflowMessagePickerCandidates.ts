import * as React from "react";

import type { RelayEvent } from "@/shared/api/types";
import {
  mergeMessageCandidateSources,
  type WorkflowMessageCandidate,
  validatedWorkflowMessageCandidate,
  validateWorkflowMessageSearchResults,
} from "./workflowMessageCandidates";

/**
 * Where a pasted message ID stands.
 *
 * - `none`: nothing was pasted.
 * - `checking`: the exact lookup has not answered yet.
 * - `verified`: the relay returned that event, in this channel.
 * - `not_in_channel`: the relay returned an event, but not a pickable message
 *   in this channel.
 * - `unverified`: the lookup failed, so the ID could not be confirmed.
 */
export type DirectMessageLookup =
  | "none"
  | "checking"
  | "verified"
  | "not_in_channel"
  | "unverified";

export function directMessageLookupState({
  directId,
  exactCandidate,
  exactEvent,
  exactFetching,
  exactFailed,
}: {
  directId: string | null;
  exactCandidate: WorkflowMessageCandidate | null;
  exactEvent: RelayEvent | null | undefined;
  exactFetching: boolean;
  exactFailed: boolean;
}): DirectMessageLookup {
  if (!directId) return "none";
  if (exactCandidate?.id === directId) return "verified";
  if (exactEvent) return "not_in_channel";
  if (exactFetching) return "checking";
  return exactFailed ? "unverified" : "checking";
}

/**
 * Keep the previous array while its elements are the same values, so a list
 * rebuilt on every render (such as `useQueries` results) can key a memo.
 */
function useShallowStableArray<T>(values: readonly T[]): readonly T[] {
  const ref = React.useRef(values);
  const previous = ref.current;
  const same =
    previous.length === values.length &&
    previous.every((value, index) => Object.is(value, values[index]));
  if (!same) ref.current = values;
  return same ? previous : values;
}

/**
 * The message picker's candidate list, and which rows may be committed.
 *
 * Every derived list is memoized on its real inputs, so an unrelated re-render
 * returns the same arrays: the picker's scroll-into-view effect is keyed on
 * `visibleCandidates`, and fresh arrays on every render re-ran it constantly.
 *
 * Only a candidate the relay has confirmed as a message in this channel
 * (history, a validated search hit, or a validated exact lookup) may be
 * selected. A pasted ID is shown straight away as a placeholder row, but while
 * its lookup is pending, or after it failed, it is not selectable: absence of
 * a negative answer is not a positive one. The currently selected ID can
 * always be clicked again, because that only clears it.
 */
export function useWorkflowMessagePickerCandidates({
  channelId,
  directId,
  exactEvent,
  historyCandidates,
  lookupId,
  normalizedQuery,
  searchEvents,
  searchHitIds,
  selectedId,
}: {
  channelId: string | null | undefined;
  directId: string | null;
  exactEvent: RelayEvent | null | undefined;
  historyCandidates: readonly WorkflowMessageCandidate[];
  lookupId: string | null;
  normalizedQuery: string;
  searchEvents: readonly (RelayEvent | null | undefined)[];
  searchHitIds: readonly string[];
  selectedId: string | null;
}) {
  const stableSearchEvents = useShallowStableArray(searchEvents);
  const searchCandidates = React.useMemo(
    () =>
      validateWorkflowMessageSearchResults(
        searchHitIds.map((requestedId, index) => ({
          requestedId,
          event: stableSearchEvents[index],
        })),
        channelId ?? "",
      ),
    [channelId, searchHitIds, stableSearchEvents],
  );
  const exactCandidate = React.useMemo(() => {
    if (!channelId || !lookupId) return null;
    return validatedWorkflowMessageCandidate(exactEvent, {
      channelId,
      requestedId: lookupId,
    });
  }, [channelId, exactEvent, lookupId]);
  const allCandidates = React.useMemo(
    () =>
      // Preserve the relay-provided history/search order. Exact lookups and
      // raw-ID fallbacks only fill gaps; selecting a row must not move it.
      mergeMessageCandidateSources([
        historyCandidates,
        searchCandidates,
        exactCandidate ? [exactCandidate] : [],
        selectedId
          ? [{ id: selectedId, pubkey: null, content: null, createdAt: null }]
          : [],
        directId
          ? [{ id: directId, pubkey: null, content: null, createdAt: null }]
          : [],
      ]),
    [directId, exactCandidate, historyCandidates, searchCandidates, selectedId],
  );
  const confirmedIds = React.useMemo(
    () =>
      new Set([
        ...historyCandidates.map(({ id }) => id),
        ...searchCandidates.map(({ id }) => id),
        ...(exactCandidate ? [exactCandidate.id] : []),
      ]),
    [exactCandidate, historyCandidates, searchCandidates],
  );
  const visibleCandidates = React.useMemo(() => {
    if (directId) return allCandidates.filter(({ id }) => id === directId);
    if (!normalizedQuery) return allCandidates;
    return allCandidates.filter(
      (candidate) =>
        candidate.id.includes(normalizedQuery) ||
        candidate.content?.toLowerCase().includes(normalizedQuery),
    );
  }, [allCandidates, directId, normalizedQuery]);
  const canCommit = React.useCallback(
    (candidateId: string) =>
      candidateId === selectedId || confirmedIds.has(candidateId),
    [confirmedIds, selectedId],
  );

  return { allCandidates, canCommit, exactCandidate, visibleCandidates };
}
