#!/usr/bin/env bash
# F-R1-5 evidence driver: a real SQLite store + a real embedder (BGE-M3 via
# llama.cpp), showing the vector leg produce a SEMANTIC hit — a recall query that
# shares NO vocabulary with a derived concept still recalls it.
#
# Prerequisites:
#   * llama-server with BGE-M3 weights on http://127.0.0.1:8080
#     (GET /health -> {"status":"ok"}; POST /v1/embeddings, OpenAI-compatible)
#   * a binary built with the two features this needs:
#       cargo build --features store-sqlite,embed-bge
#
# Run from the repository root:
#   ./evidence/mooshik-f-sqlite-bge/run.sh 2>&1 | tee evidence/mooshik-f-sqlite-bge/transcript.txt
set -uo pipefail

HERE="evidence/mooshik-f-sqlite-bge"
CFG="$HERE/lambo.bge-sqlite.toml"
DB="$HERE/f-bge.db"
SESSION="f-bge-semantic"
BIN="./target/debug/lambo"

export LAMBO_LLAMA_EMBED_URL="http://127.0.0.1:8080"

say() { printf '\n=== %s ===\n' "$1"; }

say "0. Environment"
"$BIN" --version
printf 'llama-server health: '; curl -s -m 5 http://127.0.0.1:8080/health; printf '\n'
printf 'embedder width reported by the server: '
curl -s -m 30 http://127.0.0.1:8080/v1/embeddings \
  -H 'Content-Type: application/json' \
  -d '{"input":"width probe","model":"bge-m3"}' \
  | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["data"][0]["embedding"]))'
echo "--- config ---"
cat "$CFG"

say "1. Fresh store"
rm -f "$DB" "$DB-shm" "$DB-wal"
"$BIN" --config "$CFG" provision

say "2. Derive three concepts (a real embedder, so these vectors mean something)"
# Deliberately worded so that NONE of them shares a word with the recall queries
# in step 4. Any lift there has to come from the vector leg.
"$BIN" --config "$CFG" derive --session "$SESSION" --agent agent-a \
  --content "user schema stores account records" --kind entity
"$BIN" --config "$CFG" derive --session "$SESSION" --agent agent-a \
  --content "auth middleware validates bearer tokens" --kind logic
"$BIN" --config "$CFG" derive --session "$SESSION" --agent agent-a \
  --content "deployment must stay backward compatible" --kind constraint

say "3. Durable readback: the stamped contract and the stored vectors"
sqlite3 "$DB" "SELECT 'sessions: ' || session_id || ' | kind=' || COALESCE(embedding_kind,'NULL') || ' | model=' || COALESCE(embedding_model,'NULL') || ' | dim=' || COALESCE(embedding_dim,'NULL') FROM sessions;"
# length() on the BLOB is the TEXT codec's byte length, not the element count, so
# count the elements the way the adapter's decoder does: commas + 1.
sqlite3 "$DB" "SELECT content || ' | blob_bytes=' || length(embedding) || ' | elements=' || (length(embedding) - length(replace(embedding, ',', '')) + 1) FROM concepts ORDER BY content;"

say "4. THE POINT: recall with queries that share no vocabulary with any concept"
for q in \
  "database table for people signing up" \
  "login token checking layer" \
  "changes that do not break existing clients"
do
  printf -- '\n--- query: %s\n' "$q"
  # Show the shared-vocabulary check rather than asserting it in prose.
  python3 - "$q" <<'PY'
import sys, re
q = set(re.findall(r"[a-z]+", sys.argv[1].lower()))
concepts = [
    "user schema stores account records",
    "auth middleware validates bearer tokens",
    "deployment must stay backward compatible",
]
for c in concepts:
    shared = q & set(re.findall(r"[a-z]+", c.lower()))
    print(f"    shared words with {c!r}: {sorted(shared) or 'NONE'}")
PY
  "$BIN" --config "$CFG" recall --session "$SESSION" --query "$q" --top-k 3
done

say "5. The raw similarity the adapter actually scores"
# `lambo recall`'s printed score passes through recall's leg merge (max against the
# flat RECENT_SCORE) and per-type scaling, so it is not the cosine. This recomputes
# the cosine directly from the durable vectors, which is what rank_by_cosine scores.
python3 "$HERE/cosine_probe.py"

say "6. F-R1-2: the width pin is a real authority — a disagreement is refused"
# Same database, same store, but a config whose embedder width contradicts the pin.
# Pre-remediation this was unreachable: the store echoed the embedder, so
# check_vector_compatibility compared a number to itself.
sed 's/^dim = 1024/dim = 768/' "$CFG" > "$HERE/lambo.pin-mismatch.toml"
echo "--- the only change: [embedder] dim 1024 -> 768, against store.vector_dim = 1024 ---"
"$BIN" --config "$HERE/lambo.pin-mismatch.toml" recall --session "$SESSION" --query "anything" \
  && echo "UNEXPECTED: the mismatch resolved" \
  || echo "(refused at process resolution, as it must be)"
rm -f "$HERE/lambo.pin-mismatch.toml"

say "done"
