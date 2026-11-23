import LamboProofs.Lib.List

/-
# The token budget keeps the longest ranked prefix of whole blocks

Model of `fit_blocks` in `src/recall/format/fit.rs`. The first part models
its whole-block loop:

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
except where a theorem says so. Rule 2 (#130), which truncates the first
block when not even it fits, is the second part.

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

## Rule 2: the oversize first block (#130)

The second part is concrete. A block is three lists, `content`, `label` (the
rest of the label line) and `warn` (the warning lines, each after a `"\n"`),
the three slices `ContextBlock`'s offsets `content_len` and `label_end` cut
`text` into, plus its marker `mark n = truncation_marker(n, inspect_id,
scope)`. Elements are chars, so every cut is a char boundary; the estimator
`est` is any function of the text. `fitBlocks` is all of `fit_blocks`:

* the whole-block loop, joining as the Rust does (`ctxOf`: the separator only
  after a non-empty context), with `fit` from the first part for its count;
* when it accepted nothing and there is a first block, `truncate_first`
  (`truncFirst`): the forms `WithWarnings`, `LabelOnly` (only when the block
  has warnings) and `ContentOnly`, in order. A candidate at cut `c` is
  `content[..c] ++ "…" ++ kept rest ++ "\n" ++ mark (est left_out)`, where
  `left_out = content[c..] ++ the rest not kept`; `ContentOnly` at cut `0` is
  the marker alone. Cuts are `content_cuts` (`0`, then each position whose
  preceding char is not whitespace). A form whose cut `0` does not fit is
  skipped; the first whose cut `0` fits is binary-searched (`bsearch`, the
  Rust loop with enough fuel) and returned; nothing follows
  it. If no form's cut `0` fits, the context is empty.

Exact arithmetic: `fits t m = t < usize::MAX ∧ t ≤ m` on naturals, with
`usizeMax = 2^64 - 1`, which assumes a 64-bit target. `defaultG b = (2b + 6) /
7` is `ceil(b / 3.5) = ceil(2b / 7)`. The Rust computes `(b as f64 / 3.5).ceil()`,
which agrees for every `b < 2^52`: `b` and `3.5` are exact in `f64`, and the
division is correctly rounded. If `2b/7` is an integer `k < 2^51`, it is
representable and the quotient is exactly `k`. Otherwise `2b/7` lies at least
`1/7` from every integer, while the rounding error is at most half an ulp,
at most `1/8` below `2^51`, so the rounded quotient lies strictly between the
same two integers and has the same ceiling.

Results for any estimator:

* `fitBlocks_whole`, `fitBlocks_of_first_fits`: `whole` is always the loop's
  `fit`, and when the first block fits the result is exactly the pre-#130
  loop's (`plainFit`; test
  `whole_blocks_are_byte_identical_to_the_pre_130_loop`).
* `fitBlocks_fits`: whatever is rendered fits `max_tokens` as measured by the
  same estimator ("the result is within budget for any estimator"), so the
  context never exceeds `max_tokens`. `fitBlocks_empty`: nothing rendered is
  `""`.
* `fitBlocks_rendered_pos`, `fitBlocks_context_ne_nil`: with at least one
  block, if the marker alone (the whole first block counted as left out) fits,
  the context is not empty.
* `fitBlocks_truncated`, `fitBlocks_truncated_prefix`, `mem_contentCuts`,
  `text_split`: a truncated context is a prefix of the first block's content
  ending in no whitespace, `…`, a prefix of the rest of the block, a newline
  and a marker counting exactly the text left out, or the marker alone; it
  happens only with `whole = 0` and the first block not fitting
  (`fitBlocks_truncation_whole`, `mark_context`'s `debug_assert`).
* `bsearch_spec`: the search returns a cut that fits and whose successor does
  not (or is past the end), whatever the estimator.

Results that need the estimator to be `Steady` (the count is a monotone
function of the byte size rising at most one per byte, and the marker grows by
at most one byte per unit of count). The default estimator and the exact byte
estimator with the real marker are `Steady` (`rust_default_steady`,
`rust_bytes_steady`):

* `truncFirst_longest`, `rust_default_longest`: the head is the longest that
  fits in its form, and no earlier form fits at any cut ("The first form that
  fits wins"; test `the_head_is_the_longest_that_fits`).
* `fitBlocks_rendered_zero_iff`: when the first block does not fit, nothing
  is rendered exactly when the marker alone does not fit (rule 3).

The longest head does not hold for every estimator monotone in bytes, which is
what the module doc's Arithmetic paragraph said sufficed until #302 (it now
states the `Steady` condition and cites `rust_default_steady` and
`rust_jump_skips_a_fitting_form`):
`rust_jump_skips_a_fitting_form` is a byte-faithful counterexample on a real
block (an estimator whose count jumps by `10^19` past 90 bytes), and
`jump_skips_a_fitting_form` a toy-sized one.

Rule 3 (which states no estimator condition) does hold for every estimator
monotone in bytes on blocks of the Rust's sizes (`rust_rule3_of_mono`: label
rest at least 16 bytes, counts at most `usize::MAX`), so on real blocks it can
fail only for an estimator that is not monotone in bytes. The shared step is
`fitBlocks_rendered_zero_iff_of_le`: rule 3 follows whenever the marker alone
is estimated no higher than every form's cut-`0` candidate.
`jump_renders_without_the_marker` shows that the size condition is needed:
with a one-unit label and a two-unit marker prefix a monotone estimator breaks
it.

Every caller in the crate passes `default_token_count`, which is `Steady`, so
these are precision points about the doc, not bugs in a shipped path.

Boundary: `ContextBlock::for_hit` and `with_meta` are not modelled (the three
slices are given); `fits` assumes a 64-bit target, and `defaultG` agrees with
the Rust's `f64` ceiling only for `b < 2^52` bytes.
-/

namespace Lambo.Fit

open Lambo.Lib (getD_mem getD_mono getD_eq_getElem)

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

/-- When no prefix fits the budget, `fit` renders no whole block. -/
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

/-- When every prefix fits the budget, `fit` renders every block. -/
theorem fit_of_all_fit (ok : Nat → Bool) (n : Nat) (h : ∀ k, ok k = true) : fit ok n = n := by
  have hle := fit_le ok n
  by_cases hlt : fit ok n < n
  · have hmiss := fit_stops_at_first_miss ok n hlt
    rw [h (fit ok n + 1)] at hmiss
    exact absurd hmiss (by decide)
  · omega

/-! ## Rule 2: the oversize first block (#130)

Everything above is about `fit ok n`, the whole-block count. From here on the
model is concrete: blocks are lists, the estimator is a function on lists, and
`fitBlocks` is all of `fit_blocks`, the truncation path included. -/

section Truncation

variable {α : Type}

/-- `usize::MAX` on a 64-bit target.
Pinned by `formal_constants_tests::fit`. -/
def usizeMax : Nat := 2 ^ 64 - 1

/-- `fits`: within budget and not the saturated `usize::MAX`
(`checked_add(1)` fails exactly at `usize::MAX`). -/
def fits (tokens maxTokens : Nat) : Bool :=
  decide (tokens < usizeMax) && decide (tokens ≤ maxTokens)

/-- A `ContextBlock`, split at its two offsets: `text[..content_len]`,
`text[content_len..label_end]` and `text[label_end..]`. `mark n` is
`truncation_marker(n, inspect_id, scope)`: the block's id and scope are inside
it. -/
structure Block (α : Type) where
  content : List α
  label : List α
  warn : List α
  mark : Nat → List α

/-- `ContextBlock::text`. -/
def Block.text (b : Block α) : List α := b.content ++ b.label ++ b.warn

/-- The string constants and the estimator: `SEPARATOR = "\n\n"`,
`ELLIPSIS = "…"`, the `"\n"` before the marker, `token_fn`, and
`char::is_whitespace`. -/
structure Env (α : Type) where
  est : List α → Nat
  sep : List α
  ell : List α
  nl : List α
  ws : α → Bool

/-- `Truncation`. -/
inductive Form where
  | withWarnings
  | labelOnly
  | contentOnly
  deriving DecidableEq, Repr

/-- The forms `truncate_first` tries, in order. `LabelOnly` is tried only
when `label_end < text.len()`, i.e. when the block has warning lines. -/
def forms (b : Block α) : List Form :=
  Form.withWarnings :: ((if b.warn = [] then [] else [Form.labelOnly]) ++ [Form.contentOnly])

/-- `text[content_len..kept_end]`: what of the block after the content is
kept. -/
def keptRest (b : Block α) : Form → List α
  | .withWarnings => b.label ++ b.warn
  | .labelOnly => b.label
  | .contentOnly => []

/-- `text[kept_end..]`: what of the block after the content is left out. -/
def dropRest (b : Block α) : Form → List α
  | .withWarnings => []
  | .labelOnly => b.warn
  | .contentOnly => b.label ++ b.warn

/-- `render(cut)` inside `truncate_first`: the content head, `…`, the kept
rest, a newline and the marker, whose count is the estimator's count for
exactly the left-out text. `ContentOnly` at cut `0` is the marker alone. -/
def render (e : Env α) (b : Block α) (f : Form) (cut : Nat) : List α :=
  if f = .contentOnly ∧ cut = 0 then b.mark (e.est (b.content.drop cut ++ dropRest b f))
  else b.content.take cut ++ e.ell ++ keptRest b f ++ e.nl ++
    b.mark (e.est (b.content.drop cut ++ dropRest b f))

