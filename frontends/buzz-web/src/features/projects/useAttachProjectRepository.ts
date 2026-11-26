import { useMutation, useQueryClient } from "@tanstack/react-query";

import {
  type Project,
  projectsQueryKey,
  type Repository,
} from "@/features/projects/hooks";
import { publishOwnedAgentProjectAnnouncements } from "@/features/projects/projectOwnerControl";
import { buildProjectAnnouncementTemplate } from "@/features/projects/projectCreation";
import { addRepositoryToProject } from "@/features/projects/projectModels";
import { buildProjectPatchTemplate } from "@/features/projects/projectRepositoryCreation";
import { markProjectDataAuthoritative } from "@/features/projects/projectSnapshot";
import { publishProjectOwnerAnnouncement } from "@/shared/api/projectGit";
import { relayClient } from "@/shared/api/relayClient";
import { KIND_PROJECT_ANNOUNCEMENT } from "@/shared/constants/kinds";

export type AttachProjectRepositoryInput = {
  ownerControlAgentPubkey?: string;
  project: Project;
  repository: Repository;
};

type ProjectEventTemplate = ReturnType<typeof buildProjectPatchTemplate>;

/**
 * The first project head for a legacy repository card: its name, home channel
 * and description, its repositories plus the one being attached, under the
 * card's own d-tag so `addRepositoryToProject` promotes the same address.
 */
export function buildLegacyProjectPromotionTemplate(
  project: Project,
  repository: { repoAddress: string },
  ownerPubkey: string,
): ProjectEventTemplate {
  if (!project.projectChannelId) {
    throw new Error(
      "This repository needs a home channel before it can become a project.",
    );
  }
  const repositoryAddresses = [
    ...new Set([...project.repositoryAddresses, repository.repoAddress]),
  ];
  const { project: announcement } = buildProjectAnnouncementTemplate({
    description: project.description,
    name: project.name,
    ownerPubkey,
    projectChannelId: project.projectChannelId,
    repositoryAddresses,
  });
  return {
    ...announcement,
    tags: announcement.tags.map((tag) =>
      tag[0] === "d" ? ["d", project.dtag] : tag,
    ),
  } as ProjectEventTemplate;
}

async function attachProjectRepository({
  ownerControlAgentPubkey,
  project,
  repository,
}: AttachProjectRepositoryInput) {
  const targetOwner = project.owner.toLowerCase();

  // Fetch the live signed project head immediately before mutating.
  const liveHeads = await relayClient.fetchEvents({
    kinds: [KIND_PROJECT_ANNOUNCEMENT],
    authors: [targetOwner],
    "#d": [project.dtag],
    limit: 1,
  });
  const liveHead = liveHeads[0];
  let template: ProjectEventTemplate;
  let earliestCreatedAt = 0;
  if (!liveHead) {
    // A standalone (legacy) repository card has no project head yet: this
    // attach is what creates one, under the card's own d-tag, so the card is
    // promoted in place. A non-legacy project without a head is missing.
    if (!project.legacy) {
      throw new Error(
        "Could not find this project on the relay. Refresh and try again.",
      );
    }
    template = buildLegacyProjectPromotionTemplate(project, repository, targetOwner);
  } else {
    // Dominated-write guard. For a legacy card this is also what catches a
    // project head another session published since the card was read.
    if (liveHead.created_at > project.createdAt) {
      throw new Error(
        "This project was updated by another session while you were working. Refresh and try again.",
      );
    }

    const liveAddresses = liveHead.tags
      .filter((tag) => tag[0] === "a" && tag[1])
      .map((tag) => tag[1] as string);

    if (liveAddresses.includes(repository.repoAddress)) {
      throw new Error("This repository is already part of the project.");
    }
    template = buildProjectPatchTemplate({
      liveHead,
      ownerPubkey: targetOwner,
      repositoryAddresses: [...liveAddresses, repository.repoAddress],
    });
    earliestCreatedAt = liveHead.created_at + 1;
  }
  const createdAt = Math.max(Math.floor(Date.now() / 1_000), earliestCreatedAt);
  if (ownerControlAgentPubkey) {
    const [projectEvent] = await publishOwnedAgentProjectAnnouncements(
      ownerControlAgentPubkey,
      [{ ...template, createdAt }],
    );
    if (!projectEvent || projectEvent.kind !== KIND_PROJECT_ANNOUNCEMENT) {
      throw new Error(
        "The owner agent updated the project but its response was incomplete. Refresh and try again.",
      );
    }
    return {
      previousProjectId: project.id,
      project: addRepositoryToProject(
        project,
        repository,
        projectEvent.created_at,
      ),
      repository,
    };
  }

  const publication = await publishProjectOwnerAnnouncement({
    ...template,
    createdAt,
    targetOwner,
  });
  if (publication.publicationError) {
    throw new Error(publication.publicationError);
  }
  const projectEvent = publication.event;

  return {
    previousProjectId: project.id,
    project: addRepositoryToProject(
      project,
      repository,
      projectEvent.created_at,
    ),
    repository,
  };
}

export function useAttachProjectRepositoryMutation() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: attachProjectRepository,
    onSuccess: ({ previousProjectId, project }) => {
      markProjectDataAuthoritative(project, "local-write");
      if (previousProjectId !== project.id) {
        queryClient.removeQueries({
          exact: true,
          queryKey: ["project", previousProjectId],
        });
      }
      queryClient.setQueryData<Project[]>(projectsQueryKey, (current = []) =>
        current.map((candidate) =>
          candidate.id === previousProjectId ? project : candidate,
        ),
      );
      void queryClient.invalidateQueries({ queryKey: projectsQueryKey });
    },
  });
}
