/- Proofs of `Drain/Spec/Close.lean`. -/

import LamboProofs.Drain.Spec.Close
import LamboProofs.Drain.Proofs.Worker

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

/-- No batch of the log consumes `k`. -/
def Untouched (k : Key) (log : List Batch) : Prop := ∀ t, Mut.consume k t ∉ log.flatten

/-- What every reachable state of the workers keeps: each settled write's
receipt holds its answer and the log its batch; each live or held write's is
unsettled and untouched; a write not admitted is still `pending` (`fresh`). -/
structure InvW (all : List Job) (st : WS) : Prop where
  settled : ∀ p ∈ st.settled, ansOf st.store p.1.key = some p.2 ∧
    ((p.2 = .applied ∧ [Mut.write p.1.key, .consume p.1.key .applied] ∈ st.log) ∨
     (p.2 = .failed ∧ [Mut.consume p.1.key .failed] ∈ st.log) ∨
     (p.2 = .failed ∧ st.fenced = true ∧ Untouched p.1.key st.log))
  live : ∀ j ∈ st.running ++ st.queue,
    (∃ a, ansOf st.store j.key = some a ∧ a.isSettled = false) ∧ Untouched j.key st.log
  held : ∀ j ∈ st.held, ansOf st.store j.key = some .pendingReplay ∧ Untouched j.key st.log
  fresh : ∀ j ∈ all, j.key ∉ st.admitted.map (·.key) →
    ansOf st.store j.key = some .pending ∧ Untouched j.key st.log

/-! ## The close -/

/-- Every write the workers hold: settled, running, held, queued. -/
def DW (st : WS) : List Job := st.settled.map (·.1) ++ st.running ++ st.held ++ st.queue

/-! ### Distinct receipt ids across the workers' lists -/

theorem nodup_of_agents : ∀ (l : List Job), (∀ b, (ofAgent b l).Nodup) → l.Nodup := by
  intro l
  induction l with
  | nil => intro _; exact List.nodup_nil
  | cons x xs ih =>
    intro h
    have hx := h x.agent
    rw [ofAgent_cons, ite_eq_left rfl, List.nodup_cons] at hx
    refine List.nodup_cons.mpr ⟨?_, ih ?_⟩
    · intro hm; exact hx.1 (List.mem_filter.mpr ⟨hm, by simp⟩)
    · intro b
      have hb := h b
      rw [ofAgent_cons] at hb
      split at hb
      · exact (List.nodup_cons.mp hb).2
      · exact hb

theorem nodup_keys_sub {m : List Job} (hk : (m.map (·.key)).Nodup) :
    ∀ {l : List Job}, l.Nodup → (∀ x ∈ l, x ∈ m) → (l.map (·.key)).Nodup := by
  intro l
  induction l with
  | nil => intro _ _; exact List.nodup_nil
  | cons x xs ih =>
    intro hl hm
    rw [List.nodup_cons] at hl
    rw [List.map_cons, List.nodup_cons]
    refine ⟨?_, ih hl.2 (fun y hy => hm y (List.mem_cons_of_mem _ hy))⟩
    intro hx
    obtain ⟨y, hy, hyx⟩ := List.mem_map.mp hx
    have := eq_of_key hk (hm y (List.mem_cons_of_mem _ hy)) (hm x List.mem_cons_self) hyx
    exact hl.1 (this ▸ hy)

theorem dropFirst_perm (a : Nat) : ∀ (l : List Job) (h : Job) (t : List Job),
    ofAgent a l = h :: t → (h :: dropFirst a l).Perm l := by
  intro l
  induction l with
  | nil => intro h t hl; simp [ofAgent_nil] at hl
  | cons x xs ih =>
    intro h t hl
    rw [ofAgent_cons] at hl
    by_cases hx : x.agent = a
    · simp only [hx, ite_true, List.cons.injEq] at hl
      obtain ⟨rfl, rfl⟩ := hl
      simp only [dropFirst, hx, ite_true]
      exact List.Perm.refl _
    · simp only [hx, ite_false] at hl
      simp only [dropFirst, hx, ite_false]
      exact (List.Perm.swap x h _).trans ((ih h t hl).cons x)

theorem mem_dropFirst {a : Nat} {l : List Job} {h : Job} {t : List Job} (hl : ofAgent a l = h :: t)
    {x : Job} (hx : x ∈ dropFirst a l) : x ∈ l :=
  (dropFirst_perm a l h t hl).mem_iff.mp (List.mem_cons_of_mem _ hx)

theorem head_mem {a : Nat} {l : List Job} {h : Job} {t : List Job} (hl : ofAgent a l = h :: t) :
    h ∈ l :=
  (mem_ofAgent (by rw [hl]; exact List.mem_cons_self)).1

theorem key_ne_dropFirst {a : Nat} {l : List Job} {h : Job} {t : List Job}
    (hl : ofAgent a l = h :: t) (hk : (l.map (·.key)).Nodup) {x : Job} (hx : x ∈ dropFirst a l) :
    x.key ≠ h.key := by
  have hp := (dropFirst_perm a l h t hl).map (·.key)
  have hn := (List.nodup_cons.mp (hp.nodup_iff.mpr hk)).1
  intro he
  exact hn (by show h.key ∈ _; rw [← he]; exact List.mem_map_of_mem hx)

/-- What distinct receipt ids give: the four lists' ids are distinct, within
and across lists, and every write in them was admitted. -/
structure KeysW (st : WS) : Prop where
  run : (st.running.map (·.key)).Nodup
  held : (st.held.map (·.key)).Nodup
  queue : (st.queue.map (·.key)).Nodup
  sr : ∀ p ∈ st.settled, ∀ y ∈ st.running, p.1.key ≠ y.key
  sh : ∀ p ∈ st.settled, ∀ y ∈ st.held, p.1.key ≠ y.key
  sq : ∀ p ∈ st.settled, ∀ y ∈ st.queue, p.1.key ≠ y.key
  rh : ∀ y ∈ st.running, ∀ z ∈ st.held, y.key ≠ z.key
  rq : ∀ y ∈ st.running, ∀ z ∈ st.queue, y.key ≠ z.key
  hq : ∀ y ∈ st.held, ∀ z ∈ st.queue, y.key ≠ z.key
  adm : ∀ x ∈ DW st, x ∈ st.admitted

theorem mem_DW_of_admitted {st : WS} (hO : OrdW st) {j : Job} (hj : j ∈ st.admitted) :
    j ∈ DW st := by
  have h1 : j ∈ ofAgent j.agent st.admitted := List.mem_filter.mpr ⟨hj, by simp⟩
  rw [← (hO j.agent).1] at h1
  simp only [DW] at h1 ⊢
  simp only [List.mem_append] at h1 ⊢
  rcases h1 with ((h | h) | h) | h
  · exact Or.inl (Or.inl (Or.inl (mem_ofAgent h).1))
  · exact Or.inl (Or.inl (Or.inr (mem_ofAgent h).1))
  · exact Or.inl (Or.inr (mem_ofAgent h).1)
  · exact Or.inr (mem_ofAgent h).1

theorem keysW_of (st : WS) (hO : OrdW st) (hnd : (st.admitted.map (·.key)).Nodup) : KeysW st := by
  have hadm : st.admitted.Nodup :=
    List.Pairwise.of_map (·.key) (fun _ _ h e => h (e ▸ rfl)) hnd
  have hsub : ∀ x ∈ DW st, x ∈ st.admitted := by
    intro x hx
    have h1 : x ∈ ofAgent x.agent (DW st) := List.mem_filter.mpr ⟨hx, by simp⟩
    have h2 : ofAgent x.agent (DW st) = ofAgent x.agent st.admitted := by
      simp only [DW, ofAgent_append]; exact (hO x.agent).1
    rw [h2] at h1
    exact (mem_ofAgent h1).1
  have hD : (DW st).Nodup := by
    apply nodup_of_agents
    intro b
    have h2 : ofAgent b (DW st) = ofAgent b st.admitted := by
      simp only [DW, ofAgent_append]; exact (hO b).1
    rw [h2]
    exact hadm.sublist List.filter_sublist
  have hK := nodup_keys_sub hnd hD hsub
  unfold DW at hK
  rw [List.map_append, List.map_append, List.map_append] at hK
  obtain ⟨h1, hQ, x1⟩ := List.nodup_append.mp hK
  obtain ⟨h2, hH, x2⟩ := List.nodup_append.mp h1
  obtain ⟨_, hR, x3⟩ := List.nodup_append.mp h2
  have mS : ∀ p ∈ st.settled, p.1.key ∈ (st.settled.map (·.1)).map (·.key) :=
    fun p hp => List.mem_map_of_mem (List.mem_map_of_mem hp)
  have mk : ∀ {l : List Job} {y : Job}, y ∈ l → y.key ∈ l.map (·.key) :=
    fun hy => List.mem_map_of_mem hy
  refine ⟨hR, hH, hQ, ?_, ?_, ?_, ?_, ?_, ?_, hsub⟩
  · intro p hp y hy; exact x3 _ (mS p hp) _ (mk hy)
  · intro p hp y hy; exact x2 _ (List.mem_append_left _ (mS p hp)) _ (mk hy)
  · intro p hp y hy
    exact x1 _ (List.mem_append_left _ (List.mem_append_left _ (mS p hp))) _ (mk hy)
  · intro y hy z hz; exact x2 _ (List.mem_append_right _ (mk hy)) _ (mk hz)
  · intro y hy z hz
    exact x1 _ (List.mem_append_left _ (List.mem_append_right _ (mk hy))) _ (mk hz)
  · intro y hy z hz; exact x1 _ (List.mem_append_right _ (mk hy)) _ (mk hz)

