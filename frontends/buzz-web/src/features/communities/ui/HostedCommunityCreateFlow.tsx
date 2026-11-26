import * as React from "react";
import { AlertCircle, ExternalLink, LoaderCircle } from "lucide-react";

import {
  bindBuilderlabIdentity,
  cancelBuilderlabLogin,
  checkHostedCommunityName,
  clearBuilderlabAuth,
  createHostedCommunity,
  evaluateHostedIdentityGate,
  getBuilderlabAuth,
  HOSTED_COMMUNITY_LIMIT,
  HOSTED_COMMUNITY_SUFFIX,
  hostedCommunityErrorMessage,
  hostedCommunityRelayUrl,
  loadHostedCommunityAccount,
  startBuilderlabLogin,
  switchHostedIdentityToDevice,
  settleFailedIdentitySwitch,
  usableBoundIdentityNpub,
  VALID_HOSTED_COMMUNITY_NAME,
  type BuilderlabAuth,
  type HostedCommunity,
  type HostedNostrIdentity,
  isUnauthorizedAccountLoad,
} from "@/features/communities/hostedCommunityApi";
import { useCommunityOnboarding } from "@/features/onboarding/communityOnboarding";
import {
  CHANNEL_FORM_FIELD_CONTROL_CLASS,
  CHANNEL_FORM_FIELD_SHELL_CLASS,
} from "@/features/channels/ui/channelFormStyles";
import { useIdentityQuery } from "@/shared/api/hooks";
import { cn } from "@/shared/lib/cn";
import { safeNpub } from "@/shared/lib/nostrUtils";
import { UNAVAILABLE_KEY_LABEL } from "@/shared/lib/pubkey";
import { Button } from "@/shared/ui/button";
import { Input } from "@/shared/ui/input";

type HostedCommunityCreateFlowProps = {
  onComplete: () => void;
};

