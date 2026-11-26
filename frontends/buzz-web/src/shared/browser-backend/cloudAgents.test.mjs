import assert from "node:assert/strict";
import test from "node:test";

import { handleAppCommand } from "./commands.ts";
import {
  buildCloudAgentSnapshot,
  readRoomRosters,
  relayAgentRows,
  relayAgentsForMentionRevalidation,
} from "./cloudAgents.ts";
import {
  getAgentMentionAdmission,
  getMentionableAgentPubkeys,
} from "../../features/agents/lib/agentAutocompleteEligibility.ts";

const REVKA = "a".repeat(64);
const NOVA = "b".repeat(64);
const READER = "c".repeat(64);

function row(pubkey, name) {
  return {
    pubkey,
    name,
    agent_type: "ferrite-cloud",
    channels: [],
    channel_ids: [],
    capabilities: [],
    status: "online",
    respond_to: "anyone",
    respond_to_allowlist: [],
  };
}

/** What the synced client decides at publish time for one answered row set. */
function admitted(rows, channelId, pubkey) {
  return admission(rows, { type: "channel", channelId }, new Set([channelId]), "publish", pubkey);
}

function admission(rows, eligibilityScope, sharedChannelIds, phase, pubkey) {
  const relayAgents = rows.map((agent) => ({
    pubkey: agent.pubkey,
    ownerPubkey: null,
    name: agent.name,
    agentType: agent.agent_type,
    channels: agent.channels,
    channelIds: agent.channel_ids,
    capabilities: agent.capabilities,
    status: agent.status,
    respondTo: agent.respond_to ?? null,
    respondToAllowlist: agent.respond_to_allowlist ?? [],
  }));
  const mentionable = getMentionableAgentPubkeys({
    currentPubkey: READER,
    eligibilityScope,
    phase,
    managedAgentPubkeys: [],
    relayAgents,
    sharedChannelIds,
  });
  return getAgentMentionAdmission({
    isAgent: true,
    pubkey,
    mentionableAgentPubkeys: mentionable,
    directoryReady: true,
  });
}

test("a persona on the room's roster is admitted for a mention in that room", () => {
  const rows = relayAgentsForMentionRevalidation(
    [row(REVKA, "revka"), row(NOVA, "nova")],
    [REVKA.toUpperCase()],
    "agents-room",
    [READER, REVKA],
  );
  assert.equal(rows.length, 1, "only the asked pubkeys are answered");
  assert.deepEqual(rows[0].channel_ids, ["agents-room"]);
  assert.equal(admitted(rows, "agents-room", REVKA), "allow");
});

test("a persona outside the room stays refused, as the relay would refuse its post", () => {
  const rows = relayAgentsForMentionRevalidation(
    [{ ...row(REVKA, "revka"), respond_to: "anyone", channel_ids: ["agents-room", "general"] }],
    [REVKA],
    "agents-room",
    [READER],
  );
  // The asked room's own roster wins over a stale directory scan.
  assert.deepEqual(rows[0].channel_ids, ["general"]);
  assert.equal(admitted(rows, "agents-room", REVKA), "deny");
});

test("the verbatim registry row (the old answer) refused even a room member", () => {
  // The regression this seam fixes: respond_to null and no channel ids, even
  // with the persona on the room's roster.
  const verbatim = { ...row(REVKA, "revka"), respond_to: null, channel_ids: [] };
  assert.equal(admitted([verbatim], "agents-room", REVKA), "deny");
  const answered = relayAgentsForMentionRevalidation([verbatim], [REVKA], "agents-room", [REVKA]);
  assert.equal(admitted(answered, "agents-room", REVKA), "deny", "respond_to null alone still refuses");
});

test("no channel means no room membership is claimed", () => {
  const rows = relayAgentsForMentionRevalidation(
    [row(REVKA, "revka")],
    [REVKA],
    undefined,
    [REVKA],
  );
  assert.deepEqual(rows[0].channel_ids, []);
});

const registry = [
  { agent: "revka", pubkey: REVKA, harness: "ferrite-cloud", instructions: null, model: null },
  { agent: "nova", pubkey: NOVA, harness: "", instructions: null, model: null },
];

test("registry rows carry the relay's dispatch policy and the rooms whose latest roster lists them", () => {
  const rows = relayAgentRows(registry, [
    { kind: 39002, created_at: 10, tags: [["d", "general"], ["p", REVKA, "", "member"], ["p", READER]] },
    // An older roster of the same room that still listed nova: superseded.
    { kind: 39002, created_at: 5, tags: [["d", "general"], ["p", NOVA]] },
    { kind: 39002, created_at: 7, tags: [["d", "devteam"], ["p", REVKA.toUpperCase()], ["p", NOVA]] },
    { kind: 39000, created_at: 9, tags: [["d", "ghost"], ["p", REVKA]] },
  ]);
  assert.deepEqual(rows.map((r) => [r.name, r.agent_type, r.respond_to, r.channel_ids.sort()]), [
    ["revka", "ferrite-cloud", "anyone", ["devteam", "general"]],
    ["nova", "ferrite-cloud", "anyone", ["devteam"]],
  ]);
});

test("a persona sharing a room with the reader is offered in a room's add-member search", () => {
  const rows = relayAgentRows(registry, [
    { kind: 39002, created_at: 1, tags: [["d", "general"], ["p", REVKA]] },
  ]);
  const scope = { type: "community" };
  assert.equal(admission(rows, scope, new Set(["general"]), "publish", REVKA), "allow");
  // Nova shares no room with this reader, so it is not offered.
  assert.equal(admission(rows, scope, new Set(["general"]), "publish", NOVA), "deny");
});

