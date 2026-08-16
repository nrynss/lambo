# Demo determinism — the `lambo demo` OUTCOME block is reproducible (2026-08-16)

Captured at commit `00068249b7cbcb490315a46ee8d034a3f84c9405`, on a clean tree
(`git status --porcelain` empty), on Linux `x86_64` (kernel `7.1.8-1-cachyos`),
`rustc 1.97.1 (8bab26f4f 2026-07-14)`.

This directory is the evidence behind one claim: after the script-clock fix,
`lambo demo` renders a byte-identical OUTCOME block on every run, rather than
about nine times in ten. The reasoning and the root cause are in
[`docs/plans/parity-determinism-fix.md`](../../docs/plans/parity-determinism-fix.md);
this is the measurement.

No DSN, API key, or cluster id appears here. The scratch stores are local SQLite
files created and deleted by the capture scripts. The two `.sh` files are the
scripts as run, with only the checkout root and the capture output directory
replaced by `<REPO>` and `<OUT>` — they are reproduction inputs, not captures.

## What was measured

| # | Claim | Method | Result |
|---|---|---|---|
| 1 | The ×2 OUTCOME assertion holds run after run | `cargo test --features ship --test binary_parity demo_outcome` ×100, frozen build | **100 / 100 passed, 0 failed** |
| 2 | No rebuild slipped into the loop | every run's output scanned for `Compiling` | **0 rebuilds detected** |
| 3 | Two raw demo runs differ only in known-intentional places | `diff` of two full stdout captures | **2 changed lines in the captured pair** (the third expected site did not fire — see §2) |
| 4 | Only three difference *sites* exist at all | 20 independent pairs, every diff line classified | **0 unexpected differences in 20 pairs** |
| 5 | Every rendered score is stable | score lines compared per pair, independently | **identical in 20 / 20 pairs** |
| 6 | The GC headroom line is stable | headroom line compared per pair | **identical in 20 / 20 pairs, always `2.10×`** |

### Files

| File | What it is |
|---|---|
| [`parity-100-runs.txt`](parity-100-runs.txt) | Verbatim log of the 100-run loop: per-run PASS/FAIL, the commit, the clean-tree check, and the totals. |
| [`demo-run-a.txt`](demo-run-a.txt), [`demo-run-b.txt`](demo-run-b.txt) | Verbatim stdout of two consecutive `lambo demo` runs against one shared scratch store. |
| [`demo-run-diff.txt`](demo-run-diff.txt) | Verbatim `diff -u` of those two runs. This is the *complete* diff — nothing elided. |
| [`demo-raw-diff-20-pairs.txt`](demo-raw-diff-20-pairs.txt) | Verbatim log of 20 independent run-pairs, each diff line classified, with unexpected differences counted separately. |
| `run-parity-100.sh`, `run-raw-diff-pairs.sh` | The capture scripts. |

## 1. The property holds: 100 / 100

```
$ cargo build --features ship --tests --bins     # once, to completion
$ ./run-parity-100.sh                            # 100 × cargo test, no build
```

```
passed:  100 / 100
failed:  0 / 100
rebuilds detected mid-loop: 0
```

**The build was frozen.** `cargo build --features ship --tests --bins` ran to
completion before the loop started, the tree was clean throughout
(`git status --porcelain` reported 0 modified files at loop start, and nothing
was edited during the run), and the loop scanned every run's output for a
`Compiling` line — zero were seen. So all 100 runs exercised the same binary.

Every run passed. Nothing was discarded, re-rolled, or re-run; the log in
`parity-100-runs.txt` is the first and only 100-run sample taken at this commit.

## 2. What two raw runs actually differ by

```
$ lambo --config <SCRATCH>/lambo.toml demo --scenario rest-api > a.txt
$ lambo --config <SCRATCH>/lambo.toml demo --scenario rest-api > b.txt
$ diff -u a.txt b.txt
```

Both runs are 242 lines and both wrote nothing to stderr. The complete diff for
the captured pair is in [`demo-run-diff.txt`](demo-run-diff.txt) and contains
**two** changed lines:

1. **Line 4 — the session id.** `demo-rest-api-<uuid>`, minted fresh per run by
   design (P6 R3-1).
2. **Line 54 — a node id in the high-risk warning.** `high-value node <uuid>`,
   a `Uuid::new_v4()`.

The expected third difference — the canonization **cycle index** on the
narration lines — **did not appear in this particular pair.** Both runs happened
to land the `Candidate` / `Venerable` / `Canonical` hops on cycles 1 / 2 / 3.

That is not evidence the residual is gone. It is a genuinely nondeterministic
interleaving of the canonization timer against the settle loop, so a pair can
agree by chance. Section 3 measures how often it actually fires rather than
generalizing from one pair.

**All three difference sites sit outside the asserted OUTCOME block.** The block
the test compares starts at the `scenario` line — line 164 of a 242-line
capture. The session id (4), the cycle indices (42–44) and the warning node id
(54) are all in the narration above it.

### What is now stable that was not before

Every rendered score is byte-identical between the two runs (`score 2.27`,
`score 1.50`, …), and so is the GC headroom line:

```
  GC headroom: closest to the eviction bar is 'user id column' at 2.10× — nothing in this session is collectable
```

Pre-fix, that scalar alternated between `2.06×` and `2.07×` across runs — a
score, derived from the same composite the ordering uses, moving between two
runs of the same script. It is the same `2.10×` in both captures here, and in
all 20 pairs in section 3.

## 3. The same, across 20 independent pairs

