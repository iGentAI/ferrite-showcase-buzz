import * as React from "react";
import { CloudCog, TerminalSquare } from "lucide-react";

import type { AcpRuntimeCatalogEntry } from "@/shared/api/types";
import { cn } from "@/shared/lib/cn";
import { BuzzMark } from "@/shared/ui/buzz-logo/BuzzMark";
import ampLogoUrl from "../assets/harness-logos/amp.png?inline";
import claudeLogoUrl from "../assets/harness-logos/claude.png?inline";
import devinLogoUrl from "../assets/harness-logos/devin.svg?inline";
import grokLogoUrl from "../assets/harness-logos/grok.svg?inline";
import hermesLogoUrl from "../assets/harness-logos/hermes.png?inline";
import kimiLogoUrl from "../assets/harness-logos/kimi.png?inline";
import openclawLogoUrl from "../assets/harness-logos/openclaw.svg?inline";
import opencodeLogoUrl from "../assets/harness-logos/opencode.svg?inline";
import ompLogoUrl from "../assets/harness-logos/omp.svg?inline";
import piLogoUrl from "../assets/harness-logos/pi.svg?inline";
import { RUNTIME_MARKS } from "./HarnessMarks";

// Bundled logos for compiled-in runtimes (inline base64, no network fetch).
// Monochrome marks live in RUNTIME_MARKS instead — inline SVGs that follow
// `currentColor`, so they adapt to dark/light without bitmap filters.
const RUNTIME_LOGOS: Record<string, string> = {
  claude: claudeLogoUrl,
};

// Bundled logos for the tier-2 presets. Upstream served these from the Tauri
// shell's `public/harness-logos/`; this build imports the same files so they are
// inlined next to the one above. Two reasons the web port cannot keep the
// public paths: the app is served under `/buzz/ui/`, so a root-absolute `/…`
// URL would miss, and the relay ships `dist/` through `deploy_source`, whose
// file map is JSON string-valued — a loose `.png` cannot make that hop.
// Keys match the preset `id` values emitted by the backend PRESET_HARNESSES.
export const PRESET_LOGOS: Record<string, string> = {
  devin: devinLogoUrl,
  omp: ompLogoUrl,
  pi: piLogoUrl,
  grok: grokLogoUrl,
  opencode: opencodeLogoUrl,
  kimi: kimiLogoUrl,
  amp: ampLogoUrl,
  hermes: hermesLogoUrl,
  openclaw: openclawLogoUrl,
};

function isBuzzRuntime(runtime: AcpRuntimeCatalogEntry): boolean {
  return runtime.id.trim().toLowerCase() === "buzz-agent";
}

export function getRuntimeDisplayLabel(
  runtime: AcpRuntimeCatalogEntry,
): string {
  return isBuzzRuntime(runtime) ? "Buzz" : runtime.label;
}

function getRuntimeLogoUrl(runtime: AcpRuntimeCatalogEntry): string | null {
  const id = runtime.id.trim().toLowerCase();
  return RUNTIME_LOGOS[id] ?? PRESET_LOGOS[id] ?? null;
}

export function RuntimeIcon({
  className = "h-8 w-8",
  runtime,
}: {
  className?: string;
  runtime: AcpRuntimeCatalogEntry;
}) {
  const [imageFailed, setImageFailed] = React.useState(false);
  // Only use bundled logo maps — never render user-supplied avatar URLs for
  // custom/preset entries (tracking pixel / spoofing vector, security line).
  const id = runtime.id.trim().toLowerCase();
  const imageUrl = getRuntimeLogoUrl(runtime);
  const Mark = RUNTIME_MARKS[id];

  if (isBuzzRuntime(runtime)) {
    // The mark's wide viewBox letterboxes inside a square box, so honoring
    // the caller's size keeps it optically in line with the square logos.
    return <BuzzMark className={cn(className, "text-foreground")} />;
  }

  if (Mark) {
    return <Mark className={cn(className, "p-0.5 text-foreground")} />;
  }

  // A remote harness is a service, not a terminal binary — the default
  // TerminalSquare would tell the user to look for a CLI that does not exist.
  if (id === "ferrite-cloud" || id === "fabric" || id === "a2a") {
    return (
      <CloudCog className={cn(className, "text-foreground")} strokeWidth={1.25} />
    );
  }

  if (imageUrl && !imageFailed) {
    return (
      <img
        alt=""
        className={cn(
          "rounded-md object-contain",
          className,
          id === "omp" && "bg-[#0d0d0d] p-1",
          id === "grok" && "bg-white p-1",
        )}
        onError={() => setImageFailed(true)}
        src={imageUrl}
      />
    );
  }

  return (
    <TerminalSquare
      className={cn(className, "text-foreground")}
      strokeWidth={1.25}
    />
  );
}
