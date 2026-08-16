#!/usr/bin/env bash
# Characterize what two raw `lambo demo` runs differ by, across 20 independent
# pairs. Each pair = a fresh scratch store, two sequential runs, full diff.
#
# Produces: demo-raw-diff-20-pairs.txt
#
# Every changed diff line is classified into one of the three known-intentional
# buckets (session id / warning node id / canonization cycle index). Anything
# left over is counted as UNEXPECTED and the whole diff is written out, so a
# fourth difference cannot pass unnoticed. Score lines and the GC headroom line
# are additionally compared on their own, independently of the bucket counting.
#
# <REPO> is the checkout root; <OUT> is where unexpected diffs are written.
cd <REPO> || exit 1
OUT=<OUT>
echo "commit:   $(git rev-parse HEAD)"
echo "date:     $(date -Is)"
echo "binary:   ./target/debug/lambo (frozen; built by cargo build --features ship --tests --bins)"
echo "each pair: fresh sqlite scratch store, two sequential runs of"
echo "           lambo --config <SCRATCH>/lambo.toml demo --scenario rest-api"
echo "----"
sess=0; node=0; cyc=0; unexp=0; ident=0
for i in $(seq 1 20); do
  S=/tmp/lambo-demo-determinism-pair-$i
  rm -rf "$S"; mkdir -p "$S"
  printf '[store]\nkind = "sqlite"\npath = "%s/parity.sqlite"\n\n[embedder]\nkind = "fixture"\ndim = 1024\n' "$S" > "$S/lambo.toml"
  env -u LAMBO_STORE -u LAMBO_EMBEDDER -u LAMBO_CONFIG -u LAMBO_COCKROACH_DSN \
    ./target/debug/lambo --config "$S/lambo.toml" demo --scenario rest-api > "$S/a.txt" 2>/dev/null
  env -u LAMBO_STORE -u LAMBO_EMBEDDER -u LAMBO_CONFIG -u LAMBO_COCKROACH_DSN \
    ./target/debug/lambo --config "$S/lambo.toml" demo --scenario rest-api > "$S/b.txt" 2>/dev/null
  d=$(diff "$S/a.txt" "$S/b.txt")
  # count only the changed content lines (drop diff hunk headers and separators)
  changed=$(printf '%s\n' "$d" | grep -c '^[<>]')
  s=$(printf '%s\n' "$d" | grep -c '^[<>].*session      demo-rest-api-')
  n=$(printf '%s\n' "$d" | grep -c '^[<>].*High-risk modification: high-value node ')
  c=$(printf '%s\n' "$d" | grep -c '^[<>][[:space:]]*cycle[[:space:]]')
  o=$(( changed - s - n - c ))
  [ "$changed" -eq 0 ] && ident=$((ident+1))
  [ "$s" -gt 0 ] && sess=$((sess+1))
  [ "$n" -gt 0 ] && node=$((node+1))
  [ "$c" -gt 0 ] && cyc=$((cyc+1))
  if [ "$o" -gt 0 ]; then
    unexp=$((unexp+1))
    printf 'pair %2d: changed=%d session=%d nodeid=%d cycle=%d  ** %d UNEXPECTED **\n' "$i" "$changed" "$s" "$n" "$c" "$o"
    printf '%s\n' "$d" > "$OUT/unexpected-pair-$i.diff"
  else
    printf 'pair %2d: changed=%d session=%d nodeid=%d cycle=%d\n' "$i" "$changed" "$s" "$n" "$c"
  fi
  # score-line and GC-headroom stability, checked independently of the diff
  if ! diff -q <(grep -o 'score [0-9.]*' "$S/a.txt") <(grep -o 'score [0-9.]*' "$S/b.txt") >/dev/null; then
    echo "         ** SCORES DIFFER in pair $i **"
  fi
  if ! diff -q <(grep 'GC headroom' "$S/a.txt") <(grep 'GC headroom' "$S/b.txt") >/dev/null; then
    echo "         ** GC HEADROOM DIFFERS in pair $i **"
  fi
  rm -rf "$S"
done
echo "----"
echo "pairs with a differing session id:      $sess / 20"
echo "pairs with a differing warning node id: $node / 20"
echo "pairs with a differing cycle index:     $cyc / 20"
echo "pairs with any UNEXPECTED difference:   $unexp / 20"
echo "pairs byte-identical overall:           $ident / 20"
echo "finished: $(date -Is)"
