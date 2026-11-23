import LamboProofs.Supersession.Suspected

/-
# Evidence-driven supersession (#20): the canon pass

Models `src/canon/supersession.rs` behind `Config::canonization_supersede`:
the evidence rules of `src/store/evidence.rs` (`latest_at`, `counted`,
`newest_counted`, `span_since` with `stage2_evidence_passes`), `watermark`,
`contradictions`, `usable`, `probes_for`, `gather`, `gather_holds`, `decide`
(the pair rule `outranks`, one winner per target), and the supersession pass
of `apply` in `src/canon/eval.rs` through the write gate (`Canon.gate`).
The option defaults to off (`canonization_supersede: false` in `Config::default`,
`src/config.rs`; test `canonization_supersede_defaults_off_and_parses_from_the_file`).

Results: a verdict comes from a forward probe whose source's newest counted
evidence is strictly after the target's watermark (`verdict_sound`); a
concept is never superseded by itself (`verdict_ne`); never verdicts both
ways for one pair in a cycle (`pair_rule`), the older evidence losing
(`verdict_outranks_reverse`), and of several sources the newest evidence wins
(`verdict_best`); the pass moves a concept only `Canonical → None`, never
promotes or skips, and touches only verdict targets (`applySup_legal`,
`applySup_untouched`), being `Canon.cycle`'s pass 0 (`applySup_eq_cycle`);
with the option off nothing is probed, demoted or held (`off_no_demotion`,
`held_off`); and a `SuspectedSupersedes` edge is never read as a
contradiction, so #198 never demotes (`probesFor_ignore_suspected`).

Boundary: dates are milliseconds; Stage 2's coverage is compared by
cross-multiplication, differing from the `f64` quotient only by rounding at
the boundary. Writer attribution is a given function and the evidence rows
arrive reduced; the ring window, batch size, `MAX_SUPERSESSION_PROBES`, the
hold's deferrals and the evidence cache choose which targets are probed and
are not modelled (every theorem holds for any target list), nor are
`gather_hold_for` and Stage 3's further use of the hold.
-/

namespace Lambo.Supersession

open Lambo.Canon (Status legal gate)

/-! ## Evidence-driven supersession (#20): the evidence

Dates are integer milliseconds. A row is one dependence edge into a node, as
`DependentEvidence` carries it, already reduced to what the rules read:

* `at_` is the edge's `declared` date (about-time clamped to write time);
* `eligible` is "written by the cutoff and not from the node's own origin"
  (`eligible` in `src/store/evidence.rs`);
* `originAt` is the dependent's origin interaction's declared date when that
  origin is a session interaction written by the cutoff, `none` otherwise
  (`counted` drops such a row).

`coveragePasses` takes the minimum and maximum over the rows' origin dates,
where `span_since` takes them over a per-origin map (one entry per origin).
The two agree when every row of one origin carries that origin's date. The
`Row` type does not enforce it; the Rust gives it by construction (a row's
`origin_dates` are its origin interaction's, and one interaction has one
date). No theorem in this file depends on it: they use only the count of
distinct origins and the edge dates. -/

structure Row where
  edge : Nat
  at_ : Int
  origin : Nat
  originAt : Option Int
  eligible : Bool
  deriving DecidableEq, Repr

structure Evidence where
  rows : List Row
  /-- The session's extent on about-times, `None` without interactions. -/
  extent : Option (Int × Int)
  deriving DecidableEq, Repr

/-- The running maximum of a fold (`Iterator::max`). -/
def maxOpt : Option Int → Int → Option Int
  | none, x => some x
  | some m, x => some (max m x)

/-- `latest_at`: the newest declared date of the eligible evidence, ignoring
the edges in `exclude`. -/
def latestAt (ev : Evidence) (exclude : List Nat) : Option Int :=
  (ev.rows.filter fun r => r.eligible && !decide (r.edge ∈ exclude)).foldl
    (fun m r => maxOpt m r.at_) none

/-- `counted`: eligible rows whose edge and whose origin are both dated
strictly after `since`, with the origin's date. -/
def counted (ev : Evidence) (since : Int) : List (Row × Int) :=
  ev.rows.filterMap fun r =>
    if r.eligible && decide (since < r.at_) then
      match r.originAt with
      | some d => if since < d then some (r, d) else none
      | none => none
    else none

/-- `newest_counted`: the newest edge date among the counted rows. -/
def newestCounted (ev : Evidence) (since : Int) : Option Int :=
  (counted ev since).foldl (fun m x => maxOpt m x.1.at_) none

/-- `coverage(span, sess_span, window) >= 0.3` on the counted origins'
dates, exactly: a session of no extent covers fully, otherwise
`span / min(session, window) >= 0.3`. The clamp into `[0, 1]` does not move
the comparison. `window` is positive (`validate_coverage_window`).
Pinned by `formal_constants_tests::supersession`. -/
def coveragePasses (ev : Evidence) (window : Int) (c : List (Row × Int)) : Bool :=
  match c.map (·.2) with
  | [] => false
  | d :: ds =>
    let lo := ds.foldl min d
    let hi := ds.foldl max d
    let ext := ev.extent.getD (lo, hi)
    let sess := max 0 (ext.2 - ext.1)
    let span := max 0 (hi - lo)
    if sess ≤ 0 then true else decide (3 * min sess window ≤ 10 * span)

/-- `stage2_evidence_passes(span_since(...))`: at least three distinct
origins after `since`, covering at least `0.3`.
Pinned by `formal_constants_tests::supersession`. -/
def stage2Since (ev : Evidence) (window since : Int) : Bool :=
  let c := counted ev since
  decide (3 ≤ (sortDedup (c.map (·.1.origin))).length) && coveragePasses ev window c