/-- The loop of `content_cuts` from position `i`, `prevWs` being whether the
char before `i` is whitespace. -/
def cutsFrom (ws : α → Bool) : Nat → Bool → List α → List Nat
  | _, _, [] => []
  | i, prevWs, c :: rest =>
    if 0 < i ∧ prevWs = false then i :: cutsFrom ws (i + 1) (ws c) rest
    else cutsFrom ws (i + 1) (ws c) rest

/-- `content_cuts`: `0`, then every char boundary before the end that does not
follow whitespace. Positions count chars, so every cut is a char boundary. -/
def contentCuts (ws : α → Bool) (content : List α) : List Nat :=
  0 :: cutsFrom ws 0 false content

/-- The binary search of `truncate_first`, over indices into the cut list:

```rust
let (mut lo, mut hi) = (0usize, cuts.len());
while hi - lo > 1 {
    let mid = lo + (hi - lo) / 2;
    if ok(cuts[mid]) { lo = mid; } else { hi = mid; }
}
```

`fuel` bounds the iterations; each one shrinks `hi - lo`, so
`fuel = cuts.len()` never runs out. -/
def bsearch (p : Nat → Bool) : Nat → Nat → Nat → Nat
  | 0, lo, _ => lo
  | fuel + 1, lo, hi =>
    if hi - lo ≤ 1 then lo
    else if p (lo + (hi - lo) / 2) = true then bsearch p fuel (lo + (hi - lo) / 2) hi
    else bsearch p fuel lo (lo + (hi - lo) / 2)

/-- The fit test of one form at cut index `i`: `ok(cuts[i])`. -/
def okCut (e : Env α) (b : Block α) (maxT : Nat) (f : Form) (i : Nat) : Bool :=
  fits (e.est (render e b f ((contentCuts e.ws b.content).getD i 0))) maxT

/-- The cut index the search returns for one form. -/
def bestIdx (e : Env α) (b : Block α) (maxT : Nat) (f : Form) : Nat :=
  let n := (contentCuts e.ws b.content).length
  bsearch (okCut e b maxT f) n 0 n

/-- The loop over the forms in `truncate_first`: a form whose cut `0` does not
fit is skipped (`continue`); the first whose cut `0` fits is searched and
returned. -/
def tryForms (e : Env α) (b : Block α) (maxT : Nat) : List Form → Option (List α × Form)
  | [] => none
  | f :: fs =>
    if okCut e b maxT f 0 = true then
      some (render e b f ((contentCuts e.ws b.content).getD (bestIdx e b maxT f) 0), f)
    else tryForms e b maxT fs

/-- `truncate_first`. -/
def truncFirst (e : Env α) (b : Block α) (maxT : Nat) : Option (List α × Form) :=
  tryForms e b maxT (forms b)

/-- One step of the whole-block loop: the separator only when the context is
not empty. -/
def appendBlock (sep ctx t : List α) : List α :=
  if ctx = [] then t else ctx ++ sep ++ t

/-- The context after the whole-block loop has accepted `texts`. -/
def ctxOf (sep : List α) (texts : List (List α)) : List α :=
  texts.foldl (appendBlock sep) []

/-- `ok k` of the abstract model, for concrete blocks: the first `k` blocks,
joined as the loop joins them, fit. -/
def okOf (e : Env α) (blocks : List (Block α)) (maxT : Nat) (k : Nat) : Bool :=
  fits (e.est (ctxOf e.sep ((blocks.take k).map Block.text))) maxT

/-- `FittedContext`. -/
structure Fitted (α : Type) where
  context : List α
  whole : Nat
  truncation : Option Form
  deriving DecidableEq

/-- `FittedContext::rendered`. -/
def Fitted.rendered (r : Fitted α) : Nat := r.whole + (if r.truncation.isSome then 1 else 0)

/-- What the whole-block loop alone renders. -/
def plainFit (e : Env α) (blocks : List (Block α)) (maxT : Nat) : Fitted α :=
  let whole := fit (okOf e blocks maxT) blocks.length
  ⟨ctxOf e.sep ((blocks.take whole).map Block.text), whole, none⟩

/-- `fit_blocks`: the whole-block loop, then rule 2 when it rendered nothing
and there is a first block. -/
def fitBlocks (e : Env α) (blocks : List (Block α)) (maxT : Nat) : Fitted α :=
  if fit (okOf e blocks maxT) blocks.length = 0 then
    match blocks with
    | [] => plainFit e blocks maxT
    | b :: _ =>
      match truncFirst e b maxT with
      | some (t, f) => ⟨t, 0, some f⟩
      | none => plainFit e blocks maxT
  else plainFit e blocks maxT

/-! ### The binary search -/

/-- The search keeps the Rust's invariant and ends on a boundary: the index it
returns fits, and the next one does not or is past the end. True for any
predicate; no monotonicity is needed. -/
theorem bsearch_spec (p : Nat → Bool) (n : Nat) :
    ∀ (fuel lo hi : Nat), hi - lo ≤ fuel + 1 → lo < hi → p lo = true →
      (hi = n ∨ p hi = false) →
      lo ≤ bsearch p fuel lo hi ∧ bsearch p fuel lo hi < hi ∧
      p (bsearch p fuel lo hi) = true ∧
      (bsearch p fuel lo hi + 1 = n ∨ p (bsearch p fuel lo hi + 1) = false) := by
  intro fuel
  induction fuel with
  | zero =>
    intro lo hi h1 h2 h3 h4
    have e : hi = lo + 1 := by omega
    simp only [bsearch]
    subst e
    exact ⟨Nat.le_refl lo, Nat.lt_succ_self lo, h3, h4⟩
  | succ f ih =>
    intro lo hi h1 h2 h3 h4
    by_cases hs : hi - lo ≤ 1
    · have e : hi = lo + 1 := by omega
      simp only [bsearch, ite_eq_left hs]
      subst e
      exact ⟨Nat.le_refl lo, Nat.lt_succ_self lo, h3, h4⟩
    · simp only [bsearch, ite_eq_right hs]
      by_cases hm : p (lo + (hi - lo) / 2) = true
      · rw [ite_eq_left hm]
        have := ih (lo + (hi - lo) / 2) hi (by omega) (by omega) hm h4
        exact ⟨by omega, this.2.1, this.2.2.1, this.2.2.2⟩
      · rw [ite_eq_right hm]
        have hf : p (lo + (hi - lo) / 2) = false := by
          cases hb : p (lo + (hi - lo) / 2) with
          | false => rfl
          | true => exact absurd hb hm
        have := ih lo (lo + (hi - lo) / 2) (by omega) (by omega) h3 (Or.inr hf)
        exact ⟨this.1, by omega, this.2.2.1, this.2.2.2⟩

/-- Downward closed below `n`: if index `i + 1` fits, so does `i`. -/
def DownBelow (p : Nat → Bool) (n : Nat) : Prop := ∀ i, i + 1 < n → p (i + 1) = true → p i = true

theorem DownBelow.le {p : Nat → Bool} {n : Nat} (h : DownBelow p n) :
    ∀ (d i : Nat), i + d < n → p (i + d) = true → p i = true := by
  intro d
  induction d with
  | zero => intro i _ hp; exact hp
  | succ d ih =>
    intro i hlt hp
    exact ih i (by omega) (h (i + d) (by omega) (by rw [← Nat.add_assoc] at hp; exact hp))

/-- With a downward-closed predicate the boundary is the maximum: the search
returns the last index that fits. -/
theorem bsearch_longest {p : Nat → Bool} {n : Nat} (hdc : DownBelow p n) (hn : 0 < n)
    (h0 : p 0 = true) {j : Nat} (hj : j < n) (hpj : p j = true) :
    j ≤ bsearch p n 0 n := by
  have hs := bsearch_spec p n n 0 n (by omega) hn h0 (Or.inl rfl)
  generalize bsearch p n 0 n = r at hs
  by_cases hle : j ≤ r
  · exact hle
  · have hmiss : p (r + 1) = false := by
      rcases hs.2.2.2 with h | h
      · omega
      · exact h
    have := hdc.le (j - (r + 1)) (r + 1) (by omega) (by rw [show r + 1 + (j - (r + 1)) = j by omega]; exact hpj)
    rw [hmiss] at this
    exact absurd this (by decide)

/-! ### The cut list -/

theorem mem_cutsFrom (ws : α → Bool) :
    ∀ (xs : List α) (i : Nat) (prev : Bool) (c : Nat), c ∈ cutsFrom ws i prev xs →
      i ≤ c ∧ c < i + xs.length ∧ 0 < c ∧ (c = i → prev = false) ∧
      (i < c → ∃ x, xs[c - i - 1]? = some x ∧ ws x = false) := by
  intro xs
  induction xs with
  | nil => intro i prev c h; simp [cutsFrom] at h
  | cons x rest ih =>
    intro i prev c h
    have hrest : ∀ c, c ∈ cutsFrom ws (i + 1) (ws x) rest →
        i ≤ c ∧ c < i + (x :: rest).length ∧ 0 < c ∧ (c = i → prev = false) ∧
        (i < c → ∃ y, (x :: rest)[c - i - 1]? = some y ∧ ws y = false) := by
      intro c hc
      have := ih (i + 1) (ws x) c hc
      refine ⟨by omega, by simp; omega, this.2.2.1, by omega, ?_⟩
      intro _
      by_cases hci : c = i + 1
      · refine ⟨x, ?_, this.2.2.2.1 hci⟩
        rw [show c - i - 1 = 0 by omega]
        rfl
      · obtain ⟨y, hy, hwy⟩ := this.2.2.2.2 (by omega)
        refine ⟨y, ?_, hwy⟩
        rw [show c - i - 1 = (c - (i + 1) - 1) + 1 by omega, List.getElem?_cons_succ]
        exact hy
    simp only [cutsFrom] at h
    split at h
    · rename_i hcond
      rcases List.mem_cons.mp h with he | hm
      · subst he
        refine ⟨Nat.le_refl _, by simp, hcond.1, fun _ => hcond.2, fun h => absurd h (Nat.lt_irrefl _)⟩
      · exact hrest c hm
    · exact hrest c h

