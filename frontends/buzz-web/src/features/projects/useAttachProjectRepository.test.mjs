import assert from "node:assert/strict";
import test from "node:test";

import { buildLegacyProjectPromotionTemplate } from "./useAttachProjectRepository.ts";

const OWNER = "a".repeat(64);

test("a legacy card is promoted under its own d-tag with both repositories", () => {
  const template = buildLegacyProjectPromotionTemplate(
    {
      dtag: "legacy-repo",
      name: "Legacy Repo",
      description: "",
      projectChannelId: "11111111-1111-4111-8111-111111111111",
      repositoryAddresses: [`30617:${OWNER}:legacy-repo`],
    },
    { repoAddress: `30617:${OWNER}:second` },
    OWNER,
  );
  assert.equal(template.kind, 30621);
  assert.deepEqual(
    template.tags.filter((tag) => tag[0] === "d"),
    [["d", "legacy-repo"]],
  );
  assert.deepEqual(
    template.tags.filter((tag) => tag[0] === "a").map((tag) => tag[1]).sort(),
    [`30617:${OWNER}:legacy-repo`, `30617:${OWNER}:second`],
  );
});

test("a legacy card without a home channel says so", () => {
  assert.throws(
    () =>
      buildLegacyProjectPromotionTemplate(
        { dtag: "x", name: "X", description: "", projectChannelId: null, repositoryAddresses: [] },
        { repoAddress: `30617:${OWNER}:y` },
        OWNER,
      ),
    /home channel/,
  );
});
