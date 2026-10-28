# #198: the suspected-supersession trigger, measured

The trigger (`src/graph/suspected.rs`, text rule in
`src/graph/suspected/slot.rs`) flags a hybrid write whose vector hit at or
above the merge threshold was refused by the identifier guard, when both
texts are the same `Logic`, `Constraint` or `Observation` type, share a
`repo_scope`, and differ in **one token span that starts after the first
predicate cue in both** ("one span and value slot", owner decision of
2026-10-27). This directory holds the measurement behind owner decision 1's
precision bar (0.90 per embedder) and the comparison of the Rust trigger
with the Python reference it was built from.

## Result (BGE-M3, #211 pairs, in regime, cosine >= 0.85)

In regime: both texts the same eligible type, not awaiting review, refused
by the identifier guard, labelled `supersedes`, `distinct_entity` or
`same-event`. 37 / 34 / 4 pairs on dev, 25 / 23 / 1 on held-out. A flagged
`supersedes` pair is a true positive; a flagged `distinct_entity` or
`same-event` pair is a false positive.

| Rule | Dev P (95% low) | Dev recall | Held-out P (95% low) | Held-out recall |
|---|---:|---:|---:|---:|
| masked Jaccard >= 0.9 (dropped) | 0.55 (0.43) | 34/37 | 0.53 (0.39) | 23/25 |
| one span | 0.78 (0.64) | 35/37 | 0.74 (0.57) | 25/25 |
| value slot | 0.87 (0.73) | 34/37 | 0.89 (0.73) | 25/25 |
| one span and value slot, reference lexicon | 0.97 (0.85) | 34/37 | 0.96 (0.81) | 25/25 |
| **one span and value slot, shipped lexicon (+ `reports`)** | **0.97 (0.86)** | **35/37** | **0.96 (0.81)** | **25/25** |

**Rust and Python agree exactly.** `rust-measure.txt` (the Rust trigger,
`examples/suspected_measure.rs`) reproduces `slot.txt` (the reference
script) row for row: the same in-regime counts (the Rust identifier guard
and the script's approximation of it agree on every pair here), the same
TP/FP per rule and split, the same false positives and misses. The
alignment is a port of `difflib.SequenceMatcher(autojunk=False)` over the
script's tokens, so this is expected, and the measurement confirms it.

**The one deliberate difference is `reports`.** The shipped lexicon
(`PREDICATE_CUES`) is the script's plus the verb `reports`, whose omission
made the dev supersession `p-198-s34` ("The demo's user schema reports 15
dependents.") a miss. It was added after looking at **dev only**; it moves
dev recall from 34/37 to 35/37 with no new false positive, and leaves
held-out unchanged (`slot-reports.txt` is the script with `--with-reports`,
and agrees with the Rust shipped-lexicon rows).

False positives: `p-198-s29` (dev) and `p-198-s30` (held-out), relabelled
distinct because the two measurements differ in storage format, which
neither text names, so no text-only rule can separate them. Misses on dev:
`p-198-s37` (two numbers change at once: two spans) and `p-cnd-06-rbd1` (a
reworded real supersession).

## Validated contracts

`src/recall/suspected.rs` `VALIDATED_CONTRACTS` lists BGE-M3 only (candle
`BAAI/bge-m3@…` and `bge_m3`, 1024 dimensions). Under any other contract
the default `[embedder] suspected_supersession_line = "validated"` shows no
warning line, only the structured `possibly_superseded_by` field.

## Caveats

* **The cue lexicon is hand-written English.** A predicate verb missing
  from it turns a real supersession into a miss (never a false warning).
  Extend it only against the dev split.
* **The pairs are authored templates.** Real dogfood supersessions are
  mostly reworded (masked token overlap 0.12 to 0.46 across 609
  guard-refused pairs), so recall on real memory will be low. Precision on
  real memory is unmeasured until about 100 of those pairs are labelled;
  `lambo canon suspected` on a copy of the dogfood store is that check.
* **The held-out lower bound (0.81) does not yet prove 0.90.** The point
  estimate clears the bar; the 95% Wilson interval on 26 flagged pairs does
  not.
* **EmbeddingGemma 2 is not measured.** Its calibrated threshold is 0.93;
  which pairs reach the trigger under it is unknown, so it gets the
  structured field only.
* The cosines are self-framed (`"{t} — {t}"`, what a one-concept
  `lambo_derive` embeds), from one `eval_measure` run: candle on CPU,
  `BAAI/bge-m3@5617a9f61b028005a4858fdac845db406aefb181`, weights sha256
  prefix `68440cc1b73b`, 2026-10-27.

## Files

| File | What |
|---|---|
| `slot.py` | The reference script, as measured; inputs default to this repository. `--with-reports` adds `reports`. |
| `regime.py` | The helpers `slot.py` reads (the reviewer's `same`/`jac`). |
| `slot.txt` | `slot.py` output (reference lexicon). |
| `slot-reports.txt` | `slot.py --with-reports` output. |
| `cosines-bge-m3-self.json` | The self-framed per-pair BGE-M3 cosines (and the Rust guard's verdict) the two read. |
| `rust-measure.txt` | `examples/suspected_measure.rs` output. |

## Re-run

From the repository root:

```text
python3 -I evidence/issue-198-trigger/slot.py > evidence/issue-198-trigger/slot.txt
python3 -I evidence/issue-198-trigger/slot.py --with-reports > evidence/issue-198-trigger/slot-reports.txt
cargo run --example suspected_measure > evidence/issue-198-trigger/rust-measure.txt
```

New cosines (another embedder, or new pairs): run `eval_measure` with
`--detail <path>` under that embedder and pass the detail file to both
(`slot.py <repo> <detail.json>`, `suspected_measure <pairs.json>
<detail.json>`).
