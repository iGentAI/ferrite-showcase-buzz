import * as React from "react";

import { useAppNavigation } from "@/app/navigation/useAppNavigation";
import { useProjectsQuery } from "@/features/projects/hooks";
import type { Project } from "@/features/projects/projectModels";
import {
  entityLinkProjectRouteId,
  isEntityLink,
  parseEntityLink,
  type ParsedEntityLink,
} from "@/shared/lib/entityLink";
import { parseSupportedLinkPreview } from "@/shared/lib/linkPreview";

import {
  loadBuzzEntityMetadata,
  type LinkPreviewMetadata,
} from "@/shared/lib/useResolvedLinkPreviews";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/shared/ui/tooltip";

import { BuzzInlineLink, BuzzLinkChip } from "./BuzzLinkChip";
import { SpoilerHiddenContext } from "./SpoilerInline";
import { useInlineTooltipPosition } from "./useInlineTooltipPosition";

/**
 * Relay metadata for an entity chip is resolved when the reader shows intent
 * (pointer enters or the chip takes focus), not when the chip mounts: a
 * message can carry any number of entity links, and each one would otherwise
 * queue up to two relay queries on the viewer's connection as soon as it
 * scrolled into view.
 */
function EntityMetadataTooltip({
  children,
  fallback,
  footer,
  href,
  link,
  onMetadataIntent,
  projects,
}: {
  children: (
    metadata: LinkPreviewMetadata | null | undefined,
  ) => React.ReactElement;
  fallback: string;
  footer: string;
  href: string;
  link: ParsedEntityLink;
  onMetadataIntent?: () => void;
  projects: Project[] | undefined;
}) {
  const hiddenInSpoiler = React.useContext(SpoilerHiddenContext);
  if (hiddenInSpoiler) {
    // Same rule as MaskedLinkTooltip: an unrevealed spoiler's link must not
    // leak its target through a portaled hover/focus tooltip.
    return children(undefined);
  }
  return (
    <ResolvedEntityMetadataTooltip
      fallback={fallback}
      footer={footer}
      href={href}
      link={link}
      onMetadataIntent={onMetadataIntent}
      projects={projects}
    >
      {children}
    </ResolvedEntityMetadataTooltip>
  );
}

