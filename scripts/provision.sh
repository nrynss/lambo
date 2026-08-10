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

# Everything except CREATE VECTOR INDEX lines
grep -v -i 'CREATE VECTOR INDEX' "$MIGRATION" >"$BASE_SQL"
grep -i 'CREATE VECTOR INDEX' "$MIGRATION" >"$VINDEX_SQL" || true

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
