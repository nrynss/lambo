/-
# Shared lemmas about lists

Generic facts about lists (membership, filters, duplicate-free lists,
permutations, prefixes, `getD`, folds) that mention no Lambo type and model
no Rust. There is no Mathlib here, so a model that needs such a fact proves
it; once it is proved, it belongs here, where every model can import it,
rather than in the model file. A model uses them with
`open Lambo.Lib (name ...)` inside its namespace.

Each lemma kept its statement when it moved here from the model named in its
section; only its namespace changed (`Lambo.Fusion.length_filter_mono` is now
`Lambo.Lib.length_filter_mono`). `formal/audit/moved-helpers.tsv` lists every
move, old name to new, for `scripts/formal/check-decl-equivalence.py`.

This file is not a model: it has no entry in `formal/models.toml` and is not
counted by `scripts/formal/count-theorems.py`.
-/

namespace Lambo.Lib

/-! ## Membership, `any` and `map`

Moved from `BatchQueue.lean` (now `BatchQueue/`), `Fusion.lean` (now `Fusion/`), `Hotlist.lean` (now `Hotlist/`). -/

theorem any_of_mem {ι : Type} {ids : List ι} {f : ι → Bool} {n : ι} (hn : n ∈ ids) (h : f n = true) :
    ids.any f = true := List.any_eq_true.mpr ⟨n, hn, h⟩

theorem map_eq_pointwise {α β : Type} {g h : α → β} {l : List α} (e : l.map g = l.map h) :
    ∀ a ∈ l, g a = h a := by
  induction l with
  | nil => simp
  | cons x xs ih =>
    simp only [List.map_cons, List.cons.injEq] at e
    intro a ha
    rcases List.mem_cons.mp ha with rfl | ha
    · exact e.1
    · exact ih e.2 a ha

theorem eq_of_nodup_map {α β : Type} (f : α → β) :
    ∀ {l : List α}, (l.map f).Nodup → ∀ {x y : α}, x ∈ l → y ∈ l → f x = f y → x = y
  | [], _, _, _, hx, _, _ => by simp at hx
  | z :: zs, h, x, y, hx, hy, hf => by
    have hz := List.nodup_cons.mp (show (f z :: zs.map f).Nodup from h)
    rcases List.mem_cons.mp hx with hx1 | hx2
    · rcases List.mem_cons.mp hy with hy1 | hy2
      · rw [hx1, hy1]
      · rw [hx1] at hf; exact absurd (hf ▸ List.mem_map_of_mem hy2) hz.1
    · rcases List.mem_cons.mp hy with hy1 | hy2
      · rw [hy1] at hf; exact absurd (hf.symm ▸ List.mem_map_of_mem hx2) hz.1
      · exact eq_of_nodup_map f hz.2 hx2 hy2 hf

/-! ## Lengths of filters and of duplicate-free lists

Moved from `Conflict.lean` (now `Conflict/Detect.lean`), `Fusion.lean` (now `Fusion/`), `Hotlist.lean` (now `Hotlist/`). -/

theorem length_filter_mono {ι : Type} {l : List ι} {p q : ι → Bool} (h : ∀ x, p x = true → q x = true) :
    (l.filter p).length ≤ (l.filter q).length := by
  induction l with
  | nil => simp
  | cons x rest ih =>
    simp only [List.filter_cons]
    cases hp : p x <;> cases hq : q x <;> simp_all <;> omega

theorem length_filter_lt {ι : Type} {l : List ι} {p q : ι → Bool} (h : ∀ x, p x = true → q x = true)
    {a : ι} (ha : a ∈ l) (hq : q a = true) (hp : p a = false) :
    (l.filter p).length < (l.filter q).length := by
  induction l with
  | nil => simp at ha
  | cons x rest ih =>
    have hmono := length_filter_mono (l := rest) h
    simp only [List.filter_cons]
    rcases List.mem_cons.mp ha with hx | ha'
    · subst hx
      simp [hq, hp]
      omega
    · have := ih ha'
      cases hpx : p x <;> cases hqx : q x <;> simp <;> first | omega | (have := h x hpx; simp_all)

