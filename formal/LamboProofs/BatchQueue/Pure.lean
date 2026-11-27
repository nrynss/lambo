import LamboProofs.Lib.List

/-
# The candle embedder's batch queue: the worker's pieces (#125)

Models the functions the coalescer's worker runs in `src/embed/candle.rs`, each
without the state machine around it: `BatchQueue`'s passes (`take_prioritized`,
`take_singles`: `pass`, `takePrioritized`, `takeSingles`); `length_chunks`
with `FORWARD_WIDTH` and `CHUNK_SLACK_BYTES` (`lengthChunks`, `chunksOf`,
`lcAux`); `serve_take`'s extraction of a chunk from a group and its `remove`
loop (`extract`, `chunkOf`, `removeDesc`); `forward_isolated` with the canary
(`CANARY_TEXT`), the per-text reruns and `width_checked` (`forwardIsolated`);
and one `serve_take` iteration with `MAX_SINGLE_STREAK` and the per-chunk
pull of new singles (`pick`, `pullOf`), with `drive`, a restatement of the
take loop with the arrivals as input. `MAX_BATCH`, `FORWARD_WIDTH`,
`CHUNK_SLACK_BYTES` and `MAX_SINGLE_STREAK` are the fields of `Cfg`
(`Cfg.rust` is `32, 8, 64, 3`); lengths are byte lengths, exact naturals. The
forward is abstract (`Fwd`: vectors, an error or a panic per call); `rowFwd`
is the shape the isolation theorems assume, each text's row computed on its
own, every row one width (`Uniform`).

Results: a take is the queued singles in arrival order, then the bulk ones,
holds at most `MAX_BATCH` and loses nothing beyond the cap
(`takePrioritized_fst`, `takePrioritized_length_le`, `takePrioritized_perm`);
every text is in exactly one chunk, every chunk holds at most `FORWARD_WIDTH`
texts each within twice any other's length plus `CHUNK_SLACK_BYTES`, and the
restated loop is the Rust's `last_mut` loop (`lengthChunks_perm`,
`chunksOf_slack`, `chunksOf_eq_lit`); the highest-first `remove` loop takes
exactly the chunk's requests (`removeDesc_eq`). Isolation: with a row-wise
forward each caller gets what its text gets alone, whatever it was batched
with (`forwardIsolated_rowwise`); a failing device fails every caller
`Unavailable` in two forwards, a width mismatch every caller `Backend` in
one, and there is one answer per text, for no texts too
(`forwardIsolated_device_down`, `forwardIsolated_width_mismatch`,
`forwardIsolated_length`). Fairness: at most `MAX_SINGLE_STREAK` single
chunks in a row while a take has bulk texts (`pick_progress`), and a take
ends within `phi + 1` iterations whatever arrives (`take_ends`;
`|single| + 132·|bulk| + 1` with the Rust's constants, `phi_rust`). Since
#310 a device fault that has passed by the canary changes no text's answer:
every text of the chunk, a lone text included, is rerun alone and gets what
it gets alone (`transient_fault_recovers`); for a lone text that costs three
forwards, the text, the canary and the text again (`test_lone_rerun`). About
the current code: with `MAX_BATCH` singles at every take a queued bulk text
is never taken (`takePrioritized_no_bulk`). The tests' `fake_forward` cases
are `decide` runs (`test_healthy` and the others).

Boundary: tensor math, tokenization, padding and messages are not modelled,
and the real forward's rows depend on their batch up to rounding (padding,
batched kernels), so `forwardIsolated_rowwise` holds on the Rust for the
error class and the vector up to the live test's tolerance (cosine > 0.999),
not bit for bit. Token counts are byte lengths, as in the Rust. `take_ends`
is about `drive`; `Worker.lean` connects it to the state machine
(`pullOf_drive`).
-/

namespace Lambo.BatchQueue

open Lambo.Lib (eraseIdx_perm map_eq_pointwise)

/-! ## Constants -/

/-- The coalescer's tuning constants, kept as parameters so the small `decide`
runs stay cheap; `Cfg.rust` is the Rust's values. -/
structure Cfg where
  /-- `MAX_BATCH`: most requests one take (and one top-up) removes from the queue. -/
  maxBatch : Nat
  /-- `FORWARD_WIDTH`: most texts one forward runs. -/
  width : Nat
  /-- `CHUNK_SLACK_BYTES`. -/
  slack : Nat
  /-- `MAX_SINGLE_STREAK`. -/
  streak : Nat
  deriving DecidableEq, Repr

/-- `MAX_BATCH = 32`, `FORWARD_WIDTH = 8`, `CHUNK_SLACK_BYTES = 64`,
`MAX_SINGLE_STREAK = 3`.
Pinned by `formal_constants_tests::batch_queue` (its Rust side with feature `embed-candle`). -/
def Cfg.rust : Cfg := ⟨32, 8, 64, 3⟩

/-! ## Texts, vectors and one forward -/

/-- A queued text, reduced to its byte length (`str::len`, what
`length_chunks` reads) and a tag naming its content. -/
structure Txt where
  len : Nat
  tag : Nat
  deriving DecidableEq, Repr

/-- `CANARY_TEXT = "lambo"`: five bytes, tag 0.
Pinned by `formal_constants_tests::batch_queue` (its Rust side with feature `embed-candle`). -/
def canary : Txt := ⟨5, 0⟩

/-- An embedding, reduced to its width (`Vec<f32>::len`) and a tag. -/
structure Vec where
  width : Nat
  tag : Nat
  deriving DecidableEq, Repr

/-- What one call of the forward closure did: returned vectors, returned an
`Err`, or panicked (caught by `catch_unwind`). -/
inductive Out where
  | ok (vs : List Vec)
  | err
  | panic
  deriving DecidableEq, Repr

/-- The forward closure (`|chunk| shared.core.forward(chunk)`), abstract. Its
first argument is the index of the call among all forwards the coalescer has
run, so a device whose behaviour changes over time (a transient fault) can be
modelled; a forward that ignores it is stateless (`Stateless`). -/
abbrev Fwd := Nat → List Txt → Out

/-- The answer a caller's oneshot carries: a vector, `EmbedError::Backend`
(permanent, `is_transient` false) or `EmbedError::Unavailable` (transient).
Messages are left out. -/
inductive Res where
  | vec (v : Vec)
  | backend
  | unavailable
  deriving DecidableEq, Repr

/-- `once` inside `forward_isolated`: the forward's vectors when it returned
one per text; `none` for an error, a panic, or a count mismatch. -/
def once (f : Fwd) (k : Nat) (ts : List Txt) : Option (List Vec) :=
  match f k ts with
  | .ok vs => if vs.length = ts.length then some vs else none
  | _ => none

/-- `width_checked`: one vector of the wrong width fails every slot with
`Backend`. -/
def widthChecked (dim : Nat) (vs : List Vec) : List Res :=
  if vs.any (fun v => v.width != dim) then vs.map (fun _ => .backend) else vs.map .vec

/-- One text run alone, in the per-text arm of `forward_isolated`. -/
def solo (f : Fwd) (dim k : Nat) (t : Txt) : Res :=
  match once f k [t] with
  | some vs => ((widthChecked dim vs).head?).getD .backend
  | none => .backend

/-- The per-text arm: each text alone, one forward each, in order. -/
def solos (f : Fwd) (dim : Nat) : Nat → List Txt → List Res
  | _, [] => []
  | k, t :: ts => solo f dim k t :: solos f dim (k + 1) ts

/-- `forward_isolated(texts, dim, forward)`, with `k` the index of its first
forward. Returns the per-slot results and the forwards it ran, in order (the
batch, then the canary, then each text alone; since #310 a lone text is rerun
like any other). -/
def forwardIsolated (f : Fwd) (dim k : Nat) (ts : List Txt) : List Res × List (List Txt) :=
  match once f k ts with
  | some vs => (widthChecked dim vs, [ts])
  | none =>
    match once f (k + 1) [canary] with
    | none => (ts.map (fun _ => .unavailable), [ts, [canary]])
    | some _ => (solos f dim (k + 2) ts, [ts, [canary]] ++ ts.map (fun t => [t]))

/-! ## Requests and the queue (`Pending`, `BatchQueue`) -/

/-- A `Pending`: its oneshot (named by `id`), its text, and `bulk` (queued by
`embed_batch` rather than `embed`). -/
structure Req where
  id : Nat
  txt : Txt
  bulk : Bool
  deriving DecidableEq, Repr

/-- One pass of `take_prioritized` (or of `take_singles`): walk the queue in
order, moving each request of the wanted kind to `taken` while it holds fewer
than `n`, keeping the rest in order. -/
def pass (bulk : Bool) (n : Nat) (taken : List Req) : List Req → List Req × List Req
  | [] => (taken, [])
  | p :: ps =>
    if p.bulk = bulk ∧ taken.length < n then pass bulk n (taken ++ [p]) ps
    else
      let r := pass bulk n taken ps
      (r.1, p :: r.2)

