#!/usr/bin/env bash
# Live single-writer lease conformance (runbook Result 4a/4b) — live-lease session.
set -uo pipefail
cd /home/nryn/work/lambo
set -a; source /home/nryn/work/lambo/.env; set +a
export LAMBO_STORE=cockroach LAMBO_EMBEDDER=bge_m3 LAMBO_LLAMA_EMBED_URL=http://127.0.0.1:8080

BIN=/home/nryn/work/lambo/target/debug/lambo
CFG=/tmp/lambo-live.toml
PSQL=/home/nryn/.local/bin/psql

q() { "$PSQL" "$LAMBO_COCKROACH_DSN" -tAc "$1" 2>/dev/null; }

echo "### cleanup any prior live-lease row"
q "DELETE FROM session_leases WHERE session_id='live-lease';" >/dev/null 2>&1 || true

echo "### 4a: start A (holds lease)"
tail -f /dev/null | "$BIN" --config "$CFG" serve --session live-lease --agent agent-a --transport stdio >/tmp/lease-a.log 2>&1 &
A_PID=$!
# wait for the lease row to appear (up to ~15s)
for i in $(seq 1 30); do
  ROW=$(q "SELECT holder FROM session_leases WHERE session_id='live-lease';")
  [ -n "$ROW" ] && break
  sleep 0.5
done
echo "A_PID=$A_PID lease_holder='$ROW'"

echo "### 4a: start B on same session (must refuse, name agent-a)"
timeout 25 "$BIN" --config "$CFG" serve --session live-lease --agent agent-b --transport stdio \
  </dev/null >/tmp/lease-b.log 2>&1
echo "B_EXIT=$?"

echo "### 4a: lease rows now"
q "SELECT session_id, holder, expires_at > now() AS active FROM session_leases WHERE session_id='live-lease';"

echo "### 4b: clean stop A (SIGINT), then confirm release"
kill -INT "$A_PID" 2>/dev/null
sleep 3
AFTER=$(q "SELECT count(*) FROM session_leases WHERE session_id='live-lease';")
echo "rows_after_clean_stop='$AFTER' (expect 0)"

echo "### 4b: crash A (kill -9), then confirm expiry after TTL"
tail -f /dev/null | "$BIN" --config "$CFG" serve --session live-lease --agent agent-a --transport stdio >/tmp/lease-c.log 2>&1 &
C_PID=$!
for i in $(seq 1 30); do
  ROW=$(q "SELECT holder FROM session_leases WHERE session_id='live-lease';")
  [ -n "$ROW" ] && break
  sleep 0.5
done
echo "C_PID=$C_PID lease_holder='$ROW'"
kill -9 "$C_PID" 2>/dev/null
LINGER=$(q "SELECT count(*) FROM session_leases WHERE session_id='live-lease';")
echo "rows_after_kill9='$LINGER' (expect 1, linger until TTL)"
echo "waiting 50s for TTL expiry (45s) ..."
sleep 50
EXPIRED=$(q "SELECT count(*) FROM session_leases WHERE session_id='live-lease' AND expires_at <= now();")
REMAIN=$(q "SELECT count(*) FROM session_leases WHERE session_id='live-lease' AND expires_at > now();")
echo "expired_rows='$EXPIRED' active_rows='$REMAIN' (expect expired>=1, active=0)"

echo "### cleanup"
q "DELETE FROM session_leases WHERE session_id='live-lease';" >/dev/null 2>&1 || true
kill -TERM "$A_PID" "$C_PID" 2>/dev/null || true
echo "DONE"
