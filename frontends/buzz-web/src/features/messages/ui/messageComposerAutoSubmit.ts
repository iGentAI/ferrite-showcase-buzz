import * as React from "react";

// Auto-submit a confirmed draft after the draft-persist lifecycle has restored
// it into the editor. Link-preview preparation is promoted by submit itself, so
// it must never hold this trigger in a polling loop.
export function scheduleSettleGatedAutoSubmit({
  submit,
  timers = {
    set: (fn: () => void, ms: number) => window.setTimeout(fn, ms),
    clear: (id: number) => window.clearTimeout(id),
  },
}: {
  submit: () => void;
  timers?: {
    set: (fn: () => void, ms: number) => number;
    clear: (id: number) => void;
  };
}): () => void {
  const timer = timers.set(submit, 0);
  return () => timers.clear(timer);
}

/**
 * Fire a confirmed auto-send for the draft on screen.
 *
 * The composer is not remounted when its channel or thread changes, so the
 * trigger is read whenever it or the draft key changes, not only at mount;
 * the token is single-use, so a trigger fires at most once. A send already
 * scheduled for one draft is cancelled when the composer moves to another,
 * and on unmount, but not merely because the trigger was cleared from the URL.
 */
export function useAutoSubmitDraft({
  autoSubmitDraftKey,
  effectiveDraftKey,
  onAutoSubmitComplete,
  submit,
  consumeToken,
  schedule = scheduleSettleGatedAutoSubmit,
}: {
  autoSubmitDraftKey: string | null | undefined;
  effectiveDraftKey: string | null;
  onAutoSubmitComplete?: () => void;
  submit: () => void;
  consumeToken: (token: string | null | undefined, draftKey: string | null) => boolean;
  schedule?: (options: { submit: () => void }) => () => void;
}): void {
  const latest = React.useRef({ onAutoSubmitComplete, submit, consumeToken, schedule });
  latest.current = { onAutoSubmitComplete, submit, consumeToken, schedule };
  const scheduledRef = React.useRef<{ key: string; cancel: () => void } | null>(
    null,
  );

  React.useEffect(() => {
    const scheduled = scheduledRef.current;
    if (scheduled && scheduled.key !== effectiveDraftKey) {
      scheduled.cancel();
      scheduledRef.current = null;
    }
    const current = latest.current;
    if (effectiveDraftKey === null) return;
    if (!current.consumeToken(autoSubmitDraftKey, effectiveDraftKey)) return;
    // Clear the trigger BEFORE firing so any navigation from the send cannot
    // loop back with the param still present.
    current.onAutoSubmitComplete?.();
    scheduledRef.current?.cancel();
    scheduledRef.current = {
      key: effectiveDraftKey,
      cancel: current.schedule({ submit: () => latest.current.submit() }),
    };
  }, [autoSubmitDraftKey, effectiveDraftKey]);

  React.useEffect(
    () => () => {
      scheduledRef.current?.cancel();
      scheduledRef.current = null;
    },
    [],
  );
}
