#!/usr/bin/env bash
# provision.sh — apply Lambo schema to CockroachDB Cloud (spec §12.1 / T0.2).
#
# Prerequisites:
#   - .env with LAMBO_COCKROACH_DSN (never commit .env)
#   - docker (preferred) or psql
#
# Usage:
#   ./scripts/provision.sh
#   ./scripts/provision.sh --check
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -f .env ]]; then
  set -a
  # shellcheck disable=SC1091
  source .env
  set +a
fi

DSN="${LAMBO_COCKROACH_DSN:-}"
if [[ -z "$DSN" ]]; then
  echo "error: LAMBO_COCKROACH_DSN is not set (copy .env.example → .env)" >&2
  exit 1
fi

MIGRATION="$ROOT/migrations/cockroach/001_init.sql"
if [[ ! -f "$MIGRATION" ]]; then
  echo "error: missing $MIGRATION" >&2
  exit 1
fi

# Schema surface applied from $MIGRATION (single source of truth; the splitter
# below routes each CREATE to BASE_SQL). Tables: sessions, interactions,
# concepts, edges, synonyms, canonization_events, reservations, and — since
# T8.6 — session_leases (the store-enforced single-writer lease, spec §2.2).
#
# Operator override for a wedged-but-still-heartbeating writer that will not let
# go of a session (T8.6 documents this manual escape; there is no auto-preempt):
#   DELETE FROM session_leases WHERE session_id = '<session>';
# The next writer's acquire then wins; it replays from durable state, so the
# wedged holder's un-flushed tail is lost exactly as on any crash.

run_sql() {
  local sql="$1"
  if command -v docker >/dev/null 2>&1; then
    docker run --rm -i postgres:16-alpine psql "$DSN" -v ON_ERROR_STOP=1 -c "$sql"
  elif command -v psql >/dev/null 2>&1; then
    psql "$DSN" -v ON_ERROR_STOP=1 -c "$sql"
  else
    echo "error: need docker or psql" >&2
    exit 1
  fi
}

run_sql_file() {
  local file="$1"
  if command -v docker >/dev/null 2>&1; then
    docker run --rm -i postgres:16-alpine psql "$DSN" -v ON_ERROR_STOP=1 <"$file"
  elif command -v psql >/dev/null 2>&1; then
    psql "$DSN" -v ON_ERROR_STOP=1 -f "$file"
  else
    echo "error: need docker or psql" >&2
    exit 1
  fi
}

# Raw single-value query (no table formatting) for state checks.
run_sql_value() {
  local sql="$1"
  if command -v docker >/dev/null 2>&1; then
    docker run --rm -i postgres:16-alpine psql "$DSN" -At -c "$sql"
  elif command -v psql >/dev/null 2>&1; then
    psql "$DSN" -At -c "$sql"
  else
    echo "error: need docker or psql" >&2
    exit 1
  fi
}

# ---------------------------------------------------------------------------
# Vector index state (T7.4) — spec §12.1 depends on this being PARTIAL.
#
# `concepts_embedding_idx` MUST be partial on `embedding IS NOT NULL`. Against a
# NON-partial index the optimizer cannot prove the production query's predicate
# is implied by the index, so it plans a FULL SCAN and the "we used CockroachDB
# vector indexing" claim silently becomes false. Nothing errors; the plan just
# quietly changes. That is why this script checks rather than assumes.
#
# This conditional logic lives HERE and not in migrations/cockroach/001_init.sql
# on purpose: that file is embedded as INIT_SQL and re-executed by
# CockroachStore::init_schema() over a pool with a hard 20s statement_timeout,
# while CREATE VECTOR INDEX takes ~85-96s. Slow or destructive schema work must
# stay in this script, which runs under psql with no such timeout. CockroachDB
# also has no DO blocks, and the splitter above rejects dollar-quoting, so this
# cannot be expressed in the migration at all.
#
# Echoes exactly one of: absent | partial | legacy
# Matching notes (adve-review MINOR-3): the predicate is read from DDL text
# because there is no catalog alternative — `crdb_internal` is blocked on
# CockroachDB Cloud ("Access to crdb_internal and system is restricted") and
# `SHOW INDEXES` exposes no partial-predicate column. So the text match is made
# robust instead of clever:
#   * the whole CREATE TABLE is canonicalized to ONE lowercase line with
#     whitespace collapsed, so a predicate that wraps onto another line, or
#     changes case, still matches;
#   * the predicate is then looked for in a BOUNDED window immediately after
#     this index's name, so another partial index on the same table
#     (concepts_key_non_obs_idx has its own WHERE) cannot false-positive it.
# Worst case if CockroachDB reformats beyond recognition: a misclassification
# rebuilds the index once and then the final verification gate fails LOUDLY.
# It cannot degrade into a silent full-scan cluster, which is the failure this
# whole mechanism exists to prevent.
vector_index_state() {
  local create_stmt canon after
  create_stmt="$(run_sql_value "SELECT create_statement FROM [SHOW CREATE TABLE concepts];" 2>/dev/null || true)"
  # Lowercase + collapse all whitespace (newlines included) into single spaces.
  canon="$(printf '%s' "${create_stmt,,}" | tr -s '[:space:]' ' ')"
  if [[ "$canon" != *"concepts_embedding_idx"* ]]; then
    echo "absent"
    return
  fi
  # Text following this index's name, bounded so a later index's WHERE cannot
  # be attributed to this one. The pinned shape is
  #   vector index concepts_embedding_idx (embedding vector_l2_ops) where embedding is not null
  # which is ~50 chars of column spec before the predicate.
  after="${canon#*concepts_embedding_idx}"
  after="${after:0:120}"
  if [[ "$after" == *"where embedding is not null"* ]]; then
    echo "partial"
  else
    echo "legacy"
  fi
}

