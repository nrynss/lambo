#!/usr/bin/env bash
# Fail when the differential-oracle vectors in formal/oracle/ are stale (#291).
#
# The vectors are a pure function of the Lean sources: `lake exe oracle`
# (formal/OracleExport/) evaluates the model definitions over generated
# inputs and writes one JSON file per model. This script regenerates them
# into a temporary directory and compares the result with the committed
# files, so a change to a model (or to the exporter) that changes its
# outputs must be committed together with the regenerated vectors, which
# src/formal_oracle_tests.rs then replays against the Rust.
#
# Usage, from anywhere in the repository (needs lake on PATH):
#   scripts/formal/check-oracle.sh
# To regenerate after a deliberate change:
#   (cd formal && lake exe oracle)
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root/formal"

lake build oracle
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
lake exe oracle "$tmp" > /dev/null

if ! diff -rq oracle "$tmp" > /dev/null; then
  echo "error: formal/oracle/ is stale: the Lean models now give different vectors." >&2
  echo "Regenerate with '(cd formal && lake exe oracle)', run the Rust oracle tests" >&2
  echo "(cargo test --lib formal_oracle_tests), and commit both. Differences:" >&2
  diff -ru oracle "$tmp" | head -n 60 >&2 || true
  exit 1
fi
echo "formal/oracle/ matches the Lean models ($(ls oracle | wc -l | tr -d ' ') vector files)"
