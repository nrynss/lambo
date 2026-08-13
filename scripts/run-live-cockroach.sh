#!/usr/bin/env bash
# run-live-cockroach.sh — run the LIVE CockroachDB tests against the Lambo
# cloud cluster (the same cluster CI's `cockroach-live` job uses), locally.
#
# Loads LAMBO_COCKROACH_DSN from `.env` (falling back to the environment) so a
# fresh shell runs against the provisioned cloud cluster — NOT the docker
# container, which is unused by these tests.
#
# Sets LAMBO_REQUIRE_LIVE=1 so a missing DSN or dead cluster is a hard failure
# (dsn_or_skip panics), never a silent skip-as-green.
#
# Usage:
#   ./scripts/run-live-cockroach.sh          # full live suite
#   DSN_OVERRIDE=... ./scripts/run-live-cockroach.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -n "${DSN_OVERRIDE:-}" ]]; then
  DSN="$DSN_OVERRIDE"
elif [[ -n "${LAMBO_COCKROACH_DSN:-}" ]]; then
  DSN="$LAMBO_COCKROACH_DSN"
elif [[ -f "$ROOT/.env" ]]; then
  # shellcheck disable=SC2002
  DSN="$(sed -n 's/^LAMBO_COCKROACH_DSN="\?\([^"\n]*\)"\?/\1/p' "$ROOT/.env" | head -n1)"
else
  echo "error: no LAMBO_COCKROACH_DSN (set it or DSN_OVERRIDE, or add .env)" >&2
  exit 2
fi

if [[ -z "$DSN" ]]; then
  echo "error: resolved LAMBO_COCKROACH_DSN is empty" >&2
  exit 2
fi

# Redacted description of the target so the transcript never prints the secret.
HOST="$(printf '%s' "$DSN" | sed 's#.*@##; s#/.*##' 2>/dev/null)"
echo "Running live CockroachDB tests against host: $HOST"

export LAMBO_COCKROACH_DSN="$DSN"
export LAMBO_REQUIRE_LIVE=1

# Full live CRDB scope: conformance suite + canon three-hop progression + any
# vector EXPLAIN live test. `-- --ignored` runs every ignored test, so also set
# LAMBO_LLAMA_EMBED_URL to '' guard — the BGE live test honest-skips without it.
export LAMBO_LLAMA_EMBED_URL="${LAMBO_LLAMA_EMBED_URL:-}"

exec cargo test --features store-cockroach,store-memory,fixtures --lib -- --ignored