/-- **The head never ends in whitespace.** A cut is `0`, or a position inside
the content whose preceding char is not whitespace. -/
theorem mem_contentCuts {ws : α → Bool} {xs : List α} {c : Nat} (h : c ∈ contentCuts ws xs) :
    c = 0 ∨ (0 < c ∧ c < xs.length ∧ ∃ x, xs[c - 1]? = some x ∧ ws x = false) := by
  rcases List.mem_cons.mp h with h0 | hm
  · exact Or.inl h0
  · have := mem_cutsFrom ws xs 0 false c hm
    obtain ⟨x, hx, hw⟩ := this.2.2.2.2 (by omega)
    exact Or.inr ⟨this.2.2.1, by omega, x, by simpa using hx, hw⟩

theorem cutsFrom_pairwise (ws : α → Bool) :
    ∀ (xs : List α) (i : Nat) (prev : Bool), (cutsFrom ws i prev xs).Pairwise (· < ·) := by
  intro xs
  induction xs with
  | nil => intro i prev; simp [cutsFrom]
  | cons x rest ih =>
    intro i prev
    simp only [cutsFrom]
    split
    · refine List.pairwise_cons.mpr ⟨?_, ih (i + 1) (ws x)⟩
      intro c hc
      have := mem_cutsFrom ws rest (i + 1) (ws x) c hc
      omega
    · exact ih (i + 1) (ws x)

/-- The cuts are strictly increasing, so the search over indices is a search
over head lengths. -/
theorem contentCuts_pairwise (ws : α → Bool) (xs : List α) :
    (contentCuts ws xs).Pairwise (· < ·) := by
  refine List.pairwise_cons.mpr ⟨?_, cutsFrom_pairwise ws xs 0 false⟩
  intro c hc
  exact (mem_cutsFrom ws xs 0 false c hc).2.2.1

theorem contentCuts_length_pos (ws : α → Bool) (xs : List α) :
    0 < (contentCuts ws xs).length := by
  simp [contentCuts]

/-! ### One form, and the loop over the forms -/

theorem fits_anti {t t' m : Nat} (h : t ≤ t') (h' : fits t' m = true) : fits t m = true := by
  simp only [fits, Bool.and_eq_true, decide_eq_true_eq] at h' ⊢
  omega

/-- The marker alone, the shortest form, is `ContentOnly` at cut `0`, and it
counts the whole block text as left out. -/
theorem render_marker_alone (e : Env α) (b : Block α) :
    render e b .contentOnly 0 = b.mark (e.est b.text) := by
  simp [render, dropRest, Block.text]

/-- The shape of every candidate: a content head, `…`, the kept rest, a newline
and the marker; or, for `ContentOnly` at cut `0`, the marker alone. -/
theorem render_eq (e : Env α) (b : Block α) (f : Form) (cut : Nat) :
    render e b f cut =
      (if f = .contentOnly ∧ cut = 0 then []
       else b.content.take cut ++ e.ell ++ keptRest b f ++ e.nl) ++
      b.mark (e.est (b.content.drop cut ++ dropRest b f)) := by
  unfold render
  split <;> simp

/-- The kept rest and the left-out rest split what follows the content. -/
theorem keptRest_append_dropRest (b : Block α) (f : Form) :
    keptRest b f ++ dropRest b f = b.label ++ b.warn := by
  cases f <;> simp [keptRest, dropRest]

/-- **The marker counts exactly the text left out**: the block is the kept head,
the left-out content tail, the kept rest and the left-out rest, and the marker
is the estimator's count for the second and fourth. -/
theorem text_split (b : Block α) (f : Form) (cut : Nat) :
    b.text = b.content.take cut ++ b.content.drop cut ++ (keptRest b f ++ dropRest b f) := by
  rw [keptRest_append_dropRest, List.take_append_drop]
  simp [Block.text]

theorem render_ne_nil (e : Env α) (b : Block α) (hm : ∀ n, b.mark n ≠ []) (f : Form)
    (cut : Nat) : render e b f cut ≠ [] := by
  rw [render_eq]
  intro h
  exact hm _ (List.append_eq_nil_iff.mp h).2

theorem okCut_zero (e : Env α) (b : Block α) (maxT : Nat) (f : Form) :
    okCut e b maxT f 0 = fits (e.est (render e b f 0)) maxT := by
  simp [okCut, contentCuts]

/-- The index the search returns is in range and fits, whenever cut `0`
fits. -/
theorem bestIdx_spec (e : Env α) (b : Block α) (maxT : Nat) (f : Form)
    (h0 : okCut e b maxT f 0 = true) :
    bestIdx e b maxT f < (contentCuts e.ws b.content).length ∧
      okCut e b maxT f (bestIdx e b maxT f) = true := by
  have hn := contentCuts_length_pos e.ws b.content
  have := bsearch_spec (okCut e b maxT f) (contentCuts e.ws b.content).length
    (contentCuts e.ws b.content).length 0 (contentCuts e.ws b.content).length
    (by omega) hn h0 (Or.inl rfl)
  exact ⟨this.2.1, this.2.2.1⟩

/-- What the loop over the forms returns: some form `f` of the list, every form
before it failed at cut `0`, `f` fits at cut `0`, and the text is `f` rendered
at the cut the search found. -/
theorem tryForms_some (e : Env α) (b : Block α) (maxT : Nat) :
    ∀ (fs : List Form) (t : List α) (f : Form), tryForms e b maxT fs = some (t, f) →
      ∃ pre post, fs = pre ++ f :: post ∧ (∀ g ∈ pre, okCut e b maxT g 0 = false) ∧
        okCut e b maxT f 0 = true ∧
        t = render e b f ((contentCuts e.ws b.content).getD (bestIdx e b maxT f) 0) := by
  intro fs
  induction fs with
  | nil => intro t f h; simp [tryForms] at h
  | cons g gs ih =>
    intro t f h
    simp only [tryForms] at h
    by_cases hg : okCut e b maxT g 0 = true
    · rw [ite_eq_left hg] at h
      simp only [Option.some.injEq, Prod.mk.injEq] at h
      obtain ⟨ht, hf⟩ := h
      subst hf
      exact ⟨[], gs, rfl, by simp, hg, ht.symm⟩
    · rw [ite_eq_right hg] at h
      obtain ⟨pre, post, hfs, hpre, h0, ht⟩ := ih t f h
      refine ⟨g :: pre, post, by rw [hfs]; rfl, ?_, h0, ht⟩
      intro g' hg'
      rcases List.mem_cons.mp hg' with e' | e'
      · subst e'
        cases hb : okCut e b maxT g' 0 with
        | false => rfl
        | true => exact absurd hb hg
      · exact hpre g' e'

theorem tryForms_isSome (e : Env α) (b : Block α) (maxT : Nat) :
    ∀ (fs : List Form) (f : Form), f ∈ fs → okCut e b maxT f 0 = true →
      ∃ r, tryForms e b maxT fs = some r := by
  intro fs
  induction fs with
  | nil => intro f hf; simp at hf
  | cons g gs ih =>
    intro f hf h0
    simp only [tryForms]
    by_cases hg : okCut e b maxT g 0 = true
    · rw [ite_eq_left hg]; exact ⟨_, rfl⟩
    · rw [ite_eq_right hg]
      rcases List.mem_cons.mp hf with e' | e'
      · subst e'; exact absurd h0 hg
      · exact ih f e' h0

theorem tryForms_none (e : Env α) (b : Block α) (maxT : Nat) :
    ∀ (fs : List Form), (∀ f ∈ fs, okCut e b maxT f 0 = false) → tryForms e b maxT fs = none := by
  intro fs
  induction fs with
  | nil => intro _; rfl
  | cons g gs ih =>
    intro h
    simp only [tryForms]
    rw [ite_eq_right (by rw [h g (by simp)]; decide)]
    exact ih (fun f hf => h f (List.mem_cons_of_mem g hf))

theorem contentOnly_mem_forms (b : Block α) : Form.contentOnly ∈ forms b := by
  unfold forms
  split <;> simp

/-- `truncate_first`, unfolded: the form chosen, every earlier form failing at
cut `0`, and the text, a candidate at a cut from the cut list that fits. -/
theorem truncFirst_some {e : Env α} {b : Block α} {maxT : Nat} {t : List α} {f : Form}
    (h : truncFirst e b maxT = some (t, f)) :
    (∃ pre post, forms b = pre ++ f :: post ∧ ∀ g ∈ pre, okCut e b maxT g 0 = false) ∧
      okCut e b maxT f 0 = true ∧
      ∃ i, i < (contentCuts e.ws b.content).length ∧
        t = render e b f ((contentCuts e.ws b.content).getD i 0) ∧
        okCut e b maxT f i = true ∧ i = bestIdx e b maxT f := by
  obtain ⟨pre, post, hfs, hpre, h0, ht⟩ := tryForms_some e b maxT (forms b) t f h
  have hs := bestIdx_spec e b maxT f h0
  exact ⟨⟨pre, post, hfs, hpre⟩, h0, _, hs.1, ht, hs.2, rfl⟩

/-! ### `fit_blocks` -/

