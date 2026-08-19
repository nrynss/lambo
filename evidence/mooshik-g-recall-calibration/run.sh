#!/usr/bin/env bash
# Reproduce G2's two interaction checks against the real local BGE-M3 server.
#
# The driver deliberately builds the exact before and after revisions in
# throwaway worktrees. It copies F's committed durable-vector SQLite fixture
# into a temporary directory, so it never relies on the ignored
# g-calibration.db capture. It then proves both sides of G2:
#   * before: the 0.50 recent floor masks the 0.3991 durable vector hit;
#   * after: the 0.35 floor lets that same hit rank first;
#   * both: the below-threshold paraphrase remains two concepts, no Semantic edge.
#
# Prerequisites:
#   * git history containing 74febca and its parent;
#   * cargo and a local llama.cpp BGE-M3 server at http://127.0.0.1:8080;
#   * network access from the built binary to that local server.
#
# Run from the repository root (or invoke this script by absolute path):
#   ./evidence/mooshik-g-recall-calibration/run.sh
#
# Revision overrides are useful when replaying an equivalent pair elsewhere:
#   LAMBO_G_BEFORE_REV=74febca^ LAMBO_G_AFTER_REV=74febca \
#     ./evidence/mooshik-g-recall-calibration/run.sh
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(git -C "$HERE" rev-parse --show-toplevel)"
BEFORE_REV="${LAMBO_G_BEFORE_REV:-74febca^}"
AFTER_REV="${LAMBO_G_AFTER_REV:-74febca}"
F_DB="$ROOT/evidence/mooshik-f-sqlite-bge/f-bge.db"
CONFIG_TEMPLATE="$HERE/lambo.g-bge-sqlite.toml"
QUERY="changes that do not break existing clients"
EXPECTED_TOP="deployment must stay backward compatible"

say() {
    printf '\n=== %s ===\n' "$1"
}

run() {
    printf '+ '
    printf '%q ' "$@"
    printf '\n'
    "$@"
}

run_capture() {
    printf '+ '
    printf '%q ' "$@"
    printf '\n'
    LAST_OUTPUT="$("$@" 2>&1)"
    printf '%s\n' "$LAST_OUTPUT"
}

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/lambo-g-recall-calibration.XXXXXX")"
BEFORE_TREE="$RUN_DIR/before-source"
AFTER_TREE="$RUN_DIR/after-source"

cleanup() {
    git -C "$ROOT" worktree remove --force "$BEFORE_TREE" >/dev/null 2>&1 || true
    git -C "$ROOT" worktree remove --force "$AFTER_TREE" >/dev/null 2>&1 || true
    rm -rf "$RUN_DIR"
}
trap cleanup EXIT

make_config() {
    local db="$1"
    local config="$2"
    sed "s|^path = .*|path = \"$db\"|" "$CONFIG_TEMPLATE" > "$config"
}

build_revision() {
    local label="$1"
    local revision="$2"
    local tree="$3"
    local target="$4"

    run git -C "$ROOT" rev-parse --verify "$revision^{commit}"
    run git -C "$ROOT" worktree add --detach "$tree" "$revision"
    say "Build $label revision ($revision)"
    (
        cd "$tree"
        printf '+ CARGO_TARGET_DIR=%q cargo build --features store-sqlite,embed-bge\n' "$target"
        CARGO_TARGET_DIR="$target" cargo build --features store-sqlite,embed-bge
    )
}

session_counts() {
    local db="$1"
    local session="$2"
    run_capture sqlite3 "$db" "SELECT 'sqlite: $session concepts=' || (SELECT COUNT(*) FROM concepts WHERE session_id = '$session') || ', semantic_edges=' || (SELECT COUNT(*) FROM edges WHERE session_id = '$session' AND edge_type = 'Semantic');"
    [[ "$LAST_OUTPUT" == "sqlite: $session concepts=2, semantic_edges=0" ]]
}

duplicate_check() {
    local label="$1"
    local bin="$2"
    local config="$3"
    local db="$4"
    local session="$5"

    say "$label duplicate check"
    run_capture "$bin" --config "$config" derive --session "$session" --agent agent-a --content "register user" --kind entity
    [[ "$LAST_OUTPUT" == "derived 1 concept(s): 1 created, 0 matched existing" ]]
    run_capture "$bin" --config "$config" derive --session "$session" --agent agent-a --content "create account" --kind entity
    [[ "$LAST_OUTPUT" == "derived 1 concept(s): 1 created, 0 matched existing" ]]
    session_counts "$db" "$session"
}

recall_check() {
    local label="$1"
    local bin="$2"
    local config="$3"
    local expected="$4"

    say "$label durable-vector recall"
    run_capture "$bin" --config "$config" recall --session f-bge-semantic --query "$QUERY" --top-k 3
    local first
    first="$(printf '%s\n' "$LAST_OUTPUT" | sed -n '1p')"
    [[ "$first" == "$expected"* ]]
}

say "Environment"
run curl --fail --silent --show-error --max-time 5 http://127.0.0.1:8080/health
run test -f "$F_DB"
run test -f "$CONFIG_TEMPLATE"

build_revision "before G2" "$BEFORE_REV" "$BEFORE_TREE" "$RUN_DIR/before-target"
build_revision "after G2" "$AFTER_REV" "$AFTER_TREE" "$RUN_DIR/after-target"

BEFORE_DB="$RUN_DIR/before.db"
AFTER_DB="$RUN_DIR/after.db"
BEFORE_CONFIG="$RUN_DIR/before.toml"
AFTER_CONFIG="$RUN_DIR/after.toml"
run cp "$F_DB" "$BEFORE_DB"
run cp "$F_DB" "$AFTER_DB"
make_config "$BEFORE_DB" "$BEFORE_CONFIG"
make_config "$AFTER_DB" "$AFTER_CONFIG"

BEFORE_BIN="$RUN_DIR/before-target/debug/lambo"
AFTER_BIN="$RUN_DIR/after-target/debug/lambo"
say "Before G2 uses RECENT_SCORE = 0.50"
recall_check "Before G2" "$BEFORE_BIN" "$BEFORE_CONFIG" "user schema stores account records"
duplicate_check "Before G2" "$BEFORE_BIN" "$BEFORE_CONFIG" "$BEFORE_DB" "g-merge-before"

say "After G2 uses RECENT_SCORE = 0.35"
recall_check "After G2" "$AFTER_BIN" "$AFTER_CONFIG" "$EXPECTED_TOP"
duplicate_check "After G2" "$AFTER_BIN" "$AFTER_CONFIG" "$AFTER_DB" "g-merge-after"

say "PASS"
printf 'The before floor masked the durable vector hit; the after floor surfaced it.\n'
printf 'The below-threshold paraphrase created a duplicate in both revisions.\n'
