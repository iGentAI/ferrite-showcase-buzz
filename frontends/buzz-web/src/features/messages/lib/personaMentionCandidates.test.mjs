import assert from "node:assert/strict";
import test from "node:test";

import { buildMentionCandidates } from "./buildMentionCandidates.ts";
import { personaMentionCandidates } from "./personaMentionCandidates.ts";

const NO_MANAGED_AGENTS = new Set();
const REVKA = "1d8c139016cdc19b90de2d0f8fab37606c176a02b570722449e48f08165a5da7";
const NOVA = "fc4607b754faf164cb7da4ba516cb301b9d495b7ab97eac72d6bb077ab82a2a0";

// THE REGRESSION TEST. A cloud agent reaches the picker only as a PERSONA — its relay-agent
// identity is filtered out upstream, because the roster reports no channel scope for it. And a
// persona mention cannot be sent from a browser: the composer reads it as a request to provision
// a LOCAL harness agent, finds no runtime, and abandons the send without even asking. So the row
// has to survive AND carry a pubkey.
test("a persona backed by a relay agent resolves to that agent's pubkey", () => {
  const [candidate] = personaMentionCandidates(
    [{ id: "revka", displayName: "revka" }],
    NO_MANAGED_AGENTS,
    [{ name: "revka", pubkey: REVKA }],
  );
  assert.equal(candidate.kind, "identity", "an identity mention is the sendable one");
  assert.equal(candidate.pubkey, REVKA, "and it carries the p tag's pubkey");
  assert.equal(candidate.personaId, undefined, "so nothing tries to provision a runtime");
  assert.equal(candidate.displayName, "revka", "the picker still reads the same");
});

test("a purely local persona stays a persona", () => {
  // Correct on a desktop build, where provisioning a local agent is what a persona is FOR.
  const [candidate] = personaMentionCandidates(
    [{ id: "local-helper", displayName: "Local Helper" }],
    NO_MANAGED_AGENTS,
    [{ name: "revka", pubkey: REVKA }],
  );
  assert.equal(candidate.kind, "persona");
  assert.equal(candidate.personaId, "local-helper");
  assert.equal(candidate.pubkey, undefined);
});

test("nothing is lost: every persona still yields a row", () => {
  // The first attempt at this fix DELETED the relay-backed rows, which made cloud teammates
  // unmentionable — worse than the unsendable mention it was fixing.
  const candidates = personaMentionCandidates(
    [
      { id: "revka", displayName: "revka" },
      { id: "local", displayName: "Local" },
    ],
    NO_MANAGED_AGENTS,
    [{ name: "revka", pubkey: REVKA }],
  );
  assert.equal(candidates.length, 2, "both are offered");
  assert.deepEqual(
    candidates.map((c) => c.kind),
    ["identity", "persona"],
  );
});

test("resolution survives case and padding", () => {
  // One side is a relay-generated agent id, the other a display name someone typed. The persona
  // is bound to the agent (its id is the agent's registry key), as a cloud teammate's is.
  const [candidate] = personaMentionCandidates(
    [{ id: "ReVkA", displayName: "  ReVkA " }],
    NO_MANAGED_AGENTS,
    [{ name: "revka", pubkey: REVKA }],
  );
  assert.equal(candidate.pubkey, REVKA);
});

test("resolution is equality, not resemblance", () => {
  // A local persona whose name merely CONTAINS an agent's must not be handed that agent's pubkey
  // — it would silently address the wrong teammate, which is worse than not resolving at all.
  const candidates = personaMentionCandidates(
    [
      { id: "rev", displayName: "Reviewer" },
      { id: "revka-helper", displayName: "revka helper" },
    ],
    NO_MANAGED_AGENTS,
    [
      { name: "revka", pubkey: REVKA },
      { name: "nova-helper", pubkey: NOVA },
    ],
  );
  assert.deepEqual(
    candidates.map((c) => c.kind),
    ["persona", "persona"],
    "neither name is an exact match, so neither is resolved",
  );
});

