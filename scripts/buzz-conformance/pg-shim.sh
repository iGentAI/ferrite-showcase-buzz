#!/usr/bin/env bash
# Stand up the throwaway Postgres that buzz's e2e_relay suite seeds members into.
#
# WHY
# ---
# `e2e_relay::seed_relay_member` is not an HTTP call: it is a raw `sqlx` INSERT
# into buzz's own `communities` / `relay_members` tables. Three tests call it,
# and without a reachable Postgres they die at `connect to e2e Postgres:
# PoolTimedOut` before they ever touch the relay -- i.e. they tell you nothing
# about our conformance. Giving them a real, empty, throwaway Postgres removes
# that harness dependency without editing or weakening a single assertion: the
# tests then run all the way to the relay and fail (or pass) for a reason that
# is about the relay.
#
# WHAT IT DOES *NOT* DO
# ---------------------
# It does not make the seeded pubkey an actual member of our relay's community.
# Our relay keeps membership in its own Store (`keys::community_mem`) and the
# moderator set in the config store under `buzz-community-moderators`, and
# NOTHING outside the app can write either one today -- there is no operator
# route yet. So after this shim,
# `test_invite_mint_and_claim_admits_new_pubkey` reaches the mint call and is
# refused with the honest `restricted: only a community moderator may create
# invites` instead of dying on a database timeout.
#
# When that operator route lands, set BUZZ_CONFORMANCE_OPERATOR_URL and
# BUZZ_CONFORMANCE_OPERATOR_TOKEN and this script will also register the owner
# pubkey as a community moderator -- see `register_owner()` at the bottom, which
# is the one function that needs the route.
#
# Usage:
#   eval "$(scripts/buzz-conformance/pg-shim.sh up)"   # prints exports
#   scripts/buzz-conformance/pg-shim.sh down
#   scripts/buzz-conformance/pg-shim.sh status
set -uo pipefail

PGROOT="${BUZZ_CONFORMANCE_PGROOT:-/tmp/buzz-conformance-pg}"
PGDATA="$PGROOT/data"
PGPORT="${BUZZ_CONFORMANCE_PGPORT:-55432}"
PGUSER_NAME="buzz"
PGDB="buzz"

find_bin() {
  for c in \
    "/opt/homebrew/opt/postgresql@18/bin/$1" \
    "/opt/homebrew/opt/postgresql@17/bin/$1" \
    "/opt/homebrew/opt/postgresql/bin/$1" \
    "$(command -v "$1" 2>/dev/null)"; do
    [ -n "$c" ] && [ -x "$c" ] && { echo "$c"; return 0; }
  done
  return 1
}

INITDB="$(find_bin initdb)" || { echo "pg-shim: no initdb found" >&2; exit 3; }
PGCTL="$(find_bin pg_ctl)" || { echo "pg-shim: no pg_ctl found" >&2; exit 3; }
PSQL="$(find_bin psql)" || { echo "pg-shim: no psql found" >&2; exit 3; }

# `postgres` and `psql` from libpq-only installs disagree; pg_ctl needs the real
# server binary next to it.
SERVER_DIR="$(dirname "$PGCTL")"
[ -x "$SERVER_DIR/postgres" ] || { echo "pg-shim: $SERVER_DIR has no postgres server binary" >&2; exit 3; }

SCHEMA=$(cat <<'SQL'
SET client_min_messages TO WARNING;
-- Exactly the two tables e2e_relay's seed helpers touch, and the expression
-- unique index its ON CONFLICT clause names. Nothing else: this database is a
-- landing pad for the harness, not a model of buzz's schema.
CREATE TABLE IF NOT EXISTS communities (
    id   uuid PRIMARY KEY,
    host text NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS communities_host_lower ON communities (lower(host));
CREATE TABLE IF NOT EXISTS relay_members (
    community_id uuid        NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
    pubkey       text        NOT NULL,
    role         text        NOT NULL,
    added_by     text,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (community_id, pubkey)
);
SQL
)

is_up() { "$PGCTL" -D "$PGDATA" status >/dev/null 2>&1; }

up() {
  mkdir -p "$PGROOT"
  if [ ! -f "$PGDATA/PG_VERSION" ]; then
    "$INITDB" -D "$PGDATA" -U "$PGUSER_NAME" --auth=trust --encoding=UTF8 >"$PGROOT/initdb.log" 2>&1 \
      || { echo "pg-shim: initdb failed, see $PGROOT/initdb.log" >&2; exit 3; }
  fi
  if ! is_up; then
    "$PGCTL" -D "$PGDATA" -l "$PGROOT/server.log" \
      -o "-p $PGPORT -k $PGROOT -c listen_addresses=127.0.0.1" -w start >/dev/null 2>&1 \
      || { echo "pg-shim: server failed to start, see $PGROOT/server.log" >&2; exit 3; }
  fi
  "$PSQL" -h 127.0.0.1 -p "$PGPORT" -U "$PGUSER_NAME" -d postgres -tAc \
    "SELECT 1 FROM pg_database WHERE datname='$PGDB'" 2>/dev/null | grep -q 1 \
    || "$PSQL" -h 127.0.0.1 -p "$PGPORT" -U "$PGUSER_NAME" -d postgres -c "CREATE DATABASE $PGDB" >/dev/null
  printf '%s\n' "$SCHEMA" | "$PSQL" -h 127.0.0.1 -p "$PGPORT" -U "$PGUSER_NAME" -d "$PGDB" -q >/dev/null

  echo "export DATABASE_URL=postgres://$PGUSER_NAME@127.0.0.1:$PGPORT/$PGDB"
}

reset() {
  "$PSQL" -h 127.0.0.1 -p "$PGPORT" -U "$PGUSER_NAME" -d "$PGDB" -q \
    -c "TRUNCATE relay_members, communities CASCADE" >/dev/null 2>&1
}

down() {
  is_up && "$PGCTL" -D "$PGDATA" -m fast -w stop >/dev/null 2>&1
  echo "pg-shim: stopped (data left in $PGDATA; delete it by hand if you want a clean slate)"
}

# The one thing that needs a relay operator route. Until that route exists this
# is a no-op that says so, loudly, so a green run cannot be mistaken for one
# where the owner was really an owner.
register_owner() {
  local pubkey="$1"
  if [ -z "${BUZZ_CONFORMANCE_OPERATOR_URL:-}" ]; then
    echo "pg-shim: NOT registering $pubkey as a community moderator --" >&2
    echo "         the relay has no operator route yet, so buzz-community-moderators" >&2
    echo "         cannot be written from outside the app. Invite-mint tests will be" >&2
    echo "         refused with 'only a community moderator may create invites'." >&2
    return 1
  fi
  curl -fsS -X POST "$BUZZ_CONFORMANCE_OPERATOR_URL/moderators" \
    -H "Authorization: Bearer ${BUZZ_CONFORMANCE_OPERATOR_TOKEN:-}" \
    -H 'content-type: application/json' \
    -d "{\"pubkey\":\"$pubkey\"}"
}

case "${1:-up}" in
  up) up ;;
  down) down ;;
  reset) reset ;;
  status) is_up && echo "pg-shim: running on 127.0.0.1:$PGPORT" || echo "pg-shim: not running" ;;
  register-owner) register_owner "${2:?pubkey}" ;;
  *) echo "usage: pg-shim.sh {up|down|reset|status|register-owner <pubkey>}" >&2; exit 3 ;;
esac
