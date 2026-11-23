import LamboProofs.Canon
import LamboProofs.Lib.List

/-
# The session counter (`separated_session_count`, `src/canon/event_time.rs`)

Models the greedy count of `gap`-separated session starts the Solo policy's
recurrence term reads (`sessionCount`, with `SESSION_SEPARATION` as
`sessionSeparation`), over instants as integers (seconds in the examples).

Results: the count depends only on which instants occur, so it is order-free
and ignores exact repeats (`sessionCount_congr`, `sessionCount_perm`,
`sessionCount_dup`); it is at least 1 on non-empty input and at most the
number of distinct starts; **it is the maximum number of pairwise
`gap`-separated starts**, exact rather than a lower bound
(`sessionCount_max`, `sessionCount_attained`); adding a start raises it by
at most one (`sessionCount_mono`, `sessionCount_cons_le`) and a wider gap
never counts more (`sessionCount_antitone_gap`); starts within `gap` of the
earliest count as one (`sessionCount_window`) while a chain of starts 23 h
apart counts three (`chain_counts_more_than_one`); and the Rust test
`separation_counts_clusters_once_and_stragglers_each`.

Boundary: `chrono::Duration::MAX` saturation of `gap` is not modelled (no two
instants are that far apart); the count's callers are `Support.lean`.
-/

namespace Lambo.Policy

open Lambo.Lib (nodup_eraseDups length_eraseDups_le_cons)

/-! ## Sort ascending, deduplicate -/

/-- Insert `x` into a strictly increasing list, dropping it if already present. -/
def ins (x : Int) : List Int → List Int
  | [] => [x]
  | y :: ys => if x < y then x :: y :: ys else if x = y then y :: ys else y :: ins x ys

/-- `sorted.sort(); sorted.dedup();`: the strictly increasing list of the
distinct values of `l`. -/
def canon (l : List Int) : List Int := l.foldr ins []

/-- Strictly increasing. -/
abbrev Strict (l : List Int) : Prop := l.Pairwise (· < ·)

theorem mem_ins (x z : Int) : ∀ l : List Int, z ∈ ins x l ↔ z = x ∨ z ∈ l
  | [] => by simp [ins]
  | y :: ys => by
    unfold ins
    by_cases h1 : x < y
    · rw [ite_eq_left h1]; simp
    · by_cases h2 : x = y
      · rw [ite_eq_right h1, ite_eq_left h2]; subst h2; simp
      · have ih := mem_ins x z ys
        rw [ite_eq_right h1, ite_eq_right h2]
        simp only [List.mem_cons, ih]
        constructor
        · intro h; rcases h with h | h | h <;> simp [h]
        · intro h; rcases h with h | h | h <;> simp [h]

theorem strict_ins (x : Int) : ∀ l : List Int, Strict l → Strict (ins x l)
  | [] => fun _ => by simp [ins]
  | y :: ys => by
    intro h
    rw [Strict, List.pairwise_cons] at h
    unfold ins
    by_cases h1 : x < y
    · rw [ite_eq_left h1]
      simp only [Strict, List.pairwise_cons]
      refine ⟨?_, h.1, h.2⟩
      intro a ha
      rcases List.mem_cons.mp ha with ha | ha
      · omega
      · have := h.1 a ha; omega
    · by_cases h2 : x = y
      · rw [ite_eq_right h1, ite_eq_left h2, Strict, List.pairwise_cons]
        exact h
      · rw [ite_eq_right h1, ite_eq_right h2, Strict, List.pairwise_cons]
        refine ⟨?_, strict_ins x ys h.2⟩
        intro a ha
        rcases (mem_ins x a ys).mp ha with ha | ha
        · omega
        · exact h.1 a ha

theorem mem_canon (z : Int) : ∀ l : List Int, z ∈ canon l ↔ z ∈ l
  | [] => by simp [canon]
  | x :: xs => by
    have ih := mem_canon z xs
    simp only [canon, List.foldr_cons] at ih ⊢
    rw [mem_ins, ih, List.mem_cons]

theorem strict_canon : ∀ l : List Int, Strict (canon l)
  | [] => by simp [canon, Strict]
  | x :: xs => by
    simp only [canon, List.foldr_cons]
    exact strict_ins x _ (strict_canon xs)

