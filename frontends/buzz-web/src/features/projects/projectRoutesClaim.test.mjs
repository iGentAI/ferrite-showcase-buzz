import assert from "node:assert/strict";
import test from "node:test";

import { projectMatchesRouteId } from "./projectRoutes.ts";

const OWNER = "a".repeat(64);
const STRANGER = "e".repeat(64);
const REPO = `30617:${OWNER}:repo`;
const repository = { repoAddress: REPO, owner: OWNER, maintainers: [] };
const project = (owner) => ({
  legacy: false,
  owner,
  dtag: "p",
  projectAddress: `30621:${owner}:p`,
  repositoryAddresses: [REPO],
  repositories: [repository],
});

test("finding_eb6a47ac19d86a62: a repository route resolves only to a project that claims it", () => {
  assert.equal(projectMatchesRouteId(project(OWNER), REPO), true);
  assert.equal(projectMatchesRouteId(project(STRANGER), REPO), false);
});
