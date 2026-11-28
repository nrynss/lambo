/- Proofs of `Drain/Spec/Base.lean`. -/

import LamboProofs.Drain.Spec.Base

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

/-- The answer and batch of one job through `spawn_worker`'s arms (`fenced`:
the lease-lost fence), an `EmbedUnavailable` answering `failed` with nothing
appended; the current worker's batches are these (`runW_log_refines`). -/
def workerStep (fenced : Bool) (j : Job) (r : RunResult) : Answer × Batch :=
  if fenced then (.failed, [])
  else
    match r with
    | .ok => (.applied, [.write j.key, .consume j.key .applied])
    | .err e => if e.transient then (.failed, []) else (.failed, [.consume j.key .failed])

/-- No `PutWriteIntent` in a mutation sequence. -/
def NoPut (ms : List Mut) : Prop := ∀ m ∈ ms, ∀ j, m ≠ .put j

/-! ## The receipt store -/

theorem update_cons (k : Key) (e : Entry) (s : RStore) (r : Key) (f : Entry → Entry) :
    update ((k, e) :: s) r f = (if k = r then (k, f e) else (k, e)) :: update s r f := by
  simp only [update, List.map_cons]

theorem lookup_update (s : RStore) (r q : Key) (f : Entry → Entry) :
    lookup (update s r f) q = if q = r then (lookup s q).map f else lookup s q := by
  induction s with
  | nil => simp [lookup, update]
  | cons p s ih =>
    obtain ⟨k, e⟩ := p
    rw [update_cons]
    by_cases hkr : k = r
    · by_cases hkq : k = q
      · subst hkr; subst hkq; simp [lookup]
      · have hqk : q ≠ k := fun h => hkq h.symm
        subst hkr
        simp [lookup, hkq, hqk, ih]
    · by_cases hkq : k = q
      · subst hkq; simp [lookup, hkr]
      · simp [lookup, hkr, hkq, ih]

theorem lookup_update_ne (s : RStore) {r q : Key} (f : Entry → Entry) (h : q ≠ r) :
    lookup (update s r f) q = lookup s q := by
  rw [lookup_update, ite_eq_right h]

theorem mem_update {s : RStore} {r : Key} {f : Entry → Entry} {p : Key × Entry}
    (hp : p ∈ update s r f) :
    ∃ q ∈ s, p = (if q.1 = r then (q.1, f q.2) else q) := by
  unfold update at hp
  obtain ⟨q, hq, he⟩ := List.mem_map.mp hp
  exact ⟨q, hq, he.symm⟩

/-- Admission's entries are all `Pending`. -/
theorem lookup_admit {js : List Job} {j : Job} (hj : j ∈ js) :
    ∃ e, lookup (admitStore js) j.key = some e ∧ e.answer = .pending := by
  induction js with
  | nil => simp at hj
  | cons k js ih =>
    show ∃ e, lookup ((k.key, ⟨.pending, none⟩) :: admitStore js) j.key = some e ∧ _
    by_cases hk : k.key = j.key
    · exact ⟨⟨.pending, none⟩, by simp [lookup, hk], rfl⟩
    · have hj' : j ∈ js := by
        cases List.mem_cons.mp hj with
        | inl h => subst h; exact absurd rfl hk
        | inr h => exact h
      obtain ⟨e, he, ha⟩ := ih hj'
      exact ⟨e, by simp [lookup, hk, he], ha⟩

/-- A worker batch mentions only its own job. -/
theorem workerStep_keys {f : Bool} {j : Job} {r : RunResult} {m : Mut}
    (hm : m ∈ (workerStep f j r).2) : m = .write j.key ∨ ∃ t, m = .consume j.key t := by
  unfold workerStep at hm
  cases f with
  | true => simp at hm
  | false =>
    cases r with
    | ok =>
      simp at hm
      cases hm with
      | inl h => exact Or.inl h
      | inr h => exact Or.inr ⟨_, h⟩
    | err e => cases e <;> simp [Err.transient] at hm <;> exact Or.inr ⟨_, hm⟩

/-- A worker batch that writes also consumes the intent as `applied`, in the
same batch, so the same flush. -/
theorem workerStep_atomic {f : Bool} {j : Job} {r : RunResult} {q : Key}
    (hm : Mut.write q ∈ (workerStep f j r).2) : Mut.consume q .applied ∈ (workerStep f j r).2 := by
  unfold workerStep at hm ⊢
  cases f with
  | true => simp at hm
  | false =>
    cases r with
    | ok =>
      simp at hm
      subst hm
      simp
    | err e => cases e <;> simp [Err.transient] at hm