function ResolvedEntityMetadataTooltip({
  children,
  fallback,
  footer,
  href,
  link,
  onMetadataIntent,
  projects,
}: {
  children: (
    metadata: LinkPreviewMetadata | null | undefined,
  ) => React.ReactElement;
  fallback: string;
  footer: string;
  href: string;
  link: ParsedEntityLink;
  onMetadataIntent?: () => void;
  projects: Project[] | undefined;
}) {
  const { contentRef, onPointerMove } = useInlineTooltipPosition();
  const [resolved, setResolved] = React.useState<{
    href: string;
    metadata: LinkPreviewMetadata | null;
  } | null>(null);
  const [requestedHref, setRequestedHref] = React.useState<string | null>(
    null,
  );
  const requestMetadata = React.useCallback(() => {
    setRequestedHref(href);
    onMetadataIntent?.();
  }, [href, onMetadataIntent]);
  React.useEffect(() => {
    if (requestedHref !== href) return;
    let cancelled = false;
    void loadBuzzEntityMetadata(href).then((value) => {
      if (!cancelled) setResolved({ href, metadata: value });
    });
    return () => {
      cancelled = true;
    };
  }, [href, requestedHref]);
  const metadata = resolved?.href === href ? resolved.metadata : undefined;
  const repositoryAddress =
    link.type === "issue" || link.type === "pr"
      ? `30617:${link.owner}:${link.dtag}`
      : null;
  const containingProject = repositoryAddress
    ? projects?.find((project) =>
        project.repositoryAddresses.includes(repositoryAddress),
      )
    : null;
  const resolvedTitle = metadata?.title.trim();
  const chipRepeatsTitle =
    Boolean(resolvedTitle) && (link.type === "issue" || link.type === "pr");
  const projectName = containingProject?.name.trim();
  const projectDescription = containingProject?.description.trim();
  const projectContext = projectName
    ? projectDescription && projectDescription !== projectName
      ? `${projectName} · ${projectDescription}`
      : projectName
    : null;
  const context =
    metadata === null
      ? null
      : (link.type === "issue" || link.type === "pr") && resolvedTitle
        ? resolvedTitle
        : projectContext
          ? projectContext
          : chipRepeatsTitle
            ? metadata?.description
            : [resolvedTitle || fallback, metadata?.description]
                .filter((value): value is string => Boolean(value))
                .join(" · ");
  const chip = children(metadata);
  return (
    <TooltipProvider delayDuration={500} skipDelayDuration={0}>
      <Tooltip>
        <TooltipTrigger
          asChild
          onFocus={requestMetadata}
          onPointerEnter={requestMetadata}
          onPointerMove={onPointerMove}
        >
          {chip}
        </TooltipTrigger>
        <TooltipContent
          ref={contentRef}
          className="max-w-72 p-2 text-left"
          side="top"
        >
          {context ? (
            <span
              className="line-clamp-3 [overflow-wrap:anywhere] whitespace-normal"
              data-buzz-tooltip-metadata-content=""
            >
              {context}
            </span>
          ) : null}
          <span
            className={`${context ? "mt-1 " : ""}line-clamp-2 max-w-full [overflow-wrap:anywhere] whitespace-normal text-2xs text-secondary-foreground/80`}
            data-buzz-tooltip-metadata-type=""
          >
            {footer}
          </span>
        </TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}

function entityLinkPresentation(link: ParsedEntityLink) {
  switch (link.type) {
    case "repo":
      return {
        ariaLabel: link.commitHash
          ? `Open commit ${link.commitHash.slice(0, 8)} in repository ${link.dtag}`
          : `Open repository ${link.dtag}`,
        icon: "repo" as const,
        label: link.commitHash
          ? `${link.dtag} · ${link.commitHash.slice(0, 8)}`
          : link.dtag,
        tooltipFooter: "Repository",
      };
    case "pr":
      return {
        ariaLabel: `Open pull request ${link.id.slice(0, 8)} in repository ${link.dtag}`,
        icon: "pr" as const,
        label: `${link.dtag} · ${link.id.slice(0, 8)}`,
        tooltipFooter: `Pull request · ${link.dtag}`,
      };
    case "issue":
      return {
        ariaLabel: `Open issue ${link.id.slice(0, 8)} in repository ${link.dtag}`,
        icon: "issue" as const,
        // Tooltip fallback only — the inline chip renders the repository name
        // alone (see `chip` below).
        label: `${link.dtag} · ${link.id.slice(0, 8)}`,
        tooltipFooter: `Issue · ${link.dtag}`,
      };
    case "project":
      return {
        ariaLabel: `Open project ${link.dtag}`,
        icon: "project" as const,
        label: link.dtag,
        tooltipFooter: "Project",
      };
  }
}

/**
 * Navigate to the project detail view for a `buzz://pr|issue|repo` link.
 * The link's (owner, d) coordinate is exactly the `/projects/$projectId`
 * route id, so no read-model resolution is needed.
 */
export function useOpenEntityLink(): (link: ParsedEntityLink) => void {
  const { goProject } = useAppNavigation();
  return React.useCallback(
    (link: ParsedEntityLink) => {
      const tab =
        (link.type === "repo" || link.type === "project") && link.tab
          ? link.tab
          : undefined;
      void goProject(entityLinkProjectRouteId(link), {
        entityNavigationId: crypto.randomUUID(),
        ...(tab
          ? {
              tab,
            }
          : {}),
        ...(link.type === "pr" ? { pullRequestId: link.id } : {}),
        ...(link.type === "issue" ? { issueId: link.id } : {}),
        ...(link.type === "repo" && link.commitHash
          ? { commitHash: link.commitHash }
          : {}),
      });
    },
    [goProject],
  );
}

/**
 * Resolve an anchor href to a canonical `buzz://` entity link, accepting
 * both the deep-link scheme directly and HTTPS relay clone URLs (which the
 * preview parser normalizes onto `buzz://repo` only when the URL origin
 * matches the active relay origin).
 */
function resolveEntityHref(
  href: string,
  relayOrigin: string | null,
): string | null {
  if (isEntityLink(href)) return href;
  if (!/^https?:\/\//i.test(href)) return null;

  const preview = parseSupportedLinkPreview(href, relayOrigin);
  return preview && isEntityLink(preview.href) ? preview.href : null;
}

/**
 * Render an inline anchor for a Buzz entity link (`buzz://pr|issue|repo` or
 * an HTTPS relay clone URL whose origin matches the active relay) that
 * navigates in-app instead of handing the URL to the OS. Returns null when
 * the href is not a valid entity link so the caller can fall through to its
 * default anchor.
 */
export function EntityLinkAnchor({
  children,
  href,
  onOpenEntityLink,
  relayOrigin,
  interactive = true,
  asChip = true,
}: {
  children?: React.ReactNode;
  href: string;
  onOpenEntityLink: (link: ParsedEntityLink) => void;
  relayOrigin: string | null;
  interactive?: boolean;
  asChip?: boolean;
}): React.ReactElement | null {
  // The project list is an exhaustive paginated relay scan. Reuse whatever is
  // already cached, and start the scan only once a reader actually opens an
  // entity tooltip, rather than whenever any entity link is rendered.
  const [wantsProjects, setWantsProjects] = React.useState(false);
  const requestProjects = React.useCallback(() => setWantsProjects(true), []);
  const { data: projects } = useProjectsQuery(interactive && wantsProjects);
  return renderEntityLinkAnchor({
    children,
    href,
    onOpenEntityLink,
    relayOrigin,
    interactive,
    asChip,
    onMetadataIntent: requestProjects,
    projects,
  });
}

/**
 * Pure rendering boundary retained for static-markup tests and non-provider
 * callers. The normal Markdown path uses `EntityLinkAnchor` above so the
 * authoritative Projects read model can enrich issue/PR tooltips.
 */
export function renderEntityLinkAnchor({
  children,
  href,
  onOpenEntityLink,
  relayOrigin,
  interactive = true,
  asChip = true,
  onMetadataIntent,
  projects,
}: {
  children: React.ReactNode;
  href: string | undefined;
  onOpenEntityLink: (link: ParsedEntityLink) => void;
  relayOrigin: string | null;
  interactive?: boolean;
  asChip?: boolean;
  onMetadataIntent?: () => void;
  projects?: Project[];
}): React.ReactElement | null {
  if (!href) return null;

  const canonicalHref = resolveEntityHref(href, relayOrigin);
  if (!canonicalHref) return null;

  const parsed = parseEntityLink(canonicalHref);
  if (!parsed.ok) return null;
  const presentation = entityLinkPresentation(parsed.value);

  const inlineLink = (metadata?: LinkPreviewMetadata | null) => {
    const resolvedContext = metadata?.title.trim();
    const ariaLabel = resolvedContext
      ? `${presentation.ariaLabel}: ${resolvedContext}`
      : presentation.ariaLabel;
    return (
      <BuzzInlineLink
        href={href}
        title={href}
        aria-label={ariaLabel}
        interactive={interactive}
        onOpenLink={() => onOpenEntityLink(parsed.value)}
      >
        {children}
      </BuzzInlineLink>
    );
  };

  if (!asChip) {
    return interactive ? (
      <EntityMetadataTooltip
        fallback={presentation.label}
        footer={presentation.tooltipFooter}
        href={canonicalHref}
        link={parsed.value}
        onMetadataIntent={onMetadataIntent}
        projects={projects}
      >
        {inlineLink}
      </EntityMetadataTooltip>
    ) : (
      inlineLink()
    );
  }

  const chip = (metadata?: LinkPreviewMetadata | null) => {
    const resolvedContext = metadata?.title.trim();
    // Fetched metadata belongs in the tooltip and accessible name, never the
    // visible chip. PR and issue chips share the repository-name label so their
    // inline width stays stable before, during, and after resolution.
    const chipLabel =
      parsed.value.type === "pr" || parsed.value.type === "issue"
        ? parsed.value.dtag
        : presentation.label;
    const ariaLabel =
      resolvedContext &&
      (parsed.value.type === "pr" || parsed.value.type === "issue")
        ? `${presentation.ariaLabel}: ${parsed.value.dtag} · ${resolvedContext}`
        : presentation.ariaLabel;
    return (
      <BuzzLinkChip
        data-buzz-link-kind={parsed.value.type}
        href={href}
        icon={presentation.icon}
        aria-label={ariaLabel}
        interactive={interactive}
        onOpenLink={() => onOpenEntityLink(parsed.value)}
        wrapping
      >
        {chipLabel}
      </BuzzLinkChip>
    );
  };
  return interactive ? (
    <EntityMetadataTooltip
      fallback={presentation.label}
      footer={presentation.tooltipFooter}
      href={canonicalHref}
      link={parsed.value}
      onMetadataIntent={onMetadataIntent}
      projects={projects}
    >
      {chip}
    </EntityMetadataTooltip>
  ) : (
    chip()
  );
}
