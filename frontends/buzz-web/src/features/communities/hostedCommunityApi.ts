import { normalizeRelayUrl } from "@/features/communities/relayProbe";
import { invoke } from "@tauri-apps/api/core";

import { safeNpub } from "@/shared/lib/nostrUtils";

export const HOSTED_COMMUNITY_SUFFIX = "communities.buzz.xyz";
export const HOSTED_COMMUNITY_LIMIT = 5;
export const VALID_HOSTED_COMMUNITY_NAME = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;

export type BuilderlabAuth = {
  email?: string;
  name?: string;
  expiresAt: string;
};

export type HostedCommunityApiError = {
  code?: string;
  message?: string;
  setup_needed?: boolean;
};

export type HostedNostrIdentity = {
  npub?: string;
  pubkey_hex?: string;
};

export type HostedIdentityResponse = {
  identity?: HostedNostrIdentity;
  error?: HostedCommunityApiError;
  correlation_id?: string;
};

export type HostedCommunity = {
  id?: string;
  name?: string;
  slug?: string;
  normalized_host?: string;
  owner_pubkey?: string;
  archived_at?: string | null;
};

export type HostedCommunitiesResponse = {
  communities?: HostedCommunity[];
  error?: HostedCommunityApiError;
  correlation_id?: string;
};

export type HostedCommunityAvailabilityResponse = {
  available?: boolean;
  normalized_host?: string;
  error?: HostedCommunityApiError;
  correlation_id?: string;
};

export type HostedCommunityMutationResponse = {
  community?: HostedCommunity;
  error?: HostedCommunityApiError;
  correlation_id?: string;
};

export type HostedCommunityAccount = {
  communities: HostedCommunity[];
  identity: HostedNostrIdentity | null;
};

export function hostedCommunityErrorMessage(
  error: HostedCommunityApiError | undefined,
  correlationId: string | undefined,
  fallback: string,
) {
  const messages: Record<string, string> = {
    missing_mapping: "Connect your Buzz identity before creating a community.",
    invalid_name: "Use lowercase letters, numbers, and hyphens.",
    taken: "That Buzz address is already taken.",
    limit_reached: `You've reached the limit of ${HOSTED_COMMUNITY_LIMIT} hosted communities.`,
    relay_unavailable: "Community provisioning is temporarily unavailable.",
    identity_already_bound:
      "This Builderlab account is connected to another Buzz identity.",
    pubkey_already_bound:
      "This Buzz identity is connected to another Builderlab account.",
    not_owner: "Only the community owner can do that.",
    transferee_not_registered:
      "That person needs a connected Buzz identity before you can transfer ownership to them.",
  };
  const message = messages[error?.code ?? ""] ?? error?.message ?? fallback;
  return correlationId
    ? `${message} Correlation ID: ${correlationId}`
    : message;
}