test("an archived pubkey is not a resolution", () => {
  // Resolution is the only reason a pubkey-level check applies to these rows at all — they
  // carried no pubkey before. Pointing a mention at an identity its owner discarded would
  // address a key nobody reads, so the row survives as a persona instead.
  const [candidate] = personaMentionCandidates(
    [{ id: "revka", displayName: "revka" }],
    NO_MANAGED_AGENTS,
    [{ name: "revka", pubkey: REVKA }],
    (pubkey) => pubkey === REVKA,
  );
  assert.equal(candidate.kind, "persona", "not resolved onto an archived identity");
  assert.equal(candidate.pubkey, undefined);
  assert.equal(candidate.displayName, "revka", "but still offered, exactly as before");
});

test("an unarchived pubkey still resolves", () => {
  const [candidate] = personaMentionCandidates(
    [{ id: "revka", displayName: "revka" }],
    NO_MANAGED_AGENTS,
    [{ name: "revka", pubkey: REVKA }],
    (pubkey) => pubkey === NOVA,
  );
  assert.equal(candidate.pubkey, REVKA, "a predicate that says no must not block it");
});

test("a persona backing a managed agent is excluded", () => {
  // Pre-existing rule, preserved: that agent is mentionable by pubkey already, and mentioning
  // the persona would try to start a second one.
  const candidates = personaMentionCandidates(
    [
      { id: "packaged", displayName: "Packaged" },
      { id: "running", displayName: "Running" },
    ],
    new Set(["running"]),
    [],
  );
  assert.deepEqual(
    candidates.map((c) => c.personaId),
    ["packaged"],
  );
});

test("a nameless persona is not offered", () => {
  // It would render as an unpickable blank row.
  const candidates = personaMentionCandidates(
    [
      { id: "blank", displayName: "   " },
      { id: "named", displayName: "Named" },
    ],
    NO_MANAGED_AGENTS,
    [],
  );
  assert.deepEqual(
    candidates.map((c) => c.personaId),
    ["named"],
  );
});

test("a malformed roster row resolves nothing and does not throw", () => {
  const candidates = personaMentionCandidates(
    [{ id: "one", displayName: "One" }],
    NO_MANAGED_AGENTS,
    [
      { name: "  ", pubkey: REVKA },
      { name: "one", pubkey: "" },
    ],
  );
  assert.equal(candidates[0].kind, "persona", "an empty pubkey is not a resolution");
});

test("a duplicated agent name keeps the first pubkey", () => {
  // Two agents sharing a name is a hand-edited registry; silently preferring the later one would
  // change who an existing mention addresses.
  const [candidate] = personaMentionCandidates(
    [{ id: "revka", displayName: "revka" }],
    NO_MANAGED_AGENTS,
    [
      { name: "revka", pubkey: REVKA },
      { name: "revka", pubkey: NOVA },
    ],
  );
  assert.equal(candidate.pubkey, REVKA);
});

// F386: resolution matched on display name alone, so a stranger's agent named like one of the
// viewer's local personas captured that row: it kept the persona's own name, showed no owner,
// and the mention addressed the stranger's key.
const VIEWER = "e".repeat(64);
const STRANGER = "f".repeat(64);

test("a stranger's same-named agent does not capture a local persona (F386)", () => {
  const [candidate] = personaMentionCandidates(
    [{ id: "4b1f0c2e-local-persona", displayName: "Reviewer" }],
    NO_MANAGED_AGENTS,
    [{ name: "reviewer", pubkey: NOVA, ownerPubkey: STRANGER }],
    () => false,
    { currentPubkey: VIEWER },
  );
  assert.equal(candidate.kind, "persona", "the local persona stays itself");
  assert.equal(candidate.pubkey, undefined, "and carries nobody's key");
  assert.equal(candidate.personaId, "4b1f0c2e-local-persona");
});

test("the viewer's own same-named agent still resolves, with its owner (F386)", () => {
  const [candidate] = personaMentionCandidates(
    [{ id: "4b1f0c2e-local-persona", displayName: "Reviewer" }],
    NO_MANAGED_AGENTS,
    [{ name: "reviewer", pubkey: NOVA, ownerPubkey: VIEWER.toUpperCase() }],
    () => false,
    { currentPubkey: VIEWER },
  );
  assert.equal(candidate.kind, "identity");
  assert.equal(candidate.pubkey, NOVA);
  assert.equal(candidate.ownerPubkey, VIEWER.toUpperCase(), "the picker can say whose it is");
});

