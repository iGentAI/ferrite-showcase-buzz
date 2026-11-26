import assert from "node:assert/strict";
import test from "node:test";

import {
  buildTeamMentionCandidates,
  formatTeamMention,
  orderMentionCandidatesByRecency,
  profileRecency,
} from "./mentionCandidates.ts";
import { fromRawUserProfileSummary } from "@/shared/api/tauriProfiles.ts";
import { rankMentionCandidates } from "./mentionRanking.ts";

function persona(id, displayName, isActive = true) {
  return {
    id,
    displayName,
    avatarUrl: null,
    systemPrompt: `${displayName} prompt`,
    runtime: null,
    model: null,
    provider: null,
    namePool: [],
    isBuiltIn: false,
    isActive,
    envVars: {},
    respondTo: null,
    respondToAllowlist: [],
    parallelism: null,
    createdAt: "2026-01-01T00:00:00Z",
    updatedAt: "2026-01-01T00:00:00Z",
  };
}

function team(id, personaIds, overrides = {}) {
  return {
    id,
    name: "Launch Team",
    description: null,
    instructions: null,
    personaIds,
    isBuiltin: false,
    sourceDir: null,
    isSymlink: false,
    symlinkTarget: null,
    version: null,
    createdAt: "2026-01-01T00:00:00Z",
    updatedAt: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function identity(personaId, displayName, overrides = {}) {
  return {
    kind: "identity",
    personaId,
    displayName,
    isAgent: true,
    isMember: false,
    ...overrides,
  };
}

test("team mentions preserve team order and prefer concrete managed agents", () => {
  const personas = [
    persona("planner", "Planner"),
    persona("builder", "Builder"),
    persona("reviewer", "Reviewer"),
  ];
  const candidates = [
    identity("builder", "Build Bot", {
      isManagedAgent: true,
      pubkey: "2".repeat(64),
    }),
    identity("planner", "Plan Bot", {
      isManagedAgent: true,
      pubkey: "1".repeat(64),
    }),
    identity("planner", "Planner in channel", {
      isMember: true,
      pubkey: "3".repeat(64),
    }),
  ];

  const [suggestion] = buildTeamMentionCandidates(
    [team("launch", ["planner", "builder", "reviewer"])],
    personas,
    candidates,
  );

  assert.equal(suggestion.kind, "team");
  assert.deepEqual(suggestion.teamMembers, [
    {
      displayName: "Planner in channel",
      kind: "identity",
      personaId: "planner",
      pubkey: "3".repeat(64),
    },
    {
      displayName: "Build Bot",
      kind: "identity",
      personaId: "builder",
      pubkey: "2".repeat(64),
    },
    {
      displayName: "Reviewer",
      kind: "persona",
      personaId: "reviewer",
    },
  ]);
  assert.equal(
    formatTeamMention(suggestion.displayName, suggestion.teamMembers),
    "Launch Team(@Planner in channel @Build Bot @Reviewer) ",
  );
});

test("only complete, owned teams with mentionable members are suggested", () => {
  const active = persona("active", "Active");
  const inactive = persona("inactive", "Inactive", false);
  const teams = [
    team("owned", ["active"]),
    team("builtin", ["active"], { isBuiltin: true }),
    team("missing", ["missing"]),
    team("inactive", ["inactive"]),
  ];

  assert.deepEqual(
    buildTeamMentionCandidates(teams, [active, inactive], []).map(
      (candidate) => candidate.teamId,
    ),
    ["owned"],
  );
});

test("teams with duplicate identity display names remain selectable with their exact keys", () => {
  const personas = [
    persona("builder-one", "First"),
    persona("builder-two", "Second"),
  ];
  const candidates = [
    identity("builder-one", "Builder", { pubkey: "1".repeat(64) }),
    identity("builder-two", "Builder", { pubkey: "2".repeat(64) }),
  ];

  const suggestions = buildTeamMentionCandidates(
    [team("duplicate-identities", ["builder-one", "builder-two"])],
    personas,
    candidates,
  );
  assert.deepEqual(
    suggestions[0].teamMembers.map((member) => member.pubkey),
    ["1".repeat(64), "2".repeat(64)],
  );
});

test("teams with identity and persona display-name collisions are not suggested", () => {
  const personas = [
    persona("managed-builder", "Managed Builder"),
    persona("persona-builder", "builder"),
  ];
  const candidates = [
    identity("managed-builder", "Builder", { pubkey: "1".repeat(64) }),
  ];

  assert.deepEqual(
    buildTeamMentionCandidates(
      [
        team("identity-persona-collision", [
          "managed-builder",
          "persona-builder",
        ]),
      ],
      personas,
      candidates,
    ),
    [],
  );
});

/* ------------------------------------------------------------------------- */
/* orderMentionCandidatesByRecency                                           */
/* ------------------------------------------------------------------------- */

/** One roster member, as useMentions builds them from a channel's 39002. */
function memberCandidate(pubkey, displayName, profileCreatedAt = null) {
  return {
    kind: "identity",
    pubkey,
    displayName,
    isMember: true,
    isAgent: false,
    profileCreatedAt,
  };
}

/**
 * #general on the live dev relay: 580 members, 134 of them named
 * "Bo Nilsson <run-stamp>" from earlier runs of the same scenario. Roster order
 * is pubkey order, so this run's Bo sits at an arbitrary index among them.
 */
function crowdedRoster(livePubkey) {
  const bos = Array.from({ length: 134 }, (_, i) =>
    memberCandidate(`bo${String(i).padStart(60, "0")}`, `Bo Nilsson run-${i}`),
  );
  // The colleague who just onboarded: newest kind 0 of the lot, buried at 77 —
  // the position measured on the live relay, well past the 50-item slice.
  bos[77] = memberCandidate(livePubkey, "Bo Nilsson ms5largh-5xu9", 1_800_000);
  return bos;
}

const rankedLabels = (candidates, query) =>
  rankMentionCandidates(candidates, query, new Set())
    .slice(0, 50)
    .map(({ candidate }) => candidate.pubkey);

test("orderMentionCandidatesByRecency: the just-onboarded colleague survives the 50-item slice", () => {
  const live = "live".padEnd(64, "0");
  const roster = crowdedRoster(live);

  // Teeth first: in roster order the live Bo is cut, which is scenario 5's
  // "@mention reaches the person it names" failing on the live relay.
  assert.equal(
    rankedLabels(roster, "Bo").includes(live),
    false,
    "roster order must be the failing baseline, or this test proves nothing",
  );

  const shown = rankedLabels(orderMentionCandidatesByRecency(roster), "Bo");
  assert.equal(shown.includes(live), true);
  assert.equal(shown[0], live, "and it is the first suggestion, not merely present");
});

test("orderMentionCandidatesByRecency: undated candidates keep their relative order", () => {
  const a = memberCandidate("a".repeat(64), "Ana");
  const b = memberCandidate("b".repeat(64), "Bo");
  const c = memberCandidate("c".repeat(64), "Cy");
  assert.deepEqual(
    orderMentionCandidatesByRecency([a, b, c]).map((x) => x.displayName),
    ["Ana", "Bo", "Cy"],
    "a workspace with no duplicate identities is ordered exactly as before",
  );
});

test("orderMentionCandidatesByRecency: dated candidates sort newest-first, ahead of undated ones", () => {
  const undated = memberCandidate("a".repeat(64), "Ana");
  const older = memberCandidate("b".repeat(64), "Bo", 100);
  const newer = memberCandidate("c".repeat(64), "Cy", 900);
  assert.deepEqual(
    orderMentionCandidatesByRecency([undated, older, newer]).map(
      (x) => x.displayName,
    ),
    ["Cy", "Bo", "Ana"],
  );
});

test("orderMentionCandidatesByRecency: reordering does not promote a non-member over a member", () => {
  // Ranking groups by membership BEFORE it looks at input order, so re-ordering
  // the input can only ever break ties inside a group.
  const outsider = {
    kind: "identity",
    pubkey: "d".repeat(64),
    displayName: "Bo Outsider",
    isMember: false,
    isAgent: false,
    profileCreatedAt: 9_999_999,
  };
  const member = memberCandidate("e".repeat(64), "Bo Member", 1);
  const ranked = rankMentionCandidates(
    orderMentionCandidatesByRecency([outsider, member]),
    "Bo",
    new Set(),
  );
  assert.deepEqual(
    ranked.map(({ candidate }) => candidate.displayName),
    ["Bo Member", "Bo Outsider"],
  );
});

test("orderMentionCandidatesByRecency: the input array is not mutated", () => {
  const input = [
    memberCandidate("a".repeat(64), "Ana", 1),
    memberCandidate("b".repeat(64), "Bo", 2),
  ];
  orderMentionCandidatesByRecency(input);
  assert.deepEqual(
    input.map((x) => x.displayName),
    ["Ana", "Bo"],
  );
});

/* ------------------------------------------------------------------------- */
/* Recency reaches a candidate built from the ROSTER, not just from search    */
/* ------------------------------------------------------------------------- */

/** A row exactly as `get_users_batch` returns it from the browser backend. */
const rawBatchRow = (displayName, createdAt) => ({
  display_name: displayName,
  name: null,
  avatar_url: null,
  nip05_handle: null,
  owner_pubkey: null,
  is_agent: false,
  created_at: createdAt,
});

test("profileRecency: reads the batch profile's kind-0 timestamp", () => {
  assert.equal(profileRecency({ createdAt: 1_700_000_000 }), 1_700_000_000);
  assert.equal(profileRecency({ createdAt: null }), null);
  assert.equal(profileRecency({}), null, "the desktop mirror reports no timestamp");
  assert.equal(profileRecency(null), null);
  assert.equal(profileRecency(undefined), null);
});

test("fromRawUserProfileSummary: carries created_at through to the picker", () => {
  // The one place a member's recency can be dropped between backend and UI.
  assert.equal(
    fromRawUserProfileSummary(rawBatchRow("Bo Nilsson", 1_700_000_000)).createdAt,
    1_700_000_000,
  );
  const { created_at: _drop, ...desktopRow } = rawBatchRow("Bo Nilsson", 1);
  assert.equal(
    fromRawUserProfileSummary(desktopRow).createdAt,
    null,
    "a desktop row without the field stays null rather than undefined",
  );
});

test("two channel members sharing one display name resolve to the LIVE identity", () => {
  // The real product bug: someone re-registers, an agent is recreated, or two
  // people genuinely share a name. All three are members, so nothing collapses
  // them and nothing but recency can tell them apart. Roster order lists the
  // dead one first — that is pubkey order, i.e. chance.
  const dead = "d".repeat(64);
  const older = "0".repeat(64);
  const live = "1".repeat(64);
  const batch = {
    [dead]: rawBatchRow("Bo Nilsson", 100),
    [older]: rawBatchRow("Bo Nilsson", 500),
    [live]: rawBatchRow("Bo Nilsson", 900),
  };

  // The chain the app runs: batch row -> profile summary -> member candidate.
  const profiles = Object.fromEntries(
    Object.entries(batch).map(([pk, row]) => [pk, fromRawUserProfileSummary(row)]),
  );
  const rosterOrder = [dead, older, live];
  const candidates = rosterOrder.map((pk) => ({
    kind: "identity",
    pubkey: pk,
    displayName: profiles[pk].displayName,
    isMember: true,
    isAgent: false,
    profileCreatedAt: profileRecency(profiles[pk]),
  }));

  const shown = rankMentionCandidates(
    orderMentionCandidatesByRecency(candidates),
    "Bo",
    new Set(),
  ).slice(0, 50);

  assert.equal(
    shown[0].candidate.pubkey,
    live,
    "the @mention must name the identity its owner most recently used",
  );
  assert.equal(shown.length, 3, "the other two are still offered, not collapsed away");
});

// kind-0 `created_at` is chosen by the identity's owner. A future-dated profile
// is ranked as if written now, so it cannot out-rank a genuine profile updated
// at the same moment. (F158)
test("orderMentionCandidatesByRecency: a future-dated profile claims no more than now", () => {
  const now = 1_800_000_000;
  const genuine = {
    displayName: "Alice",
    pubkey: "a".repeat(64),
    profileCreatedAt: now,
  };
  const impostor = {
    displayName: "Alice",
    pubkey: "b".repeat(64),
    profileCreatedAt: now + 899,
  };
  assert.deepEqual(
    orderMentionCandidatesByRecency([genuine, impostor], now).map(
      (candidate) => candidate.pubkey,
    ),
    [genuine.pubkey, impostor.pubkey],
    "a tie at 'now' keeps input order instead of promoting the future date",
  );
});

// Anyone can mint same-named outsider profiles with fresh timestamps. Ranking
// outsiders by that self-asserted recency let a batch of impostors push an
// older genuine outsider past the 50-row picker cap; outsiders now keep the
// search's own order. (F158, review follow-up)
test("orderMentionCandidatesByRecency: fresh impostor outsiders cannot push an older genuine outsider past the cap", () => {
  const now = 1_800_000_000;
  const outsider = (pubkey, profileCreatedAt) => ({
    kind: "identity",
    pubkey,
    displayName: "Alice",
    isMember: false,
    isAgent: false,
    isGlobalSearchResult: true,
    profileCreatedAt,
  });
  const genuine = outsider("a".repeat(64), now - 400 * 86_400);
  const impostors = Array.from({ length: 60 }, (_, index) =>
    outsider(index.toString(16).padStart(64, "0"), now + 899),
  );
  const shown = rankMentionCandidates(
    orderMentionCandidatesByRecency([genuine, ...impostors], now),
    "Alice",
    new Set(),
  ).slice(0, 50);
  assert.ok(
    shown.some((row) => row.candidate.pubkey === genuine.pubkey),
    "the genuine outsider stays inside the 50-row cap",
  );
});