/-- Two strictly increasing lists with the same members are equal. -/
theorem strict_unique : ∀ (a b : List Int), Strict a → Strict b →
    (∀ z, z ∈ a ↔ z ∈ b) → a = b
  | [], [], _, _, _ => rfl
  | [], y :: _, _, _, h => by have := (h y).mpr (by simp); simp at this
  | x :: _, [], _, _, h => by have := (h x).mp (by simp); simp at this
  | x :: xs, y :: ys, ha, hb, h => by
    rw [Strict, List.pairwise_cons] at ha hb
    have hxy : x = y := by
      have h1 := (h x).mp (by simp)
      have h2 := (h y).mpr (by simp)
      rcases List.mem_cons.mp h1 with h1 | h1
      · exact h1
      rcases List.mem_cons.mp h2 with h2 | h2
      · exact h2.symm
      have := hb.1 x h1
      have := ha.1 y h2
      omega
    subst hxy
    have rest : xs = ys := by
      apply strict_unique xs ys ha.2 hb.2
      intro z
      constructor
      · intro hz
        have hlt := ha.1 z hz
        rcases List.mem_cons.mp ((h z).mp (List.mem_cons_of_mem _ hz)) with e | e
        · omega
        · exact e
      · intro hz
        have hlt := hb.1 z hz
        rcases List.mem_cons.mp ((h z).mpr (List.mem_cons_of_mem _ hz)) with e | e
        · omega
        · exact e
    rw [rest]

theorem length_ins (x : Int) : ∀ l : List Int, (ins x l).length ≤ l.length + 1
  | [] => by simp [ins]
  | y :: ys => by
    unfold ins
    by_cases h1 : x < y
    · rw [ite_eq_left h1]; simp
    · by_cases h2 : x = y
      · rw [ite_eq_right h1, ite_eq_left h2]; simp
      · rw [ite_eq_right h1, ite_eq_right h2]
        have := length_ins x ys
        simp only [List.length_cons]
        omega

/-- At most as many distinct starts as starts. -/
theorem length_canon_le : ∀ l : List Int, (canon l).length ≤ l.length
  | [] => by simp [canon]
  | x :: xs => by
    have ih := length_canon_le xs
    have := length_ins x (canon xs)
    simp only [canon, List.foldr_cons, List.length_cons] at ih this ⊢
    omega

/-- `canon` is whatever "sort ascending, then dedup" produces: any strictly
increasing list with the members of `l` is `canon l`. -/
theorem canon_eq_of_strict (l L : List Int) (hs : Strict L) (hm : ∀ z, z ∈ L ↔ z ∈ l) :
    canon l = L :=
  strict_unique _ _ (strict_canon l) hs (fun z => (mem_canon z l).trans (hm z).symm)

