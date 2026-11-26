# The upstream sync playbook

How changes in [`block/buzz`](https://github.com/block/buzz) are brought into this
repository, repeatably. This is the OPERATIONAL half of upstream tracking: the
procedure, the anchors, the gates, and the classification rules, in the order a
sync actually runs. The strategic half — why the fork is shaped this way, with
dated measurements — is `upstream-buzz-sync-strategy.md` (not in this
repository); read it once, then work from this document.

Every sync appends an entry to the log in §8. If the log's last entry is old,
the pins below it are stale — that is the signal to run one, and the honest risk
this document exists to manage: the procedure only protects anyone if it is run.

## 1. The fork surface — five tracks, five mechanics

"Our copy of buzz" is not one thing. It is five surfaces that track upstream in
five different ways, and a sync visits them in this order:

| # | surface | what it is | sync mechanic |
|---|---------|-----------|---------------|
| 1 | `frontends/buzz-web/src` | a FILE-LEVEL FORK of upstream `desktop/src` (1,468+ of ~1,660 files byte-identical) | re-copy + allowlist (`scripts/sync-upstream.mjs`) |
| 2 | `apps/buzz/src/relay` | a SEMANTIC PORT of upstream's relay behaviour onto Ferrite planes — shares not one line | contract snapshot diff + divergence register |
| 3 | `apps/buzz/src/{agents,llm,mcp,acp}`, `packages/*` | NATIVE — our platform's own agent runtime, provider boundary and protocol faces | **never synced**; upstream's equivalents (their agent harnesses, their ACP crates) are different products |
| 4 | `frontends/buzz-web/src/shared/{browser-backend,tauri-web,lib/desktop-bridge.ts}` | OUR seam implementing the client's Tauri surface in a browser | pulled forward by track 1's gates: the strict alias build and `commandCoverage.test.mjs` name every new demand the synced client makes |
| 5 | `frontends/buzz-desktop/` | OUR desktop shell — the command families a browser cannot answer | pulled forward by the SAME gate as track 4, from the other side: `commandCoverage.test.mjs` reads `frontends/buzz-desktop/native-commands.json` and proves no command is answered by both, and none is registered that the client never invokes |

The native stance lives in tracks 3, 4 and 5: upstream's client rides on OUR
relay, OUR browser seam and OUR shell, and nothing in a sync may weaken that. A
sync brings upstream's product forward; it never trades our runtime for theirs.

**Track 5 vendors no upstream Rust, and that is a decision rather than an
oversight.** The original design anticipated vendoring upstream's `project_git*.rs` and
`buzz-acp` behind a `VENDOR-PIN.md` manifest; the shell was written against the
CLIENT'S COMMAND CONTRACT instead (`projectGit.ts`'s names and wire shapes), with
`git` itself as the git implementation. So there is no vendored copy to drift, no
sixth manifest to verify, and the sync obligation is exactly the one track 4
already has: when the synced client asks for a new command, the coverage gate
names it and either the seam or the shell answers it.

## 2. Anchors are content, not history

**Platform fact, measured 2026-08-04: repository clones here are depth-1.** The
clone tool delivers HEAD's tree and a single commit; the pin commit is not in
local history, `<pin>..HEAD` is not a computable range, and nothing may assume
otherwise. **Re-measured 2026-09-11: checkouts can be FILES-ONLY — no `.git`
at all, and shell `git clone` is refused.** `sync-upstream.mjs` therefore
computes the `desktop/src` git tree id from content (`gitTreeHash`) and takes
the commit id from `BUZZ_UPSTREAM_COMMIT` when there is no git metadata; the
computed tree is what gets recorded, so the anchor stays content. Upstream also reserves the right to rewrite history (their workspace
metadata still points at a predecessor repo). So every anchor in this process is
content that WE hold:

| anchor | where | what it proves |
|--------|-------|----------------|
| our own `frontends/buzz-web/src` | track 1 | IS the old pin's content (modulo the allowlist) — the diff base that cannot rot |
| `frontends/buzz-web/UPSTREAM-PIN.md` | track 1 | which upstream commit/tree that content corresponds to |
| `frontends/buzz-web/scripts/upstream-manifest.json` | track 1 | per-file sha256 of the pinned upstream subtree — makes the drift check runnable OFFLINE, with no clone at all (§5.1) |
| `apps/buzz/upstream-contract/` | track 2 | vendored snapshot of upstream's protocol-contract files (`kind.rs`) at the pin — the next kind-table diff is two files on disk |
| `apps/buzz/UPSTREAM-CONTRACT.md` | track 2 | the relay-side pin + the per-sync disposition ledger |
| `scripts/upstream-allowlist.json` | track 1 | every deliberate divergence, with bucket and reason |
| strategy doc §6 register | track 2 | every deliberate behavioural divergence, with verdict |

