import LamboProofs.Contract.Reembed

/-
# The durable re-embed of a contract-less session (#322)

Models the store applying a flushed batch in its own order (`MemoryStore::flush`,
`plan_flush` in `src/store/batch.rs`, which makes every `SetEmbedding` a
barrier, and the adapters' `write_rows.rs`), including the first-stamp
legacy quarantine, and `Graph::reembed`'s batch order after #322
(`reembedOrdered`), against `Reembed.lean`'s in-memory results.

Results: before #322 a re-embed of a contract-less session reported full
coverage while the store's first-stamp quarantine erased every new vector
(`reembed_contractless_durable_loses_vectors`, `example_contractless_bug`);
the fixed order emits the `SetEmbedding` first on such a session, so the
store keeps every new vector and matches the graph, and the batch of a
stamped session is unchanged (`reembedOrdered_contract_first`).

Boundary: one batch at a time, each in one transaction; SQLite's null on a
width change is left out (a re-embed never changes width,
`reembed_refuses_width`); flush failures and retries are `Flush`.
-/

namespace Lambo.Contract

variable {K M : Type} [DecidableEq K] [DecidableEq M]

/-! # The durable re-embed of a contract-less session (#322)

Everything above reads a re-embed's result off the session it returns, which
is the in-memory graph. The store applies the batch, and the store's rules
are not the graph's. This part models the store applying a flushed batch.

Rust items modelled (at the fix commit):

* Applying one flushed batch, in its own order (`MemoryStore::flush`,
  `src/store/memory.rs`; SQLite and Postgres through
  `crate::store::batch::plan_flush`, which makes every `SetEmbedding` a
  barrier, `src/store/sqlite/write_rows.rs` and `src/store/pg/write_rows.rs`):
  `applyBatch`. An upsert replaces the concept's vector (`applyMut`). A
  `SetEmbedding` of a contract on a store with none is also the **first-stamp
  legacy quarantine**: every vector in the session is nulled, then the
  contract is stamped (`memory.rs` `SetEmbedding` arm; SQLite `set_embedding`;
  Postgres `QUARANTINE_LEGACY_EMBEDDINGS_SQL`), in the same transaction.
  SQLite also nulls on a width change; a re-embed never changes width
  (`reembed_refuses_width`), so that rule is left out.
* `quarantine` (above) is the load: the graph of a session with no durable
  contract holds no vector, while the store's rows keep theirs.
* `Graph::reembed` after the fix (`src/graph/graph/embeddings.rs`):
  `reembedOrdered`. On a session with no contract it emits the
  `SetEmbedding` **first**, so the store's quarantine runs before the new
  vectors land; on a stamped session the batch is unchanged (vectors first,
  contract last).
* `lambo re-embed` after the fix: `reembedCmd322`, `reembedCmdFixed` with
  `reembedOrdered` in place of `reembed`.

## The bug (`reembed_contractless_durable_loses_vectors`)

On a contract-less session the old batch is the upserts, then
`SetEmbedding`. The store writes the new vectors, then the first stamp nulls
every vector, the new ones included. The graph says every text concept
carries a vector under the live contract (the coverage report reads the
graph: N/N); the store holds the live contract and no vector at all
(`example_contractless_bug`).

## Results

* `reembedOrdered_contractless_durable`, `reembedCmd322_contractless_durable`:
  after a successful full re-embed of a non-empty contract-less session, the
  store is under the live contract, holds one space, and every text concept
  holds a vector of the live contract.
* `reembedOrdered_contractless_matches_graph`: with distinct node ids (the
  graph's node map makes them distinct), the store after the batch is exactly
  the graph the re-embed returns, so the coverage the verb reports is the
  store's.
* `reembedOrdered_stamped_eq`, `reembedCmd322_stamped_agrees`: on a session
  that already has a contract nothing changes, the batch included.
  `reembedOrdered_graph_eq`: in every case the graph is the old one; only the
  batch order moved (`reembedBatchOrdered_perm`), with the `SetEmbedding`
  first on a contract-less session (`reembedOrdered_contract_first`).
* `missingOnly_contractless_writes_nothing`: `--missing-only` has no such
  hazard. On a non-empty contract-less session it is refused up front,
  before anything is embedded or written (the backfill never stamps a
  contract).

Modelling the store forced one observation: the old theorems are all true of
the graph, and `reembed_contract_last` ("vectors first, contract last") was
read as a durability argument. It is one only for a session that already has
a contract. For the first stamp the order has to be the other way round, the
same order an ordinary first write already uses (`write` emits its
`SetEmbedding` before its upsert).

Not modelled: the tier's index (`Tiered.lean`). With the `SetEmbedding`
first, its projection meets the contract before the upserts and indexes them
under it, as for any first write.
-/

/-- One mutation applied by the store. A contract stamped on a store with no
contract nulls every vector first (the first-stamp legacy quarantine). -/
def applyMut (d : Session K M) : Mut K M → Session K M
  | .upsert id v =>
    { d with concepts := d.concepts.map (fun c => if c.id = id then { c with vec := v } else c) }
  | .setEmbedding c =>
    { contract := c,
      concepts := if d.contract.isNone && c.isSome
        then d.concepts.map (fun x => { x with vec := none }) else d.concepts }

/-- One flushed batch, applied in order in one transaction. -/
def applyBatch (d : Session K M) (m : List (Mut K M)) : Session K M := m.foldl applyMut d

/-- The fixed batch: on a session with no contract, the contract first. -/
def reembedBatchOrdered (s : Session K M) (live : Contract K M) (ids : List Nat) (drop : Bool) :
    List (Mut K M) :=
  if s.contract = none then
    .setEmbedding (some live) :: (ids.map (fun i => .upsert i (some live)) ++
      (if drop then (imageVectorIds s).map (fun i => .upsert i none) else []))
  else reembedBatch s live ids drop

/-- `Graph::reembed` after the fix: the same checks and graph, the fixed batch. -/
def reembedOrdered (s : Session K M) (live : Contract K M) (ids : List Nat) (drop : Bool) :
    Option (Session K M × List (Mut K M)) :=
  if reembedOk s live ids drop then
    some (⟨some live, rewrite s.concepts ids live drop⟩, reembedBatchOrdered s live ids drop)
  else none

/-- `lambo re-embed` after the fix for #322: `reembedCmdFixed` with
`reembedOrdered`, and `--missing-only` refused up front on a non-empty
session with no contract (the backfill would refuse it anyway, after
embedding). -/
def reembedCmd322 (s : Session K M) (live : Contract K M)
    (allow missingOnly drop same repin : Bool) (mode : CanaryMode) (pin : PinState) :
    Option (Session K M × List (Mut K M)) :=
  if allow then none
  else if missingOnly && (drop || same || repin) then none
  else if repin && decide (mode = .off) then none
  else if missingOnly && decide (s.contract = some live) &&
      !canaryAdmits mode pin.isDrifted false then none
  else if !missingOnly && !(imageVectorIds s).isEmpty && !drop then none
  else if s.concepts.isEmpty then some (s, [])
  else if missingOnly && decide (s.contract = none) then none
  else if missingOnly then
    let ids := (s.concepts.filter (fun c => !c.image && c.vec.isNone)).map (·.id)
    if ids.isEmpty then some (s, []) else backfill s live ids
  else if same != decide (s.contract = some live) then none
  else if !(decide (mode = .off) || repin || pin.safeToReplace) then none
  else if same then reembedSame s live (textIds s) drop
  else reembedOrdered s live (textIds s) drop

/-! ### The store applying a batch -/

omit [DecidableEq K] [DecidableEq M] in
theorem applyBatch_append (d : Session K M) (a b : List (Mut K M)) :
    applyBatch d (a ++ b) = applyBatch (applyBatch d a) b := by
  simp [applyBatch, List.foldl_append]

omit [DecidableEq K] [DecidableEq M] in
/-- Upserts of one vector `v` set it on every listed concept. -/
theorem applyBatch_upserts (v : Option (Contract K M)) :
    ∀ (ids : List Nat) (d : Session K M),
      applyBatch d (ids.map (fun i => .upsert i v)) =
        ({ d with concepts :=
            d.concepts.map (fun c => if c.id ∈ ids then { c with vec := v } else c) } :
          Session K M) := by
  intro ids
  induction ids with
  | nil => intro d; simp [applyBatch]
  | cons i rest ih =>
    intro d
    show applyBatch (applyMut d (.upsert i v)) (rest.map (fun i => .upsert i v)) = _
    rw [ih]
    simp only [applyMut, List.map_map]
    congr 1
    apply List.map_congr_left
    intro c _
    by_cases hi : c.id = i
    · by_cases hr : c.id ∈ rest <;> simp [hi]
    · by_cases hr : c.id ∈ rest <;> simp [hi, hr]

omit [DecidableEq K] [DecidableEq M] in
/-- Upserts never move the store's contract. -/
theorem applyBatch_upserts_contract :
    ∀ (l : List (Mut K M)) (d : Session K M), (∀ x ∈ l, x.isUpsert = true) →
      (applyBatch d l).contract = d.contract := by
  intro l
  induction l with
  | nil => intro d _; rfl
  | cons x rest ih =>
    intro d hl
    show (applyBatch (applyMut d x) rest).contract = _
    rw [ih _ (fun y hy => hl y (by simp [hy]))]
    have := hl x (by simp)
    cases x with
    | upsert _ _ => rfl
    | setEmbedding _ => simp [Mut.isUpsert] at this

omit [DecidableEq K] [DecidableEq M] in
/-- The image-drop upserts of the re-embed batch are upserts. -/
theorem dropUpserts_isUpsert (s : Session K M) (drop : Bool) :
    ∀ x ∈ (if drop then (imageVectorIds s).map (fun i => (.upsert i none : Mut K M)) else []),
      x.isUpsert = true := by
  intro x hx
  cases drop with
  | true =>
    simp only [ite_true, List.mem_map] at hx
    obtain ⟨i, _, rfl⟩ := hx
    rfl
  | false => simp at hx

omit [DecidableEq K] [DecidableEq M] in
/-- The graph of a loaded contract-less session holds no image vector. -/
theorem imageVectorIds_quarantine (d : Session K M) (hd : d.contract = none) :
    imageVectorIds (quarantine d) = [] := by
  simp [imageVectorIds, quarantine, hd, List.filter_map, Function.comp_def]

/-- What a successful re-embed's checks give: every text concept is covered
once. -/
theorem reembedOk_cover {s : Session K M} {live : Contract K M} {ids : List Nat} {drop : Bool}
    (hg : reembedOk s live ids drop = true) : ∀ i ∈ textIds s, i ∈ ids := by
  unfold reembedOk at hg
  simp only [Bool.and_eq_true, decide_eq_true_eq, List.all_eq_true] at hg
  obtain ⟨⟨⟨⟨_, _⟩, hdist⟩, hlen⟩, hsub⟩ := hg
  exact cover_of_distinct ids (textIds s) hdist hsub hlen

/-- The graph after a successful re-embed: every text concept carries a
vector of the live contract (the coverage the verb reports). -/
theorem reembed_text_full (s s' : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) (m : List (Mut K M)) (h : reembed s live ids drop = some (s', m)) :
    ∀ c ∈ s'.concepts, c.image = false → c.vec = some live := by
  unfold reembed at h
  split at h
  · rename_i hg
    have hcov := reembedOk_cover hg
    simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    intro c' hc' himg'
    simp only [rewrite, List.mem_map] at hc'
    obtain ⟨c, hc, rfl⟩ := hc'
    cases himgc : c.image with
    | false =>
      have hct : c.id ∈ textIds s := by
        simp only [textIds, List.mem_map, List.mem_filter]
        exact ⟨c, ⟨hc, by simp [himgc]⟩, rfl⟩
      simp [hcov c.id hct]
    | true =>
      cases drop <;> simp [himgc] at himg'
  · cases h

/-! ### The bug -/

/-- **The bug (#322).** On a non-empty contract-less session (its graph
loaded through the quarantine), a successful old `re-embed` reports full
coverage from the graph, every text concept carrying a live vector, while the
store ends under the live contract with no vector at all. -/
theorem reembed_contractless_durable_loses_vectors (d s' : Session K M) (live : Contract K M)
    (drop : Bool) (m : List (Mut K M)) (hd : d.contract = none) (hne : d.concepts ≠ [])
    (h : reembedCmd (quarantine d) live false false drop = some (s', m)) :
    (applyBatch d m).contract = some live ∧ hasVectors (applyBatch d m) = false ∧
      ∀ c ∈ s'.concepts, c.image = false → c.vec = some live := by
  have hne' : (quarantine d).concepts ≠ [] := by
    simp [quarantine, hd, hne]
  rw [reembedCmd_full _ live drop hne'] at h
  by_cases hi : (!(imageVectorIds (quarantine d)).isEmpty && !drop) = true
  · rw [ite_eq_left hi] at h; cases h
  rw [ite_eq_right hi] at h
  have hfull := reembed_text_full _ s' live _ drop m h
  unfold reembed at h
  split at h
  · simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    unfold reembedBatch
    rw [applyBatch_append]
    have hc : (applyBatch d ((textIds (quarantine d)).map (fun i => .upsert i (some live)) ++
        (if drop then (imageVectorIds (quarantine d)).map (fun i => .upsert i none) else []))).contract
        = none := by
      rw [applyBatch_upserts_contract _ d, hd]
      intro x hx
      rw [List.mem_append] at hx
      rcases hx with hx | hx
      · simp only [List.mem_map] at hx
        obtain ⟨i, _, rfl⟩ := hx
        rfl
      · exact dropUpserts_isUpsert _ drop x hx
    refine ⟨rfl, ?_, hfull⟩
    show hasVectors (applyMut (applyBatch d _) (.setEmbedding (some live))) = false
    unfold applyMut
    rw [hc]
    simp [hasVectors]
  · cases h

/-! ### The fix -/

/-- **Nothing changes for a stamped session**: the fixed `Graph::reembed` is
the old one, batch included. -/
theorem reembedOrdered_stamped_eq (s : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) (hc : s.contract ≠ none) : reembedOrdered s live ids drop = reembed s live ids drop := by
  simp [reembedOrdered, reembed, reembedBatchOrdered, hc]

/-- The fix moves no graph state: the session `re-embed` leaves in memory is
the old one in every case. -/
theorem reembedOrdered_graph_eq (s : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) :
    (reembedOrdered s live ids drop).map Prod.fst = (reembed s live ids drop).map Prod.fst := by
  unfold reembedOrdered reembed
  split <;> rfl

omit [DecidableEq K] [DecidableEq M] in
/-- The fixed batch holds the old batch's mutations, reordered. -/
theorem reembedBatchOrdered_perm (s : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) : (reembedBatchOrdered s live ids drop).Perm (reembedBatch s live ids drop) := by
  unfold reembedBatchOrdered reembedBatch
  split
  · exact (List.perm_append_singleton _ _).symm
  · exact List.Perm.refl _

/-- On a contract-less session the fixed batch stamps the contract first, then
writes only upserts. -/
theorem reembedOrdered_contract_first (s s' : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) (m : List (Mut K M)) (hc : s.contract = none)
    (h : reembedOrdered s live ids drop = some (s', m)) :
    ∃ rest, m = .setEmbedding (some live) :: rest ∧ ∀ x ∈ rest, x.isUpsert = true := by
  unfold reembedOrdered at h
  split at h
  · simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    refine ⟨ids.map (fun i => .upsert i (some live)) ++
      (if drop then (imageVectorIds s).map (fun i => .upsert i none) else []),
      by simp [reembedBatchOrdered, hc], ?_⟩
    intro x hx
    rw [List.mem_append] at hx
    rcases hx with hx | hx
    · simp only [List.mem_map] at hx
      obtain ⟨i, _, rfl⟩ := hx
      rfl
    · exact dropUpserts_isUpsert _ drop x hx
  · cases h

/-- The store after the fixed batch on a contract-less session: the live
contract, every vector nulled by the first stamp, then the listed ones set. -/
theorem reembedOrdered_contractless_store (d s' : Session K M) (live : Contract K M)
    (ids : List Nat) (drop : Bool) (m : List (Mut K M)) (hd : d.contract = none)
    (h : reembedOrdered (quarantine d) live ids drop = some (s', m)) :
    reembedOk (quarantine d) live ids drop = true ∧
      s' = ⟨some live, rewrite (quarantine d).concepts ids live drop⟩ ∧
      applyBatch d m = ⟨some live, d.concepts.map (fun c =>
        if c.id ∈ ids then { c with vec := some live } else { c with vec := none })⟩ := by
  have hq : (quarantine d).contract = none := by simp [quarantine, hd]
  unfold reembedOrdered at h
  split at h
  · rename_i hg
    simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    refine ⟨hg, rfl, ?_⟩
    simp only [reembedBatchOrdered, hq, ite_true, imageVectorIds_quarantine d hd, List.map_nil,
      ite_self, List.append_nil]
    show applyBatch (applyMut d (.setEmbedding (some live))) _ = _
    rw [applyBatch_upserts]
    simp only [applyMut, hd, Option.isNone_none, Option.isSome_some, Bool.and_self, ite_true,
      List.map_map]
    rfl
  · cases h

/-- **The fix (#322).** After a successful fixed re-embed of a contract-less
session (its graph loaded through the quarantine), the store is under the
live contract, in one space, and every text concept holds a vector of the
live contract. -/
theorem reembedOrdered_contractless_durable (d s' : Session K M) (live : Contract K M)
    (ids : List Nat) (drop : Bool) (m : List (Mut K M)) (hd : d.contract = none)
    (h : reembedOrdered (quarantine d) live ids drop = some (s', m)) :
    (applyBatch d m).contract = some live ∧ oneSpace (applyBatch d m) = true ∧
      ∀ c ∈ (applyBatch d m).concepts, c.image = false → c.vec = some live := by
  obtain ⟨hg, _, hs⟩ := reembedOrdered_contractless_store d s' live ids drop m hd h
  have hcov := reembedOk_cover hg
  rw [hs]
  refine ⟨rfl, ?_, ?_⟩
  · rw [oneSpace_iff]
    intro c hc
    simp only [List.mem_map] at hc
    obtain ⟨c0, _, rfl⟩ := hc
    by_cases hi : c0.id ∈ ids <;> simp [hi]
  · intro c hc himg
    simp only [List.mem_map] at hc
    obtain ⟨c0, hc0, rfl⟩ := hc
    have himg0 : c0.image = false := by
      by_cases hi : c0.id ∈ ids <;> simp_all
    have hct : c0.id ∈ textIds (quarantine d) := by
      simp only [textIds, quarantine, hd, List.mem_map, List.mem_filter]
      exact ⟨{ c0 with vec := none }, ⟨⟨c0, hc0, rfl⟩, by simp [himg0]⟩, rfl⟩
    simp [hcov c0.id hct]

omit [DecidableEq K] [DecidableEq M] in
/-- Distinct ids name distinct concepts. -/
theorem distinct_ids_inj :
    ∀ (l : List (Concept K M)), distinct (l.map (·.id)) = true →
      ∀ a ∈ l, ∀ b ∈ l, a.id = b.id → a = b := by
  intro l
  induction l with
  | nil => intro _ a ha; cases ha
  | cons x t ih =>
    intro hd a ha b hb hab
    simp only [List.map_cons, distinct, Bool.and_eq_true, decide_eq_true_eq] at hd
    obtain ⟨hx, ht⟩ := hd
    rcases List.mem_cons.mp ha with hax | ha'
    · rcases List.mem_cons.mp hb with hbx | hb'
      · rw [hax, hbx]
      · subst hax; exact (hx (List.mem_map.mpr ⟨b, hb', hab.symm⟩)).elim
    · rcases List.mem_cons.mp hb with hbx | hb'
      · subst hbx; exact (hx (List.mem_map.mpr ⟨a, ha', hab⟩)).elim
      · exact ih ht a ha' b hb' hab

/-- **The coverage report is the store's.** With distinct node ids, the store
after a successful fixed re-embed of a contract-less session is exactly the
graph the re-embed returns. -/
theorem reembedOrdered_contractless_matches_graph (d s' : Session K M) (live : Contract K M)
    (ids : List Nat) (drop : Bool) (m : List (Mut K M)) (hd : d.contract = none)
    (hid : distinct (d.concepts.map (·.id)) = true)
    (h : reembedOrdered (quarantine d) live ids drop = some (s', m)) :
    applyBatch d m = s' := by
  obtain ⟨hg, hs', hs⟩ := reembedOrdered_contractless_store d s' live ids drop m hd h
  have hsub : ∀ i ∈ ids, i ∈ textIds (quarantine d) := by
    unfold reembedOk at hg
    simp only [Bool.and_eq_true, decide_eq_true_eq, List.all_eq_true] at hg
    exact hg.2
  rw [hs, hs']
  simp only [quarantine, hd, rewrite, List.map_map]
  congr 1
  apply List.map_congr_left
  intro c hc
  cases himg : c.image with
  | false => by_cases hi : c.id ∈ ids <;> simp [himg, hi]
  | true =>
    have hni : c.id ∉ ids := by
      intro hi
      have ht := hsub c.id hi
      simp only [textIds, quarantine, hd, List.mem_map, List.mem_filter] at ht
      obtain ⟨_, ⟨⟨t, ht, rfl⟩, htimg⟩, hteq⟩ := ht
      have := distinct_ids_inj d.concepts hid t ht c hc hteq
      subst this
      simp [himg] at htimg
    cases drop <;> simp [himg, hni]

/-- The fixed verb on a stamped session is the #296 verb, batch included. -/
theorem reembedCmd322_stamped_agrees (s : Session K M) (live : Contract K M)
    (allow missingOnly drop same repin : Bool) (mode : CanaryMode) (pin : PinState)
    (hc : s.contract ≠ none) :
    reembedCmd322 s live allow missingOnly drop same repin mode pin =
      reembedCmdFixed s live allow missingOnly drop same repin mode pin := by
  have hn : decide (s.contract = none) = false := by simp [hc]
  unfold reembedCmd322 reembedCmdFixed
  rw [reembedOrdered_stamped_eq s live _ drop hc, hn, Bool.and_false]
  simp only [Bool.false_eq_true, ↓reduceIte]

/-- **The fixed verb on a contract-less session.** A successful full run on a
non-empty contract-less session leaves the store under the live contract, in
one space, with every text concept holding a live vector. -/
theorem reembedCmd322_contractless_durable (d s' : Session K M) (live : Contract K M)
    (drop same repin : Bool) (mode : CanaryMode) (pin : PinState) (m : List (Mut K M))
    (hd : d.contract = none) (hne : d.concepts ≠ [])
    (h : reembedCmd322 (quarantine d) live false false drop same repin mode pin = some (s', m)) :
    (applyBatch d m).contract = some live ∧ oneSpace (applyBatch d m) = true ∧
      ∀ c ∈ (applyBatch d m).concepts, c.image = false → c.vec = some live := by
  have hq : (quarantine d).contract = none := by simp [quarantine, hd]
  have hne' : (quarantine d).concepts.isEmpty = false := by
    cases hcs : d.concepts with
    | nil => exact absurd hcs hne
    | cons _ _ => simp [quarantine, hd, hcs]
  unfold reembedCmd322 at h
  cases same <;>
    simp only [Bool.false_eq_true, ite_false, Bool.false_and, hne', Bool.not_false,
      Bool.true_and, hq, reduceCtorEq, decide_false, bne_self_eq_false, ite_true] at h
  · split at h
    · cases h
    · split at h
      · cases h
      · split at h
        · cases h
        · exact reembedOrdered_contractless_durable d s' live _ drop m hd h
  · simp at h

/-- **`--missing-only` has no such hazard**, and is refused up front: on a
non-empty contract-less session the fixed verb refuses it before anything is
embedded or written. (The #296 verb refused it too, through the backfill,
but only after embedding; this pins the up-front refusal.) -/
theorem missingOnly_contractless_writes_nothing (s : Session K M) (live : Contract K M)
    (mode : CanaryMode) (pin : PinState) (hc : s.contract = none) (hne : s.concepts ≠ []) :
    reembedCmd322 s live false true false false false mode pin = none := by
  have hne' : s.concepts.isEmpty = false := by
    cases hcs : s.concepts with
    | nil => exact absurd hcs hne
    | cons _ _ => rfl
  simp [reembedCmd322, hc, hne']

/-- **The #322 scenario.** A contract-less store with a legacy vector on
concept 1 and none on concept 2; its graph is loaded through the quarantine.
The old re-embed to `cA` reports both concepts embedded, and the store ends
under `cA` with no vector. The fixed one leaves both vectors in the store,
which then equals the graph; a stamped session's batch is unchanged. -/
theorem example_contractless_bug :
    let d : Session Nat Nat := ⟨none, [⟨1, false, some cC⟩, ⟨2, false, none⟩]⟩
    let g : Session Nat Nat := ⟨some cA, [⟨1, false, some cA⟩, ⟨2, false, some cA⟩]⟩
    (reembedCmd (quarantine d) cA false false false).map Prod.fst = some g ∧
    (reembedCmd (quarantine d) cA false false false).map (fun r => applyBatch d r.2) =
      some ⟨some cA, [⟨1, false, none⟩, ⟨2, false, none⟩]⟩ ∧
    (reembedCmd322 (quarantine d) cA false false false false false .warn .absent).map Prod.fst =
      some g ∧
    (reembedCmd322 (quarantine d) cA false false false false false .warn .absent).map
      (fun r => applyBatch d r.2) = some g ∧
    reembedCmd322 sA cC false false false false false .warn .absent =
      reembedCmdFixed sA cC false false false false false .warn .absent := by
  decide

end Lambo.Contract