export function hostedCommunityRelayUrl(community: HostedCommunity) {
  const host = community.normalized_host?.trim();
  if (!host) return null;
  // A host that already names its scheme keeps it. Builderlab always serves
  // `wss://`, but a self-hosted deployment reached over plain http — a local
  // or tunnelled relay — has no `wss://` endpoint to upgrade to, and rewriting
  // the scheme would hand the client a URL that cannot connect. The API field
  // is untrusted: a userinfo-spoofed or malformed host is refused
  // (finding_333522c7eab37a51).
  return normalizeRelayUrl(/^wss?:\/\//.test(host) ? host : `wss://${host}`);
}

const BOUND_KEY_HEX_REGEX = /^[0-9a-f]{64}$/;

/**
 * The ONE operational form of a hosted identity's bound key: the trimmed,
 * lowercased `pubkey_hex` when that field carries a 64-character hex key,
 * null otherwise.
 *
 * `pubkey_hex` is the authoritative field the mismatch gate and every
 * hosted-community operation act on, so its accepted shape is defined here
 * once and is hex-only. An `npub` spelling belongs to the server's separate
 * display field: one stored in the hex field is not a hex key and fails
 * closed here instead of widening the authorization domain beyond what
 * comparisons see. Whitespace-padded or mixed-case hex normalizes to the
 * identical value on both sides of every comparison, so a key that is the
 * same after normalization never reads as a mismatch demanding a
 * delete/rebind of an identity the device already holds. Consumers
 * normalize the local key through this same function before comparing.
 */
export function normalizedBoundKeyHex(
  value: string | null | undefined,
): string | null {
  if (!value) return null;
  const normalized = value.trim().toLowerCase();
  return BOUND_KEY_HEX_REGEX.test(normalized) ? normalized : null;
}

/**
 * The canonical npub of the hosted identity's authoritative bound key, or
 * null when the payload carries no key the app can act on.
 *
 * `pubkey_hex` is the field the mismatch gate and every hosted-community
 * operation act on, and it is only usable when it normalizes to a hex key
 * (`normalizedBoundKeyHex`) — the server-sent `npub` is an independent,
 * unverified spelling and is never accepted in its place. The returned npub
 * is derived from that same normalized key, so the displayed account
 * identity can never disagree with the binding the decisions act on. Hosted
 * surfaces derive their connected/readiness/create/connect gates from the
 * normalized key instead of identity-object presence, so an identity payload
 * without a usable authoritative key (missing, empty, invalid/non-hex,
 * wrong-length, npub-only, or an npub stored in the hex field) fails closed
 * into recovery rather than presenting the account as connected.
 */
export function usableBoundIdentityNpub(
  identity: HostedNostrIdentity | null | undefined,
): string | null {
  const hex = normalizedBoundKeyHex(identity?.pubkey_hex);
  return hex === null ? null : safeNpub(hex);
}

export type HostedIdentityGate = {
  boundHex: string | null;
  localHex: string | null;
  /** The account's authoritative `pubkey_hex` normalizes to a hex key. */
  usableBoundIdentity: boolean;
  /**
   * The account has a binding but this device's own key is not known — the
   * identity query is still loading, failed, or returned a non-key. The
   * ownership decision cannot be made yet, so nothing may proceed.
   */
  localKeyUnknown: boolean;
  /** Both keys are known and differ, or the bound key is unusable. */
  identityMismatch: boolean;
  /**
   * The destructive "use this device" recovery is offered only when the app
   * can name both the binding it would remove and the key it would bind.
   */
  canSwitchToDevice: boolean;
  /** Signed in, bound to a usable key, and that key is this device's key. */
  ready: boolean;
};

/**
 * The one readiness decision for the hosted-community flows. It fails closed:
 * an unknown local key is never treated as a match, so an account bound to a
 * different device's key cannot create or connect a community whose
 * server-side owner is that other key just because the local identity query
 * had not answered (or had failed).
 */
export function evaluateHostedIdentityGate({
  auth,
  identity,
  localPubkey,
}: {
  auth: BuilderlabAuth | null;
  identity: HostedNostrIdentity | null;
  localPubkey: string | null | undefined;
}): HostedIdentityGate {
  const boundHex = normalizedBoundKeyHex(identity?.pubkey_hex);
  const localHex = normalizedBoundKeyHex(localPubkey);
  const usableBoundIdentity = boundHex !== null;
  const localKeyUnknown = identity !== null && localHex === null;
  const identityMismatch = Boolean(
    identity &&
      (!usableBoundIdentity || (localHex !== null && boundHex !== localHex)),
  );
  return {
    boundHex,
    localHex,
    usableBoundIdentity,
    localKeyUnknown,
    identityMismatch,
    canSwitchToDevice:
      identityMismatch && usableBoundIdentity && localHex !== null,
    ready: Boolean(
      auth &&
        usableBoundIdentity &&
        localHex !== null &&
        boundHex === localHex,
    ),
  };
}

export type SwitchHostedIdentityOutcome =
  /** This device's key is now the account's binding. */
  | { kind: "switched"; identity: HostedNostrIdentity | null }
  /** Refused before any call: the bound or local key is not verified. */
  | { kind: "refused" }
  /** A call failed before anything changed; the old binding is intact. */
  | {
      kind: "unchanged";
      error: HostedCommunityApiError;
      correlationId?: string;
    }
  /**
   * The old binding was removed but binding this device then failed. The
   * account is left with no identity; only the device holding the previous
   * key (or this device, on a successful retry) can bind it again.
   */
  | {
      kind: "unbound";
      error: HostedCommunityApiError;
      correlationId?: string;
    }
  /**
   * A call after the pre-check never answered (transport failure), so the
   * old binding may or may not have been removed.
   */
  | {
      kind: "uncertain";
      error: HostedCommunityApiError;
      correlationId?: string;
    };

function requestFailure(cause: unknown): HostedCommunityApiError {
  return {
    code: "request_failed",
    message: cause instanceof Error ? cause.message : String(cause),
  };
}

/**
 * Move the account's identity binding to this device's key with as little
 * destruction as the Builderlab API allows. There is no atomic rebind call,
 * and `bind` always binds the local key, so a removed binding cannot be
 * restored from here. The switch therefore:
 *
 * 1. refuses unless the gate can name both keys (`canSwitchToDevice`);
 * 2. tries `bind` first — a failed bind changes nothing, so it doubles as an
 *    eligibility check: `pubkey_already_bound` (this device's key belongs to
 *    another account) or any unexpected error stops here with the old
 *    binding intact, and only `identity_already_bound` (the account holds the
 *    other key) proceeds;
 * 3. only then removes the old binding and binds this device, reporting
 *    `unbound` if that final bind fails so the caller can say so plainly.
 *
 * It never rejects: a transport failure before the delete is `unchanged`,
 * and one after the delete was sent is `uncertain`.
 *
 * Limit: step 2 relies on the Builderlab service reporting
 * `pubkey_already_bound` ahead of `identity_already_bound` when both apply.
 * That service is not in this repository, so the ordering cannot be tested
 * here; if it does not hold, the final bind fails and the outcome is
 * `unbound`, reported as such — never worse than deleting first.
 */
export async function switchHostedIdentityToDevice(
  gate: Pick<HostedIdentityGate, "canSwitchToDevice">,
  api: {
    bind: () => Promise<HostedIdentityResponse>;
    release: () => Promise<HostedIdentityResponse>;
  } = {
    bind: bindBuilderlabIdentity,
    release: deleteBuilderlabIdentity,
  },
): Promise<SwitchHostedIdentityOutcome> {
  if (!gate.canSwitchToDevice) return { kind: "refused" };

  let probe: HostedIdentityResponse;
  try {
    probe = await api.bind();
  } catch (cause) {
    return { kind: "unchanged", error: requestFailure(cause) };
  }
  if (!probe.error) {
    return { kind: "switched", identity: probe.identity ?? null };
  }
  if (probe.error.code !== "identity_already_bound") {
    return {
      kind: "unchanged",
      error: probe.error,
      correlationId: probe.correlation_id,
    };
  }

  let released: HostedIdentityResponse;
  try {
    released = await api.release();
  } catch (cause) {
    return { kind: "uncertain", error: requestFailure(cause) };
  }
  if (released.error) {
    return {
      kind: "unchanged",
      error: released.error,
      correlationId: released.correlation_id,
    };
  }

  let bound: HostedIdentityResponse;
  try {
    bound = await api.bind();
  } catch (cause) {
    return { kind: "uncertain", error: requestFailure(cause) };
  }
  if (bound.error) {
    return {
      kind: "unbound",
      error: bound.error,
      correlationId: bound.correlation_id,
    };
  }
  return { kind: "switched", identity: bound.identity ?? null };
}

/**
 * After a switch that did not complete: refresh the account (best effort)
 * and return the message to show. The warning never depends on the refresh
 * succeeding; when the account is known to be unbound and the refresh fails,
 * the caller's identity state is cleared so the screen does not keep showing
 * a binding that was removed.
 */
export async function settleFailedIdentitySwitch(
  outcome: Exclude<SwitchHostedIdentityOutcome, { kind: "switched" }>,
  reloadAccount: () => Promise<void>,
  clearIdentity: () => void,
): Promise<string> {
  const message = switchHostedIdentityErrorMessage(outcome);
  if (outcome.kind === "refused") return message;
  try {
    await reloadAccount();
  } catch (error) {
    console.warn("[hostedCommunity] account refresh after switch failed:", error);
    if (outcome.kind === "unbound") clearIdentity();
  }
  return message;
}

/** User-facing text for a switch that did not complete. */
export function switchHostedIdentityErrorMessage(
  outcome: Exclude<SwitchHostedIdentityOutcome, { kind: "switched" }>,
): string {
  if (outcome.kind === "refused") {
    return "Buzz can't confirm which identity this account or this device uses, so it won't disconnect anything. Sign out and sign in again, or restart Buzz.";
  }
  if (outcome.kind === "uncertain") {
    return `Buzz couldn't confirm whether the switch finished (${outcome.error.message ?? "no response"}). The account's previous Buzz identity may have been disconnected. Check the account again; if it shows no identity, connect this device or reconnect from the device that holds the previous identity.`;
  }
  const detail =
    outcome.error.code === "pubkey_already_bound"
      ? "This device's Buzz identity belongs to a different Builderlab account. Sign out, then sign in with the account that already owns this identity."
      : hostedCommunityErrorMessage(
          outcome.error,
          outcome.correlationId,
          outcome.kind === "unchanged"
            ? "Could not switch this account to this device's Buzz identity."
            : "Could not connect this device's Buzz identity.",
        );
  if (outcome.kind === "unchanged") {
    return `${detail} Nothing was changed.`;
  }
  return `The account's previous Buzz identity was disconnected, but connecting this device failed. ${detail} Try again, or reconnect from the device that holds the previous identity.`;
}

export function getBuilderlabAuth() {
  return invoke<BuilderlabAuth | null>("get_builderlab_auth");
}

export function cancelBuilderlabLogin() {
  return invoke<void>("cancel_builderlab_login");
}

export function clearBuilderlabAuth() {
  return invoke<void>("clear_builderlab_auth");
}

export function startBuilderlabLogin() {
  return invoke<BuilderlabAuth>("start_builderlab_login");
}

export async function loadHostedCommunityAccount(): Promise<HostedCommunityAccount> {
  const [identityResponse, communitiesResponse] = await Promise.all([
    invoke<HostedIdentityResponse>("get_builderlab_nostr_identity"),
    invoke<HostedCommunitiesResponse>("list_builderlab_communities"),
  ]);
  if (
    identityResponse.error &&
    identityResponse.error.code !== "unauthorized" &&
    !identityResponse.error.setup_needed
  ) {
    throw new Error(
      hostedCommunityErrorMessage(
        identityResponse.error,
        identityResponse.correlation_id,
        "Could not load the connected Buzz identity.",
      ),
    );
  }
  if (communitiesResponse.error && !communitiesResponse.error.setup_needed) {
    throw Object.assign(
      new Error(
        hostedCommunityErrorMessage(
          communitiesResponse.error,
          communitiesResponse.correlation_id,
          "Could not load communities.",
        ),
      ),
      { code: communitiesResponse.error.code },
    );
  }
  return {
    identity: identityResponse.identity ?? null,
    communities: communitiesResponse.communities ?? [],
  };
}

export function bindBuilderlabIdentity() {
  return invoke<HostedIdentityResponse>("bind_builderlab_nostr_identity");
}

export function deleteBuilderlabIdentity() {
  return invoke<HostedIdentityResponse>("delete_builderlab_nostr_identity");
}

export function checkHostedCommunityName(name: string) {
  return invoke<HostedCommunityAvailabilityResponse>(
    "check_builderlab_community_name",
    { name },
  );
}

export function createHostedCommunity(name: string) {
  return invoke<HostedCommunityMutationResponse>(
    "create_builderlab_community",
    {
      name,
    },
  );
}

/**
 * Whether a failed account load means the Builderlab session itself is no
 * longer valid (expired or revoked), as opposed to a transient failure.
 */
export function isUnauthorizedAccountLoad(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    (error as { code?: unknown }).code === "unauthorized"
  );
}
