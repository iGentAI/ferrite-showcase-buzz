import { isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Headphones } from "lucide-react";
import * as React from "react";
import { toast } from "sonner";
import { useQueryClient } from "@tanstack/react-query";

import { relayClient } from "@/shared/api/relayClient";
import type { RelayEvent } from "@/shared/api/types";
import { cn } from "@/shared/lib/cn";
import {
  HUDDLE_SHORTCUT_EVENT,
  type HuddleShortcutDetail,
} from "@/shared/lib/keyboard-shortcuts";
import { Button } from "@/shared/ui/button";
import { DropdownMenuItem } from "@/shared/ui/dropdown-menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/shared/ui/tooltip";
import { useRelaySelfQuery } from "@/features/moderation/hooks";
import { hasValidRelayEventSignature } from "@/shared/lib/relayEventSignature";
import { useHuddle } from "../HuddleContext";
import {
  type ActiveHuddleState,
  reconstructActiveHuddle,
} from "../lib/huddleIndicatorState";
import { formatHuddleActionError } from "../lib/huddleError";

type ActiveHuddle = ActiveHuddleState;

type HuddleIndicatorProps = {
  channelId: string;
  className?: string;
  renderMode?: "button" | "menu-item";
  /** Called when the user clicks the button and no huddle is active (start). */
  onStart?: () => void;
  /** Whether the start action is disabled (e.g., permissions, already starting). */
  startDisabled?: boolean;
};

/**
 * Detects active huddles in a channel via kind:48100-48103 events.
 * Shows a glowing headphone icon when a huddle is active, with participant count.
 * Click to join the huddle.
 */
