import { safeLocalStorage } from "@/shared/lib/safeStorage";
import { deriveCommunityName } from "@/features/communities/communityStorage";
import { normalizeRelayUrl } from "@/features/communities/relayProbe";
import { setLocalStorageItemWithRecovery } from "@/shared/lib/localStorageQuota";
import type { Profile } from "@/shared/api/types";

const STORAGE_KEY = "buzz-community-onboarding-transaction.v1";

export type CommunityOnboardingSource =
  | "first-community"
  | "add-community"
  | "membership-recovery"
  | "deep-link-connect"
  | "deep-link-join";

export type CommunityOnboardingStage =
  | "claiming"
  | "connecting"
  | "profile"
  | "team-intro"
  | "finalizing"
  /**
   * Backend setup is done and the app is mounting directly on the Welcome
   * channel underneath the onboarding screen, which stays up as an opaque
   * curtain until Welcome reports settled (or a safety timeout), then fades.
   */
  | "entering";

export type FirstCommunityPage = "join" | "member" | "owned";

export type CommunityOnboardingTransaction = {
  id: string;
  source: CommunityOnboardingSource;
  /** First-run screen that launched this transaction, restored on cancel. */
  firstCommunityPage?: FirstCommunityPage;
  stage: CommunityOnboardingStage;
  relayUrl: string;
  inviteCode?: string;
  communityName: string;
  token?: string;
  reposDir?: string;
  /**
   * Join-policy acceptance receipt minted before the claim (bound to the
   * invite code). Forwarded to `claimInvite` so relays with a configured
   * join policy admit the claim.
   */
  policyReceipt?: string;
  communityId?: string;
  previousCommunityId?: string;
  addedCommunity?: boolean;
  createdAt: string;
  updatedAt: string;
  error?: string;
  // Deep links are persisted before machine onboarding completes. Set when
  // the user dismisses the acknowledgment so it stays dismissed on relaunch.
  acknowledged?: boolean;
};

export type CommunityOnboardingTransactionPatch = Partial<
  Pick<
    CommunityOnboardingTransaction,
    | "stage"
    | "relayUrl"
    | "communityId"
    | "previousCommunityId"
    | "addedCommunity"
    | "communityName"
    | "error"
    | "acknowledged"
  >
>;

export type StartCommunityOnboardingInput = {
  source: CommunityOnboardingSource;
  firstCommunityPage?: FirstCommunityPage;
  relayUrl: string;
  inviteCode?: string;
  communityName?: string;
  token?: string;
  reposDir?: string;
  policyReceipt?: string;
};

function canonicalRelayUrl(rawRelayUrl: string) {
  // Deep links, bare-code recovery and hosted responses all arrive here, so the
  // hardened normalizer refuses userinfo, fragments and parser differentials
  // before anything is stored or connected to (finding_1fc922b4b70ade9f).
  const withScheme = normalizeRelayUrl(rawRelayUrl);
  if (withScheme === null) throw new Error("That relay address is not a valid relay URL.");
  const parsed = new URL(withScheme);
  parsed.protocol = parsed.protocol.toLowerCase();
  parsed.hostname = parsed.hostname.toLowerCase();
  parsed.pathname = parsed.pathname.replace(/\/+$/, "") || "/";
  return parsed.toString().replace(/\/$/, "");
}

function isTransaction(
  value: unknown,
): value is CommunityOnboardingTransaction {
  if (!value || typeof value !== "object") return false;
  const transaction = value as Partial<CommunityOnboardingTransaction>;
  return (
    typeof transaction.id === "string" &&
    typeof transaction.relayUrl === "string" &&
    typeof transaction.communityName === "string" &&
    typeof transaction.createdAt === "string" &&
    typeof transaction.updatedAt === "string" &&
    [
      "claiming",
      "connecting",
      "profile",
      "team-intro",
      "finalizing",
      "entering",
    ].includes(transaction.stage ?? "")
  );
}

export function loadCommunityOnboardingTransaction(
  storage: Storage = safeLocalStorage(),
): CommunityOnboardingTransaction | null {
  try {
    const raw = storage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const parsed: unknown = JSON.parse(raw);
    return isTransaction(parsed) ? parsed : null;
  } catch {
    return null;
  }
}

export function saveCommunityOnboardingTransaction(
  transaction: CommunityOnboardingTransaction,
  storage: Storage = safeLocalStorage(),
): void {
  // The real localStorage gets quota recovery; any other Storage (a test's,
  // or the in-memory stand-in for a denied origin) is written directly, and a
  // throw never escapes to strand onboarding (finding_8f2ccf6ca76636a6).
  if (storage === localStorageOrNull()) {
    setLocalStorageItemWithRecovery(STORAGE_KEY, JSON.stringify(transaction));
    return;
  }
  try {
    storage.setItem(STORAGE_KEY, JSON.stringify(transaction));
  } catch {
    // Unwritable storage: the transaction lives only in React state.
  }
}