/-- The two ways `fit_blocks` ends: the whole-block loop's result, or the
first block truncated after the loop rendered nothing. -/
theorem fitBlocks_cases (e : Env α) (bs : List (Block α)) (maxT : Nat) :
    fitBlocks e bs maxT = plainFit e bs maxT ∨
      ∃ b rest t f, bs = b :: rest ∧ fit (okOf e bs maxT) bs.length = 0 ∧
        truncFirst e b maxT = some (t, f) ∧ fitBlocks e bs maxT = ⟨t, 0, some f⟩ := by
  unfold fitBlocks
  by_cases h0 : fit (okOf e bs maxT) bs.length = 0
  · rw [ite_eq_left h0]
    match bs, h0 with
    | [], _ => exact Or.inl rfl
    | b :: rest, h0 =>
      simp only
      cases ht : truncFirst e b maxT with
      | none => exact Or.inl rfl
      | some r => exact Or.inr ⟨b, rest, r.1, r.2, rfl, h0, ht, rfl⟩
  · rw [ite_eq_right h0]
    exact Or.inl rfl

/-- `whole` is the whole-block loop's count, truncation or not: rule 2 never
changes which blocks are whole. -/
theorem fitBlocks_whole (e : Env α) (bs : List (Block α)) (maxT : Nat) :
    (fitBlocks e bs maxT).whole = fit (okOf e bs maxT) bs.length := by
  rcases fitBlocks_cases e bs maxT with h | ⟨b, rest, t, f, _, h0, _, h⟩
  · rw [h]; rfl
  · rw [h]; exact h0.symm

theorem okOf_one_false {e : Env α} {b : Block α} {rest : List (Block α)} {maxT : Nat}
    (h0 : fit (okOf e (b :: rest) maxT) (b :: rest).length = 0) :
    okOf e (b :: rest) maxT 1 = false := by
  cases hb : okOf e (b :: rest) maxT 1 with
  | false => rfl
  | true =>
    have h1 : fit (okOf e (b :: rest) maxT) (rest.length + 1) = fitFrom (okOf e (b :: rest) maxT) rest.length 1 :=
      fitFrom_hit (ok := okOf e (b :: rest) maxT) (whole := 0) hb rest.length
    have h2 := fitFrom_ge (okOf e (b :: rest) maxT) rest.length 1
    simp only [List.length_cons] at h0
    omega

/-- **Agreement with the whole-block model.** When the first block fits, the
result is exactly the pre-#130 loop's: no truncation, the same context, the
same `whole` (the test `whole_blocks_are_byte_identical_to_the_pre_130_loop`). -/
theorem fitBlocks_of_first_fits (e : Env α) (bs : List (Block α)) (maxT : Nat)
    (h : okOf e bs maxT 1 = true) : fitBlocks e bs maxT = plainFit e bs maxT := by
  rcases fitBlocks_cases e bs maxT with hp | ⟨b, rest, t, f, hbs, h0, _, _⟩
  · exact hp
  · subst hbs
    rw [okOf_one_false h0] at h
    exact absurd h (by decide)

/-- Truncation happens only when the loop rendered nothing (`mark_context`'s
`debug_assert`). -/
theorem fitBlocks_truncation_whole (e : Env α) (bs : List (Block α)) (maxT : Nat) {f : Form}
    (h : (fitBlocks e bs maxT).truncation = some f) : (fitBlocks e bs maxT).whole = 0 := by
  rcases fitBlocks_cases e bs maxT with hp | ⟨b, rest, t, g, _, _, _, hq⟩
  · rw [hp] at h; simp [plainFit] at h
  · rw [hq]

theorem foldl_appendBlock_ne_nil (sep : List α) :
    ∀ (ts : List (List α)) (acc : List α), acc ≠ [] → ts.foldl (appendBlock sep) acc ≠ [] := by
  intro ts
  induction ts with
  | nil => intro acc h; exact h
  | cons t ts ih =>
    intro acc h
    apply ih
    simp [appendBlock, h]

/-- **Within budget, for any estimator.** Whenever anything is rendered, whole
blocks or a truncated first block, the context fits `max_tokens` as measured by
the very estimator that cut it. No property of the estimator is assumed. -/
theorem fitBlocks_fits (e : Env α) (bs : List (Block α)) (maxT : Nat)
    (h : 0 < (fitBlocks e bs maxT).rendered) :
    fits (e.est (fitBlocks e bs maxT).context) maxT = true := by
  rcases fitBlocks_cases e bs maxT with hp | ⟨b, rest, t, f, _, _, ht, hq⟩
  · rw [hp] at h ⊢
    simp only [plainFit, Fitted.rendered, Option.isSome_none] at h ⊢
    have hok := fit_prefix_ok (okOf e bs maxT) bs.length _ (by simpa using h) (Nat.le_refl _)
    simpa [okOf] using hok
  · rw [hq]
    obtain ⟨_, _, i, _, hti, hoki, _⟩ := truncFirst_some ht
    rw [hti]
    simpa [okCut] using hoki

/-- When `fit` (`src/recall/format/fit.rs`) renders no block, the context is
empty. -/
theorem fitBlocks_empty (e : Env α) (bs : List (Block α)) (maxT : Nat)
    (h : (fitBlocks e bs maxT).rendered = 0) : (fitBlocks e bs maxT).context = [] := by
  rcases fitBlocks_cases e bs maxT with hp | ⟨b, rest, t, f, _, _, _, hq⟩
  · rw [hp] at h ⊢
    simp only [plainFit, Fitted.rendered, Option.isSome_none] at h ⊢
    have h0 : fit (okOf e bs maxT) bs.length = 0 := by simpa using h
    rw [h0]
    rfl
  · rw [hq] at h; simp [Fitted.rendered] at h

/-- **Never empty when the marker fits, for any estimator.** With at least one
block, if the marker alone (counting the whole first block as left out) fits,
something is rendered: whole blocks, or the first block truncated. -/
theorem fitBlocks_rendered_pos (e : Env α) (b : Block α) (rest : List (Block α)) (maxT : Nat)
    (hm : fits (e.est (b.mark (e.est b.text))) maxT = true) :
    0 < (fitBlocks e (b :: rest) maxT).rendered := by
  rcases fitBlocks_cases e (b :: rest) maxT with hp | ⟨b', rest', t, f, hbs, _, _, hq⟩
  · rw [hp]
    simp only [plainFit, Fitted.rendered, Option.isSome_none]
    by_cases h0 : fit (okOf e (b :: rest) maxT) (b :: rest).length = 0
    · exfalso
      have hsome := tryForms_isSome e b maxT (forms b) .contentOnly (contentOnly_mem_forms b)
        (by rw [okCut_zero, render_marker_alone]; exact hm)
      obtain ⟨r, hr⟩ := hsome
      have : fitBlocks e (b :: rest) maxT = ⟨r.1, 0, some r.2⟩ := by
        unfold fitBlocks
        rw [ite_eq_left h0]
        simp only
        rw [show truncFirst e b maxT = some (r.1, r.2) from hr]
      rw [this] at hp
      simp [plainFit] at hp
    · exact Nat.lt_of_lt_of_le (Nat.pos_of_ne_zero h0) (Nat.le_add_right _ _)
  · rw [hq]; simp [Fitted.rendered]

/-- **The context block is never empty when there is a hit and the marker
fits.** With a non-empty first block and a marker that is never empty (the
Rust marker never is), the context itself is non-empty. -/
theorem fitBlocks_context_ne_nil (e : Env α) (b : Block α) (rest : List (Block α))
    (maxT : Nat) (hm : fits (e.est (b.mark (e.est b.text))) maxT = true)
    (htext : b.text ≠ []) (hmark : ∀ n, b.mark n ≠ []) :
    (fitBlocks e (b :: rest) maxT).context ≠ [] := by
  have hpos := fitBlocks_rendered_pos e b rest maxT hm
  rcases fitBlocks_cases e (b :: rest) maxT with hp | ⟨b', rest', t, f, hbs, _, ht, hq⟩
  · rw [hp] at hpos ⊢
    simp only [plainFit, Fitted.rendered, Option.isSome_none] at hpos ⊢
    obtain ⟨k, hk⟩ : ∃ k, fit (okOf e (b :: rest) maxT) (b :: rest).length = k + 1 :=
      ⟨_, (Nat.succ_pred_eq_of_pos (by simpa using hpos)).symm⟩
    rw [hk]
    simp only [List.take_succ_cons, List.map_cons, ctxOf, List.foldl_cons]
    apply foldl_appendBlock_ne_nil
    simpa [appendBlock] using htext
  · rw [hq]
    simp only [List.cons.injEq] at hbs
    obtain ⟨rfl, rfl⟩ := hbs
    obtain ⟨_, _, i, _, hti, _, _⟩ := truncFirst_some ht
    rw [hti]
    exact render_ne_nil e b hmark f _

/-- **A truncated context is the first block's prefix plus the marker.** When
the first block is truncated: it did not fit whole, nothing else is rendered,
and the context is one candidate of one form at one cut from the cut list, so
(by `render_eq`, `text_split` and `mem_contentCuts`) a content head ending in
no whitespace, `…`, the kept part of the label and warnings, a newline, and a
marker counting exactly the text left out. -/
theorem fitBlocks_truncated (e : Env α) (bs : List (Block α)) (maxT : Nat) {f : Form}
    (h : (fitBlocks e bs maxT).truncation = some f) :
    ∃ b rest cut, bs = b :: rest ∧ okOf e bs maxT 1 = false ∧ f ∈ forms b ∧
      (fitBlocks e bs maxT).whole = 0 ∧ cut ∈ contentCuts e.ws b.content ∧
      (fitBlocks e bs maxT).context = render e b f cut := by
  rcases fitBlocks_cases e bs maxT with hp | ⟨b, rest, t, g, hbs, h0, ht, hq⟩
  · rw [hp] at h; simp [plainFit] at h
  · rw [hq] at h ⊢
    simp only [Option.some.injEq] at h
    subst h
    subst hbs
    obtain ⟨⟨pre, post, hfs, _⟩, _, i, hi, hti, _, _⟩ := truncFirst_some ht
    refine ⟨b, rest, _, rfl, okOf_one_false h0, by rw [hfs]; simp, rfl, getD_mem hi, hti⟩