/-! ## `settle_deferred` -/

theorem deferEntry_durable (now : Int) (e : Entry) :
    deferEntry .durable now e =
      if e.answer = .closeUnflushed then { answer := .intentRecorded, settledAt := some now }
      else (settle e .intentRecorded now).1 := rfl

theorem deferEntry_incomplete (now : Int) (e : Entry) :
    deferEntry .incomplete now e = (settle e .closeUnflushed now).1 := rfl

theorem lookup_mem : ∀ (s : RStore) (q : Key) (x : Entry), lookup s q = some x → (q, x) ∈ s := by
  intro s
  induction s with
  | nil => intro q x h; simp [lookup] at h
  | cons p s ih =>
    intro q x h
    obtain ⟨k, y⟩ := p
    by_cases hk : k = q
    · subst hk; simp [lookup] at h; subst h; exact List.mem_cons_self
    · simp [lookup, hk] at h; exact List.mem_cons_of_mem _ (ih q x h)

theorem deferEntry_settled (flush : CloseFlush) (now : Int) (e : Entry) :
    (deferEntry flush now e).answer.isSettled = true := by
  cases flush with
  | durable =>
    unfold deferEntry
    by_cases h : e.answer = .closeUnflushed
    · rw [ite_eq_left h]; rfl
    · rw [ite_eq_right h]; exact Lambo.Receipts.settle_isSettled e _ now rfl
  | incomplete => exact Lambo.Receipts.settle_isSettled e _ now rfl

/-- On a settled answer other than `close_unflushed`, `settle_deferred` does
nothing: a write the drain applied or failed keeps its answer. -/
theorem deferEntry_of_settled (flush : CloseFlush) (now : Int) (e : Entry)
    (hs : e.answer.isSettled = true) (hc : e.answer ≠ .closeUnflushed) :
    deferEntry flush now e = e := by
  cases flush with
  | durable =>
    unfold deferEntry
    rw [ite_eq_right hc]
    exact congrArg Prod.fst (Lambo.Receipts.settle_settled e _ now hs)
  | incomplete => exact congrArg Prod.fst (Lambo.Receipts.settle_settled e _ now hs)

theorem deferEntry_ne_closeUnflushed (now : Int) (e : Entry) :
    (deferEntry .durable now e).answer ≠ .closeUnflushed := by
  show (if e.answer = .closeUnflushed then { answer := .intentRecorded, settledAt := some now }
    else (settle e .intentRecorded now).1 : Entry).answer ≠ _
  by_cases h : e.answer = .closeUnflushed
  · rw [ite_eq_left h]
    show Answer.intentRecorded ≠ Answer.closeUnflushed
    decide
  · rw [ite_eq_right h]
    by_cases hs : e.answer.isSettled = true
    · rw [Lambo.Receipts.settle_settled e _ now hs]; exact h
    · rw [Lambo.Receipts.settle_unsettled e _ now hs]
      show Answer.intentRecorded ≠ Answer.closeUnflushed
      decide

theorem deferEntry_idem (flush : CloseFlush) (now : Int) (e : Entry) :
    deferEntry flush now (deferEntry flush now e) = deferEntry flush now e := by
  cases flush with
  | durable =>
    exact deferEntry_of_settled _ now _ (deferEntry_settled _ now e)
      (deferEntry_ne_closeUnflushed now e)
  | incomplete =>
    show (settle (deferEntry .incomplete now e) .closeUnflushed now).1 = _
    rw [Lambo.Receipts.settle_settled _ _ now (deferEntry_settled .incomplete now e)]

theorem lookup_settleDeferred (flush : CloseFlush) (now : Int) :
    ∀ (ids : List Key) (s : RStore) (q : Key),
      lookup (settleDeferred flush now ids s) q =
        if q ∈ ids then (lookup s q).map (deferEntry flush now) else lookup s q := by
  intro ids
  induction ids with
  | nil => intro s q; simp [settleDeferred]
  | cons r ids ih =>
    intro s q
    show lookup (settleDeferred flush now ids (update s r (deferEntry flush now))) q = _
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

theorem mem_pendingIds {s : RStore} {p : Key × Entry} (hp : p ∈ s)
    (hu : p.2.answer.isSettled = false) : p.1 ∈ pendingIds s := by
  unfold pendingIds
  exact List.mem_map_of_mem (List.mem_filter.mpr ⟨hp, by simp [hu]⟩)

