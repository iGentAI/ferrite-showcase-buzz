# buzz conformance gate

Runs **block/buzz's own end-to-end suites, unmodified**, against a deployed
Ferrite Buzz app (the `buzz` single bundle; its relay lives at
`apps/buzz/src/relay/`), and gates on a checked-in per-test baseline.

The baseline (28 July 2026) and the example URLs below were recorded against
the fork-era local fleet on port 28443, which was retired on 21 September 2026.
The baseline has not been re-recorded against an upstream round.

The protocol is the contract between the two implementations. buzz's relay
crate alone is 60k lines, over Postgres, Redis, and S3; ours is 43k with one
dependency. There is no code-level adapter between those two. There *is* a wire
between them, and upstream ships 219 tests that exercise it. Running those tests
is the only way to know whether the port still speaks buzz.

## Ground rules

* **Nothing is vendored.** The runner invokes upstream's tests where they live,
  via `cargo test --manifest-path <upstream>/Cargo.toml`. There is no copy of a
  test file in this repo, so upstream can never drift away from a stale fork.
* **The upstream checkout is read-only.** `CARGO_TARGET_DIR` is redirected out of
  the tree, and `git status --porcelain` is asserted empty before *and* after
  every run. A dirty checkout aborts the run, because results from a modified
  upstream describe somebody's edits rather than our relay.
* **No test is weakened.** Where a suite needs something the deployment does not
  offer, the gap is absorbed by a shim outside the test or recorded as a failure
  with a named cause. Assertions are never relaxed and files are never patched.

## Usage

```sh
# the recommended configuration -- this is what the baseline records
scripts/buzz-conformance.sh \
    --relay-url ws://3emul7w0sqvwfom6.localhost:28443/buzz/ --shim --pg

# straight at a relay, no shims
scripts/buzz-conformance.sh --relay-url ws://host:port/buzz/

# one suite, and keep the logs
scripts/buzz-conformance.sh --suites e2e_relay --out /tmp/run1

# after a relay change that fixes something, re-baseline
scripts/buzz-conformance.sh --shim --pg --update-expect
```

The upstream `block/buzz` checkout comes from `--upstream DIR` or
`$BUZZ_UPSTREAM` (default `~/Projects/buzz`). `--no-gate` reports without
setting a failing exit code; `-h` prints the full flag list.

Exit codes: `0` everything matched the baseline, `1` a test that must pass
failed (regression), `2` a test that was expected to fail now passes (good news
— re-baseline), `3` harness problem.

A test the baseline says must pass, but which failed inside the sequential run,
is re-run once on its own before the gate decides (`--no-retry` disables it).
That is not flake-hiding: the retry is announced, it is scoped to tests already
asserted to pass, and a second failure still reds the gate. Every flake observed
so far is a `collect_until_eose` timeout on a 5-second budget across an ssh
tunnel — 4 in ~1,000 test executions across five full runs, and each of the four
passed 3/3 when re-run in isolation.

## Where it stands (2026-07-28, relay `3emul7w0sqvwfom6`, upstream 070fb6a1)

`--shim --pg`: **43 of 196 runnable tests pass**, plus 22 tests in two suites
that are not runnable at all. 5 of the 43 are upstream's own self-skips.

| suite | pass |
|---|---|
| `e2e_user_status` | 5/5 |
| `e2e_long_form` | 7/8 |
| `e2e_managed_agent` | 4/5 |
| `e2e_mesh_llm` | 4/4 *(all four self-skip)* |
| `e2e_team` | 2/3 |
| `e2e_persona` | 10/24 |
| `e2e_relay` | 6/37 |
| `e2e_nostr_interop` | 3/25 |
| `e2e_media` | 2/7 *(one self-skips)* |
| `e2e_event_reminder` | 0/29 |
| `e2e_human_edit_agent_content` | 0/19 |
| `e2e_media_extended` | 0/21 |
| `e2e_media_video` | 0/7 |
| `e2e_git` | 0/2 |
| `conformance_multitenant` | 0/18 *(not run; 8 rows are upstream `todo!()`)* |
| `nip42_host_binding_live` | 0/4 *(not run)* |

100 of the 153 failures are the single `X-Pubkey` wall below. Behind it sit 20
named nonconformances and 31 tests that need media/S3 infrastructure this port
does not have.

## The pieces

