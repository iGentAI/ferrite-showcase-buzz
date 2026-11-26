// A kind-30620 cell is addressed by (author, d). A read or a trigger that
// resolves a bare `d` across every author lets another member's same-id cell
// be shown and run as yours (F165).
import test from "node:test";
import assert from "node:assert/strict";

import {
  getWorkflow,
  resolveWorkflowVersion,
  triggerWorkflow,
  workflowTriggerConflict,
} from "./workflows.ts";
import {
  getChannelWorkflows,
  getRunApprovals,
  getWorkflowRuns,
} from "../api/tauriWorkflows.ts";
import { relayClient } from "../api/relayClient.ts";

const VICTIM = "a".repeat(64);
const ATTACKER = "b".repeat(64);
const VIEWER = "c".repeat(64);

const cell = (owner, channel, updated, steps) => ({
  id: "deploy-0123456789ab",
  name: "Deploy",
  owner_pubkey: owner,
  channel_id: channel,
  definition: { name: "Deploy", steps },
  status: "active",
  created_at: 1,
  updated_at: updated,
});

// Newest first, as `versionsOf` returns them: the forged cell is newer.
const forged = [
  cell(ATTACKER, "ops", 200, ["rm -rf"]),
  cell(VICTIM, "ops", 100, ["ship"]),
];

test("the owner sees their own workflow, not a newer forged cell with the same id", async () => {
  const shown = await getWorkflow("deploy-0123456789ab", VICTIM, async () => forged);
  assert.equal(shown.owner_pubkey, VICTIM);
  assert.deepEqual(shown.definition.steps, ["ship"]);
});

test("a third member is not shown either cell when two authors claim one id", () => {
  assert.throws(
    () => resolveWorkflowVersion(forged, "deploy-0123456789ab", VIEWER),
    /More than one member has published a workflow with the id/,
  );
  const single = [cell(VICTIM, "ops", 100, ["ship"])];
  assert.equal(resolveWorkflowVersion(single, "x", VIEWER).owner_pubkey, VICTIM);
  assert.throws(() => resolveWorkflowVersion([], "x", VIEWER), /No workflow x/);
});

function signer(pubkey) {
  return {
    pubkey: async () => pubkey,
    sign: async (event) => ({ id: "f".repeat(64), ...event }),
  };
}

test("a trigger is refused while another author's same-id cell names the same channel", async () => {
  const published = [];
  await assert.rejects(
    triggerWorkflow(
      signer(VICTIM),
      { workflowId: "deploy-0123456789ab" },
      {
        versions: async () => forged,
        publish: async (_identity, input) => {
          published.push(input);
          return { id: "t" };
        },
      },
    ),
    /could run theirs instead/,
  );
  assert.equal(published.length, 0, "no 46020 was published");
});

test("a trigger with no rival in the channel publishes against the owner's channel", async () => {
  const published = [];
  const versions = [
    cell(ATTACKER, "elsewhere", 200, ["rm -rf"]),
    cell(VICTIM, "ops", 100, ["ship"]),
  ];
  assert.equal(workflowTriggerConflict(versions, versions[1]), null);
  const reply = await triggerWorkflow(
    signer(VICTIM),
    { workflowId: "deploy-0123456789ab" },
    {
      versions: async () => versions,
      publish: async (_identity, input) => {
        published.push(input);
        return { id: "trigger-id" };
      },
    },
  );
  assert.equal(reply.run_id, "trigger-id");
  assert.deepEqual(published[0].tags, [
    ["workflow", "deploy-0123456789ab"],
    ["h", "ops"],
  ]);
});