/-- Two jobs with the same receipt id in a list whose ids are
distinct are the same job. -/
theorem eq_of_key {js : List Job} (hnd : (js.map (·.key)).Nodup) {a b : Job} (ha : a ∈ js)
    (hb : b ∈ js) (h : a.key = b.key) : a = b := by
  induction js with
  | nil => simp at ha
  | cons k js ih =>
    rw [List.map_cons, List.nodup_cons] at hnd
    cases List.mem_cons.mp ha with
    | inl ha' =>
      cases List.mem_cons.mp hb with
      | inl hb' => rw [ha', hb']
      | inr hb' =>
        subst ha'
        exact absurd (h ▸ List.mem_map_of_mem hb') hnd.1
    | inr ha' =>
      cases List.mem_cons.mp hb with
      | inl hb' =>
        subst hb'
        exact absurd (h.symm ▸ List.mem_map_of_mem ha') hnd.1
      | inr hb' => exact ih hnd.2 ha' hb'

/-- A retried close upgrades one entry's `close_unflushed` to
`intent_durable` once its flush lands. -/
theorem retry_upgrades (now now' : Int) (e : Entry) (h : e.answer.isSettled = false) :
    (deferEntry .durable now' (deferEntry .incomplete now e)).answer = .intentRecorded := by
  have h' : ¬ e.answer.isSettled = true := by rw [h]; decide
  rw [deferEntry_incomplete, Lambo.Receipts.settle_unsettled e _ now h', deferEntry_durable,
    ite_eq_left rfl]

/-! ## The intent table -/

theorem admitLog_flatten : ∀ (js : List Job), (admitLog js).flatten = js.map Mut.put := by
  intro js
  induction js with
  | nil => rfl
  | cons j js ih =>
    show [Mut.put j] ++ (admitLog js).flatten = _
    rw [ih]
    rfl

theorem foldl_put : ∀ (js : List Job) (acc : List Intent),
    (js.map Mut.put).foldl applyMut acc = acc ++ js.map (fun j => ⟨j, none⟩) := by
  intro js
  induction js with
  | nil => intro acc; simp
  | cons j js ih =>
    intro acc
    show (js.map Mut.put).foldl applyMut (acc ++ [⟨j, none⟩]) = _
    rw [ih]
    simp

/-- Admission's puts leave one unconsumed row per admitted job. -/
theorem intentsOf_admit (js : List Job) :
    intentsOf (admitLog js).flatten = js.map (fun j => ⟨j, none⟩) := by
  unfold intentsOf
  rw [admitLog_flatten, foldl_put]
  rfl

theorem applyMut_keeps_owed {acc : List Intent} {j : Job} {m : Mut} (h : ⟨j, none⟩ ∈ acc)
    (hm : ∀ t, m ≠ .consume j.key t) : ⟨j, none⟩ ∈ applyMut acc m := by
  cases m with
  | put k => exact List.mem_append_left _ h
  | write _ => exact h
  | consume r t =>
    have hr : j.key ≠ r := fun e => hm t (e ▸ rfl)
    exact List.mem_map.mpr ⟨⟨j, none⟩, h, by simp [hr]⟩

theorem foldl_keeps_owed {j : Job} : ∀ (ms : List Mut) (acc : List Intent),
    ⟨j, none⟩ ∈ acc → (∀ t, Mut.consume j.key t ∉ ms) → ⟨j, none⟩ ∈ ms.foldl applyMut acc := by
  intro ms
  induction ms with
  | nil => intro acc h _; exact h
  | cons m ms ih =>
    intro acc h hc
    apply ih
    · exact applyMut_keeps_owed h (fun t e => hc t (e ▸ List.mem_cons_self))
    · exact fun t ht => hc t (List.mem_cons_of_mem _ ht)

theorem applyMut_consumed {acc : List Intent} {r : Key} {m : Mut} (hm : ∀ j, m ≠ .put j)
    (h : ∀ i ∈ acc, i.job.key = r → i.outcome ≠ none) :
    ∀ i ∈ applyMut acc m, i.job.key = r → i.outcome ≠ none := by
  cases m with
  | put k => exact absurd rfl (hm k)
  | write _ => exact h
  | consume q t =>
    intro i hi hr
    obtain ⟨i0, h0, rfl⟩ := List.mem_map.mp hi
    by_cases hq : i0.job.key = q
    · simp [hq]
    · simp only [hq, ite_false] at hr ⊢
      exact h i0 h0 hr

theorem applyMut_consume_self (acc : List Intent) (r : Key) (t : Tag) :
    ∀ i ∈ applyMut acc (.consume r t), i.job.key = r → i.outcome ≠ none := by
  intro i hi hr
  obtain ⟨i0, _, rfl⟩ := List.mem_map.mp hi
  by_cases hq : i0.job.key = r
  · simp [hq]
  · simp only [hq, ite_false] at hr

/-- **A consume is never undone.** Once a mutation sequence with no puts
carries a consume of `r`, or every row of `r` is already consumed, every row
of `r` is consumed afterwards. -/
theorem foldl_consumed {r : Key} : ∀ (ms : List Mut) (acc : List Intent), NoPut ms →
    ((∃ t, Mut.consume r t ∈ ms) ∨ ∀ i ∈ acc, i.job.key = r → i.outcome ≠ none) →
    ∀ i ∈ ms.foldl applyMut acc, i.job.key = r → i.outcome ≠ none := by
  intro ms
  induction ms with
  | nil =>
    intro acc _ h
    cases h with
    | inl h => obtain ⟨t, ht⟩ := h; simp at ht
    | inr h => exact h
  | cons m ms ih =>
    intro acc hnp h
    have hnp' : NoPut ms := fun x hx => hnp x (List.mem_cons_of_mem _ hx)
    have hm : ∀ j, m ≠ .put j := hnp m List.mem_cons_self
    apply ih (applyMut acc m) hnp'
    cases h with
    | inl h =>
      obtain ⟨t, ht⟩ := h
      cases List.mem_cons.mp ht with
      | inl he => subst he; exact Or.inr (applyMut_consume_self acc r t)
      | inr ht' => exact Or.inl ⟨t, ht'⟩
    | inr h => exact Or.inr (applyMut_consumed hm h)

theorem foldl_rows {i' : Intent} : ∀ (ms : List Mut) (acc : List Intent), NoPut ms →
    i' ∈ ms.foldl applyMut acc → ∃ i ∈ acc, i'.job = i.job := by
  intro ms
  induction ms with
  | nil => intro acc _ h; exact ⟨i', h, rfl⟩
  | cons m ms ih =>
    intro acc hnp h
    obtain ⟨i1, h1, he⟩ := ih (applyMut acc m) (fun x hx => hnp x (List.mem_cons_of_mem _ hx)) h
    cases m with
    | put k => exact absurd rfl (hnp _ List.mem_cons_self k)
    | write _ => exact ⟨i1, h1, he⟩
    | consume q t =>
      obtain ⟨i0, h0, rfl⟩ := List.mem_map.mp h1
      refine ⟨i0, h0, ?_⟩
      rw [he]
      by_cases hq : i0.job.key = q <;> simp [hq]

/-! ## Load order -/

theorem keyLe_iff (a b : Intent) :
    keyLe a b = true ↔ a.job.ms < b.job.ms ∨ (a.job.ms = b.job.ms ∧ a.job.rid ≤ b.job.rid) := by
  simp [keyLe]

theorem keyLe_total (a b : Intent) : keyLe a b = true ∨ keyLe b a = true := by
  rw [keyLe_iff, keyLe_iff]
  omega

theorem keyLe_trans {a b c : Intent} (h1 : keyLe a b = true) (h2 : keyLe b c = true) :
    keyLe a c = true := by
  rw [keyLe_iff] at h1 h2 ⊢
  omega

theorem mem_insertK {x y : Intent} : ∀ {l : List Intent}, y ∈ insertK x l ↔ y = x ∨ y ∈ l := by
  intro l
  induction l with
  | nil => simp [insertK]
  | cons z zs ih =>
    unfold insertK
    by_cases h : keyLe x z = true
    · simp [h]
    · simp only [h]
      simp only [Bool.false_eq_true, ite_false, List.mem_cons, ih]
      constructor
      · intro h'
        rcases h' with h' | h' | h'
        · exact Or.inr (Or.inl h')
        · exact Or.inl h'
        · exact Or.inr (Or.inr h')
      · intro h'
        rcases h' with h' | h' | h'
        · exact Or.inr (Or.inl h')
        · exact Or.inl h'
        · exact Or.inr (Or.inr h')

theorem mem_loadOrder {y : Intent} : ∀ {l : List Intent}, y ∈ loadOrder l ↔ y ∈ l := by
  intro l
  induction l with
  | nil => simp [loadOrder]
  | cons x xs ih =>
    show y ∈ insertK x (loadOrder xs) ↔ _
    rw [mem_insertK, ih, List.mem_cons]

theorem pairwise_insertK (x : Intent) : ∀ (l : List Intent),
    l.Pairwise (fun a b => keyLe a b = true) →
      (insertK x l).Pairwise (fun a b => keyLe a b = true) := by
  intro l
  induction l with
  | nil => intro _; simp [insertK]
  | cons z zs ih =>
    intro h
    rw [List.pairwise_cons] at h
    unfold insertK
    by_cases hxz : keyLe x z = true
    · simp only [hxz, ite_true]
      refine List.pairwise_cons.mpr ⟨?_, List.pairwise_cons.mpr h⟩
      intro w hw
      cases List.mem_cons.mp hw with
      | inl e => rw [e]; exact hxz
      | inr hw' => exact keyLe_trans hxz (h.1 w hw')
    · simp only [hxz, Bool.false_eq_true, ite_false]
      refine List.pairwise_cons.mpr ⟨?_, ih h.2⟩
      intro w hw
      cases mem_insertK.mp hw with
      | inl e =>
        rw [e]
        cases keyLe_total x z with
        | inl h' => exact absurd h' hxz
        | inr h' => exact h'
      | inr hw' => exact h.1 w hw'

/-- The load is sorted by (`issued_ms`, `lane_seq`). -/
theorem loadOrder_sorted : ∀ (l : List Intent),
    (loadOrder l).Pairwise (fun a b => keyLe a b = true) := by
  intro l
  induction l with
  | nil => exact List.Pairwise.nil
  | cons x xs ih => exact pairwise_insertK x _ ih

/-- While no later admission of a process carries an earlier issue time, the
load is that process's sequence order. -/
theorem load_epoch_seq_order (l : List Intent)
    (hmono : ∀ a ∈ l, ∀ b ∈ l, a.job.epoch = b.job.epoch → a.job.rid < b.job.rid →
      a.job.ms ≤ b.job.ms) :
    (loadOrder l).Pairwise (fun a b => a.job.epoch = b.job.epoch → a.job.rid ≤ b.job.rid) := by
  apply List.Pairwise.imp_of_mem _ (loadOrder_sorted l)
  intro a b ha hb hab he
  rw [keyLe_iff] at hab
  have hm := hmono b (mem_loadOrder.mp hb) a (mem_loadOrder.mp ha) he.symm
  omega

/-! ## Admission's batches -/

theorem mem_admit_flatten {all : List Job} {n : Nat} {m : Mut}
    (hm : m ∈ ((admitLog all).take n).flatten) : ∃ j, m = .put j := by
  obtain ⟨b, hb, hmb⟩ := List.mem_flatten.mp hm
  obtain ⟨j, _, rfl⟩ := List.mem_map.mp ((List.take_sublist n _).subset hb)
  exact ⟨j, List.mem_singleton.mp hmb⟩

theorem settleDeferred_all_settled (flush : CloseFlush) (now : Int) :
    ∀ (ids : List Key) (s : RStore), (∀ p ∈ s, p.1 ∉ ids → p.2.answer.isSettled = true) →
      ∀ p ∈ settleDeferred flush now ids s, p.2.answer.isSettled = true := by
  intro ids
  induction ids with
  | nil =>
    intro s h p hp
    exact h p hp (by simp)
  | cons r ids ih =>
    intro s h
    apply ih
    intro p hp hpi
    obtain ⟨q, hq, rfl⟩ := mem_update hp
    by_cases hqr : q.1 = r
    · rw [ite_eq_left hqr]; exact deferEntry_settled flush now q.2
    · rw [ite_eq_right hqr] at hpi ⊢
      apply h q hq
      intro hm
      cases List.mem_cons.mp hm with
      | inl h' => exact hqr h'
      | inr h' => exact hpi h'

/-! ## The replay step's arms -/

theorem replayStep_inputFact : Spec.replayStep_inputFact := by
  intro img e s h
  cases e <;> simp_all [Err.inputFact, replayStep]

theorem unavailable_skips : Spec.unavailable_skips := by
  intro img s h
  simp only [replayStep]
  rw [ite_eq_right (by omega)]

theorem unavailable_stops : Spec.unavailable_stops := by
  intro img s h
  simp only [replayStep]
  rw [ite_eq_left h]

end Lambo.Drain
