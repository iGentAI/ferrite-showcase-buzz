/**
 * WORKFLOWS from a browser tab.
 *
 * The relay declares kind **30620** and the whole 46001–46012 lifecycle, and
 * runs them (`relay/workflows.rs`). So a workflow is a workspace object like any
 * other, and this is its client.
 *
 * | command | how |
 * | --- | --- |
 * | `create_workflow` / `update_workflow` | publish a kind-30620 event |
 * | `delete_workflow` | NIP-09 kind 5 naming the ADDRESS |
 * | `get_workflow`, `get_channel_workflows`, `get_channels_workflows` | query 30620 |
 * | `get_workflow_runs` | `POST /buzz/api/workflows/runs` |
 * | `get_run_approvals` | query kind 46010 for the run |
 * | `grant_approval` / `deny_approval` | publish 46011 / 46012 with the token |
 * | `trigger_workflow` | publish 46020, the manual trigger |
 *
 * ## Why definitions are events and runs are not
 *
 * A definition is something a PERSON authored: signed by them, replaceable by
 * them, and portable to another relay that speaks the same kinds. A run is
 * something THIS RELAY did — its trace, its step outcomes, its gates — so it is
 * relay state, read through a route. That is why `get_workflow_runs` is the one
 * command here that is not a query, and the asymmetry is worth keeping rather
 * than smoothing away.
 *
 * ## The YAML is kept
 *
 * Both forms travel in the content: the parsed definition, which the relay
 * validates and executes, and the original text. An editor that showed back a
 * re-serialization of the parse would have silently eaten the author's comments,
 * key order and formatting — so `yaml` is what `update_workflow` round-trips.
 */
import { parse as parseYaml } from "yaml";

import { apiPost } from "@/shared/lib/api";
import { randomUuid } from "@/shared/lib/insecureContextCrypto";
import type { RelayEvent } from "@/shared/api/types";
import {
  publishSigned,
  relayEvents,
  type RelayIdentity,
} from "@/shared/browser-backend/relayConnection";

/** buzz's workflow kinds, as the relay declares them. */
const KIND_WORKFLOW_DEF = 30620;
const KIND_WORKFLOW_TRIGGER = 46020;
const KIND_APPROVAL_REQUESTED = 46010;
const KIND_APPROVAL_GRANTED = 46011;
const KIND_APPROVAL_DENIED = 46012;
const KIND_DELETION = 5;

/** The relay clamps a REQ `limit` to 500. */
const PAGE = 500;

/** The client's `RawWorkflow`. */
type RawWorkflow = {
  id: string;
  revision: string;
  name: string;
  owner_pubkey: string;
  channel_id: string | null;
  definition: Record<string, unknown>;
  status: string;
  created_at: number;
  updated_at: number;
};

async function publish(
  identity: RelayIdentity,
  input: { kind: number; content: string; tags: string[][] },
  failed: string,
): Promise<{ id: string }> {
  const signed = (await identity.sign(input)) as unknown as { id: string };
  await publishSigned(signed, failed);
  return signed;
}

function firstTag(event: RelayEvent, name: string): string | null {
  return event.tags.find((tag) => tag[0] === name)?.[1] ?? null;
}

/// The newest version of each CELL, keyed by its full replaceable address.
///
/// A parameterized-replaceable event is addressed by `(kind, pubkey, d)`, so
/// keying on `d` alone would let two authors' workflows hide each other — the
/// kind is fixed here, which leaves author and id. The newest-wins step is still
/// needed because a live socket can hold both versions across a replacement.
function latestByAddress(events: RelayEvent[]): Map<string, RelayEvent> {
  const latest = new Map<string, RelayEvent>();
  for (const event of events) {
    const id = firstTag(event, "d");
    if (!id) continue;
    const address = `${event.pubkey.toLowerCase()}\u0000${id}`;
    const seen = latest.get(address);
    if (!seen || newerThan(event, seen)) latest.set(address, event);
  }
  return latest;
}

/**
 * Nostr's own total order over two versions: newer `created_at` wins, and equal
 * timestamps break to the LOWEST id.
 *
 * The tie-break is not decoration. Without it the "newest" of two same-second
 * versions is whichever the relay happened to deliver last, so the same state
 * could answer differently on two reads — and this is the function the relay's
 * own replacement engine uses to decide which version it serves, so agreeing
 * with it is what keeps the client's view and the relay's view the same.
 */