theorem canon_congr (l l' : List Int) (h : ∀ z, z ∈ l ↔ z ∈ l') : canon l = canon l' :=
  canon_eq_of_strict l (canon l') (strict_canon l')
    (fun z => (mem_canon z l').trans (h z).symm)

/-- A strictly increasing list whose members all occur in a strictly
increasing `L` is a subsequence of `L`. -/
theorem sublist_of_strict : ∀ (S L : List Int), Strict S → Strict L →
    (∀ x, x ∈ S → x ∈ L) → S.Sublist L
  | [], _, _, _, _ => List.nil_sublist _
  | s :: S', [], _, _, h => by have := h s (by simp); simp at this
  | s :: S', t :: L', hS, hL, h => by
    rw [Strict, List.pairwise_cons] at hS hL
    by_cases hst : s = t
    · subst hst
      apply List.Sublist.cons_cons
      apply sublist_of_strict S' L' hS.2 hL.2
      intro x hx
      have := hS.1 x hx
      rcases List.mem_cons.mp (h x (List.mem_cons_of_mem _ hx)) with e | e
      · omega
      · exact e
    · apply List.Sublist.cons
      have hs : s ∈ L' := by
        rcases List.mem_cons.mp (h s (by simp)) with e | e
        · exact absurd e hst
        · exact e
      have hts := hL.1 s hs
      apply sublist_of_strict (s :: S') L' (by rw [Strict, List.pairwise_cons]; exact hS) hL.2
      intro x hx
      rcases List.mem_cons.mp hx with e | e
      · subst e; exact hs
      · have := hS.1 x e
        rcases List.mem_cons.mp (h x hx) with e' | e'
        · omega
        · exact e'

/-! ## The greedy counter

```rust
let gap = chrono::Duration::from_std(gap).unwrap_or(chrono::Duration::MAX);
let mut sorted = starts.to_vec();
sorted.sort();
sorted.dedup();
let mut count = 0;
let mut anchor: Option<DateTime<Utc>> = None;
for t in sorted {
    match anchor {
        Some(a) if t - a < gap => {}
        _ => { count += 1; anchor = Some(t); }
    }
}
count
```
-/

/-- The loop over the sorted, deduplicated starts, from a state whose last
counted start is `anchor`. -/
def greedy (gap : Nat) : Option Int → List Int → Nat
  | _, [] => 0
  | none, t :: rest => greedy gap (some t) rest + 1
  | some a, t :: rest =>
    if t - a < (gap : Int) then greedy gap (some a) rest else greedy gap (some t) rest + 1

/-- `separated_session_count(starts, gap)`. -/
def sessionCount (gap : Nat) (starts : List Int) : Nat := greedy gap none (canon starts)

/-- The starts the loop counts. -/
def picks (gap : Nat) : Option Int → List Int → List Int
  | _, [] => []
  | none, t :: rest => t :: picks gap (some t) rest
  | some a, t :: rest =>
    if t - a < (gap : Int) then picks gap (some a) rest else t :: picks gap (some t) rest

/-- Gap-separated: increasing, and each start at least `gap` after every
earlier one. -/
def Sep (gap : Nat) (S : List Int) : Prop := S.Pairwise (fun x y : Int => x < y ∧ x + (gap : Int) ≤ y)

theorem greedy_eq_length_picks (gap : Nat) :
    ∀ (anchor : Option Int) (L : List Int), greedy gap anchor L = (picks gap anchor L).length
  | _, [] => by cases ‹Option Int› <;> rfl
  | none, t :: rest => by
    simp only [greedy, picks, List.length_cons, greedy_eq_length_picks gap (some t) rest]
  | some a, t :: rest => by
    simp only [greedy, picks]
    by_cases h : t - a < (gap : Int)
    · rw [ite_eq_left h, ite_eq_left h]; exact greedy_eq_length_picks gap (some a) rest
    · rw [ite_eq_right h, ite_eq_right h, List.length_cons,
        greedy_eq_length_picks gap (some t) rest]

theorem picks_sublist (gap : Nat) :
    ∀ (anchor : Option Int) (L : List Int), (picks gap anchor L).Sublist L
  | _, [] => by cases ‹Option Int› <;> exact List.Sublist.slnil
  | none, t :: rest => List.Sublist.cons_cons _ (picks_sublist gap (some t) rest)
  | some a, t :: rest => by
    simp only [picks]
    by_cases h : t - a < (gap : Int)
    · rw [ite_eq_left h]; exact List.Sublist.cons _ (picks_sublist gap (some a) rest)
    · rw [ite_eq_right h]; exact List.Sublist.cons_cons _ (picks_sublist gap (some t) rest)

/-- Every start counted after anchor `a` is at least `gap` after it. -/
theorem picks_anchor (gap : Nat) :
    ∀ (a : Int) (L : List Int), ∀ p, p ∈ picks gap (some a) L → a + gap ≤ p
  | _, [] => by simp [picks]
  | a, t :: rest => by
    intro p hp
    simp only [picks] at hp
    by_cases h : t - a < (gap : Int)
    · rw [ite_eq_left h] at hp; exact picks_anchor gap a rest p hp
    · rw [ite_eq_right h] at hp
      rcases List.mem_cons.mp hp with e | e
      · omega
      · have := picks_anchor gap t rest p e; omega

/-- The counted starts are gap-separated. -/
theorem picks_sep (gap : Nat) :
    ∀ (anchor : Option Int) (L : List Int), Strict L → Sep gap (picks gap anchor L)
  | _, [], _ => by cases ‹Option Int› <;> simp [picks, Sep]
  | none, t :: rest, hL => by
    rw [Strict, List.pairwise_cons] at hL
    show Sep gap (t :: picks gap (some t) rest)
    rw [Sep, List.pairwise_cons]
    refine ⟨fun p hp => ⟨hL.1 p ((picks_sublist gap (some t) rest).subset hp),
      picks_anchor gap t rest p hp⟩, picks_sep gap (some t) rest hL.2⟩
  | some a, t :: rest, hL => by
    rw [Strict, List.pairwise_cons] at hL
    simp only [picks]
    by_cases h : t - a < (gap : Int)
    · rw [ite_eq_left h]; exact picks_sep gap (some a) rest hL.2
    · rw [ite_eq_right h, Sep, List.pairwise_cons]
      refine ⟨fun p hp => ⟨hL.1 p ((picks_sublist gap (some t) rest).subset hp),
        picks_anchor gap t rest p hp⟩, picks_sep gap (some t) rest hL.2⟩

/-- **Optimality** of the greedy rule: no gap-separated subsequence of the
sorted starts (lying past the anchor) is longer than the count. -/
theorem greedy_ge (gap : Nat) :
    ∀ (L : List Int) (anchor : Option Int) (S : List Int), Strict L → S.Sublist L →
      Sep gap S → (∀ a, anchor = some a → ∀ x, x ∈ S → a + gap ≤ x) →
      S.length ≤ greedy gap anchor L
  | [], _, S, _, hsub, _, _ => by
    rw [List.sublist_nil.mp hsub]; simp
  | t :: rest, anchor, S, hL, hsub, hsep, hanc => by
    rw [Strict, List.pairwise_cons] at hL
    -- the step that counts `t`: `S` minus its head fits after anchor `t`
    have counted : S.length ≤ greedy gap (some t) rest + 1 := by
      cases S with
      | nil => simp
      | cons s S' =>
        have hs' : S'.Sublist rest := by
          rcases List.sublist_cons_iff.mp hsub with h | ⟨r, hr, h⟩
          · exact (List.sublist_cons_self s S').trans h
          · cases hr; exact h
        have hts : t ≤ s := by
          rcases List.mem_cons.mp (hsub.subset (List.mem_cons_self)) with e | e
          · omega
          · have := hL.1 s e; omega
        rw [Sep, List.pairwise_cons] at hsep
        have := greedy_ge gap rest (some t) S' hL.2 hs' hsep.2 (by
          intro a ha x hx
          cases ha
          have := (hsep.1 x hx).2
          omega)
        simp only [List.length_cons]
        omega
    cases anchor with
    | none => exact counted
    | some a =>
      simp only [greedy]
      by_cases h : t - a < (gap : Int)
      · rw [ite_eq_left h]
        have hnot : t ∉ S := fun ht => by have := hanc a rfl t ht; omega
        have hs' : S.Sublist rest := by
          rcases List.sublist_cons_iff.mp hsub with h' | ⟨r, hr, _⟩
          · exact h'
          · exact absurd (hr ▸ List.mem_cons_self) hnot
        exact greedy_ge gap rest (some a) S hL.2 hs' hsep hanc
      · rw [ite_eq_right h]; exact counted

theorem greedy_le_length (gap : Nat) (anchor : Option Int) (L : List Int) :
    greedy gap anchor L ≤ L.length := by
  rw [greedy_eq_length_picks]; exact (picks_sublist gap anchor L).length_le

theorem greedy_zero (a : Int) :
    ∀ L : List Int, Strict L → (∀ y, y ∈ L → a < y) → greedy 0 (some a) L = L.length
  | [], _, _ => rfl
  | t :: rest, hL, ha => by
    rw [Strict, List.pairwise_cons] at hL
    have hat := ha t (by simp)
    simp only [greedy]
    have : ¬ (t - a < ((0 : Nat) : Int)) := by omega
    rw [ite_eq_right this, greedy_zero t rest hL.2 hL.1, List.length_cons]

/-! ### What the count is -/

/-- **Order-free.** The count depends only on which instants occur: not on
their order, not on how often each repeats. -/
theorem sessionCount_congr (gap : Nat) (l l' : List Int) (h : ∀ z, z ∈ l ↔ z ∈ l') :
    sessionCount gap l = sessionCount gap l' := by
  unfold sessionCount; rw [canon_congr l l' h]

/-- Permutation-invariant. -/
theorem sessionCount_perm (gap : Nat) {l l' : List Int} (h : l.Perm l') :
    sessionCount gap l = sessionCount gap l' :=
  sessionCount_congr gap l l' (fun _ => h.mem_iff)

/-- `separated_session_count` counts an exact duplicate instant once ("the
same instant is one session start"). -/
theorem sessionCount_dup (gap : Nat) (x : Int) (l : List Int) :
    sessionCount gap (x :: x :: l) = sessionCount gap (x :: l) :=
  sessionCount_congr gap _ _ (fun z => by simp)

/-- `separated_session_count` is unchanged when every about-time appears
twice: repeated instants never add a session. -/
theorem sessionCount_append_self (gap : Nat) (l : List Int) :
    sessionCount gap (l ++ l) = sessionCount gap l :=
  sessionCount_congr gap _ _ (fun z => by simp)

/-- No starts, no sessions (Rust test
`separation_counts_clusters_once_and_stragglers_each`). -/
theorem sessionCount_nil (gap : Nat) : sessionCount gap [] = 0 := rfl

/-- Some start, at least one session. -/
theorem sessionCount_pos (gap : Nat) (l : List Int) (h : l ≠ []) : 1 ≤ sessionCount gap l := by
  unfold sessionCount
  cases hc : canon l with
  | nil =>
    cases l with
    | nil => exact absurd rfl h
    | cons x xs =>
      have := (mem_canon x (x :: xs)).mpr (by simp)
      rw [hc] at this; simp at this
  | cons t rest => simp [greedy]

/-- At most one session per distinct start. -/
theorem sessionCount_le_distinct (gap : Nat) (l : List Int) :
    sessionCount gap l ≤ (canon l).length :=
  greedy_le_length gap none (canon l)

/-- `separated_session_count` never counts more sessions than it is given
about-times. -/
theorem sessionCount_le_length (gap : Nat) (l : List Int) : sessionCount gap l ≤ l.length :=
  Nat.le_trans (sessionCount_le_distinct gap l) (length_canon_le l)

/-- With `gap = 0` every distinct start is a session. -/
theorem sessionCount_zero_gap (l : List Int) : sessionCount 0 l = (canon l).length := by
  unfold sessionCount
  have hs := strict_canon l
  cases hc : canon l with
  | nil => rfl
  | cons t rest =>
    rw [hc, Strict, List.pairwise_cons] at hs
    simp only [greedy, List.length_cons]
    rw [greedy_zero t rest hs.2 hs.1]

/-- **The count is a maximum.** No gap-separated set of starts is larger
than the count ... -/
theorem sessionCount_max (gap : Nat) (l S : List Int) (hS : Sep gap S)
    (hm : ∀ x, x ∈ S → x ∈ l) : S.length ≤ sessionCount gap l := by
  have hstrict : Strict S := hS.imp (fun h => h.1)
  have hsub : S.Sublist (canon l) :=
    sublist_of_strict S (canon l) hstrict (strict_canon l)
      (fun x hx => (mem_canon x l).mpr (hm x hx))
  exact greedy_ge gap (canon l) none S (strict_canon l) hsub hS (fun _ h => by cases h)

/-- ... and the starts the loop counts are such a set. -/
theorem sessionCount_attained (gap : Nat) (l : List Int) :
    ∃ S, Sep gap S ∧ (∀ x, x ∈ S → x ∈ l) ∧ S.length = sessionCount gap l :=
  ⟨picks gap none (canon l), picks_sep gap none (canon l) (strict_canon l),
    fun x hx => (mem_canon x l).mp ((picks_sublist gap none (canon l)).subset hx),
    (greedy_eq_length_picks gap none (canon l)).symm⟩

/-- **Monotone.** More starts never mean fewer sessions. -/
theorem sessionCount_mono (gap : Nat) (l l' : List Int) (h : ∀ x, x ∈ l → x ∈ l') :
    sessionCount gap l ≤ sessionCount gap l' := by
  obtain ⟨S, hS, hm, hlen⟩ := sessionCount_attained gap l
  rw [← hlen]
  exact sessionCount_max gap l' S hS (fun x hx => h x (hm x hx))

/-- Adding an about-time never lowers `separated_session_count`. -/
theorem sessionCount_le_cons (gap : Nat) (x : Int) (l : List Int) :
    sessionCount gap l ≤ sessionCount gap (x :: l) :=
  sessionCount_mono gap l (x :: l) (fun _ h => List.mem_cons_of_mem _ h)

theorem length_le_filter_ne (x : Int) :
    ∀ S : List Int, Strict S → S.length ≤ (S.filter (fun y => y != x)).length + 1
  | [], _ => by simp
  | s :: S', hS => by
    rw [Strict, List.pairwise_cons] at hS
    by_cases hsx : s = x
    · subst hsx
      have : S'.filter (fun y => y != s) = S' :=
        List.filter_eq_self.mpr (fun a ha => by
          have := hS.1 a ha
          simp only [bne_iff_ne, ne_eq]; omega)
      simp [this]
    · have ih := length_le_filter_ne x S' hS.2
      have hb : (s != x) = true := by simp [hsx]
      simp only [List.filter_cons, hb, ite_true, List.length_cons]
      omega

/-- Adding an about-time raises `separated_session_count` by at most one. -/
theorem sessionCount_cons_le (gap : Nat) (x : Int) (l : List Int) :
    sessionCount gap (x :: l) ≤ sessionCount gap l + 1 := by
  obtain ⟨S, hS, hm, hlen⟩ := sessionCount_attained gap (x :: l)
  rw [← hlen]
  have hstrict : Strict S := hS.imp (fun h => h.1)
  have hf : Sep gap (S.filter (fun y => y != x)) := List.Pairwise.filter _ hS
  have hfm : ∀ y, y ∈ S.filter (fun y => y != x) → y ∈ l := by
    intro y hy
    rw [List.mem_filter] at hy
    rcases List.mem_cons.mp (hm y hy.1) with e | e
    · subst e; simp at hy
    · exact e
  have := sessionCount_max gap l _ hf hfm
  have := length_le_filter_ne x S hstrict
  omega

/-- A wider `SESSION_SEPARATION` never makes `separated_session_count` count
more sessions. -/
theorem sessionCount_antitone_gap (g g' : Nat) (hg : g ≤ g') (l : List Int) :
    sessionCount g' l ≤ sessionCount g l := by
  obtain ⟨S, hS, hm, hlen⟩ := sessionCount_attained g' l
  rw [← hlen]
  exact sessionCount_max g l S (hS.imp (fun h => ⟨h.1, by have := h.2; omega⟩)) hm

/-- **One window, one session.** Starts that all lie less than `gap` after
the earliest one count as one session. -/
theorem sessionCount_window (gap : Nat) (l : List Int) (m : Int) (hm : m ∈ l)
    (h : ∀ x, x ∈ l → m ≤ x ∧ x < m + gap) : sessionCount gap l = 1 := by
  have hpos := sessionCount_pos gap l (fun e => by subst e; simp at hm)
  obtain ⟨S, hS, hmem, hlen⟩ := sessionCount_attained gap l
  have : S.length ≤ 1 := by
    match S, hS, hmem with
    | [], _, _ => simp
    | [_], _, _ => simp
    | a :: b :: _, hS, hmem =>
      rw [Sep, List.pairwise_cons] at hS
      have hab := (hS.1 b (by simp)).2
      have ha := h a (hmem a (by simp))
      have hb := h b (hmem b (by simp))
      omega
  omega

/-! ### The Rust tests, and what "sessions" means -/

/-- Seconds since the epoch of the Rust tests' `ts(0)`. -/
def base : Int := 1752000000

/-- One day: `SESSION_SEPARATION`, 86 400 s.
Pinned by `formal_constants_tests::policy`. -/
def sessionSeparation : Nat := 86400

/-- Rust test `separation_counts_clusters_once_and_stragglers_each`: a cluster
of three starts within an hour, a start exactly 24 h later (the boundary
counts, `>=`), and one 50 h after the first count as 3; no starts as 0; one
start as 1; three copies of one instant as 1. -/
theorem separation_counts_clusters_once_and_stragglers_each :
    sessionCount sessionSeparation
        [base, base + 600, base + 3600, base + 86400, base + 172800 + 7200] = 3 ∧
      sessionCount 0 [] = 0 ∧ sessionCount 0 [base + 5] = 1 ∧
      sessionCount 0 [base, base, base] = 1 := by
  decide

/-- The count is the largest gap-separated set of starts, not the number of
clusters joined by links shorter than `gap`: five starts 23 h apart are one
such cluster (every neighbour within 24 h) but three sessions (hours 0, 46
and 92). -/
theorem chain_counts_more_than_one :
    sessionCount sessionSeparation
      [base, base + 23 * 3600, base + 46 * 3600, base + 69 * 3600, base + 92 * 3600] = 3 := by
  decide

end Lambo.Policy
