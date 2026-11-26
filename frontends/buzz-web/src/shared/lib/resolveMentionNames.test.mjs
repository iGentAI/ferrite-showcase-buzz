import assert from "node:assert/strict";
import test from "node:test";

import {
  resolveMentionNames,
  resolveMentionProps,
  resolveMentionPubkeysByName,
} from "./resolveMentionNames.ts";

const PUBKEY = "a".repeat(64);
const OTHER_PUBKEY = "b".repeat(64);

function profile(overrides = {}) {
  return {
    displayName: null,
    name: null,
    avatarUrl: null,
    nip05Handle: null,
    ownerPubkey: null,
    ...overrides,
  };
}

test("returns undefined without tags or profiles", () => {
  const profiles = { [PUBKEY]: profile({ displayName: "alice" }) };
  assert.deepEqual(resolveMentionProps(undefined, profiles), {
    mentionNames: undefined,
    mentionPubkeysByName: undefined,
  });
  assert.deepEqual(resolveMentionProps([["p", PUBKEY]], undefined), {
    mentionNames: undefined,
    mentionPubkeysByName: undefined,
  });
});

test("resolves the display name alias from p tags", () => {
  const tags = [["p", PUBKEY]];
  const profiles = { [PUBKEY]: profile({ displayName: "Alice" }) };

  const { mentionNames, mentionPubkeysByName } = resolveMentionProps(
    tags,
    profiles,
  );
  assert.deepEqual(mentionNames, ["Alice"]);
  assert.deepEqual({ ...mentionPubkeysByName }, { alice: PUBKEY });
});

test("resolves the kind-0 name alias when it differs from the display name", () => {
  // The rename / agent-send case: the message text says "@tyler" (kind-0
  // name) while the profile's display name is "Tyler Durden". Both aliases
  // must render as chips AND resolve to the pubkey.
  const tags = [["p", PUBKEY]];
  const profiles = {
    [PUBKEY]: profile({ displayName: "Tyler Durden", name: "tyler" }),
  };

  const { mentionNames, mentionPubkeysByName } = resolveMentionProps(
    tags,
    profiles,
  );
  assert.deepEqual(mentionNames, ["Tyler Durden", "tyler"]);
  assert.deepEqual({ ...mentionPubkeysByName }, {
    "tyler durden": PUBKEY,
    tyler: PUBKEY,
  });
});

test("resolves the NIP-05 local part alias", () => {
  const tags = [["p", PUBKEY]];
  const profiles = {
    [PUBKEY]: profile({
      displayName: "Tyler Durden",
      nip05Handle: "tyler@buzz.example",
    }),
  };

  const { mentionNames, mentionPubkeysByName } = resolveMentionProps(
    tags,
    profiles,
  );
  assert.deepEqual(mentionNames, ["Tyler Durden", "tyler"]);
  assert.equal(mentionPubkeysByName?.tyler, PUBKEY);
});

test("skips the NIP-05 root identifier and blank aliases", () => {
  const tags = [
    ["p", PUBKEY],
    ["p", OTHER_PUBKEY],
  ];
  const profiles = {
    [PUBKEY]: profile({ displayName: "  ", name: "", nip05Handle: "_@root" }),
    [OTHER_PUBKEY]: profile({ displayName: "bob" }),
  };

  const { mentionNames, mentionPubkeysByName } = resolveMentionProps(
    tags,
    profiles,
  );
  assert.deepEqual(mentionNames, ["bob"]);
  assert.deepEqual({ ...mentionPubkeysByName }, { bob: OTHER_PUBKEY });
});

test("includes aliases from mention reference tags", () => {
  const tags = [["mention", PUBKEY]];
  const profiles = { [PUBKEY]: profile({ displayName: "alice" }) };

  assert.deepEqual(resolveMentionNames(tags, profiles), ["alice"]);
  assert.deepEqual({ ...resolveMentionPubkeysByName(tags, profiles) }, {
    alice: PUBKEY,
  });
});

