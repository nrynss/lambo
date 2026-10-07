#!/usr/bin/env bash
# C2 — run `lambo serve` under concurrent load and pull the SIGTERM.
#
# Orchestrates the concurrency-capture run end to end:
#
#   1. provision a scratch SQLite store (config written beside the evidence)
#   2. start `lambo serve --transport http` with a scratch bearer token
#   3. run scripts/loadtest/mcp_load.py with K workers (paced main window,
#      then a burst of at-cap record_action calls building a large un-flushed
#      tail — the SIGTERM lands inside the burst)
#   4. SIGTERM the server while the load is in flight; measure wall time from
#      signal to exit and capture the exit code
#   5. run the C3 durability check against the store and save its report
#   6. write run metadata (machine, command lines, timing) as JSON
#
# Evidence lands in the target directory (default evidence/concurrency/):
#   stderr-<run>.log        full server stderr (the SIGTERM line lives here)
#   ledger-<run>.jsonl      the driver's every-response ledger
#   durability-<run>.txt    the C3 comparison
#   run-<run>.json          machine + timing + counts metadata
#   lambo.sqlite.toml       the exact config the server ran under
#   c-load-<date>.db        the scratch store itself
#
# The token never appears in any file here (it is generated into a mktemp
# file and passed via LAMBO_AUTH_TOKEN, the env channel the server documents
# as taking precedence over --auth-token). run-<run>.json records the
# placeholder <SCRATCH-TOKEN>.
#
# Isolation from a live writer on the same machine:
#   * the default port is 17700, never 7700 (the conventional port of a
#     long-lived `lambo serve`, e.g. a dogfood writer). The harness refuses
#     7700 unless --allow-production-port is passed, because taking it either
#     fails against the live writer or keeps that writer from starting.
#   * the spawned serve gets its own XDG_RUNTIME_DIR (a mktemp directory,
#     mode 700, removed on exit), so its session endpoint file never lands in
#     the shared per-user runtime dir a live writer advertises itself in.
#   * the driver is pointed at this harness's own port explicitly.
#
# Usage:
#   scripts/loadtest/capture_sigterm.sh [--out evidence/concurrency] [--workers 12]
#                                       [--session c-load-20260818] [--delay 5]
#                                       [--bin target/debug/lambo] [--port 17700]
#                                       [--allow-production-port]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"

PRODUCTION_PORT=7700
OUT="$REPO/evidence/concurrency"
WORKERS=12
SESSION="c-load-$(date +%Y%m%d)"
DELAY=5                     # seconds after burst-start before SIGTERM
BIN="$REPO/target/debug/lambo"
PORT=17700
ALLOW_PRODUCTION_PORT=0
MAIN_SECS=45
BURST_SECS=25

while [[ $# -gt 0 ]]; do
    case "$1" in
        --out) OUT="$2"; shift 2 ;;
        --workers) WORKERS="$2"; shift 2 ;;
        --session) SESSION="$2"; shift 2 ;;
        --delay) DELAY="$2"; shift 2 ;;
        --bin) BIN="$2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --allow-production-port) ALLOW_PRODUCTION_PORT=1; shift ;;
        --main-secs) MAIN_SECS="$2"; shift 2 ;;
        --burst-secs) BURST_SECS="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ "$PORT" == "$PRODUCTION_PORT" && "$ALLOW_PRODUCTION_PORT" != 1 ]]; then
    echo "refusing to run on port $PRODUCTION_PORT, the port a live lambo serve" \
         "writer listens on; pick another --port, or pass --allow-production-port" \
         "if this machine runs no such writer" >&2
    exit 2
fi