export function clearCommunityOnboardingTransaction(
  storage: Storage = safeLocalStorage(),
): void {
  try {
    storage.removeItem(STORAGE_KEY);
  } catch {
    // Denied storage holds nothing to clear.
  }
}

export function startCommunityOnboarding(
  input: StartCommunityOnboardingInput,
  storage: Storage = safeLocalStorage(),
  now = new Date(),
): CommunityOnboardingTransaction {
  const relayUrl = canonicalRelayUrl(input.relayUrl);
  const existing = loadCommunityOnboardingTransaction(storage);
  if (existing?.relayUrl === relayUrl) {
    const inviteCode = input.inviteCode?.trim() || undefined;
    const inviteChanged =
      inviteCode !== undefined && inviteCode !== existing.inviteCode;
    // Redeeming an invite to recover a denied membership must claim it: the
    // transaction resumes at "claiming" instead of staying where the denial
    // left it (finding_57265269d1bf315c).
    // A new invite that arrives while the transaction is still connecting
    // (no membership established yet) is claimed too: the claim runs only at
    // "claiming", so staying at "connecting" would never redeem it. Past
    // connecting the resume is kept, and a denial is recovered through
    // "membership-recovery".
    const reclaim =
      inviteCode !== undefined &&
      (input.source === "membership-recovery" ||
        (inviteChanged && existing.stage === "connecting"));
    const updated: CommunityOnboardingTransaction = {
      ...existing,
      ...(reclaim ? { stage: "claiming" as const, source: input.source } : {}),
      firstCommunityPage:
        input.firstCommunityPage ?? existing.firstCommunityPage,
      inviteCode: inviteCode ?? existing.inviteCode,
      communityName: input.communityName?.trim() || existing.communityName,
      token: input.token?.trim() || existing.token,
      reposDir: input.reposDir ?? existing.reposDir,
      // A receipt belongs to the invite it was issued with; a new invite
      // never inherits the old one's (finding_a6a869e55e5950ec).
      policyReceipt: inviteChanged
        ? input.policyReceipt
        : (input.policyReceipt ?? existing.policyReceipt),
      updatedAt: now.toISOString(),
      error: undefined,
      // A freshly opened link deserves fresh feedback — re-present the gate
      // even if a previous link for this relay was already dismissed.
      acknowledged: undefined,
    };
    saveCommunityOnboardingTransaction(updated, storage);
    return updated;
  }

  const timestamp = now.toISOString();
  const transaction: CommunityOnboardingTransaction = {
    id: crypto.randomUUID(),
    source: input.source,
    firstCommunityPage: input.firstCommunityPage,
    stage: input.inviteCode?.trim() ? "claiming" : "connecting",
    relayUrl,
    inviteCode: input.inviteCode?.trim() || undefined,
    communityName: input.communityName?.trim() || deriveCommunityName(relayUrl),
    token: input.token?.trim() || undefined,
    reposDir: input.reposDir,
    policyReceipt: input.policyReceipt,
    createdAt: timestamp,
    updatedAt: timestamp,
  };
  saveCommunityOnboardingTransaction(transaction, storage);
  return transaction;
}

export function updateCommunityOnboardingTransaction(
  transaction: CommunityOnboardingTransaction,
  patch: CommunityOnboardingTransactionPatch,
  storage: Storage = safeLocalStorage(),
  now = new Date(),
): CommunityOnboardingTransaction {
  const updated = { ...transaction, ...patch, updatedAt: now.toISOString() };
  saveCommunityOnboardingTransaction(updated, storage);
  return updated;
}

export function updateCurrentCommunityOnboardingTransaction(
  current: CommunityOnboardingTransaction | null,
  patch: CommunityOnboardingTransactionPatch,
  expectedId: string | undefined,
  storage: Storage = safeLocalStorage(),
  now = new Date(),
): CommunityOnboardingTransaction | null {
  if (!current || (expectedId && current.id !== expectedId)) return current;
  return updateCommunityOnboardingTransaction(current, patch, storage, now);
}

export function markCommunityOnboardingComplete(
  pubkey: string,
  relayUrl: string,
  storage: Storage = safeLocalStorage(),
): void {
  try {
    storage.setItem(
      `buzz-community-onboarding-complete.v1:${encodeURIComponent(relayUrl)}:${pubkey}`,
      "true",
    );
    // The legacy gate is identity-scoped. Marking it here prevents the old profile
    // flow from reopening after the first community transaction completes.
    storage.setItem(`buzz-onboarding-complete.v1:${pubkey}`, "true");
  } catch {
    // A full or denied store must not strand the person at the last step.
  }
}

/**
 * Returns true when a relay-profile check result means the user should skip
 * community onboarding entirely and land directly in the app.
 *
 * A profile fetch error is represented as `null` and always returns false so
 * that the fallback (show the profile step) applies — the skip must never
 * block or strand onboarding.
 */
