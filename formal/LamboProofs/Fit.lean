/-
# The token budget keeps the longest ranked prefix of whole blocks

Model of the whole-block loop of `fit_blocks` in `src/recall/format/fit.rs`:

```rust
for block in blocks {
    let before = context.len();
    if !context.is_empty() { context.push_str(SEPARATOR); }
    context.push_str(&block.text);
    if !fits(token_fn(&context), max_tokens) { context.truncate(before); break; }
    whole += 1;
}
```

## Model

After `whole` successful iterations `context` is the join of the first `whole`
blocks, so the test in iteration `whole` is a function of `whole + 1` alone.
The model therefore takes a predicate

    ok k  :=  fits(token_fn(join(blocks[..k])), max_tokens)

over prefix lengths and nothing else: the estimator, the separator and the
block texts are all inside `ok`. No property of the estimator is assumed
except where a theorem says so.

`fitFrom ok rem whole` is the loop with `rem` blocks left to try and `whole`
already accepted; `fit ok n` is the loop over `n` blocks.

Not modelled: rule 2 of the module (#130), which truncates the *first* block
when not even it fits. That path runs only when the loop below returns `0`.

## Results

* `fit_le`: `whole` is at most the number of blocks.
* `fit_prefix_ok`: every non-empty accepted prefix fits. In particular the
  rendered context fits, "for any estimator".
* `fit_stops_at_first_miss`: if a block was left out, the prefix one longer
  does not fit. The loop never skips a block and continues.
* `fit_longest`: when fitting is downward closed (a prefix of a fitting prefix
  fits, as for any estimator monotone in the text), `whole` is the length of
  the *longest* fitting prefix.
* `take_fit_prefix`: the rendered blocks are a prefix of the ranked list.
-/

namespace Lambo.Fit

/-- The loop, with `rem` blocks still to try and `whole` accepted so far. -/
def fitFrom (ok : Nat → Bool) : Nat → Nat → Nat
  | 0, whole => whole
  | rem + 1, whole => if ok (whole + 1) = true then fitFrom ok rem (whole + 1) else whole

/-- `fit_blocks`' `whole` for `n` ranked blocks. -/
def fit (ok : Nat → Bool) (n : Nat) : Nat := fitFrom ok n 0

theorem fitFrom_zero (ok : Nat → Bool) (whole : Nat) : fitFrom ok 0 whole = whole := by
  simp [fitFrom]

theorem fitFrom_hit {ok : Nat → Bool} {whole : Nat} (h : ok (whole + 1) = true) (rem : Nat) :
    fitFrom ok (rem + 1) whole = fitFrom ok rem (whole + 1) := by
  simp [fitFrom, h]

theorem fitFrom_miss {ok : Nat → Bool} {whole : Nat} (h : ¬ ok (whole + 1) = true)
    (rem : Nat) : fitFrom ok (rem + 1) whole = whole := by
  simp [fitFrom, h]

theorem fitFrom_ge (ok : Nat → Bool) :
    ∀ (rem whole : Nat), whole ≤ fitFrom ok rem whole := by
  intro rem
  induction rem with
  | zero =>
    intro whole
    rw [fitFrom_zero] <;> exact Nat.le_refl whole
  | succ r ih =>
    intro whole
    by_cases h : ok (whole + 1) = true
    · rw [fitFrom_hit h]
      have := ih (whole + 1)
      omega
    · rw [fitFrom_miss h] <;> exact Nat.le_refl whole

theorem fitFrom_le (ok : Nat → Bool) :
    ∀ (rem whole : Nat), fitFrom ok rem whole ≤ whole + rem := by
  intro rem
  induction rem with
  | zero =>
    intro whole
    rw [fitFrom_zero] <;> omega
  | succ r ih =>
    intro whole
    by_cases h : ok (whole + 1) = true
    · rw [fitFrom_hit h]
      have := ih (whole + 1)
      omega
    · rw [fitFrom_miss h] <;> omega

/-- Every prefix the loop accepted along the way fits. -/
theorem fitFrom_ok (ok : Nat → Bool) :
    ∀ (rem whole k : Nat), whole < k → k ≤ fitFrom ok rem whole → ok k = true := by
  intro rem
  induction rem with
  | zero =>
    intro whole k h1 h2
    rw [fitFrom_zero] at h2
    omega
  | succ r ih =>
    intro whole k h1 h2
    by_cases h : ok (whole + 1) = true
    · rw [fitFrom_hit h] at h2
      by_cases hk : k = whole + 1
      · rw [hk]
        exact h
      · exact ih (whole + 1) k (by omega) h2
    · rw [fitFrom_miss h] at h2
      omega

/-- If the loop stopped before running out of blocks, the next prefix does not
fit. -/
theorem fitFrom_stop (ok : Nat → Bool) :
    ∀ (rem whole : Nat), fitFrom ok rem whole < whole + rem →
      ok (fitFrom ok rem whole + 1) = false := by
  intro rem
  induction rem with
  | zero =>
    intro whole h
    rw [fitFrom_zero] at h
    omega
  | succ r ih =>
    intro whole h
    by_cases hok : ok (whole + 1) = true
    · rw [fitFrom_hit hok] at h ⊢
      exact ih (whole + 1) (by omega)
    · rw [fitFrom_miss hok]
      cases hb : ok (whole + 1) with
      | false => rfl
      | true => exact absurd hb hok

/-- Fitting is downward closed: dropping the last block of a fitting prefix
leaves a fitting prefix. True of any estimator that is monotone in the text,
such as the default `ceil(bytes / 3.5)`. -/
def DownwardClosed (ok : Nat → Bool) : Prop := ∀ k, ok (k + 1) = true → ok k = true

theorem DownwardClosed.le {ok : Nat → Bool} (h : DownwardClosed ok) :
    ∀ (d j : Nat), ok (j + d) = true → ok j = true := by
  intro d
  induction d with
  | zero =>
    intro j hj
    exact hj
  | succ d ih =>
    intro j hj
    have hj' : ok (j + d + 1) = true := by
      first
        | exact hj
        | (rw [Nat.add_assoc]; exact hj)
    exact ih j (h (j + d) hj')

/-! ## The four statements about `fit_blocks` -/

theorem fit_le (ok : Nat → Bool) (n : Nat) : fit ok n ≤ n := by
  have := fitFrom_le ok n 0
  unfold fit
  omega

/-- **Within budget.** Every accepted non-empty prefix fits, and so does the
rendered context (`k = fit ok n`), for any estimator. -/
theorem fit_prefix_ok (ok : Nat → Bool) (n k : Nat) (h1 : 0 < k) (h2 : k ≤ fit ok n) :
    ok k = true :=
  fitFrom_ok ok n 0 k h1 h2

/-- **No skipping.** If some block was left out, then adding the very next
block would not have fitted. Together with `fit_prefix_ok` this is "the cut
stops at the first block that does not fit". -/
theorem fit_stops_at_first_miss (ok : Nat → Bool) (n : Nat) (h : fit ok n < n) :
    ok (fit ok n + 1) = false := by
  unfold fit at h ⊢
  exact fitFrom_stop ok n 0 (by omega)

/-- **Longest prefix.** With a downward-closed fit test, no longer prefix of
the ranked blocks fits. -/
theorem fit_longest {ok : Nat → Bool} (hdc : DownwardClosed ok) (n k : Nat)
    (hk : k ≤ n) (hfit : ok k = true) : k ≤ fit ok n := by
  by_cases hle : k ≤ fit ok n
  · exact hle
  · have hlt : fit ok n < n := by omega
    have hmiss := fit_stops_at_first_miss ok n hlt
    have e : fit ok n + 1 + (k - (fit ok n + 1)) = k := by omega
    have hhit : ok (fit ok n + 1) = true := by
      apply hdc.le (k - (fit ok n + 1)) (fit ok n + 1)
      rw [e]
      exact hfit
    rw [hmiss] at hhit
    exact absurd hhit (by decide)

/-- The rendered blocks are a prefix of the ranked list: nothing is reordered
and a lower-ranked block never appears without every higher-ranked one. -/
theorem take_fit_prefix {α : Type} (ok : Nat → Bool) (blocks : List α) :
    ∃ dropped, blocks.take (fit ok blocks.length) ++ dropped = blocks :=
  ⟨blocks.drop (fit ok blocks.length), List.take_append_drop _ _⟩

/-- A zero budget (nothing fits) renders no whole block. -/
theorem fit_of_nothing_fits (ok : Nat → Bool) (n : Nat) (h : ∀ k, ok k = false) :
    fit ok n = 0 := by
  cases n with
  | zero => simp [fit, fitFrom]
  | succ m =>
    have h1 : ¬ ok (0 + 1) = true := by
      rw [h (0 + 1)]
      decide
    unfold fit
    exact fitFrom_miss h1 m

/-- When every prefix fits, every block is rendered. -/
theorem fit_of_all_fit (ok : Nat → Bool) (n : Nat) (h : ∀ k, ok k = true) : fit ok n = n := by
  have hle := fit_le ok n
  by_cases hlt : fit ok n < n
  · have hmiss := fit_stops_at_first_miss ok n hlt
    rw [h (fit ok n + 1)] at hmiss
    exact absurd hmiss (by decide)
  · omega

end Lambo.Fit
