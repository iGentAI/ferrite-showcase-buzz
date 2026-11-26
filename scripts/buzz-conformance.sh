#!/usr/bin/env bash
# Run block/buzz's own E2E conformance suites, unmodified and in place, against a
# Ferrite buzz deployment (the single-bundle app), and report pass/fail per test.
#
# This script vendors NOTHING. It invokes the tests that live in the upstream
# checkout, with `--manifest-path`, and it treats that checkout as read-only:
# the build target is redirected out of the tree and `git status` is asserted
# clean before and after every run. If the checkout is dirty on entry the run
# refuses to start, because a dirty upstream tree means the results describe
# somebody's edits rather than our relay.
#
# Usage
#   scripts/buzz-conformance.sh [options]
#
# Options
#   --relay-url URL     WebSocket URL of the relay under test.
#                       Default: $RELAY_URL, else ws://127.0.0.1:3000/buzz/
#   --upstream DIR      block/buzz checkout. Default: $BUZZ_UPSTREAM,
#                       else $HOME/Projects/buzz
#   --out DIR           Where logs and results land. Default: a temp dir.
#   --suites "a b c"    Suites to run. Default: every suite in the
#                       expectations file that is not marked untranslatable.
#   --shim              Start the namespace adapter (see
#                       scripts/buzz-conformance/shim.mjs) and point the suites
#                       at it instead of at the relay directly.
#   --pg                Start the throwaway Postgres e2e_relay's raw-SQL seed
#                       helpers write into (scripts/buzz-conformance/pg-shim.sh)
#                       and export DATABASE_URL at it. Without this, the three
#                       tests that seed a member die at `PoolTimedOut` before
#                       they reach the relay at all.
#   --expect FILE       Expectations file. Default:
#                       scripts/buzz-conformance/expectations.tsv
#   --update-expect     Rewrite the expectations file from this run's results
#                       (keeps the class/note columns for tests it already knew).
#   --no-gate           Report, but always exit 0.
#   --no-retry          Do not re-run, once and alone, the tests the baseline
#                       says must pass but that failed in the sequential run.
#   --suite-timeout N   Seconds per suite. Default 900.
#
# Gate semantics (exit code)
#   0  every test matched its expectation
#   1  a test that was expected to PASS failed  -> regression, or the relay moved
#   2  a test that was expected to FAIL passed  -> good news, tighten the baseline
#   3  harness problem (upstream dirty, build failure, missing tool)
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUPPORT="$HERE/buzz-conformance"

RELAY_URL_ARG="${RELAY_URL:-ws://127.0.0.1:3000/buzz/}"
UPSTREAM="${BUZZ_UPSTREAM:-$HOME/Projects/buzz}"
OUTDIR=""
SUITES=""
USE_SHIM=0
USE_PG=0
EXPECT="$SUPPORT/expectations.tsv"
UPDATE_EXPECT=0
GATE=1
RETRY=1
SUITE_TIMEOUT=900

while [ $# -gt 0 ]; do
  case "$1" in
    --relay-url) RELAY_URL_ARG="$2"; shift 2 ;;
    --upstream) UPSTREAM="$2"; shift 2 ;;
    --out) OUTDIR="$2"; shift 2 ;;
    --suites) SUITES="$2"; shift 2 ;;
    --shim) USE_SHIM=1; shift ;;
    --pg) USE_PG=1; shift ;;
    --expect) EXPECT="$2"; shift 2 ;;
    --update-expect) UPDATE_EXPECT=1; shift ;;
    --no-gate) GATE=0; shift ;;
    --no-retry) RETRY=0; shift ;;
    --suite-timeout) SUITE_TIMEOUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,40p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 3 ;;
  esac
done

die() { echo "buzz-conformance: $*" >&2; exit 3; }

command -v cargo >/dev/null || die "cargo not on PATH"
[ -d "$UPSTREAM/crates/buzz-test-client/tests" ] || die "no buzz-test-client tests under $UPSTREAM"
[ -f "$EXPECT" ] || die "expectations file not found: $EXPECT"

TIMEOUT_BIN="$(command -v timeout || command -v gtimeout || true)"

UPSTREAM_HEAD="$(git -C "$UPSTREAM" rev-parse HEAD)"
if [ -n "$(git -C "$UPSTREAM" status --porcelain)" ]; then
  die "upstream checkout $UPSTREAM is dirty; refusing to run (results would not be upstream's)"
fi

# Upstream pins its own toolchain; --manifest-path from elsewhere would pick up
# OUR rust-toolchain instead, so read theirs and pass it explicitly.
CHANNEL="$(sed -n 's/^ *channel *= *"\(.*\)"/\1/p' "$UPSTREAM/rust-toolchain.toml" 2>/dev/null | head -1)"
TOOLCHAIN_ARG=""
[ -n "$CHANNEL" ] && TOOLCHAIN_ARG="+$CHANNEL"