/-- `take_prioritized(q, n)`: singles first, then bulk, each in arrival
order; returns the take and what stays queued. -/
def takePrioritized (q : List Req) (n : Nat) : List Req × List Req :=
  let r := pass false n [] q
  pass true n r.1 r.2

/-- `BatchQueue::take_singles(n)`. -/
def takeSingles (q : List Req) (n : Nat) : List Req × List Req :=
  if q.any (fun p => !p.bulk) then pass false n [] q else ([], q)

/-! ## `length_chunks` -/

/-- Insert `i` after every index whose key is at most its own. -/
def insertBy (key : Nat → Nat) (i : Nat) : List Nat → List Nat
  | [] => [i]
  | j :: js => if key i < key j then i :: j :: js else j :: insertBy key i js

/-- A stable sort by key (`sort_by_key`): insertion, left to right. -/
def sortBy (key : Nat → Nat) (l : List Nat) : List Nat :=
  l.foldl (fun acc i => insertBy key i acc) []

/-- `lens[i]` (out of range reads `0`; never read out of range). -/
def lenAt (lens : List Nat) (i : Nat) : Nat := lens.getD i 0

/-- The chunking loop, carrying the open chunk `cur` and its first index `h`
(`chunk[0]`).
Pinned by `formal_constants_tests::batch_queue` (its Rust side with feature `embed-candle`). -/
def lcAux (c : Cfg) (len : Nat → Nat) (cur : List Nat) (h : Nat) :
    List Nat → List (List Nat)
  | [] => [cur]
  | i :: is =>
    if cur.length < c.width ∧ len i ≤ 2 * len h + c.slack then lcAux c len (cur ++ [i]) h is
    else cur :: lcAux c len [i] i is

/-- Chunks of an index order. -/
def chunksOf (c : Cfg) (len : Nat → Nat) : List Nat → List (List Nat)
  | [] => []
  | i :: is => lcAux c len [i] i is

/-- The loop as the Rust writes it: `chunks.last_mut()`, push to it or open
a new chunk. `chunksOf` is proved equal to it (`chunksOf_eq_lit`). -/
def lcLit (c : Cfg) (len : Nat → Nat) (chunks : List (List Nat)) : List Nat → List (List Nat)
  | [] => chunks
  | i :: is =>
    match chunks.getLast? with
    | some ch =>
      if ch.length < c.width ∧ len i ≤ 2 * len (ch.headD 0) + c.slack then
        lcLit c len (chunks.dropLast ++ [ch ++ [i]]) is
      else lcLit c len (chunks ++ [[i]]) is
    | none => lcLit c len (chunks ++ [[i]]) is

/-- `length_chunks(lens)`. -/
def lengthChunks (c : Cfg) (lens : List Nat) : List (List Nat) :=
  chunksOf c (lenAt lens) (sortBy (lenAt lens) (List.range lens.length))

/-! ## Taking a chunk out of a group (`serve_take`) -/

/-- Insertion sort, descending (`sort_unstable_by(|a, b| b.cmp(a))`; the
indices are distinct, so stability does not matter). -/
def insertDesc (i : Nat) : List Nat → List Nat
  | [] => [i]
  | j :: js => if j ≤ i then i :: j :: js else j :: insertDesc i js

def sortDesc (l : List Nat) : List Nat := l.foldr insertDesc []

/-- `chunk.into_iter().map(|i| group.remove(i)).collect()`: remove each index
in turn from the shrinking group. An index out of range is skipped here
(`Vec::remove` would panic); `removeDesc_eq` shows none is out of range. -/
def removeDesc {α : Type} (g : List α) : List Nat → List α × List α
  | [] => ([], g)
  | i :: is =>
    match g[i]? with
    | some x =>
      let r := removeDesc (g.eraseIdx i) is
      (x :: r.1, r.2)
    | none => removeDesc g is

/-- The chunk's requests (`items`, after `items.reverse()`) and the rest of
the group. -/
def extract (g : List Req) (chunk : List Nat) : List Req × List Req :=
  let r := removeDesc g (sortDesc chunk)
  (r.1.reverse, r.2)

/-- The first length chunk of a group, taken out of it
(`length_chunks(&lens).swap_remove(0)`, then `extract`). -/
def chunkOf (c : Cfg) (g : List Req) : List Req × List Req :=
  match lengthChunks c (g.map (·.txt.len)) with
  | [] => ([], g)
  | ch :: _ => extract g ch

/-! ## One iteration of `serve_take` -/

/-- A take in progress: its single and bulk groups and `single_streak`. -/
structure Take where
  single : List Req
  bulk : List Req
  streak : Nat
  deriving DecidableEq, Repr

def Take.items (t : Take) : List Req := t.single ++ t.bulk

/-- `take.into_iter().partition(|p| !p.bulk)`. -/
def split (b : List Req) : Take := ⟨b.filter (fun p => !p.bulk), b.filter (·.bulk), 0⟩

/-- The singles a loop iteration pulls from the queue (and the queue left):
only while the take has bulk texts and shutdown is not requested. -/
def pullOf (c : Cfg) (shutdown : Bool) (q : List Req) (t : Take) : List Req × List Req :=
  if t.bulk ≠ [] ∧ shutdown = false then takeSingles q c.maxBatch else ([], q)

/-- The group choice of one `serve_take` iteration, given what it pulled:
`none` is the `return`; otherwise the chunk's requests and the take after. -/
def pick (c : Cfg) (pulled : List Req) (t : Take) : Option (List Req × Take) :=
  if t.bulk ≠ [] ∧ (t.single ++ pulled = [] ∨ c.streak ≤ t.streak) then
    some ((chunkOf c t.bulk).1, ⟨t.single ++ pulled, (chunkOf c t.bulk).2, 0⟩)
  else if t.single ++ pulled = [] then none
  else
    some ((chunkOf c (t.single ++ pulled)).1,
      ⟨(chunkOf c (t.single ++ pulled)).2, t.bulk, t.streak + 1⟩)

/-! ## The queue passes -/

theorem pass_full (b : Bool) (n : Nat) (taken q : List Req) (h : n ≤ taken.length) :
    pass b n taken q = (taken, q) := by
  induction q with
  | nil => rfl
  | cons p ps ih =>
    have : ¬(p.bulk = b ∧ taken.length < n) := fun h' => by omega
    simp only [pass, this, ite_false, ih]

/-- What a pass takes: the wanted kind, in arrival order, up to the room
left. -/
theorem pass_fst (b : Bool) (n : Nat) (taken q : List Req) :
    (pass b n taken q).1 = taken ++ (q.filter (fun p => p.bulk == b)).take (n - taken.length) := by
  induction q generalizing taken with
  | nil => simp [pass]
  | cons p ps ih =>
    by_cases hp : p.bulk = b
    · by_cases hl : taken.length < n
      · have hk : n - taken.length = (n - (taken.length + 1)) + 1 := by omega
        simp only [pass, hp, hl, and_self, ite_true, ih, List.length_append, List.length_singleton,
          List.filter_cons, beq_self_eq_true, hk, List.take_succ_cons, List.append_assoc,
          List.singleton_append]
      · have h0 : n - taken.length = 0 := by omega
        simp only [pass, hl, and_false, ite_false, ih, h0, List.take_zero]
    · have hb : (p.bulk == b) = false := by simpa using hp
      simp only [pass, hp, false_and, ite_false, ih, List.filter_cons, hb, Bool.false_eq_true]

/-- What a pass leaves queued: the rest, in arrival order. -/
theorem pass_perm (b : Bool) (n : Nat) (taken q : List Req) :
    ((pass b n taken q).1 ++ (pass b n taken q).2).Perm (taken ++ q) := by
  induction q generalizing taken with
  | nil => simp [pass]
  | cons p ps ih =>
    unfold pass
    split
    · have := ih (taken ++ [p]); simpa using this
    · have := ih taken
      exact (List.perm_middle.trans (List.Perm.cons p this)).trans (List.perm_middle).symm

theorem pass_snd_sublist (b : Bool) (n : Nat) (taken q : List Req) :
    (pass b n taken q).2.Sublist q := by
  induction q generalizing taken with
  | nil => simp [pass]
  | cons p ps ih =>
    unfold pass
    split
    · exact (ih _).cons p
    · exact (ih taken).cons_cons p

/-- A pass never touches the other kind. -/
theorem pass_snd_filter (b : Bool) (n : Nat) (taken q : List Req) :
    (pass b n taken q).2.filter (fun p => p.bulk != b) = q.filter (fun p => p.bulk != b) := by
  induction q generalizing taken with
  | nil => simp [pass]
  | cons p ps ih =>
    unfold pass
    split
    · next h => simp [h.1, ih]
    · by_cases hp : p.bulk = b
      · simp [hp, ih]
      · have : (p.bulk != b) = true := by simpa using hp
        simp [this, ih]