theorem foldl_maxOpt_isSome {α : Type} (f : α → Int) :
    ∀ (l : List α) (m : Option Int), (m.isSome ∨ l ≠ []) →
      ∃ n, l.foldl (fun m x => maxOpt m (f x)) m = some n := by
  intro l
  induction l with
  | nil =>
    intro m h
    cases m with
    | none => simp at h
    | some n => exact ⟨n, rfl⟩
  | cons x xs ih =>
    intro m _
    simp only [List.foldl_cons]
    apply ih
    left
    cases m <;> simp [maxOpt]

theorem foldl_maxOpt_gt {α : Type} (f : α → Int) (s : Int) :
    ∀ (l : List α) (m : Option Int), (∀ x, m = some x → s < x) → (∀ x ∈ l, s < f x) →
      ∀ n, l.foldl (fun m x => maxOpt m (f x)) m = some n → s < n := by
  intro l
  induction l with
  | nil => intro m hm _ n hn; exact hm n hn
  | cons x xs ih =>
    intro m hm hl n hn
    simp only [List.foldl_cons] at hn
    apply ih _ _ (fun y hy => hl y (List.mem_cons_of_mem _ hy)) n hn
    intro y hy
    have hx := hl x (List.mem_cons_self ..)
    cases m with
    | none => simp [maxOpt] at hy; omega
    | some m' =>
      have := hm m' rfl
      simp [maxOpt] at hy; omega

theorem counted_after (ev : Evidence) (since : Int) (x : Row × Int) (hx : x ∈ counted ev since) :
    since < x.1.at_ ∧ since < x.2 := by
  simp only [counted, List.mem_filterMap] at hx
  obtain ⟨r, _, hr⟩ := hx
  split at hr
  · rename_i hc
    simp only [Bool.and_eq_true, decide_eq_true_eq] at hc
    split at hr
    · rename_i d _
      split at hr
      · rename_i hd
        simp only [Option.some.injEq] at hr
        subst hr
        exact ⟨hc.2, hd⟩
      · cases hr
    · cases hr
  · cases hr

/-- A passing span counted something, and the newest counted evidence is
strictly after `since` ("newer support"). -/
theorem stage2Since_newest (ev : Evidence) (window since : Int)
    (h : stage2Since ev window since = true) :
    ∃ n, newestCounted ev since = some n ∧ since < n := by
  have hne : counted ev since ≠ [] := by
    intro he
    simp [stage2Since, he, sortDedup] at h
  obtain ⟨n, hn⟩ := foldl_maxOpt_isSome (fun x : Row × Int => x.1.at_) (counted ev since) none
    (Or.inr hne)
  refine ⟨n, hn, ?_⟩
  exact foldl_maxOpt_gt (fun x : Row × Int => x.1.at_) since _ none (by simp)
    (fun x hx => (counted_after ev since x hx).1) n hn

/-- Evidence at the watermark instant does not count (Rust test
`support_at_the_watermark_instant_does_not_count`). -/
theorem counted_at_instant (since : Int) (r : Row) (h : r.at_ = since) :
    counted ⟨[r], none⟩ since = [] := by
  simp [counted, h]

/-! ## The probe and the verdicts -/

/-- One `(target, source)` pair to probe. `key` is the source's canonical
key in `tie_form`, as a number with the same order. -/
structure Probe where
  target : Nat
  source : Nat
  key : Nat
  edge : Nat
  baseline : Int
  exclude : List Nat
  reverseOnly : Bool
  deriving DecidableEq, Repr

/-- A passing probe with its watermark and newest counted evidence. -/
structure Pass where
  probe : Probe
  watermark : Int
  newest : Int
  deriving DecidableEq, Repr

/-- The watermark: the later of the target's baseline and its latest
eligible evidence, the self-suppressed edges excluded. -/
def watermark (ev : Nat → Evidence) (p : Probe) : Int :=
  match latestAt (ev p.target) p.exclude with
  | some a => max a p.baseline
  | none => p.baseline

/-- One probe of `decide`: the source's span strictly after the target's
watermark must pass Stage 2. -/
def evaluate (ev : Nat → Evidence) (window : Int) (p : Probe) : Option Pass :=
  let w := watermark ev p
  if stage2Since (ev p.source) window w then
    some ⟨p, w, (newestCounted (ev p.source) w).getD w⟩
  else none

/-- `outranks`: newer counted evidence, then the smaller key, then the
smaller source id. -/
def outranks (a b : Pass) : Bool :=
  decide (b.newest < a.newest ∨
    (a.newest = b.newest ∧ (a.probe.key < b.probe.key ∨
      (a.probe.key = b.probe.key ∧ a.probe.source < b.probe.source))))

theorem outranks_irrefl (a : Pass) : outranks a a = false := by
  simp [outranks]

theorem outranks_trans (a b c : Pass) (h1 : outranks a b = true) (h2 : outranks b c = true) :
    outranks a c = true := by
  simp only [outranks, decide_eq_true_eq] at *
  omega

/-- Two passes with different sources are always ordered one way. -/
theorem outranks_total (a b : Pass) (h : a.probe.source ≠ b.probe.source) :
    outranks a b = true ∨ outranks b a = true := by
  simp only [outranks, decide_eq_true_eq]
  omega

/-- `outranks` (newer counted evidence, then the smaller key, then the smaller
source id) never ranks each of two supersession passes over the other. -/
theorem outranks_asymm (a b : Pass) (h : outranks a b = true) : outranks b a = false := by
  simp only [outranks, decide_eq_true_eq, decide_eq_false_iff_not] at *
  omega