/-- The same, spelled out: the truncated context is a prefix of the first
block's content, `…`, a prefix of what follows the content, a newline and the
marker; or the marker alone. -/
theorem fitBlocks_truncated_prefix (e : Env α) (bs : List (Block α)) (maxT : Nat) {f : Form}
    (h : (fitBlocks e bs maxT).truncation = some f) :
    ∃ b rest head kept leftOut, bs = b :: rest ∧ head <+: b.content ∧
      kept <+: b.label ++ b.warn ∧
      ((fitBlocks e bs maxT).context = head ++ e.ell ++ kept ++ e.nl ++ b.mark (e.est leftOut) ∨
        (f = .contentOnly ∧ (fitBlocks e bs maxT).context = b.mark (e.est b.text))) ∧
      b.text = head ++ (b.content.drop head.length) ++ (kept ++ dropRest b f) ∧
      leftOut = b.content.drop head.length ++ dropRest b f := by
  obtain ⟨b, rest, cut, hbs, _, _, _, hcut, hctx⟩ := fitBlocks_truncated e bs maxT h
  have hcl : cut ≤ b.content.length := by
    rcases mem_contentCuts hcut with h0 | h1
    · omega
    · omega
  have hlen : (b.content.take cut).length = cut := by simp; omega
  refine ⟨b, rest, b.content.take cut, keptRest b f, b.content.drop cut ++ dropRest b f, hbs,
    List.take_prefix _ _, ?_, ?_, ?_, by rw [hlen]⟩
  · rw [← keptRest_append_dropRest]; exact List.prefix_append _ _
  · by_cases hc : f = .contentOnly ∧ cut = 0
    · right
      refine ⟨hc.1, ?_⟩
      rw [hctx, hc.1, hc.2, render_marker_alone]
    · left
      rw [hctx, render_eq, ite_eq_right hc]
  · rw [hlen]; exact text_split b f cut

/-! ### When the search finds the longest head

The module doc said, until #302, that for an estimator monotone in bytes the
head is the longest that fits, "a count that loses a digit does so only after
losing at least one byte". The argument needs more than monotonicity: the count itself
may drop by at most one per byte removed, and the marker may shrink by at most
one byte per unit of count. `Steady` is that hypothesis; the default
estimator and the Rust marker satisfy it (`rust_default_steady`), and
`jump_skips_a_fitting_form` and `rust_jump_skips_a_fitting_form` show a
monotone estimator that does not. -/

/-- The size of a text under a per-element width (`Char::len_utf8` for
bytes). -/
def wsum (w : α → Nat) (s : List α) : Nat := (s.map w).sum

theorem wsum_append (w : α → Nat) (s t : List α) : wsum w (s ++ t) = wsum w s + wsum w t := by
  simp [wsum, List.sum_append]

theorem wsum_take_drop (w : α → Nat) (s : List α) (c : Nat) :
    wsum w (s.take c) + wsum w (s.drop c) = wsum w s := by
  rw [← wsum_append, List.take_append_drop]