theorem pass_fst_length_le (b : Bool) (n : Nat) (taken q : List Req) (h : taken.length ≤ n) :
    (pass b n taken q).1.length ≤ n := by
  rw [pass_fst]; simp only [List.length_append, List.length_take]; omega

/-- `take_prioritized`: the take is the queued singles in arrival order, then
the queued bulk requests in arrival order, `n` at most. -/
theorem takePrioritized_fst (q : List Req) (n : Nat) :
    (takePrioritized q n).1 =
      (q.filter (fun p => !p.bulk)).take n ++
        (q.filter (fun p => p.bulk)).take (n - ((q.filter (fun p => !p.bulk)).take n).length) := by
  unfold takePrioritized
  rw [pass_fst, pass_fst]
  have h1 : (pass false n [] q).2.filter (fun p => p.bulk == true) = q.filter (fun p => p.bulk) := by
    have := pass_snd_filter false n [] q
    simpa using this
  have hf : (fun p : Req => p.bulk == false) = (fun p => !p.bulk) := by
    funext p; cases p.bulk <;> rfl
  rw [h1, hf]; simp

theorem takePrioritized_perm (q : List Req) (n : Nat) :
    ((takePrioritized q n).1 ++ (takePrioritized q n).2).Perm q := by
  unfold takePrioritized
  have h1 := pass_perm false n [] q
  have h2 := pass_perm true n (pass false n [] q).1 (pass false n [] q).2
  simp only [List.nil_append] at h1
  exact h2.trans h1

/-- A take is clamped to `n`. -/
theorem takePrioritized_length_le (q : List Req) (n : Nat) : (takePrioritized q n).1.length ≤ n := by
  unfold takePrioritized
  exact pass_fst_length_le _ _ _ _ (pass_fst_length_le _ _ _ _ (Nat.zero_le _))

/-- `take_prioritized` leaves the requests it does not take queued in arrival
order. -/
theorem takePrioritized_snd_sublist (q : List Req) (n : Nat) :
    (takePrioritized q n).2.Sublist q :=
  (pass_snd_sublist _ _ _ _).trans (pass_snd_sublist _ _ _ _)

/-- A take is as large as it can be: `min n (queue length)`. With 50 queued
and `MAX_BATCH = 32` it is 32, and 18 stay queued (test
`initial_take_never_exceeds_max_batch_and_leftovers_survive`). -/
theorem takePrioritized_length (q : List Req) (n : Nat) :
    (takePrioritized q n).1.length = min n q.length := by
  rw [takePrioritized_fst]
  have hp := List.filter_append_perm (fun p : Req => !p.bulk) q
  have hl := hp.length_eq
  simp only [List.length_append, Bool.not_not] at hl
  simp only [List.length_append, List.length_take]
  omega

theorem takeSingles_eq (q : List Req) (n : Nat) : takeSingles q n = pass false n [] q := by
  unfold takeSingles
  split
  · rfl
  · next h =>
    have hq : ∀ p ∈ q, p.bulk = true := by simpa using h
    clear h
    have : ∀ (taken : List Req), pass false n taken q = (taken, q) := by
      intro taken
      induction q generalizing taken with
      | nil => rfl
      | cons p ps ih =>
        have hp : p.bulk = true := hq p (by simp)
        simp only [pass, hp, Bool.true_eq_false, false_and, ite_false]
        rw [ih (fun x hx => hq x (by simp [hx])) taken]
    exact (this []).symm

/-- `take_singles` takes only singles, in arrival order, `n` at most. -/
theorem takeSingles_fst (q : List Req) (n : Nat) :
    (takeSingles q n).1 = (q.filter (fun p => !p.bulk)).take n := by
  rw [takeSingles_eq, pass_fst]
  simp

theorem takeSingles_perm (q : List Req) (n : Nat) :
    ((takeSingles q n).1 ++ (takeSingles q n).2).Perm q := by
  rw [takeSingles_eq]; simpa using pass_perm false n [] q

/-- Fairness across takes holds only while fewer than `n` singles are
queued: with `n` or more, a take holds no bulk request and every queued bulk
request stays queued. A steady supply of `MAX_BATCH` waiting single embeds
therefore keeps a write's queued texts out of every take (see
`queued_bulk_starves`). -/
theorem takePrioritized_no_bulk (q : List Req) (n : Nat)
    (h : n ≤ (q.filter (fun p => !p.bulk)).length) :
    (takePrioritized q n).1 = (q.filter (fun p => !p.bulk)).take n ∧
      (takePrioritized q n).2.filter (·.bulk) = q.filter (·.bulk) := by
  have hl : ((q.filter (fun p => !p.bulk)).take n).length = n := by simp; omega
  refine ⟨by rw [takePrioritized_fst, hl]; simp, ?_⟩
  unfold takePrioritized
  have hn : n ≤ (pass false n [] q).1.length := by
    rw [pass_fst]; simp; omega
  rw [pass_full _ _ _ _ hn]
  have := pass_snd_filter false n [] q
  simpa using this


/-! ## `length_chunks` -/

theorem insertBy_perm (key : Nat → Nat) (i : Nat) (l : List Nat) : (insertBy key i l).Perm (i :: l) := by
  induction l with
  | nil => simp [insertBy]
  | cons j js ih =>
    unfold insertBy
    split
    · exact List.Perm.refl _
    · exact ((ih.cons j).trans (List.Perm.swap i j js))

theorem insertBy_sorted (key : Nat → Nat) (i : Nat) (l : List Nat)
    (h : l.Pairwise (fun a b => key a ≤ key b)) :
    (insertBy key i l).Pairwise (fun a b => key a ≤ key b) := by
  induction l with
  | nil => simp [insertBy]
  | cons j js ih =>
    rw [List.pairwise_cons] at h
    unfold insertBy
    split
    · next hlt =>
      refine List.Pairwise.cons ?_ (List.Pairwise.cons h.1 h.2)
      intro b hb
      rcases List.mem_cons.mp hb with rfl | hb
      · omega
      · have := h.1 b hb; omega
    · next hge =>
      refine List.Pairwise.cons ?_ (ih h.2)
      intro b hb
      rcases List.mem_cons.mp ((insertBy_perm key i js).mem_iff.mp hb) with rfl | hb
      · omega
      · exact h.1 b hb

theorem sortBy_perm_aux (key : Nat → Nat) (l acc : List Nat) :
    (l.foldl (fun acc i => insertBy key i acc) acc).Perm (acc ++ l) := by
  induction l generalizing acc with
  | nil => simp
  | cons i is ih =>
    simp only [List.foldl_cons]
    refine (ih _).trans ?_
    have h1 : (insertBy key i acc ++ is).Perm (i :: acc ++ is) := (insertBy_perm key i acc).append_right is
    exact h1.trans (List.perm_middle).symm

theorem sortBy_perm (key : Nat → Nat) (l : List Nat) : (sortBy key l).Perm l := by
  unfold sortBy; simpa using sortBy_perm_aux key l []

theorem sortBy_sorted (key : Nat → Nat) (l : List Nat) :
    (sortBy key l).Pairwise (fun a b => key a ≤ key b) := by
  unfold sortBy
  suffices ∀ acc : List Nat, acc.Pairwise (fun a b => key a ≤ key b) →
      (l.foldl (fun acc i => insertBy key i acc) acc).Pairwise (fun a b => key a ≤ key b) from
    this [] List.Pairwise.nil
  induction l with
  | nil => intro acc h; exact h
  | cons i is ih => intro acc h; exact ih _ (insertBy_sorted key i acc h)

theorem lcAux_flatten (c : Cfg) (len : Nat → Nat) (cur : List Nat) (h : Nat) (is : List Nat) :
    (lcAux c len cur h is).flatten = cur ++ is := by
  induction is generalizing cur h with
  | nil => simp [lcAux]
  | cons i is ih =>
    unfold lcAux
    split
    · rw [ih]; simp
    · simp [ih]

theorem chunksOf_flatten (c : Cfg) (len : Nat → Nat) (l : List Nat) : (chunksOf c len l).flatten = l := by
  cases l with
  | nil => rfl
  | cons i is => simp [chunksOf, lcAux_flatten]

theorem lcAux_ne_nil (c : Cfg) (len : Nat → Nat) (cur : List Nat) (h : Nat) (is : List Nat)
    (hc : cur ≠ []) : ∀ ch ∈ lcAux c len cur h is, ch ≠ [] := by
  induction is generalizing cur h with
  | nil => simpa [lcAux] using hc
  | cons i is ih =>
    unfold lcAux
    split
    · exact ih _ _ (by simp)
    · intro ch hch
      rcases List.mem_cons.mp hch with rfl | hch
      · exact hc
      · exact ih _ _ (by simp) ch hch