test("every rendered name resolves to a pubkey (outputs stay in sync)", () => {
  const tags = [
    ["p", PUBKEY],
    ["p", OTHER_PUBKEY],
  ];
  const profiles = {
    [PUBKEY]: profile({
      displayName: "Tyler Durden",
      name: "tyler",
      nip05Handle: "td@buzz.example",
    }),
    [OTHER_PUBKEY]: profile({ displayName: "bob", name: "bobby" }),
  };

  const { mentionNames, mentionPubkeysByName } = resolveMentionProps(
    tags,
    profiles,
  );
  for (const name of mentionNames ?? []) {
    assert.ok(
      mentionPubkeysByName?.[name.toLowerCase()],
      `alias "${name}" renders as a chip but does not resolve to a pubkey`,
    );
  }
});

test("uppercases in tag pubkeys are normalized", () => {
  const tags = [["p", PUBKEY.toUpperCase()]];
  const profiles = { [PUBKEY]: profile({ displayName: "alice" }) };

  assert.deepEqual({ ...resolveMentionPubkeysByName(tags, profiles) }, {
    alice: PUBKEY,
  });
});

// --- F168: one message cannot drive unbounded mention work ---

import { MAX_QUALIFIED_MENTIONS } from "./resolveMentionNames.ts";

function floodOfQualifiedMentions(bytes = 96 * 1024) {
  let content = "";
  let count = 0;
  while (content.length < bytes) content += `@u${count++} (${PUBKEY}) `;
  return { content, count };
}

test("F168: a 96 KiB body of qualified mentions of one tagged key resolves quickly", () => {
  const { content, count } = floodOfQualifiedMentions();
  assert.ok(count > 1_000, "the attack body carries over a thousand mentions");
  const started = performance.now();
  const { mentionPubkeysByName } = resolveMentionProps(
    [["p", PUBKEY]],
    {},
    content,
  );
  const elapsed = performance.now() - started;
  // Took about 4.4 s on unchanged main.
  assert.ok(elapsed < 1_500, `took ${elapsed.toFixed(0)} ms`);
  assert.equal(
    Object.keys(mentionPubkeysByName ?? {}).length,
    MAX_QUALIFIED_MENTIONS,
  );
});

test("F168: qualified mentions within the cap still bind, repeated labels included", () => {
  const content = `hi @Scout (${PUBKEY}) and again @scout (${PUBKEY}) and @Other (${PUBKEY})`;
  const { mentionPubkeysByName } = resolveMentionProps(
    [["p", PUBKEY]],
    {},
    content,
  );
  assert.equal(mentionPubkeysByName?.[`scout (${PUBKEY})`], PUBKEY);
  assert.equal(mentionPubkeysByName?.[`other (${PUBKEY})`], PUBKEY);
});

test("F168: a qualified mention inside a code span still does not bind", () => {
  const content = `\`@Scout (${PUBKEY})\` but @Real (${PUBKEY})`;
  const { mentionPubkeysByName } = resolveMentionProps(
    [["p", PUBKEY]],
    {},
    content,
  );
  assert.equal(mentionPubkeysByName?.[`scout (${PUBKEY})`], undefined);
  assert.equal(mentionPubkeysByName?.[`real (${PUBKEY})`], PUBKEY);
});

test("finding_1bc8ad3e9dcb1efb: prototype-named aliases are plain keys, never inherited members", () => {
  const profiles = {
    [PUBKEY]: profile({ displayName: "constructor" }),
    [OTHER_PUBKEY]: profile({ displayName: "__proto__" }),
  };
  const byName = resolveMentionPubkeysByName([["p", PUBKEY], ["p", OTHER_PUBKEY]], profiles);
  assert.equal(byName.constructor, PUBKEY);
  assert.equal(byName.__proto__, OTHER_PUBKEY);
  assert.equal(byName.toString, undefined, "no inherited member resolves as a pubkey");
});
