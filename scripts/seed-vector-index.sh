#!/usr/bin/env bash
# seed-vector-index.sh — make the CockroachDB optimizer choose `vector search`
# so the §12.1 vector-index camera-proof can be captured (T7.3 open item).
#
# STATUS (T7.4, 2026-08-13): **NOT REQUIRED FOR THE CAMERA-PROOF.** Seeding is
# now an optional load/benchmark tool, nothing more. Do not run it before a demo
# expecting it to make the proof pass — it is not what makes the proof pass.
#
# WHY THIS EXISTS, AND WHY ITS ORIGINAL PREMISE WAS WRONG
# -------------------------------------------------------
# This script was written on the theory that `vector_explain_camera_proof` failed
# because the optimizer cost-rejected the vector index on a small, low-diversity
# table: 118 embedded concepts but only **4 distinct vectors**, because the test
# fixtures reuse FixtureEmbedder's NEAR_A / NEAR_B / FAR / NEAR_PAIR constants.
# Seeding 2000 genuinely distinct vectors was supposed to tip the cost model.
#
# That theory was FALSIFIED by T7.4. The camera-proof was failing for two
# unrelated reasons that had nothing to do with data volume or diversity:
#   1. the test asserted the spaced token `vector search` against
#      `EXPLAIN (OPT, VERBOSE)`, which spells the operator `vector-search`; and
#   2. the query's `WHERE embedding IS NOT NULL` cannot be proven implied by a
#      NON-partial vector index, so the planner chose a FULL SCAN.
# Making `concepts_embedding_idx` PARTIAL on that predicate fixed (2) with no
# query change. MEASURED AFTER THE FIX, with this seed session REMOVED (--clean)
# and NO manual ANALYZE — 858 concepts, 123 embedded, still only 4 distinct
# vectors — the plan is `vector search` on
# `concepts@concepts_embedding_idx (partial index)`. So the seed was neither
# necessary nor sufficient. Evidence:
# dev-diary/evidence/20260813-134333-vector-index-camera-proof-PASSING.txt
#
# T7.4 therefore left the demo cluster UNSEEDED (--clean applied): the §12.1
# claim is stronger when the plan is captured against ordinary demo-shaped data
# rather than 2000 rows of synthetic noise nobody can account for on camera.
#
# The rows land in ONE dedicated session (default `vector-index-seed`) so they
# are trivially removable with --clean and never mix into a demo session.
#
# Usage:
#   ./scripts/seed-vector-index.sh                 # seed 2000 rows, then ANALYZE
#   ./scripts/seed-vector-index.sh --count 5000
#   ./scripts/seed-vector-index.sh --session my-seed
#   ./scripts/seed-vector-index.sh --status        # report, change nothing
#   ./scripts/seed-vector-index.sh --clean         # delete the seed session
#
# The camera-proof does NOT depend on any of the above; it needs only the partial
# vector index from migrations/cockroach/001_init.sql:
#   ./scripts/provision.sh
#   LAMBO_REQUIRE_VECTOR_INDEX=1 ./scripts/run-live-cockroach.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

COUNT=2000
SESSION="vector-index-seed"
MODE="seed"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --count)   COUNT="${2:?--count needs a value}"; shift 2 ;;
    --session) SESSION="${2:?--session needs a value}"; shift 2 ;;
    --clean)   MODE="clean"; shift ;;
    --status)  MODE="status"; shift ;;
    -h|--help) sed -n '1,48p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [[ -f .env ]]; then
  set -a
  # shellcheck disable=SC1091
  source .env
  set +a
fi

DSN="${LAMBO_COCKROACH_DSN:-}"
if [[ -z "$DSN" ]]; then
  echo "error: LAMBO_COCKROACH_DSN is not set (copy .env.example -> .env)" >&2
  exit 1
fi

if ! command -v psql >/dev/null 2>&1; then
  echo "error: psql not found (provision.sh can use docker; this script needs psql)" >&2
  exit 1
fi