theorem chunksOf_ne_nil (c : Cfg) (len : Nat → Nat) (l : List Nat) :
    ∀ ch ∈ chunksOf c len l, ch ≠ [] := by
  cases l with
  | nil => simp [chunksOf]
  | cons i is => exact lcAux_ne_nil c len [i] i is (by simp)

theorem lcAux_width (c : Cfg) (len : Nat → Nat) (cur : List Nat) (h : Nat) (is : List Nat)
    (hw : 1 ≤ c.width) (hc : cur.length ≤ c.width) : ∀ ch ∈ lcAux c len cur h is, ch.length ≤ c.width := by
  induction is generalizing cur h with
  | nil => simpa [lcAux] using hc
  | cons i is ih =>
    unfold lcAux
    split
    · next hh => exact ih _ _ (by simp; omega)
    · intro ch hch
      rcases List.mem_cons.mp hch with rfl | hch
      · exact hc
      · exact ih _ _ (by simpa using hw) ch hch

/-- #125: at most `FORWARD_WIDTH` texts per forward. -/
theorem chunksOf_width (c : Cfg) (len : Nat → Nat) (l : List Nat) (hw : 1 ≤ c.width) :
    ∀ ch ∈ chunksOf c len l, ch.length ≤ c.width := by
  cases l with
  | nil => simp [chunksOf]
  | cons i is => exact lcAux_width c len [i] i is hw (by simpa using hw)

theorem lcAux_head (c : Cfg) (len : Nat → Nat) (cur : List Nat) (h : Nat) (is : List Nat)
    (hc : cur.headD 0 = h) (hne : cur ≠ []) (hs : ∀ j ∈ cur, len j ≤ 2 * len h + c.slack) :
    ∀ ch ∈ lcAux c len cur h is, ∀ j ∈ ch, len j ≤ 2 * len (ch.headD 0) + c.slack := by
  induction is generalizing cur h with
  | nil =>
    simp only [lcAux, List.mem_singleton]
    rintro ch rfl; rw [hc]; exact hs
  | cons i is ih =>
    unfold lcAux
    split
    · next hh =>
      refine ih _ _ ?_ (by simp) ?_
      · cases cur with
        | nil => exact absurd rfl hne
        | cons x xs => simpa using hc
      · intro j hj
        rcases List.mem_append.mp hj with hj | hj
        · exact hs j hj
        · simp at hj; subst hj; exact hh.2
    · intro ch hch
      rcases List.mem_cons.mp hch with rfl | hch
      · rw [hc]; exact hs
      · exact ih [i] i rfl (by simp) (by simp; omega) ch hch

/-- #125: no text in a forward is longer than twice the forward's first
(shortest) text plus `CHUNK_SLACK_BYTES`. -/
theorem chunksOf_head (c : Cfg) (len : Nat → Nat) (l : List Nat) :
    ∀ ch ∈ chunksOf c len l, ∀ j ∈ ch, len j ≤ 2 * len (ch.headD 0) + c.slack := by
  cases l with
  | nil => simp [chunksOf]
  | cons i is => exact lcAux_head c len [i] i is rfl (by simp) (by simp; omega)

/-- Pairwise slack: every text of a chunk is within the slack of every other
one, when the order is sorted by length (as `length_chunks` sorts it). -/
theorem chunksOf_slack (c : Cfg) (len : Nat → Nat) (l : List Nat)
    (hs : l.Pairwise (fun a b => len a ≤ len b)) :
    ∀ ch ∈ chunksOf c len l, ∀ a ∈ ch, ∀ b ∈ ch, len b ≤ 2 * len a + c.slack := by
  intro ch hch a ha b hb
  have hsub : ch.Sublist l := by
    have := List.sublist_flatten_of_mem hch
    rwa [chunksOf_flatten] at this
  have hp := hs.sublist hsub
  have hb' := chunksOf_head c len l ch hch b hb
  cases ch with
  | nil => simp at ha
  | cons x xs =>
    rw [List.pairwise_cons] at hp
    have hxa : len x ≤ len a := by
      rcases List.mem_cons.mp ha with rfl | ha
      · exact Nat.le_refl _
      · exact hp.1 a ha
    simp only [List.headD_cons] at hb'
    omega

theorem lcLit_eq (c : Cfg) (len : Nat → Nat) (pre : List (List Nat)) (cur : List Nat) (is : List Nat)
    (hne : cur ≠ []) :
    lcLit c len (pre ++ [cur]) is = pre ++ lcAux c len cur (cur.headD 0) is := by
  induction is generalizing pre cur with
  | nil => simp [lcLit, lcAux]
  | cons i is ih =>
    simp only [lcLit, List.getLast?_append, List.getLast?_singleton, Option.some_or,
      List.dropLast_concat, lcAux]
    split
    · next hh =>
      have hd : (cur ++ [i]).headD 0 = cur.headD 0 := by
        cases cur with
        | nil => exact absurd rfl hne
        | cons x xs => rfl
      rw [ih pre (cur ++ [i]) (by simp), hd]
    · have := ih (pre ++ [cur]) [i] (by simp)
      simp only [List.headD_cons, List.append_assoc, List.singleton_append] at this
      simpa using this

/-- The restatement `chunksOf` is the Rust's `chunks.last_mut()` loop. -/
theorem chunksOf_eq_lit (c : Cfg) (len : Nat → Nat) (l : List Nat) :
    chunksOf c len l = lcLit c len [] l := by
  cases l with
  | nil => rfl
  | cons i is =>
    have := lcLit_eq c len [] [i] is (by simp)
    simp only [List.nil_append, List.headD_cons] at this
    simp only [chunksOf, lcLit, List.getLast?_nil, List.nil_append]
    exact this.symm

