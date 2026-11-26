/**
 * Sweep what earlier journey and driver runs left on a shared community.
 *
 *   BUZZ_HTTP=https://<host> BUZZ_SSO_ORIGIN=https://<host> BUZZ_SSO_SUBJECT_DOMAIN=<domain> \
 *   node --import ./test-loader.mjs e2e/sweep.ts            # report only
 *   node --import ./test-loader.mjs e2e/sweep.ts --apply    # delete, then report again
 *
 * Needs the operator key (BUZZ_OPERATOR_STATE, or the provisioning file). Counts
 * are what the OPERATOR can read: every open room's latest 39000, every project
 * (30621) and every registered teammate. A private room the operator is not a
 * member of is not served to it, so it is neither counted nor deleted here.
 *
 * Everything counted is a test leftover except the shared rooms (`general`,
 * `welcome-everyone`) and the workspace personas (`revka`, `nova-helper`,
 * `fabrizio`), which are never deleted. The relay identity is not an object
 * this script can reach.
 */
import "./sso";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { finalizeEvent } from "nostr-tools/pure";

import { deleteCreated, PROTECTED_AGENTS, PROTECTED_ROOMS, type Created } from "./teardown";
import { operatorSecret, relayRequest } from "./relayHttp";

type Event = { id: string; kind: number; pubkey: string; created_at: number; tags: string[][] };

/** One page request: `limit` events, newest first, `created_at <= until`. */
export const PAGE_LIMIT = 500;
export const PAGE_BUDGET = 60;

/** What a walk found, and whether that is everything the relay would serve. */
export type Walk = { events: Event[]; complete: boolean; gaps: string[] };

/**
 * Page backwards through everything `filter` matches.
 *
 * The relay's `until` is inclusive and a page is the newest `PAGE_LIMIT`
 * events at or before it, so every event in a full page NEWER than the page's
 * oldest timestamp is complete, and the oldest timestamp may be cut short.
 * The next page therefore asks again at that timestamp. When a full page holds
 * nothing but that one timestamp, the timestamp is saturated: the relay cannot
 * page within it through `until` alone, so the walk steps past it and records
 * a gap (completeness unknown) rather than pretending it saw everything.
 * Running out of page budget is also a gap.
 */
export async function walk(operator: Uint8Array, filter: Record<string, unknown>): Promise<Walk> {
  const byId = new Map<string, Event>();
  const gaps: string[] = [];
  let until: number | undefined;
  for (let page = 0; page < PAGE_BUDGET; page += 1) {
    const reply = await relayRequest(operator, "POST", "/buzz/api/filter-query", [
      until === undefined ? { ...filter, limit: PAGE_LIMIT } : { ...filter, limit: PAGE_LIMIT, until },
    ]);
    if (reply.status !== 200) throw new Error(`filter-query refused: ${reply.status} ${reply.body.slice(0, 200)}`);
    const events = JSON.parse(reply.body) as Event[];
    let added = 0;
    let oldest = Number.POSITIVE_INFINITY;
    let newest = Number.NEGATIVE_INFINITY;
    for (const event of events) {
      oldest = Math.min(oldest, event.created_at);
      newest = Math.max(newest, event.created_at);
      if (!byId.has(event.id)) {
        byId.set(event.id, event);
        added += 1;
      }
    }
    // A short page is the end of what the filter matches.
    if (events.length < PAGE_LIMIT) return { events: [...byId.values()], complete: gaps.length === 0, gaps };
    if (oldest === newest) {
      gaps.push(
        `a full page of ${PAGE_LIMIT} events all have created_at ${oldest}; the relay cannot page within one timestamp, so whether more exist there is unknown`,
      );
      until = oldest - 1;
    } else if (added === 0) {
      // Cannot happen with a relay that honours `until`; step on rather than loop.
      gaps.push(`the page at until=${until} returned nothing new; stepped past created_at ${oldest}`);
      until = oldest - 1;
    } else {
      until = oldest;
    }
  }
  gaps.push(`stopped after ${PAGE_BUDGET} pages of ${PAGE_LIMIT}; older events were not read`);
  return { events: [...byId.values()], complete: false, gaps };
}

