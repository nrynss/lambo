/- Proofs of `Drain/Spec/Replay.lean`. -/

import LamboProofs.Drain.Spec.Replay
import LamboProofs.Drain.Proofs.Base

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

/-! ## Input refusals -/

theorem stepInput_agrees : Spec.stepInput_agrees := by
  intro img rr s h
  unfold stepInput
  split
  · exact absurd rfl h
  · rfl

/-- Every input refusal settles `failed`. -/
theorem stepInput_refusesInput (img : Bool) (e : Err) (s : Streaks) (h : e.refusesInput = true) :
    (stepInput img (.run (.err e)) s).1 = .failed := by
  cases e <;> simp_all [Err.refusesInput, Err.inputFact, stepInput, replayStep]

/-- **And still stops on everything else that is not transient**: a store
error that can mean an inconsistent graph or store, `Config`, a lost lease,
anything else. -/
theorem stepInput_sessionWide (img : Bool) (e : Err) (s : Streaks) (h1 : e.refusesInput = false)
    (h2 : e.transient = false) : ∃ b, (stepInput img (.run (.err e)) s).1 = .stop b := by
  cases e <;> simp_all [Err.refusesInput, Err.inputFact, Err.transient, stepInput, replayStep]

theorem fixed_sessionWide_stops : Spec.fixed_sessionWide_stops := by
  intro rout s i rest e he h1 h2
  obtain ⟨b, hb⟩ := stepInput_sessionWide i.job.image e s h1 h2
  refine ⟨b, ?_⟩
  simp only [loopWith, he]
  cases hs : stepInput i.job.image (.run (.err e)) s with
  | mk st s' =>
    rw [hs] at hb
    simp only at hb
    subst hb
    rfl

/-! ## Deferring an agent -/