/-- `length_chunks` places every index exactly once ("every text exactly
once"): its chunks, in order, are a permutation of `0..n`. -/
theorem lengthChunks_perm (c : Cfg) (lens : List Nat) :
    (lengthChunks c lens).flatten.Perm (List.range lens.length) := by
  unfold lengthChunks; rw [chunksOf_flatten]; exact sortBy_perm _ _

/-- Shortest texts first: in chunk order the lengths never decrease. -/
theorem lengthChunks_sorted (c : Cfg) (lens : List Nat) :
    (lengthChunks c lens).flatten.Pairwise (fun a b => lenAt lens a ≤ lenAt lens b) := by
  unfold lengthChunks; rw [chunksOf_flatten]; exact sortBy_sorted _ _

theorem lengthChunks_width (c : Cfg) (lens : List Nat) (hw : 1 ≤ c.width) :
    ∀ ch ∈ lengthChunks c lens, ch.length ≤ c.width :=
  chunksOf_width _ _ _ hw

theorem lengthChunks_ne_nil (c : Cfg) (lens : List Nat) : ∀ ch ∈ lengthChunks c lens, ch ≠ [] :=
  chunksOf_ne_nil _ _ _

theorem lengthChunks_slack (c : Cfg) (lens : List Nat) :
    ∀ ch ∈ lengthChunks c lens, ∀ a ∈ ch, ∀ b ∈ ch, lenAt lens b ≤ 2 * lenAt lens a + c.slack :=
  chunksOf_slack _ _ _ (sortBy_sorted _ _)

theorem lengthChunks_ne (c : Cfg) (lens : List Nat) (h : lens ≠ []) : lengthChunks c lens ≠ [] := by
  intro h0
  have := (lengthChunks_perm c lens).length_eq
  rw [h0] at this
  simp at this
  exact h (List.length_eq_zero_iff.mp this.symm)

/-- Test `length_chunks_keep_long_texts_out_of_short_forwards`. -/
theorem lengthChunks_long_text_alone :
    lengthChunks Cfg.rust (List.replicate 15 40 ++ [910]) =
      [[0, 1, 2, 3, 4, 5, 6, 7], [8, 9, 10, 11, 12, 13, 14], [15]] := by decide

/-- Test `length_chunks_join_texts_within_the_slack`: `10 * 2 + 64 = 84`. -/
theorem lengthChunks_slack_edge : lengthChunks Cfg.rust [100, 10, 84, 85] = [[1, 2], [3, 0]] := by
  decide


/-! ## Taking a chunk out of a group -/

theorem removeDesc_perm {α : Type} (g : List α) (is : List Nat) :
    ((removeDesc g is).1 ++ (removeDesc g is).2).Perm g := by
  induction is generalizing g with
  | nil => simp [removeDesc]
  | cons i is ih =>
    unfold removeDesc
    split
    · next x hx =>
      obtain ⟨hi, rfl⟩ := List.getElem?_eq_some_iff.mp hx
      exact ((ih _).cons _).trans (eraseIdx_perm g i hi).symm
    · exact ih g

theorem removeDesc_length {α : Type} (g : List α) (is : List Nat) :
    (removeDesc g is).1.length ≤ is.length := by
  induction is generalizing g with
  | nil => simp [removeDesc]
  | cons i is ih =>
    unfold removeDesc
    split
    · simpa using ih _
    · have := ih g; simp; omega

theorem filterMap_congr' {α β : Type} {f g : α → Option β} {l : List α}
    (h : ∀ a ∈ l, f a = g a) : l.filterMap f = l.filterMap g := by
  induction l with
  | nil => rfl
  | cons a as ih =>
    simp only [List.filterMap_cons, h a (by simp), ih (fun x hx => h x (by simp [hx]))]

/-- Removing the indices highest first takes exactly the requests at those
indices: each `remove` leaves the lower indices still to take where they were
(the comment in `serve_take`). -/
theorem removeDesc_eq {α : Type} (g : List α) (is : List Nat) (h : is.Pairwise (· > ·)) :
    (removeDesc g is).1 = is.filterMap (fun i => g[i]?) := by
  induction is generalizing g with
  | nil => simp [removeDesc]
  | cons i is ih =>
    rw [List.pairwise_cons] at h
    unfold removeDesc
    split
    · next x hx =>
      simp only [List.filterMap_cons, hx]
      rw [ih _ h.2]
      congr 1
      exact filterMap_congr' (fun j hj => List.getElem?_eraseIdx_of_lt (h.1 j hj))
    · next hx =>
      simp only [List.filterMap_cons, hx]
      exact ih g h.2

theorem insertDesc_perm (i : Nat) (l : List Nat) : (insertDesc i l).Perm (i :: l) := by
  induction l with
  | nil => simp [insertDesc]
  | cons j js ih =>
    unfold insertDesc
    split
    · exact List.Perm.refl _
    · exact (ih.cons j).trans (List.Perm.swap i j js)

theorem insertDesc_sorted (i : Nat) (l : List Nat) (h : l.Pairwise (· ≥ ·)) :
    (insertDesc i l).Pairwise (· ≥ ·) := by
  induction l with
  | nil => simp [insertDesc]
  | cons j js ih =>
    rw [List.pairwise_cons] at h
    unfold insertDesc
    split
    · next hle =>
      refine List.Pairwise.cons ?_ (List.Pairwise.cons h.1 h.2)
      intro b hb
      rcases List.mem_cons.mp hb with rfl | hb
      · omega
      · have := h.1 b hb; omega
    · next hgt =>
      refine List.Pairwise.cons ?_ (ih h.2)
      intro b hb
      rcases List.mem_cons.mp ((insertDesc_perm i js).mem_iff.mp hb) with rfl | hb
      · omega
      · exact h.1 b hb

theorem sortDesc_perm (l : List Nat) : (sortDesc l).Perm l := by
  induction l with
  | nil => simp [sortDesc]
  | cons i is ih => exact (insertDesc_perm i _).trans (ih.cons i)

theorem sortDesc_sorted (l : List Nat) : (sortDesc l).Pairwise (· ≥ ·) := by
  induction l with
  | nil => simp [sortDesc]
  | cons i is ih => exact insertDesc_sorted i _ ih

theorem sortDesc_strict (l : List Nat) (h : l.Nodup) : (sortDesc l).Pairwise (· > ·) := by
  have hn : (sortDesc l).Nodup := (sortDesc_perm l).nodup_iff.mpr h
  exact List.Pairwise.imp₂ (fun a b h1 h2 => Nat.lt_of_le_of_ne h1 (Ne.symm h2)) (sortDesc_sorted l) hn

theorem extract_perm (g : List Req) (ch : List Nat) :
    ((extract g ch).1 ++ (extract g ch).2).Perm g := by
  unfold extract
  exact ((List.reverse_perm _).append_right _).trans (removeDesc_perm _ _)

theorem extract_length (g : List Req) (ch : List Nat) : (extract g ch).1.length ≤ ch.length := by
  unfold extract
  simp only [List.length_reverse]
  exact Nat.le_trans (removeDesc_length _ _) (Nat.le_of_eq (sortDesc_perm ch).length_eq)

theorem extract_mem (g : List Req) (ch : List Nat) (hn : ch.Nodup) :
    ∀ a ∈ (extract g ch).1, ∃ i ∈ ch, g[i]? = some a := by
  intro a ha
  unfold extract at ha
  rw [List.mem_reverse, removeDesc_eq _ _ (sortDesc_strict ch hn), List.mem_filterMap] at ha
  obtain ⟨i, hi, hg⟩ := ha
  exact ⟨i, (sortDesc_perm ch).mem_iff.mp hi, hg⟩

theorem extract_ne_nil (g : List Req) (ch : List Nat) (hn : ch.Nodup) (hne : ch ≠ [])
    (hr : ∀ i ∈ ch, i < g.length) : (extract g ch).1 ≠ [] := by
  unfold extract
  rw [ne_eq, List.reverse_eq_nil_iff, removeDesc_eq _ _ (sortDesc_strict ch hn)]
  have hp := sortDesc_perm ch
  cases hs : sortDesc ch with
  | nil => rw [hs] at hp; exact absurd (List.Perm.nil_eq hp).symm hne
  | cons i is =>
    have hi : i < g.length := hr i (hp.mem_iff.mp (by simp [hs]))
    simp [List.getElem?_eq_getElem hi]

/-- The facts `chunkOf` inherits from `length_chunks` on a group. -/
theorem chunkOf_spec (c : Cfg) (g : List Req) :
    ∃ ch, (ch = [] ∧ chunkOf c g = ([], g) ∧ g = []) ∨
      (ch ∈ lengthChunks c (g.map (·.txt.len)) ∧ chunkOf c g = extract g ch ∧ ch ≠ [] ∧
        ch.Nodup ∧ ∀ i ∈ ch, i < g.length) := by
  unfold chunkOf
  split
  · next h =>
    refine ⟨[], Or.inl ⟨rfl, rfl, ?_⟩⟩
    cases g with
    | nil => rfl
    | cons x xs => exact absurd h (lengthChunks_ne c _ (by simp))
  · next ch rest h =>
    have hm : ch ∈ lengthChunks c (g.map (·.txt.len)) := by rw [h]; simp
    have hsub : ch.Sublist (lengthChunks c (g.map (·.txt.len))).flatten := List.sublist_flatten_of_mem hm
    have hperm := lengthChunks_perm c (g.map (·.txt.len))
    have hnd : (lengthChunks c (g.map (·.txt.len))).flatten.Nodup := hperm.nodup_iff.mpr List.nodup_range
    refine ⟨ch, Or.inr ⟨hm, rfl, lengthChunks_ne_nil _ _ ch hm, hnd.sublist hsub, ?_⟩⟩
    intro i hi
    have := hperm.mem_iff.mp (hsub.subset hi)
    simpa using this

theorem chunkOf_perm (c : Cfg) (g : List Req) : ((chunkOf c g).1 ++ (chunkOf c g).2).Perm g := by
  obtain ⟨ch, ⟨-, h, -⟩ | ⟨-, h, -⟩⟩ := chunkOf_spec c g
  · rw [h]; exact List.Perm.refl _
  · rw [h]; exact extract_perm _ _

theorem chunkOf_ne_nil (c : Cfg) (g : List Req) (hg : g ≠ []) : (chunkOf c g).1 ≠ [] := by
  obtain ⟨ch, ⟨-, -, h⟩ | ⟨-, h, hne, hn, hr⟩⟩ := chunkOf_spec c g
  · exact absurd h hg
  · rw [h]; exact extract_ne_nil _ _ hn hne hr

/-- A forward runs at most `FORWARD_WIDTH` texts. -/
theorem chunkOf_width (c : Cfg) (g : List Req) (hw : 1 ≤ c.width) : (chunkOf c g).1.length ≤ c.width := by
  obtain ⟨ch, ⟨-, h, -⟩ | ⟨hm, h, -⟩⟩ := chunkOf_spec c g
  · rw [h]; simp
  · rw [h]; exact Nat.le_trans (extract_length _ _) (lengthChunks_width c _ hw ch hm)

/-- The length rule holds between every two texts a forward runs. -/
def Slack (c : Cfg) (ts : List Txt) : Prop := ∀ a ∈ ts, ∀ b ∈ ts, b.len ≤ 2 * a.len + c.slack

theorem Slack.sublist {c : Cfg} {ts us : List Txt} (h : Slack c ts) (hs : us.Sublist ts) : Slack c us :=
  fun a ha b hb => h a (hs.subset ha) b (hs.subset hb)

theorem slack_single (c : Cfg) (t : Txt) : Slack c [t] := by
  intro a ha b hb; simp at ha hb; subst ha; subst hb; omega

theorem chunkOf_slack (c : Cfg) (g : List Req) : Slack c ((chunkOf c g).1.map (·.txt)) := by
  obtain ⟨ch, ⟨-, h, -⟩ | ⟨hm, h, -, hn, -⟩⟩ := chunkOf_spec c g
  · rw [h]; intro a ha; simp at ha
  · rw [h]
    intro a ha b hb
    simp only [List.mem_map] at ha hb
    obtain ⟨ra, hra, rfl⟩ := ha
    obtain ⟨rb, hrb, rfl⟩ := hb
    obtain ⟨i, hi, hgi⟩ := extract_mem g ch hn ra hra
    obtain ⟨j, hj, hgj⟩ := extract_mem g ch hn rb hrb
    have := lengthChunks_slack c (g.map (·.txt.len)) ch hm i hi j hj
    simp only [lenAt, List.getD_eq_getElem?_getD, List.getElem?_map, hgi, hgj, Option.map_some,
      Option.getD_some] at this
    exact this


/-! ## `forward_isolated` -/

theorem widthChecked_length (dim : Nat) (vs : List Vec) : (widthChecked dim vs).length = vs.length := by
  unfold widthChecked; split <;> simp

theorem solos_length (f : Fwd) (dim k : Nat) (ts : List Txt) : (solos f dim k ts).length = ts.length := by
  induction ts generalizing k with
  | nil => rfl
  | cons t ts ih => simp [solos, ih]

theorem once_length {f : Fwd} {k : Nat} {ts : List Txt} {vs : List Vec} (h : once f k ts = some vs) :
    vs.length = ts.length := by
  unfold once at h
  split at h
  · split at h
    · next he => cases h; exact he
    · cases h
  · cases h

/-- One answer per text, so `items.zip(results)` drops nobody. -/
theorem forwardIsolated_length (f : Fwd) (dim k : Nat) (ts : List Txt) :
    (forwardIsolated f dim k ts).1.length = ts.length := by
  unfold forwardIsolated
  split
  · next vs hv => rw [widthChecked_length, once_length hv]
  · split
    · simp
    · simp [solos_length]

/-- Every forward `forward_isolated` runs is the chunk itself or one text
(the canary, or a text of the chunk alone). -/
theorem forwardIsolated_fwds (f : Fwd) (dim k : Nat) (ts : List Txt) :
    ∀ b ∈ (forwardIsolated f dim k ts).2, b = ts ∨ b = [canary] ∨ ∃ t ∈ ts, b = [t] := by
  unfold forwardIsolated
  split
  · simp
  · split
    · simp
    · intro b hb
      simp only [List.cons_append, List.nil_append, List.mem_cons, List.mem_map] at hb
      rcases hb with rfl | rfl | ⟨t, ht, rfl⟩
      · exact Or.inl rfl
      · exact Or.inr (Or.inl rfl)
      · exact Or.inr (Or.inr ⟨t, ht, rfl⟩)

theorem forwardIsolated_fwds_caps (c : Cfg) (f : Fwd) (dim k : Nat) (ts : List Txt)
    (hw : ts.length ≤ c.width) (h1 : 1 ≤ c.width) (hs : Slack c ts) :
    ∀ b ∈ (forwardIsolated f dim k ts).2, b.length ≤ c.width ∧ Slack c b := by
  intro b hb
  rcases forwardIsolated_fwds f dim k ts b hb with rfl | rfl | ⟨t, -, rfl⟩
  · exact ⟨hw, hs⟩
  · exact ⟨by simpa using h1, slack_single c _⟩
  · exact ⟨by simpa using h1, slack_single c _⟩

/-- A healthy chunk is one forward (test `a_healthy_batch_is_one_forward`). -/
theorem forwardIsolated_healthy (f : Fwd) (dim k : Nat) (ts : List Txt) (vs : List Vec)
    (h : once f k ts = some vs) : forwardIsolated f dim k ts = (widthChecked dim vs, [ts]) := by
  unfold forwardIsolated; rw [h]

/-- A failing device (the chunk and the canary both fail) fails every caller
`Unavailable`, transient, in two forwards, with no per-text retries (test
`a_failing_device_fails_every_caller_transient_without_per_text_retries`). -/
theorem forwardIsolated_device_down (f : Fwd) (dim k : Nat) (ts : List Txt)
    (h1 : once f k ts = none) (h2 : once f (k + 1) [canary] = none) :
    forwardIsolated f dim k ts = (ts.map (fun _ => .unavailable), [ts, [canary]]) := by
  unfold forwardIsolated; rw [h1, h2]

/-- A width mismatch fails every caller `Backend` in one forward (test
`a_width_mismatch_fails_every_caller_without_a_retry`). -/
theorem forwardIsolated_width_mismatch (f : Fwd) (dim k : Nat) (ts : List Txt) (vs : List Vec)
    (h : once f k ts = some vs) (hv : ∃ v ∈ vs, v.width ≠ dim) :
    forwardIsolated f dim k ts = (vs.map (fun _ => .backend), [ts]) := by
  rw [forwardIsolated_healthy f dim k ts vs h]
  obtain ⟨v, hv, hw⟩ := hv
  have : vs.any (fun v => v.width != dim) = true := List.any_eq_true.mpr ⟨v, hv, by simpa using hw⟩
  simp [widthChecked, this]

/-- A forward that computes each text's row on its own and fails the whole
call if any row fails: the shape of the real `BgeM3Core::forward` (one
`encode_batch`, one model call), and of the tests' `fake_forward`. Exact
only in the model: on the real forward a row depends on its batch up to
rounding (padding, batched kernels; the live test asserts cosine > 0.999
between batched and single), so the theorems built on `rowFwd` hold for the
Rust up to that tolerance. -/
def rowFwd (row : Txt → Option Vec) : Fwd := fun _ ts =>
  if ts.all (fun t => (row t).isSome) then .ok (ts.filterMap row) else .err

/-- What a text's caller should get on its own: its vector if the row exists
at the contract width, `Backend` otherwise. -/
def soloRes (row : Txt → Option Vec) (dim : Nat) (t : Txt) : Res :=
  match row t with
  | some v => if v.width = dim then .vec v else .backend
  | none => .backend

/-- Every row has one width (a model emits fixed-width rows). -/
def Uniform (row : Txt → Option Vec) (w : Nat) : Prop := ∀ t v, row t = some v → v.width = w

theorem solo_rowFwd (row : Txt → Option Vec) (dim k : Nat) (t : Txt) :
    solo (rowFwd row) dim k t = soloRes row dim t := by
  unfold solo once rowFwd soloRes
  cases h : row t with
  | none => simp [h]
  | some v =>
    by_cases hw : v.width = dim
    · simp [h, widthChecked, hw]
    · simp [h, widthChecked, hw]

theorem solos_rowFwd (row : Txt → Option Vec) (dim k : Nat) (ts : List Txt) :
    solos (rowFwd row) dim k ts = ts.map (soloRes row dim) := by
  induction ts generalizing k with
  | nil => rfl
  | cons t ts ih => simp [solos, solo_rowFwd, ih]

theorem filterMap_length_of_all {row : Txt → Option Vec} {ts : List Txt}
    (h : ∀ t ∈ ts, (row t).isSome) : (ts.filterMap row).length = ts.length := by
  induction ts with
  | nil => rfl
  | cons t ts ih =>
    have ht := h t (by simp)
    cases hr : row t with
    | none => rw [hr] at ht; cases ht
    | some v => simp [hr, ih (fun x hx => h x (by simp [hx]))]

theorem map_vec_rows (row : Txt → Option Vec) (dim : Nat) (ts : List Txt)
    (h : ∀ t ∈ ts, ∃ v, row t = some v ∧ v.width = dim) :
    (ts.filterMap row).map .vec = ts.map (soloRes row dim) := by
  induction ts with
  | nil => rfl
  | cons t ts ih =>
    obtain ⟨v, hr, hw⟩ := h t (by simp)
    simp [hr, soloRes, hw, ih (fun x hx => h x (by simp [hx]))]

theorem map_backend_rows (row : Txt → Option Vec) (dim : Nat) (ts : List Txt)
    (h : ∀ t ∈ ts, ∃ v, row t = some v ∧ v.width ≠ dim) :
    (ts.filterMap row).map (fun _ => Res.backend) = ts.map (soloRes row dim) := by
  induction ts with
  | nil => rfl
  | cons t ts ih =>
    obtain ⟨v, hr, hw⟩ := h t (by simp)
    simp [hr, soloRes, hw, ih (fun x hx => h x (by simp [hx]))]

theorem widthChecked_rows (row : Txt → Option Vec) (dim w : Nat) (hu : Uniform row w)
    (ts : List Txt) (hne : ts ≠ []) (h : ∀ t ∈ ts, (row t).isSome) :
    widthChecked dim (ts.filterMap row) = ts.map (soloRes row dim) := by
  have hex : ∀ t ∈ ts, ∃ v, row t = some v ∧ v.width = w := by
    intro t ht
    obtain ⟨v, hv⟩ := Option.isSome_iff_exists.mp (h t ht)
    exact ⟨v, hv, hu t v hv⟩
  by_cases hw : w = dim
  · have hany : (ts.filterMap row).any (fun v => v.width != dim) = false := by
      rw [List.any_eq_false]
      intro v hv
      obtain ⟨t, -, hr⟩ := List.mem_filterMap.mp hv
      simp [hu t v hr, hw]
    rw [widthChecked, hany]
    simp only [Bool.false_eq_true, ite_false]
    exact map_vec_rows row dim ts (fun t ht => by
      obtain ⟨v, hv, hvw⟩ := hex t ht; exact ⟨v, hv, hvw ▸ hw⟩)
  · have hany : (ts.filterMap row).any (fun v => v.width != dim) = true := by
      cases ts with
      | nil => exact absurd rfl hne
      | cons t ts =>
        obtain ⟨v, hr, hvw⟩ := hex t (by simp)
        simp [hr, hvw, hw]
    rw [widthChecked, hany]
    simp only [ite_true]
    exact map_backend_rows row dim ts (fun t ht => by
      obtain ⟨v, hv, hvw⟩ := hex t ht; exact ⟨v, hv, hvw ▸ hw⟩)

/-- #125: one text's failure is its own. With a row-wise forward of fixed
width and a canary that embeds, every caller gets exactly what its text gets
alone, whatever it was batched with (tests
`one_bad_text_fails_only_its_own_caller`,
`a_panicking_text_fails_only_its_own_caller_and_names_the_panic`,
`a_lone_bad_text_is_permanent_when_the_canary_passes`). -/
theorem forwardIsolated_rowwise (row : Txt → Option Vec) (dim w k : Nat) (hu : Uniform row w)
    (hc : (row canary).isSome) (ts : List Txt) (hne : ts ≠ []) :
    (forwardIsolated (rowFwd row) dim k ts).1 = ts.map (soloRes row dim) := by
  by_cases hall : ∀ t ∈ ts, (row t).isSome
  · have hb : ts.all (fun t => (row t).isSome) = true := List.all_eq_true.mpr hall
    have ho : once (rowFwd row) k ts = some (ts.filterMap row) := by
      simp [once, rowFwd, hb, filterMap_length_of_all hall]
    rw [forwardIsolated_healthy _ _ _ _ _ ho]
    exact widthChecked_rows row dim w hu ts hne hall
  · have hb : ts.all (fun t => (row t).isSome) = false := by
      rw [Bool.eq_false_iff]; intro h; exact hall (List.all_eq_true.mp h)
    have ho : once (rowFwd row) k ts = none := by simp [once, rowFwd, hb]
    obtain ⟨v, hv⟩ := Option.isSome_iff_exists.mp hc
    have hco : once (rowFwd row) (k + 1) [canary] = some [v] := by simp [once, rowFwd, hv]
    unfold forwardIsolated
    rw [ho, hco]
    exact solos_rowFwd row dim (k + 2) ts

/-- A device fault that has passed by the canary: the first forward fails
whatever it holds, and every later one is the row-wise forward. -/
def flaky (row : Txt → Option Vec) : Fwd := fun k ts => if k = 0 then .err else rowFwd row k ts

/-- Since #310 `forward_isolated` charges no text for a device fault that has
passed by the canary: after the failed forward and a canary that embeds, every
text of the chunk, a lone text included, is run alone and gets what it gets
alone (`soloRes`), so a write's text queued alone is not refused as `Backend`
for the device's fault. -/
theorem transient_fault_recovers (row : Txt → Option Vec) (dim : Nat) (ts : List Txt)
    (hc : (row canary).isSome) :
    (forwardIsolated (flaky row) dim 0 ts).1 = ts.map (soloRes row dim) := by
  obtain ⟨v, hv⟩ := Option.isSome_iff_exists.mp hc
  have h0 : once (flaky row) 0 ts = none := by simp [once, flaky]
  have h1 : once (flaky row) (0 + 1) [canary] = some [v] := by simp [once, flaky, rowFwd, hv]
  have he : ∀ (us : List Txt) (k : Nat), 0 < k →
      solos (flaky row) dim k us = solos (rowFwd row) dim k us := by
    intro us
    induction us with
    | nil => intro k _; rfl
    | cons u us ih =>
      intro k hk
      have hk0 : k ≠ 0 := by omega
      simp only [solos]
      rw [ih (k + 1) (by omega)]
      congr 1
      simp [solo, once, flaky, hk0]
  unfold forwardIsolated
  rw [h0, h1]
  exact (he ts 2 (by omega)).trans (solos_rowFwd row dim 2 ts)

/-! ### The tests' `fake_forward`

`fake dim`: a vector per text whose tag is its length mod `dim`, a panic for
a chunk holding the tag 99 (`"panic"`), an error for one holding 98
(`"bad"`). -/

def fake (dim : Nat) : Fwd := fun _ ts =>
  if ts.any (·.tag == 99) then .panic
  else if ts.any (·.tag == 98) then .err
  else .ok (ts.map fun t => ⟨dim, t.len % dim⟩)

def tA : Txt := ⟨1, 1⟩
def tBB : Txt := ⟨2, 2⟩
def tCCC : Txt := ⟨3, 3⟩
def tBad : Txt := ⟨3, 98⟩
def tPanic : Txt := ⟨5, 99⟩

/-- `a_healthy_batch_is_one_forward`: one forward, each slot its own vector. -/
theorem test_healthy :
    forwardIsolated (fake 4) 4 0 [tA, tBB, tCCC] =
      ([.vec ⟨4, 1⟩, .vec ⟨4, 2⟩, .vec ⟨4, 3⟩], [[tA, tBB, tCCC]]) := by decide

/-- `one_bad_text_fails_only_its_own_caller`: the batch, the canary, then each
text alone (five forwards); only the bad text fails, permanently. -/
theorem test_one_bad :
    (forwardIsolated (fake 4) 4 0 [tA, tBad, tCCC]).1 = [.vec ⟨4, 1⟩, .backend, .vec ⟨4, 3⟩] ∧
      (forwardIsolated (fake 4) 4 0 [tA, tBad, tCCC]).2.length = 5 := by decide

/-- `a_panicking_text_fails_only_its_own_caller_and_names_the_panic`. -/
theorem test_panic : (forwardIsolated (fake 4) 4 0 [tPanic, tBB]).1 = [.backend, .vec ⟨4, 2⟩] := by
  decide

/-- `a_failing_device_fails_every_caller_transient_without_per_text_retries`. -/
theorem test_device_down :
    forwardIsolated (fun _ _ => .err) 4 0 [tA, tBB, tCCC, ⟨4, 4⟩] =
      ([.unavailable, .unavailable, .unavailable, .unavailable],
        [[tA, tBB, tCCC, ⟨4, 4⟩], [canary]]) := by decide

/-- `a_lone_bad_text_is_permanent_when_the_canary_passes`: three forwards
(the text, the canary, the text again). -/
theorem test_lone_bad :
    forwardIsolated (fake 4) 4 0 [tBad] = ([.backend], [[tBad], [canary], [tBad]]) := by
  decide

/-- `a_width_mismatch_fails_every_caller_without_a_retry`. -/
theorem test_width_mismatch :
    forwardIsolated (fake 4) 8 0 [tA, tBB] = ([.backend, .backend], [[tA, tBB]]) := by decide

/-- `a_lone_text_is_rerun_after_a_fault_that_has_passed_by_the_canary`: the
first forward fails, the canary embeds, and the lone text's rerun embeds. -/
theorem test_lone_rerun :
    forwardIsolated (fun k ts => if k = 0 then .err else fake 4 k ts) 4 0 [tA] =
      ([.vec ⟨4, 1⟩], [[tA], [canary], [tA]]) := by decide

/-! ## One `serve_take` iteration -/

theorem perm_count {l₁ l₂ : List Req} (h : l₁.Perm l₂) (x : Req) : l₁.count x = l₂.count x :=
  h.count_eq x

theorem pick_none_iff (c : Cfg) (pulled : List Req) (t : Take) :
    pick c pulled t = none ↔ t.bulk = [] ∧ t.single ++ pulled = [] := by
  unfold pick
  constructor
  · intro h
    split at h
    · cases h
    · split at h
      · next h1 h2 =>
        refine ⟨?_, h2⟩
        exact Classical.byContradiction fun hb => h1 ⟨hb, Or.inl h2⟩
      · cases h
  · rintro ⟨hb, hs⟩
    simp [hb, hs]

/-- `pick` (`serve_take`) always yields a batch when there is a bulk request
or any single to serve. -/
theorem pick_some (c : Cfg) (pulled : List Req) (t : Take) (h : t.bulk ≠ [] ∨ t.single ++ pulled ≠ []) :
    ∃ items t', pick c pulled t = some (items, t') := by
  cases hp : pick c pulled t with
  | none =>
    have := (pick_none_iff c pulled t).mp hp
    rcases h with h | h
    · exact absurd this.1 h
    · exact absurd this.2 h
  | some r => exact ⟨r.1, r.2, rfl⟩