test("an agent whose policy never answers the viewer is not a resolution (F386)", () => {
  const agent = { name: "revka", pubkey: REVKA, ownerPubkey: STRANGER };
  const seen = [];
  const [candidate] = personaMentionCandidates(
    [{ id: "revka", displayName: "revka" }],
    NO_MANAGED_AGENTS,
    [agent],
    () => false,
    {
      currentPubkey: VIEWER,
      admitsViewer: (candidateAgent) => {
        seen.push(candidateAgent);
        return false;
      },
    },
  );
  assert.equal(candidate.kind, "persona");
  assert.equal(candidate.pubkey, undefined);
  assert.deepEqual(seen, [agent], "the policy sees the directory row itself");
});

test("mention candidates apply the respond-to policy to persona resolution (F386)", () => {
  const base = {
    activeAgentPubkeys: new Set(),
    activePersonaById: new Map(),
    canSearchGlobalUsers: false,
    currentPubkey: VIEWER,
    isArchived: () => false,
    managedAgentDirectoryReady: true,
    managedAgentNamesByPubkey: new Map(),
    managedAgentPersonaIds: new Set(),
    managedAgentPersonaIdsByPubkey: new Map(),
    managedAgents: [],
    memberPubkeys: new Set(),
    members: [],
    mentionChannelId: "room",
    mentionableAgentPubkeys: new Set(),
    personaNameByPubkey: new Map(),
    profiles: undefined,
    relayAgentDirectoryReady: true,
    relayAgentNamesByPubkey: new Map(),
    userSearchResults: [],
  };
  const relayAgent = (name, pubkey, respondTo, extra = {}) => ({
    pubkey,
    ownerPubkey: STRANGER,
    name,
    agentType: "ferrite-cloud",
    channels: [],
    channelIds: [],
    capabilities: [],
    status: "online",
    respondTo,
    respondToAllowlist: [],
    ...extra,
  });
  const personas = [
    { id: "revka", displayName: "revka", isActive: true },
    { id: "nova", displayName: "nova", isActive: true },
    { id: "listed", displayName: "listed", isActive: true },
  ];
  const listed = "1".repeat(64);
  const byPersona = (candidates) =>
    Object.fromEntries(
      candidates.map((candidate) => [
        candidate.displayName,
        candidate.kind === "identity" ? candidate.pubkey : "persona",
      ]),
    );

  const candidates = buildMentionCandidates({
    ...base,
    activePersonas: personas,
    relayAgents: [
      // Outside this room, answering anyone: resolves, and the invite prompt handles the room.
      relayAgent("revka", REVKA, "anyone"),
      // Someone else's owner-only agent never answers this viewer.
      relayAgent("nova", NOVA, "owner-only"),
      // An allowlist that names the viewer admits them.
      relayAgent("listed", listed, "allowlist", {
        respondToAllowlist: [VIEWER],
      }),
    ],
  });
  assert.deepEqual(byPersona(candidates), {
    revka: REVKA,
    nova: "persona",
    listed,
  });
  const revka = candidates.find((candidate) => candidate.pubkey === REVKA);
  assert.equal(revka.ownerPubkey, STRANGER);
});

test("an archived or unadmitted namesake listed first does not shadow the live agent", () => {
  const archived = "a".repeat(64);
  const [candidate] = personaMentionCandidates(
    [{ id: "revka", displayName: "revka" }],
    NO_MANAGED_AGENTS,
    [
      { name: "revka", pubkey: archived },
      { name: "revka", pubkey: ` ${REVKA.toUpperCase()} ` },
    ],
    (pubkey) => pubkey === archived,
  );
  assert.equal(candidate.kind, "identity");
  assert.equal(candidate.pubkey, REVKA, "the live agent, normalised");
});

test("a whitespace-only pubkey is not an agent", () => {
  const [candidate] = personaMentionCandidates(
    [{ id: "revka", displayName: "revka" }],
    NO_MANAGED_AGENTS,
    [{ name: "revka", pubkey: "   " }],
  );
  assert.equal(candidate.kind, "persona");
});