/-- The pair rule: `a` is beaten when the reverse pass outranks it. -/
def beaten (passing : List Pass) (a : Pass) : Bool :=
  passing.any fun b =>
    b.probe.target == a.probe.source && b.probe.source == a.probe.target && outranks b a

/-- The best of a list: the first one no later one outranks. -/
def best : List Pass → Option Pass
  | [] => none
  | p :: ps => some (ps.foldl (fun cur q => if outranks q cur then q else cur) p)

/-- A demotion verdict. -/
structure Verdict where
  target : Nat
  source : Nat
  edge : Nat
  watermark : Int
  deriving DecidableEq, Repr

def Pass.verdict (w : Pass) : Verdict := ⟨w.probe.target, w.probe.source, w.probe.edge, w.watermark⟩

/-- The passing probes. -/
def passing (ev : Nat → Evidence) (window : Int) (probes : List Probe) : List Pass :=
  probes.filterMap (evaluate ev window)

/-- The forward passes the pair rule leaves standing. -/
def winners (ev : Nat → Evidence) (window : Int) (probes : List Probe) : List Pass :=
  let ps := passing ev window probes
  ps.filter fun p => !p.probe.reverseOnly && !beaten ps p

/-- `decide`: every passing probe, the pair rule, then one winning source per
forward target, in target order. -/
def decideV (ev : Nat → Evidence) (window : Int) (probes : List Probe) : List Verdict :=
  let ws := winners ev window probes
  (sortDedup (ws.map (·.probe.target))).filterMap fun t =>
    (best (ws.filter (·.probe.target == t))).map Pass.verdict

theorem evaluate_some (ev : Nat → Evidence) (window : Int) (p : Probe) (a : Pass)
    (h : evaluate ev window p = some a) :
    a.probe = p ∧ p.baseline ≤ a.watermark ∧ a.watermark < a.newest := by
  unfold evaluate at h
  dsimp only at h
  split at h
  · rename_i hs
    simp only [Option.some.injEq] at h
    subst h
    obtain ⟨n, hn, hlt⟩ := stage2Since_newest _ _ _ hs
    refine ⟨rfl, ?_, ?_⟩
    · simp only [watermark]; split <;> omega
    · simp only [hn, Option.getD_some]; exact hlt
  · cases h

theorem best_mem : ∀ (l : List Pass) (b : Pass), best l = some b → b ∈ l
  | [], _, h => by cases h
  | p :: ps, b, h => by
    simp only [best, Option.some.injEq] at h
    subst h
    suffices ∀ (cur : Pass), (cur = p ∨ cur ∈ ps) → ∀ (qs : List Pass), (∀ q ∈ qs, q ∈ ps) →
        (qs.foldl (fun cur q => if outranks q cur then q else cur) cur = p ∨
          qs.foldl (fun cur q => if outranks q cur then q else cur) cur ∈ ps) by
      rcases this p (Or.inl rfl) ps (fun q hq => hq) with h | h
      · rw [h]; exact List.mem_cons_self ..
      · exact List.mem_cons_of_mem _ h
    intro cur hcur qs hqs
    induction qs generalizing cur with
    | nil => exact hcur
    | cons q qs ih =>
      simp only [List.foldl_cons]
      apply ih
      · split
        · exact Or.inr (hqs q (List.mem_cons_self ..))
        · exact hcur
      · exact fun x hx => hqs x (List.mem_cons_of_mem _ hx)

/-- Nothing in the list outranks its best. -/
theorem best_max : ∀ (l : List Pass) (b : Pass), best l = some b → ∀ q ∈ l, outranks q b = false
  | [], _, h => by cases h
  | p :: ps, b, h => by
    simp only [best, Option.some.injEq] at h
    subst h
    suffices ∀ (cur : Pass) (seen : List Pass), (∀ q ∈ seen, outranks q cur = false) →
        outranks cur cur = false →
        ∀ (qs : List Pass), ∀ q ∈ seen ++ qs,
          outranks q (qs.foldl (fun cur q => if outranks q cur then q else cur) cur) = false by
      intro q hq
      have := this p [p] (by simp [outranks_irrefl]) (outranks_irrefl p) ps q
        (by simpa using hq)
      exact this
    intro cur seen hseen hcc qs
    induction qs generalizing cur seen with
    | nil => simpa using hseen
    | cons x xs ih =>
      intro q hq
      simp only [List.foldl_cons]
      have hq' : q ∈ (seen ++ [x]) ++ xs := by simpa using hq
      apply ih _ (seen ++ [x]) _ (by
        split
        · exact outranks_irrefl x
        · exact hcc) q hq'
      intro y hy
      simp only [List.mem_append, List.mem_singleton] at hy
      split
      · rename_i hx
        rcases hy with hy | hy
        · cases hyx : outranks y x
          · rfl
          · have := outranks_trans y x cur hyx hx
            rw [hseen y hy] at this; cases this
        · subst hy; exact outranks_irrefl y
      · rename_i hx
        rcases hy with hy | hy
        · exact hseen y hy
        · subst hy; simpa using hx

theorem mem_decideV (ev : Nat → Evidence) (window : Int) (probes : List Probe) (v : Verdict)
    (hv : v ∈ decideV ev window probes) :
    ∃ w ∈ winners ev window probes, v = w.verdict ∧
      ∀ q ∈ winners ev window probes, q.probe.target = w.probe.target → outranks q w = false := by
  simp only [decideV, List.mem_filterMap, Option.map_eq_some_iff] at hv
  obtain ⟨t, _, w, hb, rfl⟩ := hv
  have hw := best_mem _ w hb
  rw [List.mem_filter] at hw
  refine ⟨w, hw.1, rfl, ?_⟩
  intro q hq hqt
  apply best_max _ w hb q
  rw [List.mem_filter]
  refine ⟨hq, ?_⟩
  have := hw.2
  simp only [beq_iff_eq] at this ⊢
  rw [hqt, this]