export function shouldSkipCommunityOnboarding(
  profile: Profile | null,
): boolean {
  return profile !== null && profile.hasProfileEvent === true;
}

/**
 * Outcome of a profile-check attempt during the connecting → profile
 * transition. Produced by `resolveProfileCheckAction`.
 *
 * - `{ action: "skip", profile }` — kind:0 exists; mark complete and enter
 *   the app. The resolved `Profile` is included so callers have the pubkey
 *   for `markCommunityOnboardingComplete` without a second fetch.
 * - `{ action: "show-profile" }` — no kind:0, or the fetch failed / timed
 *   out; show the profile setup step.
 */
export type ProfileCheckAction =
  | { action: "skip"; profile: Profile }
  | { action: "show-profile" };

/**
 * Returns true when a live transaction snapshot still represents the
 * same connecting request that launched the profile check.
 *
 * Extracted as a pure predicate so the stale-result guard in App.tsx can
 * be unit-tested without mounting a component.
 */
export function isTransactionStillConnecting(
  live: CommunityOnboardingTransaction | null | undefined,
  transactionId: string,
): boolean {
  return live?.id === transactionId && live.stage === "connecting";
}

/**
 * Runs a bounded profile fetch and returns the action to take at the
 * `connecting → profile` transition.
 *
 * Accepts `fetchProfile`, `timeoutMs`, and `scheduleTimeout` as parameters so
 * callers (and tests) can supply controlled implementations. `scheduleTimeout`
 * must return a cancellation handle (like `window.setTimeout`) so the timer
 * can be cleared when the fetch settles before the deadline.
 *
 * Any fetch error or timeout → `{ action: "show-profile" }` (never strands
 * onboarding).
 */
export async function resolveProfileCheckAction(
  fetchProfile: () => Promise<Profile>,
  timeoutMs: number,
  scheduleTimeout: (
    fn: () => void,
    ms: number,
  ) => ReturnType<typeof setTimeout> = (fn, ms) => window.setTimeout(fn, ms),
): Promise<ProfileCheckAction> {
  let timerId: ReturnType<typeof setTimeout> | undefined;
  try {
    const profile = await Promise.race([
      fetchProfile(),
      new Promise<never>(
        (_, reject) =>
          (timerId = scheduleTimeout(
            () => reject(new Error("profile-check-timeout")),
            timeoutMs,
          )),
      ),
    ]);
    return shouldSkipCommunityOnboarding(profile)
      ? { action: "skip", profile }
      : { action: "show-profile" };
  } catch {
    return { action: "show-profile" };
  } finally {
    if (timerId !== undefined) clearTimeout(timerId);
  }
}

import * as React from "react";

type CommunityOnboardingContextValue = {
  transaction: CommunityOnboardingTransaction | null;
  start: (input: StartCommunityOnboardingInput) => boolean;
  update: (
    patch: CommunityOnboardingTransactionPatch,
    expectedId?: string,
  ) => void;
  clear: () => void;
};

const CommunityOnboardingContext =
  React.createContext<CommunityOnboardingContextValue | null>(null);

export function CommunityOnboardingProvider({
  children,
  enabled = true,
}: {
  children: React.ReactNode;
  enabled?: boolean;
}) {
  const [transaction, setTransaction] = React.useState(() =>
    enabled ? loadCommunityOnboardingTransaction() : null,
  );
  const start = React.useCallback(
    (input: StartCommunityOnboardingInput) => {
      if (!enabled) return false;
      // Read the persisted transaction, not this render's state: two starts
      // in one turn (back-to-back deep links drained in a loop) both saw
      // null in the closure, and the second overwrote the first.
      // startCommunityOnboarding persists synchronously, so the store is
      // always current.
      const active = loadCommunityOnboardingTransaction();
      if (active && canonicalRelayUrl(input.relayUrl) !== active.relayUrl) {
        return false;
      }
      setTransaction(startCommunityOnboarding(input));
      return true;
    },
    [enabled],
  );
  const update = React.useCallback(
    (patch: CommunityOnboardingTransactionPatch, expectedId?: string) => {
      if (!enabled) return;
      setTransaction((current) =>
        updateCurrentCommunityOnboardingTransaction(current, patch, expectedId),
      );
    },
    [enabled],
  );
  const clear = React.useCallback(() => {
    if (!enabled) return;
    clearCommunityOnboardingTransaction();
    setTransaction(null);
  }, [enabled]);
  const value = React.useMemo(
    () => ({ transaction, start, update, clear }),
    [clear, start, transaction, update],
  );
  return (
    <CommunityOnboardingContext.Provider value={value}>
      {children}
    </CommunityOnboardingContext.Provider>
  );
}

export function useCommunityOnboarding() {
  const context = React.useContext(CommunityOnboardingContext);
  if (!context)
    throw new Error(
      "useCommunityOnboarding must be used within CommunityOnboardingProvider",
    );
  return context;
}

/** `window.localStorage` when the getter does not throw; otherwise null. */
function localStorageOrNull(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}