if [[ "${1:-}" == "--check" ]]; then
  echo "== tables =="
  run_sql "SHOW tables;"
  echo "== indexes on concepts =="
  run_sql "SHOW INDEXES FROM concepts;" || true
  exit 0
fi

echo "== enabling vector index feature (ignore if unsupported) =="
run_sql "SET CLUSTER SETTING feature.vector_index.enabled = true;" 2>/dev/null \
  || echo "(cluster setting not available or already set — continuing)"

# Split migration into base DDL (no VECTOR INDEX) + vector index so a
# vector-index failure can be retried without blocking tables.
BASE_SQL="$(mktemp)"
VINDEX_SQL="$(mktemp)"
trap 'rm -f "$BASE_SQL" "$VINDEX_SQL"' EXIT
# Fail-fast: the splitter below only tracks -- line comments and
#'...'/"..." quoted strings. CockroachDB also supports $$...$$ and $tag$...$tag$
#dollar-quoted strings and /* ... */ block comments, and a ';' inside any of
#those would tear a statement in half — so abort loudly instead of splitting
#incorrectly. (The $tag$ alternative also matches $$ since the tag is optional.)
if grep -Eq '\$\$|\$[A-Za-z_][A-Za-z0-9_]*\$|/\*' "$MIGRATION"; then
  echo "error: provision.sh splitter does not support dollar-quoted strings or block comments in the migration; remove them or extend the splitter" >&2
  exit 1
fi

# Statement-aware split: a statement runs from the previous terminator up to
# a ';' that is NOT inside a line comment (--) or a single/double-quoted
# string, so multi-line statements (e.g. a reformatted CREATE VECTOR INDEX)
# stay whole and ';' inside those constructs can never tear one in half.
# Each complete statement (terminating ';' included) is routed by its first
# keyword.
route_statement() {
  local stmt="$1" body="" line
  # Routing test only — skip leading comment-only lines so a comment cannot
  # change the outcome (e.g. "-- ... CREATE VECTOR INDEX ..." above a CREATE
  # TABLE must not send the table to the vector file).
  while IFS= read -r line; do
    [[ "$line" =~ ^[[:space:]]*-- ]] && continue
    body+="$line "
  done <<<"$stmt"
  body="${body,,}"
  if [[ "$body" =~ ^[[:space:]]*create[[:space:]]+vector[[:space:]]+index ]]; then
    printf '%s\n' "${stmt%$'\n'}" >>"$VINDEX_SQL"
  else
    printf '%s\n' "${stmt%$'\n'}" >>"$BASE_SQL"
  fi
}