theorem mem_winners (ev : Nat → Evidence) (window : Int) (probes : List Probe) (w : Pass)
    (hw : w ∈ winners ev window probes) :
    w ∈ passing ev window probes ∧ w.probe.reverseOnly = false ∧
      beaten (passing ev window probes) w = false := by
  simp only [winners, List.mem_filter, Bool.and_eq_true, Bool.not_eq_true'] at hw
  exact ⟨hw.1, hw.2.1, hw.2.2⟩

theorem mem_passing (ev : Nat → Evidence) (window : Int) (probes : List Probe) (w : Pass)
    (hw : w ∈ passing ev window probes) :
    w.probe ∈ probes ∧ w.probe.baseline ≤ w.watermark ∧ w.watermark < w.newest := by
  simp only [passing, List.mem_filterMap] at hw
  obtain ⟨p, hp, he⟩ := hw
  obtain ⟨h1, h2, h3⟩ := evaluate_some ev window p w he
  rw [h1]; exact ⟨hp, h2, h3⟩

/-- No probe pairs a concept with itself (proved of the gathered probes by
`probesFor_ok`). -/
def NoSelf (probes : List Probe) : Prop := ∀ p ∈ probes, p.target ≠ p.source

/-- A verdict comes from a forward probe of the list, and the target's
support is older than the source's: the source's newest counted evidence is
strictly after the target's watermark, which is at least its baseline. -/
theorem verdict_sound (ev : Nat → Evidence) (window : Int) (probes : List Probe) (v : Verdict)
    (hv : v ∈ decideV ev window probes) :
    ∃ w ∈ passing ev window probes, v = w.verdict ∧ w.probe ∈ probes ∧
      w.probe.reverseOnly = false ∧ w.probe.baseline ≤ v.watermark ∧ v.watermark < w.newest := by
  obtain ⟨w, hw, rfl, _⟩ := mem_decideV ev window probes v hv
  obtain ⟨hp, hr, _⟩ := mem_winners ev window probes w hw
  obtain ⟨h1, h2, h3⟩ := mem_passing ev window probes w hp
  exact ⟨w, hp, rfl, h1, hr, h2, h3⟩

/-- **Never by itself.** -/
theorem verdict_ne (ev : Nat → Evidence) (window : Int) (probes : List Probe) (hn : NoSelf probes)
    (v : Verdict) (hv : v ∈ decideV ev window probes) : v.target ≠ v.source := by
  obtain ⟨w, _, rfl, hp, _⟩ := verdict_sound ev window probes v hv
  exact hn _ hp

/-- **The pair rule.** Of two concepts, at most one supersedes the other in
a cycle: there are never verdicts both ways. -/
theorem pair_rule (ev : Nat → Evidence) (window : Int) (probes : List Probe) (hn : NoSelf probes)
    (v u : Verdict) (hv : v ∈ decideV ev window probes) (hu : u ∈ decideV ev window probes)
    (h1 : u.target = v.source) (h2 : u.source = v.target) : False := by
  obtain ⟨w, hw, rfl, _⟩ := mem_decideV ev window probes v hv
  obtain ⟨x, hx, rfl, _⟩ := mem_decideV ev window probes u hu
  obtain ⟨hwp, _, hwb⟩ := mem_winners ev window probes w hw
  obtain ⟨hxp, _, hxb⟩ := mem_winners ev window probes x hx
  have hne : w.probe.target ≠ w.probe.source := hn _ (mem_passing ev window probes w hwp).1
  simp only [Pass.verdict] at h1 h2
  have hs : w.probe.source ≠ x.probe.source := by rw [h2]; exact Ne.symm hne
  have bw : ∀ b ∈ passing ev window probes, b.probe.target = w.probe.source →
      b.probe.source = w.probe.target → outranks b w = false := by
    intro b hb ht hs'
    cases ho : outranks b w
    · rfl
    · have : beaten (passing ev window probes) w = true := by
        simp only [beaten, List.any_eq_true, Bool.and_eq_true, beq_iff_eq]
        exact ⟨b, hb, ⟨ht, hs'⟩, ho⟩
      rw [hwb] at this; cases this
  have bx : ∀ b ∈ passing ev window probes, b.probe.target = x.probe.source →
      b.probe.source = x.probe.target → outranks b x = false := by
    intro b hb ht hs'
    cases ho : outranks b x
    · rfl
    · have : beaten (passing ev window probes) x = true := by
        simp only [beaten, List.any_eq_true, Bool.and_eq_true, beq_iff_eq]
        exact ⟨b, hb, ⟨ht, hs'⟩, ho⟩
      rw [hxb] at this; cases this
  rcases outranks_total w x hs with h | h
  · have := bx w hwp (by rw [h2]) (by rw [h1]); rw [h] at this; cases this
  · have := bw x hxp h1 h2; rw [h] at this; cases this

/-- **The older side is demoted.** When the reverse probe of a verdict
passes too, the verdict's own pass outranks it: the demoted target's newest
counted evidence against the source is at most the source's, and on a tie
the source has the smaller key, then the smaller id. -/
theorem verdict_outranks_reverse (ev : Nat → Evidence) (window : Int) (probes : List Probe)
    (hn : NoSelf probes) (v : Verdict) (hv : v ∈ decideV ev window probes) :
    ∃ w ∈ passing ev window probes, v = w.verdict ∧
      ∀ b ∈ passing ev window probes, b.probe.target = v.source → b.probe.source = v.target →
        outranks w b = true ∧ b.newest ≤ w.newest := by
  obtain ⟨w, hw, rfl, _⟩ := mem_decideV ev window probes v hv
  obtain ⟨hwp, _, hwb⟩ := mem_winners ev window probes w hw
  refine ⟨w, hwp, rfl, ?_⟩
  intro b hb ht hs
  simp only [Pass.verdict] at ht hs
  have hne : w.probe.target ≠ w.probe.source := hn _ (mem_passing ev window probes w hwp).1
  have hnb : outranks b w = false := by
    cases ho : outranks b w
    · rfl
    · have : beaten (passing ev window probes) w = true := by
        simp only [beaten, List.any_eq_true, Bool.and_eq_true, beq_iff_eq]
        exact ⟨b, hb, ⟨ht, hs⟩, ho⟩
      rw [hwb] at this; cases this
  have hsrc : w.probe.source ≠ b.probe.source := by rw [hs]; exact Ne.symm hne
  rcases outranks_total w b hsrc with h | h
  · refine ⟨h, ?_⟩
    simp only [outranks, decide_eq_true_eq] at h
    omega
  · rw [hnb] at h; cases h

/-- **The newest evidence wins** a target several sources qualify for
(Rust test `the_source_with_the_newest_evidence_wins`): no other standing
forward pass for the same target outranks the winner. -/
theorem verdict_best (ev : Nat → Evidence) (window : Int) (probes : List Probe) (v : Verdict)
    (hv : v ∈ decideV ev window probes) :
    ∃ w ∈ winners ev window probes, v = w.verdict ∧
      ∀ q ∈ winners ev window probes, q.probe.target = v.target → q.newest ≤ w.newest := by
  obtain ⟨w, hw, rfl, hmax⟩ := mem_decideV ev window probes v hv
  refine ⟨w, hw, rfl, ?_⟩
  intro q hq hqt
  have := hmax q hq hqt
  simp only [outranks, decide_eq_false_iff_not] at this
  omega

/-! ## Gathering the probes

A node is what the gather reads of a concept. `suppressed` is
`self_suppressed` by contradiction edge id: the dependence edges into the
target that do not count against that contradiction, or `none` when the
declarer, or a piece of evidence that might be the declarer's, cannot be told
apart: an ambiguity never demotes. Writer attribution itself is not modelled. -/

structure Node where
  id : Nat
  session : Nat
  status : Status
  lastDemotion : Option Int
  key : Nat
  /-- `baseline`: the origin about-time clamped, or the write time. -/
  baseline : Int
  deriving DecidableEq, Repr

structure World where
  session : Nat
  nodes : List Node
  edges : List Edge
  suppressed : Nat → Option (List Nat)

/-- `graph.node(id)` as a concept. -/
def World.node (W : World) (i : Nat) : Option Node := W.nodes.find? (·.id == i)

theorem World.node_id (W : World) (i : Nat) (c : Node) (h : W.node i = some c) : c.id = i := by
  have := List.find?_some h
  simpa using this

/-- `usable`: a concept of the session outside its re-promotion cooldown. -/
def usable (W : World) (cooldown now : Int) (i : Nat) : Option Node :=
  match W.node i with
  | some c =>
    if c.session = W.session ∧ Canon.inCooldown c.lastDemotion cooldown now = false then some c
    else none
  | none => none

/-- `contradictions`: the `Contradicts` edges into `t` from another concept,
written by `now`, whose source is usable, with that source. The Rust sorts
them by edge id; the order matters only for two edges joining the same
pair. -/
def contradictions (W : World) (cooldown now : Int) (t : Node) : List (Edge × Node) :=
  (W.edges.filter fun e =>
      e.ty == .contradicts && e.dst == t.id && e.src != t.id && decide (e.created ≤ now)).filterMap
    fun e => (usable W cooldown now e.src).map fun s => (e, s)

/-- `probe`: `None` when the attribution is ambiguous. -/
def mkProbe (supp : Nat → Option (List Nat)) (t : Node) (e : Edge) (s : Node) (rev : Bool) :
    Option Probe :=
  (supp e.id).map fun ex => ⟨t.id, s.id, s.key, e.id, t.baseline, ex, rev⟩

/-- The forward probes of one target and the ambiguous `(target, source)`
pairs met on the way. -/
def forwardOf (node : Nat → Option Node) (contra : Node → List (Edge × Node))
    (supp : Nat → Option (List Nat)) (t : Nat) : List Probe × List (Nat × Nat) :=
  match node t with
  | none => ([], [])
  | some tn =>
    (contra tn).foldl
      (fun acc es =>
        match mkProbe supp tn es.1 es.2 false with
        | some p => (acc.1 ++ [p], acc.2)
        | none => (acc.1, acc.2 ++ [(tn.id, es.2.id)]))
      ([], [])

/-- The reverse probe of a forward probe whose target also contradicts its
source, for the pair rule. -/
def reverseOf (node : Nat → Option Node) (contra : Node → List (Edge × Node))
    (supp : Nat → Option (List Nat)) (fwd : List Probe) (p : Probe) :
    List Probe × List (Nat × Nat) :=
  if fwd.any (fun q => q.target == p.source && q.source == p.target) then ([], [])
  else
    match node p.source, node p.target with
    | some t, some _ =>
      match (contra t).find? (fun es => es.2.id == p.target) with
      | some (e, s) =>
        match mkProbe supp t e s true with
        | some r => ([r], [])
        | none => ([], [(t.id, s.id)])
      | none => ([], [])
    | _, _ => ([], [])

/-- `probes_for` without the probe cap: the forward probes of `targets` in
order, the reverse probes, and neither direction of an ambiguous pair. -/
def probesWith (node : Nat → Option Node) (contra : Node → List (Edge × Node))
    (supp : Nat → Option (List Nat)) (targets : List Nat) : List Probe :=
  let fw := targets.map (forwardOf node contra supp)
  let fwd := (fw.map (·.1)).flatten
  let rv := fwd.map (reverseOf node contra supp fwd)
  let amb := (fw.map (·.2)).flatten ++ (rv.map (·.2)).flatten
  (fwd ++ (rv.map (·.1)).flatten).filter fun p => !amb.contains (p.source, p.target)

def probesFor (W : World) (cooldown now : Int) (targets : List Nat) : List Probe :=
  probesWith W.node (contradictions W cooldown now) W.suppressed targets

/-- The ring of `gather`: Canonical, usable, with a live contradiction. The
batch window, the ring cursor and the probe cap choose a sublist of it;
every result below about `probesFor` holds for any target list. -/
def ring (W : World) (cooldown now : Int) : List Nat :=
  sortDedup ((W.nodes.filter fun c =>
      c.status == .canonical && (usable W cooldown now c.id).isSome &&
        !(contradictions W cooldown now c).isEmpty).map (·.id))

/-- `gather`: nothing unless `canonization_supersede` is on. -/
def gather (W : World) (supersede : Bool) (cooldown now : Int) : List Probe :=
  if supersede then probesFor W cooldown now (ring W cooldown now) else []

/-- `gather_holds`: the hold's probes for Stage 3's window, nothing unless
the option is on. -/
def gatherHolds (W : World) (supersede : Bool) (cooldown now : Int) (window : List Nat) :
    List Probe :=
  if supersede then
    probesFor W cooldown now (window.filter fun i => (usable W cooldown now i).isSome)
  else []

/-- What a gathered probe rests on: a `Contradicts` edge of the world from
its source into its target, two different concepts. -/
def ProbeOK (W : World) (p : Probe) : Prop :=
  p.target ≠ p.source ∧
    ∃ e ∈ W.edges, e.ty = .contradicts ∧ e.id = p.edge ∧ e.src = p.source ∧ e.dst = p.target

theorem mem_contradictions (W : World) (cooldown now : Int) (t : Node) (e : Edge) (s : Node)
    (h : (e, s) ∈ contradictions W cooldown now t) :
    e ∈ W.edges ∧ e.ty = .contradicts ∧ e.dst = t.id ∧ e.src ≠ t.id ∧ s.id = e.src := by
  simp only [contradictions, List.mem_filterMap, List.mem_filter, Option.map_eq_some_iff] at h
  obtain ⟨e', ⟨he', hc⟩, s', hs', heq⟩ := h
  simp only [Prod.mk.injEq] at heq
  obtain ⟨rfl, rfl⟩ := heq
  simp only [Bool.and_eq_true, beq_iff_eq, bne_iff_ne, ne_eq, decide_eq_true_eq] at hc
  refine ⟨he', hc.1.1.1, hc.1.1.2, hc.1.2, ?_⟩
  unfold usable at hs'
  split at hs'
  · rename_i c hc'
    split at hs'
    · simp only [Option.some.injEq] at hs'; subst hs'; exact World.node_id W _ _ hc'
    · cases hs'
  · cases hs'

theorem mkProbe_ok (W : World) (cooldown now : Int) (t : Node) (e : Edge) (s : Node) (rev : Bool)
    (p : Probe) (hm : (e, s) ∈ contradictions W cooldown now t)
    (hp : mkProbe W.suppressed t e s rev = some p) : ProbeOK W p := by
  obtain ⟨he, ht, hd, hne, hs⟩ := mem_contradictions W cooldown now t e s hm
  simp only [mkProbe, Option.map_eq_some_iff] at hp
  obtain ⟨ex, _, rfl⟩ := hp
  refine ⟨?_, e, he, ht, rfl, hs.symm, hd⟩
  simp only []; rw [hs, ← hd]; exact Ne.symm (hd ▸ hne)

theorem forwardOf_ok (W : World) (cooldown now : Int) (t : Nat) (p : Probe)
    (hp : p ∈ (forwardOf W.node (contradictions W cooldown now) W.suppressed t).1) :
    ProbeOK W p := by
  unfold forwardOf at hp
  split at hp
  · simp at hp
  · rename_i tn _
    suffices ∀ (l : List (Edge × Node)) (acc : List Probe × List (Nat × Nat)),
        (∀ es ∈ l, es ∈ contradictions W cooldown now tn) → (∀ q ∈ acc.1, ProbeOK W q) →
        ∀ q ∈ (l.foldl (fun acc es =>
          match mkProbe W.suppressed tn es.1 es.2 false with
          | some p => (acc.1 ++ [p], acc.2)
          | none => (acc.1, acc.2 ++ [(tn.id, es.2.id)])) acc).1, ProbeOK W q by
      exact this _ _ (fun es h => h) (by simp) p hp
    intro l
    induction l with
    | nil => intro acc _ hacc; exact hacc
    | cons es rest ih =>
      intro acc hl hacc
      simp only [List.foldl_cons]
      apply ih _ (fun x hx => hl x (List.mem_cons_of_mem _ hx))
      cases hm : mkProbe W.suppressed tn es.1 es.2 false with
      | none => exact hacc
      | some q =>
        intro x hx
        simp only [List.mem_append, List.mem_singleton] at hx
        rcases hx with hx | hx
        · exact hacc x hx
        · subst hx
          exact mkProbe_ok W cooldown now tn es.1 es.2 false _ (hl es (List.mem_cons_self ..)) hm

theorem reverseOf_ok (W : World) (cooldown now : Int) (fwd : List Probe) (q p : Probe)
    (hp : p ∈ (reverseOf W.node (contradictions W cooldown now) W.suppressed fwd q).1) :
    ProbeOK W p := by
  unfold reverseOf at hp
  split at hp
  · simp at hp
  · split at hp
    · rename_i t _ _ _
      split at hp
      · rename_i e s hf
        have hm := List.mem_of_find?_eq_some hf
        split at hp
        · rename_i r hr
          simp only [List.mem_singleton] at hp; subst hp
          exact mkProbe_ok W cooldown now t e s true _ hm hr
        · simp at hp
      · simp at hp
    · simp at hp

/-- **Every gathered probe rests on a `Contradicts` edge between two
different concepts**, so no concept is ever probed, and so never superseded,
by itself. -/
theorem probesFor_ok (W : World) (cooldown now : Int) (targets : List Nat) (p : Probe)
    (hp : p ∈ probesFor W cooldown now targets) : ProbeOK W p := by
  simp only [probesFor, probesWith, List.mem_filter, List.mem_append, List.mem_flatten,
    List.mem_map] at hp
  obtain ⟨hp | hp, _⟩ := hp
  · obtain ⟨l, ⟨fw, ⟨t, _, rfl⟩, rfl⟩, hl⟩ := hp
    exact forwardOf_ok W cooldown now t p hl
  · obtain ⟨l, ⟨rv, ⟨q, _, rfl⟩, rfl⟩, hl⟩ := hp
    exact reverseOf_ok W cooldown now _ q p hl

/-- The supersession pass never probes a concept against itself. -/
theorem probesFor_noSelf (W : World) (cooldown now : Int) (targets : List Nat) :
    NoSelf (probesFor W cooldown now targets) :=
  fun p hp => (probesFor_ok W cooldown now targets p hp).1

/-- **Option off: nothing is probed** (`gather` and `gather_holds` return
`Probes::default()`), so nothing is decided. -/
theorem gather_off (W : World) (cooldown now : Int) : gather W false cooldown now = [] := rfl

/-- With the supersession option off, `gather_holds` returns no probes. -/
theorem gatherHolds_off (W : World) (cooldown now : Int) (window : List Nat) :
    gatherHolds W false cooldown now window = [] := rfl

/-- **The #198 edge is never read as a contradiction.** Adding
`SuspectedSupersedes` edges changes no contradiction, so no probe and no
supersession verdict. -/
theorem contradictions_ignore_suspected (W : World) (F : List Edge)
    (hF : ∀ e ∈ F, e.ty = .suspected) (cooldown now : Int) :
    contradictions { W with edges := W.edges ++ F } cooldown now =
      contradictions W cooldown now := by
  funext t
  simp only [contradictions, List.filter_append, List.filterMap_append]
  have : F.filter (fun e => e.ty == .contradicts && e.dst == t.id && e.src != t.id &&
      decide (e.created ≤ now)) = [] := by
    rw [List.filter_eq_nil_iff]
    intro e he
    simp [hF e he]
  rw [this]
  simp
  rfl

/-- The supersession pass's probes ignore `SuspectedSupersedes` edges: adding
any changes no probe. -/
theorem probesFor_ignore_suspected (W : World) (F : List Edge)
    (hF : ∀ e ∈ F, e.ty = .suspected) (cooldown now : Int) (targets : List Nat) :
    probesFor { W with edges := W.edges ++ F } cooldown now targets =
      probesFor W cooldown now targets := by
  unfold probesFor
  rw [contradictions_ignore_suspected W F hF]
  rfl

/-! ## Applying the verdicts

Each verdict is re-checked at apply time (`still_holds`). `holds v` stands
for every check but the target's status: it reads the edge, the evidence,
sessions and the cooldowns as of the start of the cycle, none of which an
earlier supersession of the cycle changes. The status check is explicit: the
target must be Canonical *now*. The event then goes through the write gate
(`Canon.gate`) as `Canonical → None`. Supersession writes no edge and changes
no other concept. -/

/-- The supersession pass of `apply`, on the statuses. -/
def applySup (holds : Verdict → Bool) : (Nat → Status) → List Verdict → (Nat → Status)
  | st, [] => st
  | st, v :: vs =>
    let st' : Nat → Status :=
      if holds v ∧ st v.target = .canonical then
        match gate (st v.target) .canonical .none with
        | some s => fun n => if n = v.target then s else st n
        | none => st
      else st
    applySup holds st' vs

/-- Whether some verdict that holds names `n`. -/
def superFlag (holds : Verdict → Bool) (vs : List Verdict) (n : Nat) : Bool :=
  vs.any fun v => v.target == n && holds v

theorem applySup_eq (holds : Verdict → Bool) :
    ∀ (vs : List Verdict) (st : Nat → Status) (n : Nat),
      applySup holds st vs n =
        if st n = .canonical ∧ superFlag holds vs n = true then .none else st n := by
  intro vs
  induction vs with
  | nil => intro st n; simp [applySup, superFlag]
  | cons v vs ih =>
    intro st n
    simp only [applySup]
    rw [ih]
    by_cases hc : holds v = true ∧ st v.target = .canonical
    · rw [ite_eq_left hc]
      have hg : gate (st v.target) .canonical .none = some .none := by
        simp [gate, hc.2, legal]
      rw [hg]
      by_cases hn : n = v.target
      · subst hn
        simp [superFlag, hc.1, hc.2]
      · have hb : (v.target == n) = false := by simp [Ne.symm hn]
        simp [hn, superFlag, hb]
    · rw [ite_eq_right hc]
      by_cases hn : n = v.target
      · subst hn
        by_cases hst : st v.target = .canonical
        · have : holds v = false := by
            cases h : holds v
            · rfl
            · exact absurd ⟨h, hst⟩ hc
          simp [superFlag, this]
        · simp [hst]
      · have hb : (v.target == n) = false := by simp [Ne.symm hn]
        simp [superFlag, hb]

/-- **Agreement with `Canon.cycle`'s supersession pass.** On every concept the
pass is `Canon.cycle` with the supersede flag "some holding verdict names it"
and every other flag off. -/
theorem applySup_eq_cycle (holds : Verdict → Bool) (vs : List Verdict) (st : Nat → Status)
    (n : Nat) :
    applySup holds st vs n = Canon.cycle (st n) ⟨superFlag holds vs n, false, false, false, false⟩ := by
  rw [applySup_eq]
  cases h : st n <;> cases superFlag holds vs n <;> rfl

/-- **Only along the one legal demotion.** A concept the pass changes was
Canonical and is now None, a transition the write gate admits; by
`Canon.legal_demotion_unique` it is the only rank-decreasing one there is. -/
theorem applySup_legal (holds : Verdict → Bool) (vs : List Verdict) (st : Nat → Status) (n : Nat)
    (h : applySup holds st vs n ≠ st n) :
    st n = .canonical ∧ applySup holds st vs n = .none ∧
      legal (st n) (applySup holds st vs n) = true := by
  rw [applySup_eq] at h ⊢
  by_cases hc : st n = .canonical ∧ superFlag holds vs n = true
  · rw [ite_eq_left hc]; exact ⟨hc.1, rfl, by rw [hc.1]; rfl⟩
  · rw [ite_eq_right hc] at h; exact absurd rfl h

/-- **The one legal demotion.** Whatever the pass changes moves along the
unique rank-decreasing edge of the gate (`Canon.legal_demotion_unique`): it
lowers the rank, and every legal demotion is this one. -/
theorem applySup_unique (holds : Verdict → Bool) (vs : List Verdict) (st : Nat → Status) (n : Nat)
    (h : applySup holds st vs n ≠ st n) :
    (applySup holds st vs n).rank < (st n).rank ∧
      ∀ a b, legal a b = true → b.rank < a.rank → a = st n ∧ b = applySup holds st vs n := by
  obtain ⟨h1, h2, _⟩ := applySup_legal holds vs st n h
  refine ⟨by rw [h1, h2]; decide, ?_⟩
  intro a b hl hr
  obtain ⟨ha, hb⟩ := Canon.legal_demotion_unique a b hl hr
  rw [ha, hb, h1, h2]; exact ⟨rfl, rfl⟩

/-- **Never promotes.** No concept's rank rises, the source's included. -/
theorem applySup_rank_le (holds : Verdict → Bool) (vs : List Verdict) (st : Nat → Status) (n : Nat) :
    (applySup holds st vs n).rank ≤ (st n).rank := by
  rw [applySup_eq]
  split
  · simp [Status.rank]
  · exact Nat.le_refl _

/-- Only a verdict's target can change. -/
theorem applySup_untouched (holds : Verdict → Bool) (vs : List Verdict) (st : Nat → Status)
    (n : Nat) (h : ∀ v ∈ vs, v.target ≠ n) : applySup holds st vs n = st n := by
  rw [applySup_eq]
  have : superFlag holds vs n = false := by
    simp only [superFlag, List.any_eq_false, Bool.and_eq_true, beq_iff_eq, not_and]
    intro v hv ht; exact absurd ht (h v hv)
  simp [this]

/-- **The source is not promoted, nor touched**, unless another verdict
demotes it. -/
theorem applySup_source (holds : Verdict → Bool) (vs : List Verdict) (st : Nat → Status)
    (v : Verdict) (_ : v ∈ vs) (h : ∀ u ∈ vs, u.target ≠ v.source) :
    applySup holds st vs v.source = st v.source :=
  applySup_untouched holds vs st v.source h

/-- **Option off agrees with plain canonization.** With
`canonization_supersede` off the probes are empty, so no verdict, so every
concept's supersede flag is false and the cycle is `Canon.cycle` with no
supersession; the hold is empty too, so Stage 3 is unchanged. -/
theorem off_agrees (W : World) (ev : Nat → Evidence) (window cooldown now : Int)
    (holds : Verdict → Bool) (n : Nat) (s : Status) (f : Canon.Flags) :
    Canon.cycle s { f with supersede := superFlag holds (decideV ev window (gather W false cooldown now)) n } =
      Canon.cycle s { f with supersede := false } := rfl

/-- With the supersession option off, the canon supersession pass demotes
nothing: `gather` probes nothing, so no verdict is applied. -/
theorem off_no_demotion (W : World) (ev : Nat → Evidence) (window cooldown now : Int)
    (holds : Verdict → Bool) (st : Nat → Status) :
    applySup holds st (decideV ev window (gather W false cooldown now)) = st := rfl

/-- The held Venerables: the targets of the hold's verdicts. Stage 3 skips
them. -/
def held (ev : Nat → Evidence) (window : Int) (probes : List Probe) (n : Nat) : Bool :=
  (decideV ev window probes).any fun v => v.target == n

/-- With the supersession option off, `gather_holds` holds no concept, so no
replacement is held off. -/
theorem held_off (W : World) (ev : Nat → Evidence) (window cooldown now : Int) (win : List Nat)
    (n : Nat) : held ev window (gatherHolds W false cooldown now win) n = false := rfl

end Lambo.Supersession
