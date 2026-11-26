import { useCallback, useEffect, useRef } from "react";

import { cancelBuilderlabLogin } from "./hostedCommunityApi";

export type BuilderlabLoginAttempt = {
  /** False once a newer attempt started, the flow was abandoned or unmounted. */
  isCurrent: () => boolean;
  /** Mark this attempt settled so unmount no longer cancels it. */
  finish: () => void;
};

/**
 * Tracks the Builderlab browser sign-in started by a component. The native
 * side of `startBuilderlabLogin()` keeps a loopback callback listener open
 * until the browser redirects; if the component unmounts first (a stage swap
 * or navigation away), that listener must be cancelled and any late result
 * discarded, or a completion nobody is watching mutates backend auth state.
 */
export function useBuilderlabLoginAttempts(
  cancel: () => Promise<unknown> = cancelBuilderlabLogin,
) {
  const attemptRef = useRef(0);
  const inFlightRef = useRef(false);
  const cancelRef = useRef(cancel);
  cancelRef.current = cancel;

  useEffect(
    () => () => {
      attemptRef.current += 1;
      if (inFlightRef.current) {
        inFlightRef.current = false;
        void cancelRef.current().catch(() => {
          // The browser sign-in is already detached from this flow;
          // cancellation is best-effort cleanup for the native callback.
        });
      }
    },
    [],
  );

  const begin = useCallback((): BuilderlabLoginAttempt => {
    const attempt = ++attemptRef.current;
    inFlightRef.current = true;
    return {
      isCurrent: () => attemptRef.current === attempt,
      finish: () => {
        if (attemptRef.current === attempt) inFlightRef.current = false;
      },
    };
  }, []);

  /** Invalidate the current attempt (explicit close); the caller cancels. */
  const abandon = useCallback(() => {
    attemptRef.current += 1;
    inFlightRef.current = false;
  }, []);

  return { begin, abandon };
}
