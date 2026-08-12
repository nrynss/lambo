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

if [[ -s "$VINDEX_SQL" ]]; then
  echo "== applying vector index =="
  if ! run_sql_file "$VINDEX_SQL"; then
    echo "warning: vector index apply failed — retrying without IF NOT EXISTS" >&2
    # fallback for older CRDB without IF NOT EXISTS on vector indexes
    run_sql "CREATE VECTOR INDEX concepts_embedding_idx ON concepts (embedding);" 2>/dev/null \
      || echo "warning: vector index may already exist or be unavailable" >&2
  fi
fi

echo "== verify =="
run_sql "SHOW tables;"
run_sql "SHOW INDEXES FROM concepts;" || true
echo "provision complete."