One pair cannot distinguish "the cycle index is now stable" from "the cycle
index happened to agree." So 20 independent pairs were run — fresh scratch store
each time — and **every** changed diff line was classified into one of the three
known buckets. Anything unclassified would have been counted as `UNEXPECTED` and
its full diff written out. Full log: [`demo-raw-diff-20-pairs.txt`](demo-raw-diff-20-pairs.txt).

```
pairs with a differing session id:      20 / 20
pairs with a differing warning node id: 20 / 20
pairs with a differing cycle index:     18 / 20
pairs with any UNEXPECTED difference:   0 / 20
pairs byte-identical overall:           0 / 20
```

Reading these:

* **No fourth difference exists.** Across 20 pairs, every changed line fell into
  one of the three intentional buckets. This was the thing most worth
  discovering, and it did not turn up.
* **The three-difference expectation holds as a description of the *sites*, not
  of every pair.** The two id sites fire every time. The cycle index fired in
  18 of 20 pairs, and when it fires it moves one, two, or three narration lines
  (the per-pair `cycle=` counts are 2, 4 or 6 changed lines, i.e. 1–3 line
  pairs). The captured pair in section 2 is one of the 2-in-20 where it did not
  fire.
* **`0 / 20` byte-identical overall is expected, not a failure.** The session id
  is deliberately fresh per run, so two full stdout captures can never be
  byte-identical. The assertion is about the OUTCOME block, not whole stdout.
* Score lines and the GC headroom line were compared separately in each pair,
  outside the bucket counting. Neither differed in any pair.

An earlier pass of this script mis-classified the cycle lines — the grep pattern
had the wrong leading-whitespace count, so cycle differences were reported as
`UNEXPECTED` in 16 of 20 pairs. The pattern was corrected and the script re-run;
the log here is from the corrected instrument. The underlying diffs were the
same in both passes.

## 4. The contrast: what this looked like before

The pre-fix numbers below are **prior results carried over from
[`docs/plans/parity-determinism-fix.md`](../../docs/plans/parity-determinism-fix.md) §1**, not
something re-measured for this capture. No pre-fix run was performed here, and
the old commit was not checked out.

| Sample | Failures | When |
|---|---|---|
| 25 runs | 2 | pre-fix |
| 40 runs | 4 | pre-fix |
| 25 runs | 2 | pre-fix, re-measured during the fix work |
| **100 runs** | **0** | **post-fix — this capture** |

Roughly an 8–10% per-run failure rate before, which is why a 40-run green streak
was never sufficient evidence. At an 8% base rate, 100 clean runs by luck has
probability about `0.92^100 ≈ 2 × 10⁻⁴`.

The failure mode was an ordering swap between two near-tied concepts
(`redis backend` and `handlers/login.rs`), not a tie-break problem — the scores
themselves differed run to run. An experiment adding `canonical_key` as a
tie-break was tried during the fix work and changed nothing (4 failures in 40,
unchanged); it was reverted. Details in the plan doc §3.

## 5. What this does and does not establish

**It establishes:** on this Linux `x86_64` machine, at commit `0006824`, with
this toolchain, the demo's OUTCOME block was byte-identical across two runs on
100 consecutive attempts on a frozen build; and across 20 further run-pairs, two
raw demo runs differed only at three known-intentional sites, all outside the
asserted block, with every rendered score and the GC headroom scalar stable.

**It does not establish:**

* **Anything cross-platform.** This is one machine, one OS, one architecture,
  one rustc. The bug first surfaced on `macos-arm64`, and nothing here speaks to
  macOS or Windows. The five-target CI release build is the separate thing that
  will confirm those; this capture is not a substitute for it.
* **Cross-platform float reproducibility.** `normalize_score` is still applied
  by the test. Scores agree across runs *on one machine*; the ×2 bar asserts the
  outcome — which concepts, in which order, with which warnings — not the f64
  summation order of the scoring loop.
* **That the demo's full stdout is deterministic.** It is not, and is not
  intended to be: the session id and the warning node id are deliberately
  random, and the narrated cycle index is a known residual (plan doc §7 item 2).
  The claim is scoped to the OUTCOME block.
* **That 100 passes prove the property universally.** It is strong evidence
  against a ~8% failure rate, not a proof. The argument that the property now
  holds *by construction* is in the plan doc §4; this capture is the empirical
  half.

## 6. Relation to the earlier live-cluster capture

[`../demo-live-diff.txt`](../demo-live-diff.txt) records `IDENTICAL - T8.4 x2 met`
for two live-cluster demo runs. That capture was taken **while this bug was
live**, so it passed probabilistically — at roughly 90% per attempt — rather
than by construction. Its conclusion still stands, and it is the only capture of
this property against a real CockroachDB cluster, but it is the weaker evidence:
a single successful pair drawn from a distribution that failed about one time in
ten.

This directory is the stronger form of the same claim, against SQLite rather
than a live cluster. Neither file has been modified; `demo-live-diff.txt` is
left exactly as captured.

## Reproducing

```
$ git checkout 00068249b7cbcb490315a46ee8d034a3f84c9405
$ cargo build --features ship --tests --bins
$ ./evidence/demo-determinism/run-parity-100.sh        # edit <REPO>/<OUT> first
$ ./evidence/demo-determinism/run-raw-diff-pairs.sh    # edit <REPO>/<OUT> first
```

The test is gated `#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]`.
**Without `--features ship` the harness runs 0 tests and reports green** — every
measurement here uses `--features ship` for that reason.
