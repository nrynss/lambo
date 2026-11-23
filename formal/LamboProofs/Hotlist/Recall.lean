import LamboProofs.Hotlist.List

/-
# How recall consumes the hot list (`src/recall/assemble.rs`)

Models `forces`, `contradiction_companions`, the `hot` / `cold_forced` /
`forced` predicates handed to `emitted`, and the kinds of the hot-list warning
lines (`format::hot_warning`, `format::contradiction_warning`), over
`List.lean`'s list; `forces_inclusion` (`src/daemon/contradiction.rs`:
Canonical or Venerable) is the parameter `pillar`.

Results: a member is forced by its payloads exactly when one is a conflict,
high-risk, drift or stale payload, or a contradiction on a pillar
(`forces_iff`, #179); every member forced by a live entry is emitted at any
`top_k`, blended or cold, with or without the redundancy skip
(`hot_member_emitted`, `hot_member_emittedR`); a forced member carries a
hot-list line and is never skipped as redundant (`forced_has_line`,
`hot_member_never_skipped`); a member whose entries have all lapsed is not
forced by the hot list (`lapsed_not_forced`); and a GC-collected node is
never emitted though its entry is still listed (`collected_not_emitted`).

Boundary: the companion step after the cut (`with_ordinary_companions`) and
the token budget are not modelled; a reservation holder is forced only in
cold mode (`forcedIn`), from the graph, not the hot list.
-/

namespace Lambo.Hotlist

open Lambo.Lib (eq_of_nodup_map length_le_of_nodup_subset)
variable {ν : Type}

/-! ## Recall: force-inclusion and warning lines -/

/-- `forces`, per payload: every kind forces, except a contradiction of a
concept that is not Canonical or Venerable (`pillar`,
`daemon::contradiction::forces_inclusion`; #179). -/
def forcesPayload (pillar : Bool) : Payload ν → Bool
  | .contradiction _ _ => pillar
  | _ => true

/-- `forces(graph, id, hot_payloads)`. `pillar id` is "a graph concept whose
status is Canonical or Venerable" (false for a graph-missing id). -/
def forces (pillar : ν → Bool) (hp : ν → List (Payload ν)) (id : ν) : Bool :=
  (hp id).any (forcesPayload (pillar id))

theorem forcesPayload_iff (pillar : Bool) (p : Payload ν) :
    forcesPayload pillar p = true ↔ p.kind ≠ .contradiction ∨ pillar = true := by
  cases p <;> simp [forcesPayload, Payload.kind]

/-- **Which kinds force**: conflict, high-risk, drift and stale always; a
contradiction only on a Canonical or Venerable concept. -/
theorem forces_iff (pillar : ν → Bool) (hp : ν → List (Payload ν)) (id : ν) :
    forces pillar hp id = true ↔ ∃ p ∈ hp id, p.kind ≠ .contradiction ∨ pillar id = true := by
  unfold forces
  simp only [List.any_eq_true, forcesPayload_iff]

/-- A well-formed entry's payload is of its condition's kind (every detector
builds them so; `HotListEntry::new` does not check it). -/
def WellFormed (e : Entry ν) : Prop := e.payload.kind = e.cond

/-- A well-formed hot-list entry's payload forces inclusion in recall
(`forces_inclusion`) unless it is a contradiction and the concept is not a
pillar. -/
theorem forcesPayload_of_wf (pillar : Bool) (e : Entry ν) (h : WellFormed e) :
    forcesPayload pillar e.payload = true ↔ e.cond ≠ .contradiction ∨ pillar = true := by
  rw [forcesPayload_iff, h]

/-- An ordinary contradicted member is not forced (#179). -/
theorem ordinary_contradiction_not_forced (pillar : ν → Bool) (hp : ν → List (Payload ν))
    (id : ν) (hpil : pillar id = false) (hk : ∀ p ∈ hp id, p.kind = .contradiction) :
    forces pillar hp id = false := by
  cases h : forces pillar hp id with
  | false => rfl
  | true =>
    obtain ⟨p, hp', hor⟩ := (forces_iff pillar hp id).mp h
    rw [hpil] at hor
    simp only [Bool.false_eq_true, or_false] at hor
    exact absurd (hk p hp') hor

section Recall

variable [DecidableEq ν]

/-- `contradiction_companions(..).contains_key(b)`: `b` is the `by` of a
`Contradiction` payload on a Canonical or Venerable member, and a live
concept of that member's session (`liveBy t b`, `same_session_concept`). -/
def companion (pillar : ν → Bool) (liveBy : ν → ν → Bool) (hp : ν → List (Payload ν))
    (members : List ν) (b : ν) : Bool :=
  members.any fun t => pillar t && (hp t).any fun p =>
    match p with
    | .contradiction b' _ => decide (b' = b) && liveBy t b'
    | _ => false

/-- `hot`: forced by its own hot-list payloads, or as a contradiction
companion. -/
def hot (pillar : ν → Bool) (liveBy : ν → ν → Bool) (hp : ν → List (Payload ν))
    (members : List ν) (id : ν) : Bool :=
  forces pillar hp id || companion pillar liveBy hp members id

/-- `forced`: in cold mode the kept reservation holders are forced too. -/
def forcedIn (cold : Bool) (reservedKeep : ν → Bool) (hotF : ν → Bool) (id : ν) : Bool :=
  if cold then hotF id || reservedKeep id else hotF id

omit [DecidableEq ν] in
theorem forcedIn_of_hot (cold : Bool) (reservedKeep hotF : ν → Bool) (id : ν)
    (h : hotF id = true) : forcedIn cold reservedKeep hotF id = true := by
  cases cold <;> simp [forcedIn, h]

/-- The force predicate recall builds from the hot list: re-validate at `now`,
then `forces` or companion, in either mode. -/
def recallForced (pillar : ν → Bool) (liveBy : ν → ν → Bool) (check : Check ν)
    (members : List ν) (now : Int) (l : HotList ν) (cold : Bool) (reservedKeep : ν → Bool) :
    ν → Bool :=
  forcedIn cold reservedKeep (hot pillar liveBy (hotPayloads check members now l) members)

/-- A member is forced by its own hot-list entries exactly when one of them is
on the list, holds at `now`, and its rebuilt payload forces. -/
theorem forces_hotPayloads_iff (pillar : ν → Bool) (check : Check ν) (members : List ν)
    (now : Int) (l : HotList ν) (id : ν) :
    forces pillar (hotPayloads check members now l) id = true ↔
      id ∈ members ∧ ∃ e ∈ l.entries, ∃ p, e.node = id ∧ check id e.cond now = some p ∧
        forcesPayload (pillar id) p = true := by
  unfold forces
  simp only [List.any_eq_true, mem_hotPayloads]
  constructor
  · rintro ⟨p, ⟨hm, e, he, hid, hc⟩, hf⟩
    exact ⟨hm, e, he, p, hid, hc, hf⟩
  · rintro ⟨hm, e, he, p, hid, hc, hf⟩
    exact ⟨p, ⟨hm, e, he, hid, hc⟩, hf⟩

/-- **A lapsed entry never forces inclusion**: a member whose entries all fail
their predicate at `now` is not forced by the hot list (it can still be
forced as a contradiction companion, or as a cold-mode reservation holder). -/
theorem lapsed_not_forced (pillar : ν → Bool) (check : Check ν) (members : List ν) (now : Int)
    (l : HotList ν) (id : ν)
    (hlapse : ∀ e ∈ l.entries, e.node = id → check id e.cond now = none) :
    forces pillar (hotPayloads check members now l) id = false := by
  unfold forces
  rw [hotPayloads_nil_of_lapsed hlapse]
  rfl

/-- **Every forced hot-list member is emitted**, past `top_k`, at any `top_k`
and in either scoring mode (`TopK.forced_mem_emit`). -/
theorem hot_member_emitted (isConcept pillar : ν → Bool) (liveBy : ν → ν → Bool)
    (check : Check ν) (members : List ν) (now : Int) (l : HotList ν) (cold : Bool)
    (reservedKeep : ν → Bool) (topK : Nat) (ranked : List ν) (id : ν)
    (e : Entry ν) (he : e ∈ l.entries) (hid : e.node = id) (hm : id ∈ members) (p : Payload ν)
    (hc : check id e.cond now = some p) (hf : forcesPayload (pillar id) p = true)
    (hcon : isConcept id = true) (hr : id ∈ ranked) :
    id ∈ TopK.emit isConcept (recallForced pillar liveBy check members now l cold reservedKeep)
      topK 0 ranked := by
  apply TopK.forced_mem_emit _ _ topK id hcon _ ranked 0 hr
  apply forcedIn_of_hot
  unfold hot
  rw [(forces_hotPayloads_iff pillar check members now l id).mpr ⟨hm, e, he, p, hid, hc, hf⟩]
  rfl

/-- The same with the redundancy skip on (#181): a forced member is never
skipped (`TopK.forced_mem_emitR`). -/
theorem hot_member_emittedR (isConcept pillar warns : ν → Bool) (liveBy : ν → ν → Bool)
    (red : ν → ν → Bool) (check : Check ν) (members : List ν) (now : Int) (l : HotList ν)
    (cold : Bool) (reservedKeep : ν → Bool) (topK win wmax : Nat) (ranked : List ν) (id : ν)
    (e : Entry ν) (he : e ∈ l.entries) (hid : e.node = id) (hm : id ∈ members) (p : Payload ν)
    (hc : check id e.cond now = some p) (hf : forcesPayload (pillar id) p = true)
    (hcon : isConcept id = true) (hr : id ∈ ranked) :
    id ∈ TopK.emitR isConcept (recallForced pillar liveBy check members now l cold reservedKeep)
      warns red topK win wmax 0 0 [] ranked := by
  apply TopK.forced_mem_emitR _ _ warns red topK win wmax id hcon _ ranked 0 0 [] hr
  apply forcedIn_of_hot
  unfold hot
  rw [(forces_hotPayloads_iff pillar check members now l id).mpr ⟨hm, e, he, p, hid, hc, hf⟩]
  rfl

omit [DecidableEq ν] in
/-- **A node removed by GC is never emitted**, even while its entry is still on
the hot list (`TopK.emit_all_concepts`). -/
theorem collected_not_emitted (isConcept forced : ν → Bool) (topK : Nat) (ranked : List ν)
    (id : ν) (hgone : isConcept id = false) : id ∉ TopK.emit isConcept forced topK 0 ranked := by
  intro h
  rw [TopK.emit_all_concepts isConcept forced topK id ranked 0 h] at hgone
  exact absurd hgone (by decide)

end Recall

/-- The annotation kind of a hot-list warning line (`AnnotationKind`). -/
inductive Annot
  | conflict
  | hot
  | contradiction
  deriving DecidableEq, Repr

/-- The kind recall attaches to a payload's line (`format::hot_warning` /
`format::contradiction_warning`). -/
def annot : Payload ν → Annot
  | .conflict _ => .conflict
  | .highRisk => .hot
  | .drift _ => .hot
  | .stale _ => .hot
  | .contradiction _ _ => .contradiction

/-- The hot-list warning lines of a shown hit, one per payload, in payload
order (the lines between ⚑ and the companion line). -/
def hotLines (hp : ν → List (Payload ν)) (id : ν) : List Annot := (hp id).map annot

/-- Recall renders one hot-list line per payload the concept carries
(`hot_warning`, `contradiction_warning`). -/
theorem hotLines_length (hp : ν → List (Payload ν)) (id : ν) :
    (hotLines hp id).length = (hp id).length := by
  simp [hotLines]

/-- Recall renders a hot-list payload with the contradiction warning
(`contradiction_warning`) exactly when it is a contradiction payload. -/
theorem annot_eq_contradiction_iff (p : Payload ν) :
    annot p = .contradiction ↔ p.kind = .contradiction := by
  cases p <;> simp [annot, Payload.kind]

/-- Recall renders a hot-list payload with the conflict warning exactly when
it is a conflict payload. -/
theorem annot_eq_conflict_iff (p : Payload ν) :
    annot p = .conflict ↔ p.kind = .conflict := by
  cases p <;> simp [annot, Payload.kind]

/-- **A forced hit always says why**: one forced by its own payloads carries at
least one hot-list line. -/
theorem forced_has_line (pillar : ν → Bool) (hp : ν → List (Payload ν)) (id : ν)
    (h : forces pillar hp id = true) : hotLines hp id ≠ [] := by
  obtain ⟨p, hp', _⟩ := (forces_iff pillar hp id).mp h
  intro hnil
  unfold hotLines at hnil
  rw [List.map_eq_nil_iff] at hnil
  rw [hnil] at hp'
  simp at hp'

/-- With the redundancy skip, a member that carries a hot-list payload is never
skipped as redundant (`warns` counts a non-empty payload list; `TopK.skipsR_spec`),
whether or not that payload forces it. -/
theorem hot_member_never_skipped (isConcept forced warns : ν → Bool) (red : ν → ν → Bool)
    (topK win wmax : Nat) (ranked : List ν) (hp : ν → List (Payload ν))
    (hw : ∀ x, hp x ≠ [] → warns x = true) (k id : ν) (hne : hp id ≠ []) :
    (k, id) ∉ TopK.skipsR isConcept forced warns red topK win wmax 0 0 [] ranked := by
  intro h
  have := (TopK.skipsR_spec isConcept forced warns red topK win wmax k id ranked 0 0 [] h).2.2.1
  rw [hw id hne] at this
  exact absurd this (by decide)

end Lambo.Hotlist