# Never write into the upstream tree.
export CARGO_TARGET_DIR="${BUZZ_CONFORMANCE_TARGET:-/tmp/buzz-conformance-target}"

if [ -z "$OUTDIR" ]; then OUTDIR="$(mktemp -d /tmp/buzz-conformance.XXXXXX)"; fi
mkdir -p "$OUTDIR"

# Optional namespace adapter.
SHIM_PID=""
cleanup() {
  if [ -n "$SHIM_PID" ]; then kill "$SHIM_PID" 2>/dev/null; fi
}
trap cleanup EXIT

if [ "$USE_PG" = "1" ]; then
  pg_export="$("$SUPPORT/pg-shim.sh" up)" || die "pg-shim failed"
  eval "$pg_export"
  export DATABASE_URL
  echo "  seed db       : $DATABASE_URL"
fi

TARGET_URL="$RELAY_URL_ARG"
if [ "$USE_SHIM" = "1" ]; then
  command -v node >/dev/null || die "--shim needs node on PATH"
  SHIM_PORT="${BUZZ_CONFORMANCE_SHIM_PORT:-38443}"
  node "$SUPPORT/shim.mjs" --port "$SHIM_PORT" --upstream-relay "$RELAY_URL_ARG" \
      >"$OUTDIR/shim.log" 2>&1 &
  SHIM_PID=$!
  for _ in $(seq 1 40); do
    if curl -fsS -m 2 "http://127.0.0.1:$SHIM_PORT/buzz/info" >/dev/null 2>&1; then break; fi
    sleep 0.25
  done
  curl -fsS -m 2 "http://127.0.0.1:$SHIM_PORT/buzz/info" >/dev/null 2>&1 \
    || die "shim did not come up; see $OUTDIR/shim.log"
  TARGET_URL="ws://127.0.0.1:$SHIM_PORT/buzz"
fi

if [ -z "$SUITES" ]; then
  # Default: every suite that has at least one row we can meaningfully judge.
  # A suite whose every row is `untranslatable` or `upstream-stub` is skipped --
  # running it costs time and can only ever produce the failures already recorded.
  SUITES="$(awk -F'\t' '$1 !~ /^#/ && NF>=4 && $4 != "untranslatable" && $4 != "upstream-stub" {print $1}' \
              "$EXPECT" | sort -u | tr '\n' ' ')"
fi

echo "buzz-conformance"
echo "  upstream      : $UPSTREAM @ $UPSTREAM_HEAD"
echo "  toolchain     : ${CHANNEL:-<inherited>}"
echo "  relay         : $RELAY_URL_ARG"
[ "$USE_SHIM" = "1" ] && echo "  via shim      : $TARGET_URL"
echo "  target dir    : $CARGO_TARGET_DIR"
echo "  output        : $OUTDIR"
echo "  suites        : $SUITES"
echo

# Build once so per-suite timings are run time, not compile time.
# shellcheck disable=SC2086
cargo $TOOLCHAIN_ARG test --manifest-path "$UPSTREAM/Cargo.toml" -p buzz-test-client --no-run \
  >"$OUTDIR/build.log" 2>&1 || { tail -30 "$OUTDIR/build.log"; die "upstream test build failed"; }

RESULTS="$OUTDIR/results.tsv"
: >"$RESULTS"

RELAY_HTTP_URL_DERIVED="$(printf '%s' "$TARGET_URL" | sed -e 's#^wss://#https://#' -e 's#^ws://#http://#' -e 's#/$##')"

for suite in $SUITES; do
  # Tests the expectations file marks SKIP are not run at all: each one names a
  # reason, and running them anyway costs minutes (subscription-limit floods) or
  # needs infrastructure we do not have (S3, an LLM endpoint).
  skips=""
  while IFS=$'\t' read -r s t exp _class _note; do
    [ "$s" = "$suite" ] || continue
    [ "$exp" = "SKIP" ] || continue
    skips="$skips --skip $t"
  done < <(grep -v '^#' "$EXPECT")

  echo "== $suite"
  log="$OUTDIR/$suite.log"
  # shellcheck disable=SC2086
  RELAY_URL="$TARGET_URL" \
  RELAY_HTTP_URL="$RELAY_HTTP_URL_DERIVED" \
  RELAY_URL_A="$RELAY_HTTP_URL_DERIVED" \
  RELAY_URL_B="$RELAY_HTTP_URL_DERIVED" \
  ${TIMEOUT_BIN:+$TIMEOUT_BIN "$SUITE_TIMEOUT"} \
    cargo $TOOLCHAIN_ARG test --manifest-path "$UPSTREAM/Cargo.toml" -p buzz-test-client \
      --test "$suite" -- --ignored --test-threads=1 $skips \
    >"$log" 2>&1
  rc=$?
  if [ "$rc" -ge 124 ] && [ -n "$TIMEOUT_BIN" ]; then
    echo "   (suite timed out after ${SUITE_TIMEOUT}s)" | tee -a "$log"
  fi
  python3 "$SUPPORT/parse.py" "$suite" "$log" >>"$RESULTS" \
    || die "could not parse the results of $suite ($log); nothing was gated or rewritten"
  tail -1 "$log" | sed 's/^/   /'