The GitHub API (compare, contents-at-ref) is a supplement for point lookups,
never a dependency: it fails exactly when upstream rewrites history, which is
the moment a sync most needs to work.

## 3. Prerequisites

```sh
# A sibling clone of upstream, OUTSIDE this repository. Depth-1 is fine.
#   ~/buzz-upstream          (sandbox)
#   ~/Projects/buzz          (a laptop)
# The location guard in sync-upstream.mjs refuses a clone inside the repo and
# a clone that contains our tree — nothing else.
```

No other setup. The sync script is dependency-free Node; the web gates need
`frontends/buzz-web/node_modules` (`npm ci` once). The cargo gates run with plain
cargo against the public SDK checkout beside this repository.

**Working directory convention: every command block below begins with an
explicit `cd`.** Phases 0 and 1 run inside `frontends/buzz-web/`; phases 2 and 3 run at
the repository root. `$REPO` names the root of this repository. Set it once before working
through any block:

```sh
export LABS=~/ferrite-buzz   # or wherever this repository is checked out
```

## 4. The procedure

### Phase 0 — measure before touching anything

```sh
cd $REPO/frontends/buzz-web
node scripts/sync-upstream.mjs --check --upstream ~/buzz-upstream | tee /tmp/sync-report.txt
```

Read the counts. The shape of a healthy report after upstream has moved:

* **UNEXPECTED divergences** ≈ files upstream changed while we held the old
  bytes. These are NOT merge work — `--apply` re-copies them. Only a file that
  is BOTH here and on the allowlist carries hand work.
* **missing here** = new upstream files. Mechanical.
* **UNCOVERED ours-only** = files WE added since the last sync without an
  allowlist entry. **Resolve these BEFORE `--apply`, or it will park live
  code and the build breaks.** Add an `additive` entry per file (with the
  reason it exists); a file that genuinely retired can be left to park.
* **`(gone upstream)` notes** = allowlist divergence entries whose file
  upstream deleted. Decide each: if only upstream's old tree imported it, let
  it park and delete the entry; if OUR additive code imports it, move the file
  to an `additive` entry (it is no longer a divergence — it is ours now).
* **stale vendored assets** = mechanical, `--apply` refreshes.

### Phase 1 — the web fork

```sh
cd $REPO/frontends/buzz-web
# 1. Allowlist hygiene from the phase-0 reading (edit upstream-allowlist.json).
# 2. The copy:
node scripts/sync-upstream.mjs --apply --upstream ~/buzz-upstream
# 3. The gate — every difference on the allowlist, upstream checkout untouched:
node scripts/sync-upstream.mjs --check --upstream ~/buzz-upstream
# 4. The builds, STRICT — an unported @tauri-apps module must fail, not warn:
BUZZ_TAURI_STRICT=1 npx tsc --noEmit -p tsconfig.json
BUZZ_TAURI_STRICT=1 npx vite build
# 5. The unit gates — these enumerate the browser seam's new obligations:
node --test src/shared/browser-backend/commandCoverage.test.mjs \
            src/features/settings/ui/settingsNavCoverage.test.mjs \
            src/shared/features/previewManifest.test.mjs   # …and the rest
```

What each failure class means, and the repair:

| failure | meaning | repair |
|---------|---------|--------|
| `tsc` error in an ALLOWLISTED file | upstream moved the code around our divergence ("both-touched") | re-apply the INTENT named in the allowlist reason onto upstream's new content — never re-apply the old diff; update the reason if the intent moved |
| `tsc` error naming a missing symbol in `tauri-web/` | upstream imported a new symbol from a facaded module | add it to the facade's export list, implement or route to `unavailable` |
| `[tauri-web] @tauri-apps/X has no facade` (fatal under STRICT) | a genuinely new Tauri plugin | add a facade: re-export what the bridge can serve, `unavailable.ts` for the rest |
| `commandCoverage` failure | the synced client invokes a command the browser backend neither answers nor explains | implement it in `browser-backend/`, or add an exact-match refusal reason — a FALSE “desktop feature” reason is the failure mode this gate exists to prevent |
| nav/manifest/lib test failure | upstream restructured a surface one of our seams enumerates | follow the test's own message; these were each written to name their repair |

