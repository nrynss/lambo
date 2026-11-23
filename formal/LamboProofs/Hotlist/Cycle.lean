import LamboProofs.Hotlist.Recall

/-
# The daemon cycle that maintains the hot list (`detect_into_hot_list`)

Models the cycle in `src/daemon/mod.rs` (shared by `prime_offline`):
contradictions, conflicts, drift, stale and high-risk hits inserted in that
order, then `retain_conditions`; before #298 (`cycle`: every insert enforces
and the cut comes last) and since (`cycleFixed`, `HotList::sync_cycle`: the
inserts are deferred, evicting nothing, then the cut, then one `enforce`).
Detection is an input: a cycle is given its hits and its condition set.

Results: the old cycle inserts before it retains, so an entry no longer
detected can evict a fresh hit from a full list (`cycle_evicts_fresh_hit`,
#298's finding: only a fresh high-risk, drift or stale hit, only to an
older entry of a strictly higher tier). The fixed cycle keeps the invariant
(`cycleFixed_inv`), keeps only detected entries (`cycleFixed_sound`), evicts
only detected entries ranking at or below every kept one
(`cycleFixed_evicts_only_for_held`), keeps every hit when the condition set
fits (`cycleFixed_keeps_hits`, `cycleFixed_keys_eq_hits`), changes nothing
where the old cycle evicted nothing (`cycleFixed_perm_cycle_of_no_evict`),
and keeps both of the issue's hits (`cycleFixed_keeps_fresh_hit`).

Boundary: `cycle` sends a cycle's contradictions through one-by-one inserts
where the Rust uses one `insert_many`, so `cycle_inv` is about that
substitute; the fair agreement of the two cycles whenever no entry lapses
rests on `insert_many`'s agreement with one-by-one inserts, checked by
`decide` on examples only. Events are not modelled.
-/

namespace Lambo.Hotlist

open Lambo.Lib (eq_of_nodup_map length_le_of_nodup_subset)
variable {ν : Type}

/-! ## Corollaries on reachable lists -/

/-- **Oldest evicted within a tier**: of two entries of one severity and of
the same kind class (both contradictions or neither), the evicted one is the
older. -/
theorem enforce_evicts_oldest_in_tier (max cmax : Nat) (es : List (Entry ν)) (k d : Entry ν)
    (hk : k ∈ (enforce max cmax es).1) (hd : d ∈ (enforce max cmax es).2)
    (hsev : k.cond.severity = d.cond.severity) (hc : isC d = isC k) : d.seq ≤ k.seq := by
  cases enforce_evicts_lowest max cmax es k d hk hd with
  | inl h =>
    unfold Above prioLe at h
    simp only [Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq] at h
    omega
  | inr h => rw [h.1, h.2] at hc; exact absurd hc (by decide)

section Reachable

variable [DecidableEq ν]

/-- What `HotList::insert` evicts ranks at or below everything it keeps,
unless it is a contradiction the budget dropped while the kept entry is of
another kind. -/
theorem insert_evicted_lowest (l : HotList ν) (n : ν) (c : Cond) (p : Payload ν) (d : Entry ν)
    (hd : (insert l n c p).2 = some d) (k : Entry ν) (hk : k ∈ (insert l n c p).1.entries) :
    Above k d ∨ (isC d = true ∧ isC k = false) := by
  have hd' : d ∈ (insertFull l n c p).2 := List.mem_of_getLast? hd
  exact enforce_evicts_lowest _ _ _ k d hk hd'

theorem cycle_inv {l : HotList ν} (hl : Inv l) (hits : List (ν × Cond × Payload ν))
    (held : List (Cond × ν)) : Inv (cycle l hits held) :=
  retain_inv (insertAll_inv hl hits) held

/-- What `upsertAll` keeps: one entry per pair, unique and issued `seq`s. -/
theorem upsertAll_keys : ∀ (l : HotList ν) (es : List (ν × Cond × Payload ν)),
    (l.entries.map key).Nodup → (l.entries.map Entry.seq).Nodup →
      (∀ e ∈ l.entries, e.seq < l.nextSeq) →
      ((upsertAll l es).entries.map key).Nodup ∧ ((upsertAll l es).entries.map Entry.seq).Nodup ∧
        (upsertAll l es).max = l.max ∧ (upsertAll l es).cmax = l.cmax
  | l, [], hk, hs, _ => ⟨hk, hs, rfl, rfl⟩
  | l, (n, c, p) :: rest, hk, hs, hlt => by
    simp only [upsertAll]
    refine upsertAll_keys _ rest (upsert_keys_nodup _ _ hk)
      (upsert_seq_nodup _ _ hs (fun x hx => hlt x hx)) ?_
    intro e he
    show e.seq < l.nextSeq + 1
    cases mem_upsert he with
    | inl h => subst h; simp [stamp]
    | inr h => have := hlt e h; omega