function newerThan(candidate: RelayEvent, incumbent: RelayEvent): boolean {
  if (candidate.created_at !== incumbent.created_at) {
    return candidate.created_at > incumbent.created_at;
  }
  return candidate.id < incumbent.id;
}

/** A 30620 event as the client's `RawWorkflow`. */
function toRawWorkflow(event: RelayEvent): RawWorkflow | null {
  const id = firstTag(event, "d");
  if (!id) return null;
  let definition: Record<string, unknown> = {};
  try {
    const parsed = JSON.parse(event.content) as unknown;
    if (parsed && typeof parsed === "object") {
      definition = parsed as Record<string, unknown>;
    }
  } catch {
    // A definition whose content is not JSON is one this client cannot show.
    // Reported as an empty definition rather than dropped, so the row still
    // appears and can be repaired instead of vanishing from the list.
    definition = {};
  }
  const status = typeof definition.status === "string" ? definition.status : "active";
  // `created_at` is carried INSIDE the definition because a replaceable event
  // only ever exposes the latest version's timestamp — without it, every edit
  // would reset the workflow's apparent creation time.
  const createdAt =
    typeof definition.created_at === "number" && definition.created_at > 0
      ? definition.created_at
      : event.created_at;
  return {
    id,
    revision: event.id,
    name: typeof definition.name === "string" ? definition.name : id,
    owner_pubkey: event.pubkey,
    channel_id: firstTag(event, "h"),
    definition,
    status,
    created_at: createdAt,
    updated_at: event.created_at,
  };
}

