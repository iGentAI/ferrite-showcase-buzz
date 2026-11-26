import assert from "node:assert/strict";
import test from "node:test";

import {
  absorbStandaloneProjectRepositories,
  homeRepositoriesToBind,
} from "./projectCollection.ts";

const OWNER = "a".repeat(64);
const AGENT = "b".repeat(64);
const CHANNEL = "11111111-1111-4111-8111-111111111111";

function explicitProject(overrides = {}) {
  return {
    id: `30621:${OWNER}:space-invaders-3d`,
    dtag: "space-invaders-3d",
    name: "Space Invaders 3D",
    description: "Recreating Space Invaders the Game but in 3D",
    owner: OWNER,
    createdAt: 100,
    projectChannelId: CHANNEL,
    relatedChannelIds: [],
    status: "active",
    projectAddress: `30621:${OWNER}:space-invaders-3d`,
    primaryRepositoryAddress: null,
    repositoryAddresses: [],
    repositoryRelayHints: {},
    repositories: [],
    unavailableRepositoryAddresses: [],
    visibility: "listed",
    legacy: false,
    ...overrides,
  };
}

function standaloneRepo(overrides = {}) {
  const owner = overrides.owner ?? AGENT;
  const dtag = overrides.dtag ?? "space-invaders-3d";
  const repoAddress = `30617:${owner}:${dtag}`;
  const repository = {
    id: `${owner}:${dtag}`,
    dtag,
    name: "Space Invaders 3D",
    description: "A 3D remake of Space Invaders built with three.js",
    cloneUrls: [],
    webUrl: null,
    owner,
    contributors: [owner],
    createdAt: 200,
    status: "active",
    defaultBranch: "main",
    repoAddress,
    channelId: CHANNEL,
    ...overrides.repository,
  };
  return {
    id: repoAddress,
    dtag,
    name: repository.name,
    description: repository.description,
    owner,
    createdAt: 200,
    projectChannelId: null,
    relatedChannelIds: [],
    status: "active",
    projectAddress: repoAddress,
    primaryRepositoryAddress: repoAddress,
    repositoryAddresses: [repoAddress],
    repositoryRelayHints: {},
    repositories: [repository],
    unavailableRepositoryAddresses: [],
    visibility: "listed",
    legacy: true,
    ...overrides.card,
  };
}

test("absorbStandaloneProjectRepositories folds an owner-delegated agent home-channel repo into the project", () => {
  const project = explicitProject();
  const repoCard = standaloneRepo({ repository: { maintainers: [OWNER] } });
  const folded = absorbStandaloneProjectRepositories([project, repoCard], {
    authorizedAgentPubkeysByOwner: new Map([[OWNER, new Set([AGENT])]]),
  });

  assert.equal(folded.length, 1);
  assert.equal(folded[0].legacy, false);
  assert.equal(folded[0].repositories.length, 1);
  assert.equal(folded[0].repositories[0].repoAddress, repoCard.projectAddress);
  assert.equal(folded[0].repositoryAddresses[0], repoCard.projectAddress);
});

test("absorbStandaloneProjectRepositories rejects a hostile home-channel claim", () => {
  const project = explicitProject();
  const repoCard = standaloneRepo();
  const folded = absorbStandaloneProjectRepositories([project, repoCard]);

  assert.equal(folded.length, 2);
  assert.deepEqual(folded[0].repositories, []);
  assert.equal(folded[1].projectAddress, repoCard.projectAddress);
});

test("absorbStandaloneProjectRepositories folds the owner's same-slug repo", () => {
  const project = explicitProject();
  const repoCard = standaloneRepo({
    owner: OWNER,
    repository: { channelId: null },
  });
  const folded = absorbStandaloneProjectRepositories([project, repoCard]);

  assert.equal(folded.length, 1);
  assert.equal(folded[0].repositories[0].owner, OWNER);
});

test("absorbStandaloneProjectRepositories keeps an unrelated standalone repo", () => {
  const project = explicitProject();
  const repoCard = standaloneRepo({
    dtag: "other-game",
    owner: AGENT,
    repository: { channelId: null, dtag: "other-game" },
  });
  const folded = absorbStandaloneProjectRepositories([project, repoCard]);

  assert.equal(folded.length, 2);
  assert.equal(
    folded.some((item) => item.legacy),
    true,
  );
});

test("homeRepositoriesToBind lists owner-delegated absorbed channel repos missing from the signed project", () => {
  const repo = standaloneRepo({ repository: { maintainers: [OWNER] } });
  const project = explicitProject({
    repositories: [repo.repositories[0]],
    repositoryAddresses: [repo.projectAddress],
  });
  const pending = homeRepositoriesToBind(project, [], {
    authorizedAgentPubkeysByOwner: new Map([[OWNER, new Set([AGENT])]]),
  });
  assert.equal(pending.length, 1);
  assert.equal(pending[0].repoAddress, repo.projectAddress);
});

test("homeRepositoriesToBind rejects a hostile absorbed channel repo", () => {
  const repo = standaloneRepo();
  const project = explicitProject({
    repositories: [repo.repositories[0]],
    repositoryAddresses: [repo.projectAddress],
  });

  assert.deepEqual(homeRepositoriesToBind(project, []), []);
});

test("homeRepositoriesToBind ignores repos already on the signed project", () => {
  const repo = standaloneRepo().repositories[0];
  const project = explicitProject({
    repositories: [repo],
    repositoryAddresses: [repo.repoAddress],
  });
  assert.equal(homeRepositoriesToBind(project, [repo.repoAddress]).length, 0);
});

test("unverified third-party repository claiming maintainers=[owner] and buzz-channel is rejected by absorbStandaloneProjectRepositories and homeRepositoriesToBind", () => {
  const STRANGER = "9".repeat(64);
  const project = explicitProject();
  const spoofedRepoCard = standaloneRepo({
    owner: STRANGER,
    repository: {
      owner: STRANGER,
      maintainers: [OWNER],
      channelId: CHANNEL,
      cloneUrls: ["https://attacker.example/hijack.git"],
    },
  });

  const folded = absorbStandaloneProjectRepositories([project, spoofedRepoCard]);
  assert.equal(
    folded.length,
    2,
    "unverified stranger kind:30617 with self-authored maintainers=[owner] must NOT be absorbed into the owner's project",
  );
  assert.deepEqual(folded[0].repositories, []);
  assert.equal(folded[0].primaryRepositoryAddress, null);

  const projectWithSpoof = explicitProject({
    repositories: [spoofedRepoCard.repositories[0]],
    repositoryAddresses: [spoofedRepoCard.projectAddress],
  });
  assert.deepEqual(
    homeRepositoriesToBind(projectWithSpoof, []),
    [],
    "homeRepositoriesToBind must refuse to auto-sign third-party repository coordinates without verified owner delegation",
  );
});
