import assert from "node:assert/strict";
import test from "node:test";

import { projectChannelApprovalAuthority } from "./useProjectChannelRequests.ts";

const OWNER = "a".repeat(64);
const VIEWER = "b".repeat(64);

test("a project owned by another of the viewer's managed agents can be approved locally", () => {
  assert.deepEqual(
    projectChannelApprovalAuthority({ owner: OWNER, ownerIsManaged: true, viewer: VIEWER, viewerOwnsOwnerAgent: true }),
    { allowed: true, ownerControlAgentPubkey: undefined },
  );
});

test("a remote owner agent of the viewer publishes through that agent", () => {
  assert.deepEqual(
    projectChannelApprovalAuthority({ owner: OWNER, ownerIsManaged: false, viewer: VIEWER, viewerOwnsOwnerAgent: true }),
    { allowed: true, ownerControlAgentPubkey: OWNER },
  );
});

test("someone else's project cannot be approved", () => {
  assert.equal(
    projectChannelApprovalAuthority({ owner: OWNER, ownerIsManaged: false, viewer: VIEWER, viewerOwnsOwnerAgent: false }).allowed,
    false,
  );
});