RUN="$(date -u +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
TOKEN_FILE="$(mktemp /tmp/c-series-token.XXXXXX)"
TOKEN="scratch-$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')"
printf '%s' "$TOKEN" > "$TOKEN_FILE"
# A private runtime dir for the spawned serve (see the isolation note above).
# Deliberately short and under /tmp rather than $TMPDIR: serve binds a unix
# socket at <dir>/lambo/<38-byte name>, and a socket address has ~104 bytes
# on macOS, which a /var/folders/... TMPDIR plus a long name would overrun.
RUNTIME_DIR="$(mktemp -d /tmp/lambo-lt.XXXXXX)"
chmod 700 "$RUNTIME_DIR"
cleanup() {
    rm -f "$TOKEN_FILE"
    if [[ -n "${SERVE_PID:-}" ]] && kill -0 "$SERVE_PID" 2>/dev/null; then
        kill -TERM "$SERVE_PID" 2>/dev/null || true
        wait "$SERVE_PID" 2>/dev/null || true
    fi
    rm -rf "$RUNTIME_DIR"
}
trap cleanup EXIT

DB="$OUT/$SESSION.db"
CFG="$OUT/lambo.sqlite.toml"
LEDGER="$OUT/ledger-$RUN.jsonl"
STDERR="$OUT/stderr-$RUN.log"

cat > "$CFG" <<EOF
[store]
kind = "sqlite"
path = "$DB"

[embedder]
kind = "fixture"
dim = 1024
EOF

export LAMBO_AUTH_TOKEN="$TOKEN"
# The GC sweep count (spec §9 housekeeping) is logged at debug level; a
# durability comparison against concept counts needs it, so the default run
# enables the daemon GC target's debug logs on stderr. Override with
# RUST_LOG=... if a quieter transcript is wanted.
export RUST_LOG="${RUST_LOG:-lambo=info,lambo::daemon::gc=debug}"

echo "== C2 capture: run=$RUN session=$SESSION workers=$WORKERS"
# Machine facts, portably: lscpu/nproc are Linux-only, sysctl is the macOS
# equivalent, and getconf answers on both.
cpu_model() {
    lscpu 2>/dev/null | awk -F: '/Model name/{print $2}' | xargs ||
        true
    sysctl -n machdep.cpu.brand_string 2>/dev/null || true
}
CPU_MODEL="$(cpu_model | sed "/^[[:space:]]*$/d" | head -n 1)"
CPU_THREADS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo unknown)"
case "$(uname -s)" in
    Linux) MACHINE_NOTE="Linux box, NOT the MBP the P8 criterion names — see concurrency-capture.md" ;;
    *) MACHINE_NOTE="$(uname -s) host — see concurrency-capture.md for which machine the P8 criterion names" ;;
esac
echo "== machine: $(uname -srm) | $CPU_MODEL | $CPU_THREADS threads"

"$BIN" provision --config "$CFG" >/dev/null

XDG_RUNTIME_DIR="$RUNTIME_DIR" \
"$BIN" serve --config "$CFG" --session "$SESSION" --agent c-load \
    --transport http --port "$PORT" --bind 127.0.0.1 \
    > "$STDERR" 2>&1 &
SERVE_PID=$!
for _ in $(seq 1 60); do
    if grep -q "listening on /mcp" "$STDERR" 2>/dev/null; then break; fi
    if ! kill -0 "$SERVE_PID" 2>/dev/null; then
        echo "serve exited before listening:" >&2
        cat "$STDERR" >&2
        exit 1
    fi
    sleep 0.25
done
grep -q "listening on /mcp" "$STDERR" || { echo "serve never listened" >&2; exit 1; }
echo "serve listening (port $PORT)"

STARTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

DRIVER_PORT_FLAG=()
if [[ "$ALLOW_PRODUCTION_PORT" == 1 ]]; then DRIVER_PORT_FLAG=(--allow-production-port); fi
python3 "$HERE/mcp_load.py" \
    --endpoint "http://127.0.0.1:$PORT/mcp" ${DRIVER_PORT_FLAG[@]+"${DRIVER_PORT_FLAG[@]}"} \
    --session "$SESSION" --ledger "$LEDGER" \
    --workers "$WORKERS" --seed 0 \
    --rate 40 --burst-rate 45 --overdrive 2 --overdrive-calls 120 \
    --main-secs "$MAIN_SECS" --burst-secs "$BURST_SECS" \
    > "$OUT/driver-$RUN.stdout" 2>&1 &
DRIVER_PID=$!
echo "driver pid: $DRIVER_PID"

