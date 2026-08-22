#!/usr/bin/env bash
# F4 — burst + SIGTERM close-flush latency sweep against the live Cockroach
# cluster. One run: provision-less fresh scratch session, fire N record_action
# writes as fast as round-trips allow, SIGTERM the instant the burst finishes,
# time signal -> "session closed, tail durable" / process exit, capture exit
# code and the close-line verdict.
#
# Usage: f4_run.sh <N> <session> <outdir>
set -euo pipefail

REPO="/home/nryn/work/lambo"
BIN="$REPO/target/release/lambo"
N="$1"
SESSION="$2"
OUT="$3"

RUN="$(date -u +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
TOKEN="scratch-$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')"
export LAMBO_AUTH_TOKEN="$TOKEN"
export RUST_LOG="${RUST_LOG:-lambo=info}"
STDERR="$OUT/stderr-$SESSION-$N.log"
LEDGER="$OUT/ledger-$SESSION-$N.jsonl"
PORT=7790
CFG="$REPO/evidence/mooshik-f4-cockroach/lambo.cockroach.toml"

: > "$LEDGER"

cleanup() {
  if [[ -n "${SERVE_PID:-}" ]] && kill -0 "$SERVE_PID" 2>/dev/null; then
    kill -TERM "$SERVE_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

"$BIN" serve --config "$CFG" --session "$SESSION" --agent f4-capture \
    --transport http --port "$PORT" --bind 127.0.0.1 \
    > "$STDERR" 2>&1 &
SERVE_PID=$!

# Wait for it to listen.
for _ in $(seq 1 120); do
  if grep -q "listening on /mcp" "$STDERR" 2>/dev/null; then break; fi
  if ! kill -0 "$SERVE_PID" 2>/dev/null; then echo "serve died early"; exit 1; fi
  sleep 0.1
done
grep -q "listening on /mcp" "$STDERR" || { echo "serve never listened"; exit 1; }


# Fire the burst in the background against THIS serve's port.
python3 "$REPO/evidence/mooshik-f4-cockroach/f4_drive.py" \
    --n "$N" --ledger "$LEDGER" --per 16 \
    --endpoint "http://127.0.0.1:${PORT}/mcp" \
    > "$OUT/driver-$SESSION-$N.stdout" 2>> "$STDERR" &

# Wait for the burst to finish (BURST_DONE), then SIGTERM as fast as possible.
for _ in $(seq 1 600); do
  if grep -q "BURST_DONE" "$OUT/driver-$SESSION-$N.stdout" 2>/dev/null; then break; fi
  if ! kill -0 "$DRIVER_PID" 2>/dev/null; then break; fi
  sleep 0.02
done

T0="$(date +%s%N)"
kill -TERM "$SERVE_PID" 2>/dev/null || true
echo "SIGTERM at $(date -u +%Y-%m-%dT%H:%M:%SZ)"

# Wait for exit and capture the code.
EXIT_CODE=""
for _ in $(seq 1 200); do
  if ! kill -0 "$SERVE_PID" 2>/dev/null; then
    wait "$SERVE_PID" 2>/dev/null && EXIT_CODE=0 || EXIT_CODE=$?
    break
  fi
  sleep 0.05
done
T1="$(date +%s%N)"
if [[ -z "$EXIT_CODE" ]]; then
  echo "server did not exit" >&2
  EXIT_CODE="TIMEOUT"
fi
SIG_TO_EXIT_MS=$(( (T1 - T0) / 1000000 ))

wait "$DRIVER_PID" 2>/dev/null || true

# Verdict lines.
if grep -q "lambo serve: session closed, tail durable" "$STDERR"; then
  VERDICT="tail-durable"
elif grep -q "tail is LOST" "$STDERR" || grep -q "did not finish within the grace window" "$STDERR"; then
  VERDICT="tail-lost-abandoned"
else
  VERDICT="unknown"
fi
TAIL_FLUSHED=$(grep -c "Memory session closed (tail flushed)" "$STDERR" || true)

echo "RESULT session=$SESSION N=$N sig_to_exit_ms=$SIG_TO_EXIT_MS exit=$EXIT_CODE verdict=$VERDICT tail_flushed=$TAIL_FLUSHED"
echo "saved: $STDERR $LEDGER"