/-- On everything but an unparseable id `stepFixed` is `stepInput`. -/
theorem stepFixed_agrees (img : Bool) (rr : RR) (s : Streaks) (h : rr ≠ .badId)
    (h' : rr ≠ .unknownFormat) : stepFixed img rr s = stepInput img rr s := by
  unfold stepFixed
  split
  · exact absurd rfl h
  · exact absurd rfl h'
  · rfl

/-- Every input refusal settles `failed` under `stepFixed` too. -/
theorem stepFixed_refusesInput (img : Bool) (e : Err) (s : Streaks) (h : e.refusesInput = true) :
    (stepFixed img (.run (.err e)) s).1 = .failed := by
  rw [stepFixed_agrees img _ s (by simp) (by simp)]
  exact stepInput_refusesInput img e s h

/-- The only result `stepFixed` leaves unsettled without stopping is a
transient embedder answer: so only it defers an agent. -/
theorem stepFixed_skip {img : Bool} {rr : RR} {s : Streaks} (h : (stepFixed img rr s).1 = .skip) :
    rr = .run (.err .unavailable) := by
  cases rr with
  | halt => simp [stepFixed, stepInput, replayStep] at h
  | badId => simp [stepFixed] at h
  | unknownFormat => simp [stepFixed] at h
  | decodeRefused => simp [stepFixed, stepInput, replayStep] at h
  | run r =>
    cases r with
    | ok => simp [stepFixed, stepInput, replayStep] at h
    | err e =>
      cases e <;> simp [stepFixed, stepInput, replayStep] at h ⊢
      all_goals (split at h <;> simp at h)

/-! ### The loop -/

theorem loopD_deferred {step : Bool → RR → Streaks → Step × Streaks} {rout : Job → RR}
    {s : Streaks} {d : List Nat} {i : Intent} {rest : List Intent} (h : i.job.agent ∈ d) :
    loopD step rout s d (i :: rest) = loopD step rout s d rest := by
  simp [loopD, h]

theorem loopD_applied {step : Bool → RR → Streaks → Step × Streaks} {rout : Job → RR}
    {s s' : Streaks} {d : List Nat} {i : Intent} {rest : List Intent} (hd : i.job.agent ∉ d)
    (h : step i.job.image (rout i.job) s = (.applied, s')) :
    loopD step rout s d (i :: rest) =
      { loopD step rout s' d rest with
        answers := (i.job.key, .appliedAfterRestart) :: (loopD step rout s' d rest).answers
        log := [.write i.job.key, .consume i.job.key .appliedAfterRestart] ::
          (loopD step rout s' d rest).log
        applied := i.job :: (loopD step rout s' d rest).applied } := by
  simp only [loopD, hd, h, ite_false]

theorem loopD_failed {step : Bool → RR → Streaks → Step × Streaks} {rout : Job → RR}
    {s s' : Streaks} {d : List Nat} {i : Intent} {rest : List Intent} (hd : i.job.agent ∉ d)
    (h : step i.job.image (rout i.job) s = (.failed, s')) :
    loopD step rout s d (i :: rest) =
      { loopD step rout s' d rest with
        answers := (i.job.key, .failed) :: (loopD step rout s' d rest).answers
        log := [.consume i.job.key .failed] :: (loopD step rout s' d rest).log
        failed := i.job :: (loopD step rout s' d rest).failed } := by
  simp only [loopD, hd, h, ite_false]

theorem loopD_skip {step : Bool → RR → Streaks → Step × Streaks} {rout : Job → RR}
    {s s' : Streaks} {d : List Nat} {i : Intent} {rest : List Intent} (hd : i.job.agent ∉ d)
    (h : step i.job.image (rout i.job) s = (.skip, s')) :
    loopD step rout s d (i :: rest) = loopD step rout s' (i.job.agent :: d) rest := by
  simp only [loopD, hd, h, ite_false]

theorem loopD_stop {step : Bool → RR → Streaks → Step × Streaks} {rout : Job → RR}
    {s s' : Streaks} {d : List Nat} {i : Intent} {rest : List Intent} {b : Block}
    (hd : i.job.agent ∉ d) (h : step i.job.image (rout i.job) s = (.stop b, s')) :
    loopD step rout s d (i :: rest) = { ROut.empty with block := some b } := by
  simp only [loopD, hd, h, ite_false]

/-- What `loopD` applies and fails comes from its rows, its log has no put and
consumes each, and no job of a deferred agent is applied. -/
theorem loopD_facts (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR) :
    ∀ (l : List Intent) (s : Streaks) (d : List Nat),
    (loopD step rout s d l).applied.Sublist (l.map (·.job)) ∧
    (loopD step rout s d l).failed.Sublist (l.map (·.job)) ∧
    NoPut (loopD step rout s d l).log.flatten ∧
    (∀ x, x ∈ (loopD step rout s d l).applied ∨ x ∈ (loopD step rout s d l).failed →
      ∃ t, Mut.consume x.key t ∈ (loopD step rout s d l).log.flatten) ∧
    (∀ x ∈ (loopD step rout s d l).applied, x.agent ∉ d) := by
  intro l
  induction l with
  | nil =>
    intro s d
    refine ⟨List.Sublist.slnil, List.Sublist.slnil, ?_, ?_, ?_⟩
    · intro m hm; simp [loopD, ROut.empty] at hm
    · intro x hx; simp [loopD, ROut.empty] at hx
    · intro x hx; simp [loopD, ROut.empty] at hx
  | cons i rest ih =>
    intro s d
    by_cases hd : i.job.agent ∈ d
    · rw [loopD_deferred hd]
      obtain ⟨h1, h2, h3, h4, h5⟩ := ih s d
      exact ⟨List.Sublist.cons _ h1, List.Sublist.cons _ h2, h3, h4, h5⟩
    · cases h : step i.job.image (rout i.job) s with
      | mk st s' =>
        cases st with
        | applied =>
          rw [loopD_applied hd h]
          obtain ⟨h1, h2, h3, h4, h5⟩ := ih s' d
          refine ⟨List.Sublist.cons_cons _ h1, List.Sublist.cons _ h2, ?_, ?_, ?_⟩
          · intro m hm j
            simp only [List.flatten_cons, List.mem_append, List.mem_cons] at hm
            rcases hm with ((hm | hm | hm) | hm)
            · rw [hm]; exact Mut.noConfusion
            · rw [hm]; exact Mut.noConfusion
            · simp at hm
            · exact h3 m hm j
          · intro x hx
            simp only [List.mem_cons] at hx
            rcases hx with (hx | hx) | hx
            · exact ⟨.appliedAfterRestart, by rw [hx]; simp⟩
            · obtain ⟨t, ht⟩ := h4 x (Or.inl hx)
              exact ⟨t, by simp only [List.flatten_cons]; exact List.mem_append_right _ ht⟩
            · obtain ⟨t, ht⟩ := h4 x (Or.inr hx)
              exact ⟨t, by simp only [List.flatten_cons]; exact List.mem_append_right _ ht⟩
          · intro x hx
            cases List.mem_cons.mp hx with
            | inl e => rw [e]; exact hd
            | inr hx' => exact h5 x hx'
        | failed =>
          rw [loopD_failed hd h]
          obtain ⟨h1, h2, h3, h4, h5⟩ := ih s' d
          refine ⟨List.Sublist.cons _ h1, List.Sublist.cons_cons _ h2, ?_, ?_, h5⟩
          · intro m hm j
            simp only [List.flatten_cons, List.mem_append, List.mem_cons] at hm
            rcases hm with ((hm | hm) | hm)
            · rw [hm]; exact Mut.noConfusion
            · simp at hm
            · exact h3 m hm j
          · intro x hx
            simp only [List.mem_cons] at hx
            rcases hx with hx | hx | hx
            · obtain ⟨t, ht⟩ := h4 x (Or.inl hx)
              exact ⟨t, by simp only [List.flatten_cons]; exact List.mem_append_right _ ht⟩
            · exact ⟨.failed, by rw [hx]; simp⟩
            · obtain ⟨t, ht⟩ := h4 x (Or.inr hx)
              exact ⟨t, by simp only [List.flatten_cons]; exact List.mem_append_right _ ht⟩
        | skip =>
          rw [loopD_skip hd h]
          obtain ⟨h1, h2, h3, h4, h5⟩ := ih s' (i.job.agent :: d)
          refine ⟨List.Sublist.cons _ h1, List.Sublist.cons _ h2, h3, h4, ?_⟩
          intro x hx hxd
          exact h5 x hx (List.mem_cons_of_mem _ hxd)
        | stop b =>
          rw [loopD_stop hd h]
          refine ⟨List.nil_sublist _, List.nil_sublist _, ?_, ?_, ?_⟩
          · intro m hm; simp [ROut.empty] at hm
          · intro x hx; simp [ROut.empty] at hx
          · intro x hx; simp [ROut.empty] at hx

theorem loopD_no_overtake : Spec.loopD_no_overtake := by
  intro step rout
  intro pre
  induction pre with
  | nil => intro ix post s d _ _ i hi; simp at hi
  | cons i0 pre' ih =>
    intro ix post s d hnd hx i hi hag
    simp only [List.cons_append] at hnd hx ⊢
    rw [List.map_cons, List.nodup_cons] at hnd
    have hne : i0.job ≠ ix.job := by
      intro e
      apply hnd.1
      rw [e]
      exact List.mem_map_of_mem (List.mem_append_right _ List.mem_cons_self)
    by_cases hd : i0.job.agent ∈ d
    · rw [loopD_deferred hd] at hx ⊢
      cases List.mem_cons.mp hi with
      | inl e =>
        subst e
        exact absurd (hag ▸ hd) ((loopD_facts step rout _ s d).2.2.2.2 _ hx)
      | inr hi' => exact ih ix post s d hnd.2 hx i hi' hag
    · cases h : step i0.job.image (rout i0.job) s with
      | mk st s' =>
        cases st with
        | applied =>
          rw [loopD_applied hd h] at hx ⊢
          have hx' := (List.mem_cons.mp hx).resolve_left (Ne.symm hne)
          cases List.mem_cons.mp hi with
          | inl e => subst e; exact Or.inl List.mem_cons_self
          | inr hi' =>
            rcases ih ix post s' d hnd.2 hx' i hi' hag with h' | h'
            · exact Or.inl (List.mem_cons_of_mem _ h')
            · exact Or.inr h'
        | failed =>
          rw [loopD_failed hd h] at hx ⊢
          cases List.mem_cons.mp hi with
          | inl e => subst e; exact Or.inr List.mem_cons_self
          | inr hi' =>
            rcases ih ix post s' d hnd.2 hx i hi' hag with h' | h'
            · exact Or.inl h'
            · exact Or.inr (List.mem_cons_of_mem _ h')
        | skip =>
          rw [loopD_skip hd h] at hx ⊢
          cases List.mem_cons.mp hi with
          | inl e =>
            subst e
            exact absurd (hag ▸ List.mem_cons_self)
              ((loopD_facts step rout _ s' _).2.2.2.2 _ hx)
          | inr hi' => exact ih ix post s' _ hnd.2 hx i hi' hag
        | stop b =>
          rw [loopD_stop hd h] at hx
          simp [ROut.empty] at hx

theorem replayD_cases (step : Bool → RR → Streaks → Step × Streaks) (live : Bool)
    (rout : Job → RR) (intents : List Intent) :
    replayD step live rout intents = ROut.empty ∨
      replayD step live rout intents = { ROut.empty with block := some .embedder } ∨
      replayD step live rout intents = loopD step rout ⟨0, 0⟩ [] (pendingOf intents) := by
  unfold replayD
  cases hp : pendingOf intents with
  | nil => exact Or.inl rfl
  | cons p ps =>
    cases live with
    | true => exact Or.inr (Or.inr rfl)
    | false => exact Or.inr (Or.inl rfl)

/-- The loop facts lifted to `replayD`, over the pending rows. -/
theorem replayD_facts (step : Bool → RR → Streaks → Step × Streaks) (live : Bool)
    (rout : Job → RR) (intents : List Intent) :
    (replayD step live rout intents).applied.Sublist ((pendingOf intents).map (·.job)) ∧
    (replayD step live rout intents).failed.Sublist ((pendingOf intents).map (·.job)) ∧
    NoPut (replayD step live rout intents).log.flatten ∧
    (∀ x, x ∈ (replayD step live rout intents).applied ∨
        x ∈ (replayD step live rout intents).failed →
      ∃ t, Mut.consume x.key t ∈ (replayD step live rout intents).log.flatten) := by
  rcases replayD_cases step live rout intents with h | h | h <;> rw [h]
  · refine ⟨List.nil_sublist _, List.nil_sublist _, ?_, ?_⟩
    · intro m hm; simp [ROut.empty] at hm
    · intro x hx; simp [ROut.empty] at hx
  · refine ⟨List.nil_sublist _, List.nil_sublist _, ?_, ?_⟩
    · intro m hm; simp [ROut.empty] at hm
    · intro x hx; simp [ROut.empty] at hx
  · obtain ⟨h1, h2, h3, h4, _⟩ := loopD_facts step rout _ ⟨0, 0⟩ []
    exact ⟨h1, h2, h3, h4⟩

/-- `replayD`, too, applies only unconsumed rows. -/
theorem replayD_applied_pending {step : Bool → RR → Streaks → Step × Streaks} {live : Bool}
    {rout : Job → RR} {intents : List Intent} {x : Job}
    (hx : x ∈ (replayD step live rout intents).applied) :
    ∃ i ∈ intents, i.job = x ∧ i.outcome = none := by
  have hs := (replayD_facts step live rout intents).1.subset hx
  obtain ⟨i, hi, rfl⟩ := List.mem_map.mp hs
  obtain ⟨hi1, hi2⟩ := List.mem_filter.mp hi
  exact ⟨i, hi1, rfl, Option.isNone_iff_eq_none.mp hi2⟩

/-- Rows with distinct receipt ids: a row is determined by its id. -/
theorem mem_eq_of_key {L : List Intent} (hnd : (L.map (·.job.key)).Nodup) {a b : Intent}
    (ha : a ∈ L) (hb : b ∈ L) (h : a.job.key = b.job.key) : a = b := by
  induction L with
  | nil => simp at ha
  | cons k L ih =>
    rw [List.map_cons, List.nodup_cons] at hnd
    cases List.mem_cons.mp ha with
    | inl ha' =>
      cases List.mem_cons.mp hb with
      | inl hb' => rw [ha', hb']
      | inr hb' =>
        subst ha'
        exact absurd (h ▸ List.mem_map_of_mem (f := (·.job.key)) hb') hnd.1
    | inr ha' =>
      cases List.mem_cons.mp hb with
      | inl hb' =>
        subst hb'
        exact absurd (h.symm ▸ List.mem_map_of_mem (f := (·.job.key)) ha') hnd.1
      | inr hb' => exact ih hnd.2 ha' hb'

/-! ### The load order -/

theorem minE_spec : ∀ (l : List Intent) (x : Intent),
    (minE x l).job.epoch = x.job.epoch ∧ minE x l ∈ x :: l ∧
      ∀ y ∈ x :: l, y.job.epoch = x.job.epoch → (minE x l).job.rid ≤ y.job.rid := by
  intro l
  induction l with
  | nil =>
    intro x
    refine ⟨rfl, List.mem_cons_self, ?_⟩
    intro y hy _
    rw [List.mem_singleton.mp hy]
    exact Nat.le_refl _
  | cons y ys ih =>
    intro x
    simp only [minE]
    by_cases hc : y.job.epoch = x.job.epoch ∧ y.job.rid < x.job.rid
    · rw [ite_eq_left hc]
      obtain ⟨h1, h2, h3⟩ := ih y
      refine ⟨h1.trans hc.1, ?_, ?_⟩
      · exact List.mem_cons_of_mem _ h2
      · intro z hz hze
        cases List.mem_cons.mp hz with
        | inl e => rw [e]; exact Nat.le_of_lt (Nat.lt_of_le_of_lt (h3 y List.mem_cons_self rfl) hc.2)
        | inr hz' => exact h3 z hz' (hze.trans hc.1.symm)
    · rw [ite_eq_right hc]
      obtain ⟨h1, h2, h3⟩ := ih x
      refine ⟨h1, ?_, ?_⟩
      · cases List.mem_cons.mp h2 with
        | inl e => rw [e]; exact List.mem_cons_self
        | inr h' => exact List.mem_cons_of_mem _ (List.mem_cons_of_mem _ h')
      · intro z hz hze
        cases List.mem_cons.mp hz with
        | inl e => rw [e]; exact h3 x List.mem_cons_self rfl
        | inr hz' =>
          cases List.mem_cons.mp hz' with
          | inl e =>
            rw [e] at hze ⊢
            have hxy : x.job.rid ≤ y.job.rid := by
              rcases Nat.lt_or_ge y.job.rid x.job.rid with hl | hl
              · exact absurd ⟨hze, hl⟩ hc
              · exact hl
            exact Nat.le_trans (h3 x List.mem_cons_self rfl) hxy
          | inr hz'' => exact h3 z (List.mem_cons_of_mem _ hz'') hze

/-- With nothing earlier in sequence in its epoch, `x` is its own minimum. -/
theorem minE_self : ∀ (l : List Intent) (x : Intent),
    (∀ y ∈ l, y.job.epoch = x.job.epoch → x.job.rid ≤ y.job.rid) → minE x l = x := by
  intro l
  induction l with
  | nil => intro x _; rfl
  | cons y ys ih =>
    intro x h
    simp only [minE]
    have hc : ¬ (y.job.epoch = x.job.epoch ∧ y.job.rid < x.job.rid) := by
      intro hc
      exact absurd (h y List.mem_cons_self hc.1) (Nat.not_le_of_lt hc.2)
    rw [ite_eq_right hc]
    exact ih x (fun z hz => h z (List.mem_cons_of_mem _ hz))

theorem swapIn_perm (m x : Intent) : ∀ (xs : List Intent), m ∈ xs →
    (x :: xs).Perm (m :: swapIn m x xs) := by
  intro xs
  induction xs with
  | nil => intro h; simp at h
  | cons y ys ih =>
    intro h
    simp only [swapIn]
    by_cases hy : y = m
    · rw [ite_eq_left hy, hy]
      exact List.Perm.swap m x ys
    · rw [ite_eq_right hy]
      have hm : m ∈ ys := (List.mem_cons.mp h).resolve_left (Ne.symm hy)
      exact ((List.Perm.swap y x ys).trans ((ih hm).cons y)).trans (List.Perm.swap m y _)

theorem swapIn_length (m x : Intent) : ∀ (xs : List Intent), (swapIn m x xs).length = xs.length := by
  intro xs
  induction xs with
  | nil => rfl
  | cons y ys ih =>
    simp only [swapIn]
    split <;> simp [ih]

theorem swapIn_epochs (m x : Intent) (h : m.job.epoch = x.job.epoch) : ∀ (xs : List Intent),
    (swapIn m x xs).map (·.job.epoch) = xs.map (·.job.epoch) := by
  intro xs
  induction xs with
  | nil => rfl
  | cons y ys ih =>
    simp only [swapIn]
    by_cases hy : y = m
    · rw [ite_eq_left hy, hy]; simp [h]
    · rw [ite_eq_right hy]; simp [ih]

theorem regroupN_perm : ∀ (n : Nat) (l : List Intent), (regroupN n l).Perm l := by
  intro n
  induction n with
  | zero => intro l; exact List.Perm.refl _
  | succ n ih =>
    intro l
    cases l with
    | nil => exact List.Perm.refl _
    | cons x xs =>
      simp only [regroupN]
      by_cases hm : minE x xs = x
      · rw [ite_eq_left hm]; exact (ih xs).cons x
      · rw [ite_eq_right hm]
        have hmem : minE x xs ∈ xs :=
          (List.mem_cons.mp (minE_spec xs x).2.1).resolve_left hm
        exact ((ih _).cons _).trans (swapIn_perm _ x xs hmem).symm

theorem regroupN_epochs : ∀ (n : Nat) (l : List Intent),
    (regroupN n l).map (·.job.epoch) = l.map (·.job.epoch) := by
  intro n
  induction n with
  | zero => intro l; rfl
  | succ n ih =>
    intro l
    cases l with
    | nil => rfl
    | cons x xs =>
      simp only [regroupN]
      by_cases hm : minE x xs = x
      · rw [ite_eq_left hm]; simp [ih]
      · rw [ite_eq_right hm]
        have he := (minE_spec xs x).1
        simp only [List.map_cons, ih, swapIn_epochs _ x he, he]

theorem regroupN_sorted : ∀ (n : Nat) (l : List Intent), l.length ≤ n →
    EpochSorted (regroupN n l) := by
  intro n
  induction n with
  | zero =>
    intro l hl
    rw [List.length_eq_zero_iff.mp (Nat.le_zero.mp hl)]
    exact List.Pairwise.nil
  | succ n ih =>
    intro l hl
    cases l with
    | nil => exact List.Pairwise.nil
    | cons x xs =>
      have hxs : xs.length ≤ n := Nat.le_of_succ_le_succ hl
      obtain ⟨he, hmem, hmin⟩ := minE_spec xs x
      simp only [regroupN]
      by_cases hm : minE x xs = x
      · rw [ite_eq_left hm]
        refine List.pairwise_cons.mpr ⟨?_, ih xs hxs⟩
        intro z hz hze
        have hz' := (regroupN_perm n xs).mem_iff.mp hz
        rw [← hm]
        exact hmin z (List.mem_cons_of_mem _ hz') (hm ▸ hze.symm)
      · rw [ite_eq_right hm]
        have hin : minE x xs ∈ xs := (List.mem_cons.mp hmem).resolve_left hm
        refine List.pairwise_cons.mpr ⟨?_, ih _ (by rw [swapIn_length]; exact hxs)⟩
        intro z hz hze
        have hz' := (regroupN_perm n _).mem_iff.mp hz
        have hz'' : z ∈ x :: xs :=
          (swapIn_perm _ x xs hin).mem_iff.mpr (List.mem_cons_of_mem _ hz')
        exact hmin z hz'' (hze.symm.trans he)

theorem regroupN_of_sorted : ∀ (n : Nat) (l : List Intent), EpochSorted l → regroupN n l = l := by
  intro n
  induction n with
  | zero => intro l _; rfl
  | succ n ih =>
    intro l hl
    cases l with
    | nil => rfl
    | cons x xs =>
      have hl' := List.pairwise_cons.mp hl
      have hm : minE x xs = x :=
        minE_self xs x (fun y hy hye => hl'.1 y hy hye.symm)
      simp only [regroupN, hm, ite_true]
      rw [ih xs hl'.2]

theorem insertK_perm (x : Intent) : ∀ (l : List Intent), (insertK x l).Perm (x :: l) := by
  intro l
  induction l with
  | nil => exact List.Perm.refl _
  | cons y ys ih =>
    unfold insertK
    split
    · exact List.Perm.refl _
    · exact (ih.cons y).trans (List.Perm.swap x y ys)

theorem loadOrder_perm : ∀ (l : List Intent), (loadOrder l).Perm l := by
  intro l
  induction l with
  | nil => exact List.Perm.refl _
  | cons x xs ih => exact (insertK_perm x _).trans (ih.cons x)

/-- `fixedLoad` holds the same rows. -/
theorem fixedLoad_perm (l : List Intent) : (fixedLoad l).Perm l :=
  (regroupN_perm _ _).trans (loadOrder_perm l)

theorem fixedLoad_sorted : Spec.fixedLoad_sorted := by
  intro l
  exact regroupN_sorted _ _ (Nat.le_refl _)

theorem fixedLoad_epochs : Spec.fixedLoad_epochs := by
  intro l
  exact regroupN_epochs _ _

theorem fixedLoad_agrees : Spec.fixedLoad_agrees := by
  intro l hmono
  exact regroupN_of_sorted _ _ (load_epoch_seq_order l hmono)

/-! ## The order invariant across passes -/

/-- Reloading the rows in another order keeps the invariant. -/
theorem orderInv_perm {rows rows' : List Intent} {A : List Job} (hp : rows'.Perm rows)
    (h : OrderInv rows A) : OrderInv rows' A :=
  ⟨h.1, fun x hx i hi => h.2 x hx i (hp.mem_iff.mp hi)⟩

theorem applyMut_jobs (acc : List Intent) (m : Mut) (hm : ∀ j, m ≠ .put j) :
    (applyMut acc m).map (·.job) = acc.map (·.job) := by
  cases m with
  | put k => exact absurd rfl (hm k)
  | write _ => rfl
  | consume r t =>
    simp only [applyMut, List.map_map]
    apply List.map_congr_left
    intro i _
    by_cases h : i.job.key = r <;> simp [h]

/-- Consumes keep the rows and their order. -/
theorem foldl_jobs : ∀ (ms : List Mut) (acc : List Intent), NoPut ms →
    (ms.foldl applyMut acc).map (·.job) = acc.map (·.job) := by
  intro ms
  induction ms with
  | nil => intro acc _; rfl
  | cons m ms ih =>
    intro acc hnp
    simp only [List.foldl_cons]
    rw [ih _ (fun x hx => hnp x (List.mem_cons_of_mem _ hx)),
      applyMut_jobs acc m (hnp m List.mem_cons_self)]

/-- **One pass of `loopD` keeps the invariant**, for any step, any
streaks and any run results, over rows keyed by receipt id and loaded in
per-process sequence order. -/
theorem loopD_preserves (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR)
    (s : Streaks) (rows : List Intent) (A : List Job) (hnd : (rows.map (·.job.key)).Nodup)
    (hs : EpochSorted rows) (hI : OrderInv rows A) :
    OrderInv (applyLog rows (loopD step rout s [] (pendingOf rows)).log)
      (A ++ (loopD step rout s [] (pendingOf rows)).applied) := by
  obtain ⟨f1, _, fnp, fcons, _⟩ := loopD_facts step rout (pendingOf rows) s []
  have hp1 : EpochSorted (pendingOf rows) := hs.sublist List.filter_sublist
  have hpa : (loopD step rout s [] (pendingOf rows)).applied.Pairwise
      (fun a b => a.agent = b.agent → a.epoch = b.epoch → a.rid ≤ b.rid) := by
    have hpl : ((pendingOf rows).map (·.job)).Pairwise
        (fun a b => a.epoch = b.epoch → a.rid ≤ b.rid) := List.pairwise_map.mpr hp1
    exact (hpl.sublist f1).imp
      (fun {a b} (h : a.epoch = b.epoch → a.rid ≤ b.rid) (_ : a.agent = b.agent) => h)
  refine ⟨List.pairwise_append.mpr ⟨hI.1, hpa, ?_⟩, ?_⟩
  · intro x hx y hy hag hep
    obtain ⟨iy, hiy, rfl⟩ := List.mem_map.mp (f1.subset hy)
    obtain ⟨hiy1, hiy2⟩ := List.mem_filter.mp hiy
    exact hI.2 x hx iy hiy1 (Option.isNone_iff_eq_none.mp hiy2) hag hep
  · intro x hx i' hi' hio' hag hep
    -- The row `i'` left owed comes from a row `i` owed before the pass.
    have hgone : ∀ z : Job,
        (∃ t, Mut.consume z.key t ∈ (loopD step rout s [] (pendingOf rows)).log.flatten) →
        z.key ≠ i'.job.key := by
      intro z hz hzk
      exact foldl_consumed _ rows fnp (Or.inl hz) i' hi' hzk.symm hio'
    obtain ⟨i, hi, hij⟩ := foldl_rows _ rows fnp hi'
    have hio : i.outcome = none := by
      cases ho : i.outcome with
      | none => rfl
      | some t =>
        exfalso
        refine foldl_consumed _ rows fnp (Or.inr ?_) i' hi' rfl hio'
        intro k hk hkk
        have hki : k = i := mem_eq_of_key hnd hk hi (by rw [hkk, hij])
        rw [hki, ho]
        exact Option.some_ne_none t
    rw [hij] at hag hep ⊢
    cases List.mem_append.mp hx with
    | inl hxA => exact hI.2 x hxA i hi hio hag hep
    | inr hxl =>
      have hip : i ∈ pendingOf rows := List.mem_filter.mpr ⟨hi, by simp [hio]⟩
      obtain ⟨ix, hixp, hixj⟩ := List.mem_map.mp (f1.subset hxl)
      obtain ⟨pre, post, hsplit⟩ := List.append_of_mem hixp
      have hndp : ((pendingOf rows).map (·.job)).Nodup := by
        have hk : ((pendingOf rows).map (·.job.key)).Nodup :=
          hnd.sublist (List.filter_sublist.map _)
        unfold List.Nodup at hk ⊢
        rw [List.pairwise_map] at hk ⊢
        exact hk.imp (fun h e => h (by rw [e]))
      have hxl' := hxl
      rw [hsplit] at hip hp1 hxl' hndp
      rw [← hixj] at hxl'
      rcases List.mem_append.mp hip with hpre | hpost
      · exfalso
        have hsettled := loopD_no_overtake step rout pre ix post s [] hndp hxl' i hpre
          (by rw [hixj]; exact hag.symm)
        apply hgone i.job _ (by rw [hij])
        apply fcons
        rw [hsplit]
        exact hsettled
      · cases List.mem_cons.mp hpost with
        | inl e =>
          exfalso
          apply hgone x (fcons x (Or.inl hxl))
          rw [hij, e, hixj]
        | inr hpost' =>
          have hrel := List.rel_of_pairwise_cons (List.pairwise_append.mp hp1).2.1 hpost'
          rw [hixj] at hrel
          exact hrel hep

/-- The same for a whole `replayD` serve (the liveness gate
included). -/
theorem replayD_preserves (step : Bool → RR → Streaks → Step × Streaks) (live : Bool)
    (rout : Job → RR) (rows : List Intent) (A : List Job) (hnd : (rows.map (·.job.key)).Nodup)
    (hs : EpochSorted rows) (hI : OrderInv rows A) :
    OrderInv (applyLog rows (replayD step live rout rows).log)
      (A ++ (replayD step live rout rows).applied) := by
  rcases replayD_cases step live rout rows with h | h | h <;> rw [h]
  · simpa [ROut.empty, applyLog] using hI
  · simpa [ROut.empty, applyLog] using hI
  · exact loopD_preserves step rout ⟨0, 0⟩ rows A hnd hs hI

/-! ## Agents' sub-lists -/

/-- A list whose every agent's sub-list is ordered is ordered for each agent. -/
theorem pairwise_of_agent_filters {P : Job → Job → Prop} :
    ∀ (l : List Job), (∀ a, (l.filter (fun k => k.agent = a)).Pairwise P) →
      l.Pairwise (fun x y => x.agent = y.agent → P x y) := by
  intro l
  induction l with
  | nil => intro _; exact List.Pairwise.nil
  | cons x xs ih =>
    intro h
    refine List.pairwise_cons.mpr ⟨?_, ih ?_⟩
    · intro y hy hxy
      have hx := h x.agent
      simp only [List.filter_cons, decide_true, ite_true] at hx
      exact (List.pairwise_cons.mp hx).1 y (List.mem_filter.mpr ⟨hy, by simp [hxy]⟩)
    · intro a
      have hx := h a
      by_cases hxa : x.agent = a
      · simp only [List.filter_cons, hxa, decide_true, ite_true] at hx
        exact (List.pairwise_cons.mp hx).2
      · simpa [List.filter_cons, hxa] using hx

/-! ## Quarantine, the retry pass and the block reason -/

theorem stepQ_agrees (img : Bool) (rr : RR) (s : Streaks) (h : rr ≠ .unknownFormat) :
    stepQ img rr s = stepFixed img rr s := by
  unfold stepQ
  split
  · exact absurd rfl h
  · rfl

theorem stepQ_skip : Spec.stepQ_skip := by
  intro img rr s h
  by_cases hq : rr = .unknownFormat
  · exact Or.inr hq
  · rw [stepQ_agrees img rr s hq] at h
    exact Or.inl (stepFixed_skip h)

/-- **The block reason is never null while writes are owed** after a serve
that did not stop: `write_queue_replay_blocked` names why. -/
theorem serveQ_block (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent)
    (h : pendingOf (applyLog intents (serveQ live rout rout' s' intents).log) ≠ []) :
    (serveQ live rout rout' s' intents).block ≠ none := by
  unfold serveQ at h ⊢
  by_cases hc : (replayP stepQ live rout rout' s' intents).block = none ∧
      pendingOf (applyLog intents (replayP stepQ live rout rout' s' intents).log) ≠ []
  · rw [ite_eq_left hc]; simp
  · rw [ite_eq_right hc] at h ⊢
    intro hb
    exact hc ⟨hb, h⟩

/-- The serve's writes and applies are the two passes'; only the block
reason is added. -/
theorem serveQ_log (live : Bool) (rout rout' : Job → RR) (s' : Streaks) (intents : List Intent) :
    (serveQ live rout rout' s' intents).log = (replayP stepQ live rout rout' s' intents).log ∧
      (serveQ live rout rout' s' intents).applied =
        (replayP stepQ live rout rout' s' intents).applied := by
  unfold serveQ
  dsimp only
  split <;> exact ⟨rfl, rfl⟩

theorem applyLog_append (rows : List Intent) (l1 l2 : List Batch) :
    applyLog rows (l1 ++ l2) = applyLog (applyLog rows l1) l2 := by
  simp [applyLog, List.flatten_append, List.foldl_append]

theorem applyLog_keys (rows : List Intent) (log : List Batch) (hnp : NoPut log.flatten) :
    (applyLog rows log).map (·.job.key) = rows.map (·.job.key) := by
  have h := foldl_jobs log.flatten rows hnp
  have := congrArg (List.map (·.key)) h
  simpa [List.map_map, applyLog, Function.comp_def] using this

theorem applyLog_sorted (rows : List Intent) (log : List Batch) (hnp : NoPut log.flatten)
    (hs : EpochSorted rows) : EpochSorted (applyLog rows log) := by
  have h := foldl_jobs log.flatten rows hnp
  unfold EpochSorted at hs ⊢
  have hs' : (rows.map (·.job)).Pairwise (fun x y : Job => x.epoch = y.epoch → x.rid ≤ y.rid) :=
    List.pairwise_map.mpr hs
  rw [← show (applyLog rows log).map (·.job) = rows.map (·.job) from h] at hs'
  exact List.pairwise_map.mp hs'

theorem replayP_preserves : Spec.replayP_preserves := by
  intro step live rout rout' s' rows A hnd hs hI
  have h1 := replayD_preserves step live rout rows A hnd hs hI
  unfold replayP
  dsimp only
  cases hb : (replayD step live rout rows).block with
  | some b => exact h1
  | none =>
    dsimp only
    have hnp := (replayD_facts step live rout rows).2.2.1
    have hnd1 : ((applyLog rows (replayD step live rout rows).log).map (·.job.key)).Nodup := by
      rw [applyLog_keys _ _ hnp]; exact hnd
    have h2 := loopD_preserves step rout' s' _ _ hnd1 (applyLog_sorted _ _ hnp hs) h1
    rw [applyLog_append, ← List.append_assoc]
    exact h2

/-- With no step skipping or stopping, `loopD` consumes every row it
is given. -/
theorem loopD_settles (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR)
    (hret : ∀ j s, (step j.image (rout j) s).1 = .applied ∨ (step j.image (rout j) s).1 = .failed) :
    ∀ (l : List Intent) (s : Streaks), ∀ i ∈ l,
      ∃ t, Mut.consume i.job.key t ∈ (loopD step rout s [] l).log.flatten := by
  intro l
  induction l with
  | nil => intro s i hi; simp at hi
  | cons i0 rest ih =>
    intro s i hi
    have hd : i0.job.agent ∉ ([] : List Nat) := by simp
    cases h : step i0.job.image (rout i0.job) s with
    | mk st s1 =>
      have hr := hret i0.job s
      rw [h] at hr
      rcases hr with hr | hr <;> simp only at hr <;> subst hr
      · rw [loopD_applied hd h]
        cases List.mem_cons.mp hi with
        | inl e => exact ⟨.appliedAfterRestart, by rw [e]; simp⟩
        | inr hi' =>
          obtain ⟨t, ht⟩ := ih s1 i hi'
          exact ⟨t, by simp only [List.flatten_cons]; exact List.mem_append_right _ ht⟩
      · rw [loopD_failed hd h]
        cases List.mem_cons.mp hi with
        | inl e => exact ⟨.failed, by rw [e]; simp⟩
        | inr hi' =>
          obtain ⟨t, ht⟩ := ih s1 i hi'
          exact ⟨t, by simp only [List.flatten_cons]; exact List.mem_append_right _ ht⟩

theorem replayP_progress : Spec.replayP_progress := by
  intro step live rout rout' s' rows hb hret
  unfold replayP
  dsimp only
  rw [hb]
  dsimp only
  have hnp1 := (replayD_facts step live rout rows).2.2.1
  let rows1 := applyLog rows (replayD step live rout rows).log
  have hnp2 := (loopD_facts step rout' (pendingOf rows1) s' []).2.2.1
  rw [applyLog_append]
  apply List.filter_eq_nil_iff.mpr
  intro i' hi' hn
  have hio : i'.outcome = none := Option.isNone_iff_eq_none.mp hn
  apply foldl_consumed _ rows1 hnp2 ?_ i' hi' rfl hio
  by_cases hk : ∃ k ∈ rows1, k.job.key = i'.job.key ∧ k.outcome = none
  · obtain ⟨k, hk1, hkk, hko⟩ := hk
    have hkp : k ∈ pendingOf rows1 := List.mem_filter.mpr ⟨hk1, by simp [hko]⟩
    obtain ⟨t, ht⟩ := loopD_settles step rout' hret _ s' k hkp
    exact Or.inl ⟨t, hkk ▸ ht⟩
  · refine Or.inr ?_
    intro k hk' hkk hko
    exact hk ⟨k, hk', hkk, hko⟩

theorem replay_dead_letter_two_restarts : Spec.replay_dead_letter_two_restarts := by
  intro bad good hne
  simp [Flush.replayRecoveryLog, applyLog, applyMut, pendingOf, hne, Ne.symm hne]

end Lambo.Drain