### Phase 2 — the relay port

```sh
cd $REPO
# 1. The contract diff — two files on disk, no history needed:
diff <(grep -oE 'pub const KIND_[A-Z0-9_]+: u32 = [0-9]+' apps/buzz/upstream-contract/kind.rs) \
     <(grep -oE 'pub const KIND_[A-Z0-9_]+: u32 = [0-9]+' ~/buzz-upstream/crates/buzz-core/src/kind.rs)
# 2. The curated enumeration — upstream's own changelog, scoped:
awk '/^## v<new>/,/^## v<old>/' ~/buzz-upstream/CHANGELOG.md \
  | grep -E '\((relay|security|persona|search|media|workflow|db|auth|nip-[a-z]+|presence)\)'
```

Disposition EVERY new kind and every matching changelog entry into exactly one
bucket (§6), record the table in `apps/buzz/UPSTREAM-CONTRACT.md`, implement the
`port` rows, refresh `apps/buzz/upstream-contract/kind.rs` from the clone, and run:

```sh
cd $REPO/apps/buzz
cargo test --locked --no-fail-fast
```

### Phase 3 — pins, log, commit (paths relative to `$REPO`)

1. `frontends/buzz-web/UPSTREAM-PIN.md`: new `commit`/`commitDate`/`commitSubject` (from the
   clone's single visible commit), `subtreeGitTree` (`git rev-parse
   HEAD:desktop/src` — computable even depth-1), file count from the sync
   report, `confidence: exact`.
2. Regenerate `frontends/buzz-web/scripts/upstream-manifest.json` (§5.1).
3. `apps/buzz/UPSTREAM-CONTRACT.md`: new pin + this sync's disposition table.
4. Append the §8 log entry: date, old→new pin, the counts, what was ported,
   what was declined and why, what was verified.
5. One commit for the sync, pins included — never split the copy from its pin.

## 5. The gates that hold between syncs

### 5.1 The offline drift check

`sync:check` needs an upstream clone, so it historically ran only when someone
was already syncing — “the honest risk … is that nobody runs the script.” The
manifest closes that: `--write-manifest` (run at every sync) records the pinned
subtree's per-file sha256 into `scripts/upstream-manifest.json`, and

```sh
cd $REPO/frontends/buzz-web
node scripts/sync-upstream.mjs --check-offline
```

verifies our tree against it with NO clone: every non-allowlisted file must
hash-match the manifest, every ours-only file must be covered by an additive
entry. It catches the one mistake that rots silently — hand-editing a vendored
copy — and belongs in the proof suite beside `ferrite-pkg check`, which is the
same idea for the same reason. Both modes are implemented in
`sync-upstream.mjs` (sync 001).

### 5.2 The seam gates

`commandCoverage.test.mjs` (every client `invoke` answered or exactly
explained), `settingsNavCoverage.test.mjs`, `previewManifest.test.mjs`, the
strict-alias build. These run in the ordinary JS gate set and fail on drift
between the vendored client and our seams — with or without a sync in flight.

### 5.3 The shell gates (track 5)

One file is the contract between the client and the desktop shell:
`frontends/buzz-desktop/native-commands.json`, which is what the shell injects as
`window.__BUZZ_DESKTOP__` before any bundle code runs.
Three tests hold it, and between them a command can only ever have one owner:

```sh
cd $REPO/frontends/buzz-desktop && cargo test -p buzz-desktop-core   # incl. registry::tests
cd $REPO/frontends/buzz-web && node --test src/shared/browser-backend/commandCoverage.test.mjs
cd $REPO/frontends/buzz-web && node --test src/shared/lib/desktopDispatch.test.mjs
```

* `registry::tests` asserts the checked-in JSON equals the COMPILED registry, so
  the document the JS gate reads cannot drift from what the shell implements.
* `commandCoverage.test.mjs` asserts no command is answered by both the seam and
  the shell, and that every registered command is one the client genuinely
  invokes — a registry entry nothing calls would be a black hole the bridge
  routes into. It also polices the one declared exception: a command whose
  truthful answer differs by face (`list_managed_agents` is `[]` in a tab and a
  real list in the shell) may be listed in the registry's `overrides`, and then
  MUST be both registered and genuinely overriding a seam answer — so a
  deliberate override is distinguishable from an accidental double answer, and a
  stale one fails.
* `desktopDispatch.test.mjs` pins the dispatch itself: fail-closed marker
  validation, mock-versus-shell discrimination, and mock precedence.

A sync that brings a new command family therefore has exactly three honest
answers, and the gate refuses everything else: implement it in the seam, register
it in the shell and implement it there, or — when the truthful answer genuinely
differs between a tab and the shell — declare it as an override and implement
both.

## 6. Classification rules — every upstream change lands in exactly one bucket

| bucket | rule | lands as |
|--------|------|----------|
| **mechanical** | byte-copy of upstream client code, vendored assets | `--apply`; no human decision |
| **port** | a protocol/behaviour change our relay must honour for the client we now ship (new kind, gating rule, contract fix) | Rust change in `apps/buzz/src/relay`, with tests |
| **adapt** | a client feature needing browser-seam work (new invoke command, new Tauri plugin) | `browser-backend`/facade change, or an honest refusal reason |
| **register** | upstream changed behaviour where we deliberately differ | a row in the strategy doc's §6 register with a keep/converge verdict |
| **N/A — infra** | Postgres/Redis/S3/Kubernetes/read-replica/deps of their runtime | logged in the sync entry, nothing else |
| **N/A — product line** | their desktop shell, mobile, hosted multi-community, pairing | logged; ours is single-community by design (strategy doc §6.3) |
| **upstreamable** | OUR fix that shrinks the fork if upstream takes it | a PR to block/buzz; entry moves out of the allowlist when it lands |

Two rules with teeth: an upstream change may not be silently dropped — if it
matched the phase-2 filters it appears in the disposition table even as N/A —
and a `port` is a port of BEHAVIOUR, never of implementation: their code
assumes Postgres and tokio; ours rides planes and one dependency.

## 7. Keeping the fork small — the standing pressure

The steady-state cost of this fork is the allowlist's divergence buckets plus
the facade directory. Each has a shrink lever, and every sync should pull at
least one:

* **Prefer an additive seam to an edited upstream file.** The `@tauri-apps`
  alias retired 41 permanent conflicts at a stroke; the recorded next cut of the
  same shape is the `?inline` asset codemod (strategy doc §4.4).
* **Send the `upstreamable-pending` bucket upstream.** Every landed PR deletes
  an allowlist entry forever — the only permanent divergence reduction there is.
* **Never edit a vendored copy in place.** The offline check exists to catch
  exactly this; the fix belongs in an additive file or an allowlisted
  divergence with a reason.
* **New ours-only files under `frontends/buzz-web/src` get their additive entry in the
  same commit.** Phase-0 hygiene should find nothing.
* **Keep the shell thin.** `buzz-desktop` exists to answer what a browser tab
  cannot; anything a tab CAN do belongs in the seam, where one implementation
  serves both faces. Every command moved from the shell to the seam is one less
  thing that has to be built, signed and shipped three times.

## 8. Sync log — append-only, oldest first (new entries go at the bottom)

| date | upstream pin | headline |
|------|-------------|----------|
| 2026-08-04 | `070fb6a1` → `f86dfc58` (v0.4.x/#3063-era → v0.5.4) | entry 001 — first sync under this playbook: web fork re-based (1,663 files), 4 relay ports, offline drift check landed |
| 2026-09-10 | `f86dfc58` → `051c3a27` (v0.5.4 → v0.5.23) | entry 002 — web fork re-based (2,417 files), allowlist rebuilt (162 entries), 2 relay ports (30179 author-only, 48104 relay-only), 34 seam obligations answered or refused |
| 2026-09-11 | `051c3a27` → `813bbd14` (v0.5.23, 4 commits) | entry 003 — 13 client files re-copied, 1 both-touched merge, presence storage failure refuses the heartbeat (#7532), files-only checkout support in the sync script |

### Entry 001 — 2026-08-04, `070fb6a1` → `f86dfc58`

**Phase 0 (raw two-way):** 1,129 byte-identical · 42 allowlisted divergences
(2,291 changed lines) · 303 UNEXPECTED · 189 upstream files missing here ·
18 ours-only uncovered · 2 stale vendored assets · 1 allowlist entry gone
upstream.

**The method finding that shaped the rest:** the 303 “unexpected” were
reclassified THREE-way against the old pin's recovered content (a ref-pinned
clone of `desktop/src` at `070fb6a1` — the depth-1 workaround §2 forces):
293 were mechanical re-copies (ours == pin, only upstream moved), 6 were OUR
unregistered session edits over unchanged upstream content, and 4 were genuine
unregistered both-touched merges. A blind `--apply` would have destroyed the
6; the classifier is the difference between “303 merges” and “10 decisions”.

**Allowlist:** +17 (10 drifted divergences registered with intents precise
enough to re-derive next sync, 7 additive including the whole
`features/mcp-connectors/` prefix), −1 (`DoctorSettingsPanel.tsx` — upstream
retired the panel; our copy parked).

**Both-touched repairs:** all 20 merged with `diff3 -m ours pin new`; 13
clean, 7 hand-resolved by re-applying the allowlist INTENT onto upstream's new
content (`RuntimeIcon`, `presetLogos.test`, `SetupStep`,
`onboardingRuntimeSelection`, `ProjectIssuesPanel`, `TimelineMessageList`,
`SettingsView`).

**New dependency:** `@fontsource/jetbrains-mono` (upstream's `main.tsx`
imports it) — the only package.json gap.

**Seam obligations (the gates named them):** 18 new commands. 6 implemented —
NIP-49 encrypted backup (`browser-backend/backup.ts`), raw-body media upload
(Tauri 2's three-argument `invoke` routed to `handleRawAppCommand`), real
chunk-granular upload cancellation, `BrowserAppWindow.close()`. 12 refused
with reasons about THEMSELVES (agent cards, pocket voices,
`fetch_join_policy`, `set_persona_shared`), never the generic sentence.

**Relay dispositions (phase 2):** the full table is
`apps/buzz/UPSTREAM-CONTRACT.md`. Four ports — kind 30178 under the generalized
shared-gate set, kind 30621 at ingest, presence TTL 90 s → 180 s (the client's
heartbeat slowed to 60 s and the contract is three windows), and the search
operators honoured in the index walk, validated at the route, forwarded by the
seam. One register row (9033 stays inert per the strategy doc §6.2 #11), one
already-true (NIP-11 `max_limit`), the rest N/A with reasons. The contract
snapshot `apps/buzz/upstream-contract/kind.rs` is vendored at the new pin.

**Machinery this sync added:** `--write-manifest` / `--check-offline`
(pin-bound — §5.1 is now implemented, not designed), npm scripts for both,
the recovered-pin three-way method, and `apps/buzz/UPSTREAM-CONTRACT.md` itself.

**Verified:** strict `tsc` and strict `vite build` exit 0; `sync:check` PASS
(residual fork **1,644 changed lines across 52 files** — down from 3,104
mid-sync, because the diff3 merges landed upstream's own edits into the
divergent files); `--check-offline` PASS against the 1,663-file manifest;
`commandCoverage` 3/0 and `backup` 3/0; search gate 6/6 including the new
operator test; full Rust suite **1,112 passed / 2 failed**, both failures the
recorded load-sensitive gates (`severe_saga_gate`, `severe_schedule_gate`)
passing 6/6 and 2/2 in isolation immediately after — 1,114 effective, exactly
the 3 tests this sync added over the prior 1,111.

### Entry 002 — 2026-09-10, `f86dfc58` → `051c3a27`

**Run across three sessions** (two predecessor threads stopped by the operator;
their workspaces were lost, their branch `buzz-upstream-sync` was not). The
upstream clone was re-taken depth-1 on 2026-09-10 and its HEAD re-measured at
`051c3a270be9c73da9ab06700bcab7d5552fceaa` (2026-09-09T16:52:35-07:00,
`fix(buzz-acp): report missing models without retrying (#7538)`,
`desktop/package.json` 0.5.23), the same commit the first session copied — so
the copy is at HEAD and `confidence: exact` holds.

**Phase 0 (raw, first session):** upstream grew from 1,663 to **2,417** files
under `desktop/src` (36 releases, v0.5.5–v0.5.23). The raw two-way report is
lost with that workspace; the copy it produced is recoverable from git and
reads: **775 files added, 707 modified, 19 deleted** under `buzz-web/src`
against main (`fb7d62a`). The allowlist went from **90 to 162 entries**
(+73 − 1): the +73 are 56 registrations of drifted divergences (25 browser-only, 19 product, 12 upstreamable-pending) with intents
precise enough to re-derive (the register text in `upstream-allowlist.json` is
the record) plus 17 additive files this fork had grown since sync 001 without an
entry (hygiene the phase-0 rule exists for); the −1 is
`features/projects/ui/ProjectsView.tsx`, whose old divergence upstream absorbed.
Upstream's protected-feature indirection (`@protected-features`,
`@protected-feature-components`) and the `@model-capabilities-manifest` alias
are resolved to their OSS variants in `vite.config.ts`, `tsconfig.json` and
`test-loader.mjs` — the same additive-alias shape as the `@tauri-apps` cut, so
those files stay byte-identical. Three new dependencies (`linkifyjs`,
`mdast-util-from-markdown`, `react-day-picker`) and two vendored manifests
(`model-capabilities.json`; `preview-features.json` refreshed).