theorem upsertAll_seq_lt : ∀ (l : HotList ν) (es : List (ν × Cond × Payload ν)),
    (∀ e ∈ l.entries, e.seq < l.nextSeq) →
      ∀ e ∈ (upsertAll l es).entries, e.seq < (upsertAll l es).nextSeq
  | l, [], hlt => hlt
  | l, (n, c, p) :: rest, hlt => by
    simp only [upsertAll]
    apply upsertAll_seq_lt
    intro e he
    show e.seq < l.nextSeq + 1
    cases mem_upsert he with
    | inl h => subst h; simp [stamp]
    | inr h => have := hlt e h; omega

/-- The batch insert keeps the invariant. -/
theorem insertMany_inv {l : HotList ν} (hl : Inv l) (es : List (ν × Cond × Payload ν)) :
    Inv (insertMany l es).1 := by
  have hu := upsertAll_keys l es hl.keys_nodup hl.seq_nodup hl.seq_lt
  have hlt := upsertAll_seq_lt l es hl.seq_lt
  refine ⟨enforce_map_nodup key _ _ _ hu.1, enforce_map_nodup Entry.seq _ _ _ hu.2.1, ?_,
    enforce_length_le _ _ _, enforce_countC_le _ _ _⟩
  intro e he
  exact hlt e (mem_enforce _ _ _ he)

end Reachable

/-! ## The cycle since #298: retain, then enforce once -/

section Fixed

variable [DecidableEq ν]

/-- The cycle's condition set as Rust builds it: the `(condition, node)` pair
of every hit (`condition_set`, plus the contradiction hits' pairs). -/
def heldOf (hits : List (ν × Cond × Payload ν)) : List (Cond × ν) :=
  hits.map fun r => (r.2.1, r.1)

/-- How many pairs of a condition set are contradictions. -/
def countCHeld (held : List (Cond × ν)) : Nat :=
  (held.filter fun q => decide (q.1 = .contradiction)).length

/-- One daemon cycle's hot-list step since #298 (`HotList::sync_cycle`, which
`detect_into_hot_list` runs): every hit is inserted with the sort, the budget
and the bound deferred, so each refreshes its pair in place or is pushed,
with the next `seq`, and nothing is evicted (`upsertAll`); the list is cut to the cycle's
condition set; only then are the budget and the bound enforced, once. Returns
the list and what that `enforce` evicted. -/
def cycleFixedFull (l : HotList ν) (hits : List (ν × Cond × Payload ν))
    (held : List (Cond × ν)) : HotList ν × List (Entry ν) :=
  let r := retain held (upsertAll l hits)
  let e := enforce r.max r.cmax r.entries
  ({ r with entries := e.1 }, e.2)

/-- The list after a cycle since #298. -/
def cycleFixed (l : HotList ν) (hits : List (ν × Cond × Payload ν)) (held : List (Cond × ν)) :
    HotList ν :=
  (cycleFixedFull l hits held).1

theorem upsertAll_limits : ∀ (l : HotList ν) (es : List (ν × Cond × Payload ν)),
    (upsertAll l es).max = l.max ∧ (upsertAll l es).cmax = l.cmax
  | _, [] => ⟨rfl, rfl⟩
  | l, (n, c, p) :: rest => by
    simp only [upsertAll]
    exact upsertAll_limits _ rest

theorem insertAll_limits : ∀ (l : HotList ν) (es : List (ν × Cond × Payload ν)),
    (insertAll l es).max = l.max ∧ (insertAll l es).cmax = l.cmax
  | _, [] => ⟨rfl, rfl⟩
  | l, (n, c, p) :: rest => by
    simp only [insertAll]
    exact insertAll_limits _ rest

/-- The keys after a batch of upserts: the old ones plus the inserted ones. -/
theorem key_mem_upsertAll : ∀ (l : HotList ν) (es : List (ν × Cond × Payload ν)) (k : ν × Cond),
    k ∈ (upsertAll l es).entries.map key ↔ k ∈ l.entries.map key ∨ k ∈ es.map reqKey
  | l, [], k => by simp [upsertAll]
  | l, (n, c, p) :: rest, k => by
    simp only [upsertAll]
    rw [key_mem_upsertAll _ rest k]
    show k ∈ (upsert (stamp l n c p) l.entries).map key ∨ _ ↔ _
    rw [key_mem_upsert]
    simp only [List.map_cons, List.mem_cons, reqKey, key, stamp]
    constructor
    · rintro ((h | h) | h)
      · exact Or.inl h
      · exact Or.inr (Or.inl h)
      · exact Or.inr (Or.inr h)
    · rintro (h | h | h)
      · exact Or.inl (Or.inl h)
      · exact Or.inl (Or.inr h)
      · exact Or.inr h

