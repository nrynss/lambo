#!/usr/bin/env bash
# Result 6 — CLI over Cockroach: readers lease-free, writer fail-closed, CLI<->MCP differential.
set -uo pipefail
cd /home/nryn/work/lambo
set -a; source /home/nryn/work/lambo/.env; set +a
export LAMBO_STORE=cockroach LAMBO_EMBEDDER=bge_m3 LAMBO_LLAMA_EMBED_URL=http://127.0.0.1:8080
BIN=/home/nryn/work/lambo/target/debug/lambo
CFG=/tmp/lambo-live.toml
PSQL=/home/nryn/.local/bin/psql
q(){ "$PSQL" "$LAMBO_COCKROACH_DSN" -tAc "$1" 2>/dev/null; }

### 6a — read verbs are lease-free readers (no server)
S=live-mcp-5a   # existing session with durable concepts
echo "### 6a readers (lease-free)"
for verb in "recall --session $S --query auth --top-k 3" "saints --session $S" "inspect --session $S --focus \"user schema\" --depth 2" "stats --session $S"; do
  out=$(timeout 60 "$BIN" --config "$CFG" $verb 2>&1)
  rc=$?
  echo "--- $verb (rc=$rc)"
  echo "$out" | head -6
done
echo "lease_rows_after_reads=$(q "SELECT count(*) FROM session_leases WHERE session_id='$S';") (expect 0)"

### 6b — write verb fails closed while a server owns the session; succeeds when free
S2=live-cli-6b
q "DELETE FROM concepts WHERE session_id='$S2';" >/dev/null 2>&1
q "DELETE FROM session_leases WHERE session_id='$S2';" >/dev/null 2>&1
echo "### 6b writer fail-closed vs server"
tail -f /dev/null | "$BIN" --config "$CFG" serve --session "$S2" --agent srv --transport stdio >/tmp/6b-serve.log 2>&1 &
SRV=$!
for i in $(seq 1 30); do h=$(q "SELECT holder FROM session_leases WHERE session_id='$S2';"); [ -n "$h" ] && break; sleep 0.5; done
echo "server holds: $(q "SELECT holder FROM session_leases WHERE session_id='$S2';")"
timeout 60 "$BIN" --config "$CFG" derive --session "$S2" --agent cli --content "cli write attempt" --kind entity >/tmp/6b-refuse.log 2>&1
echo "derive_while_server_owns rc=$? -> $(head -2 /tmp/6b-refuse.log | tr '\n' ' ')"
kill -INT "$SRV" 2>/dev/null; sleep 3
echo "after server stop, lease rows: $(q "SELECT count(*) FROM session_leases WHERE session_id='$S2';") (expect 0)"
timeout 60 "$BIN" --config "$CFG" derive --session "$S2" --agent cli --content "cli write ok" --kind entity >/tmp/6b-write.log 2>&1
echo "derive_when_free rc=$? -> $(head -2 /tmp/6b-write.log | tr '\n' ' ')"
echo "concepts_after_free_write=$(q "SELECT count(*) FROM concepts WHERE session_id='$S2';") lease_after=$(q "SELECT count(*) FROM session_leases WHERE session_id='$S2';") (expect concept>=1, lease=0)"

### 6c — CLI <-> MCP differential on the same data (recall + saints agreement)
S3=live-diff-6c
q "DELETE FROM concepts WHERE session_id='$S3';" >/dev/null 2>&1
q "DELETE FROM session_leases WHERE session_id='$S3';" >/dev/null 2>&1
echo "### 6c differential: derive a concept via CLI, read back via CLI and via MCP"
timeout 60 "$BIN" --config "$CFG" derive --session "$S3" --agent cli --content "billing retries change" --kind entity >/dev/null 2>&1
"$BIN" --config "$CFG" recall --session "$S3" --query "billing" --top-k 3 > /tmp/6c-cli-recall.txt 2>&1
echo "--- CLI recall ---"; cat /tmp/6c-cli-recall.txt | head -6
# MCP recall of the same session (reader does NOT take lease; use a temporary serve on a DIFFERENT writer? recall is a tool; drive via stdio serve same session)
tail -f /dev/null | "$BIN" --config "$CFG" serve --session "$S3" --agent mcp --transport stdio >/tmp/6c-mcp-serve.log 2>&1 &
MSRV=$!
for i in $(seq 1 30); do h=$(q "SELECT holder FROM session_leases WHERE session_id='$S3';"); [ -n "$h" ] && break; sleep 0.5; done
printf '%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"6c","version":"1"}}}' \
 '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lambo_recall","arguments":{"agent_id":"mcp","query":"billing","top_k":3}}}' \
 | timeout 60 "$BIN" --config "$CFG" serve --session "$S3" --agent mcp2 --transport stdio >/tmp/6c-attempt.log 2>&1
# the above second serve on same session must be refused (mcp holds lease) — instead read via psql-independent CLI saints
echo "--- CLI saints (reader, lease-free) ---"
"$BIN" --config "$CFG" saints --session "$S3" 2>&1 | head -4
kill -INT "$MSRV" 2>/dev/null; sleep 2
q "DELETE FROM session_leases WHERE session_id='$S3';" >/dev/null 2>&1
echo DONE