const d = (event: Event) => event.tags.find((tag) => tag[0] === "d")?.[1] ?? "";

async function inventory(operator: Uint8Array) {
  const gaps: string[] = [];
  const rooms = new Map<string, Event>();
  const roomWalk = await walk(operator, { kinds: [39000] });
  gaps.push(...roomWalk.gaps.map((gap) => `rooms (39000): ${gap}`));
  for (const event of roomWalk.events) {
    const kept = rooms.get(d(event));
    if (!kept || kept.created_at < event.created_at) rooms.set(d(event), event);
  }
  const projects = new Map<string, Event>();
  const projectWalk = await walk(operator, { kinds: [30621] });
  gaps.push(...projectWalk.gaps.map((gap) => `projects (30621): ${gap}`));
  for (const event of projectWalk.events) {
    projects.set(`30621:${event.pubkey}:${d(event)}`, event);
  }
  const agents = await relayRequest(operator, "GET", "/buzz/api/agents");
  // A refused or malformed agents reply is a gap, not an empty inventory.
  let registered: string[] = [];
  if (agents.status !== 200) {
    gaps.push(`agents: query refused: ${agents.status} ${agents.body.slice(0, 200)}`);
  } else {
    const parsed = JSON.parse(agents.body) as { agents?: unknown };
    if (Array.isArray(parsed.agents)) {
      registered = parsed.agents
        .map((entry) => (entry as { agent?: unknown })?.agent)
        .filter((agent): agent is string => typeof agent === "string");
    } else {
      gaps.push("agents: reply carried no agents list");
    }
  }
  return { rooms: [...rooms.keys()], projects: [...projects.keys()], agents: registered, gaps };
}

function counts(label: string, found: { rooms: string[]; projects: string[]; agents: string[]; gaps: string[] }) {
  // An incomplete walk found AT LEAST this many; say so rather than print a
  // number that looks exact.
  const atLeast = found.gaps.length ? "at least " : "";
  console.log(
    `${new Date().toISOString()} ${label}: rooms ${atLeast}${found.rooms.length}, projects ${atLeast}${found.projects.length}, agents ${found.agents.length}` +
      ` (agents: ${found.agents.join(", ")})`,
  );
  for (const gap of found.gaps) console.log(`  WARNING incomplete inventory: ${gap}`);
}

async function main() {
  const operator = operatorSecret();
  if (!operator) throw new Error("no operator key: set BUZZ_OPERATOR_STATE");
  // Touch the operator plane first so a wrong key fails before anything else.
  const probe = finalizeEvent({ kind: 0, created_at: 0, tags: [], content: "" }, operator);
  console.log(`operator ${probe.pubkey.slice(0, 12)}…`);

  const before = await inventory(operator);
  counts("before", before);
  // A partial inventory still deletes what it found, but the run must not read
  // as a clean sweep.
  if (before.gaps.length) process.exitCode = 1;
  const targets: Created[] = [
    ...before.rooms.filter((room) => !PROTECTED_ROOMS.has(room)).map((id): Created => ({ type: "room", id })),
    ...before.projects.map((coordinate): Created => ({ type: "project", coordinate })),
    ...before.agents.filter((agent) => !PROTECTED_AGENTS.has(agent)).map((agent): Created => ({ type: "agent", agent })),
  ];
  console.log(`targets: ${targets.length} (${targets.filter((t) => t.type === "room").length} rooms, ${targets.filter((t) => t.type === "project").length} projects, ${targets.filter((t) => t.type === "agent").length} agents)`);
  if (process.argv.includes("--apply")) {
    const report = await deleteCreated(targets);
    console.log(`deleted ${report.deleted.length}, kept ${report.kept.length}, failed ${report.failed.length}`);
    for (const failure of report.failed) console.log(`  FAILED ${failure}`);
    const after = await inventory(operator);
    counts("after", after);
    if (report.failed.length || after.gaps.length) process.exitCode = 1;
  }
}

// Run only as a script, so tests can import `walk` without sweeping anything.
if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  await main();
}