/-- **The invariant holds after a cycle since #298**: one entry per pair,
unique and issued `seq`s, the bound and the contradiction budget. -/
theorem cycleFixed_inv {l : HotList ν} (hl : Inv l) (hits : List (ν × Cond × Payload ν))
    (held : List (Cond × ν)) : Inv (cycleFixed l hits held) := by
  have hu := upsertAll_keys l hits hl.keys_nodup hl.seq_nodup hl.seq_lt
  have hlt := upsertAll_seq_lt l hits hl.seq_lt
  have hs : (retain held (upsertAll l hits)).entries.Sublist (upsertAll l hits).entries :=
    List.filter_sublist
  refine ⟨enforce_map_nodup key _ _ _ (hu.1.sublist (hs.map key)),
    enforce_map_nodup Entry.seq _ _ _ (hu.2.1.sublist (hs.map _)), ?_,
    enforce_length_le _ _ _, enforce_countC_le _ _ _⟩
  intro e he
  exact hlt e (hs.subset (mem_enforce _ _ _ he))

/-- After a cycle since #298 every entry is one the cycle detected. -/
theorem cycleFixed_sound (l : HotList ν) (hits : List (ν × Cond × Payload ν))
    (held : List (Cond × ν)) (e : Entry ν) (he : e ∈ (cycleFixed l hits held).entries) :
    (e.cond, e.node) ∈ held :=
  (mem_retain.mp (mem_enforce _ _ _ he)).2

/-- **Only a detected entry evicts a fresh hit.** Since #298 what the cycle
evicts is itself detected this cycle, and ranks at or below every entry kept,
each of them detected too (except a contradiction dropped by the budget,
which may outrank a kept entry of another kind): an entry the cycle no longer
detects is gone before the budget and the bound are enforced, so it can no
longer push a fresh hit off the list (`cycle_evicts_fresh_hit`). -/
theorem cycleFixed_evicts_only_for_held (l : HotList ν) (hits : List (ν × Cond × Payload ν))
    (held : List (Cond × ν)) (d : Entry ν) (hd : d ∈ (cycleFixedFull l hits held).2) :
    (d.cond, d.node) ∈ held ∧ ∀ k ∈ (cycleFixed l hits held).entries,
      (k.cond, k.node) ∈ held ∧ (Above k d ∨ (isC d = true ∧ isC k = false)) := by
  have hp := enforce_perm (retain held (upsertAll l hits)).max
    (retain held (upsertAll l hits)).cmax (retain held (upsertAll l hits)).entries
  refine ⟨(mem_retain.mp (hp.mem_iff.mp (List.mem_append_right _ hd))).2, ?_⟩
  intro k hk
  exact ⟨cycleFixed_sound l hits held k hk, enforce_evicts_lowest _ _ _ k d hk hd⟩

/-- **Every fresh hit that fits is kept.** When the cycle's condition set fits
within the bound and its contradictions within the budget, the cycle since
#298 evicts nothing, and every hit whose pair is in the set is on the list,
whatever entries the cycle no longer detects were on it before. -/
theorem cycleFixed_keeps_hits {l : HotList ν} (hl : Inv l) (hits : List (ν × Cond × Payload ν))
    (held : List (Cond × ν)) (hlen : held.length ≤ l.max) (hcnt : countCHeld held ≤ l.cmax) :
    (cycleFixedFull l hits held).2 = [] ∧
      ∀ r ∈ hits, (r.2.1, r.1) ∈ held → reqKey r ∈ (cycleFixed l hits held).entries.map key := by
  have hu := upsertAll_keys l hits hl.keys_nodup hl.seq_nodup hl.seq_lt
  have hlim := upsertAll_limits l hits
  have hs : (retain held (upsertAll l hits)).entries.Sublist (upsertAll l hits).entries :=
    List.filter_sublist
  have hnd : ((retain held (upsertAll l hits)).entries.map key).Nodup := hu.1.sublist (hs.map key)
  -- the retained list fits
  have hR : (retain held (upsertAll l hits)).entries.length ≤ l.max := by
    have h := length_le_of_nodup_subset (held.map Prod.swap) hnd (by
      intro k hk
      obtain ⟨e, he, rfl⟩ := List.mem_map.mp hk
      exact List.mem_map.mpr ⟨_, (mem_retain.mp he).2, rfl⟩)
    simp only [List.length_map] at h
    omega
  have hRc : countC (retain held (upsertAll l hits)).entries ≤ l.cmax := by
    have hnd' : (((retain held (upsertAll l hits)).entries.filter isC).map key).Nodup :=
      hnd.sublist (List.filter_sublist.map key)
    have h := length_le_of_nodup_subset
      ((held.filter fun q => decide (q.1 = .contradiction)).map Prod.swap) hnd' (by
        intro k hk
        obtain ⟨e, he, rfl⟩ := List.mem_map.mp hk
        obtain ⟨he, hc⟩ := List.mem_filter.mp he
        refine List.mem_map.mpr ⟨(e.cond, e.node), List.mem_filter.mpr
          ⟨(mem_retain.mp he).2, ?_⟩, rfl⟩
        simpa [isC] using hc)
    simp only [List.length_map] at h
    unfold countC
    unfold countCHeld at hcnt
    omega
  have hno := enforce_noop (retain held (upsertAll l hits)).max
    (retain held (upsertAll l hits)).cmax (retain held (upsertAll l hits)).entries
    (by show _ ≤ (upsertAll l hits).max; rw [hlim.1]; exact hR)
    (by show _ ≤ (upsertAll l hits).cmax; rw [hlim.2]; exact hRc)
  refine ⟨hno.1, ?_⟩
  intro r hr hheld
  obtain ⟨e, he, hek⟩ := List.mem_map.mp
    ((key_mem_upsertAll l hits (reqKey r)).mpr (Or.inr (List.mem_map_of_mem hr)))
  have hn : e.node = r.1 := congrArg Prod.fst hek
  have hc : e.cond = r.2.1 := congrArg Prod.snd hek
  have heR : e ∈ (retain held (upsertAll l hits)).entries :=
    mem_retain.mpr ⟨he, by rw [hn, hc]; exact hheld⟩
  rw [← hek]
  exact List.mem_map_of_mem (hno.2.mem_iff.mpr heR)