# Never print the DSN — redacted target only.
echo "target: $(printf '%s' "$DSN" | sed 's#.*@##; s#/.*##')  session=$SESSION"

sql() { psql "$DSN" -v ON_ERROR_STOP=1 "$@"; }

report() {
  sql -c "
SELECT
  (SELECT count(*) FROM concepts WHERE embedding IS NOT NULL)                     AS with_embedding,
  (SELECT count(DISTINCT embedding::string) FROM concepts WHERE embedding IS NOT NULL) AS distinct_vectors,
  (SELECT count(*) FROM concepts WHERE session_id = '$SESSION')                   AS seeded_rows;"
}

case "$MODE" in
  status)
    report
    exit 0
    ;;

  clean)
    # Order matters: concepts reference interactions, both reference sessions.
    sql -c "DELETE FROM concepts     WHERE session_id = '$SESSION';" \
        -c "DELETE FROM edges        WHERE session_id = '$SESSION';" \
        -c "DELETE FROM interactions WHERE session_id = '$SESSION';" \
        -c "DELETE FROM sessions     WHERE session_id = '$SESSION';" || exit 1
    echo "removed seed session '$SESSION'."
    report
    exit 0
    ;;
esac

# ---- seed -------------------------------------------------------------------
# One session + one origin interaction satisfy the FKs on concepts.
# canonical_key is unique per row: the partial unique index
# concepts_key_non_obs_idx covers concept_type <> 'Observation'.
echo "seeding $COUNT concepts with distinct VECTOR(1024) embeddings..."

sql <<SQL || { echo "seed failed" >&2; exit 1; }
INSERT INTO sessions (session_id, created_at)
VALUES ('$SESSION', now())
ON CONFLICT (session_id) DO NOTHING;

INSERT INTO interactions (id, session_id, agent_id, prompt_text, created_at)
SELECT '00000000-0000-4000-8000-000000000001'::UUID, '$SESSION', 'seed-agent',
       'vector index seed origin', now()
WHERE NOT EXISTS (
  SELECT 1 FROM interactions WHERE id = '00000000-0000-4000-8000-000000000001'::UUID
);

-- Vector construction, and why it looks like this:
--
--   A scalar subquery `(SELECT ... string_agg(random()) FROM generate_series(1,1024))`
--   is UNCORRELATED, so the optimizer hoists it and every row gets the SAME
--   vector — measured: 5 rows produced 1 distinct vector. That is precisely the
--   bug this script exists to fix, so it must not be reintroduced.
--
--   Correlating it by referencing the outer \`g\` inside the aggregate is rejected
--   ("column g must appear in the GROUP BY clause").
--
--   The working shape is a CROSS JOIN of the row series with the dimension
--   series, aggregated with GROUP BY g: random() is then evaluated once per
--   (row, dimension) cell. Verified 20 rows -> 20 distinct vectors.
INSERT INTO concepts (
  id, session_id, content, canonical_key, concept_type,
  origin_interaction, origin_agent, created_at, embedding
)
WITH dims AS (
  SELECT g, s
  FROM generate_series(1, $COUNT) g
  CROSS JOIN generate_series(1, 1024) s
),
vecs AS (
  SELECT g, ('[' || string_agg(random()::STRING, ',') || ']')::VECTOR(1024) AS e
  FROM dims
  GROUP BY g
)
SELECT
  gen_random_uuid(),
  '$SESSION',
  'seed concept ' || g,
  'seed-key-' || g,
  'Entity',
  '00000000-0000-4000-8000-000000000001'::UUID,
  'seed-agent',
  now(),
  e
FROM vecs
ON CONFLICT DO NOTHING;

-- Statistics drive the cost model that picks vector search over a scan.
-- Without this the planner keeps costing against pre-seed row counts.
ANALYZE concepts;
SQL

echo
echo "post-seed state:"
report
echo
echo "note: this seed is NOT required for the §12.1 camera-proof (T7.4) — the"
echo "      partial vector index is. Remove it again with --clean when done."
