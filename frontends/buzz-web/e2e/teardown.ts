/**
 * Teardown for the live journeys: a run deletes what it created.
 *
 * Every person's page is tapped (`onboarding.ts` `watch`) and each object the
 * relay ACCEPTED a create for is appended to a run log: rooms (a 2xx answer to
 * `POST /buzz/api/channels`, or an accepted 9007), projects (an accepted kind
 * 30621, by coordinate), and teammates (a 2xx `POST /buzz/api/agents`). The log
 * is one JSON object per line, so several specs in one process append safely.
 *
 * `globalTeardown` (playwright.config.ts) replays the log with the operator key:
 * a room goes through a 9008 the relay admits from an operator for any room, a
 * project through `POST /buzz/api/projects/delete`, a teammate through
 * `POST /buzz/api/agents/remove`. The shared rooms and the workspace personas are
 * never touched, whoever the log says created them. `sweepLeftovers` applies the
 * same deletions to objects earlier runs left behind, by name pattern.
 */
import fs from "node:fs";
import path from "node:path";
import type { FullConfig } from "@playwright/test";
import { finalizeEvent } from "nostr-tools/pure";

import { operatorSecret, relayRequest, type RelayReply } from "./relayHttp";
import { scrubTraces } from "./traceScrub";

export const PROTECTED_ROOMS = new Set(["general", "welcome-everyone"]);
export const PROTECTED_AGENTS = new Set(["revka", "nova-helper", "fabrizio"]);

export type Created =
  | { type: "room"; id: string }
  | { type: "project"; coordinate: string }
  | { type: "agent"; agent: string };

/**
 * Where this run's creations are logged. Not under test-results/: Playwright
 * empties its outputDir at the start of every run, which would drop the
 * undeleted entries teardown keeps for the next run to retry.
 */
export const CREATED_LOG =
  process.env.BUZZ_CREATED_LOG ?? path.join(process.cwd(), ".e2e-state", "created.jsonl");

export function recordCreated(entry: Created) {
  fs.mkdirSync(path.dirname(CREATED_LOG), { recursive: true });
  fs.appendFileSync(CREATED_LOG, `${JSON.stringify(entry)}\n`);
}

/** One signed request or frame a page sent, and what the relay answered, if known. */
export function createdFromEvent(event: { kind?: unknown; pubkey?: unknown; tags?: unknown }): Created | null {
  if (!Array.isArray(event.tags)) return null;
  const tags = event.tags as string[][];
  if (event.kind === 9007) {
    const id = tags.find((tag) => tag[0] === "h")?.[1];
    return id ? { type: "room", id } : null;
  }
  if (event.kind === 30621 && typeof event.pubkey === "string") {
    const d = tags.find((tag) => tag[0] === "d")?.[1];
    return d ? { type: "project", coordinate: `30621:${event.pubkey}:${d}` } : null;
  }
  return null;
}

export function readCreated(log = CREATED_LOG): Created[] {
  if (!fs.existsSync(log)) return [];
  const seen = new Set<string>();
  const out: Created[] = [];
  for (const line of fs.readFileSync(log, "utf8").split("\n")) {
    if (!line.trim()) continue;
    try {
      const entry = JSON.parse(line) as Created;
      const key = JSON.stringify(entry);
      if (!seen.has(key)) {
        seen.add(key);
        out.push(entry);
      }
    } catch {
      // A torn last line from a killed run: skip it.
    }
  }
  return out;
}

export type TeardownReport = {
  deleted: string[];
  kept: string[];
  failed: string[];
  /** The entries that were neither deleted nor deliberately kept. */
  remaining: Created[];
};

/**
 * Send one deletion, waiting out the relay's per-author write limit. A sweep
 * issues many writes from one key and the relay answers 429 once the author's
 * window is spent (measured: 54 of 139 on the first sweep); the window rolls
 * over, so waiting is the correct answer rather than a failure.
 */
async function withRateLimit(send: () => Promise<RelayReply>): Promise<RelayReply> {
  let reply = await send();
  for (let attempt = 0; attempt < 8 && reply.status === 429; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 15_000));
    reply = await send();
  }
  return reply;
}