# Wait for the burst to start, then give it `--delay` seconds to build a
# non-trivial tail, then pull the SIGTERM while the load is in flight.
for _ in $(seq 1 300); do
    if grep -q '"name":"burst-start"' "$LEDGER" 2>/dev/null; then break; fi
    sleep 0.2
done
grep -q '"name":"burst-start"' "$LEDGER" || { echo "burst never started" >&2; exit 1; }
echo "burst started; SIGTERM in ${DELAY}s"
sleep "$DELAY"

T0="$(date +%s%N)"
kill -TERM "$SERVE_PID" 2>/dev/null || true
echo "SIGTERM sent at $(date -u +%Y-%m-%dT%H:%M:%SZ)"

# Wait for the server to exit and grab its exit code.
EXIT_CODE=""
for _ in $(seq 1 120); do
    if ! kill -0 "$SERVE_PID" 2>/dev/null; then
        wait "$SERVE_PID" && EXIT_CODE=0 || EXIT_CODE=$?
        break
    fi
    sleep 0.1
done
T1="$(date +%s%N)"
if [[ -z "$EXIT_CODE" ]]; then
    echo "serve did not exit within 12s of SIGTERM" >&2
    EXIT_CODE="TIMEOUT"
fi
SIG_TO_EXIT_MS=$(( (T1 - T0) / 1000000 ))
echo "signal->exit: ${SIG_TO_EXIT_MS} ms (exit code $EXIT_CODE)"

# The driver finishes its phases (transport errors after the server died are
# recorded, not fatal) — wait for it so the ledger is complete.
wait "$DRIVER_PID" || { echo "driver failed" >&2; exit 1; }

# C3 — prove the tail is durable.
python3 "$HERE/check_durability.py" \
    --ledger "$LEDGER" --db "$DB" --session "$SESSION" \
    --stderr "$STDERR" \
    > "$OUT/durability-$RUN.txt" || true   # exit 2 is the honest SHORTFALL signal

cat > "$OUT/run-$RUN.json" <<EOF
{
  "run_id": "$RUN",
  "session": "$SESSION",
  "date_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "started_at": "$STARTED_AT",
  "machine": {
    "host": "$(hostname)",
    "os": "$(uname -srm)",
    "cpu": "$CPU_MODEL",
    "threads": "$CPU_THREADS",
    "note": "$MACHINE_NOTE"
  },
  "server": {
    "binary": "$BIN",
    "features": "store-sqlite (+ default embed-bge, embed-fixture, store-memory)",
    "cmd": "$BIN serve --config <cfg> --session $SESSION --agent c-load --transport http --port $PORT --bind 127.0.0.1 (auth via LAMBO_AUTH_TOKEN env)",
    "auth_token": "<SCRATCH-TOKEN>",
    "rate_limit_rps": 50,
    "max_sessions": 32
  },
  "load": {
    "workers": $WORKERS,
    "seed": 0,
    "main_rps_aggregate": 40,
    "burst_rps_aggregate": 45,
    "overdrive_secs": 2,
    "overdrive_calls_per_worker": 120,
    "adversarial_fraction": 0.2
  },
  "sigterm": {
    "delay_after_burst_start_secs": $DELAY,
    "signal_to_exit_ms": $SIG_TO_EXIT_MS,
    "exit_code": "$EXIT_CODE",
    "assertion_line": "lambo serve: session closed, tail durable",
    "assertion_met": "$(grep -q 'lambo serve: session closed, tail durable' "$STDERR" && echo true || echo false)",
    "tail_lost_present": "$(grep -q 'tail lost on exit' "$STDERR" && echo true || echo false)"
  },
  "artifacts": {
    "stderr": "stderr-$RUN.log",
    "ledger": "ledger-$RUN.jsonl",
    "durability": "durability-$RUN.txt",
    "driver_stdout": "driver-$RUN.stdout",
    "config": "lambo.sqlite.toml",
    "store_db": "$SESSION.db"
  }
}
EOF

echo "== done: $OUT"
echo "== artifacts: $(ls "$OUT" | tr '\n' ' ')"
