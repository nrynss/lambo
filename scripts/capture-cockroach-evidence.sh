#!/usr/bin/env bash
# capture-cockroach-evidence.sh — run the live Cockroach conformance suite and
# capture a timestamped evidence file (dev-diary/evidence/), matching the review
# evidence convention (see dev-diary/evidence/e2e-p0-p3-fable-gates.txt).
#
# Live tests are #[ignore]d by default (TEST-1): this script runs them via
# `-- --ignored` under LAMBO_REQUIRE_LIVE=1, so a missing DSN fails loudly
# (dsn_or_skip panics -> failed test) instead of silently skipping, and the
# run is scoped to the cockroach conformance tests (a bare `--ignored` would
# also pull in the BGE live tests, which need a llama.cpp server).
#
# Usage:
#   LAMBO_COCKROACH_DSN="postgresql://..." ./scripts/capture-cockroach-evidence.sh
#
# Exits non-zero on any failure or non-zero harness exit. Under
# LAMBO_REQUIRE_LIVE=1 skips are structurally impossible — dsn_or_skip panics
# instead — so any skip surfaces as a failed test and drives the non-zero exit.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

EV_DIR="$ROOT/dev-diary/evidence"
TS="$(date +%Y%m%d-%H%M%S)"
OUT="$EV_DIR/${TS}-cockroach-live.txt"

mkdir -p "$EV_DIR"

RUN_CMD=(cargo test --features store-cockroach -- --ignored cockroach::conformance)

{
  echo "Cockroach live conformance evidence — $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "Machine: $(uname -sm), $(rustc --version 2>/dev/null || echo 'rustc unavailable')"
  echo "Command: LAMBO_REQUIRE_LIVE=1 ${RUN_CMD[*]}"
  echo
  echo "=== run ==="
} > "$OUT"

LAMBO_REQUIRE_LIVE=1 "${RUN_CMD[@]}" 2>&1 | tee -a "$OUT"
STATUS=${PIPESTATUS[0]}

# Sum pass/fail/ignored across every `test result:` line emitted by the harnesses.
PASSED="$(grep -oE '[0-9]+ passed' "$OUT" | awk '{s+=$1} END {print s+0}')"
FAILED="$(grep -oE '[0-9]+ failed' "$OUT" | awk '{s+=$1} END {print s+0}')"
IGNORED="$(grep -oE '[0-9]+ ignored' "$OUT" | awk '{s+=$1} END {print s+0}')"

{
  echo
  echo "exit status: $STATUS"
  echo "passed: $PASSED / failed: $FAILED / ignored: $IGNORED"
} >> "$OUT"

if [[ $STATUS -ne 0 || $FAILED -gt 0 || $IGNORED -gt 0 ]]; then
  echo "FAILURE: live cockroach run was not fully green (exit=$STATUS, failed=$FAILED, ignored=$IGNORED) — evidence at $OUT" >&2
  exit 1
fi

echo "OK: live cockroach run fully green (passed=$PASSED, failed=0, ignored=0) — evidence at $OUT"