export function HostedCommunityCreateFlow({
  onComplete,
}: HostedCommunityCreateFlowProps) {
  const onboarding = useCommunityOnboarding();
  const identityQuery = useIdentityQuery();
  const localPubkey = identityQuery.data?.pubkey ?? null;
  const [auth, setAuth] = React.useState<BuilderlabAuth | null>(null);
  const [identity, setIdentity] = React.useState<HostedNostrIdentity | null>(
    null,
  );
  const [communities, setCommunities] = React.useState<HostedCommunity[]>([]);
  const [name, setName] = React.useState("");
  const [availability, setAvailability] = React.useState<boolean | null>(null);
  const [checkingName, setCheckingName] = React.useState(false);
  const [loading, setLoading] = React.useState(true);
  const [action, setAction] = React.useState<string | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const loginAttempt = React.useRef(0);
  const signingIn = React.useRef(false);

  // A failed account load is its own state, not "no identity bound": the
  // identity is unknown, and offering to bind one (or treating the account as
  // ready with no communities) was wrong. An expired session signs out.
  const [accountLoadFailed, setAccountLoadFailed] = React.useState(false);
  const loadAccount = React.useCallback(async () => {
    try {
      const account = await loadHostedCommunityAccount();
      setIdentity(account.identity);
      setCommunities(account.communities);
      setAccountLoadFailed(false);
    } catch (error) {
      if (isUnauthorizedAccountLoad(error)) {
        setAuth(null);
        setIdentity(null);
        setCommunities([]);
        setAccountLoadFailed(false);
        void clearBuilderlabAuth().catch(() => {});
      } else {
        setAccountLoadFailed(true);
      }
      throw error;
    }
  }, []);

  React.useEffect(() => {
    let active = true;
    void getBuilderlabAuth()
      .then(async (nextAuth) => {
        if (!active) return;
        setAuth(nextAuth);
        if (nextAuth) await loadAccount();
      })
      .catch((cause) => {
        if (active)
          setError(cause instanceof Error ? cause.message : String(cause));
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
      loginAttempt.current += 1;
      if (signingIn.current) {
        signingIn.current = false;
        void cancelBuilderlabLogin().catch(() => {
          // The browser sign-in is already detached from this flow.
          // Cancellation is best-effort cleanup for the native callback.
        });
      }
    };
  }, [loadAccount]);

  const run = async (label: string, operation: () => Promise<void>) => {
    setAction(label);
    setError(null);
    try {
      await operation();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setAction(null);
    }
  };

  const signIn = () => {
    const attempt = ++loginAttempt.current;
    signingIn.current = true;
    setAction("Signing in…");
    setError(null);
    void startBuilderlabLogin()
      .then(async (nextAuth) => {
        if (loginAttempt.current !== attempt) return;
        setAuth(nextAuth);
        await loadAccount();
      })
      .catch((cause) => {
        if (loginAttempt.current !== attempt) return;
        setError(cause instanceof Error ? cause.message : String(cause));
      })
      .finally(() => {
        if (loginAttempt.current === attempt) {
          signingIn.current = false;
          setAction(null);
        }
      });
  };

  const connectIdentity = () =>
    run("Connecting identity…", async () => {
      const response = await bindBuilderlabIdentity();
      if (response.error) {
        throw new Error(
          hostedCommunityErrorMessage(
            response.error,
            response.correlation_id,
            "Could not connect the Buzz identity.",
          ),
        );
      }
      await loadAccount();
    });

  const signOut = () =>
    run("Signing out…", async () => {
      await clearBuilderlabAuth();
      setAuth(null);
      setIdentity(null);
      setCommunities([]);
    });

  // Identity rows display npubs derived from the same key the mismatch gate
  // and recovery actions act on: the account row from the authoritative
  // bound `pubkey_hex` — the server-sent `npub` is an independent field that
  // nothing on this path proves encodes the same key — and the device row
  // from the local key. An unencodable or non-identity-length key renders the
  // neutral label instead of leaking raw hex or the unverified npub. Both
  // keys are compared in the one normalized hex form, so a padded or
  // mixed-case spelling of the same key never reads as a mismatch, and an
  // npub stored in the hex field is not a key at all.
  //
  // The gate fails closed: until this device's key is known (loading, failed
  // or unusable) the flow neither reports a match nor offers create/connect,
  // and the destructive switch is offered only when both keys are known.
  const gate = evaluateHostedIdentityGate({ auth, identity, localPubkey });
  const { localHex, usableBoundIdentity, identityMismatch, localKeyUnknown } =
    gate;
  const localNpub = localHex === null ? null : safeNpub(localHex);
  const boundNpub = usableBoundIdentityNpub(identity);

  const switchToDeviceIdentity = () =>
    run("Switching identity…", async () => {
      const outcome = await switchHostedIdentityToDevice(gate);
      if (outcome.kind !== "switched") {
        throw new Error(
          // The previous identity is known to be gone: the account is
          // unbound, not unloaded.
          await settleFailedIdentitySwitch(outcome, loadAccount, () => {
            setIdentity(null);
            setAccountLoadFailed(false);
          }),
        );
      }
      await loadAccount();
    });

  const normalizedName = name.trim().toLowerCase();
  const validName =
    normalizedName.length <= 63 &&
    VALID_HOSTED_COMMUNITY_NAME.test(normalizedName);
  const atCommunityLimit = communities.length >= HOSTED_COMMUNITY_LIMIT;
  const ready = gate.ready;

  React.useEffect(() => {
    if (!ready || !normalizedName || !validName) {
      setCheckingName(false);
      return;
    }
    let cancelled = false;
    setCheckingName(true);
    const handle = window.setTimeout(() => {
      void checkHostedCommunityName(normalizedName)
        .then((response) => {
          if (!cancelled)
            setAvailability(
              response.error ? null : (response.available ?? false),
            );
        })
        .catch(() => {
          if (!cancelled) setAvailability(null);
        })
        .finally(() => {
          if (!cancelled) setCheckingName(false);
        });
    }, 500);
    return () => {
      cancelled = true;
      window.clearTimeout(handle);
    };
  }, [normalizedName, ready, validName]);

  const create = (event: React.FormEvent) => {
    event.preventDefault();
    if (!validName || !ready || atCommunityLimit) return;
    void run("Creating community…", async () => {
      const available = await checkHostedCommunityName(normalizedName);
      if (available.error || !available.available) {
        // A failed check says nothing about the name: keep it retryable
        // rather than reporting it taken.
        setAvailability(available.error ? null : false);
        throw new Error(
          hostedCommunityErrorMessage(
            available.error,
            available.correlation_id,
            "That Buzz address is already taken.",
          ),
        );
      }
      const response = await createHostedCommunity(normalizedName);
      if (response.error || !response.community) {
        throw new Error(
          hostedCommunityErrorMessage(
            response.error,
            response.correlation_id,
            "Could not create the community.",
          ),
        );
      }
      // The community exists now, whatever happens next: clear the form and
      // refresh the account so the limit and the list are current.
      setName("");
      setAvailability(null);
      void loadAccount().catch(() => {});
      const relayUrl = hostedCommunityRelayUrl(response.community);
      if (!relayUrl) {
        throw new Error(
          "The community was created, but Builderlab did not return its community URL. Try connecting it again from settings.",
        );
      }
      const started = onboarding.start({
        source: "add-community",
        relayUrl,
        communityName: response.community.name ?? response.community.slug,
      });
      if (!started) {
        throw new Error(
          "Finish connecting the community already in progress, then try again.",
        );
      }
      onComplete();
    });
  };

  const errorBox = error ? (
    <div
      className="flex items-start gap-3 rounded-xl border border-destructive/30 bg-destructive/10 p-4"
      role="alert"
    >
      <AlertCircle className="mt-0.5 h-4 w-4 shrink-0 text-destructive" />
      <p className="text-sm leading-5 text-destructive">{error}</p>
    </div>
  ) : null;

  if (loading) {
    return (
      <div className="flex min-h-40 items-center justify-center" role="status">
        <LoaderCircle className="h-5 w-5 animate-spin text-muted-foreground" />
        <span className="sr-only">Checking sign-in</span>
      </div>
    );
  }

  if (!auth) {
    return (
      <div className="space-y-5">
        <p className="text-sm leading-6 text-muted-foreground">
          Sign in with Builderlab to create and host a community. Buzz will open
          your browser, then bring you back here.
        </p>
        {errorBox}
        <div className="flex justify-end pt-1">
          <Button disabled={Boolean(action)} onClick={signIn} type="button">
            {action === "Signing in…" ? (
              <LoaderCircle className="h-4 w-4 animate-spin" />
            ) : null}
            {action ?? "Continue to Builderlab"}
            {action ? null : <ExternalLink className="h-4 w-4" />}
          </Button>
        </div>
      </div>
    );
  }

  if (accountLoadFailed) {
    return (
      <div className="space-y-5">
        <p className="text-sm leading-6 text-muted-foreground">
          Buzz could not load your Builderlab account.
        </p>
        {errorBox}
        <div className="flex justify-end gap-2 pt-1">
          <Button
            disabled={Boolean(action)}
            onClick={() => void signOut()}
            type="button"
            variant="outline"
          >
            Use a different account
          </Button>
          <Button
            disabled={Boolean(action)}
            onClick={() => void run("Loading account…", loadAccount)}
            type="button"
          >
            {action ? <LoaderCircle className="h-4 w-4 animate-spin" /> : null}
            {action ?? "Try again"}
          </Button>
        </div>
      </div>
    );
  }

  if (!identity) {
    return (
      <div className="space-y-5">
        <p className="text-sm leading-6 text-muted-foreground">
          Connect this device’s Buzz identity to your Builderlab account. Your
          private key stays on this device.
        </p>
        {errorBox}
        <div className="flex justify-end gap-2 pt-1">
          <Button
            disabled={Boolean(action)}
            onClick={() => void signOut()}
            type="button"
            variant="outline"
          >
            Use a different account
          </Button>
          <Button
            disabled={Boolean(action)}
            onClick={() => void connectIdentity()}
            type="button"
          >
            {action ? <LoaderCircle className="h-4 w-4 animate-spin" /> : null}
            {action ?? "Connect and continue"}
          </Button>
        </div>
      </div>
    );
  }

  if (identityMismatch) {
    return (
      <div className="space-y-5">
        <p className="text-sm leading-6 text-muted-foreground">
          {usableBoundIdentity
            ? "This Builderlab account uses a different Buzz identity. Switch it to this device, or sign in with another account."
            : "Buzz can't read the identity this Builderlab account is connected to, so it won't disconnect it from here. Sign in with another account, or try again later."}
        </p>
        <div className="rounded-xl bg-muted/40 px-4 py-3 font-mono text-xs text-muted-foreground">
          <p className="break-all">
            Account: {boundNpub ?? UNAVAILABLE_KEY_LABEL}
          </p>
          <p className="mt-1 break-all">
            This device: {localNpub ?? UNAVAILABLE_KEY_LABEL}
          </p>
        </div>
        {errorBox}
        <div className="flex justify-end gap-2 pt-1">
          <Button
            disabled={Boolean(action)}
            onClick={() => void signOut()}
            type="button"
            variant="outline"
          >
            Use a different account
          </Button>
          <Button
            disabled={Boolean(action) || !gate.canSwitchToDevice}
            onClick={() => void switchToDeviceIdentity()}
            type="button"
          >
            {action ? <LoaderCircle className="h-4 w-4 animate-spin" /> : null}
            {action ?? "Use this device"}
          </Button>
        </div>
      </div>
    );
  }

  if (localKeyUnknown) {
    // Bound account, but this device's key is not known yet: never guess a
    // match. Wait for the identity query, or say it failed.
    return identityQuery.isPending ? (
      <div className="flex min-h-40 items-center justify-center" role="status">
        <LoaderCircle className="h-5 w-5 animate-spin text-muted-foreground" />
        <span className="sr-only">Checking this device’s identity</span>
      </div>
    ) : (
      <div className="space-y-5">
        <div
          className="flex items-start gap-3 rounded-xl border border-destructive/30 bg-destructive/10 p-4"
          role="alert"
        >
          <AlertCircle className="mt-0.5 h-4 w-4 shrink-0 text-destructive" />
          <p className="text-sm leading-5 text-destructive">
            Buzz couldn’t read this device’s identity, so it can’t confirm this
            account belongs to it. Restart Buzz and try again.
          </p>
        </div>
        {errorBox}
        <div className="flex justify-end gap-2 pt-1">
          <Button
            disabled={Boolean(action)}
            onClick={() => void identityQuery.refetch()}
            type="button"
            variant="outline"
          >
            Try again
          </Button>
          <Button
            disabled={Boolean(action)}
            onClick={() => void signOut()}
            type="button"
            variant="outline"
          >
            Use a different account
          </Button>
        </div>
      </div>
    );
  }

  const feedback = atCommunityLimit
    ? `You’ve reached the limit of ${HOSTED_COMMUNITY_LIMIT} hosted communities.`
    : name && !validName
      ? "Use lowercase letters, numbers, and single hyphens."
      : checkingName
        ? "Checking availability…"
        : availability === false
          ? "That address is already taken."
          : availability === true
            ? "That address is available."
            : "You can’t change this address after creating the community.";

  return (
    <form className="space-y-5" onSubmit={create}>
      <div className="space-y-1.5">
        <label
          className="text-sm font-medium text-foreground"
          htmlFor="hosted-community-create-name"
        >
          Community address
        </label>
        <div
          className={cn(
            "flex min-h-11 items-center px-3",
            CHANNEL_FORM_FIELD_SHELL_CLASS,
          )}
        >
          <Input
            autoCapitalize="none"
            autoComplete="off"
            autoCorrect="off"
            autoFocus
            className={cn(
              "h-8 min-w-0 px-0 py-0 leading-6",
              CHANNEL_FORM_FIELD_CONTROL_CLASS,
            )}
            data-testid="hosted-community-create-name"
            disabled={Boolean(action) || atCommunityLimit}
            id="hosted-community-create-name"
            maxLength={63}
            onChange={(event) => {
              setName(event.target.value.toLowerCase());
              setAvailability(null);
              setError(null);
            }}
            placeholder="north-star"
            spellCheck={false}
            value={name}
          />
          <span className="shrink-0 text-sm text-muted-foreground/70">
            .{HOSTED_COMMUNITY_SUFFIX}
          </span>
        </div>
        <p
          className={cn(
            "text-xs leading-5",
            availability === false || (name && !validName)
              ? "text-destructive"
              : "text-muted-foreground",
          )}
        >
          {feedback}
        </p>
      </div>
      {errorBox}
      <div className="flex justify-end pt-1">
        <Button
          data-testid="hosted-community-create-submit"
          disabled={
            !validName ||
            availability === false ||
            checkingName ||
            Boolean(action) ||
            atCommunityLimit
          }
          type="submit"
        >
          {action ? <LoaderCircle className="h-4 w-4 animate-spin" /> : null}
          {action ?? "Create community"}
        </Button>
      </div>
    </form>
  );
}