test("getWorkflowRuns, getRunApprovals, and getChannelWorkflows match tauriWorkflows envelopes and field units", async () => {
  const originalWindow = globalThis.window;
  const originalFetch = globalThis.fetch;
  const originalFetchEvents = relayClient.fetchEvents;
  const store = new Map();
  globalThis.window = {
    location: { origin: "https://relay.example" },
    localStorage: {
      getItem: (k) => store.get(k) ?? null,
      setItem: (k, v) => store.set(k, String(v)),
      removeItem: (k) => store.delete(k),
    },
  };
  globalThis.fetch = async (input) => {
    const url = String(input);
    assert.ok(url.endsWith("/buzz/api/workflows/runs"));
    return new Response(
      JSON.stringify({
        runs: [
          {
            id: "run-1",
            workflow_id: "deploy-0123456789ab",
            status: "awaiting_approval",
            next_step: 1,
            started_at: 1_760_000_000_000,
            completed_at: 1_760_000_005_000,
            error: null,
            trace: [
              {
                step_id: "gate",
                status: "awaiting_approval",
                started_at: 1_760_000_000_000,
                completed_at: null,
                error: null,
              },
            ],
          },
        ],
      }),
      { status: 200, headers: { "Content-Type": "application/json" } },
    );
  };
  relayClient.fetchEvents = async (filter) => {
    if (filter.kinds?.includes(30620)) {
      return [
        {
          id: "c".repeat(64),
          pubkey: VICTIM,
          kind: 30620,
          created_at: 1_760_000_000,
          content: JSON.stringify({ name: "Deploy", status: "active" }),
          tags: [
            ["d", "deploy-0123456789ab"],
            ["h", "ops"],
          ],
          sig: "0".repeat(128),
        },
      ];
    }
    if (filter.kinds?.includes(46010)) {
      return [
        {
          id: "d".repeat(64),
          pubkey: VICTIM,
          kind: 46010,
          created_at: 1_760_000_001,
          content: "",
          tags: [
            ["token", "tok-1"],
            ["workflow", "deploy-0123456789ab"],
            ["run", "run-1"],
            ["step", "gate"],
            ["approver", "admin"],
            ["expiration", "1900000000"],
          ],
          sig: "0".repeat(128),
        },
      ];
    }
    return [];
  };
  try {
    const workflows = await getChannelWorkflows("ops");
    assert.equal(workflows.length, 1);
    assert.equal(workflows[0].revision, "c".repeat(64));

    const runs = await getWorkflowRuns("deploy-0123456789ab");
    assert.equal(runs.length, 1);
    assert.equal(runs[0].status, "waiting_approval");
    assert.equal(runs[0].startedAt, 1_760_000_000);
    assert.equal(runs[0].completedAt, 1_760_000_005);
    assert.equal(runs[0].createdAt, 1_760_000_000);
    assert.equal(runs[0].executionTrace[0].startedAt, 1_760_000_000);

    const approvals = await getRunApprovals("deploy-0123456789ab", "run-1");
    assert.equal(approvals.length, 1);
    assert.equal(approvals[0].approvalRef, "tok-1");
    assert.equal(new Date(approvals[0].expiresAt).getTime(), 1_900_000_000_000);
  } finally {
    if (originalWindow === undefined) delete globalThis.window;
    else globalThis.window = originalWindow;
    globalThis.fetch = originalFetch;
    relayClient.fetchEvents = originalFetchEvents;
  }
});

// A 46011/46012 decision is an ordinary client event: any member who saw the
// gate's token can publish one. Only the gate's named approver, before the gate
// expired, may decide what the card shows, and the first such decision wins
// (finding_58ba0146eb52aa43).
test("getRunApprovals ignores decisions from keys the gate does not admit or made after expiry", async () => {
  const originalWindow = globalThis.window;
  const originalFetchEvents = relayClient.fetchEvents;
  globalThis.window = { location: { origin: "https://relay.example" } };
  const APPROVER = "e".repeat(64);
  const decision = (id, pubkey, kind, createdAt, note) => ({
    id: id.repeat(64),
    pubkey,
    kind,
    created_at: createdAt,
    content: note,
    tags: [["token", "tok-2"]],
    sig: "0".repeat(128),
  });
  relayClient.fetchEvents = async (filter) => {
    if (filter.kinds?.includes(46010)) {
      return [
        {
          id: "d".repeat(64),
          pubkey: VICTIM,
          kind: 46010,
          created_at: 1_000,
          content: "",
          tags: [
            ["token", "tok-2"],
            ["run", "run-2"],
            ["step", "gate"],
            ["approver", APPROVER],
            ["expiration", "2000"],
          ],
          sig: "0".repeat(128),
        },
      ];
    }
    if (filter.kinds?.includes(46011)) {
      return [
        decision("1", ATTACKER, 46011, 1_500, "forged grant"),
        decision("2", APPROVER, 46012, 1_200, "no"),
        decision("3", APPROVER, 46011, 1_300, "changed my mind"),
        decision("4", APPROVER, 46011, 2_500, "too late"),
      ];
    }
    return [];
  };
  try {
    const approvals = await getRunApprovals("deploy-0123456789ab", "run-2");
    assert.equal(approvals.length, 1);
    assert.equal(approvals[0].status, "denied");
    assert.equal(approvals[0].approverPubkey, APPROVER);
    assert.equal(approvals[0].note, "no");
  } finally {
    if (originalWindow === undefined) delete globalThis.window;
    else globalThis.window = originalWindow;
    relayClient.fetchEvents = originalFetchEvents;
  }
});