theorem length_filter_shift {ι : Type} [DecidableEq ι] {l : List ι} (hnd : l.Nodup) (p q : ι → Bool) (t : ι)
    (heq : ∀ x, x ≠ t → q x = p x) (hpt : p t = true → q t = true) :
    (l.filter q).length =
      (l.filter p).length + (if t ∈ l ∧ q t = true ∧ p t = false then 1 else 0) := by
  induction l with
  | nil => simp
  | cons x rest ih =>
    rw [List.nodup_cons] at hnd
    have ih := ih hnd.2
    simp only [List.filter_cons]
    by_cases hx : x = t
    · subst hx
      have hc : ¬ (x ∈ rest ∧ q x = true ∧ p x = false) := fun h => hnd.1 h.1
      rw [ite_eq_right hc] at ih
      cases hp : p x <;> cases hq : q x <;> simp_all
    · have hqx := heq x hx
      have hmem : (t ∈ x :: rest) ↔ t ∈ rest := by
        simp only [List.mem_cons]; constructor
        · rintro (h | h)
          · exact absurd h.symm hx
          · exact h
        · exact Or.inr
      by_cases hc : t ∈ rest ∧ q t = true ∧ p t = false
      · have hc' : t ∈ x :: rest ∧ q t = true ∧ p t = false := ⟨hmem.mpr hc.1, hc.2⟩
        rw [ite_eq_left hc] at ih; rw [ite_eq_left hc']
        cases hp : p x <;> simp_all <;> omega
      · have hc' : ¬ (t ∈ x :: rest ∧ q t = true ∧ p t = false) := fun h => hc ⟨hmem.mp h.1, h.2⟩
        rw [ite_eq_right hc] at ih; rw [ite_eq_right hc']
        cases hp : p x <;> simp_all

theorem length_le_of_nodup_subset {α : Type} [DecidableEq α] :
    ∀ {l₁ : List α} (l₂ : List α), l₁.Nodup → (∀ x ∈ l₁, x ∈ l₂) → l₁.length ≤ l₂.length
  | [], _, _, _ => by simp
  | a :: t, l₂, hn, hs => by
    have hn' := List.nodup_cons.mp hn
    have ha : a ∈ l₂ := hs a List.mem_cons_self
    have ht : ∀ x ∈ t, x ∈ l₂.erase a := fun x hx =>
      (List.mem_erase_of_ne (fun h : x = a => hn'.1 (by rw [← h]; exact hx))).mpr (hs x (List.mem_cons_of_mem _ hx))
    have h1 := length_le_of_nodup_subset (l₂.erase a) hn'.2 ht
    rw [List.length_erase_of_mem ha] at h1
    have := List.length_pos_of_mem ha
    simp only [List.length_cons]
    omega

/-- A duplicate-free list has two elements iff it holds two distinct ones. -/
theorem two_le_length_iff {A : Type} {l : List A} (h : l.Nodup) :
    2 ≤ l.length ↔ ∃ a ∈ l, ∃ b ∈ l, a ≠ b := by
  constructor
  · intro hl
    match l, hl, h with
    | a :: b :: _, _, h =>
      rw [List.nodup_cons] at h
      exact ⟨a, by simp, b, by simp, fun hab => h.1 (by simp [hab])⟩
  · rintro ⟨a, ha, b, hb, hab⟩
    match l, ha, hb with
    | [], ha, _ => simp at ha
    | [c], ha, hb =>
      simp at ha hb
      exact absurd (ha.trans hb.symm) hab
    | _ :: _ :: _, _, _ => simp

/-! ## Permutations and prefixes

Moved from `BatchQueue.lean` (now `BatchQueue/`), `Flush.lean` (now `Flush/Loop.lean`). -/

theorem eraseIdx_perm {α : Type} (g : List α) (i : Nat) (h : i < g.length) :
    g.Perm (g[i] :: g.eraseIdx i) := by
  rw [List.eraseIdx_eq_take_drop_succ]
  conv => lhs; rw [← List.take_append_drop i g, List.drop_eq_getElem_cons h]
  exact List.perm_middle

theorem prefix_drop {α : Type} {l₁ l₂ : List α} (h : l₁ <+: l₂) (k : Nat) : l₁.drop k <+: l₂.drop k := by
  obtain ⟨r, rfl⟩ := h
  rw [List.drop_append]
  by_cases hk : k ≤ l₁.length
  · have : k - l₁.length = 0 := by omega
    rw [this, List.drop_zero]
    exact List.prefix_append _ _
  · have : l₁.drop k = [] := List.drop_eq_nil_of_le (by omega)
    rw [this]
    simp

/-! ## `getD` on a list of naturals

Moved from `Fit.lean`. -/

theorem getD_mem {l : List Nat} {i : Nat} (h : i < l.length) : l.getD i 0 ∈ l := by
  rw [List.getD_eq_getElem?_getD, List.getElem?_eq_getElem h]
  exact List.getElem_mem h

theorem getD_mono {l : List Nat} (hp : l.Pairwise (· < ·)) {i j : Nat} (hij : i ≤ j)
    (hj : j < l.length) : l.getD i 0 ≤ l.getD j 0 := by
  rw [List.getD_eq_getElem?_getD, List.getD_eq_getElem?_getD,
    List.getElem?_eq_getElem (by omega), List.getElem?_eq_getElem hj]
  by_cases e : i = j
  · subst e; exact Nat.le_refl _
  · exact Nat.le_of_lt (List.pairwise_iff_getElem.mp hp i j (by omega) hj (by omega))

theorem getD_eq_getElem {l : List Nat} {i : Nat} (h : i < l.length) : l.getD i 0 = l[i] := by
  rw [List.getD_eq_getElem?_getD, List.getElem?_eq_getElem h]
  rfl

/-! ## Folding `max` over integers

Moved from `Confidence.lean`. -/

theorem foldl_max_ge_init (xs : List Int) (x : Int) : x ≤ xs.foldl max x := by
  induction xs generalizing x with
  | nil => simp
  | cons y ys ih =>
    simp only [List.foldl_cons]
    exact Int.le_trans (by omega) (ih (max x y))

theorem foldl_max_ge (xs : List Int) (x y : Int) (hy : y ∈ xs) : y ≤ xs.foldl max x := by
  induction xs generalizing x with
  | nil => cases hy
  | cons z zs ih =>
    simp only [List.foldl_cons]
    rcases List.mem_cons.mp hy with rfl | h
    · exact Int.le_trans (by omega) (foldl_max_ge_init zs (max x y))
    · exact ih _ h

theorem foldl_max_mem (xs : List Int) (x : Int) : xs.foldl max x ∈ x :: xs := by
  induction xs generalizing x with
  | nil => simp
  | cons y ys ih =>
    simp only [List.foldl_cons]
    rcases List.mem_cons.mp (ih (max x y)) with h | h
    · rw [h]
      by_cases hxy : x ≤ y
      · rw [show max x y = y by omega]; simp
      · rw [show max x y = x by omega]; simp
    · simp [h]

/-! ## `eraseDups` on naturals

Moved from `Policy.lean` (now `Policy/Support.lean`). -/

/-- `eraseDups` leaves no duplicates. -/
theorem nodup_eraseDups : ∀ (n : Nat) (l : List Nat), l.length ≤ n → l.eraseDups.Nodup
  | _, [], _ => by simp [List.eraseDups, List.eraseDupsBy, List.eraseDupsBy.loop]
  | 0, _ :: _, h => by simp at h
  | n + 1, a :: as, h => by
    rw [List.eraseDups_cons, List.nodup_cons]
    constructor
    · intro hm
      rw [List.mem_eraseDups, List.mem_filter] at hm
      simp at hm
    · apply nodup_eraseDups n
      have := List.length_filter_le (fun b => !b == a) as
      simp only [List.length_cons] at h
      omega

/-- One more element never shortens the deduplicated list. -/
theorem length_eraseDups_le_cons (x : Nat) (l : List Nat) :
    l.eraseDups.length ≤ (x :: l).eraseDups.length :=
  List.Nodup.length_le_of_subset (nodup_eraseDups _ l (Nat.le_refl _)) (fun y hy => by
    rw [List.mem_eraseDups] at hy ⊢
    exact List.mem_cons_of_mem _ hy)

end Lambo.Lib