/** Turn an author's YAML into the definition the relay validates and runs. */
function definitionFromYaml(yamlDefinition: string, createdAt: number): Record<string, unknown> {
  let parsed: unknown;
  try {
    parsed = parseYaml(yamlDefinition);
  } catch (error) {
    throw new Error(
      `That workflow is not valid YAML: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error("A workflow definition must be a YAML mapping.");
  }
  return {
    ...(parsed as Record<string, unknown>),
    // The original text, so an editor shows the author their own file back
    // rather than a re-serialization of the parse.
    yaml: yamlDefinition,
    created_at: createdAt,
  };
}

/// A workflow id: a readable slug plus RANDOM bytes.
///
/// The random half is load-bearing rather than cosmetic. This id becomes the
/// `d` of a replaceable cell, so two ids that collide are two workflows that
/// REPLACE each other — and a previous version derived the suffix from the
/// second, which meant two people creating "Deploy" in the same second produced
/// exactly that. `randomUuid` falls back to `crypto.getRandomValues` where
/// `crypto.randomUUID` is missing (a plain-http origin is not a secure context).
function workflowIdFor(definition: Record<string, unknown>): string {
  const name = typeof definition.name === "string" ? definition.name : "workflow";
  const slug = name
    .toLowerCase()
    .replace(/[^a-z0-9-_]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 48);
  const suffix = randomUuid().replace(/-/g, "").slice(0, 12);
  return `${slug || "workflow"}-${suffix}`;
}

/** `get_channel_workflows` / `get_channels_workflows`. */
export async function channelWorkflows(channelIds: string[]): Promise<RawWorkflow[]> {
  const wanted = channelIds.filter((id) => typeof id === "string" && id.length > 0);
  if (wanted.length === 0) return [];
  const events = await relayEvents({
    kinds: [KIND_WORKFLOW_DEF],
    "#h": wanted,
    limit: PAGE,
  });
  return [...latestByAddress(events).values()]
    .map(toRawWorkflow)
    .filter((row): row is RawWorkflow => row !== null);
}

/** Every version of one workflow id, newest first — across authors, because a
 * `d` alone does not name a cell. */
async function versionsOf(workflowId: string): Promise<RawWorkflow[]> {
  const events = await relayEvents({
    kinds: [KIND_WORKFLOW_DEF],
    "#d": [workflowId],
    limit: PAGE,
  });
  // Sorted on the EVENTS, before `toRawWorkflow` discards the event id: two
  // authors' cells can share a `d` and a second, and without the id tie-break
  // the "newest" of those is whichever the relay happened to deliver last — so
  // the same relay state would answer differently on two reads. `newerThan` is
  // the relay's own ordering, which is what keeps this view and its view the
  // same.
  return [...latestByAddress(events).values()]
    .sort((a, b) => (newerThan(a, b) ? -1 : newerThan(b, a) ? 1 : 0))
    .map(toRawWorkflow)
    .filter((row): row is RawWorkflow => row !== null);
}

/** `get_workflow`.
 *
 * A `d` alone is half an address, so the version shown is resolved by
 * `resolveWorkflowVersion` rather than taken as the newest across authors:
 * otherwise any member could publish a newer 30620 with the same `d` and have
 * their steps displayed under someone else's workflow. `me` is the caller's
 * pubkey; `versions` is injectable so the rule can be driven in node.
 */
export async function getWorkflow(
  workflowId: string,
  me: string | null = null,
  versions: (workflowId: string) => Promise<RawWorkflow[]> = versionsOf,
): Promise<RawWorkflow> {
  if (!workflowId) throw new Error("get_workflow needs a workflowId.");
  return resolveWorkflowVersion(await versions(workflowId), workflowId, me);
}

/**
 * Which cell a bare workflow id means, given every author's newest version.
 *
 * The caller's OWN cell wins, because that is the only one they can have
 * created under this id: ids carry random bytes (`workflowIdFor`), so a second
 * author with the same `d` copied it. With no cell of theirs, a single author
 * is unambiguous; several are refused rather than guessed between, because
 * every signal that could rank them (`created_at` included) is the
 * publisher's to choose.
 */
export function resolveWorkflowVersion(
  versions: RawWorkflow[],
  workflowId: string,
  me: string | null,
): RawWorkflow {
  if (versions.length === 0) throw new Error(`No workflow ${workflowId} on this relay.`);
  const self = me?.toLowerCase();
  const mine = self
    ? versions.find((row) => row.owner_pubkey.toLowerCase() === self)
    : undefined;
  if (mine) return mine;
  const authors = new Set(versions.map((row) => row.owner_pubkey.toLowerCase()));
  if (authors.size > 1) {
    throw new Error(
      `More than one member has published a workflow with the id ${workflowId}, ` +
        "and the id alone cannot say which one you opened, so none is shown.",
    );
  }
  return versions[0];
}

/**
 * Why `chosen` must not be triggered, or null.
 *
 * The manual trigger names only the workflow id and the channel, and the relay
 * resolves that pair to whichever definition it indexed last for the channel,
 * whatever its author. So while ANOTHER member's cell with the same id names
 * the same channel, a trigger meant for `chosen` could run their steps in this
 * room under the caller's authority. Refusing is the only safe answer from
 * here; keying the relay's index by author is the lasting fix.
 */
export function workflowTriggerConflict(
  versions: RawWorkflow[],
  chosen: RawWorkflow,
): string | null {
  const owner = chosen.owner_pubkey.toLowerCase();
  const rival = versions.find(
    (row) =>
      row.owner_pubkey.toLowerCase() !== owner &&
      (row.channel_id ?? null) === (chosen.channel_id ?? null),
  );
  if (!rival) return null;
  return (
    `Another member has published a workflow with the id ${chosen.id} in the same channel. ` +
    "The relay identifies a workflow to run by channel and id alone, so running this one " +
    "could run theirs instead; it was not started."
  );
}

/**
 * The version of `workflowId` that `me` OWNS, if any.
 *
 * Editing and deleting need this rather than `getWorkflow`: a `d` tag is only
 * half of a replaceable address, so the newest version of an id may belong to
 * somebody else. Publishing an "update" against theirs would create a NEW cell
 * under the caller — validly signed, accepted by the relay, and leaving the
 * original untouched while the caller believes they edited it.
 */
async function ownedWorkflow(workflowId: string, me: string): Promise<RawWorkflow> {
  const mine = (await versionsOf(workflowId)).find(
    (row) => row.owner_pubkey.toLowerCase() === me.toLowerCase(),
  );
  if (!mine) {
    throw new Error(
      `You do not own a workflow ${workflowId}, so it cannot be changed from here.`,
    );
  }
  return mine;
}

/** `create_workflow` — publish a new definition. */
export async function createWorkflow(
  identity: RelayIdentity,
  args: { channelId?: string; yamlDefinition?: string },
): Promise<RawWorkflow & { webhook_secret: string | null }> {
  const channelId = args.channelId ?? "";
  if (!channelId) throw new Error("create_workflow needs a channelId.");
  const now = Math.floor(Date.now() / 1000);
  const definition = definitionFromYaml(args.yamlDefinition ?? "", now);
  const id = workflowIdFor(definition);
  const signed = await publish(
    identity,
    {
      kind: KIND_WORKFLOW_DEF,
      content: JSON.stringify(definition),
      tags: [
        ["d", id],
        ["h", channelId],
      ],
    },
    "Could not save that workflow.",
  );
  const pubkey = await identity.pubkey();
  return {
    id,
    revision: signed.id,
    name: typeof definition.name === "string" ? definition.name : id,
    owner_pubkey: pubkey,
    channel_id: channelId,
    definition,
    status: typeof definition.status === "string" ? definition.status : "active",
    created_at: now,
    updated_at: now,
    // Webhook triggers are not a form this relay's engine offers, so there is no
    // secret to hand back. Null rather than a fabricated one: a caller that
    // displayed a fake secret would be showing a credential that opens nothing.
    webhook_secret: null,
  };
}

/** `update_workflow` — replace a definition in place. */
export async function updateWorkflow(
  identity: RelayIdentity,
  args: {
    workflowId?: string;
    yamlDefinition?: string;
    expectedOwnerPubkey?: string;
    expectedChannelId?: string | null;
  },
): Promise<RawWorkflow & { webhook_secret: string | null }> {
  const workflowId = args.workflowId ?? "";
  if (!workflowId) throw new Error("update_workflow needs a workflowId.");
  const me = await identity.pubkey();
  if (
    args.expectedOwnerPubkey !== undefined &&
    args.expectedOwnerPubkey.toLowerCase() !== me.toLowerCase()
  ) {
    throw new Error(
      `You do not own workflow ${workflowId}, so it cannot be changed from here.`,
    );
  }
  const existing = await ownedWorkflow(workflowId, me);
  if (
    args.expectedChannelId !== undefined &&
    (existing.channel_id ?? null) !== args.expectedChannelId
  ) {
    throw new Error(
      `Workflow ${workflowId} belongs to a different channel than the card selected.`,
    );
  }
  const definition = definitionFromYaml(args.yamlDefinition ?? "", existing.created_at);
  const tags: string[][] = [["d", workflowId]];
  if (existing.channel_id) tags.push(["h", existing.channel_id]);
  const signed = await publish(
    identity,
    { kind: KIND_WORKFLOW_DEF, content: JSON.stringify(definition), tags },
    "Could not save that workflow.",
  );
  const now = Math.floor(Date.now() / 1000);
  return {
    ...existing,
    revision: signed.id,
    name: typeof definition.name === "string" ? definition.name : workflowId,
    definition,
    status: typeof definition.status === "string" ? definition.status : "active",
    updated_at: now,
    webhook_secret: null,
  };
}

/** `delete_workflow` — retire the whole addressable cell. */
export async function deleteWorkflow(
  identity: RelayIdentity,
  args: {
    workflowId?: string;
    expectedOwnerPubkey?: string;
    expectedChannelId?: string | null;
  },
): Promise<null> {
  const workflowId = args.workflowId ?? "";
  if (!workflowId) throw new Error("delete_workflow needs a workflowId.");
  const pubkey = await identity.pubkey();
  if (
    args.expectedOwnerPubkey !== undefined &&
    args.expectedOwnerPubkey.toLowerCase() !== pubkey.toLowerCase()
  ) {
    throw new Error(
      `You do not own workflow ${workflowId}, so it cannot be deleted from here.`,
    );
  }
  const existing = await ownedWorkflow(workflowId, pubkey);
  if (
    args.expectedChannelId !== undefined &&
    (existing.channel_id ?? null) !== args.expectedChannelId
  ) {
    throw new Error(
      `Workflow ${workflowId} belongs to a different channel than the card selected.`,
    );
  }
  // A NIP-09 `a` tag names the ADDRESS, not one event id, so every version of
  // this workflow is retired — including ones this client never held, and ones
  // that arrive afterwards. An `e` tag would delete only the row we happen to
  // have seen and would let an older version resurface.
  await publish(
    identity,
    {
      kind: KIND_DELETION,
      content: "",
      tags: [["a", `${KIND_WORKFLOW_DEF}:${pubkey}:${workflowId}`]],
    },
    "Could not delete that workflow.",
  );
  return null;
}

function toUnixSeconds(value: number | null | undefined): number | null {
  if (typeof value !== "number" || !Number.isFinite(value)) return null;
  return value > 1e11 ? Math.floor(value / 1_000) : Math.floor(value);
}

function normalizeRunStatus(status: string): string {
  return status === "awaiting_approval" ? "waiting_approval" : status;
}

function normalizeExpiresAt(raw: string | null): string {
  if (!raw) return new Date(0).toISOString();
  const trimmed = raw.trim();
  if (/^\d+$/.test(trimmed)) {
    const num = Number(trimmed);
    if (Number.isFinite(num)) {
      const ms = num > 1e11 ? num : num * 1_000;
      return new Date(ms).toISOString();
    }
  }
  const parsed = Date.parse(trimmed);
  return Number.isNaN(parsed) ? new Date(0).toISOString() : new Date(parsed).toISOString();
}

/** `get_workflow_runs` — relay state, so a route rather than a query. */
export async function workflowRuns(args: {
  workflowId?: string;
  limit?: number | null;
}): Promise<{ runs: unknown[]; next: null }> {
  const workflowId = args.workflowId ?? "";
  if (!workflowId) throw new Error("get_workflow_runs needs a workflowId.");
  const reply = await apiPost<{
    runs?: Array<{
      id: string;
      workflow_id: string;
      status: string;
      next_step: number;
      started_at: number;
      completed_at?: number | null;
      error?: string | null;
      trace?: Array<{
        step_id: string;
        status: string;
        started_at?: number | null;
        completed_at?: number | null;
        error?: string | null;
      }>;
    }>;
  }>("/workflows/runs", {
    workflow: workflowId,
    ...(typeof args.limit === "number" ? { limit: args.limit } : {}),
  });
  const runs = (reply.runs ?? []).map((run) => {
    const startedAt = toUnixSeconds(run.started_at);
    return {
      id: run.id,
      workflow_id: run.workflow_id,
      status: normalizeRunStatus(run.status),
      current_step: run.next_step,
      execution_trace: (run.trace ?? []).map((entry) => ({
        step_id: entry.step_id,
        status: normalizeRunStatus(entry.status),
        output: {},
        started_at: toUnixSeconds(entry.started_at),
        completed_at: toUnixSeconds(entry.completed_at),
        error: entry.error ?? null,
      })),
      started_at: startedAt,
      completed_at: toUnixSeconds(run.completed_at),
      error_message: run.error ?? null,
      created_at: startedAt ?? 0,
    };
  });
  return { runs, next: null };
}

/** Whether `decision` comes from a key `gate` admits, before it expired. */
function decisionAdmittedByGate(gate: RelayEvent, decision: RelayEvent): boolean {
  const approver = (firstTag(gate, "approver") ?? "").trim().toLowerCase();
  if (/^[0-9a-f]{64}$/.test(approver) && decision.pubkey.toLowerCase() !== approver) {
    return false;
  }
  const expiresMs = Date.parse(normalizeExpiresAt(firstTag(gate, "expiration")));
  if (expiresMs > 0 && decision.created_at * 1_000 > expiresMs) return false;
  return decision.created_at >= gate.created_at;
}

/** `get_run_approvals` — the gates a run opened, from its 46010 events. */
export async function runApprovals(args: {
  workflowId?: string;
  runId?: string;
}): Promise<{ approvals: unknown[] }> {
  const runId = args.runId ?? "";
  if (!runId) throw new Error("get_run_approvals needs a runId.");
  const requested = await relayEvents({
    kinds: [KIND_APPROVAL_REQUESTED],
    "#run": [runId],
    limit: PAGE,
  });
  // Sorted before anything is derived from position: `step_index` is the gate's
  // place in the run, and taking it from arrival order would renumber the same
  // gates differently on two reads.
  const gates = [...requested].sort(
    (a, b) => a.created_at - b.created_at || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0),
  );
  const tokens = gates
    .map((event) => firstTag(event, "token"))
    .filter((token): token is string => typeof token === "string" && token.length > 0);

  // Decisions for THESE tokens, not every decision on the relay: a decision
  // names its gate by token, so the filter is exact and the reply is bounded by
  // the run rather than by the workspace's whole approval history.
  const decided =
    tokens.length === 0
      ? []
      : await relayEvents({
          kinds: [KIND_APPROVAL_GRANTED, KIND_APPROVAL_DENIED],
          "#token": tokens,
          limit: PAGE,
        });
  // The decision that CLOSED each gate: the earliest decision from a pubkey the
  // gate admits, made before the gate expired. 46011/46012 are ordinary client
  // kinds that any member who saw the token can publish, so neither "newest"
  // nor "any author" may decide what the card shows. A gate naming one approver
  // pubkey admits only that key; a role gate ("moderator") cannot be checked
  // here and keeps the relay's first-decision rule. Ties break on id so two
  // same-second decisions resolve the same way every time.
  const gateByToken = new Map<string, RelayEvent>();
  for (const gate of gates) {
    const token = firstTag(gate, "token");
    if (token && !gateByToken.has(token)) gateByToken.set(token, gate);
  }
  const outcome = new Map<string, RelayEvent>();
  for (const event of decided) {
    const token = firstTag(event, "token");
    if (!token) continue;
    const gate = gateByToken.get(token);
    if (!gate || !decisionAdmittedByGate(gate, event)) continue;
    const seen = outcome.get(token);
    if (!seen || newerThan(seen, event)) outcome.set(token, event);
  }

  const approvals = gates.map((event, index) => {
    const token = firstTag(event, "token") ?? "";
    const decision = outcome.get(token);
    const status = !decision
      ? "pending"
      : decision.kind === KIND_APPROVAL_GRANTED
        ? "granted"
        : "denied";
    return {
      approval_ref: token,
      workflow_id: firstTag(event, "workflow") ?? args.workflowId ?? "",
      run_id: runId,
      step_id: firstTag(event, "step") ?? "",
      // The relay does not number its steps on the wire — a gate is identified
      // by its step id — so this is the position among a run's gates, which is
      // what the UI uses it for.
      step_index: index,
      approver_spec: firstTag(event, "approver") ?? "",
      status,
      approver_pubkey: decision?.pubkey ?? null,
      note: decision?.content || null,
      expires_at: normalizeExpiresAt(firstTag(event, "expiration")),
      created_at: event.created_at,
    };
  });
  return { approvals };
}

/** `grant_approval` / `deny_approval`. */
export async function decideApproval(
  identity: RelayIdentity,
  args: { token?: string; note?: string | null },
  granted: boolean,
): Promise<{ token: string; status: string; run_id: string; workflow_id: string }> {
  const token = args.token ?? "";
  if (!token) throw new Error("An approval decision needs a token.");
  await publish(
    identity,
    {
      kind: granted ? KIND_APPROVAL_GRANTED : KIND_APPROVAL_DENIED,
      content: args.note ?? "",
      tags: [["token", token]],
    },
    granted ? "Could not approve that step." : "Could not deny that step.",
  );
  // The run and workflow ids are the relay's to report — it resolves the token
  // to its gate — and the caller already knows which run it was looking at, so
  // they are echoed empty rather than guessed at here.
  return { token, status: granted ? "granted" : "denied", run_id: "", workflow_id: "" };
}

/** `trigger_workflow` — run one now, regardless of its trigger condition.
 *
 * The definition is resolved the way `get_workflow` resolves it (the caller's
 * own cell first), and the trigger is refused while another author's cell with
 * the same id names the same channel — see `workflowTriggerConflict`. `deps` is
 * injectable so the rule can be driven in node. */
export async function triggerWorkflow(
  identity: RelayIdentity,
  args: { workflowId?: string },
  deps: {
    versions: (workflowId: string) => Promise<RawWorkflow[]>;
    publish: typeof publish;
  } = { versions: versionsOf, publish },
): Promise<{ run_id: string; workflow_id: string; status: string }> {
  const workflowId = args.workflowId ?? "";
  if (!workflowId) throw new Error("trigger_workflow needs a workflowId.");
  const versions = await deps.versions(workflowId);
  const workflow = resolveWorkflowVersion(versions, workflowId, await identity.pubkey());
  const conflict = workflowTriggerConflict(versions, workflow);
  if (conflict) throw new Error(conflict);
  const signed = await deps.publish(
    identity,
    {
      kind: KIND_WORKFLOW_TRIGGER,
      content: "",
      tags: [
        ["workflow", workflowId],
        ...(workflow.channel_id ? [["h", workflow.channel_id]] : []),
      ],
    },
    "Could not start that workflow.",
  );
  // The run id is DERIVED by the relay from (workflow, trigger event), so the
  // trigger's own id is what a caller needs to correlate the run it started.
  return { run_id: signed.id, workflow_id: workflowId, status: "running" };
}
