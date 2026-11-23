/-
# Phase-2 expansion is exactly the bounded neighbourhood, each node once

Model of `expand` in `src/recall/expand.rs`.

The Rust function keeps a `visited` set, a `frontier` and the output vector
`required`. It seeds all three from the phase-1 candidates (first occurrence
wins), then repeats `depth` times: walk the frontier in order, and for each
frontier node walk its traversable out-neighbours in `TRAVERSAL_ORDER`; a
neighbour not yet visited is marked, appended to `required` and to the next
frontier. It stops early when a level discovers nothing.

## Model

* A node is a value of an arbitrary type `ι` with decidable equality.
* `nbrs x` is the list the two inner loops iterate for a frontier node `x`:
  the concatenation over `TRAVERSAL_ORDER` of `out_neighbors_typed(x, ty)`.
  Which edge types are traversable, and their priority, live entirely inside
  `nbrs`; nothing below depends on them.
* `visited` and `required` always hold the same nodes, so the model carries one
  list, `req`, for both.
* Scores are not modelled. Candidates keep their phase-1 score and every other
  member is stamped `UNSCORED`; that is bookkeeping on top of the membership
  and order proved here.

## Results

* `expand_nodup`: no node is recorded twice ("a node is recorded and enqueued
  at most once").
* `expand_sound`: every member is reachable from some candidate by a path of
  at most `depth` traversable edges.
* `expand_complete`: every node reachable that way is a member.
* `expand_prefix`, `discover_of_nodup`: the output starts with the candidates,
  in input order.

The first three together say the result is *exactly* the `depth`-bounded
neighbourhood of the candidate set, with no duplicates, for every graph,
cyclic or not.
-/

namespace Lambo.Expand

variable {ι : Type} [DecidableEq ι]

/-! ## One pass of "insert if not visited" -/

/-- Walk `ts` in order; a node not in `visited` is recorded and marked visited
before the walk continues. Returns the recorded nodes, in order. This is the
`if visited.insert(x) { ...push(x) }` loop, both for the candidates and for one
BFS level. -/
def discover : List ι → List ι → List ι
  | _, [] => []
  | visited, t :: ts =>
    if t ∈ visited then discover visited ts else t :: discover (t :: visited) ts

theorem discover_seen {visited : List ι} {t : ι} (h : t ∈ visited) (ts : List ι) :
    discover visited (t :: ts) = discover visited ts := by
  simp [discover, h]

theorem discover_new {visited : List ι} {t : ι} (h : t ∉ visited) (ts : List ι) :
    discover visited (t :: ts) = t :: discover (t :: visited) ts := by
  simp [discover, h]

/-- A recorded node came from the walked list and had not been visited. -/
theorem discover_sound :
    ∀ (ts visited : List ι) (x : ι), x ∈ discover visited ts → x ∈ ts ∧ x ∉ visited := by
  intro ts
  induction ts with
  | nil =>
    intro visited x hx
    simp [discover] at hx
  | cons t ts ih =>
    intro visited x hx
    by_cases h : t ∈ visited
    · rw [discover_seen h] at hx
      have hrec := ih visited x hx
      exact ⟨List.mem_cons.mpr (Or.inr hrec.1), hrec.2⟩
    · rw [discover_new h] at hx
      cases List.mem_cons.mp hx with
      | inl heq =>
        rw [heq]
        exact ⟨List.mem_cons.mpr (Or.inl rfl), h⟩
      | inr hmem =>
        have hrec := ih (t :: visited) x hmem
        exact ⟨List.mem_cons.mpr (Or.inr hrec.1),
          fun hv => hrec.2 (List.mem_cons.mpr (Or.inr hv))⟩

/-- Every walked node ends up visited: it was already, or it is recorded. -/
theorem discover_complete :
    ∀ (ts visited : List ι) (x : ι), x ∈ ts → x ∈ visited ∨ x ∈ discover visited ts := by
  intro ts
  induction ts with
  | nil =>
    intro visited x hx
    simp at hx
  | cons t ts ih =>
    intro visited x hx
    by_cases h : t ∈ visited
    · rw [discover_seen h]
      cases List.mem_cons.mp hx with
      | inl heq =>
        rw [heq]
        exact Or.inl h
      | inr hmem => exact ih visited x hmem
    · rw [discover_new h]
      cases List.mem_cons.mp hx with
      | inl heq => exact Or.inr (List.mem_cons.mpr (Or.inl heq))
      | inr hmem =>
        cases ih (t :: visited) x hmem with
        | inl hv =>
          cases List.mem_cons.mp hv with
          | inl heq => exact Or.inr (List.mem_cons.mpr (Or.inl heq))
          | inr hv' => exact Or.inl hv'
        | inr hd => exact Or.inr (List.mem_cons.mpr (Or.inr hd))

/-- No node is recorded twice in one pass. -/
theorem discover_nodup :
    ∀ (ts visited : List ι), (discover visited ts).Pairwise (fun a b => a ≠ b) := by
  intro ts
  induction ts with
  | nil =>
    intro visited
    first
      | exact List.Pairwise.nil
      | simp [discover]
  | cons t ts ih =>
    intro visited
    by_cases h : t ∈ visited
    · rw [discover_seen h]
      exact ih visited
    · rw [discover_new h]
      refine List.pairwise_cons.mpr ⟨?_, ih (t :: visited)⟩
      intro y hy heq
      exact (discover_sound ts (t :: visited) y hy).2 (List.mem_cons.mpr (Or.inl heq.symm))

/-- A duplicate-free list of unvisited nodes is recorded unchanged: phase-1
candidates (which carry distinct ids) pass through in input order. -/
theorem discover_of_nodup :
    ∀ (ts visited : List ι), ts.Pairwise (fun a b => a ≠ b) → (∀ x, x ∈ ts → x ∉ visited) →
      discover visited ts = ts := by
  intro ts
  induction ts with
  | nil =>
    intro visited _ _
    simp [discover]
  | cons t ts ih =>
    intro visited hp hv
    have hp' := List.pairwise_cons.mp hp
    have ht : t ∉ visited := hv t (List.mem_cons.mpr (Or.inl rfl))
    rw [discover_new ht]
    have hrec : discover (t :: visited) ts = ts := by
      apply ih (t :: visited) hp'.2
      intro x hx hmem
      cases List.mem_cons.mp hmem with
      | inl e => exact hp'.1 x hx e.symm
      | inr hv' => exact hv x (List.mem_cons.mpr (Or.inr hx)) hv'
    rw [hrec]

/-! ## The level loop -/

/-- The nodes one level discovers: walk the frontier in order, each frontier
node's neighbours in priority order, recording the unvisited ones. -/
def nextOf (nbrs : ι → List ι) (req frontier : List ι) : List ι :=
  discover req (frontier.flatMap nbrs)

/-- `for _ in 0..depth { ...; if next.is_empty() { break } frontier = next }`.
`req` is both `required` and the visited set. -/
def levels (nbrs : ι → List ι) : Nat → List ι → List ι → List ι
  | 0, req, _ => req
  | d + 1, req, frontier =>
    if nextOf nbrs req frontier = [] then req
    else levels nbrs d (req ++ nextOf nbrs req frontier) (nextOf nbrs req frontier)

/-- `expand`: seed from the candidates, then run `depth` levels. -/
def expand (nbrs : ι → List ι) (depth : Nat) (seeds : List ι) : List ι :=
  levels nbrs depth (discover [] seeds) (discover [] seeds)

theorem levels_zero (nbrs : ι → List ι) (req frontier : List ι) :
    levels nbrs 0 req frontier = req := by
  simp [levels]

theorem levels_stop {nbrs : ι → List ι} {req frontier : List ι}
    (h : nextOf nbrs req frontier = []) (d : Nat) :
    levels nbrs (d + 1) req frontier = req := by
  simp [levels, h]

theorem levels_go {nbrs : ι → List ι} {req frontier : List ι}
    (h : nextOf nbrs req frontier ≠ []) (d : Nat) :
    levels nbrs (d + 1) req frontier
      = levels nbrs d (req ++ nextOf nbrs req frontier) (nextOf nbrs req frontier) := by
  simp [levels, h]

/-- The loop only appends: what was recorded stays recorded, in place. -/
theorem levels_prefix (nbrs : ι → List ι) :
    ∀ (d : Nat) (req frontier : List ι), ∃ t, levels nbrs d req frontier = req ++ t := by
  intro d
  induction d with
  | zero =>
    intro req frontier
    exact ⟨[], by rw [levels_zero] <;> simp⟩
  | succ d ih =>
    intro req frontier
    by_cases h : nextOf nbrs req frontier = []
    · exact ⟨[], by rw [levels_stop h] <;> simp⟩
    · cases ih (req ++ nextOf nbrs req frontier) (nextOf nbrs req frontier) with
      | intro t ht =>
        exact ⟨nextOf nbrs req frontier ++ t, by rw [levels_go h, ht, List.append_assoc]⟩

theorem levels_mono (nbrs : ι → List ι) (d : Nat) (req frontier : List ι) (x : ι)
    (hx : x ∈ req) : x ∈ levels nbrs d req frontier := by
  cases levels_prefix nbrs d req frontier with
  | intro t ht =>
    rw [ht]
    exact List.mem_append.mpr (Or.inl hx)

theorem levels_nodup (nbrs : ι → List ι) :
    ∀ (d : Nat) (req frontier : List ι), req.Pairwise (fun a b => a ≠ b) →
      (levels nbrs d req frontier).Pairwise (fun a b => a ≠ b) := by
  intro d
  induction d with
  | zero =>
    intro req frontier hreq
    rw [levels_zero]
    exact hreq
  | succ d ih =>
    intro req frontier hreq
    by_cases h : nextOf nbrs req frontier = []
    · rw [levels_stop h]
      exact hreq
    · rw [levels_go h]
      apply ih
      refine List.pairwise_append.mpr ⟨hreq, discover_nodup _ req, ?_⟩
      intro a ha b hb heq
      have hb' : b ∉ req := (discover_sound (frontier.flatMap nbrs) req b hb).2
      apply hb'
      rw [← heq]
      exact ha

/-! ## Paths -/

/-- `Path nbrs n x z`: `z` is reached from `x` by exactly `n` traversable
edges. -/
inductive Path (nbrs : ι → List ι) : Nat → ι → ι → Prop
  | refl (x : ι) : Path nbrs 0 x x
  | step {n : Nat} {x y z : ι} : y ∈ nbrs x → Path nbrs n y z → Path nbrs (n + 1) x z

omit [DecidableEq ι] in
theorem Path.zero_inv {nbrs : ι → List ι} {x z : ι} (h : Path nbrs 0 x z) : x = z := by
  cases h
  rfl

omit [DecidableEq ι] in
theorem Path.succ_inv {nbrs : ι → List ι} {n : Nat} {x z : ι} (h : Path nbrs (n + 1) x z) :
    ∃ y, y ∈ nbrs x ∧ Path nbrs n y z := by
  cases h with
  | step hxy hrest => exact ⟨_, hxy, hrest⟩

omit [DecidableEq ι] in
/-- Extend a path by one edge at its far end. -/
theorem Path.snoc {nbrs : ι → List ι} {n : Nat} {x y : ι} (h : Path nbrs n x y) :
    ∀ z, z ∈ nbrs y → Path nbrs (n + 1) x z := by
  induction h with
  | refl x =>
    intro z hz
    exact Path.step hz (Path.refl z)
  | step hxy _ ih =>
    intro z hz
    exact Path.step hxy (ih z hz)

omit [DecidableEq ι] in
/-- A set closed under `nbrs` contains every path that starts in it. -/
theorem Path.stays {nbrs : ι → List ι} {S : List ι}
    (hclosed : ∀ a, a ∈ S → ∀ b, b ∈ nbrs a → b ∈ S)
    {n : Nat} {x z : ι} (h : Path nbrs n x z) : x ∈ S → z ∈ S := by
  induction h with
  | refl x =>
    intro hx
    exact hx
  | step hxy _ ih =>
    intro hx
    exact ih (hclosed _ hx _ hxy)

/-! ## Completeness -/

/-- Loop invariant behind completeness. If every recorded node outside the
frontier already has all its neighbours recorded, then `d` more levels record
everything within `d` edges of a recorded node. -/
theorem levels_complete (nbrs : ι → List ι) :
    ∀ (d : Nat) (req frontier : List ι),
      (∀ a, a ∈ req → a ∉ frontier → ∀ b, b ∈ nbrs a → b ∈ req) →
      ∀ (n : Nat) (x z : ι), Path nbrs n x z → n ≤ d → x ∈ req →
        z ∈ levels nbrs d req frontier := by
  intro d
  induction d with
  | zero =>
    intro req frontier _ n x z hp hn hx
    have hn0 : n = 0 := by omega
    subst hn0
    have e := Path.zero_inv hp
    rw [levels_zero, ← e]
    exact hx
  | succ d ih =>
    intro req frontier hclosed n x z hp hn hx
    cases n with
    | zero =>
      have e := Path.zero_inv hp
      rw [← e]
      exact levels_mono nbrs (d + 1) req frontier x hx
    | succ m =>
      cases Path.succ_inv hp with
      | intro y hy =>
        have hxy := hy.1
        have hrest := hy.2
        -- Every neighbour of a recorded node is recorded after this level.
        have hstep : ∀ a, a ∈ req → ∀ b, b ∈ nbrs a →
            b ∈ req ∨ b ∈ nextOf nbrs req frontier := by
          intro a ha b hb
          by_cases haf : a ∈ frontier
          · exact discover_complete (frontier.flatMap nbrs) req b
              (List.mem_flatMap.mpr ⟨a, haf, hb⟩)
          · exact Or.inl (hclosed a ha haf b hb)
        by_cases hnext : nextOf nbrs req frontier = []
        · -- Nothing new: `req` is closed, so the rest of the path stays inside.
          rw [levels_stop hnext]
          have hcl : ∀ a, a ∈ req → ∀ b, b ∈ nbrs a → b ∈ req := by
            intro a ha b hb
            cases hstep a ha b hb with
            | inl h => exact h
            | inr h =>
              rw [hnext] at h
              simp at h
          exact Path.stays hcl hrest (hcl x hx y hxy)
        · rw [levels_go hnext]
          have hyreq : y ∈ req ++ nextOf nbrs req frontier :=
            List.mem_append.mpr (hstep x hx y hxy)
          have hclosed' : ∀ a, a ∈ req ++ nextOf nbrs req frontier →
              a ∉ nextOf nbrs req frontier →
              ∀ b, b ∈ nbrs a → b ∈ req ++ nextOf nbrs req frontier := by
            intro a ha hanext b hb
            have hareq : a ∈ req := by
              cases List.mem_append.mp ha with
              | inl h => exact h
              | inr h => exact absurd h hanext
            exact List.mem_append.mpr (hstep a hareq b hb)
          exact ih (req ++ nextOf nbrs req frontier) (nextOf nbrs req frontier) hclosed'
            m y z hrest (by omega) hyreq

/-- **Completeness.** Every node within `depth` traversable edges of a
candidate is in the expansion. -/
theorem expand_complete (nbrs : ι → List ι) (depth : Nat) (seeds : List ι)
    {s x : ι} {n : Nat} (hs : s ∈ seeds) (hp : Path nbrs n s x) (hn : n ≤ depth) :
    x ∈ expand nbrs depth seeds := by
  have hseed : s ∈ discover [] seeds := by
    cases discover_complete seeds [] s hs with
    | inl h => simp at h
    | inr h => exact h
  exact levels_complete nbrs depth (discover [] seeds) (discover [] seeds)
    (fun a ha hna => absurd ha hna) n s x hp hn hseed

/-! ## Soundness -/

/-- `x` is within `l` traversable edges of some candidate. -/
def Within (nbrs : ι → List ι) (seeds : List ι) (l : Nat) (x : ι) : Prop :=
  ∃ s, s ∈ seeds ∧ ∃ k, k ≤ l ∧ Path nbrs k s x

omit [DecidableEq ι] in
theorem Within.mono {nbrs : ι → List ι} {seeds : List ι} {l l' : Nat} {x : ι}
    (hle : l ≤ l') (h : Within nbrs seeds l x) : Within nbrs seeds l' x := by
  cases h with
  | intro s hs =>
    cases hs.2 with
    | intro k hk => exact ⟨s, hs.1, k, Nat.le_trans hk.1 hle, hk.2⟩

/-- Loop invariant behind soundness: if everything recorded so far is within
`l` edges of a candidate, everything recorded after `d` more levels is within
`l + d`. -/
theorem levels_sound (nbrs : ι → List ι) (seeds : List ι) :
    ∀ (d l : Nat) (req frontier : List ι),
      (∀ x, x ∈ req → Within nbrs seeds l x) →
      (∀ x, x ∈ frontier → Within nbrs seeds l x) →
      ∀ x, x ∈ levels nbrs d req frontier → Within nbrs seeds (l + d) x := by
  intro d
  induction d with
  | zero =>
    intro l req frontier hreq _ x hx
    rw [levels_zero] at hx
    exact hreq x hx
  | succ d ih =>
    intro l req frontier hreq hfr x hx
    by_cases hnext : nextOf nbrs req frontier = []
    · rw [levels_stop hnext] at hx
      exact Within.mono (by omega) (hreq x hx)
    · rw [levels_go hnext] at hx
      have hnew : ∀ y, y ∈ nextOf nbrs req frontier → Within nbrs seeds (l + 1) y := by
        intro y hy
        have hy' : y ∈ frontier.flatMap nbrs :=
          (discover_sound (frontier.flatMap nbrs) req y hy).1
        cases List.mem_flatMap.mp hy' with
        | intro f hf =>
          cases hfr f hf.1 with
          | intro s hs =>
            cases hs.2 with
            | intro k hk =>
              have hk1 : k ≤ l := hk.1
              exact ⟨s, hs.1, k + 1, by omega, Path.snoc hk.2 y hf.2⟩
      have hreq' : ∀ y, y ∈ req ++ nextOf nbrs req frontier →
          Within nbrs seeds (l + 1) y := by
        intro y hy
        cases List.mem_append.mp hy with
        | inl h => exact Within.mono (by omega) (hreq y h)
        | inr h => exact hnew y h
      have hres := ih (l + 1) (req ++ nextOf nbrs req frontier) (nextOf nbrs req frontier)
        hreq' hnew x hx
      exact Within.mono (by omega) hres

/-- **Soundness.** Every member of the expansion is within `depth` traversable
edges of a candidate. -/
theorem expand_sound (nbrs : ι → List ι) (depth : Nat) (seeds : List ι) {x : ι}
    (hx : x ∈ expand nbrs depth seeds) : Within nbrs seeds depth x := by
  have h0 : ∀ y, y ∈ discover [] seeds → Within nbrs seeds 0 y := by
    intro y hy
    exact ⟨y, (discover_sound seeds [] y hy).1, 0, Nat.le_refl 0, Path.refl y⟩
  have hres := levels_sound nbrs seeds depth 0 (discover [] seeds) (discover [] seeds)
    h0 h0 x hx
  exact Within.mono (by omega) hres

/-! ## No duplicates, candidates first -/

/-- **At most once.** No node appears twice in `required`, on any graph: a
cycle or a diamond cannot make a node be recorded again. -/
theorem expand_nodup (nbrs : ι → List ι) (depth : Nat) (seeds : List ι) :
    (expand nbrs depth seeds).Pairwise (fun a b => a ≠ b) :=
  levels_nodup nbrs depth (discover [] seeds) (discover [] seeds) (discover_nodup seeds [])

/-- The expansion starts with the (de-duplicated) candidates, in input order;
discovered neighbours follow. -/
theorem expand_prefix (nbrs : ι → List ι) (depth : Nat) (seeds : List ι) :
    ∃ t, expand nbrs depth seeds = discover [] seeds ++ t :=
  levels_prefix nbrs depth (discover [] seeds) (discover [] seeds)

/-- With distinct candidate ids, `expand` returns the candidates first, in
their input order. -/
theorem expand_prefix_of_nodup (nbrs : ι → List ι) (depth : Nat) (seeds : List ι)
    (hs : seeds.Pairwise (fun a b => a ≠ b)) : ∃ t, expand nbrs depth seeds = seeds ++ t := by
  have e : discover [] seeds = seeds :=
    discover_of_nodup seeds [] hs (fun x _ hx => by simp at hx)
  cases expand_prefix nbrs depth seeds with
  | intro t ht =>
    rw [e] at ht
    exact ⟨t, ht⟩

/-- `expand` (`src/recall/expand.rs`) at `depth = 0` returns the de-duplicated
candidates and nothing else. -/
theorem expand_depth_zero (nbrs : ι → List ι) (seeds : List ι) :
    expand nbrs 0 seeds = discover [] seeds := by
  unfold expand
  rw [levels_zero]

end Lambo.Expand
