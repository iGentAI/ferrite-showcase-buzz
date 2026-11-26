# Upstream pin — `src/` is a copy of block/buzz `desktop/src`

`buzz-web/src` is not our code. It is **block/buzz's real desktop
client**, copied verbatim, with a register of deliberate divergences
(`scripts/upstream-allowlist.json`) and a directory of additive files that
upstream never sees. This file records **which upstream commit the copy
corresponds to**, so the next sync knows what it is merging from.

```json
{
  "repo": "git@github.com:block/buzz.git",
  "commit": "4cd82f513214aad11c2b742ce7cc7c681e8e32a0",
  "commitDate": "2026-09-13T14:04:56Z",
  "commitSubject": "feat(buzz-agent): add opt-in workspace-scoped Databricks reuse (#7606)",
  "subtree": "desktop/src",
  "subtreeGitTree": "e650ce1009f5f9d4df824dc40e1f420e6e2e29a0",
  "subtreeFileCount": 2421,
  "copiedInFerriteCommit": "sync 004 (docs/upstream-sync-playbook.md §8, entry 004)",
  "copiedAt": "2026-09-14",
  "confidence": "exact",
  "treeAlsoMatches": []
}
```

Upstream's `desktop/package.json` still reads `0.5.23` at this commit (eleven
commits past sync 003's `813bbd14`). The checkout was FILES-ONLY (no `.git`),
so `subtreeGitTree` was COMPUTED from the files by `sync-upstream.mjs`
(`gitTreeHash`) and matched the tree id GitHub reports for
`desktop/src@4cd82f51` byte for byte; the commit id is the one the checkout
tool fetched, declared through `BUZZ_UPSTREAM_COMMIT` and cross-checked against
the GitHub API.

## How that sha was established

Differently from the first pin, because the ground changed: **repository
clones here are depth-1** (measured 2026-08-04, re-measured 2026-09-10 — one
commit in history, no tags, `<pin>..HEAD` uncomputable; playbook §2). That removes the old
provenance-from-reflog and tree-dating arguments, and replaces them with two
that need no history:

1. **The clone IS the commit.** A depth-1 clone delivers exactly HEAD's tree
   and its single commit, so `git -C ~/buzz-upstream rev-parse HEAD` names the
   only commit the copy can correspond to. `confidence` is `exact` because the
   sync ran against that clone directly.

2. **Content.** After the sync, every file under upstream `desktop/src` is
   byte-identical to ours except the allowlisted divergences, and no upstream
   file is missing — `node scripts/sync-upstream.mjs --check` exits 0. Between
   syncs, `--check-offline` verifies the same property against the recorded
   per-file manifest (`scripts/upstream-manifest.json`) with no clone at all.

`subtreeGitTree` remains the pin that cannot rot: `git rev-parse
HEAD:desktop/src` is computable even depth-1, and a future clone whose subtree
tree-sha equals the recorded one holds identical content regardless of which
commit it names. `treeAlsoMatches` is empty because computing it requires
walking history a shallow clone does not have — it was only ever a
documentation nicety.

## The honest uncertainty

block/buzz lands roughly 790 commits a month, so assume this pin is stale and
re-establish it on every sync. The recorded commit can also UNDERSTATE how
current we are: commits after it that leave `desktop/src` untouched would make
a re-sync find nothing to copy — a conservative error in the safe direction.

## How to update it

1. Re-clone upstream (clones are depth-1; there is no fetch-and-checkout of an
   arbitrary sha — a fresh clone at HEAD is the unit of update), outside this
   repo:

   ```
   ~/buzz-upstream
   ```

2. Dry-run the sync and read the report:

   ```
   node scripts/sync-upstream.mjs --check --upstream ~/buzz-upstream
   ```

   Work the report per the playbook's phase-1 table (`docs/upstream-sync-playbook.md`).

3. Apply, then verify:

   ```
   node scripts/sync-upstream.mjs --apply --upstream ~/buzz-upstream
   BUZZ_TAURI_STRICT=1 npx tsc --noEmit -p tsconfig.json
   BUZZ_TAURI_STRICT=1 npx vite build
   ```

4. Regenerate the offline-check baseline:

   ```
   node scripts/sync-upstream.mjs --write-manifest --upstream ~/buzz-upstream
   ```

5. Update the JSON block above:

   - `commit`, `commitDate`, `commitSubject` — `git -C ~/buzz-upstream log -1
     --format='%H %cI %s'` (the clone's single visible commit)
   - `subtreeGitTree` — `git -C ~/buzz-upstream rev-parse HEAD:desktop/src`
   - `subtreeFileCount` — the `upstream files` line of the sync report
   - `copiedInFerriteCommit` / `copiedAt` — the sync log entry that lands it
   - `confidence` — `exact` when the sync ran against the clone being recorded

`scripts/sync-upstream.mjs` reads the JSON block above and warns when the
clone's HEAD does not match it, so a stale pin is loud rather than silent.