| file | what it is |
|---|---|
| `../buzz-conformance.sh` | the runner and the gate |
| `parse.py` | libtest human output -> `suite ⇥ test ⇥ status ⇥ reason` TSV |
| `gate.py` | results vs `expectations.tsv`; classifies and sets the exit code |
| `expectations.tsv` | the baseline: one row per test, with *why* each non-pass is tolerated |
| `shim.mjs` | namespace adapter (deployment shape, not protocol) |
| `pg-shim.sh` | throwaway Postgres for `e2e_relay`'s raw-SQL seed helpers |

### `shim.mjs` — the namespace adapter

buzz's relay owns an origin: WebSocket at `/`, HTTP bridge at `/events`,
`/query`, `/count`, NIP-11 at `/info`, NIP-05 at `/.well-known/nostr.json`. A
Ferrite app owns a *namespace*: the gateway hands it `/buzz/` and nothing above
that. The adapter maps one onto the other and does nothing else.

It deliberately leaves `/buzz/api/...` paths untouched, because the suites sign
those with NIP-98 and `auth::url_covers_path` compares the `u` tag's path with
the real path — rewriting them would invalidate signatures that are currently
valid. Point the suites at `ws://127.0.0.1:<port>/buzz` and every `u` tag they
mint already names the path the relay will check.

It does **not** touch authentication, and cannot: see "what a shim cannot fix"
below.

### `pg-shim.sh` — the seed database

`e2e_relay::seed_relay_member` is a raw `sqlx` INSERT into buzz's `communities`
and `relay_members` tables, not an HTTP call. Without a reachable Postgres the
three tests that use it die at `PoolTimedOut` before touching the relay at all.
`pg-shim.sh up` initdb's a throwaway cluster on port 55432 with exactly those
two tables, so the tests run all the way to the relay and fail (or pass) for a
reason that is about the relay.

It does not make the seeded pubkey a real member of our community — see below.

## What a shim cannot fix, and why

Two gaps are structural. Both are recorded in `expectations.tsv` as
`missing-shim`, and both need a change in our relay, not in the harness.

1. **HTTP auth: upstream's suites send `X-Pubkey`.** That is buzz's dev-mode
   fallback (`crates/buzz-relay/src/api/bridge.rs:117`, active when
   `require_auth_token = false`). Our relay is NIP-98 only
   (`apps/buzz/src/relay/auth.rs`). A proxy cannot bridge this: it
   would have to mint a NIP-98 event signed by the caller's secret key, which it
   never sees, and `submit_event` refuses any event whose author differs from
   the authenticated caller (`apps/buzz/src/relay/ingress/events.rs`, *"event pubkey
   does not match the authenticated caller"*). Our deployment closes the door
   twice over — `apps/buzz/ferrite.json` forwards only the headers its routes
   declare (`accept`, `cookie`, `x-buzz-authorization`), so an `X-Pubkey`
   header never reaches the app even if the app wanted it.

2. **Member/moderator seeding is not wired into the harness.** Community
   membership lives in the relay's own Store and the moderator set in the config
   store under `buzz-community-moderators`; the platform owner MCP face has no
   store-write tool — its 57 tools are all app lifecycle. The relay has since
   grown an NIP-98-gated operator route that can write
   `buzz-community-moderators` (`POST /buzz/api/operator/config/<key>`), but the harness hook predates it:
   `pg-shim.sh register-owner` posts an unsigned request to
   `$BUZZ_CONFORMANCE_OPERATOR_URL/moderators`, which is neither that route's
   path nor its auth, so it still refuses loudly rather than pretending. Wiring
   the hook to the real operator route (an NIP-98-signed config write from a
   seeded operator key) is what makes the invite-mint tests reachable; the
   recorded baseline predates any such wiring.

## Reading `expectations.tsv`

`suite ⇥ test ⇥ expected ⇥ class ⇥ note`, where `class` is why a non-pass is
tolerated:

| class | meaning |
|---|---|
| `conform` | passes; our relay implements the obligation |
| `nonconformance` | **our relay is genuinely wrong**; `note` names the NIP and the fix site |
| `missing-shim` | needs `X-Pubkey` auth or moderator seeding (above) |
| `deployment` | asserts one of upstream's own constants (limits, error wording) |
| `untranslatable` | needs two tenants in one process; one Ferrite app is one community |
| `infra` | needs S3, an LLM endpoint, or media blob routes we do not run |
| `upstream-stub` | upstream ships the row as `todo!()`; it fails on buzz's relay too |
| `self-skip` | upstream's test returns early on a missing fixture — a vacuous pass |
| `flaky` | passes in isolation, timed out once under load |

A row whose class is `nonconformance` is a bug report with a test attached. Fix
the relay, re-run, and the gate will tell you the row is stale.