done

# One retry for tests the baseline says must pass.
# Not flake-hiding: the retry is scoped to tests the baseline already asserts
# pass, it is announced, and a test that fails twice still reds the gate. Every
# flake seen so far is a `collect_until_eose` timeout on a 5s budget across an
# ssh tunnel, and the retried test is re-run alone.
if [ "$RETRY" = "1" ]; then
  retry_list="$(python3 - "$RESULTS" "$EXPECT" <<'PY'
import sys
res, exp = sys.argv[1], sys.argv[2]
want = {}
for line in open(exp):
    if line.startswith("#") or not line.strip():
        continue
    f = line.rstrip("\n").split("\t")
    if len(f) >= 3:
        want[(f[0], f[1])] = f[2]
for line in open(res):
    f = line.rstrip("\n").split("\t")
    if len(f) >= 3 and f[2] != "PASS" and want.get((f[0], f[1])) == "PASS":
        print(f"{f[0]} {f[1]}")
PY
)" || die "could not compute the retry list from $RESULTS and $EXPECT"
  if [ -n "$retry_list" ]; then
    echo
    echo "retrying $(printf '%s\n' "$retry_list" | wc -l | tr -d ' ') test(s) that the baseline says must pass:"
    while read -r suite test; do
      [ -n "$suite" ] || continue
      echo "   $suite::$test"
      rlog="$OUTDIR/retry-$suite-$test.log"
      RELAY_URL="$TARGET_URL" RELAY_HTTP_URL="$RELAY_HTTP_URL_DERIVED" \
      RELAY_URL_A="$RELAY_HTTP_URL_DERIVED" RELAY_URL_B="$RELAY_HTTP_URL_DERIVED" \
      ${TIMEOUT_BIN:+$TIMEOUT_BIN 300} \
        cargo $TOOLCHAIN_ARG test --manifest-path "$UPSTREAM/Cargo.toml" -p buzz-test-client \
          --test "$suite" -- --ignored --exact "$test" >"$rlog" 2>&1
      if grep -q "^test $test \.\.\. ok" "$rlog"; then
        python3 - "$RESULTS" "$suite" "$test" <<'PY'
import sys
res, suite, test = sys.argv[1], sys.argv[2], sys.argv[3]
out = []
for line in open(res):
    f = line.rstrip("\n").split("\t")
    if len(f) >= 3 and f[0] == suite and f[1] == test:
        f[2] = "PASS"
        f[3] = "(passed on retry; failed in the full sequential run)"
        line = "\t".join(f[:4]) + "\n"
    out.append(line)
open(res, "w").writelines(out)
PY
        [ $? -eq 0 ] || die "could not record the retry result of $suite::$test in $RESULTS"
        echo "      -> passed on retry"
      else
        echo "      -> failed again"
      fi
    done <<<"$retry_list"
  fi
fi

if [ -n "$(git -C "$UPSTREAM" status --porcelain)" ]; then
  die "upstream checkout became dirty during the run -- this is a bug in this script"
fi
[ "$(git -C "$UPSTREAM" rev-parse HEAD)" = "$UPSTREAM_HEAD" ] || die "upstream HEAD moved during the run"

echo
if [ "$UPDATE_EXPECT" = "1" ]; then
  python3 "$SUPPORT/gate.py" --results "$RESULTS" --expect "$EXPECT" --update \
    || die "expectations were NOT rewritten: gate.py --update failed for $EXPECT"
  echo "expectations rewritten: $EXPECT"
  exit 0
fi

python3 "$SUPPORT/gate.py" --results "$RESULTS" --expect "$EXPECT" --report "$OUTDIR/report.txt"
gate_rc=$?
echo
echo "raw logs   : $OUTDIR"
echo "results    : $RESULTS"
echo "report     : $OUTDIR/report.txt"
[ "$GATE" = "1" ] || exit 0
exit "$gate_rc"