export function HuddleIndicator({
  channelId,
  className,
  renderMode = "button",
  onStart,
  startDisabled,
}: HuddleIndicatorProps) {
  const huddlesAvailable = isTauri();
  const { activeEphemeralChannelId, joinHuddle, isStarting } = useHuddle();
  const queryClient = useQueryClient();
  const [activeHuddle, setActiveHuddle] = React.useState<ActiveHuddle | null>(
    null,
  );
  const [isJoining, setIsJoining] = React.useState(false);
  const reconstructRef = React.useRef<(() => void) | null>(null);
  const relaySelfPubkey = useRelaySelfQuery(Boolean(channelId)).data;

  React.useEffect(() => {
    if (!channelId) return;

    let disposed = false;
    let cleanup: (() => void) | null = null;

    // Track all seen events for reconstruction. Keyed by event.id for dedup.
    const seenEvents = new Map<string, RelayEvent>();

    function reconstruct() {
      const huddle = reconstructActiveHuddle(seenEvents.values(), relaySelfPubkey);
      if (!disposed) {
        setActiveHuddle(huddle);
      }
    }
    reconstructRef.current = reconstruct;

    // Subscribe to huddle lifecycle events only (kinds 48100–48103).
    // limit: 100 covers long-lived huddles with many join/leave cycles.
    relayClient
      .subscribeToHuddleEvents(channelId, (event: RelayEvent) => {
        if (disposed) return;

        // Dedup by event ID — ignore replayed events from reconnect.
        if (seenEvents.has(event.id)) return;
        // Frames are unverified relay JSON; provenance needs the signature.
        if (!hasValidRelayEventSignature(event)) return;
        seenEvents.set(event.id, event);

        // Reconstruct from full history on every new event.
        // This is cheap — huddle lifecycle events are rare (typically <20).
        reconstruct();
      })
      .then((dispose) => {
        if (disposed) {
          void dispose();
          return;
        }
        cleanup = () => void dispose();
      })
      .catch((err) => {
        console.error("[HuddleIndicator] subscription failed:", err);
      });

    return () => {
      disposed = true;
      cleanup?.();
      if (reconstructRef.current === reconstruct) reconstructRef.current = null;
      setActiveHuddle(null);
    };
  }, [channelId, relaySelfPubkey]);

  // When the local user ends/leaves a huddle, the backend transitions to idle
  // and emits huddle-state-changed. Clear the indicator immediately rather than
  // waiting for the relay's 48103 event (which may arrive late or not at all
  // if the relay connection tears down first).
  React.useEffect(() => {
    let unlisten: (() => void) | null = null;
    let cancelled = false;

    // "idle" says only that THIS client left: it is reported when the local
    // user leaves a huddle others are still in, and for a huddle in another
    // channel. So the indicator is re-derived from the signed lifecycle
    // events instead of cleared; clearing it left a running huddle showing
    // "Start huddle", with no way back in.
    listen<{ phase: string }>("huddle-state-changed", (event) => {
      if (!cancelled && event.payload.phase === "idle") {
        reconstructRef.current?.();
      }
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  React.useEffect(() => {
    function handleHuddleShortcut(event: Event) {
      const { channelId: shortcutChannelId } = (
        event as CustomEvent<HuddleShortcutDetail>
      ).detail;
      if (
        !huddlesAvailable ||
        shortcutChannelId !== channelId ||
        activeEphemeralChannelId ||
        isStarting ||
        isJoining
      )
        return;

      if (activeHuddle) {
        setIsJoining(true);
        void joinHuddle(
          channelId,
          activeHuddle.ephemeralChannelId,
          activeHuddle.huddleThreadEventId ?? undefined,
        )
          .then(() => {
            void queryClient.invalidateQueries({ queryKey: ["channels"] });
          })
          .catch((error) => {
            console.error("Failed to join huddle:", error);
            toast.error(formatHuddleActionError(error, "join"));
          })
          .finally(() => setIsJoining(false));
        return;
      }

      if (!startDisabled) onStart?.();
    }

    window.addEventListener(HUDDLE_SHORTCUT_EVENT, handleHuddleShortcut);
    return () =>
      window.removeEventListener(HUDDLE_SHORTCUT_EVENT, handleHuddleShortcut);
  }, [
    activeEphemeralChannelId,
    activeHuddle,
    channelId,
    huddlesAvailable,
    isJoining,
    isStarting,
    joinHuddle,
    onStart,
    queryClient,
    startDisabled,
  ]);

  // No active huddle — render the start button (if onStart provided).
  if (!activeHuddle) {
    if (!onStart) return null;
    if (renderMode === "menu-item") {
      return (
        <DropdownMenuItem
          className={className}
          data-testid="channel-start-huddle-trigger"
          disabled={!huddlesAvailable || startDisabled || isStarting}
          onSelect={() => onStart()}
        >
          <Headphones />
          <span>
            {huddlesAvailable ? "Start huddle" : "Huddles require desktop"}
          </span>
        </DropdownMenuItem>
      );
    }

    return (
      <Tooltip disableHoverableContent>
        <TooltipTrigger asChild>
          <span
            className="inline-flex"
            data-testid="channel-huddle-tooltip-trigger"
          >
            <Button
              aria-label={
                huddlesAvailable ? "Start huddle" : "Huddles require desktop"
              }
              className={className}
              data-testid="channel-start-huddle-trigger"
              disabled={!huddlesAvailable || startDisabled || isStarting}
              onClick={() => onStart()}
              size="icon"
              type="button"
              variant="outline"
            >
              <Headphones />
            </Button>
          </span>
        </TooltipTrigger>
        <TooltipContent>
          {huddlesAvailable ? "Huddle" : "Huddles require the Buzz desktop app"}
        </TooltipContent>
      </Tooltip>
    );
  }

  // At least 1 participant must exist for the huddle to be active.
  // When START fell out of the event window, the creator isn't in the
  // reconstructed set — floor at 1 to avoid showing "0 participants".
  const participantCount = Math.max(1, activeHuddle.participants.size);

  async function doJoin() {
    if (!huddlesAvailable || !activeHuddle || isJoining) return;
    setIsJoining(true);
    try {
      await joinHuddle(
        channelId,
        activeHuddle.ephemeralChannelId,
        activeHuddle.huddleThreadEventId ?? undefined,
      );
      // Refetch channels so the ephemeral channel appears in the sidebar.
      void queryClient.invalidateQueries({ queryKey: ["channels"] });
    } catch (e) {
      console.error("Failed to join huddle:", e);
      toast.error(formatHuddleActionError(e, "join"));
    } finally {
      setIsJoining(false);
    }
  }

  if (renderMode === "menu-item") {
    return (
      <DropdownMenuItem
        className={className}
        data-testid="channel-start-huddle-trigger"
        disabled={!huddlesAvailable || isJoining || isStarting}
        onSelect={() => void doJoin()}
      >
        <Headphones />
        <span>
          {huddlesAvailable ? "Join huddle" : "Huddles require desktop"}
        </span>
        <span className="ml-auto text-xs text-muted-foreground">
          {participantCount}
        </span>
      </DropdownMenuItem>
    );
  }

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          aria-label={`Join active huddle (${participantCount} participant${participantCount !== 1 ? "s" : ""})`}
          className={cn("relative", className)}
          disabled={!huddlesAvailable || isJoining || isStarting}
          onClick={() => void doJoin()}
          size="icon"
          type="button"
          variant="outline"
        >
          <Headphones />
          <span className="absolute inset-0 animate-pulse rounded-lg ring-2 ring-border/70" />
          {/* Participant count badge */}
          {participantCount > 0 && (
            <span className="absolute -right-1 -top-1 flex h-3.5 min-w-3.5 items-center justify-center rounded-full border border-border bg-background px-0.5 text-2xs font-bold text-muted-foreground">
              {participantCount}
            </span>
          )}
        </Button>
      </TooltipTrigger>
      <TooltipContent>
        {huddlesAvailable
          ? `Huddle active — ${participantCount} participant${participantCount !== 1 ? "s" : ""}`
          : "Huddles require the Buzz desktop app"}
      </TooltipContent>
    </Tooltip>
  );
}