test("no roster evidence claims no membership", () => {
  const rows = relayAgentRows(registry, []);
  assert.deepEqual(rows.map((r) => r.channel_ids), [[], []]);
});

test("hosted agent snapshots use the portable v1 JSON shape without memories", () => {
  const encoded = buildCloudAgentSnapshot({
    id: "reviewer",
    display_name: "Release Reviewer",
    avatar_url: "https://relay.example/reviewer.png",
    system_prompt: "Review releases.",
    runtime: "ferrite-cloud",
    model: "gpt-5.4",
    provider: null,
    name_pool: [],
    is_builtin: false,
    is_active: true,
    env_vars: {},
    respond_to: "owner-only",
    respond_to_allowlist: [],
    parallelism: null,
    created_at: "1970-01-01T00:00:00.000Z",
    updated_at: "1970-01-01T00:00:00.000Z",
  });

  assert.equal(encoded.fileName, "release-reviewer.agent.json");
  const snapshot = JSON.parse(
    new TextDecoder().decode(Uint8Array.from(encoded.fileBytes)),
  );
  assert.deepEqual(snapshot, {
    format: "buzz-agent-snapshot",
    version: 1,
    definition: {
      name: "Release Reviewer",
      sourceIsBuiltIn: false,
      systemPrompt: "Review releases.",
      runtime: "ferrite-cloud",
      model: "gpt-5.4",
      provider: null,
      respondTo: "owner-only",
      respondToAllowlist: [],
      namePool: [],
    },
    profile: {
      displayName: "Release Reviewer",
      avatarUrl: "https://relay.example/reviewer.png",
    },
    memory: { level: "none", entries: [] },
  });
});

test("hosted agent snapshot filenames stay portable", () => {
  const encoded = buildCloudAgentSnapshot({
    id: "symbols",
    display_name: "  R&D / Review ✨  ",
    avatar_url: null,
    system_prompt: "",
    is_builtin: false,
    created_at: "1970-01-01T00:00:00.000Z",
    updated_at: "1970-01-01T00:00:00.000Z",
  });

  assert.equal(encoded.fileName, "r-d-review.agent.json");
});

test("a per-thread session policy is refused at the create and update seam, before any relay write", async () => {
  const originalFetch = globalThis.fetch;
  let fetched = 0;
  globalThis.fetch = async () => {
    fetched += 1;
    throw new Error("network disabled in this test");
  };
  try {
    for (const cmd of ["create_persona", "update_persona"]) {
      await assert.rejects(
        handleAppCommand(cmd, {
          input: { id: "ada", displayName: "Ada", behavior: { sessionPolicy: "thread" } },
        }),
        /one conversation per channel/,
      );
    }
    assert.equal(fetched, 0, "the refusal happens before a request is sent");
    // The channel policy is not refused by this guard: it proceeds to the relay write.
    await assert.rejects(
      handleAppCommand("create_persona", {
        input: { displayName: "Ada", behavior: { sessionPolicy: "channel" } },
      }),
      (error) => !/one conversation per channel/.test(String(error?.message ?? error)),
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

/* --- readRoomRosters pagination & removal superseding ---------------------- */

const RELAY = "d".repeat(64);

function roster(id, room, createdAt, members) {
  return {
    id: id.padEnd(64, "0"),
    pubkey: RELAY,
    kind: 39002,
    created_at: createdAt,
    tags: [["d", room], ...members.map((member) => ["p", member])],
    content: "",
    sig: "e".repeat(128),
  };
}

function fakeRelay(events, pageSize) {
  const asked = [];
  const fetchPage = async (filter) => {
    asked.push(filter);
    return events
      .filter((event) => filter.kinds.includes(event.kind))
      .filter((event) => !filter["#p"] || event.tags.some((tag) => tag[0] === "p" && filter["#p"].includes(tag[1])))
      .filter((event) => filter.until === undefined || event.created_at <= filter.until)
      .sort((left, right) => right.created_at - left.created_at)
      .slice(0, Math.min(filter.limit, pageSize));
  };
  return { fetchPage, asked };
}

test("a persona removed from a room is no longer listed in it", async () => {
  const events = [
    roster("v1", "ops", 100, [READER, REVKA]),
    roster("v2", "ops", 200, [READER]),
    roster("g1", "general", 150, [READER, REVKA]),
  ];
  const relay = fakeRelay(events, 500);
  const rows = relayAgentRows([registry[0]], await readRoomRosters(relay.fetchPage));
  assert.deepEqual(rows[0].channel_ids, ["general"]);
  assert.ok(relay.asked.every((filter) => filter["#p"] === undefined), "no #p filter");
});

test("rosters beyond the first page are read", async () => {
  const events = [roster("member", "old-room", 10, [REVKA])];
  for (let index = 0; index < 30; index += 1) {
    events.push(roster(`r${index}`, `room-${index}`, 1000 + index, [READER]));
  }
  const relay = fakeRelay(events, 8);
  const rows = relayAgentRows([registry[0]], await readRoomRosters(relay.fetchPage));
  assert.deepEqual(rows[0].channel_ids, ["old-room"]);
  assert.ok(relay.asked.length > 1, "the walk went past page one");
});