/-- The take's chunk comes out of a group and the rest stays: nothing is
lost or duplicated by an iteration. -/
theorem pick_perm (c : Cfg) (pulled : List Req) (t : Take) (items : List Req) (t' : Take)
    (h : pick c pulled t = some (items, t')) : (items ++ t'.items).Perm (t.items ++ pulled) := by
  unfold pick at h
  rw [List.perm_iff_count]
  intro x
  split at h
  · cases h
    have := perm_count (chunkOf_perm c t.bulk) x
    simp only [Take.items, List.count_append] at this ⊢
    omega
  · split at h
    · cases h
    · cases h
      have := perm_count (chunkOf_perm c (t.single ++ pulled)) x
      simp only [Take.items, List.count_append] at this ⊢
      omega

theorem pick_items_ne (c : Cfg) (pulled : List Req) (t : Take) (items : List Req) (t' : Take)
    (h : pick c pulled t = some (items, t')) : items ≠ [] := by
  unfold pick at h
  split at h
  · next hb => cases h; exact chunkOf_ne_nil c _ hb.1
  · split at h
    · cases h
    · next hs => cases h; exact chunkOf_ne_nil c _ hs

theorem pick_items_caps (c : Cfg) (pulled : List Req) (t : Take) (items : List Req) (t' : Take)
    (hw : 1 ≤ c.width) (h : pick c pulled t = some (items, t')) :
    items.length ≤ c.width ∧ Slack c (items.map (·.txt)) := by
  unfold pick at h
  split at h
  · cases h; exact ⟨chunkOf_width c _ hw, chunkOf_slack c _⟩
  · split at h
    · cases h
    · cases h; exact ⟨chunkOf_width c _ hw, chunkOf_slack c _⟩

/-- #125 focused check: with bulk texts in the take, an iteration either runs
a bulk chunk (resetting the streak) or runs a single chunk with the streak
still below `MAX_SINGLE_STREAK`. So at most `MAX_SINGLE_STREAK` single
chunks run in a row while the take has bulk texts. -/
theorem pick_progress (c : Cfg) (pulled : List Req) (t : Take) (items : List Req) (t' : Take)
    (hb : t.bulk ≠ []) (h : pick c pulled t = some (items, t')) :
    (t'.single = t.single ++ pulled ∧ items.length + t'.bulk.length = t.bulk.length ∧
        t'.streak = 0) ∨
      (t'.bulk = t.bulk ∧ items.length + t'.single.length = t.single.length + pulled.length ∧
        t'.streak = t.streak + 1 ∧ t.streak < c.streak) := by
  unfold pick at h
  split at h
  · cases h
    left
    have hl := (chunkOf_perm c t.bulk).length_eq
    simp only [List.length_append] at hl
    exact ⟨rfl, hl, rfl⟩
  · next hr =>
    split at h
    · cases h
    · cases h
      right
      have hl := (chunkOf_perm c (t.single ++ pulled)).length_eq
      simp only [List.length_append] at hl
      refine ⟨rfl, by simpa using hl, rfl, ?_⟩
      exact Classical.byContradiction fun hc => hr ⟨hb, Or.inr (by omega)⟩

/-- With no bulk texts left, an iteration only shrinks the single group. -/
theorem pick_no_bulk (c : Cfg) (t : Take) (items : List Req) (t' : Take)
    (hb : t.bulk = []) (h : pick c [] t = some (items, t')) :
    t'.bulk = [] ∧ t'.single.length < t.single.length := by
  have hne := pick_items_ne c [] t items t' h
  unfold pick at h
  simp only [List.append_nil] at h
  split at h
  · next hr => exact absurd hb hr.1
  · split at h
    · cases h
    · cases h
      refine ⟨hb, ?_⟩
      show (chunkOf c t.single).2.length < t.single.length
      have hl := (chunkOf_perm c t.single).length_eq
      have : 0 < (chunkOf c t.single).1.length := List.length_pos_iff.mpr hne
      simp only [List.length_append] at hl
      omega

/-- The take loop on its own, with the singles the environment queues before
each iteration as input (`ps`; pulled only while bulk texts remain, as
`serve_take` pulls). Returns the forwards run and the take, `none` once it
has returned. -/
def drive (c : Cfg) : List (List Req) → Take → Nat × Option Take
  | [], t => (0, some t)
  | p :: ps, t =>
    match pick c (if t.bulk ≠ [] then p else []) t with
    | none => (0, none)
    | some (_, t') =>
      let r := drive c ps t'
      (r.1 + 1, r.2)

/-- A potential that every iteration lowers: singles, plus, while bulk texts
remain, `(P + 1)` for each forward the streak rule still allows before the
bulk runs out (`P` the most singles one pull brings). -/
def phi (c : Cfg) (P : Nat) (t : Take) : Nat :=
  t.single.length + if t.bulk = [] then 0 else (P + 1) * ((c.streak + 1) * t.bulk.length - t.streak)

theorem pick_phi (c : Cfg) (P : Nat) (p : List Req) (t : Take) (items : List Req) (t' : Take)
    (hp : p.length ≤ P) (hs : t.bulk ≠ [] → t.streak ≤ c.streak)
    (h : pick c (if t.bulk ≠ [] then p else []) t = some (items, t')) :
    phi c P t' < phi c P t ∧ (t'.bulk ≠ [] → t'.streak ≤ c.streak) := by
  by_cases hb : t.bulk = []
  · simp only [hb, ne_eq, not_true_eq_false, ite_false] at h
    obtain ⟨hb', hl⟩ := pick_no_bulk c t items t' hb h
    refine ⟨?_, fun h' => absurd hb' h'⟩
    simp [phi, hb, hb', hl]
  · simp only [hb, ne_eq, not_false_eq_true, ite_true] at h
    have hst := hs hb
    have hne := List.length_pos_iff.mpr (pick_items_ne c p t items t' h)
    have hbl : 0 < t.bulk.length := List.length_pos_iff.mpr hb
    have hS : c.streak + 1 ≤ (c.streak + 1) * t.bulk.length := Nat.le_mul_of_pos_right _ hbl
    rcases pick_progress c p t items t' hb h with ⟨hsin, hl, h0⟩ | ⟨heq, hl, hsucc, hlt⟩
    · refine ⟨?_, fun _ => by omega⟩
      have hsl : t'.single.length = t.single.length + p.length := by rw [hsin]; simp
      have hm : (c.streak + 1) * t'.bulk.length + (c.streak + 1) ≤ (c.streak + 1) * t.bulk.length := by
        rw [← Nat.mul_succ]; exact Nat.mul_le_mul_left _ (by omega)
      unfold phi
      simp only [hb, ite_false]
      split
      · have h1 : (P + 1) * 1 ≤ (P + 1) * ((c.streak + 1) * t.bulk.length - t.streak) :=
          Nat.mul_le_mul_left _ (by omega)
        omega
      · rw [h0]
        have : (P + 1) * ((c.streak + 1) * t'.bulk.length - 0) + (P + 1) * 1 ≤
            (P + 1) * ((c.streak + 1) * t.bulk.length - t.streak) := by
          rw [← Nat.mul_add]; apply Nat.mul_le_mul_left; omega
        omega
    · refine ⟨?_, fun _ => by omega⟩
      unfold phi
      simp only [heq, hb, ite_false, hsucc]
      have e1 : (c.streak + 1) * t.bulk.length - t.streak =
          ((c.streak + 1) * t.bulk.length - (t.streak + 1)) + 1 := by omega
      rw [e1, Nat.mul_add, Nat.mul_one]
      omega

/-- #125: "a steady stream of recalls cannot keep the take from ending". If
each pull brings at most `P` singles, a take ends within `phi + 1`
iterations, whatever arrives: with the Rust's constants, at most
`|single| + 132 · |bulk| + 1` (`phi_rust`). -/
theorem take_ends (c : Cfg) (P : Nat) (ps : List (List Req)) (t : Take)
    (hp : ∀ p ∈ ps, p.length ≤ P) (hs : t.bulk ≠ [] → t.streak ≤ c.streak)
    (hlen : phi c P t < ps.length) : (drive c ps t).2 = none := by
  induction ps generalizing t with
  | nil => simp at hlen
  | cons p ps ih =>
    unfold drive
    split
    · rfl
    · next items t' h =>
      obtain ⟨hlt, hs'⟩ := pick_phi c P p t items t' (hp p (by simp)) hs h
      exact ih t' (fun q hq => hp q (by simp [hq])) hs' (by simp at hlen; omega)

/-- With the Rust's configuration (`MAX_BATCH` = 32), the coalescer's
potential is at most the singles plus 132 per bulk request: the bound on
`coalesce_loop`'s iterations. -/
theorem phi_rust (t : Take) (ht : t.streak = 0) :
    phi Cfg.rust 32 t ≤ t.single.length + 132 * t.bulk.length := by
  unfold phi; split
  · omega
  · simp only [Cfg.rust, ht, Nat.sub_zero]; omega

end Lambo.BatchQueue
