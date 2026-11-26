import { useRouter } from "@tanstack/react-router";
import {
  loadActiveCommunityId,
  loadCommunities,
} from "@/features/communities/communityStorage";
import * as React from "react";

import type { deriveShellRoute } from "@/app/AppShell.helpers";
import type { useAppNavigation } from "@/app/navigation/useAppNavigation";
import { allowNavigation } from "@/app/navigation/navigationGuard";
import {
  replaceCommunityDestinationRoute,
  runCommunityViewTransition,
} from "@/app/communityViewTransition";
import {
  loadCommunityDestination,
  markPendingCommunityRestore,
  saveCommunityDestination,
} from "@/features/communities/communityNavigationStorage";
import { markCommunityDiscoveryAfterLeave } from "@/features/communities/communityStorage";
import type { useCommunities } from "@/features/communities/useCommunities";
import { leaveCommunity } from "@/features/communities/leaveCommunity";

type Communities = ReturnType<typeof useCommunities>;
type ShellRoute = ReturnType<typeof deriveShellRoute>;
type GoHome = ReturnType<typeof useAppNavigation>["goHome"];

export function useCommunityNavigationTransitions({
  communities,
  selectedChannelId,
  selectedView,
}: {
  communities: Communities;
  /** Unused: teardown navigates unguarded once the guard has been asked. */
  goHome?: GoHome;
  selectedChannelId: ShellRoute["selectedChannelId"];
  selectedView: ShellRoute["selectedView"];
}) {
  const router = useRouter();
  // The transitions await relay I/O while the shell stays interactive, so
  // they read community and route state from here after each await, never
  // from the render that started them (finding_1eb1d83a481ea545).
  const liveRef = React.useRef({ communities, selectedChannelId, selectedView });
  liveRef.current = { communities, selectedChannelId, selectedView };

  const saveActiveDestination = React.useCallback(() => {
    const live = liveRef.current;
    const activeCommunityId = live.communities.activeCommunity?.id;
    if (!activeCommunityId) return;
    saveCommunityDestination(
      activeCommunityId,
      live.selectedView === "channel" && live.selectedChannelId
        ? { kind: "channel", channelId: live.selectedChannelId }
        : { kind: "home" },
    );
  }, []);

  // Whether a guard (an unfinished thread edit, say) lets the view leave for
  // Home. Asked before anything changes, so a refusal changes nothing.
  const homeAllowed = React.useCallback(() => {
    if (router.state.location.pathname === "/") return true;
    return allowNavigation({ kind: "route", href: "/" });
  }, [router]);

  // Home is a teardown barrier: the outgoing channel must unmount before the
  // relay changes, or its read effect can advance markers on the wrong relay.
  // The guard was asked already, so this navigation is unguarded and always
  // reaches Home, also when the next community has no saved destination.
  const teardownToHome = React.useCallback(async () => {
    await router.navigate({ to: "/", replace: true });
  }, [router]);

  const switchCommunity = React.useCallback(
    async (id: string) => {
      const activeCommunityId = liveRef.current.communities.activeCommunity?.id;
      if (id === activeCommunityId) return;
      if (!activeCommunityId) {
        liveRef.current.communities.switchCommunity(id);
        return;
      }
      if (!homeAllowed()) return;

      await runCommunityViewTransition(async () => {
        saveActiveDestination();
        await teardownToHome();
        markPendingCommunityRestore(id);
        const destination = loadCommunityDestination(id);
        if (destination?.kind === "channel") {
          replaceCommunityDestinationRoute(
            destination.channelId,
            router.history,
          );
        }
        liveRef.current.communities.switchCommunity(id);
      });
    },
    [homeAllowed, router.history, saveActiveDestination, teardownToHome],
  );

  const removeCommunity = React.useCallback(
    async (id: string) => {
      const target = liveRef.current.communities.communities.find(
        (community) => community.id === id,
      );
      if (!target) return;
      // Leaving is irreversible once the relay accepts it, so a guard that
      // would keep the view from leaving the active community is asked first.
      if (
        id === liveRef.current.communities.activeCommunity?.id &&
        !homeAllowed()
      ) {
        return;
      }

      // Do not touch local state until this relay has explicitly accepted the
      // signed NIP-43 leave request. Rejections and timeouts bubble back to the
      // dialog so the person can retry without losing their community config.
      // Read the active relay again after each await inside, so a switch
      // meanwhile is seen (finding_b008ebabe1f129e9).
      const leaveResult = await leaveCommunity(target.relayUrl, () => {
        const activeId = loadActiveCommunityId();
        return loadCommunities().find((community) => community.id === activeId)
          ?.relayUrl;
      });

      // Re-derive everything after the await: the active community may have
      // changed while the relay answered.
      const live = liveRef.current.communities;
      if (id !== live.activeCommunity?.id) {
        live.removeCommunity(id);
        return leaveResult;
      }
      const fallback = live.communities.find((community) => community.id !== id);

      if (!fallback) {
        if (!markCommunityDiscoveryAfterLeave()) {
          throw new Error(
            "Membership was removed, but community discovery state could not be saved. Restart Buzz and try again.",
          );
        }
        await teardownToHome();
        liveRef.current.communities.removeCommunity(id);
        return leaveResult;
      }

      await runCommunityViewTransition(async () => {
        saveActiveDestination();
        await teardownToHome();
        markPendingCommunityRestore(fallback.id);
        const destination = loadCommunityDestination(fallback.id);
        if (destination?.kind === "channel") {
          replaceCommunityDestinationRoute(
            destination.channelId,
            router.history,
          );
        }
        liveRef.current.communities.removeCommunity(id);
      });
      return leaveResult;
    },
    [homeAllowed, router.history, saveActiveDestination, teardownToHome],
  );

  return { removeCommunity, switchCommunity };
}