theorem wsum_take_mono (w : α → Nat) (s : List α) {c c' : Nat} (h : c ≤ c') :
    wsum w (s.take c) ≤ wsum w (s.take c') := by
  have e : s.take c = (s.take c').take c := by rw [List.take_take, Nat.min_eq_left h]
  rw [e]
  have := wsum_take_drop w (s.take c') c
  omega

/-- The estimator is `G` of the size, `G` is monotone and rises by at most one
per unit of size, and the marker's size rises by at most one per unit of
count. -/
structure Steady (e : Env α) (b : Block α) (w : α → Nat) (G : Nat → Nat) : Prop where
  est_eq : ∀ s, e.est s = G (wsum w s)
  mono : ∀ x y, x ≤ y → G x ≤ G y
  step : ∀ x, G (x + 1) ≤ G x + 1
  mark : ∀ n n', n' ≤ n → wsum w (b.mark n) ≤ wsum w (b.mark n') + (n - n')

variable {e : Env α} {b : Block α} {w : α → Nat} {G : Nat → Nat}

theorem Steady.lip (h : Steady e b w G) : ∀ d x, G (x + d) ≤ G x + d := by
  intro d
  induction d with
  | zero => intro x; simp
  | succ d ih =>
    intro x
    have h1 := h.step (x + d)
    have h2 := ih x
    rw [← Nat.add_assoc]
    omega

/-- Under `Steady`, a longer head never gives a smaller candidate: the head
gains what the left-out text loses, and the marker loses at most that. -/
theorem wsum_nil (w : α → Nat) : wsum w [] = 0 := rfl

theorem render_wsum_mono (h : Steady e b w G) (f : Form) {c c' : Nat} (hcc : c ≤ c') :
    wsum w (render e b f c) ≤ wsum w (render e b f c') := by
  by_cases hc' : f = .contentOnly ∧ c' = 0
  · have hc0 : c = 0 := by omega
    subst hc0
    rw [hc'.2]
    exact Nat.le_refl _
  have hA := wsum_take_mono w b.content hcc
  have hT := wsum_take_drop w b.content c
  have hT' := wsum_take_drop w b.content c'
  have hn : e.est (b.content.drop c ++ dropRest b f) =
      G (wsum w (b.content.drop c) + wsum w (dropRest b f)) := by rw [h.est_eq, wsum_append]
  have hn' : e.est (b.content.drop c' ++ dropRest b f) =
      G (wsum w (b.content.drop c') + wsum w (dropRest b f)) := by rw [h.est_eq, wsum_append]
  have hmono : G (wsum w (b.content.drop c') + wsum w (dropRest b f)) ≤
      G (wsum w (b.content.drop c) + wsum w (dropRest b f)) := h.mono _ _ (by omega)
  have hlip : G (wsum w (b.content.drop c) + wsum w (dropRest b f)) ≤
      G (wsum w (b.content.drop c') + wsum w (dropRest b f)) +
        (wsum w (b.content.take c') - wsum w (b.content.take c)) := by
    have := h.lip (wsum w (b.content.take c') - wsum w (b.content.take c))
      (wsum w (b.content.drop c') + wsum w (dropRest b f))
    rw [show wsum w (b.content.drop c') + wsum w (dropRest b f) +
        (wsum w (b.content.take c') - wsum w (b.content.take c)) =
        wsum w (b.content.drop c) + wsum w (dropRest b f) by omega] at this
    exact this
  have hm := h.mark _ _ hmono
  rw [render_eq, render_eq, wsum_append, wsum_append, hn, hn', ite_eq_right hc']
  by_cases hc : f = .contentOnly ∧ c = 0
  · rw [ite_eq_left hc]
    have hA0 : wsum w (b.content.take c) = 0 := by rw [hc.2]; rfl
    simp only [wsum_append, wsum_nil]
    omega
  · rw [ite_eq_right hc]
    simp only [wsum_append]
    omega

/-- Under `Steady`, each form's fit test is downward closed over the cut
list: the search's precondition. -/
theorem okCut_down (h : Steady e b w G) (maxT : Nat) (f : Form) :
    DownBelow (okCut e b maxT f) (contentCuts e.ws b.content).length := by
  intro i hi hp
  unfold okCut at hp ⊢
  apply fits_anti _ hp
  rw [h.est_eq, h.est_eq]
  apply h.mono
  apply render_wsum_mono h f
  exact getD_mono (contentCuts_pairwise _ _) (Nat.le_succ i) hi

/-- **The longest head, and the first form that fits.** Under `Steady`, the
truncated block is the chosen form at the longest cut that fits, and no
earlier form fits at any cut (test `the_head_is_the_longest_that_fits`, and
"The first form that fits wins"). -/
theorem truncFirst_longest (h : Steady e b w G) {maxT : Nat} {t : List α} {f : Form}
    (ht : truncFirst e b maxT = some (t, f)) :
    ∃ cut, cut ∈ contentCuts e.ws b.content ∧ t = render e b f cut ∧
      (∀ c ∈ contentCuts e.ws b.content, fits (e.est (render e b f c)) maxT = true → c ≤ cut) ∧
      ∃ pre post, forms b = pre ++ f :: post ∧
        ∀ g ∈ pre, ∀ c ∈ contentCuts e.ws b.content, fits (e.est (render e b g c)) maxT = false := by
  obtain ⟨⟨pre, post, hfs, hpre⟩, h0, i, hi, hti, _, hidx⟩ := truncFirst_some ht
  have hn := contentCuts_length_pos e.ws b.content
  refine ⟨_, getD_mem hi, hti, ?_, pre, post, hfs, ?_⟩
  · intro c hc hfit
    obtain ⟨j, hj, hcj⟩ := List.mem_iff_getElem.mp hc
    have hokj : okCut e b maxT f j = true := by
      unfold okCut
      rw [getD_eq_getElem hj, hcj]
      exact hfit
    have hle : j ≤ i := by
      rw [hidx]
      exact bsearch_longest (okCut_down h maxT f) hn h0 hj hokj
    rw [← hcj, ← getD_eq_getElem hj]
    exact getD_mono (contentCuts_pairwise _ _) hle hi
  · intro g hg c hc
    cases hfit : fits (e.est (render e b g c)) maxT with
    | false => rfl
    | true =>
      exfalso
      obtain ⟨j, hj, hcj⟩ := List.mem_iff_getElem.mp hc
      have hokj : okCut e b maxT g j = true := by
        unfold okCut
        rw [getD_eq_getElem hj, hcj]
        exact hfit
      have := (okCut_down h maxT g).le j 0 (by omega) (by simpa using hokj)
      rw [hpre g hg] at this
      exact absurd this (by decide)

/-- Under `Steady`, every form's shortest candidate is at least as large as the
marker alone. -/
theorem marker_alone_le (h : Steady e b w G) (f : Form) :
    wsum w (b.mark (e.est b.text)) ≤ wsum w (render e b f 0) := by
  by_cases hc : f = .contentOnly
  · subst hc; rw [render_marker_alone]; exact Nat.le_refl _
  · have hne : ¬ (f = .contentOnly ∧ (0 : Nat) = 0) := fun h' => hc h'.1
    have htext : e.est b.text =
        G (wsum w b.content + wsum w (keptRest b f) + wsum w (dropRest b f)) := by
      rw [h.est_eq, Block.text, List.append_assoc, ← keptRest_append_dropRest b f]
      simp only [wsum_append]
      congr 1
      omega
    have hcnt : e.est (b.content.drop 0 ++ dropRest b f) =
        G (wsum w b.content + wsum w (dropRest b f)) := by
      rw [h.est_eq, List.drop_zero, wsum_append]
    have hmono : G (wsum w b.content + wsum w (dropRest b f)) ≤
        G (wsum w b.content + wsum w (keptRest b f) + wsum w (dropRest b f)) :=
      h.mono _ _ (by omega)
    have hlip := h.lip (wsum w (keptRest b f)) (wsum w b.content + wsum w (dropRest b f))
    rw [show wsum w b.content + wsum w (dropRest b f) + wsum w (keptRest b f) =
      wsum w b.content + wsum w (keptRest b f) + wsum w (dropRest b f) by omega] at hlip
    have hm := h.mark _ _ hmono
    rw [render_eq, ite_eq_right hne, hcnt, htext]
    simp only [wsum_append]
    omega

/-- **Rule 3, under `Steady`.** When the first block does not fit whole,
nothing is rendered exactly when the marker alone does not fit. The `→`
direction holds for any estimator (`fitBlocks_rendered_pos`); the `←`
direction needs a condition: `Steady` here, or monotone in bytes with the
Rust's sizes (`rust_rule3_of_mono`); with toy sizes a monotone estimator can
break it (`jump_renders_without_the_marker`). -/
theorem fitBlocks_rendered_zero_iff (h : Steady e b w G) (rest : List (Block α)) (maxT : Nat)
    (h1 : okOf e (b :: rest) maxT 1 = false) :
    (fitBlocks e (b :: rest) maxT).rendered = 0 ↔
      fits (e.est (b.mark (e.est b.text))) maxT = false := by
  constructor
  · intro hz
    cases hm : fits (e.est (b.mark (e.est b.text))) maxT with
    | false => rfl
    | true =>
      have := fitBlocks_rendered_pos e b rest maxT hm
      omega
  · intro hm
    have hnone : truncFirst e b maxT = none := by
      apply tryForms_none
      intro f _
      rw [okCut_zero]
      cases hf : fits (e.est (render e b f 0)) maxT with
      | false => rfl
      | true =>
        have : fits (e.est (b.mark (e.est b.text))) maxT = true := by
          apply fits_anti _ hf
          rw [h.est_eq, h.est_eq (render e b f 0)]
          exact h.mono _ _ (marker_alone_le h f)
        rw [hm] at this
        exact absurd this (by decide)
    have h0 : fit (okOf e (b :: rest) maxT) (b :: rest).length = 0 := by
      have : ¬ okOf e (b :: rest) maxT (0 + 1) = true := by rw [Nat.zero_add, h1]; decide
      exact fitFrom_miss this rest.length
    have : fitBlocks e (b :: rest) maxT = plainFit e (b :: rest) maxT := by
      unfold fitBlocks
      rw [ite_eq_left h0]
      simp only [hnone]
    rw [this]
    simp only [List.length_cons] at h0
    simp [plainFit, Fitted.rendered, h0]

/-- Rule 3 from one comparison: when the marker alone is estimated no higher
than any form's cut-`0` candidate, then with a first block that does not fit
whole, nothing is rendered exactly when the marker alone does not fit.
`Steady` gives the comparison (`marker_alone_le`); so does any estimator
monotone in bytes on blocks of the Rust's sizes (`rust_rule3_of_mono`). -/
theorem fitBlocks_rendered_zero_iff_of_le (rest : List (Block α)) (maxT : Nat)
    (hle : ∀ f, e.est (b.mark (e.est b.text)) ≤ e.est (render e b f 0))
    (h1 : okOf e (b :: rest) maxT 1 = false) :
    (fitBlocks e (b :: rest) maxT).rendered = 0 ↔
      fits (e.est (b.mark (e.est b.text))) maxT = false := by
  constructor
  · intro hz
    cases hm : fits (e.est (b.mark (e.est b.text))) maxT with
    | false => rfl
    | true =>
      have := fitBlocks_rendered_pos e b rest maxT hm
      omega
  · intro hm
    have hnone : truncFirst e b maxT = none := by
      apply tryForms_none
      intro f _
      rw [okCut_zero]
      cases hf : fits (e.est (render e b f 0)) maxT with
      | false => rfl
      | true =>
        have := fits_anti (hle f) hf
        rw [hm] at this
        exact absurd this (by decide)
    have h0 : fit (okOf e (b :: rest) maxT) (b :: rest).length = 0 := by
      have : ¬ okOf e (b :: rest) maxT (0 + 1) = true := by rw [Nat.zero_add, h1]; decide
      exact fitFrom_miss this rest.length
    have : fitBlocks e (b :: rest) maxT = plainFit e (b :: rest) maxT := by
      unfold fitBlocks
      rw [ite_eq_left h0]
      simp only [hnone]
    rw [this]
    simp only [List.length_cons] at h0
    simp [plainFit, Fitted.rendered, h0]

/-! ### The Rust instance: the default estimator and the real marker -/

/-- `default_token_count` on a byte count: `ceil(bytes / 3.5) = ceil(2 bytes /
7)`. The Rust's `f64` computation agrees for every length below `2^52` (the
argument is in the header).
Pinned by `formal_constants_tests::fit`. -/
def defaultG (bytes : Nat) : Nat := (2 * bytes + 6) / 7

/-- `default_token_count` on chars, measuring UTF-8 bytes. -/
def defaultTokenCount (s : List Char) : Nat := defaultG (wsum Char.utf8Size s)

/-- `char::is_whitespace`: the Unicode `White_Space` property. -/
def rustWs (c : Char) : Bool :=
  let v := c.toNat
  (9 ≤ v && v ≤ 13) || v == 32 || v == 0x85 || v == 0xA0 || v == 0x1680 ||
    (0x2000 ≤ v && v ≤ 0x200A) || v == 0x2028 || v == 0x2029 || v == 0x202F ||
    v == 0x205F || v == 0x3000

/-- The Rust constants: `SEPARATOR`, `ELLIPSIS`, the newline before the
marker.
Pinned by `formal_constants_tests::fit`. -/
def rustEnv (est : List Char → Nat) : Env Char :=
  ⟨est, ['\n', '\n'], ['…'], ['\n'], rustWs⟩

/-- `truncation_marker(n, inspect_id, scope)`, both forms.
Pinned by `formal_constants_tests::fit`. -/
def rustMarker (inspectId : List Char) (scope : Option (List Char)) (n : Nat) : List Char :=
  "[truncated: ".toList ++ Nat.toDigits 10 n ++ " tokens left out; lambo_inspect ".toList ++
    inspectId ++
    (match scope with
     | none => []
     | some s => " (scope ".toList ++ s ++ [')']) ++
    " for the full concept]".toList

/-- A `ContextBlock` with the real marker. -/
def rustBlock (content label warn inspectId : List Char) (scope : Option (List Char)) :
    Block Char :=
  ⟨content, label, warn, rustMarker inspectId scope⟩

theorem utf8Size_of_isDigit {c : Char} (h : c.isDigit = true) : c.utf8Size = 1 := by
  simp only [Char.isDigit, Bool.and_eq_true, decide_eq_true_eq] at h
  have h2 : c.val ≤ 127 := UInt32.le_trans h.2 (by decide)
  simp [Char.utf8Size, h2]

theorem wsum_toDigits (n : Nat) : wsum Char.utf8Size (Nat.toDigits 10 n) = (Nat.toDigits 10 n).length := by
  have : ∀ (l : List Char), (∀ c ∈ l, c.isDigit = true) → wsum Char.utf8Size l = l.length := by
    intro l
    induction l with
    | nil => intro _; rfl
    | cons c l ih =>
      intro hl
      have h1 := utf8Size_of_isDigit (hl c (by simp))
      have h2 := ih (fun d hd => hl d (List.mem_cons_of_mem c hd))
      simp only [wsum, List.map_cons, List.sum_cons, List.length_cons] at h2 ⊢
      omega
  exact this _ (fun c hc => Nat.isDigit_of_mem_toDigits (by decide) (by decide) hc)

/-- A decimal count gains at most one digit per unit. -/
theorem toDigits_length_lip (n : Nat) : ∀ n', n' ≤ n →
    (Nat.toDigits 10 n).length ≤ (Nat.toDigits 10 n').length + (n - n') := by
  induction n using Nat.strongRecOn with
  | ind n ih =>
    intro n' hle
    have hpos : ∀ m, 0 < (Nat.toDigits 10 m).length := fun _ => Nat.length_toDigits_pos
    rw [Nat.toDigits_eq_ite (by decide : 1 < 10) (n := n)]
    by_cases hn : n < 10
    · rw [ite_eq_left hn]; have := hpos n'; simp; omega
    · rw [ite_eq_right hn, List.length_append]
      simp only [List.length_singleton]
      by_cases hn' : n' < 10
      · have h1 := ih (n / 10) (by omega) 1 (by omega)
        have h1' : (Nat.toDigits 10 1).length = 1 := by decide
        have := hpos n'
        omega
      · rw [Nat.toDigits_eq_ite (by decide : 1 < 10) (n := n'), ite_eq_right hn', List.length_append]
        simp only [List.length_singleton]
        have h1 := ih (n / 10) (by omega) (n' / 10) (by omega)
        omega

theorem wsum_rustMarker (inspectId : List Char) (scope : Option (List Char)) (n : Nat) :
    wsum Char.utf8Size (rustMarker inspectId scope n) =
      wsum Char.utf8Size (rustMarker inspectId scope 0) - 1 + (Nat.toDigits 10 n).length := by
  have h0 : (Nat.toDigits 10 0).length = 1 := by decide
  simp only [rustMarker, wsum_append, wsum_toDigits, h0]
  omega

/-- **The default estimator with the real marker is `Steady`**, so for the
Rust as configured by default the head is the longest that fits, the first
form with any fitting cut wins, and rule 3 holds exactly
(`truncFirst_longest`, `fitBlocks_rendered_zero_iff`). -/
theorem rust_default_steady (content label warn inspectId : List Char)
    (scope : Option (List Char)) :
    Steady (rustEnv defaultTokenCount) (rustBlock content label warn inspectId scope)
      Char.utf8Size defaultG where
  est_eq _ := rfl
  mono x y h := by unfold defaultG; omega
  step x := by unfold defaultG; omega
  mark n n' h := by
    simp only [rustBlock]
    rw [wsum_rustMarker, wsum_rustMarker inspectId scope n']
    have := toDigits_length_lip n n' h
    omega

/-- The byte estimator `|s| s.len()` the Rust tests use is `Steady`, so the
fit theorems cover it. -/
theorem rust_bytes_steady (content label warn inspectId : List Char)
    (scope : Option (List Char)) :
    Steady (rustEnv (wsum Char.utf8Size)) (rustBlock content label warn inspectId scope)
      Char.utf8Size id where
  est_eq _ := rfl
  mono _ _ h := h
  step _ := Nat.le_refl _
  mark n n' h := by
    simp only [rustBlock]
    rw [wsum_rustMarker, wsum_rustMarker inspectId scope n']
    have := toDigits_length_lip n n' h
    omega

/-- **`truncFirst_longest` for the Rust as configured by default**: the head
is the longest that fits in its form, and no earlier form fits at any cut. -/
theorem rust_default_longest (content label warn inspectId : List Char)
    (scope : Option (List Char)) {maxT : Nat} {t : List Char} {f : Form}
    (ht : truncFirst (rustEnv defaultTokenCount) (rustBlock content label warn inspectId scope)
      maxT = some (t, f)) :
    ∃ cut, cut ∈ contentCuts rustWs content ∧
      t = render (rustEnv defaultTokenCount) (rustBlock content label warn inspectId scope) f cut ∧
      (∀ c ∈ contentCuts rustWs content,
        fits (defaultTokenCount (render (rustEnv defaultTokenCount)
          (rustBlock content label warn inspectId scope) f c)) maxT = true → c ≤ cut) ∧
      ∃ pre post, forms (rustBlock content label warn inspectId scope) = pre ++ f :: post ∧
        ∀ g ∈ pre, ∀ c ∈ contentCuts rustWs content,
          fits (defaultTokenCount (render (rustEnv defaultTokenCount)
            (rustBlock content label warn inspectId scope) g c)) maxT = false :=
  truncFirst_longest (rust_default_steady content label warn inspectId scope) ht

theorem toDigits_length_le_20 {n : Nat} (h : n ≤ usizeMax) : (Nat.toDigits 10 n).length ≤ 20 :=
  (Nat.length_toDigits_le_iff (by decide) (by decide)).mpr
    (Nat.lt_of_le_of_lt h (by decide))

theorem wsum_label_le_keptRest (b : Block α) (w : α → Nat) {f : Form}
    (hf : f ≠ .contentOnly) : wsum w b.label ≤ wsum w (keptRest b f) := by
  cases f with
  | withWarnings => simp only [keptRest, wsum_append]; omega
  | labelOnly => exact Nat.le_refl _
  | contentOnly => exact absurd rfl hf

/-- Two real markers differ by at most 19 bytes: a `usize` count has 1 to 20
digits. -/
theorem rust_mark_close (inspectId : List Char) (scope : Option (List Char)) {n : Nat}
    (hn : n ≤ usizeMax) (n' : Nat) :
    wsum Char.utf8Size (rustMarker inspectId scope n) ≤
      wsum Char.utf8Size (rustMarker inspectId scope n') + 19 := by
  have h1 := toDigits_length_le_20 hn
  have h2 : 0 < (Nat.toDigits 10 n').length := Nat.length_toDigits_pos
  have e1 := wsum_rustMarker inspectId scope n
  have e2 := wsum_rustMarker inspectId scope n'
  omega

/-- **Rule 3 for any estimator monotone in bytes, on blocks of the Rust's
sizes.** If the estimator is `G` of the UTF-8 byte count with `G` monotone
(no bound on how fast it rises), every count is a `usize` (at most 20 decimal
digits), and the label rest is at least 16 bytes (`for_hit` always renders
`" [<Type>] (score <s>, id <8 hex>)"`, at least 24), then when the first block
does not fit whole, nothing is rendered exactly when the marker alone does not
fit. The marker alone is at most 19 bytes longer than any other marker, and
every other cut-`0` candidate adds `…` (3 bytes), the label and a newline. -/
theorem rust_rule3_of_mono (G : Nat → Nat) (hmono : ∀ x y, x ≤ y → G x ≤ G y)
    (hbound : ∀ x, G x ≤ usizeMax) (content label warn inspectId : List Char)
    (scope : Option (List Char)) (hlabel : 16 ≤ wsum Char.utf8Size label)
    (rest : List (Block Char)) (maxT : Nat)
    (h1 : okOf (rustEnv (fun s => G (wsum Char.utf8Size s)))
      (rustBlock content label warn inspectId scope :: rest) maxT 1 = false) :
    (fitBlocks (rustEnv (fun s => G (wsum Char.utf8Size s)))
        (rustBlock content label warn inspectId scope :: rest) maxT).rendered = 0 ↔
      fits ((rustEnv (fun s => G (wsum Char.utf8Size s))).est
        ((rustBlock content label warn inspectId scope).mark
          ((rustEnv (fun s => G (wsum Char.utf8Size s))).est
            (rustBlock content label warn inspectId scope).text))) maxT = false := by
  apply fitBlocks_rendered_zero_iff_of_le rest maxT _ h1
  intro f
  by_cases hc : f = .contentOnly
  · subst hc; rw [render_marker_alone]; exact Nat.le_refl _
  · apply hmono
    have hne : ¬ (f = .contentOnly ∧ (0 : Nat) = 0) := fun h' => hc h'.1
    have hk := wsum_label_le_keptRest (rustBlock content label warn inspectId scope)
      Char.utf8Size hc
    have hkey := rust_mark_close inspectId scope
      (hbound (wsum Char.utf8Size (rustBlock content label warn inspectId scope).text))
      (G (wsum Char.utf8Size (content ++ dropRest (rustBlock content label warn inspectId scope) f)))
    have hell : wsum Char.utf8Size ['…'] = 3 := by decide
    rw [render_eq, ite_eq_right hne]
    simp only [List.take_zero, List.drop_zero, wsum_append, wsum_nil]
    show wsum Char.utf8Size (rustMarker inspectId scope
        (G (wsum Char.utf8Size (rustBlock content label warn inspectId scope).text))) ≤
      0 + wsum Char.utf8Size ['…'] +
        wsum Char.utf8Size (keptRest (rustBlock content label warn inspectId scope) f) +
        wsum Char.utf8Size ['\n'] +
        wsum Char.utf8Size (rustMarker inspectId scope (G (wsum Char.utf8Size
          (content ++ dropRest (rustBlock content label warn inspectId scope) f))))
    have hk' : wsum Char.utf8Size (rustBlock content label warn inspectId scope).label =
        wsum Char.utf8Size label := rfl
    rw [hell]
    omega

/-! ### Examples on the Rust instance

The default estimator, the real marker and a label as `render_block` writes
it, checked by evaluation. -/

/-- A label line as `render_block` renders it for a non-canonical Entity. -/
def exLabel : List Char := " [Entity] (score 0.50, id 1a2b3c4d)".toList

/-- A 95-byte block: 28 tokens. -/
def exSmall : Block Char := rustBlock (List.replicate 60 'y') exLabel [] "1a2b3c4d".toList none

/-- A 435-byte block: 125 tokens. -/
def exBig : Block Char := rustBlock (List.replicate 400 'y') exLabel [] "1a2b3c4d".toList none

/-- A canonical block with a long warning line. -/
def exCanon : Block Char :=
  rustBlock (List.replicate 300 'c')
    " [Entity, canonical] (score 0.50, id 1a2b3c4d, blast radius 3)".toList
    ('\n' :: List.replicate 300 'w') "1a2b3c4d".toList none

/-- `fitBlocks` with the default estimator. -/
def exFit (bs : List (Block Char)) (maxT : Nat) : Fitted Char :=
  fitBlocks (rustEnv defaultTokenCount) bs maxT

/-- A first block that fits renders whole, with no marker
(`first_block_that_fits_exactly_is_whole_with_no_marker`). -/
example : exFit [exSmall] 28 = ⟨exSmall.text, 1, none⟩ := by decide +kernel

/-- One token over: the content is cut, the label kept, the marker names the
id, and the second block is not rendered
(`first_block_one_token_over_keeps_its_label_and_names_its_id`). -/
example : (exFit [exBig, exSmall] 124).truncation = some .withWarnings ∧
    (exFit [exBig, exSmall] 124).context =
      List.replicate 319 'y' ++ ['…'] ++ exLabel ++ ['\n'] ++
        "[truncated: 24 tokens left out; lambo_inspect 1a2b3c4d for the full concept]".toList ∧
    defaultTokenCount (exFit [exBig, exSmall] 124).context = 124 := by decide +kernel

/-- The head is the longest that fits (`truncFirst_longest`, here through
`rust_default_longest`): the 319-char head of the example above fits at 124,
and the next cut, 320, does not. -/
example : 320 ∈ contentCuts rustWs exBig.content ∧
    fits (defaultTokenCount
      (render (rustEnv defaultTokenCount) exBig .withWarnings 319)) 124 = true ∧
    fits (defaultTokenCount
      (render (rustEnv defaultTokenCount) exBig .withWarnings 320)) 124 = false := by
  decide +kernel

/-- Long warnings fall back to the label line, then to the content head
(`long_warnings_fall_back_to_the_label_line_then_to_the_content`). -/
example : (exFit [exCanon] 120).truncation = some .labelOnly ∧
    (exFit [exCanon] 40).truncation = some .contentOnly := by decide +kernel

/-- The marker alone costs 22 tokens here: a budget of 21 renders nothing, 22
renders the marker alone (`budget_below_the_marker_gives_an_empty_context`). -/
example : exFit [exSmall] 21 = ⟨[], 0, none⟩ ∧
    exFit [exSmall] 22 =
      ⟨"[truncated: 28 tokens left out; lambo_inspect 1a2b3c4d for the full concept]".toList,
        0, some .contentOnly⟩ := by decide +kernel

/-- A saturated estimator never fits (`saturated_estimator_never_fits`). -/
example : fitBlocks (rustEnv (fun _ => usizeMax)) [exSmall] usizeMax = ⟨[], 0, none⟩ := by
  decide +kernel

/-- A second block that does not fit is dropped whole, and nothing after it
(`second_block_that_does_not_fit_is_dropped_whole_and_nothing_after_it`). -/
example : exFit [exSmall, exBig, exSmall] 60 = ⟨exSmall.text, 1, none⟩ := by decide +kernel

/-! ### Where the guarantee depends on the estimator

Two estimators, each a strictly increasing function of the text's length (so
"monotone in bytes"), that are not `Steady`: the count jumps by far more than
one per byte. Block elements are `Nat`s, `0` is whitespace, the marker is `P`
fixed units then one unit per decimal digit. -/

/-- A marker of `P` fixed units and one per digit of the count. -/
def jMark (P : Nat) (n : Nat) : List Nat :=
  List.replicate P 9 ++ (Nat.toDigits 10 n).map (fun _ => 8)

/-- Constants of length 1 or 2, as `…`, `"\n"` and `"\n\n"` are in chars. -/
def jEnv (est : List Nat → Nat) : Env Nat := ⟨est, [10, 10], [1], [10], fun x => x == 0⟩

/-- Length up to `J`, then `10^k` more. -/
def jumpEst (J k : Nat) (s : List Nat) : Nat := if s.length ≤ J then s.length else 10 ^ k + s.length

/-- Ten content units, a one-unit label, no warnings. -/
def jBlockA : Block Nat := ⟨List.replicate 10 5, [3], [], jMark 1⟩

/-- **The cut-0 gate can skip a form that fits, and the search can miss the
longest head.** With `jumpEst 5 6` the `WithWarnings` candidate at cut `5`
fits, and so does `ContentOnly` at cut `6`, but `WithWarnings` fails at cut
`0` and is skipped, and the search over `ContentOnly` stops at cut `0`: the
context is the marker alone. Contradicts, for this estimator, the module
doc's "The first form that fits wins" and "For an estimator that is monotone
in bytes ... the head is also the longest such head that fits". -/
theorem jump_skips_a_fitting_form :
    (fitBlocks (jEnv (jumpEst 5 6)) [jBlockA] (10 ^ 6 + 10)).truncation = some .contentOnly ∧
    (fitBlocks (jEnv (jumpEst 5 6)) [jBlockA] (10 ^ 6 + 10)).context =
      render (jEnv (jumpEst 5 6)) jBlockA .contentOnly 0 ∧
    fits (jumpEst 5 6 (render (jEnv (jumpEst 5 6)) jBlockA .withWarnings 5)) (10 ^ 6 + 10) = true ∧
    fits (jumpEst 5 6 (render (jEnv (jumpEst 5 6)) jBlockA .contentOnly 6)) (10 ^ 6 + 10) = true ∧
    5 ∈ contentCuts (jEnv (jumpEst 5 6)).ws jBlockA.content ∧
    6 ∈ contentCuts (jEnv (jumpEst 5 6)).ws jBlockA.content := by
  decide

/-- The same on the Rust instance, byte for byte: an estimator that is the
byte count up to 90 bytes and `10^19` more above. -/
def rjEst (s : List Char) : Nat :=
  if wsum Char.utf8Size s ≤ 90 then wsum Char.utf8Size s else 10 ^ 19 + wsum Char.utf8Size s

/-- The counterexample estimator for `fit.rs` (the byte count up to 90 bytes,
`10^19` more above) is monotone in the byte count: monotonicity alone does not
make the cut safe. -/
theorem rjEst_mono {s t : List Char} (h : wsum Char.utf8Size s ≤ wsum Char.utf8Size t) :
    rjEst s ≤ rjEst t := by
  unfold rjEst
  by_cases hs : wsum Char.utf8Size s ≤ 90
  · rw [ite_eq_left hs]
    by_cases ht : wsum Char.utf8Size t ≤ 90
    · rw [ite_eq_left ht]; exact h
    · rw [ite_eq_right ht]; exact Nat.le_trans h (Nat.le_add_left _ _)
  · rw [ite_eq_right hs]
    have ht : ¬ wsum Char.utf8Size t ≤ 90 := by omega
    rw [ite_eq_right ht]
    exact Nat.add_le_add_left h _

/-- 91 content bytes and a real label: 126 bytes. -/
def rjBlock : Block Char := rustBlock (List.replicate 91 'y') exLabel [] "1a2b3c4d".toList none

/-- **Finding (1), byte-faithful.** With `rjEst` and `max_tokens = 10^19 +
124` the whole block does not fit. `WithWarnings` at cut `0` (133 bytes, its
count has 20 digits) does not fit, but at cuts 1 to 9 (116 to 124 bytes) it
does; the Rust skips the form and returns `ContentOnly` with a 26-byte head,
although `ContentOnly` heads of 36 to 44 bytes fit too. Breaks "The first form
that fits wins" (`fit.rs:26`) and "the head is also the longest such head that
fits" (`fit.rs:53-55`) for an estimator monotone in bytes. -/
theorem rust_jump_skips_a_fitting_form :
    okOf (rustEnv rjEst) [rjBlock] (10 ^ 19 + 124) 1 = false ∧
    fits (rjEst (render (rustEnv rjEst) rjBlock .withWarnings 0)) (10 ^ 19 + 124) = false ∧
    fits (rjEst (render (rustEnv rjEst) rjBlock .withWarnings 1)) (10 ^ 19 + 124) = true ∧
    fits (rjEst (render (rustEnv rjEst) rjBlock .withWarnings 9)) (10 ^ 19 + 124) = true ∧
    fitBlocks (rustEnv rjEst) [rjBlock] (10 ^ 19 + 124) =
      ⟨render (rustEnv rjEst) rjBlock .contentOnly 26, 0, some .contentOnly⟩ ∧
    fits (rjEst (render (rustEnv rjEst) rjBlock .contentOnly 44)) (10 ^ 19 + 124) = true ∧
    44 ∈ contentCuts rustWs rjBlock.content := by
  decide +kernel

/-- Thirty content units, a one-unit label, no warnings. -/
def jBlockB : Block Nat := ⟨List.replicate 30 5, [3], [], jMark 2⟩

/-- **Rule 3 needs the label to outweigh the marker's digits.** With toy
sizes (a one-unit label and `…`, a two-unit marker prefix) and `jumpEst 30 9`,
the marker alone does not fit a budget of 7, yet the context is not empty:
`WithWarnings` fits at cut `0` because its marker counts only the content,
priced at 30, while the marker alone counts the whole block, priced at `10^9`.
Real blocks never have these sizes: on them rule 3 holds for every estimator
monotone in bytes (`rust_rule3_of_mono`), so it can fail there only for an
estimator that is not. -/
theorem jump_renders_without_the_marker :
    fits (jumpEst 30 9 (jBlockB.mark (jumpEst 30 9 jBlockB.text))) 7 = false ∧
    (fitBlocks (jEnv (jumpEst 30 9)) [jBlockB] 7).truncation = some .withWarnings ∧
    (fitBlocks (jEnv (jumpEst 30 9)) [jBlockB] 7).context = [1, 3, 10, 9, 9, 8, 8] := by
  decide

end Truncation

end Lambo.Fit
