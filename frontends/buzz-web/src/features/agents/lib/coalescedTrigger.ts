/**
 * Leading-and-trailing throttle for relay-driven cache invalidation.
 *
 * The persona and team catalog live subscriptions accept events from any
 * community member, and each invalidation re-walks the whole catalog. Calling
 * `invalidateQueries` per event let one member who republishes a replaceable
 * catalog head drive every connected client into back-to-back full refetches.
 *
 * `trigger()` runs `action` at once when it has not run within
 * `minIntervalMs`; otherwise it schedules exactly one trailing run for when
 * the interval ends, and further triggers inside that window coalesce into it.
 * So a burst costs at most one run per interval, and the last event of a burst
 * is never lost.
 */
export type CoalescedTrigger = {
  trigger: () => void;
  dispose: () => void;
};

export type CoalescedTriggerClock = {
  now: () => number;
  setTimer: (callback: () => void, ms: number) => unknown;
  clearTimer: (handle: unknown) => void;
};

const defaultClock: CoalescedTriggerClock = {
  now: () => Date.now(),
  setTimer: (callback, ms) => setTimeout(callback, ms),
  clearTimer: (handle) =>
    clearTimeout(handle as ReturnType<typeof setTimeout>),
};

/** Minimum spacing between relay-driven catalog refetches. */
export const CATALOG_LIVE_INVALIDATION_MIN_INTERVAL_MS = 10_000;

export function createCoalescedTrigger(
  action: () => void,
  minIntervalMs: number,
  clock: CoalescedTriggerClock = defaultClock,
): CoalescedTrigger {
  let lastRunAt = Number.NEGATIVE_INFINITY;
  let pending: unknown = null;
  let disposed = false;

  function run() {
    pending = null;
    if (disposed) return;
    lastRunAt = clock.now();
    action();
  }

  return {
    trigger() {
      if (disposed || pending !== null) return;
      const wait = lastRunAt + minIntervalMs - clock.now();
      if (wait <= 0) {
        run();
        return;
      }
      pending = clock.setTimer(run, wait);
    },
    dispose() {
      disposed = true;
      if (pending !== null) {
        clock.clearTimer(pending);
        pending = null;
      }
    },
  };
}