**Phase 1 verification (third session, fresh clone):** `sync:check` PASS —
2,417 upstream files, **2,310 byte-identical, 107 allowlisted divergences
(browser-only 53, product 30, upstreamable-pending 24), 0 unexpected, 0 missing,
148 additive, 0 uncovered, 48 vendored assets in sync**; residual fork
**3,625 changed lines across 107 files** (up from 1,644/52 — the fork grew
with the product, chiefly the workflow relay-dialect (`workflowFormTypes.ts`
+188) and the HTTP relay transport (`relayClientSession.ts` +349)). Strict
`tsc --noEmit` exit 0.

**Seam obligations (the gate named them):** `commandCoverage.test.mjs`
reported **34 new client commands** neither answered nor explained. **22
answered** in `browser-backend/`: `revalidate_relay_agents` (the send path's
last check before an @mention is authorised — unanswered, it refused every
mention of a cloud teammate), `fetch_persona_catalog` / `fetch_team_catalog`
(new `catalogs.ts`: upstream's NIP-33 head-select-then-shared-then-parse order,
pinned by `catalogs.test.mjs` 6/6), `get_channel_reconnect_repair` (unanswered
it rejected 3× per channel per reconnect and the repair window closed WITHOUT
repair — messages during a disconnect were dropped; `before_id` cannot be
honoured because the relay's filter parser ignores unknown keys, so the page is
`until`-bounded and the cursor event is dropped), `get_events` (chunked at 500
with upstream's kind list), `get_open_channel_directory` (the seam's projection
is already the discovery superset), `fetch_link_preview_metadata` (title from
the relay's host-scoped route, image fields honestly "none"), `agent_access_owner_only`
(`false` — unanswered, `welcomeGuide` awaited the rejection in a `Promise.all`
and the welcome team never seeded), `read_clipboard_text`,
`publish_project_owner_announcement` (own-key branch; managed-owner branch
refused with the `sign_project_pull_request*` reason), `release_media_upload`,
and the truthful no-ops: channel-head cache load/store/clear (nothing is
persisted; `[]`/`null`), archive-sync announce/start/stop (`1`/`null` — the
epoch must be a number), link-preview and media-fetch cancel/release (`null`),
`get_bestie_assignment` (`null`). **12 refused with reasons about
themselves**: `observed_unread_open_scope`/`_ingest` and `unread_catch_up`
(the client's localStorage fallback is TRIGGERED by the rejection — a synthetic
snapshot would drop every badge, so this must stay a refusal), `set_team_shared`
/ `add_team_from_catalog` (local pack store), `set_thread_scoped_acp_sessions`
(desktop-only preview), `get_agent_usage_series` (local SQLite archive; no
caller in the vendored client), `sign_project_issue_(un)assignment`
(managed-agent key; joined the PR-signing family), and the three bestie writes
(a Bestie is a LOCAL managed agent; the UI is behind the OSS
`publicUi.ts` alias and unreachable here). `commandCoverage` 4/4.

**Relay dispositions (phase 2):** the full table is
`apps/buzz/UPSTREAM-CONTRACT.md`. Kind-table diff: exactly **two new
constants**, both ported — **30179** `KIND_PRIVATE_MANAGED_AGENT` registered
author-only + global-only (before this an unregistered 30179 was ACCEPTED as an
ordinary world-readable event; gate
`severe_a_private_managed_agent_aggregate_stays_with_its_owner`, including the
by-id read) and **48104** `KIND_HUDDLE_LIVENESS` registered relay-only (before
this a client could store a forged liveness record every synced client's huddle
runtime subscribes to; gate `severe_huddle_liveness_cannot_be_forged_by_a_client`).
The mint half of 48104 is a **register** row: this relay has no audio rooms to
attest, so a liveness REQ answers empty. Curated changelog window
v0.5.5→v0.5.23: 1 port (above), 2 register (48104 mint; #4610 authenticated
media reads — ours stays public-by-digest by design), 3 already-true, the rest
N/A infra (their Postgres runtime, readiness metrics, frame acks) or N/A product
line (NIP-FI hosted identity, media resolution decoding, NIP-AM cost ladders).
`ALL_KINDS` 131 → 133; `upstream-contract/kind.rs` re-vendored at the pin
(1,088 lines, 130 constants).

**Phase 3:** `UPSTREAM-PIN.md` rewritten (`subtreeGitTree`
`e28b8ea8e9e72457a00a56e467c766c754a283ac`, 2,417 files); manifest regenerated
with `--write-manifest`; `--check-offline` PASS (2,310 hash-match, 107
allowlisted) — and it refused the regenerated manifest until the pin was
rewritten, which is the pin-binding doing its job.

**Method notes:** (1) two accidental `node_modules` symlinks
were committed by the first
session's `git add -A` — the platform redirects build directories through
symlinks and `.gitignore`'s `node_modules/` (trailing slash) does not match a
symlink; removed from the index. (2) On this platform `buzz-web/node_modules`
must live OFF the synced workspace mount (a bind mount from sandbox-local disk
keeps realpaths inside the project so TypeScript resolves `@types` correctly; a
plain symlink out of the tree does not, and pnpm's hoisted linker resolves
`motion`/`@tiptap` wrongly). (3) The Rust workspace resolves private Ferrite git
dependencies, so `cargo` needs `CARGO_NET_GIT_FETCH_WITH_CLI=1` and a git
credential for github.com even for `-p buzz`.

**Verified (2026-09-11, third session, all on this branch's final revision):**
strict `tsc --noEmit` exit 0 and strict `vite build` exit 0 (6,118 modules);
`sync:check` PASS (pin `[matches pin]`, 2,309 byte-identical, 108 allowlisted,
0 unexpected / 0 missing / 0 uncovered, residual fork 3,628 lines across 108
files); `sync:check-offline` PASS against the 2,417-file manifest; the §5.2
unit gates — `commandCoverage` 4/4, `settingsNavCoverage` 4/4,
`previewManifest` 3/3, `nip98ManagedAuthBoundary` 1/1 — plus `catalogs.test.mjs`
6/6, all 50/50 in one run; §5.3 `desktopDispatch.test.mjs` 15/15 and
`cargo test -p buzz-desktop-core` **153 passed / 0 failed**; the relay suite
`cargo test -p buzz` **1,203 passed / 0 failed / 1 ignored** (a measurement)
across the lib and 60 gate binaries, both new gates green. One test binary,
`topic_retention_gate.rs`, does not COMPILE on main either (it calls
`TestHarness::with_topic_segment_bytes`, which the vendored `ferrite-app-test`
at `b4d5771` lacks; the same commit left the root `Cargo.lock` naming
`ferrite-app-test 0.1.3` while the crate says 0.1.4) — excluded from the run
and reported, not fixed here. The platform's spec gate passed and its code gate admitted the source
(116 files, 3.9 MB).

**Known-red, not a gate:** the FULL `buzz-web` unit lane (`pnpm test`) is
deliberately unregistered in the proof suite and was red before this sync; on
this revision it still carries ~30 failing upstream tests that assume the
desktop repository layout (`../crates/buzz-relay/...`, `src-tauri/...`,
`../docs/nips/NIP-MP*.json`, `../test-fixtures/entity-links.json`, the
`tauriCommand` wrapper, `buildProtectedFeatureArtifacts`,
`modelCapabilitiesCorpus`), a jsdom-environment class (`HTMLFormElement`,
`window.focus`), and three that assert upstream behaviour our registered
divergences change on purpose (`composerLinkPastePipeline` vs the
`useRichTextEditor.ts` linkify divergence; `workflowYamlDocument` vs the
relay workflow dialect; `useArchiveSync` companion-realm vs the archive no-ops).
The next sync should give the layout-dependent ones the `presetLogos.test.mjs`
treatment (skip with the reason when the file is absent) and decide the three
divergence tests explicitly.

### Entry 003 — 2026-09-11, `051c3a27` → `813bbd14`

**Phase 0:** upstream main moved four commits overnight (#7550 inbox/author
truncation, #7532 presence storage rejection, #7501 mention chip wrapping, #7552
Pi adapter fork). The GitHub compare named 31 changed files, 14 under
`desktop/src`; `crates/buzz-core/src/kind.rs` untouched. Raw check against the
fresh checkout: 2,297 identical, 108 allowlisted, **12 unexpected, 1 missing**, 0
uncovered — all 13 pure upstream moves (the pin's offline check had passed, so
none carried our edits), plus one allowlisted file both-touched.

**Method finding:** the checkout tool now delivers a FILES-ONLY tree and shell
`git clone` is refused, so `git rev-parse` has nothing to read and
`--write-manifest` would have recorded `commit: null`. `sync-upstream.mjs` gained
`gitTreeHash` (the git tree id recomputed from content) and a declared
`BUZZ_UPSTREAM_COMMIT`; the computed `desktop/src` tree
`b29e47eb0752182171f90053d7877eedaaeeb96f` matched GitHub's id for the same
subtree exactly, which is also the proof the hash is implemented right (§2 updated).

**Phase 1:** `--apply` copied 13 files (0 vendored assets, 0 parked).
`MessageActionBar.tsx` (upstreamable-pending) was both-touched: upstream added a
`ref` so hosts can measure the rail; upstream's content taken and the
`!message.pending` reply gate re-applied — the intent in the allowlist reason is
unchanged. No new client commands: `commandCoverage` stayed green with no seam
change. Allowlist unchanged (163 entries).

**Phase 2:** one **port** — #7532: the presence TTL key is written before the
heartbeat is admitted and driven, and a failed write refuses it with a retryable
`error:` instead of being discarded after fan-out. The refusal branch is not
falsifiable in the fleet harness (no store fault injection) and is recorded as
such in `UPSTREAM-CONTRACT.md`. #7552's Rust is N/A product line; its client half
and #7550/#7501 were mechanical.

**Verified (2026-09-11 ~10:00 UTC):** strict `tsc --noEmit` exit 0; strict
`vite build` exit 0; `sync:check` PASS after apply (2,310 identical, 108
allowlisted, 0 unexpected/missing/uncovered); `sync:check-offline` PASS against
the 2,418-file manifest at `813bbd14`; unit gates (`commandCoverage`,
`settingsNavCoverage`, `previewManifest`, `desktopDispatch`,
`nip98ManagedAuthBoundary`, `catalogs`, and the synced
`agentSessionTranscriptPi`, `harnessCatalogCopy`, `markdown`) 122/122;
`cargo test -p buzz` **1,203 passed / 0 failed / 1 ignored** on the edited source,
including both presence gates (`topic_retention_gate` still excluded: does not
compile on main, unchanged).


### Entry 004 — recovered 2026-09-20, `813bbd14` → `4cd82f51`

This entry records recovered work from September 14. The original private
workspace was unavailable, so the recovery used its preserved file archive
and verified each file against the preservation manifest. Seventy-three
changed files were compared with their recorded base and current main; none
conflicted with newer main changes. Those newer changes remain in place.

The frontend pin is `4cd82f513214aad11c2b742ce7cc7c681e8e32a0`, with upstream
subtree `e650ce1009f5f9d4df824dc40e1f420e6e2e29a0`. The 2,421-file upstream
manifest was reconstructed from the prior manifest and exact upstream Git
blobs. Its SHA-256 is
`8adcf50a2d09ba1f9c415f8cdbfe4fe4009625d25cdc3d816d86240b82811795`,
which matches the original September 14 readback. The offline sync check
reports 2,307 matching files, 114 allowlisted adaptations, and no unexpected
or missing files.

Strict TypeScript checking and the strict production frontend build pass.
The full frontend suite reports 6,666 passes, 33 failures, and three skips
across 6,702 tests. Re-running the failing files against current main
reproduces 32 failures. The additional inventory-query timing failure is in
unchanged source and passes when run alone; the full suite is not claimed
green. The failures include tests that require upstream native files and
fixtures absent from this browser port, alongside existing browser-adapter
assertions. They remain outstanding validation limits, not waived checks.

The retained upstream kind snapshot matches the new pin byte for byte. The
new GET and POST Git default-branch routes are explicitly declined because
this relay does not host Git objects. All six relay wire tests pass, including
actual requests for both new refusals and the existing served routes. The full
Buzz Rust suite also passes: 1,228 tests, no failures, and six existing ignores.

This entry establishes source recovery and the stated local checks. It does
not replace the separately required deployed browser and agent journeys.