/** Delete the given objects with the operator key. */
export async function deleteCreated(entries: Created[]): Promise<TeardownReport> {
  const report: TeardownReport = { deleted: [], kept: [], failed: [], remaining: [] };
  const operator = operatorSecret();
  if (!operator) {
    report.failed.push("no operator key (BUZZ_OPERATOR_STATE or the provisioning file): nothing was deleted");
    report.remaining.push(...entries);
    return report;
  }
  const outcome = (entry: Created, label: string, reply: RelayReply, gone: RegExp) => {
    if (reply.status === 200 || gone.test(reply.body)) report.deleted.push(label);
    else {
      report.failed.push(`${label}: ${reply.status} ${reply.body.slice(0, 160)}`);
      report.remaining.push(entry);
    }
  };
  for (const entry of entries) {
    if (entry.type === "room") {
      if (PROTECTED_ROOMS.has(entry.id)) {
        report.kept.push(`room ${entry.id}`);
        continue;
      }
      const id = entry.id;
      const send = () =>
        relayRequest(
          operator,
          "POST",
          "/buzz/api/events",
          finalizeEvent({ kind: 9008, created_at: Math.floor(Date.now() / 1000), tags: [["h", id]], content: "" }, operator),
        );
      outcome(entry, `room ${id}`, await withRateLimit(send), /channel is deleted|no such channel/);
    } else if (entry.type === "project") {
      outcome(
        entry,
        `project ${entry.coordinate}`,
        await withRateLimit(() => relayRequest(operator, "POST", "/buzz/api/projects/delete", { coordinate: entry.coordinate })),
        /$^/,
      );
    } else if (entry.type === "agent") {
      if (PROTECTED_AGENTS.has(entry.agent)) {
        report.kept.push(`agent ${entry.agent}`);
        continue;
      }
      outcome(
        entry,
        `agent ${entry.agent}`,
        await withRateLimit(() => relayRequest(operator, "POST", "/buzz/api/agents/remove", { agent: entry.agent })),
        /$^/,
      );
    }
  }
  return report;
}

/**
 * Where this run's traces are: every project's `outputDir` as Playwright
 * resolved it (relative to the config file, not the working directory), or
 * `<cwd>/test-results` when teardown runs without a config.
 */
export function traceRoots(config?: Pick<FullConfig, "projects">): string[] {
  const roots = new Set<string>();
  for (const project of config?.projects ?? []) {
    if (project.outputDir) roots.add(project.outputDir);
  }
  if (roots.size === 0) roots.add(path.join(process.cwd(), "test-results"));
  return [...roots];
}

/** Scrub every trace root; answers one line per trace that could not be scrubbed. */
function scrubRetainedTraces(config?: Pick<FullConfig, "projects">): string[] {
  const problems: string[] = [];
  const roots = traceRoots(config);
  for (const root of roots) {
    try {
      const report = scrubTraces(root, { outputRoots: roots });
      if (!report.found) {
        console.log(`[teardown] no trace directory at ${root}`);
        continue;
      }
      if (report.seen > 0) {
        console.log(`[teardown] scrubbed the session cookie from ${report.rewritten} of ${report.seen} trace(s) under ${root}`);
      }
      for (const failure of report.failures) console.log(`[teardown] TRACE NOT SCRUBBED: ${failure}`);
      problems.push(...report.failures.map((failure) => `trace not scrubbed: ${failure}`));
    } catch (error) {
      console.log(`[teardown] trace scrub failed under ${root}: ${(error as Error).message}`);
      problems.push(`trace scrub failed under ${root}: ${(error as Error).message}`);
    }
  }
  return problems;
}

/**
 * Rotate the log only when everything in it was dealt with. Otherwise rewrite
 * it with exactly the entries that were not deleted, so the next run (or an
 * operator re-run) retries those and nothing else.
 */
function settleCreatedLog(report: TeardownReport) {
  if (report.remaining.length === 0) {
    fs.renameSync(CREATED_LOG, `${CREATED_LOG}.${Date.now()}.done`);
    return;
  }
  const temporary = `${CREATED_LOG}.${process.pid}.tmp`;
  fs.writeFileSync(temporary, report.remaining.map((entry) => `${JSON.stringify(entry)}\n`).join(""));
  fs.renameSync(temporary, CREATED_LOG);
  console.log(`[teardown] ${report.remaining.length} undeleted entr${report.remaining.length === 1 ? "y" : "ies"} kept in ${CREATED_LOG} for the next run`);
}

/**
 * Playwright globalTeardown: scrub retained traces, replay this run's log, then settle it.
 *
 * Any trace that could not be scrubbed and any object that could not be
 * deleted fails the run by THROWING once all of the work is done. Setting
 * `process.exitCode` is not enough: Playwright computes its own exit status
 * from the runner's result and exits with it explicitly.
 */
export default async function globalTeardown(config?: Pick<FullConfig, "projects">) {
  // First, and whatever else happens: a retained trace must not carry a live
  // broker session (see traceScrub.ts). A trace that cannot be scrubbed is
  // quarantined and fails the run, but never stops the deletions below.
  const problems = scrubRetainedTraces(config);
  const finish = () => {
    if (problems.length === 0) return;
    process.exitCode = 1;
    throw new Error(`[teardown] ${problems.length} problem(s):\n  ${problems.join("\n  ")}`);
  };
  if (process.env.BUZZ_KEEP_CREATED === "1") {
    console.log(`[teardown] skipped (BUZZ_KEEP_CREATED=1); log kept at ${CREATED_LOG}`);
    return finish();
  }
  const entries = readCreated();
  if (entries.length === 0) return finish();
  const report = await deleteCreated(entries);
  console.log(
    `[teardown] deleted ${report.deleted.length}, kept ${report.kept.length}, failed ${report.failed.length}` +
      (report.failed.length ? `\n  ${report.failed.join("\n  ")}` : ""),
  );
  settleCreatedLog(report);
  problems.push(...report.failed.map((failure) => `not deleted: ${failure}`));
  return finish();
}