/-! ### The receipts and the log, as the workers leave them -/

theorem ansOf_settleOne_ne {s : RStore} {k q : Key} (x : Answer) (now : Int) (h : q ≠ k) :
    ansOf (settleOne s k x now) q = ansOf s q := by
  unfold ansOf settleOne; rw [lookup_update_ne s _ h]

theorem ansOf_settleOne_self {s : RStore} {k : Key} {a : Answer} (x : Answer) (now : Int)
    (ha : ansOf s k = some a) (hu : a.isSettled = false) :
    ansOf (settleOne s k x now) k = some x := by
  unfold ansOf at ha ⊢
  unfold settleOne
  rw [lookup_update, ite_eq_left rfl]
  cases hl : lookup s k with
  | none => rw [hl] at ha; simp at ha
  | some e =>
    rw [hl] at ha
    have he : e.answer = a := by simpa using ha
    have hs : ¬ e.answer.isSettled = true := by rw [he, hu]; decide
    simp [Lambo.Receipts.settle_unsettled e x now hs]

theorem untouched_snoc {q k : Key} {log : List Batch} {b : Batch} (hq : q ≠ k)
    (hb : ∀ m ∈ b, m = .write k ∨ ∃ t, m = .consume k t) (hu : Untouched q log) :
    Untouched q (log ++ [b]) := by
  intro t ht
  rw [List.flatten_append, List.mem_append] at ht
  cases ht with
  | inl h => exact hu t h
  | inr h =>
    simp only [List.flatten_cons, List.flatten_nil, List.append_nil] at h
    rcases hb _ h with h' | ⟨t', h'⟩
    · cases h'
    · exact hq (Mut.consume.inj h').1

theorem invW_init (all : List Job) : InvW all (WS.init (admitStore all)) := by
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp; simp [WS.init] at hp
  · intro j hj; simp [WS.init] at hj
  · intro j hj; simp [WS.init] at hj
  · intro j hj _
    obtain ⟨e, he, ha⟩ := lookup_admit hj
    refine ⟨show ansOf (admitStore all) j.key = _ by simp [ansOf, he, ha], ?_⟩
    intro t ht; simp [WS.init] at ht

/-! The moves, one at a time. -/

theorem takeHeld_inv {all : List Job} {st : WS} {a : Nat} {h : Job} {t : List Job}
    (hI : InvW all st) (hh : ofAgent a st.held = h :: t) : InvW all (takeHeld a h st) := by
  refine ⟨hI.settled, ?_, ?_, hI.fresh⟩
  · intro j hj
    simp only [takeHeld, List.mem_append, List.mem_singleton] at hj
    rcases hj with (hj | hj) | hj
    · exact hI.live j (List.mem_append_left _ hj)
    · subst hj
      obtain ⟨h1, h2⟩ := hI.held _ (head_mem hh)
      exact ⟨⟨_, h1, rfl⟩, h2⟩
    · exact hI.live j (List.mem_append_right _ hj)
  · intro j hj; exact hI.held j (mem_dropFirst hh hj)

theorem takeQueued_inv {all : List Job} {st : WS} {a : Nat} {q : Job} {t : List Job}
    (hI : InvW all st) (hq : ofAgent a st.queue = q :: t) : InvW all (takeQueued a q st) := by
  refine ⟨hI.settled, ?_, hI.held, hI.fresh⟩
  intro j hj
  simp only [takeQueued, List.mem_append, List.mem_singleton] at hj
  rcases hj with (hj | hj) | hj
  · exact hI.live j (List.mem_append_left _ hj)
  · subst hj; exact hI.live _ (List.mem_append_right _ (head_mem hq))
  · exact hI.live j (List.mem_append_right _ (mem_dropFirst hq hj))

theorem parkQueued_inv {all : List Job} {st : WS} {a : Nat} {q : Job} {t : List Job} (now : Int)
    (hI : InvW all st) (hK : KeysW st) (hq : ofAgent a st.queue = q :: t) :
    InvW all (parkQueued now a q st) := by
  have hqm := head_mem hq
  have hqA : q.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm q (by simp [DW, hqm]))
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    obtain ⟨h1, h2⟩ := hI.settled p hp
    exact ⟨by simp only [parkQueued]; rw [ansOf_settleOne_ne _ _ (hK.sq p hp q hqm)]; exact h1, h2⟩
  · intro j hj
    simp only [parkQueued, List.mem_append] at hj
    have hne : j.key ≠ q.key := by
      rcases hj with hj | hj
      · exact hK.rq j hj q hqm
      · exact key_ne_dropFirst hq hK.queue hj
    have hj' : j ∈ st.running ++ st.queue := by
      rcases hj with hj | hj
      · exact List.mem_append_left _ hj
      · exact List.mem_append_right _ (mem_dropFirst hq hj)
    obtain ⟨h1, h2⟩ := hI.live j hj'
    exact ⟨by simp only [parkQueued]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro j hj
    simp only [parkQueued, List.mem_append, List.mem_singleton] at hj
    rcases hj with hj | hj
    · obtain ⟨h1, h2⟩ := hI.held j hj
      exact ⟨by simp only [parkQueued]; rw [ansOf_settleOne_ne _ _ (hK.hq j hj q hqm)]; exact h1, h2⟩
    · subst hj
      obtain ⟨⟨x, h1, h2⟩, h3⟩ := hI.live _ (List.mem_append_right _ hqm)
      exact ⟨ansOf_settleOne_self _ now h1 h2, h3⟩
  · intro j hj hn
    obtain ⟨h1, h2⟩ := hI.fresh j hj hn
    have hne : j.key ≠ q.key := fun e => hn (e ▸ hqA)
    exact ⟨by simp only [parkQueued]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩

theorem refuseHeld_inv {all : List Job} {st : WS} {a : Nat} {h : Job} {t : List Job} (now : Int)
    (hI : InvW all st) (hK : KeysW st) (hf : st.fenced = true) (hh : ofAgent a st.held = h :: t) :
    InvW all (refuseHeld now a h st) := by
  have hm := head_mem hh
  have hA : h.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm h (by simp [DW, hm]))
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    simp only [refuseHeld, List.mem_append, List.mem_singleton] at hp
    rcases hp with hp | hp
    · obtain ⟨h1, h2⟩ := hI.settled p hp
      exact ⟨by simp only [refuseHeld]; rw [ansOf_settleOne_ne _ _ (hK.sh p hp h hm)]; exact h1, h2⟩
    · subst hp
      obtain ⟨h1, h2⟩ := hI.held h hm
      exact ⟨ansOf_settleOne_self _ now h1 rfl, Or.inr (Or.inr ⟨rfl, hf, h2⟩)⟩
  · intro j hj
    have hne : j.key ≠ h.key := by
      rcases List.mem_append.mp hj with hj | hj
      · exact hK.rh j hj h hm
      · exact fun e => hK.hq h hm j hj e.symm
    obtain ⟨h1, h2⟩ := hI.live j hj
    exact ⟨by simp only [refuseHeld]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro j hj
    have hj' : j ∈ dropFirst a st.held := hj
    obtain ⟨h1, h2⟩ := hI.held j (mem_dropFirst hh hj')
    exact ⟨by
      simp only [refuseHeld]; rw [ansOf_settleOne_ne _ _ (key_ne_dropFirst hh hK.held hj')]; exact h1,
      h2⟩
  · intro j hj hn
    obtain ⟨h1, h2⟩ := hI.fresh j hj hn
    have hne : j.key ≠ h.key := fun e => hn (e ▸ hA)
    exact ⟨by simp only [refuseHeld]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩

theorem refuseQueued_inv {all : List Job} {st : WS} {a : Nat} {q : Job} {t : List Job} (now : Int)
    (hI : InvW all st) (hK : KeysW st) (hf : st.fenced = true) (hq : ofAgent a st.queue = q :: t) :
    InvW all (refuseQueued now a q st) := by
  have hm := head_mem hq
  have hA : q.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm q (by simp [DW, hm]))
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    simp only [refuseQueued, List.mem_append, List.mem_singleton] at hp
    rcases hp with hp | hp
    · obtain ⟨h1, h2⟩ := hI.settled p hp
      exact ⟨by simp only [refuseQueued]; rw [ansOf_settleOne_ne _ _ (hK.sq p hp q hm)]; exact h1,
        h2⟩
    · subst hp
      obtain ⟨⟨x, h1, h2⟩, h3⟩ := hI.live q (List.mem_append_right _ hm)
      exact ⟨ansOf_settleOne_self _ now h1 h2, Or.inr (Or.inr ⟨rfl, hf, h3⟩)⟩
  · intro j hj
    simp only [refuseQueued, List.mem_append] at hj
    have hne : j.key ≠ q.key := by
      rcases hj with hj | hj
      · exact hK.rq j hj q hm
      · exact key_ne_dropFirst hq hK.queue hj
    have hj' : j ∈ st.running ++ st.queue := by
      rcases hj with hj | hj
      · exact List.mem_append_left _ hj
      · exact List.mem_append_right _ (mem_dropFirst hq hj)
    obtain ⟨h1, h2⟩ := hI.live j hj'
    exact ⟨by simp only [refuseQueued]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro j hj
    obtain ⟨h1, h2⟩ := hI.held j hj
    exact ⟨by simp only [refuseQueued]; rw [ansOf_settleOne_ne _ _ (hK.hq j hj q hm)]; exact h1, h2⟩
  · intro j hj hn
    obtain ⟨h1, h2⟩ := hI.fresh j hj hn
    have hne : j.key ≠ q.key := fun e => hn (e ▸ hA)
    exact ⟨by simp only [refuseQueued]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩

theorem settleRun_inv {all : List Job} {st : WS} {a : Nat} {j : Job} {t : List Job} (now : Int)
    (x : Answer) (b : Batch) (hI : InvW all st) (hK : KeysW st) (hr : ofAgent a st.running = j :: t)
    (hxb : (x = .applied ∧ b = [.write j.key, .consume j.key .applied]) ∨
      (x = .failed ∧ b = [.consume j.key .failed])) :
    InvW all (settleRun now a j x b st) := by
  have hm := head_mem hr
  have hA : j.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm j (by simp [DW, hm]))
  have hb : ∀ m ∈ b, m = .write j.key ∨ ∃ t, m = .consume j.key t := by
    rcases hxb with ⟨_, rfl⟩ | ⟨_, rfl⟩
    · intro m hm'
      simp only [List.mem_cons, List.not_mem_nil, or_false] at hm'
      rcases hm' with rfl | rfl
      · exact Or.inl rfl
      · exact Or.inr ⟨_, rfl⟩
    · intro m hm'
      simp only [List.mem_cons, List.not_mem_nil, or_false] at hm'
      subst hm'; exact Or.inr ⟨_, rfl⟩
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    simp only [settleRun, List.mem_append, List.mem_singleton] at hp
    rcases hp with hp | hp
    · obtain ⟨h1, h2⟩ := hI.settled p hp
      have hne := hK.sr p hp j hm
      refine ⟨by simp only [settleRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, ?_⟩
      simp only [settleRun]
      rcases h2 with ⟨e1, e2⟩ | ⟨e1, e2⟩ | ⟨e1, e2, e3⟩
      · exact Or.inl ⟨e1, List.mem_append_left _ e2⟩
      · exact Or.inr (Or.inl ⟨e1, List.mem_append_left _ e2⟩)
      · exact Or.inr (Or.inr ⟨e1, e2, untouched_snoc hne hb e3⟩)
    · subst hp
      obtain ⟨⟨y, h1, h2⟩, _⟩ := hI.live j (List.mem_append_left _ hm)
      refine ⟨ansOf_settleOne_self _ now h1 h2, ?_⟩
      simp only [settleRun]
      rcases hxb with ⟨rfl, rfl⟩ | ⟨rfl, rfl⟩
      · exact Or.inl ⟨rfl, List.mem_append_right _ (List.mem_singleton.mpr rfl)⟩
      · exact Or.inr (Or.inl ⟨rfl, List.mem_append_right _ (List.mem_singleton.mpr rfl)⟩)
  · intro y hy
    simp only [settleRun, List.mem_append] at hy
    have hne : y.key ≠ j.key := by
      rcases hy with hy | hy
      · exact key_ne_dropFirst hr hK.run hy
      · exact fun e => hK.rq j hm y hy e.symm
    have hy' : y ∈ st.running ++ st.queue := by
      rcases hy with hy | hy
      · exact List.mem_append_left _ (mem_dropFirst hr hy)
      · exact List.mem_append_right _ hy
    obtain ⟨h1, h2⟩ := hI.live y hy'
    exact ⟨by simp only [settleRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1,
      untouched_snoc hne hb h2⟩
  · intro y hy
    have hne : y.key ≠ j.key := fun e => hK.rh j hm y hy e.symm
    obtain ⟨h1, h2⟩ := hI.held y hy
    exact ⟨by simp only [settleRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1,
      untouched_snoc hne hb h2⟩
  · intro y hy hn
    obtain ⟨h1, h2⟩ := hI.fresh y hy hn
    have hne : y.key ≠ j.key := fun e => hn (e ▸ hA)
    exact ⟨by simp only [settleRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1,
      untouched_snoc hne hb h2⟩

theorem holdRun_inv {all : List Job} {st : WS} {a : Nat} {j : Job} {t : List Job} (now : Int)
    (hI : InvW all st) (hK : KeysW st) (hr : ofAgent a st.running = j :: t) :
    InvW all (holdRun now a j st) := by
  have hm := head_mem hr
  have hA : j.key ∈ st.admitted.map (·.key) :=
    List.mem_map_of_mem (hK.adm j (by simp [DW, hm]))
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro p hp
    obtain ⟨h1, h2⟩ := hI.settled p hp
    exact ⟨by simp only [holdRun]; rw [ansOf_settleOne_ne _ _ (hK.sr p hp j hm)]; exact h1, h2⟩
  · intro y hy
    simp only [holdRun, List.mem_append] at hy
    have hne : y.key ≠ j.key := by
      rcases hy with hy | hy
      · exact key_ne_dropFirst hr hK.run hy
      · exact fun e => hK.rq j hm y hy e.symm
    have hy' : y ∈ st.running ++ st.queue := by
      rcases hy with hy | hy
      · exact List.mem_append_left _ (mem_dropFirst hr hy)
      · exact List.mem_append_right _ hy
    obtain ⟨h1, h2⟩ := hI.live y hy'
    exact ⟨by simp only [holdRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro y hy
    simp only [holdRun, List.mem_cons] at hy
    rcases hy with rfl | hy
    · obtain ⟨⟨x, h1, h2⟩, h3⟩ := hI.live y (List.mem_append_left _ hm)
      exact ⟨ansOf_settleOne_self _ now h1 h2, h3⟩
    · have hne : y.key ≠ j.key := fun e => hK.rh j hm y hy e.symm
      obtain ⟨h1, h2⟩ := hI.held y hy
      exact ⟨by simp only [holdRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩
  · intro y hy hn
    obtain ⟨h1, h2⟩ := hI.fresh y hy hn
    have hne : y.key ≠ j.key := fun e => hn (e ▸ hA)
    exact ⟨by simp only [holdRun]; rw [ansOf_settleOne_ne _ _ hne]; exact h1, h2⟩

theorem startW_inv (now : Int) (a : Nat) {all : List Job} {st : WS} (hI : InvW all st)
    (hK : KeysW st) : InvW all (startW now a st) := by
  unfold startW
  by_cases hr : ofAgent a st.running ≠ []
  · rw [ite_eq_left hr]; exact hI
  · rw [ite_eq_right hr]
    cases hh : ofAgent a st.held with
    | nil =>
      cases hq : ofAgent a st.queue with
      | nil => exact hI
      | cons q t =>
        simp only
        split
        · next hf => exact refuseQueued_inv now hI hK hf hq
        · exact takeQueued_inv hI hq
    | cons h t =>
      cases hq : ofAgent a st.queue with
      | nil =>
        simp only
        split
        · next hf => exact refuseHeld_inv now hI hK hf hh
        · split
          · exact hI
          · exact takeHeld_inv hI hh
      | cons q t' =>
        simp only
        split
        · next hf => exact refuseHeld_inv now hI hK hf hh
        · split
          · exact parkQueued_inv now hI hK hq
          · exact takeHeld_inv hI hh

theorem finishW_inv (now : Int) (a : Nat) (r : RunResult) {all : List Job} {st : WS}
    (hI : InvW all st) (hK : KeysW st) : InvW all (finishW now a r st) := by
  unfold finishW
  cases hj : ofAgent a st.running with
  | nil => exact hI
  | cons j t =>
    simp only
    cases r with
    | ok => exact settleRun_inv now _ _ hI hK hj (Or.inl ⟨rfl, rfl⟩)
    | err e =>
      simp only
      split
      · exact holdRun_inv now hI hK hj
      · exact settleRun_inv now _ _ hI hK hj (Or.inr ⟨rfl, rfl⟩)

theorem stepW_admitted (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (ev : Ev) :
    ∃ l, (stepW c sz now st ev).admitted = st.admitted ++ l := by
  cases ev with
  | submit j =>
    simp only [stepW]
    split
    · exact ⟨[j], rfl⟩
    · split <;> exact ⟨[], by simp⟩
  | start a =>
    refine ⟨[], ?_⟩
    simp only [stepW, List.append_nil]
    unfold startW
    split
    · rfl
    · split
      · split
        · simp [refuseHeld]
        · split
          · simp [parkQueued]
          · simp [takeHeld]
      · split
        · simp [refuseHeld]
        · split
          · rfl
          · simp [takeHeld]
      · split
        · simp [refuseQueued]
        · simp [takeQueued]
      · rfl
  | finish a r =>
    refine ⟨[], ?_⟩
    simp only [stepW, List.append_nil]
    unfold finishW
    split
    · rfl
    · split
      · simp [settleRun]
      · split
        · simp [holdRun]
        · simp [settleRun]
  | wake a => exact ⟨[], by simp [stepW]⟩
  | fence => exact ⟨[], by simp [stepW]⟩

theorem runW_admitted (c : Caps) (sz : Job → Nat) (now : Int) :
    ∀ (evs : List Ev) (st : WS), ∃ l, (runW c sz now st evs).admitted = st.admitted ++ l := by
  intro evs
  induction evs with
  | nil => intro st; exact ⟨[], by simp [runW]⟩
  | cons e es ih =>
    intro st
    obtain ⟨l1, h1⟩ := stepW_admitted c sz now st e
    obtain ⟨l2, h2⟩ := ih (stepW c sz now st e)
    exact ⟨l1 ++ l2, by simp only [runW]; rw [h2, h1, List.append_assoc]⟩

theorem stepW_inv (c : Caps) (sz : Job → Nat) (now : Int) {all : List Job} (st : WS) (ev : Ev)
    (hO : OrdW st) (hI : InvW all st)
    (hnd : ((stepW c sz now st ev).admitted.map (·.key)).Nodup)
    (hall : ∀ j ∈ (stepW c sz now st ev).admitted, j ∈ all) :
    InvW all (stepW c sz now st ev) := by
  obtain ⟨l, hl⟩ := stepW_admitted c sz now st ev
  have hnd0 : (st.admitted.map (·.key)).Nodup := by
    rw [hl, List.map_append] at hnd; exact (List.nodup_append.mp hnd).1
  have hK := keysW_of st hO hnd0
  cases ev with
  | submit j =>
    simp only [stepW] at hnd hall ⊢
    split
    · next hfit =>
      rw [ite_eq_left hfit] at hnd hall
      have hjn : j.key ∉ st.admitted.map (·.key) := by
        simp only [List.map_append, List.map_cons, List.map_nil] at hnd
        exact fun hm => (List.nodup_append.mp hnd).2.2 _ hm _ List.mem_cons_self rfl
      have hja : j ∈ all := hall j (List.mem_append_right _ List.mem_cons_self)
      refine ⟨hI.settled, ?_, hI.held, ?_⟩
      · intro y hy
        simp only [List.mem_append, List.mem_singleton] at hy
        rcases hy with hy | hy | hy
        · exact hI.live y (List.mem_append_left _ hy)
        · exact hI.live y (List.mem_append_right _ hy)
        · subst hy
          obtain ⟨h1, h2⟩ := hI.fresh y hja hjn
          exact ⟨⟨_, h1, rfl⟩, h2⟩
      · intro y hy hn
        exact hI.fresh y hy (fun hm => hn (by
          rw [List.map_append]; exact List.mem_append_left _ hm))
    · split
      · exact ⟨hI.settled, hI.live, hI.held, hI.fresh⟩
      · exact hI
  | start a => exact startW_inv now a hI hK
  | finish a r => exact finishW_inv now a r hI hK
  | wake a => exact ⟨hI.settled, hI.live, hI.held, hI.fresh⟩
  | fence =>
    refine ⟨?_, hI.live, hI.held, hI.fresh⟩
    intro p hp
    obtain ⟨h1, h2⟩ := hI.settled p hp
    refine ⟨h1, ?_⟩
    rcases h2 with h | h | ⟨e1, _, e3⟩
    · exact Or.inl h
    · exact Or.inr (Or.inl h)
    · exact Or.inr (Or.inr ⟨e1, rfl, e3⟩)

theorem runW_inv (c : Caps) (sz : Job → Nat) (now : Int) (all : List Job) :
    ∀ (evs : List Ev) (st : WS), OrdW st → InvW all st →
      ((runW c sz now st evs).admitted.map (·.key)).Nodup →
      (∀ j ∈ (runW c sz now st evs).admitted, j ∈ all) → InvW all (runW c sz now st evs) := by
  intro evs
  induction evs with
  | nil => intro st _ hI _ _; exact hI
  | cons e es ih =>
    intro st hO hI hnd hall
    obtain ⟨l, hl⟩ := runW_admitted c sz now es (stepW c sz now st e)
    have hnd' : (runW c sz now (stepW c sz now st e) es).admitted.map (·.key) =
        (stepW c sz now st e).admitted.map (·.key) ++ l.map (·.key) := by
      rw [hl, List.map_append]
    have hnd1 : ((stepW c sz now st e).admitted.map (·.key)).Nodup := by
      have := hnd; simp only [runW] at this; rw [hnd'] at this
      exact (List.nodup_append.mp this).1
    have hall1 : ∀ j ∈ (stepW c sz now st e).admitted, j ∈ all := by
      intro j hj; apply hall; simp only [runW]; rw [hl]; exact List.mem_append_left _ hj
    exact ih _ (stepW_ord c sz now st e hO) (stepW_inv c sz now st e hO hI hnd1 hall1) hnd hall

/-- The current workers' reachable states keep `InvW`. -/
theorem sessionW_inv (c : Caps) (sz : Job → Nat) (now : Int) (all : List Job) (evs : List Ev)
    (hnd : ((runW c sz now (WS.init (admitStore all)) evs).admitted.map (·.key)).Nodup)
    (hall : ∀ j ∈ (runW c sz now (WS.init (admitStore all)) evs).admitted, j ∈ all) :
    InvW all (runW c sz now (WS.init (admitStore all)) evs) :=
  runW_inv c sz now all evs _ (ordW_init _) (invW_init all) hnd hall

/-! ### The close -/

theorem ansOf_closeW_settled {flush : CloseFlush} {now : Int} {st : WS} {k : Key} {x : Answer}
    (h : ansOf st.store k = some x) (hs : x.isSettled = true) (hc : x ≠ .closeUnflushed) :
    ansOf (closeW flush now st) k = some x := by
  unfold ansOf closeW at *
  rw [lookup_settleDeferred]
  cases hl : lookup st.store k with
  | none => rw [hl] at h; simp at h
  | some e =>
    rw [hl] at h
    have he : e.answer = x := by simpa using h
    split
    · simp [deferEntry_of_settled flush now e (he ▸ hs) (he ▸ hc), he]
    · simp [he]

theorem ansOf_closeW_unsettled {flush : CloseFlush} {now : Int} {st : WS} {k : Key} {a : Answer}
    (h : ansOf st.store k = some a) (hu : a.isSettled = false) :
    k ∈ deferredW st ∧ ansOf (closeW flush now st) k = some (closeAnswer flush) := by
  unfold ansOf at h
  cases hl : lookup st.store k with
  | none => rw [hl] at h; simp at h
  | some e =>
    rw [hl] at h
    have he : e.answer = a := by simpa using h
    have hu' : e.answer.isSettled = false := he ▸ hu
    have hm : k ∈ deferredW st := mem_pendingIds (lookup_mem _ _ _ hl) hu'
    refine ⟨hm, ?_⟩
    unfold ansOf closeW
    rw [lookup_settleDeferred, ite_eq_left hm, hl]
    have hs : ¬ e.answer.isSettled = true := by rw [hu']; decide
    cases flush with
    | durable =>
      have hc : e.answer ≠ .closeUnflushed := by
        intro hc; rw [hc] at hu'; exact absurd hu' (by decide)
      simp only [Option.map_some, deferEntry_durable, ite_eq_right hc,
        Lambo.Receipts.settle_unsettled e _ now hs]
      rfl
    | incomplete =>
      simp only [Option.map_some, deferEntry_incomplete, Lambo.Receipts.settle_unsettled e _ now hs]
      rfl

theorem owedW_of {all : List Job} {st : WS} {j : Job} (hj : j ∈ all) (hu : Untouched j.key st.log) :
    OwedW all st j := by
  apply List.mem_filter.mpr
  refine ⟨(fixedLoad_perm _).mem_iff.mpr ?_, rfl⟩
  unfold intentsOf
  rw [List.flatten_append, List.foldl_append, ← intentsOf, intentsOf_admit]
  exact foldl_keeps_owed _ _ (List.mem_map.mpr ⟨j, hj, rfl⟩) hu

/-- After a session and `closeW`, every admitted write is applied, failed and
consumed, failed after the fence and owed, `intent_durable` and owed, or
`close_unflushed`. -/
theorem shutdown_no_silent_lossW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (flush : CloseFlush) (now' : Int) (j : Job) (hj : j ∈ st.admitted) :
    ((j, Answer.applied) ∈ st.settled ∧ ansOf (closeW flush now' st) j.key = some .applied ∧
      [Mut.write j.key, .consume j.key .applied] ∈ st.log) ∨
    ((j, Answer.failed) ∈ st.settled ∧ ansOf (closeW flush now' st) j.key = some .failed ∧
      [Mut.consume j.key .failed] ∈ st.log) ∨
    ((j, Answer.failed) ∈ st.settled ∧ st.fenced = true ∧
      ansOf (closeW flush now' st) j.key = some .failed ∧ OwedW all st j) ∨
    (j ∈ st.running ++ st.held ++ st.queue ∧ flush = .durable ∧
      ansOf (closeW flush now' st) j.key = some .intentRecorded ∧ OwedW all st j) ∨
    (j ∈ st.running ++ st.held ++ st.queue ∧ flush = .incomplete ∧
      ansOf (closeW flush now' st) j.key = some .closeUnflushed) := by
  subst hst
  have hO := runW_ord c sz now evs _ (ordW_init (admitStore all))
  have hI := sessionW_inv c sz now all evs hnd hall
  have hja := hall j hj
  have hD := mem_DW_of_admitted hO hj
  simp only [DW, List.mem_append] at hD
  rcases hD with ((hD | hD) | hD) | hD
  · obtain ⟨p, hp, hpj⟩ := List.mem_map.mp hD
    subst hpj
    obtain ⟨h1, h2⟩ := hI.settled p hp
    rcases h2 with ⟨e1, e2⟩ | ⟨e1, e2⟩ | ⟨e1, e2, e3⟩
    · refine Or.inl ⟨by rw [← e1]; exact hp, ?_, e2⟩
      exact ansOf_closeW_settled (e1 ▸ h1) rfl (by decide)
    · refine Or.inr (Or.inl ⟨by rw [← e1]; exact hp, ?_, e2⟩)
      exact ansOf_closeW_settled (e1 ▸ h1) rfl (by decide)
    · refine Or.inr (Or.inr (Or.inl ⟨by rw [← e1]; exact hp, e2, ?_, owedW_of hja e3⟩))
      exact ansOf_closeW_settled (e1 ▸ h1) rfl (by decide)
  · obtain ⟨⟨a, h1, h2⟩, h3⟩ := hI.live j (List.mem_append_left _ hD)
    have hm : j ∈ (runW c sz now (WS.init (admitStore all)) evs).running ++
        (runW c sz now (WS.init (admitStore all)) evs).held ++
        (runW c sz now (WS.init (admitStore all)) evs).queue := by simp [hD]
    have hc := (ansOf_closeW_unsettled (flush := flush) (now := now') h1 h2).2
    cases flush with
    | durable => exact Or.inr (Or.inr (Or.inr (Or.inl ⟨hm, rfl, hc, owedW_of hja h3⟩)))
    | incomplete => exact Or.inr (Or.inr (Or.inr (Or.inr ⟨hm, rfl, hc⟩)))
  · obtain ⟨h1, h3⟩ := hI.held j hD
    have hm : j ∈ (runW c sz now (WS.init (admitStore all)) evs).running ++
        (runW c sz now (WS.init (admitStore all)) evs).held ++
        (runW c sz now (WS.init (admitStore all)) evs).queue := by simp [hD]
    have hc := (ansOf_closeW_unsettled (flush := flush) (now := now') h1 rfl).2
    cases flush with
    | durable => exact Or.inr (Or.inr (Or.inr (Or.inl ⟨hm, rfl, hc, owedW_of hja h3⟩)))
    | incomplete => exact Or.inr (Or.inr (Or.inr (Or.inr ⟨hm, rfl, hc⟩)))
  · obtain ⟨⟨a, h1, h2⟩, h3⟩ := hI.live j (List.mem_append_right _ hD)
    have hm : j ∈ (runW c sz now (WS.init (admitStore all)) evs).running ++
        (runW c sz now (WS.init (admitStore all)) evs).held ++
        (runW c sz now (WS.init (admitStore all)) evs).queue := by simp [hD]
    have hc := (ansOf_closeW_unsettled (flush := flush) (now := now') h1 h2).2
    cases flush with
    | durable => exact Or.inr (Or.inr (Or.inr (Or.inl ⟨hm, rfl, hc, owedW_of hja h3⟩)))
    | incomplete => exact Or.inr (Or.inr (Or.inr (Or.inr ⟨hm, rfl, hc⟩)))

theorem held_at_closeW : Spec.held_at_closeW := by
  intro c sz now all evs st hst hnd hall j hj flush now'
  subst hst
  have hO := runW_ord c sz now evs _ (ordW_init (admitStore all))
  have hI := sessionW_inv c sz now all evs hnd hall
  obtain ⟨h1, h2⟩ := hI.held j hj
  have hja : j ∈ all := hall j ((keysW_of _ hO hnd).adm j (by simp [DW, hj]))
  exact ⟨h1, owedW_of hja h2, (ansOf_closeW_unsettled h1 rfl).2⟩

theorem outage_holdsW : Spec.outage_holdsW := by
  intro c sz now all evs st hst hnd hall a j t hrun
  have hfin : stepW c sz now st (.finish a (.err .unavailable)) = holdRun now a j st := by
    simp [stepW, finishW, hrun, holdsS]
  rw [hfin]
  subst hst
  have hO := runW_ord c sz now evs _ (ordW_init (admitStore all))
  have hI := sessionW_inv c sz now all evs hnd hall
  have hm := head_mem hrun
  obtain ⟨⟨x, h1, h2⟩, h3⟩ := hI.live j (List.mem_append_left _ hm)
  have hja : j ∈ all := hall j ((keysW_of _ hO hnd).adm j (by simp [DW, hm]))
  exact ⟨List.mem_cons_self, ansOf_settleOne_self _ now h1 h2, rfl, owedW_of hja h3⟩

theorem heldErrW_holds : Spec.heldErrW_holds := by
  intro retried streak e
  cases e <;> simp only [heldErrW] <;> (try split) <;> simp_all [holdsS] <;> decide

theorem lookupTimeout_settlesW : Spec.lookupTimeout_settlesW := by
  intro c sz now all evs st hst hnd hall a j t hrun retried streak hone
  have he : heldErrW retried streak .lookupTimeout = .lookupTimeout := by
    simp only [heldErrW]
    rw [ite_eq_right]
    rintro ⟨h1, h2⟩
    rcases hone with h | h
    · rw [h] at h1; cases h1
    · omega
  have hfin : stepW c sz now st (.finish a (.err (heldErrW retried streak .lookupTimeout))) =
      settleRun now a j .failed [.consume j.key .failed] st := by
    rw [he]; simp [stepW, finishW, hrun, holdsS]
  simp only [hfin]
  subst hst
  have hI := sessionW_inv c sz now all evs hnd hall
  obtain ⟨⟨x, h1, h2⟩, _⟩ := hI.live j (List.mem_append_left _ (head_mem hrun))
  exact ⟨ansOf_settleOne_self _ now h1 h2, List.mem_append_right _ (List.mem_singleton.mpr rfl), rfl⟩

theorem lookupTimeout_run_holdsW : Spec.lookupTimeout_run_holdsW := by
  intro c sz now all evs st hst hnd hall a j t hrun streak hrun3
  have he : heldErrW false streak .lookupTimeout = .unavailable := by
    simp [heldErrW, hrun3]
  simp only [he]
  exact outage_holdsW hst hnd hall a j t hrun

/-- A set-aside write answers `close_unflushed` after an incomplete flush, and
`intent_durable` after a retried `closeW` whose flush completes. -/
theorem closeW_retry_upgradesW {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (j : Job) (hj : j ∈ st.running ++ st.held ++ st.queue) (now1 now2 : Int) :
    ansOf (closeW .incomplete now1 st) j.key = some .closeUnflushed ∧
      ansOf (settleDeferred .durable now2 (deferredW st) (closeW .incomplete now1 st)) j.key =
        some .intentRecorded := by
  subst hst
  have hI := sessionW_inv c sz now all evs hnd hall
  have hu : ∃ a, ansOf (runW c sz now (WS.init (admitStore all)) evs).store j.key = some a ∧
      a.isSettled = false := by
    simp only [List.mem_append] at hj
    rcases hj with (hj | hj) | hj
    · exact (hI.live j (List.mem_append_left _ hj)).1
    · exact ⟨_, (hI.held j hj).1, rfl⟩
    · exact (hI.live j (List.mem_append_right _ hj)).1
  obtain ⟨a, h1, h2⟩ := hu
  obtain ⟨hm, hc⟩ := ansOf_closeW_unsettled (flush := .incomplete) (now := now1) h1 h2
  refine ⟨hc, ?_⟩
  unfold ansOf at h1 ⊢
  cases hl : lookup (runW c sz now (WS.init (admitStore all)) evs).store j.key with
  | none => rw [hl] at h1; simp at h1
  | some e =>
    rw [hl] at h1
    have he : e.answer.isSettled = false := by
      have : e.answer = a := by simpa using h1
      rw [this]; exact h2
    unfold closeW
    rw [lookup_settleDeferred, ite_eq_left hm, lookup_settleDeferred, ite_eq_left hm, hl]
    simp only [Option.map_some]
    rw [retry_upgrades now1 now2 e he]

/-! ### Crash and the serve that follows -/

theorem startW_log (now : Int) (a : Nat) (st : WS) : (startW now a st).log = st.log := by
  unfold startW
  split
  · rfl
  · split
    · split
      · rfl
      · split <;> rfl
    · split
      · rfl
      · split <;> rfl
    · split <;> rfl
    · rfl

theorem stepW_log (c : Caps) (sz : Job → Nat) (now : Int) (st : WS) (ev : Ev) :
    (stepW c sz now st ev).log = st.log ∨
      ∃ j r, (stepW c sz now st ev).log = st.log ++ [(workerStep false j r).2] := by
  cases ev with
  | submit j =>
    simp only [stepW]
    split
    · exact Or.inl rfl
    · split <;> exact Or.inl rfl
  | start a => exact Or.inl (startW_log now a st)
  | finish a r =>
    simp only [stepW]
    unfold finishW
    split
    · exact Or.inl rfl
    · next j _ _ =>
      cases r with
      | ok => exact Or.inr ⟨j, .ok, rfl⟩
      | err e =>
        simp only
        split
        · exact Or.inl rfl
        · exact Or.inr ⟨j, .err .embed, rfl⟩
  | wake a => exact Or.inl rfl
  | fence => exact Or.inl rfl

/-- Every batch the workers append is one `workerStep` (unfenced) appends, so
`workerStep_atomic` and `workerStep_keys` hold of them. -/
theorem runW_log_refines (c : Caps) (sz : Job → Nat) (now : Int) :
    ∀ (evs : List Ev) (st : WS), (∀ b ∈ st.log, ∃ j r, b = (workerStep false j r).2) →
      ∀ b ∈ (runW c sz now st evs).log, ∃ j r, b = (workerStep false j r).2 := by
  intro evs
  induction evs with
  | nil => intro st h; exact h
  | cons e es ih =>
    intro st h
    apply ih
    intro b hb
    rcases stepW_log c sz now st e with hl | ⟨j, r, hl⟩
    · rw [hl] at hb; exact h b hb
    · rw [hl] at hb
      cases List.mem_append.mp hb with
      | inl hb => exact h b hb
      | inr hb => exact ⟨j, r, List.mem_singleton.mp hb⟩

/-- A row still unconsumed after a run of consumes was there, unconsumed,
before it. -/
theorem foldl_pending_mem {i' : Intent} : ∀ (ms : List Mut) (acc : List Intent), NoPut ms →
    i' ∈ ms.foldl applyMut acc → i'.outcome = none → i' ∈ acc := by
  intro ms
  induction ms with
  | nil => intro acc _ h _; exact h
  | cons m ms ih =>
    intro acc hnp h ho
    have h1 := ih (applyMut acc m) (fun x hx => hnp x (List.mem_cons_of_mem _ hx)) h ho
    cases m with
    | put k => exact absurd rfl (hnp _ List.mem_cons_self k)
    | write _ => exact h1
    | consume q t =>
      obtain ⟨i0, h0, he⟩ := List.mem_map.mp h1
      by_cases hq : i0.job.key = q
      · rw [ite_eq_left hq] at he; rw [← he] at ho; simp at ho
      · rw [ite_eq_right hq] at he; rw [← he]; exact h0

theorem serveR_applied_pending : Spec.serveR_applied_pending := by
  intro live rout rout' s' intents x hx
  rw [(serveR_log live rout rout' s' intents).2, (serveQ_log live rout rout' s' intents).2] at hx
  unfold replayP at hx
  dsimp only at hx
  split at hx
  · exact replayD_applied_pending hx
  · rcases List.mem_append.mp hx with h | h
    · exact replayD_applied_pending h
    · have hs := (loopD_facts stepQ rout' _ s' []).1.subset h
      obtain ⟨i', hi', rfl⟩ := List.mem_map.mp hs
      obtain ⟨hi1, hi2⟩ := List.mem_filter.mp hi'
      have hio : i'.outcome = none := Option.isNone_iff_eq_none.mp hi2
      have hnp := (replayD_facts stepQ live rout intents).2.2.1
      exact ⟨i', foldl_pending_mem _ _ hnp hi1 hio, rfl, hio⟩

theorem crash_never_reappliesW : Spec.crash_never_reappliesW := by
  intro c sz now s evs all n r live rout rout' s' hw
  intro x hx hxr
  obtain ⟨i, hi, hix, hio⟩ := serveR_applied_pending hx
  rw [(fixedLoad_perm _).mem_iff] at hi
  have hL := runW_log_refines c sz now evs (WS.init s) (by intro b hb; simp [WS.init] at hb)
  generalize (runW c sz now (WS.init s) evs).log = L at hw hi hL
  rw [List.take_append, List.flatten_append] at hw hi
  have hwB : Mut.write r ∈ (L.take (n - (admitLog all).length)).flatten := by
    cases List.mem_append.mp hw with
    | inl h =>
      obtain ⟨j, hj⟩ := mem_admit_flatten h
      cases hj
    | inr h => exact h
  have hcB : Mut.consume r .applied ∈ (L.take (n - (admitLog all).length)).flatten := by
    obtain ⟨b, hb, hwb⟩ := List.mem_flatten.mp hwB
    obtain ⟨j, rr, rfl⟩ := hL _ ((List.take_sublist _ _).subset hb)
    exact List.mem_flatten.mpr ⟨_, hb, workerStep_atomic hwb⟩
  have hnp : NoPut (L.take (n - (admitLog all).length)).flatten := by
    intro m hm j he
    obtain ⟨b, hb, hmb⟩ := List.mem_flatten.mp hm
    obtain ⟨j', rr, rfl⟩ := hL _ ((List.take_sublist _ _).subset hb)
    rcases workerStep_keys hmb with h | ⟨t, h⟩ <;> rw [h] at he <;> cases he
  unfold intentsOf at hi
  rw [List.foldl_append] at hi
  exact foldl_consumed _ _ hnp (Or.inl ⟨_, hcB⟩) i hi (by rw [hix]; exact hxr) hio

/-! ### Exactly one fate at close -/

/-- The answer the close gives in each case. -/
def FateW.answer : FateW → Answer
  | .applied => .applied
  | .failedConsumed => .failed
  | .failedFenced => .failed
  | .durable => .intentRecorded
  | .unflushed => .closeUnflushed

theorem FateW.holds_answer {all : List Job} {st : WS} {flush : CloseFlush} {now' : Int}
    {j : Job} : ∀ f : FateW, f.holds all st flush now' j →
      ansOf (closeW flush now' st) j.key = some f.answer
  | .applied, h => h.2.1
  | .failedConsumed, h => h.2.1
  | .failedFenced, h => h.2.2.1
  | .durable, h => h.2.2.1
  | .unflushed, h => h.2.2

/-- A write whose `[consume failed]` batch is in the log is not owed: the
consume comes after admission's puts, so every row of its id is consumed. -/
theorem not_owed_of_consumed {all : List Job} {st : WS} {j : Job} {t : Tag}
    (hnp : NoPut st.log.flatten) (hc : [Mut.consume j.key t] ∈ st.log) : ¬ OwedW all st j := by
  intro ho
  have hm := (List.mem_filter.mp ho).1
  rw [(fixedLoad_perm _).mem_iff] at hm
  unfold intentsOf at hm
  rw [List.flatten_append, List.foldl_append] at hm
  have hc' : Mut.consume j.key t ∈ st.log.flatten :=
    List.mem_flatten.mpr ⟨_, hc, List.mem_singleton.mpr rfl⟩
  exact foldl_consumed _ _ hnp (Or.inl ⟨t, hc'⟩) _ hm rfl rfl

theorem shutdown_exactly_oneW : Spec.shutdown_exactly_oneW := by
  intro c sz now all evs st hst hnd hall flush now' j hj
  refine ⟨?_, ?_⟩
  · rcases shutdown_no_silent_lossW hst hnd hall flush now' j hj with h | h | h | h | h
    · exact ⟨.applied, h⟩
    · exact ⟨.failedConsumed, h⟩
    · exact ⟨.failedFenced, h⟩
    · exact ⟨.durable, h⟩
    · exact ⟨.unflushed, h⟩
  · have hnp : NoPut st.log.flatten := by
      subst hst
      intro m hm k he
      obtain ⟨b, hb, hmb⟩ := List.mem_flatten.mp hm
      obtain ⟨j', rr, rfl⟩ := runW_log_refines c sz now evs (WS.init (admitStore all))
        (by intro b hb; simp [WS.init] at hb) b hb
      rcases workerStep_keys hmb with h | ⟨t, h⟩ <;> rw [h] at he <;> cases he
    intro f g hf hg
    have ha := (FateW.holds_answer f hf).symm.trans (FateW.holds_answer g hg)
    cases f <;> cases g <;> simp [FateW.answer] at ha ⊢
    · exact not_owed_of_consumed hnp hf.2.2 hg.2.2.2
    · exact not_owed_of_consumed hnp hg.2.2 hf.2.2.2

/-! ## Dead-lettered batches -/

/-! ### The history's puts -/

theorem keysLanded_cons (b : Batch) (log : List Batch) (f : Landed) (hist : List Landed)
    (g : Landed) :
    keysLanded (b :: log) (f :: hist) g =
      (if f = g then putKeys b else []) ++ keysLanded log hist g := by
  unfold keysLanded
  by_cases h : f = g <;> simp [h]

theorem mem_putKeys {b : Batch} {j : Job} (h : Mut.put j ∈ b) : j.key ∈ putKeys b :=
  List.mem_filterMap.mpr ⟨_, h, rfl⟩

theorem keysLanded_sub : ∀ (log : List Batch) (hist : List Landed) (g : Landed) (k : Key),
    k ∈ keysLanded log hist g → k ∈ log.flatMap putKeys
  | [], _, _, _, h => by simp [keysLanded] at h
  | _ :: _, [], _, _, h => by simp [keysLanded] at h
  | b :: log, f :: hist, g, k, h => by
    rw [keysLanded_cons] at h
    rw [List.flatMap_cons]
    rcases List.mem_append.mp h with h | h
    · split at h
      · exact List.mem_append_left _ h
      · simp at h
    · exact List.mem_append_right _ (keysLanded_sub log hist g k h)

/-- With distinct put ids, no put is both committed and dead-lettered. -/
theorem keysLanded_disjoint : ∀ (log : List Batch) (hist : List Landed),
    (log.flatMap putKeys).Nodup → ∀ k, k ∈ committedPuts log hist → k ∉ droppedPuts log hist
  | [], _, _, _, h, _ => by simp [committedPuts, keysLanded] at h
  | _ :: _, [], _, _, h, _ => by simp [committedPuts, keysLanded] at h
  | b :: log, f :: hist, hnd, k, hc, hd => by
    unfold committedPuts at hc
    unfold droppedPuts at hd
    rw [List.flatMap_cons] at hnd
    obtain ⟨_, hrest, hdisj⟩ := List.nodup_append.mp hnd
    rw [keysLanded_cons] at hc hd
    rcases List.mem_append.mp hc with hc | hc <;> rcases List.mem_append.mp hd with hd | hd
    · split at hc
      · split at hd
        · rename_i h1 h2; rw [h1] at h2; cases h2
        · simp at hd
      · simp at hc
    · split at hc
      · exact hdisj _ hc _ (keysLanded_sub log hist _ k hd) rfl
      · simp at hc
    · split at hd
      · exact hdisj _ hd _ (keysLanded_sub log hist _ k hc) rfl
      · simp at hd
    · exact keysLanded_disjoint log hist hrest k hc hd

theorem landed_of_mem : ∀ (log : List Batch) (hist : List Landed), hist.length = log.length →
    ∀ b ∈ log, ∃ f, (b, f) ∈ log.zip hist
  | [], _, _, _, hb => by simp at hb
  | _ :: _, [], h, _, _ => by simp at h
  | c :: log, f :: hist, h, b, hb => by
    rcases List.mem_cons.mp hb with rfl | hb
    · exact ⟨f, by simp⟩
    · obtain ⟨g, hg⟩ := landed_of_mem log hist (by simpa using h) b hb
      exact ⟨g, by simp [hg]⟩

theorem mem_keysLanded {log : List Batch} {hist : List Landed} {b : Batch} {f : Landed} {j : Job}
    (hb : (b, f) ∈ log.zip hist) (hj : Mut.put j ∈ b) : j.key ∈ keysLanded log hist f :=
  List.mem_flatMap.mpr ⟨(b, f), List.mem_filter.mpr ⟨hb, by simp⟩, mem_putKeys hj⟩

theorem putKeys_admitLog : ∀ (all : List Job), (admitLog all).flatMap putKeys = all.map (·.key)
  | [] => rfl
  | j :: js => by
    have h : admitLog (j :: js) = [Mut.put j] :: admitLog js := rfl
    rw [h, List.flatMap_cons, putKeys_admitLog js]
    rfl

theorem putKeys_noPut (log : List Batch) (h : NoPut log.flatten) : log.flatMap putKeys = [] := by
  apply List.eq_nil_iff_forall_not_mem.mpr
  intro k hk
  obtain ⟨b, hb, hk⟩ := List.mem_flatMap.mp hk
  obtain ⟨m, hm, he⟩ := List.mem_filterMap.mp hk
  cases m with
  | put j => exact h _ (List.mem_flatten.mpr ⟨b, hb, hm⟩) j rfl
  | write _ => simp at he
  | consume _ _ => simp at he

/-- The current worker's batches carry no put. -/
theorem runW_noPut (c : Caps) (sz : Job → Nat) (now : Int) (all : List Job) (evs : List Ev) :
    NoPut (runW c sz now (WS.init (admitStore all)) evs).log.flatten := by
  intro m hm k he
  obtain ⟨b, hb, hmb⟩ := List.mem_flatten.mp hm
  obtain ⟨j', rr, rfl⟩ := runW_log_refines c sz now evs (WS.init (admitStore all))
    (by intro b hb; simp [WS.init] at hb) b hb
  rcases workerStep_keys hmb with h | ⟨t, h⟩ <;> rw [h] at he <;> cases he

/-- Every put of the session's log has its own receipt id. -/
theorem sessLog_puts_nodup {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hkeys : (all.map (·.key)).Nodup) : ((sessLog all st).flatMap putKeys).Nodup := by
  subst hst
  rw [sessLog, List.flatMap_append, putKeys_admitLog,
    putKeys_noPut _ (runW_noPut c sz now all evs), List.append_nil]
  exact hkeys

/-- Once every batch has a fate, an admitted job's put is committed or
dead-lettered. -/
theorem put_landed {all : List Job} {st : WS} {hist : List Landed}
    (hcov : hist.length = (sessLog all st).length) {j : Job} (hj : j ∈ all) :
    j.key ∈ committedPuts (sessLog all st) hist ∨ j.key ∈ droppedPuts (sessLog all st) hist := by
  have hb : [Mut.put j] ∈ sessLog all st :=
    List.mem_append_left _ (List.mem_map.mpr ⟨j, hj, rfl⟩)
  obtain ⟨f, hf⟩ := landed_of_mem _ hist hcov _ hb
  cases f with
  | committed => exact Or.inl (mem_keysLanded hf List.mem_cons_self)
  | deadLettered => exact Or.inr (mem_keysLanded hf List.mem_cons_self)

/-- With every batch landed and distinct put ids, a put is dead-lettered
exactly when it is not committed. -/
theorem dropped_iff_not_committed {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hkeys : (all.map (·.key)).Nodup) {hist : List Landed}
    (hcov : hist.length = (sessLog all st).length) {j : Job} (hj : j ∈ all) :
    j.key ∈ droppedPuts (sessLog all st) hist ↔ j.key ∉ committedPuts (sessLog all st) hist := by
  constructor
  · intro hd hc
    exact keysLanded_disjoint _ hist (sessLog_puts_nodup hst hkeys) _ hc hd
  · intro hc
    rcases put_landed hcov hj with h | h
    · exact absurd h hc
    · exact h

/-! ### The close's receipts -/

theorem lookup_settleDeferredD (flush : CloseFlush) (dropped : List Key) (now : Int) :
    ∀ (ids : List Key) (s : RStore) (q : Key),
      lookup (settleDeferredD flush dropped now ids s) q =
        if q ∈ ids then (lookup s q).map (deferEntry (flushFor flush dropped q) now)
        else lookup s q := by
  intro ids
  induction ids with
  | nil => intro s q; simp [settleDeferredD]
  | cons r ids ih =>
    intro s q
    show lookup (settleDeferredD flush dropped now ids
      (update s r (deferEntry (flushFor flush dropped r) now))) q = _
    rw [ih, lookup_update]
    by_cases hr : q = r
    · subst hr
      by_cases hq : q ∈ ids
      · cases h : lookup s q with
        | none => simp
        | some e => simp [hq, deferEntry_idem]
      · simp [hq]
    · by_cases hq : q ∈ ids
      · simp [hq, hr]
      · simp [hq, hr]

/-- At each receipt `closeD` settles by its own `flushFor`. -/
theorem lookup_settleDeferredD_eq (flush : CloseFlush) (dropped : List Key) (now : Int)
    (ids : List Key) (s : RStore) (q : Key) :
    lookup (settleDeferredD flush dropped now ids s) q =
      lookup (settleDeferred (flushFor flush dropped q) now ids s) q := by
  rw [lookup_settleDeferredD, lookup_settleDeferred]

theorem ansOf_closeD (flush : CloseFlush) (dropped : List Key) (now : Int) (st : WS) (k : Key) :
    ansOf (closeD flush dropped now st) k = ansOf (closeW (flushFor flush dropped k) now st) k := by
  unfold ansOf closeD closeW
  rw [lookup_settleDeferredD_eq]

theorem closeD_agrees : Spec.closeD_agrees := by
  intro flush now st k
  rw [ansOf_closeD]
  simp [flushFor]

theorem setAside_unsettled {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    {j : Job} (hj : j ∈ st.running ++ st.held ++ st.queue) :
    ∃ a, ansOf st.store j.key = some a ∧ a.isSettled = false := by
  subst hst
  have hI := sessionW_inv c sz now all evs hnd hall
  simp only [List.mem_append] at hj
  rcases hj with (hj | hj) | hj
  · exact (hI.live j (List.mem_append_left _ hj)).1
  · exact ⟨_, (hI.held j hj).1, rfl⟩
  · exact (hI.live j (List.mem_append_right _ hj)).1

theorem setAside_mem_all {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    {j : Job} (hj : j ∈ st.running ++ st.held ++ st.queue) : j ∈ all := by
  subst hst
  have hO := runW_ord c sz now evs _ (ordW_init (admitStore all))
  apply hall j
  apply (keysW_of _ hO hnd).adm j
  simp only [DW, List.mem_append] at hj ⊢
  rcases hj with (hj | hj) | hj
  · exact Or.inl (Or.inl (Or.inr hj))
  · exact Or.inl (Or.inr hj)
  · exact Or.inr hj

/-- A set-aside write's answer under `closeD`. -/
theorem closeD_setAside {c : Caps} {sz : Job → Nat} {now : Int} {all : List Job}
    {evs : List Ev} {st : WS} (hst : st = runW c sz now (WS.init (admitStore all)) evs)
    (hnd : (st.admitted.map (·.key)).Nodup) (hall : ∀ j ∈ st.admitted, j ∈ all)
    (flush : CloseFlush) (dropped : List Key) (now' : Int)
    {j : Job} (hj : j ∈ st.running ++ st.held ++ st.queue) :
    ansOf (closeD flush dropped now' st) j.key =
      some (closeAnswer (flushFor flush dropped j.key)) := by
  obtain ⟨a, h1, h2⟩ := setAside_unsettled hst hnd hall hj
  rw [ansOf_closeD]
  exact (ansOf_closeW_unsettled h1 h2).2

/-! ### The rule -/

theorem shutdown_no_silent_lossD : Spec.shutdown_no_silent_lossD := by
  intro c sz now all evs st hst hnd hall flush dropped now' j hj
  rw [ansOf_closeD]
  rcases shutdown_no_silent_lossW hst hnd hall (flushFor flush dropped j.key) now' j hj with
    h | h | h | ⟨h1, h2, h3, h4⟩ | ⟨h1, h2, h3⟩
  · exact Or.inl h
  · exact Or.inr (Or.inl h)
  · exact Or.inr (Or.inr (Or.inl h))
  · unfold flushFor at h2
    split at h2
    · cases h2
    · rename_i hn
      exact Or.inr (Or.inr (Or.inr (Or.inl ⟨h1, h2, hn, h3, h4⟩)))
  · unfold flushFor at h2
    split at h2
    · rename_i hm
      exact Or.inr (Or.inr (Or.inr (Or.inr ⟨h1, Or.inr hm, h3⟩)))
    · exact Or.inr (Or.inr (Or.inr (Or.inr ⟨h1, Or.inl h2, h3⟩)))

theorem closeD_never_vouches_dropped : Spec.closeD_never_vouches_dropped := by
  intro c sz now all evs st hst hnd hall flush hist now' j hj hd
  intro hv
  rcases shutdown_no_silent_lossD hst hnd hall flush (droppedPuts (sessLog all st) hist) now' j hj
    with ⟨_, h, _⟩ | ⟨_, h, _⟩ | ⟨_, _, h, _⟩ | ⟨_, _, hn, _, _⟩ | ⟨_, _, h⟩
  · rw [hv] at h; cases h
  · rw [hv] at h; cases h
  · rw [hv] at h; cases h
  · exact hn hd
  · rw [hv] at h; cases h

theorem closeD_vouches_only_committed : Spec.closeD_vouches_only_committed := by
  intro c sz now all evs st hst hnd hall flush hist now' j hj hcov hv
  rcases put_landed hcov (hall j hj) with h | h
  · exact h
  · exact absurd hv (closeD_never_vouches_dropped hst hnd hall flush hist now' j hj h)

theorem closeD_durable_iff : Spec.closeD_durable_iff := by
  intro c sz now all evs st hst hnd hall hkeys hist now' hcov j hj
  have hiff := dropped_iff_not_committed hst hkeys hcov (setAside_mem_all hst hnd hall hj)
  rw [closeD_setAside hst hnd hall .durable _ now' hj]
  unfold flushFor
  by_cases hd : j.key ∈ droppedPuts (sessLog all st) hist
  · rw [ite_eq_left hd]
    have hc := hiff.mp hd
    exact ⟨⟨fun h => by simp [closeAnswer] at h, fun h => absurd h hc⟩, fun _ => rfl⟩
  · rw [ite_eq_right hd]
    have hc : j.key ∈ committedPuts (sessLog all st) hist := by
      exact Classical.byContradiction (fun hn => hd (hiff.mpr hn))
    exact ⟨⟨fun _ => hc, fun _ => rfl⟩, fun hn => absurd hc hn⟩

theorem closeD_retryD : Spec.closeD_retryD := by
  intro c sz now all evs st hst hnd hall dropped j hj now1 now2
  have hinc : ∀ k, flushFor .incomplete dropped k = .incomplete := by
    intro k; unfold flushFor; split <;> rfl
  obtain ⟨a, h1, h2⟩ := setAside_unsettled hst hnd hall hj
  have hm := (ansOf_closeW_unsettled (flush := .incomplete) (now := now1) h1 h2).1
  have hc1 : ansOf (closeD .incomplete dropped now1 st) j.key = some .closeUnflushed := by
    rw [ansOf_closeD, hinc]; exact (ansOf_closeW_unsettled h1 h2).2
  refine ⟨hc1, ?_⟩
  have hl : lookup (closeD .incomplete dropped now1 st) j.key =
      lookup (closeW .incomplete now1 st) j.key := by
    unfold closeD closeW; rw [lookup_settleDeferredD_eq, hinc]
  unfold ansOf
  rw [lookup_settleDeferredD, ite_eq_left hm, hl]
  unfold flushFor
  by_cases hd : j.key ∈ dropped
  · rw [ite_eq_left hd, ite_eq_left hd]
    unfold ansOf at hc1
    rw [hl] at hc1
    cases hx : lookup (closeW .incomplete now1 st) j.key with
    | none => rw [hx] at hc1; cases hc1
    | some x =>
      rw [hx] at hc1
      have hxa : x.answer = .closeUnflushed := by simpa using hc1
      have hs : x.answer.isSettled = true := by rw [hxa]; rfl
      simp only [Option.map_some, deferEntry_incomplete,
        Lambo.Receipts.settle_settled x _ now2 hs, hxa]
  · rw [ite_eq_right hd, ite_eq_right hd]
    have hr := (closeW_retry_upgradesW hst hnd hall j hj now1 now2).2
    unfold ansOf at hr
    rw [lookup_settleDeferred, ite_eq_left hm] at hr
    exact hr

/-! ## Worker panics and the dead-lettered set -/

/-- The current rule's invariant. -/
structure PInv (s : DS) : Prop where
  /-- A dropped put whose write is not settled is in the set. -/
  sound : ∀ k ∈ s.dropped, k ∉ s.settled → k ∈ s.dl
  /-- The set holds only dropped puts. -/
  sub : ∀ k ∈ s.dl, k ∈ s.dropped
  /-- A consumed write not yet settled is in the record. -/
  cu_of : ∀ k ∈ s.consumed, k ∉ s.settled → k ∈ s.cu
  /-- The record holds only consumed, unsettled writes. -/
  cu_sub : ∀ k ∈ s.cu, k ∈ s.consumed ∧ k ∉ s.settled

theorem pinv_init : PInv DS.init := by
  refine ⟨?_, ?_, ?_, ?_⟩ <;> intro k hk <;> simp [DS.init] at hk

theorem stepP_inv (s : DS) (e : DEv) (hI : PInv s) (he : e.ok s) : PInv (stepP s e) := by
  cases e with
  | consume k =>
    simp only [DEv.ok] at he
    refine ⟨hI.sound, hI.sub, ?_, ?_⟩
    · intro x hx hs
      simp only [stepP, List.mem_cons] at hx hs ⊢
      rcases hx with rfl | hx
      · exact Or.inl rfl
      · exact Or.inr (hI.cu_of x hx hs)
    · intro x hx
      simp only [stepP, List.mem_cons] at hx ⊢
      rcases hx with rfl | hx
      · exact ⟨Or.inl rfl, he⟩
      · exact ⟨Or.inr (hI.cu_sub x hx).1, (hI.cu_sub x hx).2⟩
  | settle k =>
    refine ⟨?_, ?_, ?_, ?_⟩
    · intro x hx hs
      simp only [stepP, List.mem_cons, not_or] at hx hs ⊢
      exact List.mem_filter.mpr ⟨hI.sound x hx hs.2, by simp [hs.1]⟩
    · intro x hx
      simp only [stepP] at hx ⊢
      exact hI.sub x (List.mem_filter.mp hx).1
    · intro x hx hs
      simp only [stepP, List.mem_cons, not_or] at hx hs ⊢
      exact List.mem_filter.mpr ⟨hI.cu_of x hx hs.2, by simp [hs.1]⟩
    · intro x hx
      simp only [stepP, List.mem_cons, not_or] at hx ⊢
      have h1 := List.mem_filter.mp hx
      have h2 := hI.cu_sub x h1.1
      refine ⟨h2.1, ?_, h2.2⟩
      intro hk; subst hk; simp at h1
  | deadLetter ks =>
    refine ⟨?_, ?_, hI.cu_of, hI.cu_sub⟩
    · intro x hx hs
      simp only [stepP, List.mem_append] at hx hs ⊢
      rcases hx with hx | hx
      · refine Or.inl (List.mem_filter.mpr ⟨hx, ?_⟩)
        by_cases hc : x ∈ s.consumed
        · simp [hI.cu_of x hc hs]
        · simp [hc]
      · exact Or.inr (hI.sound x hx hs)
    · intro x hx
      simp only [stepP, List.mem_append] at hx ⊢
      rcases hx with hx | hx
      · exact Or.inl (List.mem_filter.mp hx).1
      · exact Or.inr (hI.sub x hx)

theorem runP_inv : ∀ (evs : List DEv) (s : DS), PInv s → DOk s evs → PInv (evs.foldl stepP s)
  | [], _, hI, _ => hI
  | e :: es, s, hI, ⟨he, hr⟩ => runP_inv es (stepP s e) (stepP_inv s e hI he) hr

theorem settlePrune_exact : Spec.settlePrune_exact := by
  intro evs h k hs
  have hI := runP_inv evs DS.init pinv_init h
  exact ⟨hI.sub k, fun hd => hI.sound k hd hs⟩

theorem settlePrune_unsettled_bounded : Spec.settlePrune_unsettled_bounded := by
  intro evs h k hk
  exact (runP_inv evs DS.init pinv_init h).cu_sub k hk

theorem settlePrune_settled_forgotten : Spec.settlePrune_settled_forgotten := by
  intro s k hc ks
  have h1 : k ∉ (stepP s (.settle k)).dl := by simp [stepP]
  have h2 : k ∉ (stepP s (.settle k)).cu := by simp [stepP]
  refine ⟨h1, h2, ?_⟩
  intro hm
  simp only [stepP, List.mem_append] at hm
  rcases hm with hm | hm
  · have hk := (List.mem_filter.mp hm).2
    simp at hk
    exact hk hc
  · simp at hm

/-! ## The lookup-timeout streak -/

theorem runSince_snoc (evs : List SEv) (e : SEv) :
    runSince (evs ++ [e]) = streakNew (runSince evs) e := by
  unfold runSince
  rw [List.reverse_append]
  cases e <;> simp [streakNew]

theorem streakNew_eq_runSince_rev : ∀ (r : List SEv),
    r.reverse.foldl streakNew 0 = runSince r.reverse
  | [] => rfl
  | e :: r => by
    rw [List.reverse_cons, List.foldl_append, runSince_snoc, ← streakNew_eq_runSince_rev r]
    rfl

/-- **`streakNew` is the documented count**, over every trace. -/
theorem streakNew_eq_runSince (evs : List SEv) : evs.foldl streakNew 0 = runSince evs := by
  have h := streakNew_eq_runSince_rev evs.reverse
  rwa [List.reverse_reverse] at h

theorem heldNew_iff : Spec.heldNew_iff := by
  intro evs
  rw [heldErrW_holds, streakNew_eq_runSince]
  simp

theorem foldl_streakNew_zero : ∀ (mid : List SEv), (∀ e ∈ mid, e ≠ .timeout) →
    mid.foldl streakNew 0 = 0
  | [], _ => rfl
  | e :: mid, h => by
    have he : streakNew 0 e = 0 := by
      cases e with
      | commit => rfl
      | okArm => rfl
      | settleOk => rfl
      | timeout => exact absurd rfl (h _ List.mem_cons_self)
    rw [List.foldl_cons, he]
    exact foldl_streakNew_zero mid (fun x hx => h x (List.mem_cons_of_mem _ hx))

/-- After a commit with no timeout since, the streak is 0. -/
theorem streakNew_after_commit (pre mid : List SEv) (hmid : ∀ e ∈ mid, e ≠ .timeout) :
    (pre ++ SEv.commit :: mid).foldl streakNew 0 = 0 := by
  rw [List.foldl_append, List.foldl_cons]
  exact foldl_streakNew_zero mid hmid

theorem timeout_after_commit_settlesW : Spec.timeout_after_commit_settlesW := by
  intro c sz now all evs st hst hnd hall a j t hrun pre mid hmid
  exact lookupTimeout_settlesW hst hnd hall a j t hrun false _
      (Or.inr (by rw [streakNew_after_commit pre mid hmid]; decide))

end Lambo.Drain