stmt=""
in_comment=0
in_squote=0
in_dquote=0
while IFS= read -r line || [[ -n "$line" ]]; do
  i=0
  len=${#line}
  while (( i < len )); do
    ch="${line:i:1}"
    if (( in_comment )); then
      stmt+="$ch"
      i=$((i + 1))
      continue
    fi
    if (( in_squote )); then
      stmt+="$ch"
      if [[ "$ch" == "'" ]]; then
        if (( i + 1 < len )) && [[ "${line:i+1:1}" == "'" ]]; then
          stmt+="'"
          i=$((i + 2))
          continue
        fi
        in_squote=0
      fi
      i=$((i + 1))
      continue
    fi
    if (( in_dquote )); then
      stmt+="$ch"
      if [[ "$ch" == '"' ]]; then
        if (( i + 1 < len )) && [[ "${line:i+1:1}" == '"' ]]; then
          stmt+='"'
          i=$((i + 2))
          continue
        fi
        in_dquote=0
      fi
      i=$((i + 1))
      continue
    fi
    if [[ "$ch" == "-" ]] && (( i + 1 < len )) && [[ "${line:i+1:1}" == "-" ]]; then
      stmt+="--"
      in_comment=1
      i=$((i + 2))
      continue
    fi
    if [[ "$ch" == "'" ]]; then
      in_squote=1
      stmt+="$ch"
      i=$((i + 1))
      continue
    fi
    if [[ "$ch" == '"' ]]; then
      in_dquote=1
      stmt+="$ch"
      i=$((i + 1))
      continue
    fi
    if [[ "$ch" == ";" ]]; then
      route_statement "$stmt;"
      stmt=""
      i=$((i + 1))
      continue
    fi
    stmt+="$ch"
    i=$((i + 1))
  done
  in_comment=0
  stmt+=$'\n'
done <"$MIGRATION"

# Trailing statement without a terminating ';' (should not happen).
if [[ -n "${stmt//[[:space:]]/}" ]]; then
  route_statement "$stmt"
fi

echo "== applying base tables/indexes from $MIGRATION =="
run_sql_file "$BASE_SQL"

# adve-review NIT-7: the vector-index apply AND its verification are both
# guarded by `-s "$VINDEX_SQL"`, so a routing bug that produced an empty vector
# file would skip both — silently yielding exactly the full-scan cluster this
# script exists to prevent. Assert the routing instead of trusting it: if the
# migration declares a vector index, the splitter must have captured one.
if grep -Eiq '^[[:space:]]*create[[:space:]]+vector[[:space:]]+index' "$MIGRATION" && [[ ! -s "$VINDEX_SQL" ]]; then
  echo "error: $MIGRATION declares a CREATE VECTOR INDEX but the splitter routed none." >&2
  echo "       Refusing to continue: provisioning would skip both the index and its" >&2
  echo "       verification, leaving a cluster that silently plans a FULL SCAN." >&2
  exit 1
fi

if [[ -s "$VINDEX_SQL" ]]; then
  # Upgrade a pre-T7.4 cluster. `CREATE VECTOR INDEX IF NOT EXISTS ... WHERE ...`
  # matches on NAME ONLY: against a legacy non-partial index of the same name it
  # reports CREATE INDEX, succeeds in ~1s, and changes NOTHING. It does not even
  # error. So the legacy index must be dropped first, and only this script can
  # do it (see vector_index_state above for why not the migration).
  state="$(vector_index_state)"
  case "$state" in
    legacy)
      echo "== dropping legacy NON-partial concepts_embedding_idx (one-time upgrade) =="
      echo "   (drop ~3s, rebuild ~85-96s — do not interrupt)"
      run_sql "DROP INDEX IF EXISTS concepts@concepts_embedding_idx;"
      ;;
    partial) echo "== vector index already partial — nothing to upgrade ==" ;;
    absent)  echo "== no vector index yet — creating ==" ;;
  esac

  echo "== applying vector index =="
  if ! run_sql_file "$VINDEX_SQL"; then
    echo "warning: vector index apply failed — retrying without IF NOT EXISTS" >&2
    # Fallback for older CRDB without IF NOT EXISTS on vector indexes.
    # The WHERE clause is NOT optional: dropping it here would silently
    # reinstate the FULL SCAN plan and falsify the §12.1 claim (T7.4).
    run_sql "CREATE VECTOR INDEX concepts_embedding_idx ON concepts (embedding) WHERE embedding IS NOT NULL;" 2>/dev/null \
      || echo "warning: vector index may already exist or be unavailable" >&2
  fi
fi

echo "== verify =="
run_sql "SHOW tables;"
run_sql "SHOW INDEXES FROM concepts;" || true

# Fail loudly rather than hand back a cluster that plans a FULL SCAN. Without
# this gate the failure mode is silent: everything "works", only slower, and the
# §12.1 vector-indexing claim is quietly untrue until someone runs the camera
# proof. Skipped when the migration ships no vector index at all.
if [[ -s "$VINDEX_SQL" ]]; then
  final_state="$(vector_index_state)"
  if [[ "$final_state" != "partial" ]]; then
    echo "error: concepts_embedding_idx is '$final_state', expected 'partial'." >&2
    echo "       A non-partial vector index makes the planner choose a FULL SCAN," >&2
    echo "       which silently falsifies the spec §12.1 vector-index claim." >&2
    echo "       Fix: DROP INDEX IF EXISTS concepts@concepts_embedding_idx; then re-run." >&2
    exit 1
  fi
  echo "vector index verified PARTIAL (spec §12.1 plan is vector search)."
fi
echo "provision complete."