/-- **The list equals the cycle's fresh hits, up to the bound**
(`src/daemon/hotlist.rs`, module doc): with the condition set the daemon
builds (every hit's pair) and the hits within the bound and the budget, the
pairs on the list after a cycle since #298 are exactly the hits' pairs. -/
theorem cycleFixed_keys_eq_hits {l : HotList ν} (hl : Inv l)
    (hits : List (ν × Cond × Payload ν)) (hlen : hits.length ≤ l.max)
    (hcnt : countCHeld (heldOf hits) ≤ l.cmax) (k : ν × Cond) :
    k ∈ (cycleFixed l hits (heldOf hits)).entries.map key ↔ k ∈ hits.map reqKey := by
  constructor
  · intro hk
    obtain ⟨e, he, rfl⟩ := List.mem_map.mp hk
    obtain ⟨r, hr, hre⟩ := List.mem_map.mp (cycleFixed_sound l hits (heldOf hits) e he)
    refine List.mem_map.mpr ⟨r, hr, ?_⟩
    simp only [Prod.mk.injEq] at hre
    simp [reqKey, key, hre.1, hre.2]
  · intro hk
    obtain ⟨r, hr, rfl⟩ := List.mem_map.mp hk
    exact (cycleFixed_keeps_hits hl hits (heldOf hits) (by simpa [heldOf] using hlen) hcnt).2 r hr
      (List.mem_map_of_mem hr)

theorem upsert_perm_congr (e : Entry ν) {l₁ l₂ : List (Entry ν)} (hp : l₁.Perm l₂)
    (hn : (l₁.map key).Nodup) : (upsert e l₁).Perm (upsert e l₂) := by
  by_cases hm : key e ∈ l₁.map key
  · have hm₂ : key e ∈ l₂.map key := (hp.map key).mem_iff.mp hm
    obtain ⟨x, hx, hxk, h1⟩ := upsert_perm_of_mem e l₁ hm
    obtain ⟨y, hy, hyk, h2⟩ := upsert_perm_of_mem e l₂ hm₂
    have hxy : x = y := eq_of_nodup_map key hn hx (hp.mem_iff.mpr hy) (hxk.trans hyk.symm)
    subst hxy
    exact h1.trans (((hp.erase x).cons e).trans h2.symm)
  · have hm₂ : key e ∉ l₂.map key := fun h => hm ((hp.map key).mem_iff.mpr h)
    rw [upsert_of_not_mem e l₁ hm, upsert_of_not_mem e l₂ hm₂]
    exact hp.append_right [e]

/-- While nothing is evicted, single inserts leave the entries a batch of
upserts leaves (up to order). -/
theorem insertAll_perm_upsertAll : ∀ (l u : HotList ν) (es : List (ν × Cond × Payload ν)),
    Inv l → u.nextSeq = l.nextSeq → l.entries.Perm u.entries → insertAllEvicted l es = [] →
      (insertAll l es).entries.Perm (upsertAll u es).entries
  | _, _, [], _, _, hp, _ => hp
  | l, u, (n, c, p) :: rest, hl, hs, hp, hev => by
    simp only [insertAllEvicted, List.append_eq_nil_iff] at hev
    simp only [insertAll, upsertAll]
    refine insertAll_perm_upsertAll _ _ rest (insert_inv hl n c p)
      (by show u.nextSeq + 1 = l.nextSeq + 1; rw [hs]) ?_ hev.2
    have he := enforce_perm l.max l.cmax (upsert (stamp l n c p) l.entries)
    have h0 : (enforce l.max l.cmax (upsert (stamp l n c p) l.entries)).2 = [] := hev.1
    rw [h0, List.append_nil] at he
    have hst : stamp u n c p = stamp l n c p := by simp [stamp, hs]
    show (enforce l.max l.cmax (upsert (stamp l n c p) l.entries)).1.Perm
      (upsert (stamp u n c p) u.entries)
    rw [hst]
    exact he.trans (upsert_perm_congr _ hp hl.keys_nodup)

/-- **Agreement where nothing was lost.** When the cycle before #298 evicts
nothing while inserting, the cycle since #298 evicts nothing either and
leaves the same entries. -/
theorem cycleFixed_perm_cycle_of_no_evict {l : HotList ν} (hl : Inv l)
    (hits : List (ν × Cond × Payload ν)) (held : List (Cond × ν))
    (h : insertAllEvicted l hits = []) :
    (cycleFixedFull l hits held).2 = [] ∧
      (cycleFixed l hits held).entries.Perm (cycle l hits held).entries := by
  have hp := insertAll_perm_upsertAll l l hits hl rfl (List.Perm.refl _) h
  have hf : (retain held (upsertAll l hits)).entries.Perm (cycle l hits held).entries :=
    (hp.filter _).symm
  have hc := cycle_inv hl hits held
  have hlim := upsertAll_limits l hits
  have hlim' := insertAll_limits l hits
  have hno := enforce_noop (retain held (upsertAll l hits)).max
    (retain held (upsertAll l hits)).cmax (retain held (upsertAll l hits)).entries
    (by
      show _ ≤ (upsertAll l hits).max
      rw [hf.length_eq, hlim.1, ← hlim'.1]
      exact hc.len_le)
    (by
      show _ ≤ (upsertAll l hits).cmax
      rw [countC_perm hf, hlim.2, ← hlim'.2]
      exact hc.countC_le)
  exact ⟨hno.1, hno.2.trans hf⟩

end Fixed

/-! ## Concrete examples (nodes are `Nat`) -/

section Examples

/-- An insert request with a payload of the condition's kind. -/
def req (n : Nat) : Cond → Nat × Cond × Payload Nat
  | .conflict => (n, .conflict, .conflict 5)
  | .highRisk => (n, .highRisk, .highRisk)
  | .drift => (n, .drift, .drift 4)
  | .stale => (n, .stale, .stale 60)
  | .contradiction => (n, .contradiction, .contradiction 0 0)

def nodes (l : HotList Nat) : List Nat := l.entries.map (·.node)

def keys (l : HotList Nat) : List (Nat × Cond) := l.entries.map key

def reqs (ns : List Nat) (c : Cond) : List (Nat × Cond × Payload Nat) := ns.map (req · c)

/-- `default_contradiction_max`: `the_default_contradiction_budget_is_half_the_bound`. -/
example : (new : HotList Nat).cmax = 500 ∧ defaultCmax 7 = 3 ∧ defaultCmax 0 = 0 := by decide

/-- `overflow_evicts_lowest_priority_and_keeps_bound`: same condition, so the
oldest goes; the list is `[n5, n4, n3]`. -/
example :
    let l3 := insertAll (withMax 3) (reqs [1, 2, 3] .conflict)
    let r4 := insert l3 4 .conflict (.conflict 5)
    let r5 := insert r4.1 5 .conflict (.conflict 5)
    (insertAllEvicted (withMax 3) (reqs [1, 2, 3] .conflict)).length = 0 ∧
      r4.2.map (·.node) = some 1 ∧ r5.2.map (·.node) = some 2 ∧ nodes r5.1 = [5, 4, 3] := by
  decide

/-- `zero_bound_evicts_every_insert`. -/
example :
    let r1 := insert (withMax 0) 1 .conflict (.conflict 5)
    let r2 := insert r1.1 2 .conflict (.conflict 5)
    r1.2.map (·.node) = some 1 ∧ r2.2.map (·.node) = some 2 ∧ r2.1.entries = [] := by decide

/-- `bound_enforced_under_mixed_severity_overflow`: the least severe goes. -/
example :
    let l := insertAll (withMax 3) [req 1 .conflict, req 2 .drift, req 3 .stale]
    (insert l 4 .stale (.stale 60)).2.map (·.cond) = some .stale := by decide

/-- `severity_outranks_recency`. -/
example :
    keys (insertAll new [req 1 .stale, req 2 .drift, req 3 .conflict])
      = [(3, .conflict), (2, .drift), (1, .stale)] := by decide

/-- `refresh_replaces_payload_and_restores_recency`: no duplicate, no eviction,
the new payload, and the refreshed entry now outranks the later `n2`. -/
example :
    let l := insertAll new [(1, .conflict, .conflict 30), req 2 .conflict]
    let r := insertFull l 1 .conflict (.conflict 11)
    r.2 = [] ∧ nodes r.1 = [1, 2] ∧ (r.1.entries.head?.map (·.payload)) = some (.conflict 11) := by
  decide

/-- `a_flood_of_contradictions_does_not_evict_the_other_kinds` (20
contradictions instead of 100): five contradictions stay, the latest
inserted; drift, stale and high-risk stay; only contradictions are evicted. -/
example :
    let l := insertAll (withMax 10) [req 1 .drift, req 2 .stale, req 3 .highRisk]
    let r := insertMany l (reqs (List.range' 100 20) .contradiction)
    countC r.1.entries = 5 ∧ r.2.length = 15 ∧ r.2.all isC ∧
      (r.1.entries.filter isC).map (·.node) = [119, 118, 117, 116, 115] ∧
      (1, .drift) ∈ keys r.1 ∧ (2, .stale) ∈ keys r.1 ∧ (3, .highRisk) ∈ keys r.1 ∧
      (insert r.1 500 .contradiction (.contradiction 0 0)).2.map (·.node) = some 115 := by
  decide

/-- `under_the_budget_nothing_changes`: conflicts are not counted against the
budget; over the bound the lowest goes as before. -/
example :
    let l := insertAll (withMax 6) (reqs [1, 2, 3] .contradiction ++ reqs [10, 11, 12] .conflict)
    insertAllEvicted (withMax 6) (reqs [1, 2, 3] .contradiction ++ reqs [10, 11, 12] .conflict)
        = [] ∧ (insert l 20 .conflict (.conflict 5)).2.map (·.node) = some 1 := by
  decide

/-- `other_kinds_can_use_the_whole_list`. -/
example :
    let l := insertAll (withMax 4) (reqs [1, 2, 3, 4] .drift)
    (insert l 9 .contradiction (.contradiction 0 0)).2.map (·.node) = some 1 := by decide

/-- `a_budget_at_the_bound_reserves_nothing`. -/
example :
    let r := insertMany (insertAll (withBudget 3 3) [req 1 .drift])
      (reqs [10, 11, 12] .contradiction)
    countC r.1.entries = 3 ∧ (1, .drift) ∉ keys r.1 := by decide

/-- The daemon inserts contradictions before conflicts, so on a full list the
contradiction goes first (api.mdx). -/
example :
    keys (insertAll (withBudget 1 1) [req 1 .contradiction, req 2 .conflict]) = [(2, .conflict)] := by
  decide

/-- The plan of `insert_many_matches_inserting_one_by_one` (shortened), with
repeated `(node, condition)` keys inside the batch and a refresh of the seeded
`(99, drift)` entry, so the refresh-in-place path is exercised: the batch
leaves exactly the list one-by-one inserts leave (entries, payloads, `seq`s),
for several bounds and budgets. -/
def plan : List (Nat × Cond × Payload Nat) :=
  (List.range 16).map (fun i =>
    req ((i * 7) % 13) ([Cond.contradiction, .stale, .conflict, .drift, .highRisk].getD ((i * 3) % 5) .drift))
  ++ [(0, .contradiction, .contradiction 5 3), (7, .drift, .drift 9), (99, .drift, .drift 7),
      (1, .stale, .stale 90), (0, .contradiction, .contradiction 6 4), (12, .conflict, .conflict 2)]

example :
    [(0, 0), (3, 0), (3, 1), (3, 3), (8, 2), (8, 4), (8, 8), (20, 3), (20, 10), (100, 50)].all
      fun (m, b) =>
        let l := insertAll (withBudget m b) [req 99 .drift]
        (insertMany l plan).1 = insertAll l plan := by
  decide +kernel

/-- The plan repeats keys, and with room to spare the seeded entry is refreshed
in place with its new payload. -/
example :
    let l := insertAll (withBudget 100 50) [req 99 .drift]
    (plan.map reqKey).length = 22 ∧ (keys (insertMany l plan).1).length = 18 ∧
      ((insertMany l plan).1.entries.find? (·.node = 99)).map (·.payload) = some (.drift 7) := by
  decide

/-- **Order matters once the list is full**: two drift entries into a list of
one keep whichever came last. -/
theorem order_matters_when_full :
    keys (insertAll (withMax 1) [req 1 .drift, req 2 .drift]) = [(2, .drift)] ∧
      keys (insertAll (withMax 1) [req 2 .drift, req 1 .drift]) = [(1, .drift)] := by
  decide

/-- **The cycle inserts before it retains** (`detect_into_hot_list`), so an
entry that is no longer detected can push a fresh hit off a full list, and is
then dropped itself: after cycle 2 the list (bound 2) holds one entry, though
both fresh hits fit. The drift entry is lost to the old conflict, a higher tier;
an old entry of its own tier could not evict it, since the fresh one has the
newer `seq`. The next cycle restores it (no further entry lapses there). -/
theorem cycle_evicts_fresh_hit :
    let l1 := cycle (withMax 2) [req 1 .conflict, req 2 .conflict] [(.conflict, 1), (.conflict, 2)]
    let l2 := cycle l1 [req 2 .conflict, req 3 .drift] [(.conflict, 2), (.drift, 3)]
    let l3 := cycle l2 [req 2 .conflict, req 3 .drift] [(.conflict, 2), (.drift, 3)]
    keys l1 = [(2, .conflict), (1, .conflict)] ∧ keys l2 = [(2, .conflict)] ∧
      keys l3 = [(2, .conflict), (3, .drift)] := by
  decide

/-- **#298 fixed** (`sync_cycle_cuts_before_it_enforces` in
`src/daemon/hotlist.rs`; `a_lapsed_entry_never_evicts_a_fresh_hit` in
`src/daemon/tests/conditions.rs` runs it through `detect_into_hot_list`, with
a stale hit in place of the drift): the scenario of `cycle_evicts_fresh_hit` under the
cycle since #298. Cycle 2 drops the old conflict on n1 before enforcing the
bound, so both fresh hits stay: n2's conflict and n3's drift. -/
theorem cycleFixed_keeps_fresh_hit :
    let l1 := cycleFixed (withMax 2) [req 1 .conflict, req 2 .conflict]
      [(.conflict, 1), (.conflict, 2)]
    let l2 := cycleFixed l1 [req 2 .conflict, req 3 .drift] [(.conflict, 2), (.drift, 3)]
    keys l1 = [(2, .conflict), (1, .conflict)] ∧ keys l2 = [(2, .conflict), (3, .drift)] ∧
      (cycleFixedFull l1 [req 2 .conflict, req 3 .drift] [(.conflict, 2), (.drift, 3)]).2 = [] ∧
      keys (cycle l1 [req 2 .conflict, req 3 .drift] [(.conflict, 2), (.drift, 3)])
        = [(2, .conflict)] := by
  decide

/-- When the fresh hits themselves overflow, the bound evicts the lowest of
them, never for an entry the cycle no longer detects: bound 2, an old
high-risk entry on n9 lapses, and of three fresh hits the stale one goes. -/
example :
    let l1 := cycleFixed (withMax 2) [req 9 .highRisk] (heldOf [req 9 .highRisk])
    let hits := [req 1 .conflict, req 2 .drift, req 3 .stale]
    let r := cycleFixedFull l1 hits (heldOf hits)
    keys r.1 = [(1, .conflict), (2, .drift)] ∧ r.2.map key = [(3, .stale)] := by
  decide

/-- The budget still holds: bound 3, budget 1, two fresh contradictions and a
drift after an old conflict lapses; the later-declared contradiction stays. -/
example :
    let l1 := cycleFixed (withBudget 3 1) [req 9 .conflict] (heldOf [req 9 .conflict])
    let hits := [req 1 .contradiction, req 2 .contradiction, req 3 .drift]
    let r := cycleFixedFull l1 hits (heldOf hits)
    keys r.1 = [(2, .contradiction), (3, .drift)] ∧ r.2.map key = [(1, .contradiction)] := by
  decide

/-- Deferring the limits or lifting them leaves the same entries: single
inserts on the list with a large bound and budget leave the entries
`upsertAll` leaves, here with a refresh of a present pair. -/
example :
    let l1 := insertAll (withMax 2) [req 1 .conflict, req 2 .conflict]
    let hits := [req 2 .conflict, req 3 .drift, req 1 .stale]
    sortPrio (insertAll { l1 with max := 100, cmax := 100 } hits).entries
      = sortPrio (upsertAll l1 hits).entries := by
  decide

/-- **No lapsed entry, overflowing hits**: when every pair on the list is held
and the held set is the hits' pairs, the cycles before and since #298 leave
the same list, even where the hits overflow the bound or the budget. Checked
on the `plan` hits (which repeat pairs and refresh the seeded one) for several
bounds and budgets; proving it in general needs the `insert_many`
equivalence, which is not proved (see the header). -/
example :
    [(0, 0), (3, 0), (3, 1), (3, 3), (8, 2), (8, 4), (8, 8), (20, 3), (20, 10), (100, 50)].all
      fun (m, b) =>
        let l := insertAll (withBudget m b) [req 99 .drift]
        let hits := req 99 .drift :: plan
        (cycleFixed l hits (heldOf hits)).entries = (cycle l hits (heldOf hits)).entries := by
  decide +kernel

/-- `revalidate_uses_the_callers_clock_and_refreshes_the_payload`: written at
1000 with a 30 s window, the entry reads 11 then 25 seconds, and has lapsed at
1031. -/
example :
    (windowCheck 1000 30 1011 : Option (Payload Nat)) = some (.conflict 11) ∧
      (windowCheck 1000 30 1025 : Option (Payload Nat)) = some (.conflict 25) ∧
      (windowCheck 1000 30 1031 : Option (Payload Nat)) = none := by decide

/-- Write times for the next example. -/
def writtenAt : Nat → Int
  | 1 => 49
  | _ => 29

def exCheck : Check Nat := fun n _ now => windowCheck (writtenAt n) 30 now

/-- `revalidate_members_keeps_live_and_drops_lapsed`: at `now = 60`, members
`[1, 2, 3, 1]`; `n1` is live (11 s), `n2` lapsed (31 s) and is evicted, `n3` is
not hot, `n4` lapsed but is not a member, so it is left alone. -/
example :
    let l := insertAll new [req 1 .conflict, req 2 .conflict, req 4 .conflict]
    let m := [1, 2, 3, 1]
    hotPayloads exCheck m 60 l 1 = [.conflict 11] ∧ hotPayloads exCheck m 60 l 2 = [] ∧
      hotPayloads exCheck m 60 l 3 = [] ∧ hotPayloads exCheck m 60 l 4 = [] ∧
      nodes (revalidate exCheck m 60 l) = [4, 1] := by
  decide

/-- Which payloads force: everything but a contradiction of an ordinary
concept. -/
example :
    let ord : Nat → Bool := fun _ => false
    let pil : Nat → Bool := fun _ => true
    forces ord (fun _ => [.conflict 11]) 1 = true ∧ forces ord (fun _ => [.highRisk]) 1 = true ∧
      forces ord (fun _ => [.drift 4]) 1 = true ∧ forces ord (fun _ => [.stale 60]) 1 = true ∧
      forces ord (fun _ => [.contradiction 2 0]) 1 = false ∧
      forces pil (fun _ => [.contradiction 2 0]) 1 = true ∧
      forces ord (fun _ => [.contradiction 2 0, .stale 60]) 1 = true := by
  decide

/-- An ordinary contradicted hit is not forced but, once shown, carries its
line (`an_ordinary_contradicted_hit_below_the_cut_is_not_forced`). -/
example :
    let hp : Nat → List (Payload Nat) := fun n => if n = 3 then [.contradiction 9 0] else []
    forces (fun _ => false) hp 3 = false ∧ hotLines hp 3 = [.contradiction] ∧
      TopK.emit (fun _ => true) (hot (fun _ => false) (fun _ _ => true) hp [1, 2, 3]) 1 0 [1, 2, 3]
        = [1] := by
  decide

/-- A Venerable contradicted hit below the cut is forced and pulls its
contradicting concept `9` in, both past `top_k` (#19). -/
example :
    let hp : Nat → List (Payload Nat) := fun n => if n = 3 then [.contradiction 9 0] else []
    TopK.emit (fun _ => true) (hot (fun n => n = 3) (fun _ _ => true) hp [1, 2, 3]) 1 0
        [1, 2, 3, 9] = [1, 3, 9] := by
  decide

/-- `hot_force_include_keeps_live_and_drops_lapsed`: with `top_k = 1`, the
live conflict member `n1` ranked last is still emitted; the lapsed `n2` is not
forced. -/
example :
    let l := insertAll new [req 1 .conflict, req 2 .conflict]
    TopK.emit (fun _ => true)
        (recallForced (fun _ => false) (fun _ _ => true) exCheck [3, 2, 1] 60 l false (fun _ => false))
        1 0 [3, 2, 1] = [3, 1] := by
  decide

/-- GC: the entry for collected `n1` is still on the list, but `n1` is not
emitted. -/
example :
    let l := insertAll new [req 1 .conflict]
    (1, .conflict) ∈ keys l ∧
      TopK.emit (fun n => n ≠ 1)
        (recallForced (fun _ => false) (fun _ _ => true) (fun _ _ _ => some (.conflict 5)) [1, 2]
          60 l false (fun _ => false)) 0 0 [1, 2] = [] := by
  decide

end Examples

end Lambo.Hotlist
